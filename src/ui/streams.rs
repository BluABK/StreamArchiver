//! Streams view: channel grid, add-channel form, imports, OAuth connects.

use super::*;

/// `app_settings` key for the Streams grid's "Group" checkbox (channel-group
/// header clustering) — `"1"`/`"0"`, defaults to on (clustering has always
/// been the default behavior). Saved immediately on toggle, not part of the
/// batched Settings form — same shape as `K_SCHEDULE_COMPACT`.
pub(super) const K_STREAMS_GROUP_VISUALLY: &str = "streams_group_visually";

/// `app_settings` key for the Streams grid's "Only stored" checkbox —
/// `"1"`/`"0"`, defaults to off. Saved immediately on toggle, same shape as
/// [`K_STREAMS_GROUP_VISUALLY`].
pub(super) const K_STREAMS_ONLY_RECORDED: &str = "streams_only_recorded";

/// Backing state for the "Add to group…" dialog (bulk-adds `selected_streams`
/// to a recording group — new or existing). `pick` wins over `new_name` when
/// both are set (picking an existing group after having typed a name that's
/// then abandoned).
#[derive(Default)]
pub(super) struct AddToRecordingGroupDialog {
    pick: Option<i64>,
    new_name: String,
}

/// Pending confirmation for a manual "🗑🔥 Delete file from disk" — set when
/// the take-row context-menu item is clicked (see [`crate::manual_delete`]),
/// cleared on Delete/Cancel. `method` is resolved ONCE at menu-click time
/// (`crate::disposal::effective_method_for_recording`) so the confirm dialog
/// can tell the user exactly what's about to happen before they commit —
/// trash/Recycle Bin/permanent.
pub(super) struct ConfirmDeleteFile {
    pub(super) rec_id: i64,
    pub(super) channel_id: i64,
    pub(super) monitor_id: i64,
    pub(super) path: String,
    /// "{channel name} — Take N" (or similar), for the confirm dialog's text.
    pub(super) label: String,
    pub(super) method: crate::disposal::DisposalMethod,
}

/// Pending confirmation for a bulk "🗑🔥 Delete all take files from disk" —
/// the stream-row equivalent of [`ConfirmDeleteFile`], for cleaning up every
/// take of one broadcast at once (e.g. an error/retry storm that left a
/// broadcast with a dozen useless takes). Every take's file, path, size and
/// resolved method is captured up front at menu-click time — takes can
/// resolve to different disposal methods (a per-recording trigger override),
/// so this is a list, not one shared method.
pub(super) struct ConfirmDeleteStreamFiles {
    pub(super) channel_id: i64,
    pub(super) monitor_id: i64,
    /// `(rec_id, path, bytes, resolved method)`, oldest take first.
    pub(super) items: Vec<(i64, String, i64, crate::disposal::DisposalMethod)>,
    /// "{channel name} — {stream date}", for the confirm dialog's text.
    pub(super) label: String,
}

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
    /// Rolling-recording overrides for this channel (`None`/empty = inherit
    /// global): whether its captures are auto-deleted after a TTL unless kept,
    /// and how long that TTL is (in **hours** as typed, stored as seconds).
    /// See [`crate::rolling`].
    pub(super) rolling: Option<bool>,
    pub(super) rolling_ttl_hours: String,
    /// Preferred platform when this channel has multiple instances
    /// simultaneously live (`None` = inherit the global default).
    pub(super) primary_platform_pref: Option<Platform>,
    /// Simulcast-dedup overrides for this channel (`None` = inherit global):
    /// which platform to record when several are live at once, and the
    /// platform that overrides it when its instance is ad-free. Unlike
    /// `primary_platform_pref` above, these decide what gets CAPTURED — see
    /// [`crate::simulcast`].
    pub(super) simulcast_pref: Option<crate::simulcast::SimulcastPref>,
    pub(super) simulcast_ad_free_pref: Option<crate::simulcast::SimulcastPref>,
    /// Chapter-embedding master toggle override for this channel (`None` =
    /// inherit global).
    pub(super) chapters_enabled: Option<bool>,
    /// Title/category coalesce-window override for this channel, seconds
    /// (empty = inherit the global default).
    pub(super) chapters_coalesce_secs: String,
    /// Follow-raid overrides for this channel (`None` = inherit global):
    /// whether raiding out from this channel auto-records the target, and
    /// whether this channel itself is ever auto-recorded as a raid target.
    pub(super) follow_my_raids: Option<bool>,
    pub(super) record_me_as_raid_target: Option<bool>,
    /// Whether raiding out from this channel auto-OPENS a live-edge player
    /// for the target (no recording) — independent of `follow_my_raids`.
    pub(super) follow_my_raids_play: Option<bool>,
    /// Whether this channel is ever excluded from being auto-played as a
    /// raid target — independent of `record_me_as_raid_target` (auto-play
    /// isn't gated by the disabled-check at all, only by this).
    pub(super) exclude_from_auto_play: Option<bool>,
    /// One of the two per-channel/instance gates the manual "Delete file from
    /// disk" take-row action needs (see [`crate::manual_delete`]) — plain
    /// bool, NOT an inherit chain: off by default, no global default to fall
    /// back to besides the Streams toolbar's own master switch.
    pub(super) allow_delete: bool,
    /// The group this channel clusters under in the Streams grid's default
    /// view (`None` = ungrouped there). Always a member of `groups` too —
    /// see `models::Channel::primary_group_id`.
    pub(super) primary_group: Option<i64>,
    /// Every group this channel belongs to (primary included). Diffed
    /// against the DB's current membership on save.
    pub(super) groups: std::collections::HashSet<i64>,
    /// Set by the deferred closure on Save; read back by
    /// `channel_form_window` next call.
    pub(super) do_save: bool,
    /// Set by the deferred closure on Cancel/close.
    pub(super) closed: bool,
    /// Snapshot of `self.channel_groups`, refreshed every wrapper call — the
    /// deferred closure can't reach `self` to read it directly.
    pub(super) channel_groups: Vec<crate::models::ChannelGroup>,
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
    /// Import this candidate as a NEW INSTANCE under an existing channel
    /// instead of creating a new one — `None` = new channel (the default).
    pub(super) target_channel: Option<i64>,
    /// `target_channel` was set by the "Guess existing channels" bulk action
    /// and hasn't been manually confirmed yet. While true, the guess is
    /// treated as unresolved (the row is excluded from import entirely, not
    /// silently downgraded to "new channel") — ticking the confirm checkbox,
    /// or manually touching the dropdown, clears this.
    pub(super) guess_pending: bool,
    /// Why `target_channel` was guessed, shown in the "auto-assumed" hover
    /// text. Empty when `target_channel` was picked manually (or unset).
    pub(super) guess_reason: &'static str,
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
    /// Hide already-added rows from the list — for importing in batches over
    /// time without re-scrolling past everything already added on a
    /// previous pass.
    pub(super) hide_already: bool,
    pub(super) status: String,
    /// Batch quality override for this import ("Overrides for this import"
    /// section). Empty = each monitor gets its per-platform default quality.
    pub(super) quality_override: String,
    /// Batch output-directory override. Empty = per-platform default output dir.
    pub(super) out_dir_override: String,
    /// Existing channels this import can target instead of creating a new
    /// one — snapshot of `self.rows`, refreshed by the wrapper every call
    /// (the deferred closure can't reach `self` to read it directly).
    pub(super) existing_channels: Vec<(i64, String)>,
    /// "Import N selected" clicked — applied by the wrapper.
    pub(super) do_import: bool,
    /// "🔗 Guess existing channels" clicked — applied by the wrapper.
    pub(super) do_guess: bool,
    /// Close/Cancel clicked, or the OS close button.
    pub(super) closed: bool,
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
    /// A Year/Month/Week header's triangle was clicked — see `period_toggles`.
    toggle_period: Option<String>,
    /// A channel-group header's triangle was clicked — see `collapsed_channel_groups`.
    toggle_channel_group: Option<i64>,
    /// A channel-group header's bulk Auto/Enabled action — (group id, on).
    bulk_set_group_enabled: Option<(i64, bool)>,
    bulk_set_group_automation: Option<(i64, bool)>,
    /// Ctrl/shift-clicked a Stream row — toggle `(key, take ids)` in
    /// `selected_streams`. Take ids captured now (see that field's doc
    /// comment) rather than re-resolved later.
    toggle_select_stream: Option<(String, Vec<i64>)>,
    /// Plain-clicked a Stream row — replace the selection with just this one.
    select_only_stream: Option<(String, Vec<i64>)>,
    /// "Remove from \"…\"" on a Stream row's context menu — (group id, take ids).
    remove_from_recording_group: Option<(i64, Vec<i64>)>,
    open_path: Option<std::path::PathBuf>,
    open_in_player: Option<StreamTarget>,
    play_new_instance_mid: Option<i64>,
    /// `(StreamGroup::key, monitor_id)` — set alongside a finished-file play
    /// action (open file / stream in player), never a folder-open or a live
    /// "play new instance". Drives the Backlog auto-"started" transition;
    /// see `crate::store::Store::stream_watch_state`.
    mark_started_stream: Option<(String, i64)>,
    copy_text: Option<String>,
    delete_recording: Option<i64>,
    /// "🗑🔥 Delete file from disk…" on a take row — by recording id. Opens
    /// the confirm dialog (`ConfirmDeleteFile`); the actual disposal only
    /// runs after the user confirms. Enabled only when all three
    /// `manual_delete` gates are on for this take's channel/instance.
    delete_recording_file: Option<i64>,
    /// "🗑🔥 Delete all take files from disk…" on a stream row — every
    /// eligible (non-active, has a file, not already mid-delete) take id in
    /// that broadcast. Opens the bulk confirm dialog
    /// (`ConfirmDeleteStreamFiles`); same three `manual_delete` gates as the
    /// single-take version, checked once for the whole instance.
    delete_stream_files: Option<Vec<i64>>,
    open_recording_props: Option<i64>,
    open_recover_take: Option<i64>,
    archive_vod_now: Option<i64>,
    /// "⏬ Backfill missed VOD" on a not_recorded (or discovery-synthesized)
    /// row — by recording id.
    backfill_missed_vod_now: Option<i64>,
    /// "🔎 Scan for missed streams" — by monitor id.
    scan_for_missed_streams: Option<i64>,
    /// "▷ Play VOD" on a past take/stream row — by recording id. Works
    /// regardless of whether the take was ever captured.
    play_vod_now: Option<i64>,
    /// "🌐 Open VOD webpage" on a past take/stream row — by recording id.
    open_vod_webpage: Option<i64>,
    backfill_head_now: Option<i64>,
    abort_backfill: Option<i64>,
    /// "📑 Embed chapters"/"🔁 Re-embed chapters" on a take/stream row.
    retrigger_chapters: Option<i64>,
    /// (recording id, new err_ack value) — "Acknowledge failure" /
    /// "Un-acknowledge" on a failed take/stream row.
    set_err_ack: Option<(i64, bool)>,
    /// (monitor id, recording id) — "View chat" on a stream/take row.
    view_chat_rec: Option<(i64, i64)>,
    // Container-level actions.
    toggle_channel_enabled: Option<(i64, bool)>, // set all instances
    toggle_channel_automation: Option<(i64, bool)>, // master switch
    rename_channel: Option<i64>,
    /// Channel id to open the "Merge into another channel" dialog for.
    merge_channel: Option<i64>,
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
    /// A capture-alert badge (🚨/🩹/⚠ on a take or stream row) was clicked —
    /// open the Warnings window.
    open_warnings: bool,
}

#[derive(Clone, Copy)]
enum VodJobKind {
    Recovery,
    Backfill,
}
/// A Year/Month/Week grouping header over a contiguous run of a monitor's
/// streams — see [`period_levels_needed`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PeriodKind {
    Year,
    Month,
    Week,
}
#[derive(Clone, Copy)]
enum Vis {
    /// A primary-group header — see `build_vis_rows`'s clustering pass. The
    /// group's name is looked up from `group_id` at render time (kept out of
    /// this Copy enum on purpose — same reasoning as `Period` resolving its
    /// label from `groups[&mid][gi_start..gi_end]` rather than storing it).
    /// Never emitted for an ungrouped run of channels, and never emitted at
    /// all while `streams_group_filter` narrows to a single group.
    ChannelGroup { group_id: i64, count: usize, expanded: bool },
    Channel(usize),
    Instance { row: usize, depth: usize },
    /// `gi_start..gi_end` is the absolute index range into `groups[&mid]`
    /// this header covers — the same indexing `Stream`/`Take` already use,
    /// so no separate lookup table is needed; the render fn re-derives the
    /// label/summary from `groups[&mid][gi_start..gi_end]` directly.
    /// `expanded` is `build_vis_rows`'s own `period_open(...)` result,
    /// carried along rather than recomputed at render time (recomputing
    /// would need `year_idx == 0`-style ancestor-chain state this row no
    /// longer has once flattened).
    Period {
        mid: i64,
        kind: PeriodKind,
        gi_start: usize,
        gi_end: usize,
        depth: usize,
        expanded: bool,
    },
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

/// The calendar date a stream is bucketed by: went-live time when known,
/// else the earliest take's own start (mirrors `stream_row`'s own
/// go-live-or-fallback convention for this exact "when did this broadcast
/// happen" question).
fn period_anchor_date(g: &crate::models::StreamGroup) -> chrono::NaiveDate {
    local_date(g.went_live_at.unwrap_or_else(|| g.started_at())).unwrap_or(chrono::NaiveDate::MIN)
}

/// Whether Year/Month/Week header rows are worth showing for a set of
/// stream dates (order doesn't matter — sorted internally): a level only
/// earns a header row when it would actually group more than one bucket,
/// otherwise it's a single always-open wrapper adding a row for nothing.
/// This is why a channel with only recent history renders identically to
/// before this feature — its one bucket chain never crosses a boundary.
fn period_levels_needed(dates: &[chrono::NaiveDate]) -> (bool, bool, bool) {
    use chrono::Datelike;
    if dates.len() < 2 {
        return (false, false, false);
    }
    let mut sorted = dates.to_vec();
    sorted.sort();
    let years = sorted.chunk_by(|a, b| a.year() == b.year()).count();
    let months = sorted
        .chunk_by(|a, b| (a.year(), a.month()) == (b.year(), b.month()))
        .count();
    let weeks = sorted.chunk_by(|a, b| week_start(*a) == week_start(*b)).count();
    (years > 1, months > 1, weeks > 1)
}

/// Stable identity for a period bucket, shared by `build_vis_rows` (checking
/// whether it's open) and `period_row` (setting the toggle on click) so the
/// two can never drift out of sync on key format. `date` is any date inside
/// the bucket — normalized to the bucket's own start before formatting.
fn period_key(mid: i64, kind: PeriodKind, date: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    match kind {
        PeriodKind::Year => format!("{mid}|Y|{}", date.year()),
        PeriodKind::Month => format!("{mid}|M|{}-{:02}", date.year(), date.month()),
        PeriodKind::Week => format!("{mid}|W|{}", week_start(date).format("%Y-%m-%d")),
    }
}

/// Effective open/closed state for a period bucket: `default_open` (true
/// only for the single most-recent bucket at each shown level) XOR'd
/// against whether the user has ever clicked it — unlike
/// `expanded_channels`/`_instances`/`_streams` (plain "presence = open"),
/// the default here varies per bucket, so `period_toggles` records
/// *deviations from default* rather than the open state itself.
fn period_open(default_open: bool, toggles: &HashSet<String>, key: &str) -> bool {
    default_open ^ toggles.contains(key)
}

impl StreamArchiverApp {
    /// Modal for creating a new channel container or renaming an existing one.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn channel_form_window(&mut self, ctx: &egui::Context) {
        let Some(form_arc) = self.channel_form.clone() else {
            return;
        };
        // Re-snapshotted every call — the deferred closure can't reach
        // `self.channel_groups` itself.
        form_arc.lock().unwrap().channel_groups = self.channel_groups.clone();

        let f = form_arc.lock().unwrap();
        let renaming = f.id.is_some();
        let title = if renaming { "Rename channel" } else { "Add channel" }.to_string();
        drop(f);

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("channel_form_vp"),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([420.0, 480.0]),
            form_arc.clone(),
            shared,
            |ctx, s, _shared| {
                let renaming = s.id.is_some();
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::TopBottomPanel::bottom("channel_form_bottom_bar").show(ctx, |ui| {
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
                        let channel_groups = s.channel_groups.clone();
                        egui::Grid::new("channel_form_grid")
                            .num_columns(2)
                            .spacing([8.0, 6.0])
                            .show(ui, |ui| {
                                ui.label("Name");
                                ui.text_edit_singleline(&mut s.name);
                                ui.end_row();

                                ui.label("Color");
                                ui.horizontal(|ui| {
                                    // Colored swatch preview
                                    let swatch_color = if s.color.is_empty() {
                                        egui::Color32::from_gray(0x60)
                                    } else {
                                        parse_hex_color(&s.color)
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
                                        egui::TextEdit::singleline(&mut s.color)
                                            .hint_text("#rrggbb")
                                            .desired_width(80.0),
                                    );
                                    if !s.color.is_empty() && ui.small_button("✕").clicked() {
                                        s.color.clear();
                                    }
                                });
                                ui.end_row();

                                ui.label("Download VOD after end");
                                tristate_combo(ui, "chform_vod_download", &mut s.vod_download)
                                    .on_hover_text(
                                        "Post-stream VOD download for every instance in this channel. \
                                         Inherit follows the global default (Settings).",
                                    );
                                ui.end_row();

                                ui.label("Replace with VOD");
                                tristate_combo(ui, "chform_vod_replace", &mut s.vod_replace)
                                    .on_hover_text(
                                        "Replace the live recording with the VOD on success (never for \
                                         a muted Twitch VOD). Inherit follows the global default.",
                                    );
                                ui.end_row();

                                ui.label("Fetch new head backfill on new take");
                                tristate_combo(ui, "chform_head_backfill_fetch", &mut s.head_backfill_fetch)
                                    .on_hover_text(
                                        "Capture-from-start only: fetch a fresh head backfill for a \
                                         retake (reconnect mid-broadcast), not just the stream's first \
                                         take. Inherit follows the global default (Settings).",
                                    );
                                ui.end_row();

                                ui.label("Replace old head (if new is undamaged)");
                                tristate_combo(ui, "chform_head_backfill_replace", &mut s.head_backfill_replace)
                                    .on_hover_text(
                                        "Once a fresh head backfill passes its integrity checks, delete \
                                         older takes' now-redundant head files for the same stream. Only \
                                         takes effect when fetching a new head is also on. Inherit \
                                         follows the global default.",
                                    );
                                ui.end_row();

                                ui.label("After full.mkv join");
                                join_cleanup_combo(ui, "chform_join_cleanup", &mut s.join_cleanup)
                                    .on_hover_text(
                                        "Once a verified full.mkv (head + live capture joined) lands \
                                         for a take in this channel: keep both parts (safe, doubles \
                                         the stream's disk cost), delete just the head, or delete \
                                         both parts (the take then points at the full). Deletions \
                                         follow the deletion method below. Inherit follows the \
                                         global default (Settings → Post-processing → Automatic deletion).",
                                    );
                                ui.end_row();

                                ui.label("Automatic deletes go to");
                                disposal_method_combo(ui, "chform_disposal_method", &mut s.disposal_method)
                                    .on_hover_text(
                                        "How automatic media deletions for this channel are executed \
                                         (post-join cleanup, superseded heads, a live capture \
                                         replaced by its VOD): moved to the configured trash folder, \
                                         sent to the Recycle Bin, or deleted permanently. Inherit \
                                         follows the global default. Note that \"Trash folder\" \
                                         needs a trash folder configured for the drive in \
                                         Settings → Automatic deletion — without one it quietly \
                                         falls back to the Recycle Bin.",
                                    );
                                ui.end_row();

                                ui.label("Rolling recordings");
                                ui.horizontal(|ui| {
                                    tristate_combo(ui, "chform_rolling", &mut s.rolling)
                                        .on_hover_text(
                                            "Treat this channel's captures as rolling: each one is \
                                             automatically deleted a set time after it finishes, \
                                             unless you press Keep on it (📥 Backlog → Rolling \
                                             recordings). The take's history row always survives — \
                                             title, stats, chat log, chapters and notes are kept, \
                                             only the video file goes, using the deletion method \
                                             above. Inherit follows the global default, and an \
                                             instance can override this again. Only captures \
                                             started AFTER this is turned on are affected; nothing \
                                             already recorded is put at risk.",
                                        );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut s.rolling_ttl_hours)
                                            .hint_text("hours")
                                            .desired_width(60.0),
                                    )
                                    .on_hover_text(
                                        "How many hours a rolling capture's file survives after the \
                                         recording ends. Empty inherits the global default. Each \
                                         take freezes the value in force when it started, so \
                                         changing this never re-times takes you already have.",
                                    );
                                });
                                ui.end_row();

                                ui.label("Preferred platform when multiple live");
                                platform_pref_combo(ui, "chform_platform_pref", &mut s.primary_platform_pref)
                                    .on_hover_text(
                                        "When this channel has more than one instance simultaneously \
                                         live, show this platform's info on the channel row instead of \
                                         whichever went live earliest. An instance-level pin (per \
                                         instance) overrides this. Inherit follows the global default \
                                         (Settings → Interface → Display). DISPLAY only — to control \
                                         which one gets RECORDED, see Simulcast dedup below.",
                                    );
                                ui.end_row();

                                ui.label("Simulcast: record only");
                                simulcast_pref_combo(
                                    ui,
                                    "chform_simulcast_pref",
                                    &mut s.simulcast_pref,
                                    "Off — record every live instance",
                                )
                                .on_hover_text(
                                    "When this channel is live on more than one platform at once, \
                                     record only this one — one copy of a simulcast instead of two. \
                                     If that platform isn't live, whatever is live still records. \
                                     The other instances stay armed as failover. Inherit follows \
                                     the global default (Settings → Automation → Simulcast dedup).",
                                );
                                ui.end_row();

                                ui.label("…prefer when ad-free");
                                simulcast_pref_combo(
                                    ui,
                                    "chform_simulcast_ad_free_pref",
                                    &mut s.simulcast_ad_free_pref,
                                    "No ad-free override",
                                )
                                .on_hover_text(
                                    "Overrides the row above whenever this channel's instance on \
                                     THIS platform is ad-free for you (marked by hand, or a \
                                     detected Twitch subscription) — its stream has no ad-break \
                                     cuts either, so it's the better copy. Ignored when that \
                                     instance isn't live.",
                                );
                                ui.end_row();

                                ui.label("Embed chapters");
                                tristate_combo(ui, "chform_chapters_enabled", &mut s.chapters_enabled)
                                    .on_hover_text(
                                        "Embed chapter markers (title/category changes, raids, \
                                         recovered/muted gap-splice segments) into finalized \
                                         recordings for every instance in this channel. Inherit \
                                         follows the global default (Settings → \
                                         Post-processing → Chapters).",
                                    );
                                ui.end_row();

                                ui.label("Title/game coalesce window (s)");
                                ui.add(
                                    egui::TextEdit::singleline(&mut s.chapters_coalesce_secs)
                                        .desired_width(80.0)
                                        .hint_text("Inherit"),
                                )
                                .on_hover_text(
                                    "How many seconds apart a title change and a category/game \
                                     change may land and still merge into one chapter, for every \
                                     instance in this channel. Blank inherits the global default \
                                     (Settings → Post-processing → Chapters).",
                                );
                                ui.end_row();

                                ui.label("Auto-record my raids");
                                tristate_combo(ui, "chform_follow_my_raids", &mut s.follow_my_raids)
                                    .on_hover_text(
                                        "When any instance in this channel raids out to another \
                                         Twitch channel, auto-record the target (Settings → \
                                         Follow raid). Inherit follows the global default there \
                                         — off unless you've turned it on. Independent of \
                                         \"Auto-play my raids\" below.",
                                    );
                                ui.end_row();

                                ui.label("Auto-play my raids");
                                tristate_combo(ui, "chform_follow_my_raids_play", &mut s.follow_my_raids_play)
                                    .on_hover_text(
                                        "When any instance in this channel raids out to another \
                                         Twitch channel, auto-open the target at the live edge in \
                                         your media player — no recording, same as the manual \
                                         \"▷🏃 Follow raid\" button but automatic (Settings → \
                                         Follow raid). Inherit follows the global default there. \
                                         Independent of \"Auto-record my raids\" above.",
                                    );
                                ui.end_row();

                                ui.label("Record me when I'm a raid target");
                                tristate_combo(
                                    ui,
                                    "chform_raid_target_record",
                                    &mut s.record_me_as_raid_target,
                                )
                                .on_hover_text(
                                    "Whether Follow raid may auto-RECORD this channel when a \
                                     followed raid lands on it. Always/Never override the \
                                     \"skip disabled raid targets\" default too — set this to \
                                     Always if you want this channel recorded via a raid even \
                                     while its master switch is off. Inherit follows that global \
                                     default (Settings → Follow raid).",
                                );
                                ui.end_row();

                                ui.label("Exclude from auto-play");
                                tristate_combo(
                                    ui,
                                    "chform_raid_play_exclude",
                                    &mut s.exclude_from_auto_play,
                                )
                                .on_hover_text(
                                    "Set to Always to make sure this channel never gets an \
                                     auto-opened player when a followed raid lands on it. Unlike \
                                     the record-side setting above, auto-play otherwise ignores \
                                     this channel's disabled state entirely — this is the only \
                                     way to opt it out. Inherit/Never both mean \"allowed\".",
                                );
                                ui.end_row();

                                ui.label("Allow deleting files");
                                ui.checkbox(&mut s.allow_delete, "")
                                    .on_hover_text(
                                        "Half of the gate for this channel's take rows' \
                                         \"🗑🔥 Delete file from disk\" action — the OTHER half \
                                         is a per-instance switch (Edit instance), and BOTH need \
                                         the Streams toolbar's own \"Allow deletion\" master \
                                         switch on too. Off by default, on purpose: unlike the \
                                         settings above, this has no inherited global default to \
                                         fall back to.",
                                    );
                                ui.end_row();

                                ui.label("Primary group");
                                egui::ComboBox::from_id_salt("chform_primary_group")
                                    .selected_text(
                                        s.primary_group
                                            .and_then(|gid| channel_groups.iter().find(|g| g.id == gid))
                                            .map(|g| g.name.as_str())
                                            .unwrap_or("(ungrouped)"),
                                    )
                                    .show_ui(ui, |ui| {
                                        if ui.selectable_label(s.primary_group.is_none(), "(ungrouped)").clicked() {
                                            s.primary_group = None;
                                        }
                                        for g in &channel_groups {
                                            if ui.selectable_label(s.primary_group == Some(g.id), &g.name).clicked() {
                                                s.primary_group = Some(g.id);
                                                s.groups.insert(g.id);
                                            }
                                        }
                                    })
                                    .response
                                    .on_hover_text(
                                        "Which group this channel clusters under in the Streams \
                                         grid's default view. A channel can also belong to other \
                                         groups (below) — those just don't drive the default \
                                         clustering, only the group filter.",
                                    );
                                ui.end_row();

                                ui.label("Also in these groups");
                                ui.vertical(|ui| {
                                    if channel_groups.is_empty() {
                                        ui.weak("No groups yet — create one from the Streams toolbar.");
                                    }
                                    for g in &channel_groups {
                                        let mut member = s.groups.contains(&g.id);
                                        let is_primary = s.primary_group == Some(g.id);
                                        ui.add_enabled_ui(!is_primary, |ui| {
                                            let resp = ui.checkbox(&mut member, &g.name);
                                            if is_primary {
                                                resp.on_hover_text("Already included as the primary group above.");
                                            }
                                        });
                                        if !is_primary {
                                            if member {
                                                s.groups.insert(g.id);
                                            } else {
                                                s.groups.remove(&g.id);
                                            }
                                        }
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "Secondary memberships — this channel shows up when the \
                                     Streams grid is filtered to any of these groups too, not \
                                     just its primary one.",
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
                    });
                });
            },
        );

        let (do_save, closed) = {
            let mut f = form_arc.lock().unwrap();
            let result = (f.do_save, f.closed);
            // Consume: a failed-validation Save (name empty) must not keep
            // re-triggering every subsequent call, and must not permanently
            // block Cancel from ever taking effect.
            f.do_save = false;
            result
        };

        if do_save {
            let f = form_arc.lock().unwrap();
            let name = f.name.trim().to_string();
            if name.is_empty() {
                self.status = "Name is required.".into();
            } else {
                let id_opt = f.id;
                let color = f.color.trim().to_string();
                let platform_pref = f.primary_platform_pref;
                let simulcast_scope = crate::simulcast::SimulcastScope {
                    pref: f.simulcast_pref,
                    ad_free_pref: f.simulcast_ad_free_pref,
                };
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
                    // No per-channel/instance gap-splice-cleanup override UI
                    // yet — always inherits the global setting for now.
                    gap_splice_cleanup: None,
                    rolling: f.rolling,
                    rolling_ttl_secs: crate::rolling::hours_field_to_secs(&f.rolling_ttl_hours),
                };
                let chapters_scope = crate::chapters::ChaptersScope {
                    enabled: f.chapters_enabled,
                    coalesce_secs: f.chapters_coalesce_secs.trim().parse().ok(),
                };
                let target_groups = f.groups.clone();
                let target_primary = f.primary_group;
                let res = match id_opt {
                    Some(id) => {
                        let old_name = self.channels.iter().find(|c| c.id == id).map(|c| c.name.clone());
                        let r = self
                            .core
                            .store
                            .rename_channel(id, &name)
                            .and_then(|()| self.core.store.set_channel_color(id, &color))
                            .map(|()| id);
                        // The asset cache tree is keyed by display name, not id —
                        // follow the rename so avatar/banner/emotes/Twitch name-colour
                        // don't silently orphan under the old name.
                        if r.is_ok()
                            && let Some(old_name) = old_name
                            && old_name != name
                        {
                            crate::assets::rename_channel_asset_dir(&old_name, &name);
                        }
                        r
                    }
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
                        let _ = crate::chapters::save_channel_chapters_scope(
                            &self.core.store,
                            cid,
                            &chapters_scope,
                        );
                        let _ = crate::raid_follow::save_bool_scope(
                            &self.core.store,
                            crate::raid_follow::K_CHANNEL_RAID_FOLLOW_SCOPE,
                            cid,
                            f.follow_my_raids,
                        );
                        let _ = crate::raid_follow::save_bool_scope(
                            &self.core.store,
                            crate::raid_follow::K_CHANNEL_RAID_TARGET_SCOPE,
                            cid,
                            f.record_me_as_raid_target,
                        );
                        let _ = crate::raid_follow::save_bool_scope(
                            &self.core.store,
                            crate::raid_follow::K_CHANNEL_RAID_FOLLOW_PLAY_SCOPE,
                            cid,
                            f.follow_my_raids_play,
                        );
                        let _ = crate::raid_follow::save_bool_scope(
                            &self.core.store,
                            crate::raid_follow::K_CHANNEL_RAID_PLAY_EXCLUDE_SCOPE,
                            cid,
                            f.exclude_from_auto_play,
                        );
                        let _ = crate::raid_follow::save_bool_scope(
                            &self.core.store,
                            crate::manual_delete::K_CHANNEL_ALLOW_DELETE,
                            cid,
                            Some(f.allow_delete),
                        );
                        // Diff target membership against what's currently saved
                        // (empty for a brand-new channel) rather than blindly
                        // clearing + re-inserting — cheaper, and avoids
                        // needlessly bouncing `primary_group_id` through NULL
                        // for a group that isn't actually changing.
                        let current_groups: std::collections::HashSet<i64> = self
                            .core
                            .store
                            .channel_groups_for_channel(cid)
                            .unwrap_or_default()
                            .into_iter()
                            .collect();
                        for gid in current_groups.difference(&target_groups) {
                            let _ = self.core.store.set_channel_group_member(cid, *gid, false);
                        }
                        for gid in target_groups.difference(&current_groups) {
                            let _ = self.core.store.set_channel_group_member(cid, *gid, true);
                        }
                        let _ = self.core.store.set_channel_primary_group(cid, target_primary);
                        let _ = crate::platform_pref::save_channel_primary_platform(
                            &self.core.store,
                            cid,
                            platform_pref,
                        );
                        let _ = crate::simulcast::save_channel_simulcast_scope(
                            &self.core.store,
                            cid,
                            &simulcast_scope,
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
        } else if closed {
            self.channel_form = None;
        }
    }

    /// "Manage groups" dialog: create/rename/delete channel groups.
    /// Assigning a *channel* to groups happens in that channel's own
    /// Properties dialog ([`Self::channel_form_window`]) — this window only
    /// manages the groups themselves.
    pub(super) fn group_manager_window(&mut self, ctx: &egui::Context) {
        if !self.show_group_manager {
            return;
        }
        // Snapshot/take everything the closure needs as plain locals up
        // front — same reasoning as `channel_form_window`'s `f`/`channel_groups`
        // locals: a nested `egui::Window`/`egui::Grid` closure borrowing
        // several different `self` fields (some mutably) at once is exactly
        // the shape the borrow checker can't reason through, closure capture
        // or not.
        let mut open = true;
        let groups = self.channel_groups.clone();
        let member_counts: HashMap<i64, usize> = groups
            .iter()
            .map(|g| (g.id, self.core.store.channel_ids_in_group(g.id).map(|s| s.len()).unwrap_or(0)))
            .collect();
        let mut new_name = std::mem::take(&mut self.group_manager_new_name);
        let mut rename_state = self.group_manager_rename.take();
        let mut add_clicked = false;
        let mut renamed: Option<(i64, String)> = None;
        let mut cancel_rename = false;
        let mut deleted: Option<i64> = None;
        // Same shape, second section — recording groups.
        let rgroups = self.recording_groups.clone();
        let r_member_counts: HashMap<i64, usize> = rgroups
            .iter()
            .map(|g| (g.id, self.core.store.recording_ids_in_group(g.id).map(|s| s.len()).unwrap_or(0)))
            .collect();
        let mut r_new_name = std::mem::take(&mut self.recording_group_manager_new_name);
        let mut r_rename_state = self.recording_group_manager_rename.take();
        let mut r_add_clicked = false;
        let mut r_renamed: Option<(i64, String)> = None;
        let mut r_cancel_rename = false;
        let mut r_deleted: Option<i64> = None;

        egui::Window::new("Manage groups")
            .collapsible(false)
            .resizable(true)
            .default_width(320.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Channel groups").strong());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut new_name)
                            .hint_text("New group name")
                            .desired_width(200.0),
                    );
                    if ui.add_enabled(!new_name.trim().is_empty(), egui::Button::new("➕ Add")).clicked() {
                        add_clicked = true;
                    }
                });
                ui.separator();
                if groups.is_empty() {
                    ui.weak("No groups yet.");
                }
                egui::Grid::new("group_manager_grid")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        for g in &groups {
                            let count = member_counts.get(&g.id).copied().unwrap_or(0);
                            if rename_state.as_ref().is_some_and(|(id, _)| *id == g.id) {
                                let draft = &mut rename_state.as_mut().unwrap().1;
                                let resp = ui.text_edit_singleline(draft);
                                let commit = (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                    || ui.small_button("✔").clicked();
                                if commit {
                                    renamed = Some((g.id, draft.trim().to_string()));
                                }
                                if ui.small_button("✕").clicked() {
                                    cancel_rename = true;
                                }
                            } else {
                                ui.label(format!("{} ({count})", g.name));
                                ui.horizontal(|ui| {
                                    if ui.small_button("✏").on_hover_text("Rename").clicked() {
                                        rename_state = Some((g.id, g.name.clone()));
                                    }
                                    if ui
                                        .small_button("🗑")
                                        .on_hover_text(
                                            "Delete this group. Channels in it are unaffected \
                                             (they just lose the grouping — this doesn't touch \
                                             any recordings/settings).",
                                        )
                                        .clicked()
                                    {
                                        deleted = Some(g.id);
                                    }
                                });
                            }
                            ui.end_row();
                        }
                    });

                ui.add_space(12.0);
                ui.separator();
                ui.label(egui::RichText::new("Recording groups").strong());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut r_new_name)
                            .hint_text("e.g. Numi Subathon 2025")
                            .desired_width(200.0),
                    );
                    if ui.add_enabled(!r_new_name.trim().is_empty(), egui::Button::new("➕ Add")).clicked() {
                        r_add_clicked = true;
                    }
                })
                .response
                .on_hover_text(
                    "A named tag spanning any number of streams — see the Streams grid's \
                     multi-select (\"➕ Add to group…\") for building one up.",
                );
                ui.separator();
                if rgroups.is_empty() {
                    ui.weak("No recording groups yet.");
                }
                egui::Grid::new("recording_group_manager_grid")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        for g in &rgroups {
                            let count = r_member_counts.get(&g.id).copied().unwrap_or(0);
                            if r_rename_state.as_ref().is_some_and(|(id, _)| *id == g.id) {
                                let draft = &mut r_rename_state.as_mut().unwrap().1;
                                let resp = ui.text_edit_singleline(draft);
                                let commit = (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                    || ui.small_button("✔").clicked();
                                if commit {
                                    r_renamed = Some((g.id, draft.trim().to_string()));
                                }
                                if ui.small_button("✕").clicked() {
                                    r_cancel_rename = true;
                                }
                            } else {
                                ui.label(format!("{} ({count} take(s))", g.name));
                                ui.horizontal(|ui| {
                                    if ui.small_button("✏").on_hover_text("Rename").clicked() {
                                        r_rename_state = Some((g.id, g.name.clone()));
                                    }
                                    if ui
                                        .small_button("🗑")
                                        .on_hover_text(
                                            "Delete this group. Recordings in it are unaffected \
                                             — this only drops the tag.",
                                        )
                                        .clicked()
                                    {
                                        r_deleted = Some(g.id);
                                    }
                                });
                            }
                            ui.end_row();
                        }
                    });
            });

        self.group_manager_new_name = new_name;
        if add_clicked {
            let name = self.group_manager_new_name.trim().to_string();
            match self.core.store.create_channel_group(&name) {
                Ok(_) => {
                    self.group_manager_new_name.clear();
                    self.reload_rows();
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
        }
        if let Some((id, name)) = renamed {
            if name.is_empty() {
                self.status = "Group name can't be empty.".into();
                self.group_manager_rename = rename_state;
            } else {
                if let Err(e) = self.core.store.rename_channel_group(id, &name) {
                    self.status = format!("Error: {e}");
                } else {
                    self.reload_rows();
                }
                self.group_manager_rename = None;
            }
        } else if cancel_rename {
            self.group_manager_rename = None;
        } else {
            self.group_manager_rename = rename_state;
        }
        if let Some(id) = deleted {
            if let Err(e) = self.core.store.delete_channel_group(id) {
                self.status = format!("Error: {e}");
            } else {
                if self.streams_group_filter == Some(id) {
                    self.streams_group_filter = None;
                    self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                }
                self.reload_rows();
            }
        }

        self.recording_group_manager_new_name = r_new_name;
        if r_add_clicked {
            let name = self.recording_group_manager_new_name.trim().to_string();
            match self.core.store.create_recording_group(&name) {
                Ok(_) => {
                    self.recording_group_manager_new_name.clear();
                    self.reload_rows();
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
        }
        if let Some((id, name)) = r_renamed {
            if name.is_empty() {
                self.status = "Group name can't be empty.".into();
                self.recording_group_manager_rename = r_rename_state;
            } else {
                if let Err(e) = self.core.store.rename_recording_group(id, &name) {
                    self.status = format!("Error: {e}");
                } else {
                    self.reload_rows();
                }
                self.recording_group_manager_rename = None;
            }
        } else if r_cancel_rename {
            self.recording_group_manager_rename = None;
        } else {
            self.recording_group_manager_rename = r_rename_state;
        }
        if let Some(id) = r_deleted {
            if let Err(e) = self.core.store.delete_recording_group(id) {
                self.status = format!("Error: {e}");
            } else {
                if self.streams_recording_group_filter == Some(id) {
                    self.streams_recording_group_filter = None;
                    self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                }
                self.reload_rows();
            }
        }
        if !open {
            self.show_group_manager = false;
        }
    }

    /// Snapshot the Streams grid's current sort/grouping/filters/group-filter
    /// selections into a saved view under `name` (creating or overwriting —
    /// see `saved_views::upsert_view`), and make it the active view.
    pub(super) fn save_current_streams_view(&mut self, name: &str) {
        let keys: Vec<(usize, bool)> =
            self.streams_sort.keys.iter().map(|l| (l.col, l.ascending)).collect();
        let view = SavedView {
            name: name.to_string(),
            sort: grid_columns::unresolve_sort(&STREAM_COLUMNS, &keys),
            group_visually: self.streams_group_visually,
            filters: saved_views::unresolve_filters(&STREAM_COLUMNS, &self.streams_filters),
            channel_group_id: self.streams_group_filter,
            recording_group_id: self.streams_recording_group_filter,
        };
        saved_views::upsert_view(&self.core.store, GridTableId::Streams, view);
        self.streams_views = saved_views::list_views(&self.core.store, GridTableId::Streams);
        self.streams_active_view = Some(name.to_string());
    }

    /// Apply a saved view's sort/grouping/filters/group-filter selections to
    /// the live grid state, and persist the sort the same way any manual
    /// column-header sort change already does (see `channels_table`'s tail)
    /// — so the sort also becomes the ad hoc "current" sort a fresh session
    /// (with no view re-applied) would start from. No-op if `name` doesn't
    /// resolve to a saved view.
    pub(super) fn apply_streams_view(&mut self, name: &str) {
        let Some(view) = self.streams_views.iter().find(|v| v.name == name).cloned() else {
            return;
        };
        self.streams_sort = SortState {
            keys: grid_columns::resolve_sort(&STREAM_COLUMNS, &view.sort)
                .into_iter()
                .map(|(col, ascending)| SortLevel { col, ascending })
                .collect(),
        };
        grid_columns::save_sort(&self.core.store, GridTableId::Streams, &view.sort);
        self.streams_filters = saved_views::resolve_filters(&STREAM_COLUMNS, &view.filters);
        self.streams_group_visually = view.group_visually;
        let _ = self.core.store.set_setting(
            K_STREAMS_GROUP_VISUALLY,
            if view.group_visually { "1" } else { "0" },
        );
        self.streams_group_filter = view.channel_group_id;
        self.streams_recording_group_filter = view.recording_group_id;
        self.streams_active_view = Some(name.to_string());
        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
    }

    /// "Manage views" dialog: save the Streams grid's current sort/grouping/
    /// filters/group-filter selections as a named, reusable preset; apply,
    /// rename, update (overwrite with the current live state), or delete any
    /// saved view. Mirrors `group_manager_window`'s shape (snapshot locals up
    /// front, mutate `self` once after the window closure) — deletion has no
    /// confirmation dialog either, matching channel/recording groups.
    /// Body of the Streams toolbar's "Views" dropdown popup: an inline
    /// "save current as new" row, then one row per saved view — click the
    /// name to apply it, **💾** overwrites it with the grid's current state,
    /// **✏** renames it in place, **🗑** deletes it (no confirmation, same
    /// as channel/recording groups). Folded directly into the combo instead
    /// of a separate management window/button, since egui popups stay open
    /// across clicks inside themselves (only an outside click/Escape closes
    /// one) — there's no need for a whole extra window just to fit a text
    /// field and a handful of per-row buttons.
    pub(super) fn views_combo_popup(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.views_manager_new_name)
                    .hint_text("New view name")
                    .desired_width(140.0),
            );
            if ui
                .add_enabled(
                    !self.views_manager_new_name.trim().is_empty(),
                    egui::Button::new("💾"),
                )
                .on_hover_text("Save the grid's current sort/grouping/filters under this name")
                .clicked()
            {
                let name = self.views_manager_new_name.trim().to_string();
                if self.streams_views.iter().any(|v| v.name == name) {
                    self.status = format!("A view named \"{name}\" already exists.");
                } else {
                    self.save_current_streams_view(&name);
                    self.views_manager_new_name.clear();
                }
            }
        });
        ui.separator();
        if self.streams_views.is_empty() {
            ui.weak("No saved views yet");
            return;
        }
        let views = self.streams_views.clone();
        let active = self.streams_active_view.clone();
        let mut rename_state = self.views_manager_rename.take();
        for v in &views {
            if rename_state.as_ref().is_some_and(|(n, _)| n == &v.name) {
                ui.horizontal(|ui| {
                    let draft = &mut rename_state.as_mut().unwrap().1;
                    let resp = ui.text_edit_singleline(draft);
                    let commit = (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || ui.small_button("✔").clicked();
                    let cancel = ui.small_button("✕").clicked();
                    if commit {
                        let (old, new) = rename_state.take().unwrap();
                        let new = new.trim().to_string();
                        if new.is_empty() {
                            self.status = "View name can't be empty.".into();
                        } else if new != old && self.streams_views.iter().any(|v| v.name == new) {
                            self.status = format!("A view named \"{new}\" already exists.");
                        } else {
                            saved_views::rename_view(&self.core.store, GridTableId::Streams, &old, &new);
                            self.streams_views =
                                saved_views::list_views(&self.core.store, GridTableId::Streams);
                            if self.streams_active_view.as_deref() == Some(old.as_str()) {
                                self.streams_active_view = Some(new);
                            }
                        }
                    } else if cancel {
                        rename_state = None;
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    let is_active = active.as_deref() == Some(v.name.as_str());
                    let mut label = egui::RichText::new(&v.name);
                    if is_active {
                        label = label.strong();
                    }
                    if ui.selectable_label(is_active, label).on_hover_text("Apply this view").clicked() {
                        self.apply_streams_view(&v.name);
                    }
                    if ui
                        .small_button("💾")
                        .on_hover_text("Overwrite this view with the grid's current sort/grouping/filters")
                        .clicked()
                    {
                        self.save_current_streams_view(&v.name);
                    }
                    if ui.small_button("✏").on_hover_text("Rename").clicked() {
                        rename_state = Some((v.name.clone(), v.name.clone()));
                    }
                    if ui.small_button("🗑").on_hover_text("Delete this view").clicked() {
                        saved_views::delete_view(&self.core.store, GridTableId::Streams, &v.name);
                        self.streams_views =
                            saved_views::list_views(&self.core.store, GridTableId::Streams);
                        if self.streams_active_view.as_deref() == Some(v.name.as_str()) {
                            self.streams_active_view = None;
                        }
                    }
                });
            }
        }
        self.views_manager_rename = rename_state;
    }

    /// "Add to group…" dialog: bulk-adds every take of every stream in
    /// `selected_streams` to a recording group (existing pick, or a new one
    /// typed in). Closing/confirming clears the selection either way.
    pub(super) fn add_to_recording_group_window(&mut self, ctx: &egui::Context) {
        if self.add_to_recording_group.is_none() {
            return;
        }
        let n_sel = self.selected_streams.len();
        let mut open = true;
        let groups = self.recording_groups.clone();
        let mut pick = self.add_to_recording_group.as_ref().unwrap().pick;
        let mut new_name = std::mem::take(&mut self.add_to_recording_group.as_mut().unwrap().new_name);
        let mut confirm_clicked = false;

        egui::Window::new("Add to recording group")
            .collapsible(false)
            .resizable(true)
            .default_width(320.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Adding {n_sel} stream(s) (every take) to:"));
                ui.separator();
                if !groups.is_empty() {
                    egui::Grid::new("add_to_recgroup_grid").num_columns(1).show(ui, |ui| {
                        for g in &groups {
                            if ui.selectable_label(pick == Some(g.id), &g.name).clicked() {
                                pick = Some(g.id);
                                new_name.clear();
                            }
                            ui.end_row();
                        }
                    });
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    ui.label("New group:");
                    if ui
                        .add(egui::TextEdit::singleline(&mut new_name).hint_text("e.g. Numi Subathon 2025"))
                        .changed()
                        && !new_name.trim().is_empty()
                    {
                        pick = None;
                    }
                });
                ui.add_space(8.0);
                let can_confirm = pick.is_some() || !new_name.trim().is_empty();
                if ui.add_enabled(can_confirm, egui::Button::new("Add")).clicked() {
                    confirm_clicked = true;
                }
            });

        if confirm_clicked {
            let group_id = match pick {
                Some(gid) => Some(gid),
                None => match self.core.store.create_recording_group(new_name.trim()) {
                    Ok(gid) => Some(gid),
                    Err(e) => {
                        self.status = format!("Error: {e}");
                        None
                    }
                },
            };
            if let Some(gid) = group_id {
                // Take ids were captured at click time (see `selected_streams`'s
                // doc comment) — no re-resolution against the (expansion-gated)
                // frame cache needed, and no risk of silently dropping a
                // stream whose instance got collapsed in the meantime.
                let rec_ids: Vec<i64> = self.selected_streams.values().flatten().copied().collect();
                match self.core.store.add_recordings_to_group(&rec_ids, gid) {
                    Ok(()) => {
                        self.status = format!("Added {n_sel} stream(s) to the group.");
                        self.selected_streams.clear();
                        self.reload_rows();
                    }
                    Err(e) => self.status = format!("Error: {e}"),
                }
            }
            self.add_to_recording_group = None;
            return;
        }
        if let Some(d) = self.add_to_recording_group.as_mut() {
            d.pick = pick;
            d.new_name = new_name;
        }
        if !open {
            self.add_to_recording_group = None;
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
        self.streams_selection_bar(ui);
        let out = self.channels_table(ui, now, &active_ids);
        self.apply_streams_actions(ui, out, any_active);
    }

    /// Bar shown above the Streams grid while one or more Stream rows are
    /// multi-selected (ctrl/shift-click) — mirrors `schedule_selection_bar`'s
    /// shape. The only bulk action today is adding the selection to a
    /// recording group; more could land here later the same way.
    fn streams_selection_bar(&mut self, ui: &mut egui::Ui) {
        let n_sel = self.selected_streams.len();
        if n_sel == 0 {
            return;
        }
        let accent = ui.visuals().selection.bg_fill;
        ui.horizontal(|ui| {
            ui.colored_label(accent, format!("{n_sel} stream(s) selected"));
            ui.separator();
            if ui
                .button("➕ Add to group…")
                .on_hover_text(
                    "Add every take of the selected stream(s) to a recording group \
                     (new or existing) — e.g. \"Numi Subathon 2025\" spanning several \
                     broadcasts. Ctrl/shift-click more Stream rows to extend the \
                     selection first.",
                )
                .clicked()
            {
                self.add_to_recording_group = Some(AddToRecordingGroupDialog::default());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕ Clear").on_hover_text("Clear selection").clicked() {
                    self.selected_streams.clear();
                }
            });
        });
        ui.separator();
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
                // Name colour: manual custom colour > the streamer's own (cached)
                // Twitch colour > the automatic palette. Shared with the 🔔
                // notifications feed so a channel reads the same colour there.
                channel_name_colors.insert(cid, self.channel_name_color(cid));
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
            // for expanded instances inside expanded channels. `expanded_monitors`
            // itself stays scoped to true expansion (other lazy caches below —
            // ad-breaks, meta-change logs — are deliberately bounded by what's
            // actually visible).
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
            // `groups` doubles as the data "Only stored" / the Recording-group
            // filter scan to decide which channels/instances even qualify to be
            // shown (`build_vis_rows`'s `qualifying_monitors`) — a collapsed
            // monitor whose takes were never loaded here would look like it has
            // nothing stored and get hidden outright, the filter deciding "no
            // match" from data it never fetched rather than an actual absence of
            // stored recordings. So while either filter is active, `groups` (and
            // the `rec_cache` it reads from) covers every monitor, not just the
            // expanded ones — the cache-rebuild `stamp` gate above still means
            // this only reruns on an actual change (reload/expansion/F5/settings
            // save), not every frame.
            let filter_active =
                self.streams_only_recorded || self.streams_recording_group_filter.is_some();
            let monitors_to_group: Vec<i64> = if filter_active {
                self.rows.iter().map(|r| r.monitor.id).collect()
            } else {
                expanded_monitors.clone()
            };
            for &mid in &monitors_to_group {
                if !self.rec_cache.contains_key(&mid) {
                    let recs = self
                        .core
                        .store
                        .recordings_for_monitor(mid)
                        .unwrap_or_default();
                    self.rec_cache.insert(mid, recs);
                }
            }
            let groups: HashMap<i64, Vec<StreamGroup>> = monitors_to_group
                .iter()
                .map(|&mid| {
                    let recs = self.rec_cache.get(&mid).map(Vec::as_slice).unwrap_or(&[]);
                    (mid, group_recordings(recs))
                })
                .collect();

            // Cheap global fallback for the active-take lookups above: unlike
            // `groups`, not gated on expansion, so a collapsed instance row's
            // live capture is still findable.
            let mut active_recordings: HashMap<i64, Vec<crate::models::Recording>> = HashMap::new();
            for r in self.core.store.recordings_marked_recording().unwrap_or_default() {
                active_recordings.entry(r.monitor_id).or_default().push(r);
            }

            // Lowercase Twitch login -> monitor id, for resolving a collab
            // partner (known only by login) to a locally-tracked monitor.
            let mut twitch_login_to_mid: HashMap<String, i64> = HashMap::new();
            for r in &self.rows {
                if r.monitor.platform() == Platform::Twitch
                    && let Some(login) = crate::detectors::twitch_login(&r.monitor.url)
                {
                    twitch_login_to_mid.insert(login, r.monitor.id);
                }
            }

            // Keyed on `streams_cache_rev`, NOT the per-second stamp — same
            // reasoning as `deep_filter_texts` below, but this one mattered
            // most: `latest_raid_outs_all` was ~100 ms *with the DB lock held*
            // on a real library (see migration 87, which brings that down to
            // sub-millisecond), and re-running it every second during a
            // capture stalled the UI thread and every background thread
            // waiting on the same lock. A raid can only appear via EventSub,
            // which reloads the grid and bumps the rev.
            if self.raid_out_cache.as_ref().map(|(rev, _)| *rev) != Some(self.streams_cache_rev) {
                let fresh = self.core.store.latest_raid_outs_all().unwrap_or_default();
                self.raid_out_cache = Some((self.streams_cache_rev, fresh));
            }
            let latest_raid_out = self
                .raid_out_cache
                .as_ref()
                .map(|(_, m)| m.clone())
                .unwrap_or_default();
            // Same rev-keyed treatment (and same reason) as `raid_out_cache`:
            // one small indexed query per grid rebuild, never per frame.
            if self.rolling_counts_cache.as_ref().map(|(rev, _)| *rev) != Some(self.streams_cache_rev)
            {
                let fresh = self.core.store.rolling_counts_by_monitor().unwrap_or_default();
                self.rolling_counts_cache = Some((self.streams_cache_rev, fresh));
            }
            let rolling_counts = self
                .rolling_counts_cache
                .as_ref()
                .map(|(_, m)| m.clone())
                .unwrap_or_default();

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
            // Per-monitor viewer/event stats (one query pair per monitor,
            // not per take) — powers the take-row 👁 badge and the Recording
            // Properties "Viewer stats" section. Bounded by expansion like
            // the ad-break/meta-change caches above (a take row only ever
            // renders under an expanded instance).
            for &mid in &expanded_monitors {
                if !self.take_stats_cache.contains_key(&mid) {
                    let v = self
                        .core
                        .store
                        .stream_stats_for_monitor(mid, 0)
                        .unwrap_or_default();
                    self.take_stats_cache.insert(mid, v);
                }
            }

            // Preferred-platform-when-multiple-live config: loaded once per
            // rebuild (not per channel row per frame — see `PlatformPrefCtx`).
            let platform_pref = crate::platform_pref::PlatformPrefCtx::load(&self.core.store);

            // Per-monitor logged title/category history for the deep filter —
            // covers stream/take rows of monitors that were never expanded
            // (`rec_cache` only loads those on expansion). Cached against
            // `streams_cache_rev`, NOT the per-second stamp: while a capture
            // is active this rebuild runs every second, and the history only
            // changes when the data reloads (which bumps the rev).
            if self.deep_filter_texts.as_ref().map(|(rev, _)| *rev) != Some(self.streams_cache_rev)
            {
                let texts = self.core.store.monitor_meta_filter_texts().unwrap_or_default();
                self.deep_filter_texts = Some((self.streams_cache_rev, texts));
            }
            let rec_texts = self
                .deep_filter_texts
                .as_ref()
                .map(|(_, t)| t)
                .cloned()
                .unwrap_or_default();

            // Which live instances are standing by for a sibling that's
            // recording this broadcast instead (see `crate::simulcast`), and
            // which platform took it. Derived rather than plumbed out of the
            // supervisor: same facts, no new shared state, and correct after a
            // restart. Deliberately narrower than the supervisor's own
            // decision — only an actually-running sibling capture counts — so
            // the badge never claims a standby that isn't visibly happening.
            let simulcast_standby: HashMap<i64, String> = {
                let ctx = crate::simulcast::SimulcastCtx::load(&self.core.store);
                let mut out = HashMap::new();
                for e in &chan_entries {
                    if e.rows.len() < 2 {
                        continue;
                    }
                    let mons: Vec<&MonitorWithChannel> =
                        e.rows.iter().map(|&i| &self.rows[i]).collect();
                    for m in &mons {
                        let mid = m.monitor.id;
                        if active_ids.contains(&mid)
                            || m.monitor.last_state != "live"
                            || !m.auto_record_on()
                            || !m.automation_on()
                        {
                            continue;
                        }
                        let policy = ctx.policy_for(e.channel.id, mid);
                        // Ad-free override, same rule the decision uses: the
                        // instance on that platform has to be live AND ad-free.
                        let ad_free_live = |p: crate::models::Platform| {
                            mons.iter().any(|s| {
                                s.monitor.platform() == p
                                    && (s.monitor.ad_free || s.ad_free_sub == Some(true))
                                    && (active_ids.contains(&s.monitor.id)
                                        || s.monitor.last_state == "live")
                            })
                        };
                        let pref = policy
                            .ad_free_pref
                            .filter(|p| ad_free_live(*p))
                            .or(policy.pref);
                        let Some(pref) = pref.filter(|p| *p != m.monitor.platform()) else {
                            continue;
                        };
                        if mons.iter().any(|s| {
                            s.monitor.platform() == pref && active_ids.contains(&s.monitor.id)
                        }) {
                            out.insert(mid, pref.label().to_string());
                        }
                    }
                }
                out
            };

            // Channel-level sort/filter model (one entry per top-level channel row).
            let model: Vec<Vec<Cell>> = chan_entries
                .iter()
                .map(|e| {
                    let mons: Vec<&MonitorWithChannel> =
                        e.rows.iter().map(|&i| &self.rows[i]).collect();
                    channel_cells(&e.channel, &mons, active_ids, now, &platform_pref, &rec_texts)
                })
                .collect();

            self.streams_cache = Some(StreamsViewCache {
                stamp,
                chan_entries,
                channel_avatars,
                instance_avatars,
                channel_name_colors,
                groups,
                active_recordings,
                twitch_login_to_mid,
                latest_raid_out,
                rolling_counts,
                model,
                platform_pref,
                simulcast_standby,
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
        let selected_streams = self.selected_streams.clone();
        let period_toggles = self.period_toggles.clone();
        let collapsed_channel_groups = self.collapsed_channel_groups.clone();
        let group_names: HashMap<i64, String> =
            self.channel_groups.iter().map(|g| (g.id, g.name.clone())).collect();
        // Group filter: resolves the selected group's membership every frame
        // while active — a small indexed query (this whole table already
        // re-sorts/re-flattens the full tree every frame; see `build_vis_rows`
        // below), so it's negligible next to that.
        let group_filter_members: Option<(i64, HashSet<i64>)> = self.streams_group_filter.map(|gid| {
            (gid, self.core.store.channel_ids_in_group(gid).unwrap_or_default())
        });
        let recording_group_filter_members: Option<HashSet<i64>> = self
            .streams_recording_group_filter
            .map(|gid| self.core.store.recording_ids_in_group(gid).unwrap_or_default());
        // "Only stored" toolbar checkbox: reuses the SAME filter mechanism as
        // the Recording group dropdown (`build_vis_rows`'s single
        // `recording_group_filter` param) rather than a parallel one — a
        // take id set restricted to ones with a file on disk, intersected
        // with any active Recording group so the two combine sensibly
        // instead of one silently overriding the other. In-memory only
        // (`cache.groups`, no DB call): cheap next to the rest of this
        // function's per-frame work.
        let recording_group_filter_members: Option<HashSet<i64>> = if self.streams_only_recorded {
            let stored_ids: HashSet<i64> = self
                .streams_cache
                .as_ref()
                .map(|c| {
                    c.groups
                        .values()
                        .flat_map(|grps| grps.iter())
                        .flat_map(|g| g.takes.iter())
                        .filter(|t| !t.output_path.is_empty())
                        .map(|t| t.id)
                        .collect()
                })
                .unwrap_or_default();
            Some(match recording_group_filter_members {
                Some(named_group) => named_group.intersection(&stored_ids).copied().collect(),
                None => stored_ids,
            })
        } else {
            recording_group_filter_members
        };
        let current_recording_group: Option<(i64, String)> = self.streams_recording_group_filter.and_then(|gid| {
            self.recording_groups.iter().find(|g| g.id == gid).map(|g| (gid, g.name.clone()))
        });
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
        // Subscriber-only broadcasts being archived from the CDN — no capture
        // process, so they are absent from `active_ids`, but they are being
        // recorded and the row has to say so.
        let cdn_captures = self.core.cdn_captures.lock().unwrap().clone();
        let cdn_capture_ids: HashSet<i64> = cdn_captures.keys().copied().collect();
        // …and the anchor TAKE of each, so the stream row for the broadcast
        // actually being extended can say so. `gated` alone marks every
        // subscriber-only stream ever archived; this marks the live one.
        let cdn_capture_recs: HashSet<i64> = cdn_captures.values().map(|c| c.rec_id).collect();
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
        let active_recordings = &cache.active_recordings;
        let twitch_login_to_mid = &cache.twitch_login_to_mid;
        let latest_raid_out = &cache.latest_raid_out;
        let model = &cache.model;
        let ad_breaks = &self.ad_break_cache;
        let meta_logs = &self.meta_change_cache;
        let take_stats = &self.take_stats_cache;
        let mut sort = self.streams_sort.clone();
        let mut filters = self.streams_filters.clone();
        if filters.len() != STREAM_COLS {
            filters = vec![String::new(); STREAM_COLS];
        }
        // Active filters resolved for hit marking (row tints + matched-text
        // highlight), and the set of instances whose data — including the
        // collapsed stream history — contains the matches. Recomputed per
        // frame only while a filter is set; a plain grid skips all of it.
        let fhits = FilterHits::from_filters(&filters);
        let hit_instances: HashSet<i64> = match &fhits {
            Some(fh) => {
                let deep = self.deep_filter_texts.as_ref().map(|(_, t)| t);
                self.rows
                    .iter()
                    .filter(|r| fh.instance_hit(r, deep.and_then(|d| d.get(&r.monitor.id))))
                    .map(|r| r.monitor.id)
                    .collect()
            }
            None => HashSet::new(),
        };
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
                    &exp_channels, &exp_instances, &exp_streams, &period_toggles,
                    &self.channel_groups, &collapsed_channel_groups,
                    self.streams_group_visually,
                    group_filter_members.as_ref().map(|(gid, members)| (*gid, members)),
                    recording_group_filter_members.as_ref(),
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
                    self.reorder_columns = Some(Arc::new(Mutex::new(ReorderColumnsState {
                        table: GridTableId::Streams,
                        draft: entries.clone(),
                        apply: false,
                        cancel: false,
                    })));
                }
                // The Layout submenu's saved-layouts list. Read once per grid
                // rebuild (keyed on `streams_cache_rev`, same shape as
                // `raid_out_cache`), not once per frame: the render path
                // should never take the DB lock at mouse-move repaint rates,
                // however cheap the individual query looks.
                let layouts_rev = self.saved_layouts_cache.as_ref().map(|(rev, _)| *rev);
                if layouts_rev != Some(self.streams_cache_rev) {
                    let fresh = crate::layout::list_layouts(&self.core.store);
                    self.saved_layouts_cache = Some((self.streams_cache_rev, fresh));
                }
                let saved_layouts = self
                    .saved_layouts_cache
                    .as_ref()
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                table.body(|body| {
                    // Virtualized: only the rows in view are laid out — the old
                    // per-row loop rebuilt every widget of every row each frame.
                    body.rows(24.0, vis.len(), |mut tr| {
                        match vis[tr.index()] {
                            Vis::ChannelGroup { group_id, count, expanded } => {
                                Self::group_row(
                                    &mut tr, group_id, &group_names, count, expanded,
                                    &col_order, &mut out,
                                );
                            }
                            Vis::Channel(ci) => {
                                Self::channel_row(
                                    &mut tr, &chan_entries[ci], &self.rows, groups,
                                    channel_avatars, channel_name_colors, twitch_login_to_mid,
                                    &cache.rolling_counts, &ptex,
                                    active_ids, &finalizing_ids, &active_chat_ids,
                                    &ad_running, &exp_channels, now, sel_color,
                                    status_bgcolor, &col_order, &self.spark_data,
                                    fhits.as_ref(), &mut out, &cache.platform_pref,
                                    self.collab_title_in_name,
                                );
                            }
                            Vis::Instance { row: ri, depth } => {
                                Self::instance_row(
                                    &mut tr, &self.rows[ri], depth, groups,
                                    active_recordings, twitch_login_to_mid,
                                    &self.rows, channel_name_colors, latest_raid_out,
                                    &mut self.fs_probes.lock().unwrap(), &self.settings,
                                    &self.scheduled_recordings, &ptex, now, active_ids,
                                    &finalizing_ids, &cdn_capture_ids, &active_chat_ids,
                                    selected_monitor,
                                    &exp_instances, instance_avatars,
                                    cache.rolling_counts.get(&self.rows[ri].monitor.id).copied().unwrap_or(0),
                                    cache
                                        .simulcast_standby
                                        .get(&self.rows[ri].monitor.id)
                                        .map(String::as_str),
                                    &stop_holds_snapshot, &ad_running, sel_color,
                                    status_bgcolor, &col_order, &self.spark_data,
                                    hit_instances.contains(&self.rows[ri].monitor.id),
                                    fhits.as_ref(),
                                    self.collab_title_in_name,
                                    &saved_layouts,
                                    &mut out,
                                );
                            }
                            Vis::Period { mid, kind, gi_start, gi_end, depth, expanded } => {
                                Self::period_row(
                                    &mut tr, mid, kind, &groups[&mid][gi_start..gi_end], depth,
                                    expanded, &mut self.fs_probes.lock().unwrap(), now, &col_order, &mut out,
                                );
                            }
                            Vis::Stream { mid, gi, depth } => {
                                Self::stream_row(
                                    &mut tr, &groups[&mid][gi], mid, depth, &self.rows,
                                    &mut self.fs_probes.lock().unwrap(), &self.settings,
                                    &self.background_tasks, &finalizing_recs,
                                    &cdn_capture_recs, ad_breaks,
                                    meta_logs, &self.collab_by_stream, &exp_streams,
                                    &selected_streams, sel_color,
                                    current_recording_group.as_ref().map(|(id, name)| (*id, name.as_str())),
                                    now,
                                    &col_order, &self.rec_alert_badges,
                                    active_ids, &finalizing_ids, fhits.as_ref(),
                                    &self.core, &self.manual_delete_pending, &mut out,
                                );
                            }
                            Vis::Take { mid, gi, ti, depth } => {
                                Self::take_row(
                                    &mut tr, &groups[&mid][gi], ti, depth, &self.rows,
                                    mid, &self.core, &mut self.status,
                                    &mut self.fs_probes.lock().unwrap(), &self.settings,
                                    &self.background_tasks, &finalizing_recs,
                                    &cdn_capture_recs, ad_breaks,
                                    meta_logs, &self.collab_by_stream,
                                    &mut self.rename_rec_id, &mut self.rename_draft,
                                    &mut self.rename_preview,
                                    &mut self.show_rename_dialog, now, &col_order,
                                    &self.rec_alert_badges,
                                    active_ids, &finalizing_ids, fhits.as_ref(),
                                    &self.manual_delete_pending, take_stats, &mut out,
                                );
                            }
                            Vis::VodJob { mid, gi, ti, kind, depth } => {
                                Self::vod_job_row(
                                    &mut tr, &groups[&mid][gi], ti, kind, depth,
                                    &self.background_tasks, &vid_progress,
                                    &mut self.fs_probes.lock().unwrap(), &col_order, &mut out,
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
    /// "Play all collab instances (current downloads)" ▸ a Layout submenu
    /// entry — shared by [`apply_streams_actions`]' own `RowActions` dispatch
    /// and the Custom layout editor's "Apply now"/"Save as preset…" buttons
    /// ([`layout_editor_window`]), which resolve to the same
    /// `(Vec<StreamTarget>, LayoutChoice)` shape and just call this directly
    /// instead of round-tripping through a row-click action.
    pub(super) fn dispatch_play_collab_current(
        &mut self,
        targets: Vec<StreamTarget>,
        choice: crate::layout::LayoutChoice,
    ) {
        let player = self.settings.media_player_path.trim().to_string();
        if player.is_empty() {
            return;
        }
        let monitors = crate::display::enumerate_monitors();
        let rects = crate::layout::resolve_choice(&choice, &self.core.store, &monitors, targets.len());
        for (t, rect) in targets.into_iter().zip(rects) {
            let mut cmd = build_player_command(&player, &t);
            let win32_rect = apply_tile_or_geometry(&mut cmd, &player, Some(rect));
            if let Some(msg) = spawn_logged(cmd, "stream in player", None, win32_rect) {
                self.status = msg;
            }
        }
    }

    /// "Play all collab instances (live edge)" ▸ a Layout submenu entry —
    /// see [`Self::dispatch_play_collab_current`]'s doc comment, same shared-
    /// dispatch rationale.
    pub(super) fn dispatch_play_collab_live_edge(
        &mut self,
        source_mid: i64,
        partner_mids: Vec<i64>,
        untracked: Vec<UntrackedCollabPartner>,
        choice: crate::layout::LayoutChoice,
    ) {
        let player = self.settings.media_player_path.trim().to_string();
        if player.is_empty() {
            return;
        }
        let meta = crate::ui::player::LiveMetaCtx::from_core(&self.core);
        let mute_partners = self.settings.mute_collab_instances;
        let untracked_template = self.settings.collab_untracked_title_template.trim().to_string();
        let untracked_override = (!untracked_template.is_empty()).then_some(untracked_template.as_str());
        let n = 1 + partner_mids.len() + untracked.len();
        let monitors = crate::display::enumerate_monitors();
        let mut rects = crate::layout::resolve_choice(&choice, &self.core.store, &monitors, n).into_iter();
        // The clicked-on instance always keeps its own audio — only the
        // OTHER angles (tracked partners, then untracked ones) respect the
        // mute setting.
        if let Some(row) = self.rows.iter().find(|r| r.monitor.id == source_mid)
            && let Some(msg) = spawn_play_new_instance(
                row, &player, &self.settings, &self.core.store, false, None, meta.as_ref(), false,
                rects.next(),
            )
        {
            self.status = msg;
        }
        for mid in partner_mids {
            let rect = rects.next();
            if let Some(row) = self.rows.iter().find(|r| r.monitor.id == mid)
                && let Some(msg) = spawn_play_new_instance(
                    row, &player, &self.settings, &self.core.store, mute_partners, None,
                    meta.as_ref(), false, rect,
                )
            {
                self.status = msg;
            }
        }
        if !untracked.is_empty()
            && let Some(source_row) = self.rows.iter().find(|r| r.monitor.id == source_mid)
        {
            for partner in untracked {
                let rect = rects.next();
                if let Some(msg) = spawn_play_collab_partner(
                    source_row, &partner, &player, &self.settings, &self.core.store, mute_partners,
                    untracked_override, meta.as_ref(), rect,
                ) {
                    self.status = msg;
                }
            }
        }
    }

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
            toggle_period,
            toggle_channel_group,
            bulk_set_group_enabled,
            bulk_set_group_automation,
            toggle_select_stream,
            select_only_stream,
            remove_from_recording_group,
            open_path,
            open_in_player,
            play_new_instance_mid,
            mark_started_stream,
            copy_text,
            delete_recording,
            delete_recording_file,
            delete_stream_files,
            open_recording_props,
            open_recover_take,
            archive_vod_now,
            backfill_missed_vod_now,
            scan_for_missed_streams,
            play_vod_now,
            open_vod_webpage,
            backfill_head_now,
            abort_backfill,
            retrigger_chapters,
            set_err_ack,
            view_chat_rec,
            toggle_channel_enabled,
            toggle_channel_automation,
            rename_channel,
            merge_channel,
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
            open_warnings,
        } = out;
        if open_warnings {
            self.show_warnings = true;
            self.warn_refreshed = None; // force an immediate refresh
        }
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
            self.show_hype_mark = Some(Arc::new(Mutex::new(HypeMarkDraft {
                channel: cid,
                mins_ago: self.hype_mark_mins_ago,
                abs: String::new(),
                dur: self.hype_mark_dur,
                do_mark: false,
                closed: false,
            })));
        }
        if let Some(rec_id) = open_recover_take {
            self.open_recover_vod_from_seed(rec_id);
        }
        if let Some(rec_id) = archive_vod_now {
            self.core.manual(ManualCommand::ArchiveVodNow(rec_id));
            self.status = "Downloading published VOD…".into();
        }
        if let Some(rec_id) = backfill_missed_vod_now {
            self.core.manual(ManualCommand::BackfillMissedVodNow(rec_id));
            self.status = "Backfilling missed stream…".into();
        }
        if let Some(monitor_id) = scan_for_missed_streams {
            self.core.manual(ManualCommand::ScanForMissedStreams(monitor_id));
            self.status = "Scanning for missed streams…".into();
        }
        if let Some(rec_id) = play_vod_now {
            self.core.manual(ManualCommand::PlayVodNow(rec_id));
            self.status = "Resolving VOD to play…".into();
        }
        if let Some(rec_id) = open_vod_webpage {
            self.core.manual(ManualCommand::OpenVodWebpage(rec_id));
            self.status = "Resolving VOD webpage…".into();
        }
        if let Some(rec_id) = backfill_head_now.or_else(|| acts.backfill_head.take()) {
            self.core.manual(ManualCommand::BackfillHeadNow(rec_id));
            self.status = "Backfilling head…".into();
        }
        if let Some(rec_id) = abort_backfill {
            self.core.manual(ManualCommand::AbortHeadBackfill(rec_id));
            self.status = "Aborting backfill…".into();
        }
        if let Some(rec_id) = retrigger_chapters {
            self.core.manual(ManualCommand::RetriggerChapters(rec_id));
            self.status = "Embedding chapters…".into();
        }
        if let Some((rec_id, ack)) = set_err_ack {
            if let Err(e) = self.core.store.set_recording_err_ack(rec_id, ack) {
                self.status = format!("Error: {e}");
            } else {
                self.reload_rows();
            }
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

        if toggle_channel.is_some()
            || toggle_instance.is_some()
            || toggle_stream.is_some()
            || toggle_period.is_some()
            || toggle_channel_group.is_some()
        {
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
        if let Some((k, ids)) = toggle_select_stream {
            if self.selected_streams.remove(&k).is_none() {
                self.selected_streams.insert(k, ids);
            }
        }
        if let Some((k, ids)) = select_only_stream {
            self.selected_streams.clear();
            self.selected_streams.insert(k, ids);
        }
        if let Some((gid, ids)) = remove_from_recording_group {
            if let Err(e) = self.core.store.remove_recordings_from_group(&ids, gid) {
                self.status = format!("Error: {e}");
            } else {
                self.reload_rows();
            }
        }
        if let Some(k) = toggle_period {
            // `period_toggles` records DEVIATIONS from the computed default
            // (see `period_open`), not the open state itself — same flip,
            // opposite meaning of presence.
            if !self.period_toggles.remove(&k) {
                self.period_toggles.insert(k);
            }
        }
        if let Some(gid) = toggle_channel_group {
            // Presence = collapsed — opposite convention from `period_toggles`
            // (a group defaults OPEN; see `collapsed_channel_groups`'s doc
            // comment), so this is a plain flip, not a XOR-default dance.
            if !self.collapsed_channel_groups.remove(&gid) {
                self.collapsed_channel_groups.insert(gid);
            }
        }
        if let Some((gid, on)) = bulk_set_group_enabled {
            let members = self.core.store.channel_ids_in_group(gid).unwrap_or_default();
            for cid in members {
                let _ = self.core.store.set_channel_enabled(cid, on);
            }
            self.reload_rows();
        }
        if let Some((gid, on)) = bulk_set_group_automation {
            let members = self.core.store.channel_ids_in_group(gid).unwrap_or_default();
            for cid in members {
                let _ = self.core.store.set_channel_automation_enabled(cid, on);
            }
            self.reload_rows();
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
                let smsc = crate::simulcast::load_monitor_simulcast_scope(&self.core.store, r.monitor.id);
                mf.simulcast_pref = smsc.pref;
                mf.simulcast_ad_free_pref = smsc.ad_free_pref;
                let mchsc = crate::chapters::load_monitor_chapters_scope(&self.core.store, r.monitor.id);
                mf.chapters_enabled = mchsc.enabled;
                mf.chapters_coalesce_secs = mchsc.coalesce_secs.map(|v| v.to_string()).unwrap_or_default();
                mf.follow_my_raids = crate::raid_follow::load_bool_scope(
                    &self.core.store,
                    crate::raid_follow::K_MONITOR_RAID_FOLLOW_SCOPE,
                    r.monitor.id,
                );
                mf.record_me_as_raid_target = crate::raid_follow::load_bool_scope(
                    &self.core.store,
                    crate::raid_follow::K_MONITOR_RAID_TARGET_SCOPE,
                    r.monitor.id,
                );
                mf.follow_my_raids_play = crate::raid_follow::load_bool_scope(
                    &self.core.store,
                    crate::raid_follow::K_MONITOR_RAID_FOLLOW_PLAY_SCOPE,
                    r.monitor.id,
                );
                mf.exclude_from_auto_play = crate::raid_follow::load_bool_scope(
                    &self.core.store,
                    crate::raid_follow::K_MONITOR_RAID_PLAY_EXCLUDE_SCOPE,
                    r.monitor.id,
                );
                mf.allow_delete = crate::manual_delete::monitor_gate_on(&self.core.store, r.monitor.id);
                self.form = Some(Arc::new(Mutex::new(mf)));
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
                self.form = Some(Arc::new(Mutex::new(MonitorForm::add_instance(
                    c,
                    &self.monitor_defaults,
                    &self.settings.default_output_dir,
                ))));
            }
        }
        if let Some(p) = acts.add_collab_instance {
            // Right-clicked an untracked-but-confirmed collab partner's name
            // (Name-cell suffix or 🤝 Collab column) → open the Add-stream
            // form pre-filled with their Twitch login/display name, same as
            // a manual "Add stream" but without retyping the URL.
            self.form = Some(Arc::new(Mutex::new(MonitorForm::from_collab_partner(
                &p.login,
                &p.name,
                &self.monitor_defaults,
                &self.settings.default_output_dir,
            ))));
        }
        if let Some(mid) = acts.move_instance {
            self.move_instance_dialog = Some(Arc::new(Mutex::new(MoveInstanceState {
                mid,
                dest: None,
                do_move: false,
                closed: false,
            })));
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
            self.confirm_delete = Some(ConfirmDialogState::open((id, name)));
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
                let smsc = crate::simulcast::load_channel_simulcast_scope(&self.core.store, cid);
                let chsc = crate::chapters::load_channel_chapters_scope(&self.core.store, cid);
                let chapters_coalesce_secs =
                    chsc.coalesce_secs.map(|v| v.to_string()).unwrap_or_default();
                let follow_my_raids = crate::raid_follow::load_bool_scope(
                    &self.core.store,
                    crate::raid_follow::K_CHANNEL_RAID_FOLLOW_SCOPE,
                    cid,
                );
                let record_me_as_raid_target = crate::raid_follow::load_bool_scope(
                    &self.core.store,
                    crate::raid_follow::K_CHANNEL_RAID_TARGET_SCOPE,
                    cid,
                );
                let follow_my_raids_play = crate::raid_follow::load_bool_scope(
                    &self.core.store,
                    crate::raid_follow::K_CHANNEL_RAID_FOLLOW_PLAY_SCOPE,
                    cid,
                );
                let exclude_from_auto_play = crate::raid_follow::load_bool_scope(
                    &self.core.store,
                    crate::raid_follow::K_CHANNEL_RAID_PLAY_EXCLUDE_SCOPE,
                    cid,
                );
                let allow_delete = crate::manual_delete::channel_gate_on(&self.core.store, cid);
                let primary_group = c.primary_group_id;
                let groups = self
                    .core
                    .store
                    .channel_groups_for_channel(cid)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                self.channel_form = Some(Arc::new(Mutex::new(ChannelForm {
                    id: Some(cid),
                    name: c.name.clone(),
                    color: c.color.clone(),
                    vod_download: sc.download,
                    vod_replace: sc.replace,
                    head_backfill_fetch: hbsc.fetch,
                    head_backfill_replace: hbsc.replace,
                    join_cleanup: dsc.join_cleanup,
                    disposal_method: dsc.method,
                    rolling: dsc.rolling,
                    rolling_ttl_hours: crate::rolling::secs_to_hours_field(dsc.rolling_ttl_secs),
                    primary_platform_pref: platform_pref,
                    simulcast_pref: smsc.pref,
                    simulcast_ad_free_pref: smsc.ad_free_pref,
                    chapters_enabled: chsc.enabled,
                    chapters_coalesce_secs,
                    follow_my_raids,
                    record_me_as_raid_target,
                    follow_my_raids_play,
                    exclude_from_auto_play,
                    allow_delete,
                    primary_group,
                    groups,
                    do_save: false,
                    closed: false,
                    channel_groups: Vec::new(),
                })));
            }
        }
        if let Some((cid, name)) = delete_channel {
            self.confirm_delete_channel = Some(ConfirmDialogState::open((cid, name)));
        }
        if let Some(cid) = merge_channel {
            self.merge_channel_dialog = Some(Arc::new(Mutex::new(MergeChannelState {
                src: cid,
                dest: None,
                do_merge: false,
                closed: false,
            })));
        }
        if let Some(cid) = clear_channel_err {
            if let Err(e) = self.core.store.clear_channel_errors(cid) {
                self.status = format!("Error: {e}");
            } else {
                self.reload_rows();
            }
        }
        if let Some(cid) = open_channel_props.or(acts.open_channel_props) {
            self.open_channel_properties(cid);
        }
        if let Some(id) = acts.start {
            self.core.manual(ManualCommand::Start { id, user_initiated: true });
            self.status = "Checking channel… will record if live.".into();
        }
        if let Some((id, hours)) = acts.stop {
            self.core.manual(ManualCommand::StopHoldFor {
                monitor_id: id,
                hours,
                allow_triggers: false,
            });
            self.status = match hours {
                Some(h) => format!("Stopping — auto-record held for {h} hours."),
                None => "Stopping — auto-record held until a new broadcast.".into(),
            };
        }
        if let Some((id, hours)) = acts.stop_allow_triggers {
            self.core.manual(ManualCommand::StopHoldFor {
                monitor_id: id,
                hours,
                allow_triggers: true,
            });
            self.status = match hours {
                Some(h) => format!(
                    "Stopping — auto-record held for {h} hours (trigger words can still fire)."
                ),
                None => "Stopping — auto-record held until a new broadcast (trigger words can still fire)."
                    .into(),
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
        if let Some((key, mid)) = mark_started_stream {
            self.mark_broadcast_started(&key, mid);
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
        // Shared by every play action below: lets a player opened for a channel
        // this app doesn't track (collab partner, raid target) fetch its
        // title/game from Helix after launch — see
        // `player::run_untracked_title_updater`. `None` before the app core has
        // started, in which case those windows keep their launch title.
        let meta = crate::ui::player::LiveMetaCtx::from_core(&self.core);
        if let Some(mid) = play_new_instance_mid.or(acts.play_new_instance.take()) {
            let player = self.settings.media_player_path.trim().to_string();
            if !player.is_empty()
                && let Some(row) = self.rows.iter().find(|r| r.monitor.id == mid)
                && let Some(msg) =
                    spawn_play_new_instance(
                        row, &player, &self.settings, &self.core.store, false, None, meta.as_ref(),
                        false, None,
                    )
            {
                self.status = msg;
            }
        }
        if let Some((targets, choice)) = acts.play_collab_all_current.take() {
            self.dispatch_play_collab_current(targets, choice);
        }
        if let Some((source_mid, partner_mids, untracked, choice)) = acts.play_collab_all_live_edge.take() {
            self.dispatch_play_collab_live_edge(source_mid, partner_mids, untracked, choice);
        }
        if let Some((source_mid, partner)) = acts.play_collab_partner_live_edge.take() {
            let player = self.settings.media_player_path.trim().to_string();
            let untracked_template = self.settings.collab_untracked_title_template.trim().to_string();
            let untracked_override =
                (!untracked_template.is_empty()).then_some(untracked_template.as_str());
            if !player.is_empty()
                && let Some(source_row) = self.rows.iter().find(|r| r.monitor.id == source_mid)
                && let Some(msg) = spawn_play_collab_partner(
                    source_row,
                    &partner,
                    &player,
                    &self.settings,
                    &self.core.store,
                    false,
                    untracked_override,
                    meta.as_ref(),
                    None,
                )
            {
                self.status = msg;
            }
        }
        if let Some((labels, targets)) = acts.open_layout_editor.take() {
            self.open_layout_editor(labels, targets);
        }
        if let Some(name) = acts.delete_saved_layout.take() {
            crate::layout::delete_layout(&self.core.store, &name);
            self.status = format!("Layout \"{name}\" deleted.");
            // Invalidate `saved_layouts_cache` (keyed on this rev).
            self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
        }
        if let Some(mid) = acts.follow_raid.take() {
            let player = self.settings.media_player_path.trim().to_string();
            if !player.is_empty()
                && let Some(row) = self.rows.iter().find(|r| r.monitor.id == mid)
                && let Ok(Some(raid)) = self.core.store.latest_raid_out(mid)
                && let Some(msg) = spawn_follow_raid(
                    row, &raid.detail, &raid.target, &player, &self.settings, &self.core.store,
                    meta.as_ref(),
                )
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
            self.rec_props_popups.retain(|p| p.lock().unwrap().rec_id != rid);
            self.reload_rows();
        }
        if let Some(rid) = delete_recording_file {
            let rec = self.rec_cache.values().flat_map(|v| v.iter()).find(|r| r.id == rid).cloned();
            if let Some(rec) = rec
                && let Some(channel_id) =
                    self.rows.iter().find(|r| r.monitor.id == rec.monitor_id).map(|r| r.channel.id)
            {
                let method = crate::disposal::effective_method_for_recording(
                    &self.core.store,
                    channel_id,
                    rec.monitor_id,
                    rid,
                );
                let channel_name = self
                    .channels
                    .iter()
                    .find(|c| c.id == channel_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                self.confirm_delete_file = Some(ConfirmDialogState::open(ConfirmDeleteFile {
                    rec_id: rid,
                    channel_id,
                    monitor_id: rec.monitor_id,
                    path: rec.output_path.clone(),
                    label: format!(
                        "{channel_name} — {}",
                        std::path::Path::new(&rec.output_path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "this recording".into())
                    ),
                    method,
                }));
            }
        }
        if let Some(rec_ids) = delete_stream_files
            && !rec_ids.is_empty()
        {
            let recs: Vec<crate::models::Recording> = self
                .rec_cache
                .values()
                .flat_map(|v| v.iter())
                .filter(|r| rec_ids.contains(&r.id))
                .cloned()
                .collect();
            if let Some(monitor_id) = recs.first().map(|r| r.monitor_id)
                && let Some(channel_id) =
                    self.rows.iter().find(|r| r.monitor.id == monitor_id).map(|r| r.channel.id)
            {
                let channel_name = self
                    .channels
                    .iter()
                    .find(|c| c.id == channel_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                let mut items: Vec<(i64, String, i64, crate::disposal::DisposalMethod)> = recs
                    .iter()
                    .map(|rec| {
                        let method = crate::disposal::effective_method_for_recording(
                            &self.core.store,
                            channel_id,
                            monitor_id,
                            rec.id,
                        );
                        (rec.id, rec.output_path.clone(), rec.bytes, method)
                    })
                    .collect();
                items.sort_by_key(|(rid, ..)| {
                    recs.iter().find(|r| r.id == *rid).map(|r| r.started_at).unwrap_or(0)
                });
                let started = recs.iter().map(|r| r.started_at).min().unwrap_or(0);
                self.confirm_delete_stream_files = Some(ConfirmDialogState::open(ConfirmDeleteStreamFiles {
                    channel_id,
                    monitor_id,
                    items,
                    label: format!("{channel_name} — {}", fmt_datetime_short(started)),
                }));
            }
        }
        if let Some(rid) = open_recording_props {
            // Seed the notes draft from the cached recording (already loaded).
            let notes = self
                .rec_cache
                .values()
                .flat_map(|v| v.iter())
                .find(|r| r.id == rid)
                .map(|r| r.notes.clone())
                .unwrap_or_default();
            self.open_recording_properties(rid, notes);
        }
    }

    /// Open a take's 📄 Properties window (no-op when it's already open).
    /// `notes` seeds the editable notes draft — the caller supplies it because
    /// the two callers read from different in-memory lists (Streams' per-monitor
    /// `rec_cache`, Backlog's flat `history_all`) and neither should hit the DB
    /// for a field it already has.
    pub(super) fn open_recording_properties(&mut self, rec_id: i64, notes: String) {
        if self.rec_props_popups.iter().any(|p| p.lock().unwrap().rec_id == rec_id) {
            return;
        }
        self.rec_props_popups.push(Arc::new(Mutex::new(RecPropsPopup {
            rec_id,
            notes,
            notes_dirty: false,
            closed: false,
        })));
    }

    /// Advance a broadcast's watch state to "started" because the user just
    /// opened/played it — never downgrades one already marked started/watched
    /// (see [`history::should_advance_to_started`]). Shared by the Streams take
    /// row and the Backlog row.
    pub(super) fn mark_broadcast_started(&mut self, key: &str, mid: i64) {
        let cur = self.core.store.stream_watch_state(key).ok().flatten().map(|(s, _)| s);
        if history::should_advance_to_started(cur.as_deref()) {
            let _ = self.core.store.set_stream_watch_state(key, mid, "started");
        }
    }

    /// Flatten the channel -> (instance) -> [year -> month -> week ->]
    /// stream -> take tree into the rows currently visible (respecting
    /// sort/filter order and expansion state). The year/month/week levels
    /// are inserted only where they'd actually group something — see
    /// [`period_levels_needed`].
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
        period_toggles: &HashSet<String>,
        channel_groups: &[crate::models::ChannelGroup],
        collapsed_channel_groups: &HashSet<i64>,
        // The Streams toolbar's "Group" checkbox — off flattens the list
        // exactly like an active `group_filter` does below, just without
        // narrowing membership.
        group_visually: bool,
        // `Some((group_id, members))` narrows to one group's members
        // (primary or secondary) and disables header clustering entirely —
        // see the toolbar's group filter dropdown.
        group_filter: Option<(i64, &HashSet<i64>)>,
        // `Some(recording_ids)` narrows to streams with at least one take in
        // the set — channels/instances with no qualifying stream are hidden
        // entirely, and the ones that remain force-expand down to their
        // matching streams regardless of `exp_channels`/`exp_instances`
        // (there's no point showing a match and then hiding it behind a
        // collapsed triangle the user has to know to click).
        recording_group_filter: Option<&HashSet<i64>>,
    ) -> Vec<Vis> {
        use chrono::Datelike;
        let mut order = ordered_rows(model, sort, filters);
        if let Some((_, members)) = group_filter {
            order.retain(|&ci| members.contains(&chan_entries[ci].channel.id));
        }
        let qualifying_monitors: Option<HashSet<i64>> = recording_group_filter.map(|rec_filter| {
            groups
                .iter()
                .filter(|(_, grps)| {
                    grps.iter().any(|g| g.takes.iter().any(|t| rec_filter.contains(&t.id)))
                })
                .map(|(&mid, _)| mid)
                .collect()
        });
        if let Some(qm) = &qualifying_monitors {
            order.retain(|&ci| chan_entries[ci].rows.iter().any(|&ri| qm.contains(&rows[ri].monitor.id)));
        }
        // Cluster by primary group, alphabetical by group name, ungrouped
        // channels first — a stable sort so each cluster's internal order
        // still respects the user's actual sort/filter. Skipped (flat list,
        // no header clustering) while a group filter is active OR the
        // toolbar's "Group" checkbox is off.
        let flatten = group_filter.is_some() || !group_visually;
        let group_name: HashMap<i64, &str> =
            channel_groups.iter().map(|g| (g.id, g.name.as_str())).collect();
        let mut grouped_order = order.clone();
        if !flatten {
            grouped_order.sort_by_key(|&ci| {
                chan_entries[ci]
                    .channel
                    .primary_group_id
                    .map(|gid| group_name.get(&gid).copied().unwrap_or(""))
            });
        }
        let mut vis: Vec<Vis> = Vec::new();
        for chunk in grouped_order.chunk_by(|&a, &b| {
            flatten
                || chan_entries[a].channel.primary_group_id == chan_entries[b].channel.primary_group_id
        }) {
            let gid = if flatten {
                None
            } else {
                chan_entries[chunk[0]].channel.primary_group_id
            };
            let collapsed = gid.is_some_and(|g| collapsed_channel_groups.contains(&g));
            if let Some(g) = gid {
                vis.push(Vis::ChannelGroup { group_id: g, count: chunk.len(), expanded: !collapsed });
                if collapsed {
                    continue;
                }
            }
        for &ci in chunk {
            let e = &chan_entries[ci];
            vis.push(Vis::Channel(ci));
            if qualifying_monitors.is_none() && !exp_channels.contains(&e.channel.id) {
                continue;
            }
            // Channel container -> its instances -> each instance's
            // stream history -> takes.
            for &ri in &e.rows {
                let mid = rows[ri].monitor.id;
                if let Some(qm) = &qualifying_monitors
                    && !qm.contains(&mid)
                {
                    // Not this filter's business — no matching stream here.
                    continue;
                }
                vis.push(Vis::Instance { row: ri, depth: 1 });
                if qualifying_monitors.is_none() && !exp_instances.contains(&mid) {
                    continue;
                }
                let Some(grps) = groups.get(&mid) else { continue };
                if let Some(rec_filter) = recording_group_filter {
                    // Flat, no Year/Month/Week headers: `Period`'s
                    // `gi_start..gi_end` assumes a CONTIGUOUS index range
                    // into `groups[&mid]`, which recording-group filtering
                    // (skipping individual, possibly non-adjacent streams)
                    // would break. `gi` stays a valid index into the
                    // UNFILTERED `grps` either way — only which entries get
                    // pushed to `vis` is filtered.
                    for (gi, g) in grps.iter().enumerate() {
                        if !g.takes.iter().any(|t| rec_filter.contains(&t.id)) {
                            continue;
                        }
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
                    continue;
                }
                let dates: Vec<chrono::NaiveDate> = grps.iter().map(period_anchor_date).collect();
                let (show_years, show_months, show_weeks) = period_levels_needed(&dates);
                // Fixed for this instance — doesn't depend on which bucket,
                // only on which levels are shown at all (verified to match
                // today's Stream=2/Take=3 depths when none are shown).
                let year_depth = 2;
                let month_depth = if show_years { 3 } else { 2 };
                let week_depth = month_depth + usize::from(show_months);
                let stream_depth = week_depth + usize::from(show_weeks);
                let take_depth = stream_depth + 1;

                let mut yi = 0usize;
                for (year_idx, year_chunk) in grps
                    .chunk_by(|a, b| period_anchor_date(a).year() == period_anchor_date(b).year())
                    .enumerate()
                {
                    let year_newest = year_idx == 0;
                    if show_years {
                        let key = period_key(mid, PeriodKind::Year, period_anchor_date(&year_chunk[0]));
                        let open = period_open(year_newest, period_toggles, &key);
                        vis.push(Vis::Period {
                            mid, kind: PeriodKind::Year, gi_start: yi, gi_end: yi + year_chunk.len(),
                            depth: year_depth, expanded: open,
                        });
                        if !open {
                            yi += year_chunk.len();
                            continue;
                        }
                    }

                    let mut mi = yi;
                    for (month_idx, month_chunk) in year_chunk
                        .chunk_by(|a, b| {
                            let (da, db) = (period_anchor_date(a), period_anchor_date(b));
                            (da.year(), da.month()) == (db.year(), db.month())
                        })
                        .enumerate()
                    {
                        let month_newest = year_newest && month_idx == 0;
                        if show_months {
                            let key = period_key(mid, PeriodKind::Month, period_anchor_date(&month_chunk[0]));
                            let open = period_open(month_newest, period_toggles, &key);
                            vis.push(Vis::Period {
                                mid, kind: PeriodKind::Month, gi_start: mi, gi_end: mi + month_chunk.len(),
                                depth: month_depth, expanded: open,
                            });
                            if !open {
                                mi += month_chunk.len();
                                continue;
                            }
                        }

                        let mut wi = mi;
                        for (week_idx, week_chunk) in month_chunk
                            .chunk_by(|a, b| {
                                week_start(period_anchor_date(a)) == week_start(period_anchor_date(b))
                            })
                            .enumerate()
                        {
                            let week_newest = month_newest && week_idx == 0;
                            if show_weeks {
                                let key = period_key(mid, PeriodKind::Week, period_anchor_date(&week_chunk[0]));
                                let open = period_open(week_newest, period_toggles, &key);
                                vis.push(Vis::Period {
                                    mid, kind: PeriodKind::Week, gi_start: wi, gi_end: wi + week_chunk.len(),
                                    depth: week_depth, expanded: open,
                                });
                                if !open {
                                    wi += week_chunk.len();
                                    continue;
                                }
                            }

                            for (local_i, g) in week_chunk.iter().enumerate() {
                                let gi = wi + local_i;
                                vis.push(Vis::Stream { mid, gi, depth: stream_depth });
                                if stream_has_children(g) && exp_streams.contains(&g.key) {
                                    for (ti, t) in g.takes.iter().enumerate() {
                                        if g.takes.len() > 1 {
                                            vis.push(Vis::Take { mid, gi, ti, depth: take_depth });
                                        }
                                        if t.recovery_state.is_some() {
                                            vis.push(Vis::VodJob {
                                                mid, gi, ti, kind: VodJobKind::Recovery, depth: take_depth,
                                            });
                                        }
                                        if t.vod_dl_state.is_some() {
                                            vis.push(Vis::VodJob {
                                                mid, gi, ti, kind: VodJobKind::Backfill, depth: take_depth,
                                            });
                                        }
                                    }
                                }
                            }
                            wi += week_chunk.len();
                        }
                        mi += month_chunk.len();
                    }
                    yi += year_chunk.len();
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
        login_to_mid: &HashMap<String, i64>,
        // Per-monitor rolling-recording counts (see `StreamsViewCache`) —
        // summed across this channel's instances for the 🕰 rollup badge.
        rolling_counts: &HashMap<i64, i64>,
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
        // Active header filters — the channel row itself is never hit-tinted
        // (surviving the filter IS its marker), but its game/title text still
        // gets the matched-substring highlight.
        fhits: Option<&FilterHits>,
        out: &mut StreamsOut,
        platform_pref: &crate::platform_pref::PlatformPrefCtx,
        // Whether title-`@mention` collab partners also get a Name-cell
        // suffix here, same as `instance_row`'s `collab_title_in_name` —
        // must match or a collapsed channel silently drops collab info its
        // own (expanded) instance row shows.
        collab_title_in_name: bool,
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
                        // "Stream Together" partners as a " × Partner" suffix
                        // while a shared-chat session is live; title
                        // `@mentions` join the same suffix when the setting
                        // is on, same as `instance_row`'s — otherwise a
                        // title-mention-only collab (no real shared-chat
                        // session) would show on the expanded instance row
                        // but silently vanish the moment its channel is
                        // collapsed. Each name coloured/linked when it
                        // resolves to a tracked channel (see
                        // `tracked_name_label`).
                        if let Some(c) = &cur_collab {
                            let shown: Vec<&crate::models::CollabPartner> = c
                                .partners
                                .iter()
                                .filter(|p| !p.from_title || collab_title_in_name)
                                .collect();
                            if !shown.is_empty() {
                                let resp = ui
                                    .horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 0.0;
                                        for p in &shown {
                                            ui.weak(" × ");
                                            let pcid = login_to_mid.get(&p.login).and_then(|&mid| {
                                                rows.iter()
                                                    .find(|r| r.monitor.id == mid)
                                                    .map(|r| r.channel.id)
                                            });
                                            let color = pcid.map(|pcid| {
                                                let (base, adjust) = channel_name_colors
                                                    .get(&pcid)
                                                    .copied()
                                                    .unwrap_or_else(|| (channel_event_color(pcid, ""), false));
                                                if adjust {
                                                    readable_color(base, tint.unwrap_or_else(|| ui.visuals().panel_fill))
                                                } else {
                                                    base
                                                }
                                            });
                                            let (cid, add) = collab_name_label(ui, p, pcid, color);
                                            if let Some(cid) = cid {
                                                out.acts.open_channel_props = Some(cid);
                                            }
                                            if add.is_some() {
                                                out.acts.add_collab_instance = add;
                                            }
                                        }
                                    })
                                    .response;
                                resp.on_hover_text(collab_hover(c));
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
                        } else if let Some(p) = primary
                            && p.last_recording_status.as_deref() == Some("failed")
                        {
                            let (icon, color) = state_icon_ack("failed", p.last_recording_err_ack);
                            let hover = if p.last_recording_err_ack {
                                format!("Acknowledged — {}", fail_hover(&p.last_recording_log))
                            } else {
                                fail_hover(&p.last_recording_log)
                            };
                            ui.colored_label(color, icon).on_hover_text(hover);
                        }
                        // Rolling recordings under this channel, summed across
                        // its instances — unlike the badges below this one is
                        // NOT present-state-only: a collapsed channel hiding
                        // that something under it is about to be auto-deleted
                        // is exactly the case this exists to prevent.
                        let rolling: i64 =
                            mons.iter().map(|m| rolling_counts.get(&m.monitor.id).copied().unwrap_or(0)).sum();
                        rolling_rollup_badge(ui, rolling, true);
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
                            .filter(|t| t.needs_remux())
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
                        meta_value_cell(ui, &cur_category, fhits.and_then(|f| f.needle("game")));
                    }
                    "title" => {
                        meta_value_cell(ui, &cur_title, fhits.and_then(|f| f.needle("title")));
                    }
                    "collab" => {
                        let (pcid, add) =
                            collab_cell(ui, cur_collab.as_ref(), rows, login_to_mid, channel_name_colors, tint);
                        if let Some(pcid) = pcid {
                            out.acts.open_channel_props = Some(pcid);
                        }
                        if add.is_some() {
                            out.acts.add_collab_instance = add;
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
                        let free: Vec<&&MonitorWithChannel> = mons
                            .iter()
                            .filter(|m| ad_free_status(m.monitor.ad_free, m.ad_free_sub).is_some())
                            .collect();
                        if !free.is_empty() {
                            let resp = ui
                                .horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 2.0;
                                    for _ in &free {
                                        ui.colored_label(SUCCESS_GREEN, "🛡");
                                    }
                                })
                                .response;
                            let lines: String = free
                                .iter()
                                .map(|m| instance_label(&m.monitor.url))
                                .collect::<Vec<_>>()
                                .join("\n");
                            resp.on_hover_text(format!(
                                "{}/{ninst} instance(s) marked or detected ad-free:\n{lines}",
                                free.len()
                            ));
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
                    .button("🤝  Collab history")
                    .on_hover_text(
                        "Every \"Stream Together\" session recorded for this channel: \
                         when, with whom, and who hosted (plus @mention-in-title \
                         collabs). Also reachable by clicking a tracked partner's name \
                         in the 🤝 Collab column.",
                    )
                    .clicked()
                {
                    out.open_collab_history = Some(cid);
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
                if ui
                    .button("⇋  Merge into another channel…")
                    .on_hover_text(
                        "Move ALL of this channel's instances into another channel \
                         (recordings, schedules, stats, posts, and about history move \
                         with them), then delete this emptied channel. The \
                         destination's own channel-level settings apply afterwards.",
                    )
                    .clicked()
                {
                    out.merge_channel = Some(cid);
                    ui.close();
                }
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

    /// Best target to open in the media player for a monitor: prefer an
    /// active take's live capture the configured player can actually play
    /// (a dual-capture monitor falls through to the DASH companion under
    /// non-mpv players), falling back to the most recent finished
    /// recording's output file. `groups` only has data for expanded
    /// monitors — `active_recordings` (the cheap global "currently
    /// recording" list) covers a collapsed row's live capture too. Shared
    /// between the row's own target and each resolved collab partner's.
    fn resolve_stream_target(
        mid: i64,
        groups: &HashMap<i64, Vec<StreamGroup>>,
        active_recordings: &HashMap<i64, Vec<crate::models::Recording>>,
        fs: &mut FsProbes,
        media_player: &str,
    ) -> Option<StreamTarget> {
        let group_takes = groups.get(&mid);
        let active: Vec<StreamTarget> = match group_takes {
            Some(gs) => gs
                .iter()
                .flat_map(|g| g.takes.iter())
                .filter(|t| t.is_active())
                .filter_map(|t| fs.target(&t.output_path))
                .collect(),
            None => active_recordings
                .get(&mid)
                .into_iter()
                .flatten()
                .filter_map(|t| fs.target(&t.output_path))
                .collect(),
        };
        if let Some(t) = active
            .iter()
            .find(|t| playable_with(t, media_player))
            .or_else(|| active.first())
        {
            return Some(t.clone());
        }
        // Most recent finished take (only known once the row's been
        // expanded at least once — `groups` isn't populated otherwise).
        if let Some(t) = group_takes
            .into_iter()
            .flatten()
            .flat_map(|g| g.takes.iter())
            .find(|t| !t.output_path.is_empty() && fs.is_file(std::path::Path::new(&t.output_path)))
        {
            return Some(StreamTarget::Finished(std::path::PathBuf::from(&t.output_path)));
        }
        // A subscriber-only broadcast: every take was refused, so none of them
        // has an output file — but the CDN session has been writing numbered
        // parts beside where that file would go, and those are playable now.
        // Checked last so a real capture always wins, and only for GATED takes
        // so this costs one directory scan on the rows that can benefit rather
        // than on every take in the archive.
        group_takes
            .into_iter()
            .flatten()
            .flat_map(|g| g.takes.iter())
            .filter(|t| t.gated && !t.output_path.is_empty())
            .find_map(|t| match fs.target(&t.output_path) {
                Some(t @ StreamTarget::Sequence(_)) => Some(t),
                _ => None,
            })
    }

    /// What "Play local recording" should open for ONE take.
    ///
    /// One function because there are two call sites per take — the row's ⏵
    /// button and its context-menu entry — which have to agree about what is
    /// playable, or a button is enabled and the menu entry beside it isn't.
    fn take_stream_target(
        t: &crate::models::Recording,
        file_ok: bool,
        fs: &mut FsProbes,
    ) -> Option<StreamTarget> {
        if t.is_active() {
            return fs.target(&t.output_path);
        }
        if file_ok {
            return Some(StreamTarget::Finished(std::path::PathBuf::from(&t.output_path)));
        }
        // Subscriber-only: Twitch refused the live edge, so this take has no
        // output file and never will — but the CDN session writes numbered
        // parts beside where that file would go, and they are complete and
        // playable now. Without this the one take that IS being archived is
        // also the one whose Play button is greyed out.
        if t.gated && !t.output_path.is_empty() {
            return match fs.target(&t.output_path) {
                Some(st @ StreamTarget::Sequence(_)) => Some(st),
                _ => None,
            };
        }
        None
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
        active_recordings: &HashMap<i64, Vec<crate::models::Recording>>,
        login_to_mid: &HashMap<String, i64>,
        rows: &[MonitorWithChannel],
        channel_name_colors: &HashMap<i64, (egui::Color32, bool)>,
        latest_raid_out: &HashMap<i64, crate::models::StreamEventRow>,
        fs_probes: &mut FsProbes,
        settings: &SettingsForm,
        scheduled_recordings: &[ScheduledRecordingWithNames],
        ptex: &PlatformTextures,
        now: i64,
        active_ids: &HashSet<i64>,
        finalizing_ids: &HashSet<i64>,
        // Monitors with a subscriber-only CDN capture session running.
        cdn_capture_ids: &HashSet<i64>,
        active_chat_ids: &HashSet<i64>,
        selected_monitor: Option<i64>,
        exp_instances: &HashSet<i64>,
        instance_avatars: &HashMap<i64, egui::TextureHandle>,
        // Takes of THIS instance still counting down towards rolling
        // auto-deletion — the 🕰 rollup badge (see `crate::rolling`).
        rolling_count: i64,
        // Set when this instance is live but standing by for a sibling that is
        // recording the broadcast on the named platform (see `crate::simulcast`).
        standby_for: Option<&str>,
        stop_holds_snapshot: &HashMap<i64, crate::downloader::StopHold>,
        ad_running: &impl Fn(i64) -> bool,
        sel_color: egui::Color32,
        status_bgcolor: bool,
        col_order: &[usize],
        // Recent viewer samples per monitor for the 👁 sparkline (last hour).
        spark: &HashMap<i64, Vec<(i64, i64)>>,
        // Whether THIS instance contains the active filters' matches
        // (precomputed `FilterHits::instance_hit` — needs the deep history
        // map, which lives on `self`), plus the filters themselves for the
        // text highlight inside cells.
        filter_hit: bool,
        fhits: Option<&FilterHits>,
        // Whether title-`@mention` collab partners also get a Name-cell suffix
        // (see `render_instance_row`'s doc comment) — `self.collab_title_in_name`.
        collab_title_in_name: bool,
        // User-saved tiling layouts, listed in the Layout submenus —
        // computed once per frame by the caller, not per row.
        saved_layouts: &[crate::layout::CustomLayout],
        out: &mut StreamsOut,
    ) {
        let mid = row.monitor.id;
        let finalizing = finalizing_ids.contains(&mid);
        // "Recording" = a live capture process; a finalize-pending take still
        // occupies `active` but its capture has ended.
        let recording = active_ids.contains(&mid) && !finalizing;
        let cdn_capture = cdn_capture_ids.contains(&mid);
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
        )
        // Lowest priority: mark the instance whose (possibly collapsed)
        // data contains the active filters' matches — the answer to "the
        // channel survived the filter, but WHERE is the hit?".
        .or_else(|| filter_hit.then_some(HL_FILTER_HIT));
        let inst_needs_remux = groups.get(&mid)
            .map(|gs| {
                gs.iter()
                    .flat_map(|g| g.takes.iter())
                    .filter(|t| {
                        t.needs_remux()
                    })
                    .count()
            })
            .unwrap_or(0);
        let media_player = settings.media_player_path.trim().to_string();
        // Best target to open in the media player for this monitor: prefer
        // an active take's live capture the configured player can actually
        // play (a dual-capture monitor falls through to the DASH companion
        // under non-mpv players); fall back to the most recent finished
        // recording's output file. Probes go through the TTL cache
        // (`fs_probes`) — this runs per row per frame.
        let inst_stream_target: Option<StreamTarget> =
            Self::resolve_stream_target(mid, groups, active_recordings, fs_probes, &media_player);
        // Collab partners resolved to a locally-tracked monitor (by Twitch
        // login), each with its own "current download" target — feeds the
        // "Play all collab instances" / "Play collab instance…" menu. Only
        // computed when this row actually has a live collab (rare), so the
        // per-partner resolve_stream_target calls don't run on every row.
        let collab_plays: Vec<(crate::models::CollabPartner, Option<StreamTarget>, Option<i64>)> =
            match row.live_collab.as_ref() {
                Some(c) => c
                    .partners
                    .iter()
                    .map(|p| {
                        let pmid =
                            login_to_mid.get(&p.login).copied().filter(|&pmid| pmid != mid);
                        let target = pmid.and_then(|pmid| {
                            Self::resolve_stream_target(
                                pmid,
                                groups,
                                active_recordings,
                                fs_probes,
                                &media_player,
                            )
                        });
                        (p.clone(), target, pmid)
                    })
                    .collect(),
                None => Vec::new(),
            };
        let output_dir_ok = fs_probes
            .is_dir(std::path::Path::new(&row.monitor.output_dir));
        // The most recently started take for this instance (any
        // stream) — the "Backfill head" manual action's target.
        // Same `groups`-empty-when-collapsed fallback as `inst_stream_target`.
        let inst_latest_rec_id = groups
            .get(&mid)
            .and_then(|gs| {
                gs.iter()
                    .flat_map(|g| g.takes.iter())
                    .max_by_key(|t| t.started_at)
                    .map(|t| t.id)
            })
            .or_else(|| {
                active_recordings
                    .get(&mid)
                    .and_then(|v| v.iter().max_by_key(|t| t.started_at).map(|t| t.id))
            });
        let stop_hold_desc = stop_holds_snapshot.get(&mid).map(|h| {
            let mut s = match h {
                crate::downloader::StopHold::Until { at, .. } => {
                    format!("until {}", fmt_datetime_short(*at))
                }
                crate::downloader::StopHold::FreshStream { .. } => {
                    "until this channel starts a new broadcast".to_string()
                }
            };
            if h.allow_triggers() {
                s.push_str(" (trigger words can still start a recording)");
            }
            s
        });
        if render_instance_row(
            tr, row, ptex, now, recording, finalizing, cdn_capture, chat_active,
            tint, output_dir_ok, depth, has_hist, expanded,
            inst_needs_remux,
            inst_stream_target.as_ref(), &media_player,
            instance_avatars.get(&mid),
            instance_avatars,
            rolling_count,
            standby_for,
            inst_latest_rec_id,
            scheduled_recordings,
            stop_hold_desc,
            spark.get(&mid),
            &collab_plays,
            rows,
            channel_name_colors,
            latest_raid_out.get(&mid),
            col_order, fhits,
            collab_title_in_name,
            saved_layouts,
            &mut out.acts,
        ) {
            out.toggle_instance = Some(mid);
        }
    }

    /// Render one primary-group header (see [`Vis::ChannelGroup`]) — pure
    /// navigation/summary + bulk actions, so only the "name" column is
    /// populated; every other `STREAM_COLUMNS` id falls through blank.
    fn group_row(
        tr: &mut egui_extras::TableRow<'_, '_>,
        group_id: i64,
        group_names: &HashMap<i64, String>,
        count: usize,
        expanded: bool,
        col_order: &[usize],
        out: &mut StreamsOut,
    ) {
        let label = group_names.get(&group_id).map(String::as_str).unwrap_or("(deleted group)");
        let noun = if count == 1 { "channel" } else { "channels" };
        let mut disc = false;
        for &ci in col_order {
            tr.col(|ui| {
                if STREAM_COLUMNS[ci].id == "name" {
                    disc = tree_name(ui, 0, true, expanded, None, egui::RichText::new(label).strong());
                    ui.weak(format!("· {count} {noun}"));
                }
            });
        }
        tr.response().context_menu(|ui| {
            if ui.button("🔴 Set Auto on for all in group").clicked() {
                out.bulk_set_group_enabled = Some((group_id, true));
                ui.close();
            }
            if ui.button("⏸ Set Auto off for all in group").clicked() {
                out.bulk_set_group_enabled = Some((group_id, false));
                ui.close();
            }
            ui.separator();
            if ui.button("▶ Enable all in group").clicked() {
                out.bulk_set_group_automation = Some((group_id, true));
                ui.close();
            }
            if ui.button("⏸ Disable (dormant) all in group").clicked() {
                out.bulk_set_group_automation = Some((group_id, false));
                ui.close();
            }
        });
        if disc {
            out.toggle_channel_group = Some(group_id);
        }
    }

    /// Render one Year/Month/Week grouping header (see [`Vis::Period`]) —
    /// pure navigation/summary, so only the "name" column is populated;
    /// every other `STREAM_COLUMNS` id falls through blank.
    #[allow(clippy::too_many_arguments)]
    fn period_row(
        tr: &mut egui_extras::TableRow<'_, '_>,
        mid: i64,
        kind: PeriodKind,
        streams: &[StreamGroup],
        depth: usize,
        expanded: bool,
        fs_probes: &mut FsProbes,
        now: i64,
        col_order: &[usize],
        out: &mut StreamsOut,
    ) {
        let anchor = period_anchor_date(&streams[0]);
        let label = match kind {
            PeriodKind::Year => {
                use chrono::Datelike;
                anchor.year().to_string()
            }
            PeriodKind::Month => {
                use chrono::Datelike;
                month_title(anchor.year(), anchor.month())
            }
            PeriodKind::Week => {
                let ws = week_start(anchor);
                let we = add_days(ws, 6);
                let pat = active_date_fmt().date_pattern();
                format!("{} – {}", ws.format(pat), we.format(pat))
            }
        };
        let n = streams.len();
        let total: u64 = streams
            .iter()
            .flat_map(|g| g.takes.iter())
            .map(|t| take_size_bytes(fs_probes, t))
            .sum();
        let captured_secs: i64 = streams.iter().map(|g| g.captured_secs(now)).sum();
        let key = period_key(mid, kind, anchor);
        let mut disc = false;
        for &ci in col_order {
            tr.col(|ui| {
                if STREAM_COLUMNS[ci].id == "name" {
                    disc = tree_name(ui, depth, true, expanded, None, egui::RichText::new(label.clone()));
                    let noun = if n == 1 { "stream" } else { "streams" };
                    ui.weak(format!("· {n} {noun}"));
                    if total > 0 {
                        ui.weak(format!("({})", fmt_bytes(total as i64)))
                            .on_hover_text(stream_size_hover(total, captured_secs));
                    }
                }
            });
        }
        if disc {
            out.toggle_period = Some(key);
        }
    }

    /// Renders "▷ Play stream (live edge)" — a normal enabled button when
    /// the owning monitor looks live, or a submenu offering an enabled "Try
    /// anyway" when it doesn't. Live-edge playback is meaningless once a
    /// broadcast is over (there's no live edge to tune into), but if live
    /// detection is stale or simply wrong, the user can still force it.
    /// Shared by the icon-button action bar and the context menu, on both
    /// the stream row and the take row.
    ///
    /// `small`: renders the compact icon-only variant used in the table
    /// row's action bar instead of the full labeled button/menu.
    /// `close_menu_on_click`: whether a direct click (the `is_live` branch)
    /// should also close an enclosing context menu — `true` for the
    /// context-menu call sites, `false` for the always-visible action bar
    /// (there's no enclosing menu to close there). "Try anyway" always
    /// closes its own submenu regardless, since that popup only exists here.
    fn play_live_edge_control(
        ui: &mut egui::Ui,
        mid: i64,
        is_live: bool,
        media_player_empty: bool,
        small: bool,
        close_menu_on_click: bool,
        target: &mut Option<i64>,
    ) {
        let full_label = "▷  Play stream (live edge)";
        if is_live {
            let button = if small {
                egui::Button::new("▷").small()
            } else {
                egui::Button::new(full_label)
            };
            if ui
                .add_enabled(!media_player_empty, button)
                .on_hover_text("Tune into the stream at the live edge in the media player (does not record)")
                .on_disabled_hover_text("Set a media player in Settings → Defaults first")
                .clicked()
            {
                *target = Some(mid);
                if close_menu_on_click {
                    ui.close();
                }
            }
            return;
        }
        let label = if small { "▷" } else { full_label };
        ui.menu_button(label, |ui| {
            if ui
                .add_enabled(!media_player_empty, egui::Button::new("Try anyway"))
                .on_disabled_hover_text("Set a media player in Settings → Defaults first")
                .clicked()
            {
                *target = Some(mid);
                ui.close();
            }
        })
        .response
        .on_hover_text(
            "This channel doesn't currently look live, so there's no live edge to \
             tune into. If live detection is stale or wrong, use \"Try anyway\" to \
             force it.",
        );
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
        // Anchor takes of running subscriber-only CDN sessions — see
        // `crate::downloader::CdnCaptures`.
        cdn_capture_recs: &HashSet<i64>,
        ad_breaks: &HashMap<i64, Vec<AdBreak>>,
        meta_logs: &HashMap<i64, Vec<StreamMetaChange>>,
        collab_by_stream: &HashMap<(i64, String), String>,
        exp_streams: &HashSet<String>,
        selected_streams: &HashMap<String, Vec<i64>>,
        sel_color: egui::Color32,
        // The recording group currently filtered to, if any — offers a
        // one-click "remove this stream from it" in the context menu
        // instead of needing a per-row live membership query.
        current_recording_group: Option<(i64, &str)>,
        now: i64,
        col_order: &[usize],
        rec_alerts: &HashMap<i64, crate::store::RecAlertBadge>,
        // Whether the owning monitor has a live capture process right now —
        // same source of truth as the instance row's own Stop button, so
        // the stream row's "Stop recording" entry agrees with it (rather
        // than a possibly-stale DB status).
        active_ids: &HashSet<i64>,
        finalizing_ids: &HashSet<i64>,
        // Active header filters, for marking rows/text that contain the
        // match (`None` = no filter set).
        fhits: Option<&FilterHits>,
        core: &AppCore,
        // Recording ids with a manual "Delete file from disk" in flight —
        // see `take_row`'s copy of this parameter.
        manual_delete_pending: &HashSet<i64>,
        out: &mut StreamsOut,
    ) {
        // The broadcast's stored collab names — shared by the collab cell
        // below and the filter-hit check here.
        let stream_collab = g
            .stream_id
            .as_ref()
            .and_then(|sid| collab_by_stream.get(&(mid, sid.clone())));
        // Multi-select tint (ctrl/shift-click) — see `selected_streams`'s doc
        // comment. Stream rows otherwise carry no tint, EXCEPT when a header
        // filter's match lives in this stream (deep filter) — then the row
        // gets the dim hit tint so the match is traceable after expanding.
        let tint = selected_streams
            .contains_key(&g.key)
            .then_some(sel_color)
            .or_else(|| {
                fhits
                    .is_some_and(|f| f.stream_hit(g, stream_collab.map(String::as_str)))
                    .then_some(HL_FILTER_HIT)
            });
        // Both must agree: the DB status ties the live process to THIS
        // broadcast specifically (the monitor could already be capturing a
        // newer one while an older StreamGroup's row is still on screen),
        // and active_ids/finalizing_ids rule out a stale "recording" status
        // left behind by a crash with no live process to actually stop.
        let recording = g.status() == "recording"
            && active_ids.contains(&mid)
            && !finalizing_ids.contains(&mid);
        // Owning monitor's live status — gates "Play stream (live edge)"
        // (pointless once the broadcast is over) and, further down,
        // "Backfill head" (needs the still-growing live CDN playlist).
        let owning_monitor = rows.iter().find(|r| r.monitor.id == mid).map(|r| &r.monitor);
        let is_live = owning_monitor
            .map(|m| matches!(m.last_state.as_str(), "live" | "recording"))
            .unwrap_or(false);
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
        let mut ctrl_or_shift = false;
        {
            let mut disc = false;
            for &ci2 in col_order {
                tr.col(|ui| { tint_cell(ui, tint); match STREAM_COLUMNS[ci2].id {
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
                                "Play local recording (start)"
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
                            out.mark_started_stream = Some((g.key.clone(), mid));
                        }
                        Self::play_live_edge_control(
                            ui, mid, is_live, media_player.is_empty(), true, false,
                            &mut out.play_new_instance_mid,
                        );
                    }
                    "name" => {
                        ctrl_or_shift = ui.input(|i| i.modifiers.ctrl || i.modifiers.shift);
                        disc = tree_name(
                            ui, depth, has_takes, expanded, None,
                            egui::RichText::new(label.clone()),
                        );
                        if has_takes {
                            ui.weak(format!("· {} takes", g.takes.len()));
                        }
                        // Per-take live probe: `t.bytes` alone reads 0 for an
                        // active take (that column is only written at
                        // finalize), which would make the group's running
                        // total vanish for the entire duration of a capture.
                        let total: u64 =
                            g.takes.iter().map(|t| take_size_bytes(fs_probes, t)).sum();
                        if total > 0 {
                            ui.weak(format!("({})", fmt_bytes(total as i64)))
                                .on_hover_text(stream_size_hover(total, g.captured_secs(now)));
                        }
                    }
                    "state" => {
                        let finalizing = g.status() == "recording"
                            && g.takes.iter().any(|t| finalizing_recs.contains(&t.id));
                        // A subscriber-only broadcast whose CDN session is
                        // still running. Its takes are all `failed` — Twitch
                        // refused every one — so the group's own status says
                        // "failed" while parts are actively landing on disk.
                        let cdn_running =
                            g.takes.iter().any(|t| cdn_capture_recs.contains(&t.id));
                        let last_err_ack = g.takes.last().is_some_and(|t| t.err_ack);
                        let shown = if finalizing {
                            "finalizing"
                        } else if cdn_running {
                            "recording"
                        } else {
                            g.status()
                        };
                        let (icon, color) = state_icon_ack(shown, last_err_ack);
                        let resp = ui.colored_label(color, icon);
                        if finalizing {
                            resp.on_hover_text(FINALIZING_HOVER);
                        } else if g.status() == "failed" {
                            let log = g
                                .takes
                                .last()
                                .map(|t| t.log_excerpt.as_str())
                                .unwrap_or("");
                            let msg = if last_err_ack {
                                format!("Acknowledged — {}", fail_hover(log))
                            } else {
                                fail_hover(log)
                            };
                            resp.on_hover_text(msg);
                        } else {
                            resp.on_hover_text(g.status());
                        }
                        let nr = g.takes.iter().filter(|t| t.needs_remux()).count();
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
                        let sabr_live_edge_fallback =
                            g.takes.iter().any(|t| t.sabr_live_edge_fallback);
                        let chapters_done = g.takes.iter().any(|t| t.chapters_state == "done");
                        // Alert rollup over this stream's takes (a dual
                        // capture's legs and retakes sum into one badge).
                        let alert_agg = {
                            let mut agg = crate::store::RecAlertBadge::default();
                            let mut any = false;
                            for t in &g.takes {
                                if let Some(a) = rec_alerts.get(&t.id) {
                                    any = true;
                                    agg.merge(a);
                                }
                            }
                            any.then_some(agg)
                        };
                        if take_status_badges(
                            ui,
                            trigger_info,
                            vod_not_published,
                            vod_muted_secs,
                            full_backfilled,
                            head_backfilled,
                            backfill_running,
                            backfill_queued,
                            sabr_live_edge_fallback,
                            if chapters_done { "done" } else { "" },
                            gap_running,
                            alert_agg.as_ref(),
                        ) {
                            out.open_warnings = true;
                        }
                    }
                    "game" => {
                        meta_value_cell(ui, g.category(), fhits.and_then(|f| f.needle("game")));
                    }
                    "title" => {
                        meta_value_cell(ui, g.title(), fhits.and_then(|f| f.needle("title")));
                    }
                    "collab" => {
                        // Stored collab of this past/current broadcast, from
                        // the preloaded (monitor, stream id) → names map.
                        if let Some(names) = stream_collab
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
                        // Same live per-take probe as the "name" cell (see
                        // its comment) so this hover doesn't go stale for the
                        // entire length of an in-progress recording.
                        let total: u64 =
                            g.takes.iter().map(|t| take_size_bytes(fs_probes, t)).sum();
                        let mut hover = format!(
                            "{} captured across {} take(s) · span {}",
                            fmt_bytes(total as i64),
                            g.takes.len(),
                            fmt_duration(span),
                        );
                        if g.takes.iter().any(|t| t.ended_at_predates_accuracy_fix()) {
                            hover.push_str(
                                "\n\n⚠ At least one take here finished before a fix (2026-07-26) \
                                 that stamps the end time from the capture's real exit — before \
                                 then, a slow remux queued at the disk gate could push it hours \
                                 later than the broadcast actually ended. Check the affected \
                                 take's row for which one.",
                            );
                        }
                        ui.label(fmt_duration(g.captured_secs(now))).on_hover_text(hover);
                    }
                    // "on"/"platform"/"tool"/"detection"/"polled"/
                    // "next_stream"/"ad_free"/"added" are n/a per stream.
                    _ => {}
                }});
            }
            let row_resp = tr.response();
            if row_resp.clicked() {
                let take_ids: Vec<i64> = g.takes.iter().map(|t| t.id).collect();
                if ctrl_or_shift {
                    out.toggle_select_stream = Some((g.key.clone(), take_ids));
                } else {
                    out.select_only_stream = Some((g.key.clone(), take_ids));
                }
            }
            row_resp.context_menu(|ui| {
                ui.set_min_width(180.0);
                if recording {
                    stop_recording_submenus(ui, mid, &mut out.acts);
                    ui.separator();
                }
                if let Some((gid, gname)) = current_recording_group
                    && ui
                        .button(format!("➖  Remove from \"{gname}\""))
                        .on_hover_text("Remove this stream's takes from the recording group currently filtered to.")
                        .clicked()
                {
                    out.remove_from_recording_group = Some((gid, g.takes.iter().map(|t| t.id).collect()));
                    ui.close();
                }
                if g.status() == "failed"
                    && let Some(last) = g.takes.last()
                {
                    if last.err_ack {
                        if ui
                            .button("↺  Un-acknowledge failure")
                            .on_hover_text(
                                "Restore this stream's ⚠ as a normal (red) unacknowledged \
                                 failure — it'll bubble back up to the instance/channel row.",
                            )
                            .clicked()
                        {
                            out.set_err_ack = Some((last.id, false));
                            ui.close();
                        }
                    } else if ui
                        .button("✓  Acknowledge failure")
                        .on_hover_text(
                            "Mark this failed stream as handled: its ⚠ stops bubbling up to \
                             the instance/channel row, but stays visible here (muted) so \
                             the failure history isn't hidden.",
                        )
                        .clicked()
                    {
                        out.set_err_ack = Some((last.id, true));
                        ui.close();
                    }
                    ui.separator();
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
                        out.mark_started_stream = Some((g.key.clone(), mid));
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
                        egui::Button::new("⏵  Play local recording (start)"),
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
                    out.mark_started_stream = Some((g.key.clone(), mid));
                    ui.close();
                }
                Self::play_live_edge_control(
                    ui, mid, is_live, media_player.is_empty(), false, true,
                    &mut out.play_new_instance_mid,
                );
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
                    // "Play VOD"/"Open VOD webpage" work regardless of
                    // whether this stream's latest take was ever captured —
                    // see the take row's copy of this comment.
                    if !recording {
                        if ui
                            .button("▷  Play VOD")
                            .on_hover_text(
                                "Play this stream's (latest take's) VOD in the media \
                                 player — the platform's published VOD if available, \
                                 else (Twitch) reconstructed from CDN segments. Works \
                                 regardless of whether it was ever recorded. No-ops \
                                 quietly if nothing resolves.",
                            )
                            .clicked()
                        {
                            out.play_vod_now = Some(t.id);
                            ui.close();
                        }
                        if ui
                            .button("🌐  Open VOD webpage")
                            .on_hover_text(
                                "Open this stream's (latest take's) VOD webpage in \
                                 your browser — resolved the same way as \"Play \
                                 VOD\", so it works even before any \
                                 download/recovery has run. No-ops quietly if \
                                 nothing resolves.",
                            )
                            .clicked()
                        {
                            out.open_vod_webpage = Some(t.id);
                            ui.close();
                        }
                    }
                    if ui
                        .button("🛟  Recover VOD…")
                        .on_hover_text("Reconstruct this stream's (latest take's) VOD from segments still on the Twitch CDN (deleted or DMCA-muted). Works on a \u{1F441} \"seen live, Auto was off\" row too.")
                        .clicked()
                    {
                        out.open_recover_take = Some(t.id);
                        ui.close();
                    }
                    if ui
                        .button("📥  Download post-stream VOD")
                        .on_hover_text("Download the platform's full published VOD for this stream's latest take now (also retries a failed archive). Also works on a \u{1F441} \"seen live, Auto was off\" row — same as \"\u{23EC} Backfill missed VOD\" without the CDN-recovery fallback. For the missed intro of a from-start capture, use \"Backfill head\" instead.")
                        .clicked()
                    {
                        out.archive_vod_now = Some(t.id);
                        ui.close();
                    }
                    if t.output_path.is_empty()
                        && ui
                            .button("⏬  Backfill missed VOD")
                            .on_hover_text(
                                "Retroactively fetch this stream's latest take now — the \
                                 platform's published VOD if still up, else (Twitch) \
                                 reconstructed from CDN segments if not. The one-click \
                                 version of \"Download post-stream VOD\"/\"Recover VOD…\" \
                                 for a \u{1F441} \"seen live, Auto was off\" row or a \
                                 discovery-found broadcast.",
                            )
                            .clicked()
                    {
                        out.backfill_missed_vod_now = Some(t.id);
                        ui.close();
                    }
                    let is_twitch = owning_monitor
                        .map(|m| m.platform() == Platform::Twitch)
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
                    if head_backfill_running(background_tasks, t.id)
                        && ui
                            .button("⛔  Abort backfill")
                            .on_hover_text(
                                "Stop this stream's latest take's in-progress head \
                                 backfill now. The head fetched so far is discarded — \
                                 the take keeps its normal capture untouched, just \
                                 without the missed intro.",
                            )
                            .clicked()
                    {
                        out.abort_backfill = Some(t.id);
                        ui.close();
                    }
                }
                if ui
                    .button("🔎  Scan for missed streams")
                    .on_hover_text(
                        "Check this channel/instance's platform now for broadcasts this \
                         app has no record of at all (it wasn't running/monitoring at \
                         the time) — the on-demand version of the \
                         \"Auto-backfill missed streams\" setting's periodic sweep. Any \
                         found show up as new \u{1F441} \"seen live\" rows and are \
                         immediately backfilled the same way \"\u{23EC} Backfill missed \
                         VOD\" works.",
                    )
                    .clicked()
                {
                    out.scan_for_missed_streams = Some(mid);
                    ui.close();
                }
                if let Some(t) = g.takes.iter().max_by_key(|t| t.started_at)
                    && t.status == "completed"
                    && !chapters_running(background_tasks, t.id)
                {
                    let label = if t.chapters_state == "done" { "🔁  Re-embed chapters" } else { "📑  Embed chapters" };
                    if ui
                        .button(label)
                        .on_hover_text(
                            "Embed/refresh chapter markers for this stream's latest take \
                             now, instead of waiting for the next app restart's automatic \
                             sweep — also works as a retry after a failed/skipped attempt, \
                             or to pick up a change to which chapter kinds are enabled. \
                             No-ops quietly if this take isn't actually eligible (still \
                             resolving a gap-splice, or a head backfill still in progress).",
                        )
                        .clicked()
                    {
                        out.retrigger_chapters = Some(t.id);
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
                    .button("📝  Title/category/tags history")
                    .on_hover_text(
                        "Every title/category/tags change ever seen for this instance — \
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
                // Bulk "delete the FILE, keep every row" for the whole
                // broadcast — the stream-level equivalent of the take row's
                // "🗑🔥 Delete file from disk…", for cleaning up an
                // error/retry storm that left a broadcast with a dozen
                // useless takes in one go. Same three `manual_delete` gates,
                // checked once for the whole instance (not per-take).
                let owning_channel_id = rows.iter().find(|r| r.monitor.id == mid).map(|r| r.channel.id);
                let allow = owning_channel_id
                    .map(|cid| crate::manual_delete::deletion_allowed(&core.store, cid, mid))
                    .unwrap_or(false);
                let eligible: Vec<i64> = g
                    .takes
                    .iter()
                    .filter(|t| {
                        !t.is_active()
                            && !t.output_path.is_empty()
                            && !manual_delete_pending.contains(&t.id)
                    })
                    .map(|t| t.id)
                    .collect();
                if ui
                    .add_enabled(
                        allow && !eligible.is_empty(),
                        egui::Button::new("🗑🔥  Delete all take files from disk…"),
                    )
                    .on_hover_text(format!(
                        "Move (or delete, per Settings → Automatic deletion) the captured \
                         file for every take of this broadcast ({} eligible). Every take's \
                         history row stays — only the media files go away. Asks to confirm \
                         first. Handy after an error/retry storm left a broadcast with a \
                         pile of useless takes.",
                        eligible.len()
                    ))
                    .on_disabled_hover_text(if !allow {
                        "Blocked: needs \"Allow deletion\" on in the Streams toolbar AND \
                         \"Allow delete\" enabled on both this channel and this instance \
                         (all off by default)."
                    } else {
                        "No eligible take files to delete (all still recording, already \
                         gone, or already being deleted)"
                    })
                    .clicked()
                {
                    out.delete_stream_files = Some(eligible);
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
        // Anchor takes of running subscriber-only CDN sessions — see
        // `crate::downloader::CdnCaptures`.
        cdn_capture_recs: &HashSet<i64>,
        ad_breaks: &HashMap<i64, Vec<AdBreak>>,
        meta_logs: &HashMap<i64, Vec<StreamMetaChange>>,
        collab_by_stream: &HashMap<(i64, String), String>,
        rename_rec_id: &mut Option<i64>,
        rename_draft: &mut String,
        rename_preview: &mut String,
        show_rename_dialog: &mut bool,
        now: i64,
        col_order: &[usize],
        rec_alerts: &HashMap<i64, crate::store::RecAlertBadge>,
        // Same source of truth as the instance row's Stop button — see the
        // comment on `stream_row`'s copy of these parameters.
        active_ids: &HashSet<i64>,
        finalizing_ids: &HashSet<i64>,
        // Active header filters, for marking rows/text that contain the
        // match (`None` = no filter set).
        fhits: Option<&FilterHits>,
        // Recording ids with a manual "Delete file from disk" in flight —
        // disables the action so it can't be double-fired before the async
        // disposal finishes (see `crate::manual_delete`).
        manual_delete_pending: &HashSet<i64>,
        // Per-monitor broadcast stats (peak/avg viewers, sub/bits/raid
        // totals) for the 👁 badge — see `Store::stream_stats_for_monitor`.
        take_stats: &HashMap<i64, Vec<crate::models::StreamStatRow>>,
        out: &mut StreamsOut,
    ) {
        let t = &g.takes[ti];
        // Take rows normally carry no tint at all; the deep-filter hit tint
        // is the one exception (same rationale as `stream_row`'s).
        let tint = fhits.is_some_and(|f| f.take_hit(t)).then_some(HL_FILTER_HIT);
        // This specific take must be the DB's current "recording" one (not
        // an older take of the same multi-take stream) AND the live
        // process/finalize state must agree — see `stream_row`'s comment.
        let recording = t.is_active()
            && active_ids.contains(&mid)
            && !finalizing_ids.contains(&mid);
        // Owning monitor's live status — see `stream_row`'s copy of this
        // comment.
        let owning_monitor = rows.iter().find(|r| r.monitor.id == mid).map(|r| &r.monitor);
        let is_live = owning_monitor
            .map(|m| matches!(m.last_state.as_str(), "live" | "recording"))
            .unwrap_or(false);
        // For the "🗑🔥 Delete file from disk" gate check (`manual_delete`
        // needs both the channel and the instance id).
        let owning_channel_id = rows.iter().find(|r| r.monitor.id == mid).map(|r| r.channel.id);
        let take_variant = dual_take_variant(g, t);
        let dir = std::path::Path::new(&t.output_path)
            .parent()
            .map(|p| p.to_path_buf());
        let file_ok = !t.output_path.is_empty()
            && fs_probes.is_file(std::path::Path::new(&t.output_path));
        let media_player = settings.media_player_path.trim().to_string();
        {
            for &ci2 in col_order {
                tr.col(|ui| { tint_cell(ui, tint); match STREAM_COLUMNS[ci2].id {
                    "actions" => {
                        ui.push_id(t.id, |ui| {
                            if ui
                                .add_enabled(file_ok, egui::Button::new("▶").small())
                                .on_hover_text("Open file")
                                .clicked()
                            {
                                out.open_path =
                                    Some(std::path::PathBuf::from(&t.output_path));
                                out.mark_started_stream = Some((g.key.clone(), mid));
                            }
                            let stream_target =
                                Self::take_stream_target(t, file_ok, fs_probes);
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
                                    "Play local recording (start) — opens the live capture"
                                } else if matches!(stream_target, Some(StreamTarget::Sequence(_))) {
                                    "Play the subscriber-only CDN parts captured so far, in order — this broadcast was refused at the live edge, so the numbered parts ARE the archive until they are joined when it ends."
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
                                out.mark_started_stream = Some((g.key.clone(), mid));
                            }
                            Self::play_live_edge_control(
                                ui, t.monitor_id, is_live, media_player.is_empty(), true, false,
                                &mut out.play_new_instance_mid,
                            );
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
                        let size = take_size_bytes(fs_probes, t);
                        if size > 0 {
                            let hover = if t.is_active() {
                                "Live size — the capture is still growing (probed \
                                 directly from the file handle; updates every \
                                 couple of seconds)."
                            } else {
                                "Final file size."
                            };
                            ui.weak(format!("({})", fmt_bytes(size as i64)))
                                .on_hover_text(hover);
                        }
                    }
                    "state" => {
                        let finalizing =
                            t.status == "recording" && finalizing_recs.contains(&t.id);
                        // This take is the anchor of a running CDN session:
                        // its own status is `failed` (Twitch refused the live
                        // edge) while parts are landing on disk right now.
                        let cdn_running = cdn_capture_recs.contains(&t.id);
                        let shown = if finalizing { "finalizing" } else { t.status.as_str() };
                        let (icon, color) = state_icon_ack(shown, t.err_ack);
                        let sub_only = crate::models::sub_only_rejected(&t.log_excerpt)
                            || crate::models::members_only_rejected(&t.log_excerpt);
                        // 🔒 replaces the state glyph outright for a take Twitch
                        // refused: "ended" reads as "nothing was there", which
                        // is exactly wrong — the broadcast happened, we just
                        // weren't entitled to it.
                        let resp = if sub_only {
                            ui.colored_label(grid::SUB_ONLY_COLOR, "🔒")
                        } else {
                            ui.colored_label(color, icon)
                        };
                        if cdn_running {
                            ui.colored_label(
                                egui::Color32::from_rgb(0x6e, 0xc0, 0x8a),
                                "⭳ CDN",
                            )
                            .on_hover_text(grid::CDN_CAPTURE_HOVER);
                        }
                        if finalizing {
                            resp.on_hover_text(FINALIZING_HOVER);
                        } else if t.status == "failed" {
                            let mut msg = fail_hover(&t.log_excerpt);
                            if let Some(code) = t.exit_code {
                                msg = format!("{msg}\n(exit code {code})");
                            }
                            if t.err_ack {
                                msg = format!("Acknowledged — {msg}");
                            }
                            resp.on_hover_text(msg);
                        } else if t.status == "ended"
                            && (crate::models::sub_only_rejected(&t.log_excerpt)
                                || crate::models::members_only_rejected(&t.log_excerpt))
                        {
                            // Not "nothing to capture" — Twitch refused us. The
                            // take is empty on purpose; its head backfill (if
                            // any) is where this broadcast actually lives.
                            let platform = rows
                                .iter()
                                .find(|r| r.monitor.id == mid)
                                .map(|r| r.monitor.platform())
                                .unwrap_or(Platform::Twitch);
                            resp.on_hover_text(grid::sub_only_hover(platform, Some(t.started_at), now));
                        } else if t.status == "ended" {
                            resp.on_hover_text(
                                "The stream had already ended or wasn't live when we \
                                 tried — nothing to capture (not a failure).",
                            );
                        } else if t.status == "not_recorded" {
                            // Why it wasn't captured. Empty is the historical
                            // (and still commonest) reason, Auto-record off;
                            // anything else names itself — see
                            // `Recording::not_recorded_reason`.
                            let why = if t.not_recorded_reason.is_empty() {
                                "Auto-record was off for this channel/instance".to_string()
                            } else {
                                t.not_recorded_reason.clone()
                            };
                            resp.on_hover_text(format!(
                                "Not recorded — {why} while this stream was live, so nothing was \
                                 captured here. Kept as a history entry \
                                 (title/category/duration) only."
                            ));
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
                        if take_status_badges(
                            ui,
                            &t.trigger_info,
                            t.vod_state.as_deref() == Some("not_published"),
                            vod_muted_secs,
                            t.full_path.is_some(),
                            t.backfill_path.is_some(),
                            head_backfill_running(background_tasks, t.id),
                            t.head_backfill_state == "queued",
                            t.sabr_live_edge_fallback,
                            &t.chapters_state,
                            gap_recover_running(background_tasks, t.id),
                            rec_alerts.get(&t.id),
                        ) {
                            out.open_warnings = true;
                        }
                        rolling_take_badge(ui, t, now);
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
                        let needs_remux = t.needs_remux();
                        let remuxing = background_tasks.iter().any(|bt| {
                            matches!(bt.kind, crate::events::BackgroundTaskKind::Remux(_))
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
                        meta_value_cell(ui, &t.category, fhits.and_then(|f| f.needle("game")));
                    }
                    "title" => {
                        meta_value_cell(ui, &t.title, fhits.and_then(|f| f.needle("title")));
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
                    "viewers" => {
                        // Only for settled takes — a still-live take's badge
                        // would otherwise freeze at whatever the cache last
                        // saw instead of tracking the live count already
                        // shown one row up (the monitor row's 👁 cell).
                        if t.ended_at.is_some()
                            && let Some(s) =
                                take_stats.get(&mid).and_then(|v| find_take_stats(v, t))
                        {
                            let mut hover = format!(
                                "Peak {} · avg {} viewers over this take\n\
                                 {} of viewer samples tracked",
                                fmt_viewers(s.peak_viewers),
                                fmt_viewers(s.avg_viewers.round() as i64),
                                fmt_duration(s.live_secs),
                            );
                            let totals = format_event_totals(s.totals);
                            if !totals.is_empty() {
                                hover.push_str("\n\n");
                                hover.push_str(&totals);
                            }
                            ui.weak(format!("👁 {}", fmt_viewers(s.peak_viewers)))
                                .on_hover_text(hover);
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
                        let stale_note = t.ended_at_predates_accuracy_fix().then_some(
                            "⚠ Finished before a fix (2026-07-26) that stamps the end time from \
                             the capture's real exit — before then, a slow remux queued at the \
                             disk gate could push this hours later than the broadcast actually \
                             ended. The capture isn't necessarily incomplete; check the Recording \
                             Properties dialog or probe the file directly to see by how much, if \
                             at all.",
                        );
                        let bytes_note = (t.bytes > 0).then(|| fmt_bytes(t.bytes));
                        let hover = match (stale_note, bytes_note) {
                            (Some(s), Some(b)) => Some(format!("{s}\n\n{b}")),
                            (Some(s), None) => Some(s.to_string()),
                            (None, Some(b)) => Some(b),
                            (None, None) => None,
                        };
                        if let Some(h) = hover {
                            d.on_hover_text(h);
                        }
                    }
                    // "on"/"platform"/"tool"/"detection"/"polled"/
                    // "next_stream"/"went_live"/"ad_free"/"added" are
                    // n/a per take.
                    _ => {}
                }});
            }
            tr.response().context_menu(|ui| {
                ui.set_min_width(180.0);
                if recording {
                    stop_recording_submenus(ui, mid, &mut out.acts);
                    ui.separator();
                }
                if t.status == "failed" {
                    if t.err_ack {
                        if ui
                            .button("↺  Un-acknowledge failure")
                            .on_hover_text(
                                "Restore this take's ⚠ as a normal (red) unacknowledged \
                                 failure — it'll bubble back up to the instance/channel row.",
                            )
                            .clicked()
                        {
                            out.set_err_ack = Some((t.id, false));
                            ui.close();
                        }
                    } else if ui
                        .button("✓  Acknowledge failure")
                        .on_hover_text(
                            "Mark this failed take as handled: its ⚠ stops bubbling up to \
                             the instance/channel row, but stays visible here (muted) so \
                             the failure history isn't hidden.",
                        )
                        .clicked()
                    {
                        out.set_err_ack = Some((t.id, true));
                        ui.close();
                    }
                    ui.separator();
                }
                // Offer re-remux when the finalized file is still a .ts
                // (the automatic remux failed at recording end).
                let needs_remux = t.needs_remux();
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
                    out.mark_started_stream = Some((g.key.clone(), mid));
                    ui.close();
                }
                {
                    let stream_target = Self::take_stream_target(t, file_ok, fs_probes);
                    let player_ok = !media_player.is_empty()
                        && stream_target
                            .as_ref()
                            .map(|st| playable_with(st, &media_player))
                            .unwrap_or(false);
                    if ui
                        .add_enabled(
                            player_ok,
                            egui::Button::new("⏵  Play local recording (start)"),
                        )
                        .on_hover_text(if t.is_active() {
                            "Open live capture in the configured media player"
                        } else if matches!(stream_target, Some(StreamTarget::Sequence(_))) {
                            "Play the subscriber-only CDN parts captured so far, in order — this broadcast was refused at the live edge, so the numbered parts ARE the archive until they are joined when it ends."
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
                        out.mark_started_stream = Some((g.key.clone(), mid));
                        ui.close();
                    }
                    Self::play_live_edge_control(
                        ui, t.monitor_id, is_live, media_player.is_empty(), false, true,
                        &mut out.play_new_instance_mid,
                    );
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
                // "Play VOD"/"Open VOD webpage" work on a past broadcast
                // regardless of whether it was ever captured (unlike the
                // buttons below, which need `output_path`/`vod_id` already
                // set) — both re-resolve the VOD URL live via
                // `vod::resolve_vod_url`, the same lookup
                // `attempt_missed_stream_backfill` uses.
                if t.stream_id.is_some() && !t.is_active() {
                    if ui
                        .button("▷  Play VOD")
                        .on_hover_text(
                            "Play this take's VOD in the media player — the \
                             platform's published VOD if available, else \
                             (Twitch) reconstructed from CDN segments. Works \
                             regardless of whether this take was ever \
                             recorded. No-ops quietly if nothing resolves.",
                        )
                        .clicked()
                    {
                        out.play_vod_now = Some(t.id);
                        ui.close();
                    }
                    if ui
                        .button("🌐  Open VOD webpage")
                        .on_hover_text(
                            "Open this take's VOD webpage in your browser — \
                             resolved the same way as \"Play VOD\", so it \
                             works even before any download/recovery has \
                             run. No-ops quietly if nothing resolves.",
                        )
                        .clicked()
                    {
                        out.open_vod_webpage = Some(t.id);
                        ui.close();
                    }
                }
                // Recover a deleted/muted VOD from the CDN (Twitch takes
                // that carry a broadcast/stream id).
                if t.stream_id.is_some()
                    && ui
                        .button("🛟  Recover VOD…")
                        .on_hover_text("Reconstruct this VOD from segments still on the Twitch CDN (deleted or DMCA-muted). Works on a \u{1F441} \"seen live, Auto was off\" row too.")
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
                        .on_hover_text("Download the platform's full published VOD for this recording now (also retries a failed archive). Also works on a \u{1F441} \"seen live, Auto was off\" row. For the missed intro of a from-start capture, use \"Backfill head\" instead.")
                        .clicked()
                {
                    out.archive_vod_now = Some(t.id);
                    ui.close();
                }
                // One-click "try the published VOD, else (Twitch)
                // reconstruct from the CDN" — the unified action for a
                // 👁 "seen live, Auto was off" (or discovery-found) take,
                // which has no `output_path` for the two actions above to
                // hang a filename off of before this existed.
                if t.output_path.is_empty()
                    && t.stream_id.is_some()
                    && ui
                        .button("⏬  Backfill missed VOD")
                        .on_hover_text(
                            "Retroactively fetch this take now — the platform's \
                             published VOD if still up, else (Twitch) reconstructed \
                             from CDN segments if not.",
                        )
                        .clicked()
                {
                    out.backfill_missed_vod_now = Some(t.id);
                    ui.close();
                }
                // Manually (re)trigger the CDN head-backfill for this
                // take — Twitch capture-from-start only, and only while
                // the channel is live (the growing CDN playlist this
                // depends on stops being pre-mute-safe once the stream
                // ends). Forced regardless of the "fetch new head
                // backfill on new take" setting (user-initiated).
                let is_twitch = owning_monitor
                    .map(|m| m.platform() == Platform::Twitch)
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
                if head_backfill_running(background_tasks, t.id)
                    && ui
                        .button("⛔  Abort backfill")
                        .on_hover_text(
                            "Stop this take's in-progress head backfill now. The head \
                             fetched so far is discarded — the take keeps its normal \
                             capture untouched, just without the missed intro.",
                        )
                        .clicked()
                {
                    out.abort_backfill = Some(t.id);
                    ui.close();
                }
                if t.status == "completed" && !chapters_running(background_tasks, t.id) {
                    let label = if t.chapters_state == "done" { "🔁  Re-embed chapters" } else { "📑  Embed chapters" };
                    if ui
                        .button(label)
                        .on_hover_text(
                            "Embed/refresh chapter markers for this take now, instead of \
                             waiting for the next app restart's automatic sweep — also \
                             works as a retry after a failed/skipped attempt, or to pick up \
                             a change to which chapter kinds are enabled. No-ops quietly if \
                             this take isn't actually eligible (still resolving a \
                             gap-splice, or a head backfill still in progress).",
                        )
                        .clicked()
                    {
                        out.retrigger_chapters = Some(t.id);
                        ui.close();
                    }
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
                // Manual "delete the FILE, keep the row" — the inverse of
                // "Delete from list" above. Gated on all three
                // `manual_delete` switches (Streams toolbar master + this
                // channel + this instance, all off by default) so it can't
                // fire from a stray click — see `crate::manual_delete`.
                let allow = owning_channel_id
                    .map(|cid| crate::manual_delete::deletion_allowed(&core.store, cid, mid))
                    .unwrap_or(false);
                let pending = manual_delete_pending.contains(&t.id);
                if ui
                    .add_enabled(
                        allow && !t.output_path.is_empty() && !t.is_active() && !pending,
                        egui::Button::new("🗑🔥  Delete file from disk…"),
                    )
                    .on_hover_text(
                        "Move (or delete, per Settings → Automatic deletion) this take's \
                         captured file from disk. The history row itself stays — title, \
                         stats, chat log, chapters, notes are all kept; only the media file \
                         goes away. Asks to confirm first.",
                    )
                    .on_disabled_hover_text(if pending {
                        "Deleting…"
                    } else if !allow {
                        "Blocked: needs \"Allow deletion\" on in the Streams toolbar AND \
                         \"Allow delete\" enabled on both this channel and this instance \
                         (all off by default)."
                    } else if t.output_path.is_empty() {
                        "No file to delete"
                    } else {
                        "Stop the recording before deleting its file"
                    })
                    .clicked()
                {
                    out.delete_recording_file = Some(t.id);
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
        self.import_dialog = Some(Arc::new(Mutex::new(ImportDialog {
            title,
            load,
            rows: Vec::new(),
            loaded: false,
            search: String::new(),
            hide_already: false,
            status: String::new(),
            quality_override: String::new(),
            out_dir_override: String::new(),
            existing_channels: Vec::new(),
            do_import: false,
            do_guess: false,
            closed: false,
        })));
    }

    #[allow(deprecated)]
    pub(super) fn import_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.import_dialog.clone() else {
            return;
        };
        // Promote a completed background fetch into editable rows once — this needs
        // `self.rows` to mark channels already added, so it happens here in the
        // wrapper (the deferred closure can't reach `self`).
        let promote = {
            let d = state.lock().unwrap();
            !d.loaded && matches!(&*d.load.lock().unwrap(), ImportLoadState::Loaded { .. })
        };
        if promote {
            // Take the guard once (a second .lock() on the same thread would deadlock).
            let (cands, resolved) = {
                let d = state.lock().unwrap();
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
            let mut d = state.lock().unwrap();
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
                        target_channel: None,
                        guess_pending: false,
                        guess_reason: "",
                    }
                })
                .collect();
            d.loaded = true;
        }

        // Existing channels this import can target instead of creating a new
        // one — refreshed every call (the deferred closure can't reach `self`
        // to read `self.rows` directly).
        let mut existing_channels: Vec<(i64, String)> = self
            .rows
            .iter()
            .map(|r| (r.channel.id, r.channel.name.clone()))
            .collect::<HashMap<_, _>>()
            .into_iter()
            .collect();
        existing_channels.sort_by(|a, b| a.1.cmp(&b.1));
        state.lock().unwrap().existing_channels = existing_channels.clone();

        let title = state.lock().unwrap().title.clone();
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("import_vp"),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([620.0, 560.0]),
            state.clone(),
            shared,
            |ctx, dialog, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    dialog.closed = true;
                }
                let existing_channels = dialog.existing_channels.clone();

                if !dialog.loaded {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        match &*dialog.load.lock().unwrap() {
                            ImportLoadState::Loading => {
                                ui.horizontal(|ui| {
                                    // The spinner's own throttled repaint is
                                    // what re-polls this state; a zero-delay
                                    // request here would free-run the viewport.
                                    throttled_spinner(ui);
                                    ui.label("Loading…");
                                });
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
                            dialog.closed = true;
                        }
                    });
                    return;
                }

                // Row-filter/selection stats, shared by the bottom bar (declared
                // FIRST so it reserves its own fixed-height strip) and the
                // CentralPanel below (gets whatever's left) — this split is what
                // lets the row list actually grow when the window is resized
                // taller, instead of the list staying pinned to a fixed height
                // and all the extra space landing below the buttons.
                let q = dialog.search.to_lowercase();
                let visible: Vec<usize> = (0..dialog.rows.len())
                    .filter(|&i| import_row_matches(&dialog.rows[i], &q))
                    .filter(|&i| !dialog.hide_already || !dialog.rows[i].already)
                    .collect();
                let selectable: Vec<usize> =
                    visible.iter().copied().filter(|&i| !dialog.rows[i].already).collect();
                let n = dialog
                    .rows
                    .iter()
                    .filter(|r| r.selected && !r.already && !r.guess_pending)
                    .count();
                let pending = dialog.rows.iter().filter(|r| r.guess_pending).count();

                egui::TopBottomPanel::bottom("import_bottom_bar").show(ctx, |ui| {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(n > 0, egui::Button::new(format!("Import {n} selected")))
                            .clicked()
                        {
                            dialog.do_import = true;
                        }
                        if ui.button("Cancel").clicked() {
                            dialog.closed = true;
                        }
                        if pending > 0 {
                            ui.weak(format!(
                                "{pending} unconfirmed guess(es) held back — see \"Import into\"."
                            ));
                        }
                        if !dialog.status.is_empty() {
                            ui.label(&dialog.status);
                        }
                    });
                    ui.add_space(6.0);
                });

                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        let already_count = dialog.rows.iter().filter(|r| r.already).count();
                        ui.label(format!("{} channels found.", dialog.rows.len()));
                        if already_count > 0 {
                            ui.checkbox(&mut dialog.hide_already, "Hide already added")
                                .on_hover_text(
                                    "Importing in batches (to avoid hammering the platform's \
                                     asset fetches with one huge import)? Hide the channels \
                                     you've already added on a previous pass.",
                                );
                        }
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
                             Already-added channels are greyed out. \"Import into\" adds a \
                             candidate as a new instance of an existing channel instead of \
                             creating a new one — a guessed match needs its checkbox ticked \
                             before it's used; until then that row is held back from import.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.separator();

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
                        ui.separator();
                        if ui
                            .small_button("🔗 Guess existing channels")
                            .on_hover_text(
                                "For every row without a target picked yet, look for an \
                                 existing channel that's probably the same person (a linked \
                                 About page, or a similar name) and fill in \"Import into\" — \
                                 marked \"auto-assumed\" and held back from import until you \
                                 tick its confirm box.",
                            )
                            .clicked()
                        {
                            dialog.do_guess = true;
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
                        .show(ui, |ui| {
                            egui::Grid::new("import_grid")
                                .num_columns(7)
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
                                    ui.strong("Import into").on_hover_text(
                                        "Add this candidate as a new instance of an existing \
                                         channel instead of creating a new one. \"(new \
                                         channel)\" is the default.",
                                    );
                                    ui.strong("ID");
                                    ui.strong("Info");
                                    ui.end_row();
                                    for &i in &visible {
                                        let row = &mut dialog.rows[i];
                                        let ready = row.selected && !row.already && !row.guess_pending;
                                        ui.add_enabled(
                                            !row.already,
                                            egui::Checkbox::new(&mut row.selected, ""),
                                        );
                                        ui.add_enabled(
                                            ready,
                                            egui::Checkbox::new(&mut row.auto, ""),
                                        );
                                        ui.add_enabled(
                                            ready,
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
                                        ui.add_enabled_ui(!row.already, |ui| {
                                            ui.horizontal(|ui| {
                                                egui::ComboBox::from_id_salt(("import_target", i))
                                                    .width(150.0)
                                                    .selected_text(
                                                        row.target_channel
                                                            .and_then(|id| {
                                                                existing_channels
                                                                    .iter()
                                                                    .find(|(cid, _)| *cid == id)
                                                                    .map(|(_, n)| n.clone())
                                                            })
                                                            .unwrap_or_else(|| "(new channel)".into()),
                                                    )
                                                    .show_ui(ui, |ui| {
                                                        if ui
                                                            .selectable_value(
                                                                &mut row.target_channel,
                                                                None,
                                                                "(new channel)",
                                                            )
                                                            .clicked()
                                                        {
                                                            row.guess_pending = false;
                                                        }
                                                        for (cid, name) in &existing_channels {
                                                            if ui
                                                                .selectable_value(
                                                                    &mut row.target_channel,
                                                                    Some(*cid),
                                                                    name,
                                                                )
                                                                .clicked()
                                                            {
                                                                row.guess_pending = false;
                                                            }
                                                        }
                                                    });
                                                if row.guess_pending {
                                                    // A one-shot "confirm" action, not a
                                                    // persisted toggle — the checkbox and its
                                                    // "auto-assumed" label both disappear once
                                                    // ticked (guess_pending flips to false).
                                                    let mut confirmed = false;
                                                    if ui
                                                        .checkbox(&mut confirmed, "")
                                                        .on_hover_text(
                                                            "Confirm this guessed match — until \
                                                             ticked, this row is held back from \
                                                             import.",
                                                        )
                                                        .changed()
                                                        && confirmed
                                                    {
                                                        row.guess_pending = false;
                                                    }
                                                    ui.weak("auto-assumed").on_hover_text(format!(
                                                        "Guessed: {}. Not used until confirmed.",
                                                        row.guess_reason
                                                    ));
                                                }
                                            });
                                        });
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
                });
            },
        );

        let (do_import, do_guess, closed) = {
            let mut d = state.lock().unwrap();
            let result = (d.do_import, d.do_guess, d.closed);
            d.do_import = false;
            d.do_guess = false;
            result
        };

        // Collect the chosen rows before the dialog borrow ends (the create + reload
        // below need `&mut self`).
        let to_create: Vec<(String, String, bool, bool, Option<i64>)> = if do_import {
            state
                .lock()
                .unwrap()
                .rows
                .iter()
                .filter(|r| r.selected && !r.already && !r.guess_pending)
                .map(|r| {
                    (r.cand.name.clone(), r.cand.url.clone(), r.auto, r.disabled, r.target_channel)
                })
                .collect()
        } else {
            Vec::new()
        };
        let (quality_override, out_dir_override) = {
            let d = state.lock().unwrap();
            (d.quality_override.clone(), d.out_dir_override.clone())
        };

        if do_guess {
            // Off the render path (button-triggered, not per-frame): one
            // `about_latest_per_account` call per distinct existing channel,
            // then a pure name/link guess per not-yet-targeted candidate row.
            let about_links: HashMap<i64, Vec<String>> = existing_channels
                .iter()
                .map(|(id, _)| {
                    let links = self
                        .core
                        .store
                        .about_latest_per_account(*id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(row, _versions)| row.links_json)
                        .collect();
                    (*id, links)
                })
                .collect();
            let mut d = state.lock().unwrap();
            for row in &mut d.rows {
                if row.already || row.target_channel.is_some() {
                    continue;
                }
                if let Some((id, reason)) =
                    guess_existing_channel(&row.cand, &existing_channels, &about_links)
                {
                    row.target_channel = Some(id);
                    row.guess_pending = true;
                    row.guess_reason = reason;
                    row.selected = true;
                }
            }
        }

        if do_import {
            let out = self.settings.default_output_dir.clone();
            let mut ok = 0usize;
            let mut failed = 0usize;
            let mut last_err: Option<String> = None;
            // Continue past a per-row failure so one bad channel can't drop the rest
            // of the batch.
            for (name, url, auto, disabled, target_channel) in &to_create {
                match imports::create_monitor(
                    &self.core.store,
                    &self.monitor_defaults,
                    &out,
                    *target_channel,
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
        } else if closed {
            self.import_dialog = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn period_levels_needed_single_date_shows_nothing() {
        assert_eq!(period_levels_needed(&[]), (false, false, false));
        assert_eq!(period_levels_needed(&[ymd(2026, 7, 20)]), (false, false, false));
    }

    #[test]
    fn period_levels_needed_within_one_week_shows_nothing() {
        // Mon..Sun of the same ISO week — a channel with only this week's
        // history should render exactly as it did before this feature.
        let dates = [ymd(2026, 7, 20), ymd(2026, 7, 21), ymd(2026, 7, 26)];
        assert_eq!(period_levels_needed(&dates), (false, false, false));
    }

    #[test]
    fn period_levels_needed_multi_week_same_month_shows_weeks_only() {
        let dates = [ymd(2026, 7, 6), ymd(2026, 7, 20)];
        assert_eq!(period_levels_needed(&dates), (false, false, true));
    }

    #[test]
    fn period_levels_needed_multi_month_same_year_shows_month_and_week() {
        let dates = [ymd(2026, 3, 1), ymd(2026, 7, 20)];
        assert_eq!(period_levels_needed(&dates), (false, true, true));
    }

    #[test]
    fn period_levels_needed_multi_year_shows_everything() {
        let dates = [ymd(2024, 12, 30), ymd(2026, 7, 20)];
        assert_eq!(period_levels_needed(&dates), (true, true, true));
    }

    #[test]
    fn period_levels_needed_ignores_input_order() {
        // build_vis_rows always calls this with a newest-first slice, but
        // the function's own contract shouldn't depend on that.
        let newest_first = [ymd(2026, 7, 20), ymd(2026, 3, 1)];
        let oldest_first = [ymd(2026, 3, 1), ymd(2026, 7, 20)];
        assert_eq!(
            period_levels_needed(&newest_first),
            period_levels_needed(&oldest_first)
        );
    }

    #[test]
    fn period_key_normalizes_to_bucket_start() {
        // Any date inside a bucket must key identically to the bucket's
        // own start — otherwise a StreamGroup could look up a Year/Month
        // key that its own header row never pushed.
        let mid = 7;
        assert_eq!(
            period_key(mid, PeriodKind::Year, ymd(2026, 1, 1)),
            period_key(mid, PeriodKind::Year, ymd(2026, 12, 31)),
        );
        assert_eq!(
            period_key(mid, PeriodKind::Month, ymd(2026, 7, 1)),
            period_key(mid, PeriodKind::Month, ymd(2026, 7, 31)),
        );
        // A week spanning a month boundary still keys identically for
        // every day in it (Mon 2026-06-29 .. Sun 2026-07-05).
        assert_eq!(
            period_key(mid, PeriodKind::Week, ymd(2026, 6, 29)),
            period_key(mid, PeriodKind::Week, ymd(2026, 7, 5)),
        );
        // Different monitors never collide even for the same date.
        assert_ne!(
            period_key(1, PeriodKind::Year, ymd(2026, 7, 20)),
            period_key(2, PeriodKind::Year, ymd(2026, 7, 20)),
        );
    }

    #[test]
    fn period_open_defaults_and_flips() {
        let mut toggles = HashSet::new();
        // Default-open bucket stays open until explicitly toggled.
        assert!(period_open(true, &toggles, "k"));
        toggles.insert("k".to_string());
        assert!(!period_open(true, &toggles, "k"));
        // Default-closed bucket stays closed until explicitly toggled.
        assert!(!period_open(false, &toggles, "other"));
        toggles.insert("other".to_string());
        assert!(period_open(false, &toggles, "other"));
    }
}
