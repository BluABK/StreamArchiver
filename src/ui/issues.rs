//! Issues and notifications windows, quota warnings.

use super::*;

/// Deferred-viewport state for `warnings_window`. `search`/`sev_filter`/
/// `hide_acked`/`bgcolor` mirror `self.warn_*` (seeded from them at open,
/// synced back after every call — same "remembered across opens" shape as
/// `hype_mark_mins_ago`/`hype_mark_dur`); `rows`/`ident` are refreshed by the
/// wrapper on the same throttle as the DB reload, not every frame (they're
/// cloned in, and `CaptureAlertRow` × up to 500 rows isn't free to copy at
/// 60fps for no reason). `act` is the one action the user picked this pass,
/// applied by the wrapper next call.
pub(super) struct WarningsPopupState {
    pub(super) search: String,
    pub(super) sev_filter: Option<bool>,
    pub(super) hide_acked: bool,
    pub(super) bgcolor: bool,
    pub(super) rows: Vec<crate::store::CaptureAlertRow>,
    pub(super) ident: HashMap<i64, (Option<egui::TextureHandle>, (egui::Color32, bool))>,
    pub(super) act: Option<WarningsAct>,
    pub(super) closed: bool,
}

pub(super) enum WarningsAct {
    Ack(i64),
    AckAll,
    /// Batch-ack every alert of one category ("Ack all disk full").
    AckGroup(Vec<i64>),
    OpenLog(String),
    /// Open the folder holding a recording's recovered patch files.
    OpenPatches(i64),
}

/// Deferred-viewport state for `notifications_window`. `search`/
/// `kind_filter`/`bgcolor` mirror `self.notif_*` (seeded from them at open,
/// synced back after every call, same shape as [`WarningsPopupState`]);
/// `rows`/`ident`/`live_mids`/`have_player` are refreshed by the wrapper on
/// the same throttle as the DB reload, not every frame. `act` is the one
/// action the user picked this pass, applied by the wrapper next call.
pub(super) struct NotificationsPopupState {
    pub(super) search: String,
    pub(super) kind_filter: Option<crate::models::NotificationKind>,
    pub(super) bgcolor: bool,
    pub(super) rows: Vec<crate::store::NotificationRow>,
    pub(super) ident: HashMap<i64, (Option<egui::TextureHandle>, (egui::Color32, bool))>,
    /// notification id → still-tracked monitor id, for rows whose
    /// "Watch in player" button should be offered.
    pub(super) live_mids: HashMap<i64, i64>,
    pub(super) have_player: bool,
    pub(super) act: Option<NotifAct>,
    pub(super) closed: bool,
}

pub(super) enum NotifAct {
    OpenUrl(String),
    MarkAllRead,
    /// Show one community post in the 📣 Posts window (`post_id`).
    ViewPost(String),
    /// Tune into a channel's live edge in the media player (`monitor_id`).
    WatchInPlayer(i64),
    /// Open the 🚨 Capture warnings window (a capture-alert row's
    /// "Details" — the feed no longer repeats the alert body).
    OpenWarnings,
}

/// Deferred-viewport state for `issues_window`. Field names deliberately
/// mirror `self.issues_*`/`self.yt_*`/`self.background_tasks`/etc. exactly —
/// the eight `issues_*_section`/`issues_*_rows`/`issues_toolbar`/
/// `issues_table` helper methods moved from `impl StreamArchiverApp` to
/// `impl IssuesPopupState` near-verbatim (same field names in, same bodies),
/// since they only ever *read* this data (the one exception, the header's
/// column-reorder click and the toolbar's Refresh button, route through
/// `reorder_columns`/`refresh` below instead of touching `self` directly).
/// Rebuilt from `self` every call — these lists are already re-read from
/// `self.issues_*` every frame in the pre-migration code (no separate
/// throttle to preserve, unlike `WarningsPopupState`'s 500-row DB reload).
pub(super) struct IssuesPopupState {
    pub(super) issues_recs: Vec<crate::models::Recording>,
    pub(super) issues_missing: Vec<crate::models::Recording>,
    pub(super) issues_errors: Vec<crate::models::Recording>,
    pub(super) issues_errors_no_file: Vec<crate::models::Recording>,
    pub(super) issues_stuck: Vec<crate::models::Recording>,
    pub(super) issues_unmerged: Vec<(crate::models::Recording, Vec<std::path::PathBuf>)>,
    pub(super) issues_head_mismatch: Vec<(crate::models::Recording, String, String)>,
    pub(super) issues_gap_splice: Vec<crate::models::Recording>,
    pub(super) issues_stale_recording: Vec<(crate::models::Recording, Option<i64>)>,
    pub(super) issues_muted_vod: Vec<crate::models::MutedVodIssue>,
    pub(super) yt_quota_today: i64,
    pub(super) yt_quota_cutoff: i64,
    pub(super) yt_search_today: i64,
    pub(super) yt_search_cutoff: i64,
    pub(super) background_tasks: Vec<crate::events::BackgroundTask>,
    pub(super) finished_tasks: Vec<(crate::events::BackgroundTask, crate::events::TaskOutcome, i64)>,
    pub(super) fs_probes: Arc<Mutex<FsProbes>>,
    pub(super) issues_confirm_clear: bool,
    /// Toolbar text filter for the main table (channel / filename). Owned by
    /// the popup and NOT re-seeded from `self` each call — the wrapper's
    /// per-call snapshot block below must never touch it, or every keystroke
    /// would be overwritten before the closure could read it back.
    pub(super) filter: String,
    /// Toolbar row-shape filter for the main table. Popup-owned, same as
    /// `filter`.
    pub(super) kind_filter: IssueKind,
    /// Column order/visibility draft — mirrors `self.issues_grid.entries`,
    /// synced back to it (and persisted) by the wrapper whenever the header's
    /// column-chooser context menu changes it.
    pub(super) issues_entries: Vec<grid_columns::ColumnEntry>,
    /// Set by `issues_table`'s column-header click, in place of
    /// `self.reorder_columns = Some(...)`; read (and cleared) by the
    /// wrapper next call, which writes it into the real `self.reorder_columns`.
    pub(super) reorder_columns: Option<Arc<Mutex<ReorderColumnsState>>>,
    /// The 🔍 "View error details" popup — an embedded `egui::Window`
    /// rendered inside this SAME deferred viewport (not its own native
    /// window), so it lives here instead of on `self.issues_error_view`.
    pub(super) issues_error_view: Option<(String, String)>,
    pub(super) act: Option<Act>,
    /// Set by the toolbar's ⟳ Refresh button in place of
    /// `self.issues_refreshed = None`.
    pub(super) refresh: bool,
    pub(super) closed: bool,
}

/// Human-readable byte size (B / KB / MB / GB).
/// Everything the background Issues scan computes off the UI thread — every
/// `path.exists()`/`read_dir`/ffprobe in here can block for seconds against
/// the recordings drive under load.
pub(super) struct IssuesScan {
    /// Output file genuinely gone from disk (and no recoverable parts).
    pub(super) missing: Vec<crate::models::Recording>,
    /// Failed/aborted recordings whose file still exists.
    pub(super) errors_with_file: Vec<crate::models::Recording>,
    /// Failed/aborted recordings whose file is gone.
    pub(super) errors_no_file: Vec<crate::models::Recording>,
    /// File-gone takes whose media survived as split per-format parts in
    /// `.cache\` — recoverable via merge.
    pub(super) unmerged: Vec<(crate::models::Recording, Vec<std::path::PathBuf>)>,
    /// Head/live join blocked by codec parameters: (rec, head, live) with
    /// human-readable stream params.
    pub(super) head_mismatch: Vec<(crate::models::Recording, String, String)>,
    /// Rows still marked `recording` whose files have gone quiet: the capture
    /// died unnoticed (power loss / sleep) or the finalize is still pending.
    /// Paired with the seconds since the last write (`None` = nothing on disk).
    pub(super) stale_recording: Vec<(crate::models::Recording, Option<i64>)>,
}

/// A `recording` row whose newest capture-file write is older than this is
/// listed in Issues as stale (a live capture writes continuously).
const STALE_RECORDING_SECS: i64 = 600;

/// Setting key for the 🚨 Warnings window's "Row colors" toggle (default on):
/// paint each row in its severity/state tint. Off = plain rows; the
/// accent-coloured icon/title still carry the state.
pub(super) const K_WARN_BGCOLOR: &str = "warnings_row_bgcolor";
/// Setting key for the 🔔 Notifications window's "Row colors" toggle
/// (default on) — same idea for the feed's per-kind tints.
pub(super) const K_NOTIF_BGCOLOR: &str = "notif_row_bgcolor";

/// (icon, human label) per capture-alert kind (the 🚨 Warnings window rows).
fn alert_kind_label(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "sequence_gap" => ("⛔", "Lost segments"),
        "fetch_failed" => ("⛔", "Failed segment fetches"),
        "tool_error" => ("❌", "Tool errors"),
        "capture_failed" => ("⛔", "Capture failed"),
        "po_token_rejected" => ("🎫", "PO token rejected"),
        "youtube_experiment" => ("🧪", "Platform experiment"),
        "cookies_invalid" => ("🍪", "Cookies expired"),
        // Good news filed as a warning: the capture escalated itself to the
        // broadcast's real quality, but the user should know the platform
        // quality-gates this channel.
        "quality_gated" => ("🎚", "Quality-gated channel — upgraded via CDN"),
        _ => ("⚠", "Tool warnings"),
    }
}

/// Damage/recovery summary line for a capture-alert row — lost time and
/// recovery progress, `None` when there's neither (the common PO-token /
/// experiment / plain-warning rows). The occurrence count used to lead this
/// line; it now sits inline in the title row as `×N`, so a damage-less alert
/// is a two-line row instead of three.
fn alert_damage_summary(r: &crate::store::CaptureAlertRow) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if r.lost_segments > 0 {
        // Twitch live segments are 2 s; for yt-dlp fragments this is still a
        // usable order-of-magnitude estimate.
        let secs = r.lost_segments * 2;
        parts.push(format!(
            "{} segments (~{}) of content lost",
            crate::models::group_thousands(r.lost_segments),
            fmt_duration(secs)
        ));
    }
    if r.ranges_total > 0 {
        let mark = if r.recovered == r.ranges_total { " ✔" } else { "" };
        parts.push(format!(
            "{}/{} lost ranges recovered from the VOD{mark}",
            r.recovered, r.ranges_total
        ));
        if r.recovered_muted > 0 {
            parts.push(format!(
                "✂ {} recovered segment(s) use DMCA-muted audio",
                crate::models::group_thousands(r.recovered_muted)
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A human explanation when a capture/resume died on a network/DNS failure —
/// matched against the tool's log tail.
pub(super) fn network_failure_hint(log: &str) -> Option<&'static str> {
    let broken = [
        "getaddrinfo failed",
        "Failed to resolve",
        "[Errno 11001]",
        "Temporary failure in name resolution",
        "Name or service not known",
    ]
    .iter()
    .any(|m| log.contains(m));
    broken.then_some(
        "Likely cause: the network/DNS was unavailable when the download tool \
         (re)started — e.g. the machine woke from sleep before the network came \
         back up, or the connection dropped. The stream itself was fine; only \
         this attempt could not reach the site.",
    )
}

/// Deferred Issues-panel actions: collected while rendering inside the
/// viewport closure, applied after it releases its borrows of `self`.
pub(super) enum Act {
    Remux(usize),
    RemuxError(usize),
    Delete(usize),
    ClearPath(usize),
    DeleteError(usize),
    ClearError(usize),
    ClearMissingError(usize),
    /// Acknowledge a failed/aborted/orphaned take (index into `issues_errors`)
    /// — non-destructive alternative to Clear: drops out of this list and
    /// stops bubbling its ⚠ up to the instance/channel row, but the DB row
    /// (and the take-row's own muted ⚠) survives. See `Recording::err_ack`.
    AckError(usize),
    /// Same, for the file-gone list (`issues_errors_no_file`).
    AckMissingError(usize),
    ClearEmpties,
    ClearAllMissing,
    ClearAllErrors,
    ClearFilelessErrors,
    RecoverStuck(usize),
    ConfirmClear,
    ClearAll,
    DismissWarning(String),
    OpenMutedLive(usize),
    OpenMutedRecovered(usize),
    RerunMuted(usize),
    DismissMuted(usize),
    MergeSplit(usize),
    /// Archive the published VOD for an unmerged-split take (covers the part
    /// of the stream the interrupted capture missed).
    DownloadVodUnmerged(usize),
    /// Settle a stale 'recording' row (Issues → "Finalize now").
    FinalizeStale(usize),
    RefetchHeadMatchLive(usize),
    FetchVodForMismatch(usize),
    DismissMismatch(usize),
    /// Acknowledge a blocked gap-splice — index into `issues_gap_splice`.
    DismissGapSplice(usize),
    /// Open the folder holding a blocked take's recovered patch file(s).
    OpenGapSplicePatchFolder(usize),
    /// Open the error-details window: (title, full text). Same text as the
    /// status-column hover — the 🔍 button makes it readable/copyable.
    ViewError(String, String),
}

/// Row tint + accent colour for one 🔔 notifications-feed row, as
/// `((r, g, b), accent)`. Severity wins — an error is red whichever kind
/// produced it — and otherwise every kind gets its own hue so the feed can be
/// skimmed by colour, the way the 🚨 Warnings window's rows are. Read rows use
/// the same hue at a much lower alpha (see the call site).
fn notif_colors(
    kind: Option<crate::models::NotificationKind>,
    severity: &str,
) -> ((u8, u8, u8), egui::Color32) {
    use crate::models::NotificationKind as K;
    const RED: ((u8, u8, u8), egui::Color32) =
        ((120, 25, 25), egui::Color32::from_rgb(230, 100, 100));
    const AMBER: ((u8, u8, u8), egui::Color32) =
        ((120, 95, 10), egui::Color32::from_rgb(220, 175, 60));
    match (severity, kind) {
        ("error", _) | (_, Some(K::Error | K::TaskFailed)) => RED,
        ("warn", _) | (_, Some(K::CaptureAlert)) => AMBER,
        // Live/positive events: went-live purple (the Twitch hue), a fired
        // trigger green (it did the thing), a vetoed one orange.
        (_, Some(K::WentLive)) => ((70, 40, 120), egui::Color32::from_rgb(185, 150, 255)),
        (_, Some(K::TriggerMatched)) => ((25, 95, 45), egui::Color32::from_rgb(110, 200, 130)),
        (_, Some(K::TriggerBlocked)) => ((120, 70, 15), egui::Color32::from_rgb(235, 160, 70)),
        (_, Some(K::RecordingFinished)) => ((25, 60, 110), egui::Color32::from_rgb(120, 175, 245)),
        (_, Some(K::QualityUpgrade)) => ((15, 90, 90), egui::Color32::from_rgb(100, 210, 210)),
        (_, Some(K::ScheduleAdded | K::ScheduleUpdated)) => {
            ((45, 55, 120), egui::Color32::from_rgb(135, 150, 245))
        }
        (_, Some(K::YoutubePost)) => ((105, 35, 95), egui::Color32::from_rgb(230, 130, 210)),
        (_, Some(K::VodMuted)) => ((95, 45, 95), egui::Color32::from_rgb(215, 130, 215)),
        _ => ((60, 60, 60), egui::Color32::from_rgb(170, 170, 170)),
    }
}

/// Display label for a feed row's link button. Rows written before the wording
/// changed still carry the old label in the DB, so remap here rather than
/// migrating — the URL behind the button never changed, only what it's called.
fn notif_action_label(stored: &str) -> &str {
    match stored {
        "" => "Open",
        "Watch stream" => "Watch on Web",
        "Open post" => "View on YouTube",
        s => s,
    }
}

/// Whether a feed row's action URL points at a channel that was LIVE at the
/// time (rather than a VOD, a post, or nothing) — those rows get the "Watch in
/// player" companion button next to the web link.
fn notif_is_live_stream(kind: Option<crate::models::NotificationKind>) -> bool {
    use crate::models::NotificationKind as K;
    matches!(
        kind,
        Some(K::WentLive | K::TriggerMatched | K::TriggerBlocked | K::QualityUpgrade)
    )
}

/// The community-post id a `youtube_post` feed row refers to. Prefers the
/// dedup key it was inserted with (`post:{monitor}:{post_id}`) and falls back
/// to the trailing segment of the `youtube.com/post/…` action URL.
fn notif_post_id(r: &crate::store::NotificationRow) -> Option<String> {
    if let Some(id) = r.ref_key.strip_prefix("post:").and_then(|s| s.split_once(':')).map(|(_, id)| id)
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    let id = r.action_url.rsplit('/').next().unwrap_or_default();
    (!id.is_empty() && r.action_url.contains("/post/")).then(|| id.to_string())
}

/// Render a feed row's title with the channel's name inside it drawn in that
/// channel's own colour (the same one the Streams grid uses). Titles are built
/// as `"{channel} is live"`, `"⚡ {channel} — trigger matched"`, … so the name
/// is a plain substring; rows without one (generic errors, schedule changes)
/// render the title whole.
/// `base` colours the non-channel parts (the 🚨 Warnings window uses its
/// severity accent there); `None` = the default strong text colour.
fn notif_title(
    ui: &mut egui::Ui,
    title: &str,
    channel: &str,
    name_color: Option<egui::Color32>,
    base: Option<egui::Color32>,
) {
    let styled = |text: &str| {
        let rich = egui::RichText::new(text).strong();
        match base {
            Some(c) => rich.color(c),
            None => rich,
        }
    };
    let hit = name_color
        .filter(|_| !channel.is_empty())
        .zip(title.find(channel))
        .map(|(c, at)| (at, c));
    let Some((at, color)) = hit else {
        ui.label(styled(title));
        return;
    };
    let (head, rest) = title.split_at(at);
    let (name, tail) = rest.split_at(channel.len());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        if !head.is_empty() {
            ui.label(styled(head));
        }
        ui.label(egui::RichText::new(name).strong().color(color));
        if !tail.is_empty() {
            ui.label(styled(tail));
        }
    });
}

/// Status-column explainer for stuck-in-cache rows (hover AND 🔍 window).
const STUCK_IN_CACHE_DETAILS: &str =
    "The recording finished successfully, but moving it out of the hidden \
     working folder failed — most commonly because the filename was too long \
     for the filesystem. The file is safe; it just isn't where it should be \
     yet.";

/// Width cap for a top-section row's name cell. Capture filenames run to
/// 150+ characters (channel + timestamp + full stream title + tags), and an
/// `egui::Grid` sizes a column to its widest cell — uncapped, one long title
/// pushed every action button in that section off the right edge of the
/// window. The name truncates here and is available in full on hover.
const SECTION_NAME_W: f32 = 380.0;

/// A top section starts collapsed once it holds more rows than this. A long
/// backlog (a hundred unmerged split captures) would otherwise bury every
/// other section — and the toolbar and main table with them.
const SECTION_AUTO_COLLAPSE: usize = 8;

/// A top-section row's name cell: truncated to [`SECTION_NAME_W`], full text
/// on hover. Always paired with a separate short detail cell, because the
/// interesting part of these rows (part count, size, age, mismatch) would be
/// exactly what a truncation at the end of one combined string threw away.
fn issue_name_cell(ui: &mut egui::Ui, name: &str) {
    ui.scope(|ui| {
        ui.set_max_width(SECTION_NAME_W);
        ui.add(egui::Label::new(name).truncate()).on_hover_text(name);
    });
}

/// Wrap one top section in a collapsible header carrying its own row count,
/// with the explanatory blurb inside the body (so a section you've folded
/// away costs one line, not a paragraph plus its rows).
fn issue_section<R>(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    count: usize,
    blurb: &str,
    add: impl FnOnce(&mut egui::Ui) -> R,
) {
    egui::CollapsingHeader::new(
        egui::RichText::new(format!("{title} — {count}")).strong(),
    )
    .id_salt(id)
    .default_open(count <= SECTION_AUTO_COLLAPSE)
    .show(ui, |ui| {
        ui.weak(blurb);
        add(ui);
    });
    ui.separator();
}

/// Which of the main table's five row shapes to show. The table concatenates
/// all five, so with a few hundred takes needing attention the only way to
/// work through one category is to hide the others.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IssueKind {
    All,
    NeedsRemux,
    StuckInCache,
    FileMissing,
    FailedNoFile,
    Failed,
}

impl IssueKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            IssueKind::All => "All types",
            IssueKind::NeedsRemux => "Needs remux",
            IssueKind::StuckInCache => "Stuck in cache",
            IssueKind::FileMissing => "File missing",
            IssueKind::FailedNoFile => "Failed (no file)",
            IssueKind::Failed => "Failed",
        }
    }

    fn hover(self) -> &'static str {
        match self {
            IssueKind::All => "Show every kind of row.",
            IssueKind::NeedsRemux => {
                "Captures still sitting in the working folder as .ts — re-remuxable to MKV."
            }
            IssueKind::StuckInCache => {
                "The capture succeeded but the move out of the working folder never \
                 completed (usually a too-long filename)."
            }
            IssueKind::FileMissing => {
                "The database still points at an output file that is gone from disk."
            }
            IssueKind::FailedNoFile => {
                "Failed/aborted takes whose output file no longer exists — nothing to recover."
            }
            IssueKind::Failed => "Failed/aborted takes whose file is still on disk.",
        }
    }

    const ALL: [IssueKind; 6] = [
        IssueKind::All,
        IssueKind::NeedsRemux,
        IssueKind::StuckInCache,
        IssueKind::FileMissing,
        IssueKind::FailedNoFile,
        IssueKind::Failed,
    ];
}

/// Does a table row survive the toolbar's text filter? Matched against the
/// two columns that identify a row — the channel and the output path (which
/// carries the filename shown in the File column). `filter` must already be
/// lowercased; it's compared once per row, so folding it per row would mean
/// a few hundred throwaway allocations every repaint.
fn issue_filter_hit(filter: &str, ch_name: &str, path: &str) -> bool {
    filter.is_empty()
        || ch_name.to_lowercase().contains(filter)
        || path.to_lowercase().contains(filter)
}

impl IssuesPopupState {
    /// ── Quota warnings ── one row per active warning + dismiss button.
    fn issues_quota_section(
        &self,
        ui: &mut egui::Ui,
        quota_warnings: &[String],
        act: &mut Option<Act>,
    ) {
        for key in quota_warnings {
            let (msg, color) = match key.as_str() {
                "youtube_units_exceeded" => (
                    format!("YouTube Data API daily unit quota reached ({} / {} units). API calls are paused until tomorrow.", self.yt_quota_today, self.yt_quota_cutoff),
                    egui::Color32::from_rgb(200, 80, 80),
                ),
                "youtube_units_near_cutoff" => (
                    format!("YouTube Data API units near cutoff ({} / {} units today).", self.yt_quota_today, self.yt_quota_cutoff),
                    egui::Color32::from_rgb(200, 150, 60),
                ),
                "youtube_search_exceeded" => (
                    format!("YouTube search.list daily limit reached ({} / {} queries). Search-based detection paused until tomorrow.", self.yt_search_today, self.yt_search_cutoff),
                    egui::Color32::from_rgb(200, 80, 80),
                ),
                "youtube_search_near_cutoff" => (
                    format!("YouTube search.list queries near limit ({} / {} today).", self.yt_search_today, self.yt_search_cutoff),
                    egui::Color32::from_rgb(200, 150, 60),
                ),
                _ => continue,
            };
            ui.horizontal(|ui| {
                ui.colored_label(color, &msg);
                if ui.small_button("✕ Dismiss").clicked() {
                    *act = Some(Act::DismissWarning(key.clone()));
                }
            });
        }
        if !quota_warnings.is_empty() {
            ui.separator();
        }
    }

    /// ── DMCA-muted published VODs (live recording kept) ──
    fn issues_muted_vod_section(&self, ui: &mut egui::Ui, act: &mut Option<Act>) {
        if self.issues_muted_vod.is_empty() {
            return;
        }
        let fs = &self.fs_probes;
        let muted = &self.issues_muted_vod;
        issue_section(
            ui,
            "issues_muted_vod",
            "✂ DMCA-muted VODs (live recording kept)",
            muted.len(),
            "The published VOD is DMCA-silenced, so it is never downloaded as-is and \
             never replaces the live recording — which has the full audio. CDN \
             recovery has already run to un-mute what it could; these rows are here \
             to be checked and dismissed.",
            |ui| {
                egui::Grid::new("issues_muted_vod_grid")
                    .num_columns(6)
                    .spacing([10.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (i, m) in muted.iter().enumerate() {
                            let mins = (m.muted_secs / 60).max(1);
                            issue_name_cell(ui, &m.channel);
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 120, 30),
                                format!("~{mins} min muted"),
                            );
                            let live_ok = !m.output_path.is_empty()
                                && fs.lock().unwrap().is_file(std::path::Path::new(&m.output_path));
                            if ui
                                .add_enabled(live_ok, egui::Button::new("▶ Live"))
                                .on_hover_text("Open the live recording — it has the full audio.")
                                .clicked()
                            {
                                *act = Some(Act::OpenMutedLive(i));
                            }
                            let rec = m.recovered_path.as_deref().unwrap_or("");
                            let rec_ok =
                                !rec.is_empty() && fs.lock().unwrap().is_file(std::path::Path::new(rec));
                            if ui
                                .add_enabled(rec_ok, egui::Button::new("📼 VOD"))
                                .on_hover_text("Open the recovered VOD file.")
                                .clicked()
                            {
                                *act = Some(Act::OpenMutedRecovered(i));
                            }
                            if ui
                                .button("♻ Re-run")
                                .on_hover_text("Run VOD recovery for this take again.")
                                .clicked()
                            {
                                *act = Some(Act::RerunMuted(i));
                            }
                            if ui
                                .button("✓ Dismiss")
                                .on_hover_text("Acknowledge — the live recording has the full audio.")
                                .clicked()
                            {
                                *act = Some(Act::DismissMuted(i));
                            }
                            ui.weak(
                                m.recovery_state
                                    .as_deref()
                                    .map(|s| format!("recovery: {s}"))
                                    .unwrap_or_default(),
                            );
                            ui.end_row();
                        }
                    });
            },
        );
    }

    /// ── Rows stuck in 'recording' with no live capture ──
    fn issues_stale_recording_section(&self, ui: &mut egui::Ui, act: &mut Option<Act>) {
        if self.issues_stale_recording.is_empty() {
            return;
        }
        let now = crate::models::now_unix();
        let stale = &self.issues_stale_recording;
        issue_section(
            ui,
            "issues_stale_recording",
            "⏸ Marked 'recording' but not being written",
            stale.len(),
            "These takes claim to be recording, but their files have not been \
             written for a while. Either the capture process died without the \
             app noticing (power loss, sleep, forced kill), or the post-capture \
             finalize is still waiting for its turn at the disk gate (then it \
             shows a remux job here and under Background jobs). Finalize now \
             promotes whatever was captured and settles the row.",
            |ui| {
                egui::Grid::new("issues_stale_recording_grid")
                    .num_columns(4)
                    .spacing([10.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (i, (rec, age)) in stale.iter().enumerate() {
                            let name = std::path::Path::new(&rec.output_path)
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| format!("recording {}", rec.id));
                            let age_s = match age {
                                Some(a) => format!("last write {} ago", fmt_duration(*a)),
                                None => "no capture file found on disk".to_string(),
                            };
                            issue_name_cell(ui, &name);
                            ui.colored_label(egui::Color32::from_rgb(220, 160, 30), &age_s);
                            // An in-flight finalize/remux for this take (startup re-drive
                            // or a manual action) is a Remux background task keyed by the
                            // recording id.
                            let task = self.background_tasks.iter().find(|bt| {
                                matches!(bt.kind, crate::events::BackgroundTaskKind::Remux(_))
                                    && bt.id == rec.id as u64
                            });
                            ui.horizontal(|ui| {
                                if let Some(bt) = task {
                                    let elapsed = (now - bt.started_at).max(0);
                                    if let Some(p) = bt.progress {
                                        ui.add(
                                            egui::ProgressBar::new(p)
                                                .show_percentage()
                                                .desired_width(110.0),
                                        );
                                    }
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 160, 220),
                                        format!("⏳ finalizing… {}", fmt_duration(elapsed)),
                                    )
                                    .on_hover_text(
                                        "The finalize is queued/running — remuxes take turns on \
                                         the recordings drive, so a backlog can hold this for a \
                                         while. Progress shows once ffmpeg starts.",
                                    );
                                } else if ui
                                    .button("🛠 Finalize now")
                                    .on_hover_text(
                                        "Promote whatever was captured (remux/move it out of \
                                         the working folder) and settle this row.",
                                    )
                                    .clicked()
                                {
                                    *act = Some(Act::FinalizeStale(i));
                                }
                            });
                            if ui
                                .button("🔍")
                                .on_hover_text("View details in a window.")
                                .clicked()
                            {
                                let mut text = format!(
                                    "Status: recording (stale)\n{age_s}\nStarted: {}\nPath: {}",
                                    fmt_datetime_short(rec.started_at),
                                    rec.output_path
                                );
                                if let Some(hint) = network_failure_hint(&rec.log_excerpt) {
                                    text.push_str("\n\n");
                                    text.push_str(hint);
                                }
                                if !rec.log_excerpt.is_empty() {
                                    text.push_str("\n\n");
                                    text.push_str(rec.log_excerpt.trim());
                                }
                                *act = Some(Act::ViewError(name.clone(), text));
                            }
                            ui.end_row();
                        }
                    });
            },
        );
    }

    /// ── Unmerged split captures (recoverable, NOT lost) ──
    fn issues_unmerged_section(
        &self,
        ui: &mut egui::Ui,
        has_active_remux: bool,
        act: &mut Option<Act>,
    ) {
        if self.issues_unmerged.is_empty() {
            return;
        }
        let now = crate::models::now_unix();
        let fs = &self.fs_probes;
        let unmerged = &self.issues_unmerged;
        issue_section(
            ui,
            "issues_unmerged",
            "🧩 Unmerged split captures (recoverable)",
            unmerged.len(),
            "The download tool died before merging its per-format files — the \
             final file was never written (the take reads as 0 bytes / gone), \
             but the video and audio survived as parts in `.cache\\`. Rows \
             marked (interrupted) recovered from unfinished working files: the \
             merged video is intact up to where the capture stopped, but its \
             very tail may be cut, and the stream continued past that point — \
             Download VOD gets the whole broadcast if it's still published. \
             Merge is lossless and runs throttled like any finalize pass.",
            |ui| {
                egui::Grid::new("issues_unmerged_grid")
                    .num_columns(5)
                    .spacing([10.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (i, (rec, parts)) in unmerged.iter().enumerate() {
                            let name = std::path::Path::new(&rec.output_path)
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| rec.output_path.clone());
                            let total: u64 =
                                parts.iter().map(|p| fs.lock().unwrap().len(p)).sum();
                            let partial =
                                parts.iter().any(|p| p.to_string_lossy().ends_with(".part"));
                            issue_name_cell(ui, &name);
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 160, 30),
                                format!(
                                    "{} part(s), {}{}",
                                    parts.len(),
                                    fmt_bytes(total as i64),
                                    if partial { " (interrupted)" } else { "" },
                                ),
                            );
                            // This take's own merge (running or queued for the disk
                            // gate) — keyed by the recording id. Show its live state
                            // instead of the button.
                            let merge_task = self.background_tasks.iter().find(|bt| {
                                matches!(bt.kind, crate::events::BackgroundTaskKind::Remux(_))
                                    && bt.id == rec.id as u64
                            });
                            ui.horizontal(|ui| {
                                if let Some(bt) = merge_task {
                                    let elapsed = (now - bt.started_at).max(0);
                                    if let Some(p) = bt.progress {
                                        ui.add(
                                            egui::ProgressBar::new(p)
                                                .show_percentage()
                                                .desired_width(110.0),
                                        );
                                    }
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 160, 220),
                                        bt.progress_info
                                            .clone()
                                            .unwrap_or_else(|| "⏳ merging…".into()),
                                    )
                                    .on_hover_text(format!(
                                        "Elapsed: {} — a queued merge shows what currently \
                                         holds the disk gate; speed/position appear once \
                                         its own ffmpeg starts.",
                                        fmt_duration(elapsed)
                                    ));
                                } else if ui
                                    .add_enabled(!has_active_remux, egui::Button::new("🧩 Merge"))
                                    .on_hover_text(
                                        "Losslessly mux the parts into the final MKV, promote it, \
                                         and mark the recording completed. Parts are deleted only \
                                         on success.",
                                    )
                                    .on_disabled_hover_text(
                                        "Another remux/merge is running — this one starts after \
                                         it (see Background jobs for the live queue).",
                                    )
                                    .clicked()
                                {
                                    *act = Some(Act::MergeSplit(i));
                                }
                            });
                            if ui
                                .button("📼 VOD")
                                .on_hover_text(
                                    "Archive the published VOD instead / as well — the only \
                                     way to get the part of the stream after the capture \
                                     died.",
                                )
                                .clicked()
                            {
                                *act = Some(Act::DownloadVodUnmerged(i));
                            }
                            if ui
                                .button("🔍")
                                .on_hover_text("View details in a window.")
                                .clicked()
                            {
                                let mut text = format!(
                                    "Status: {}\nPath: {}\n\nSurviving parts:",
                                    rec.status, rec.output_path
                                );
                                for p in parts {
                                    text.push_str(&format!(
                                        "\n  {} ({})",
                                        p.file_name()
                                            .map(|n| n.to_string_lossy())
                                            .unwrap_or_default(),
                                        fmt_bytes(fs.lock().unwrap().len(p) as i64),
                                    ));
                                }
                                if let Some(hint) = network_failure_hint(&rec.log_excerpt) {
                                    text.push_str("\n\n");
                                    text.push_str(hint);
                                }
                                if !rec.log_excerpt.is_empty() {
                                    text.push_str("\n\n");
                                    text.push_str(rec.log_excerpt.trim());
                                }
                                *act = Some(Act::ViewError(name.clone(), text));
                            }
                            ui.end_row();
                        }
                    });
            },
        );
    }

    /// ── Head/live join mismatches ──
    fn issues_head_mismatch_section(&self, ui: &mut egui::Ui, act: &mut Option<Act>) {
        if self.issues_head_mismatch.is_empty() {
            return;
        }
        let mismatch = &self.issues_head_mismatch;
        issue_section(
            ui,
            "issues_head_mismatch",
            "🔗 Head backfill can't join the live capture",
            mismatch.len(),
            "The backfilled head and the live capture carry different stream \
             parameters, so a lossless join is impossible. Usual cause: the \
             capture joined seconds after go-live, before Twitch listed the \
             source rendition — the take recorded a transcode while the head \
             fetched at source. Both files are kept and playable; pick a fix:",
            |ui| {
                egui::Grid::new("issues_head_mismatch_grid")
                    .num_columns(5)
                    .spacing([10.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (i, (rec, head, live)) in mismatch.iter().enumerate() {
                            let name = std::path::Path::new(&rec.output_path)
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| rec.output_path.clone());
                            let (head_d, live_d) = (
                                if head.is_empty() { "?" } else { head.as_str() },
                                if live.is_empty() { "?" } else { live.as_str() },
                            );
                            issue_name_cell(ui, &name);
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 160, 30),
                                format!("head {head_d} vs live {live_d}"),
                            );
                            if ui
                                .button("🧩 Re-fetch head")
                                .on_hover_text(
                                    "Fetch the head again at the live capture's own rendition \
                                     so the lossless join can succeed. Full quality is then \
                                     available via the VOD instead. (Post-stream: any \
                                     DMCA-muted section fetches muted.)",
                                )
                                .clicked()
                            {
                                *act = Some(Act::RefetchHeadMatchLive(i));
                            }
                            if ui
                                .button("📼 VOD")
                                .on_hover_text(
                                    "Grab the published VOD at source quality instead — the \
                                     full stream, including the head, at the better \
                                     resolution the live capture missed.",
                                )
                                .clicked()
                            {
                                *act = Some(Act::FetchVodForMismatch(i));
                            }
                            if ui
                                .button("✓ Dismiss")
                                .on_hover_text(
                                    "Acknowledge — keep the head and live capture as separate \
                                     playable files.",
                                )
                                .clicked()
                            {
                                *act = Some(Act::DismissMismatch(i));
                            }
                            ui.end_row();
                        }
                    });
            },
        );
    }

    /// Human reason text for a blocked `gap_splice_state` value.
    fn gap_splice_reason(state: &str) -> &'static str {
        match state {
            "mismatch" => "a recovered patch's codec/resolution doesn't match the capture",
            "anchor_failed" => "couldn't locate the gap precisely enough in the capture's own timeline",
            "verify_failed" => "the spliced result failed its post-splice verification",
            _ => "a safety check blocked it",
        }
    }

    /// ── Recovered gap patches that couldn't be spliced in ──
    fn issues_gap_splice_section(&self, ui: &mut egui::Ui, act: &mut Option<Act>) {
        if self.issues_gap_splice.is_empty() {
            return;
        }
        let blocked = &self.issues_gap_splice;
        issue_section(
            ui,
            "issues_gap_splice",
            "🩹 Recovered gap patches couldn't be spliced in",
            blocked.len(),
            "A recovered lost-segment patch exists, but gap-splice's safety checks \
             wouldn't trust the result — nothing was touched; the recording and its \
             patch(es) are exactly as they were. The recording is complete either way — \
             this only means the patch stays a separate sibling file instead of being \
             muxed into one gapless recording.",
            |ui| {
                egui::Grid::new("issues_gap_splice_grid")
                    .num_columns(4)
                    .spacing([10.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (i, rec) in blocked.iter().enumerate() {
                            let name = std::path::Path::new(&rec.output_path)
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| rec.output_path.clone());
                            issue_name_cell(ui, &name);
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 160, 30),
                                Self::gap_splice_reason(&rec.gap_splice_state),
                            );
                            if ui
                                .button("🩹 Patches")
                                .on_hover_text(
                                    "Open the folder holding the recovered patch file(s).",
                                )
                                .clicked()
                            {
                                *act = Some(Act::OpenGapSplicePatchFolder(i));
                            }
                            if ui
                                .button("✓ Dismiss")
                                .on_hover_text(
                                    "Acknowledge — the patch stays a separate file; splicing is \
                                     never re-attempted for this recording.",
                                )
                                .clicked()
                            {
                                *act = Some(Act::DismissGapSplice(i));
                            }
                            ui.end_row();
                        }
                    });
            },
        );
    }

    /// Summary count + Refresh + the bulk delete/clear buttons.
    #[allow(clippy::too_many_arguments)]
    fn issues_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        n_empty: usize,
        n_missing: usize,
        n_errors: usize,
        n_missing_errors: usize,
        n_stuck: usize,
        confirm_clear: bool,
        shown: usize,
        total: usize,
        act: &mut Option<Act>,
    ) {
        // Wrapped, not a single row: with the filter controls plus up to five
        // bulk buttons this outgrows any sane window width, and a plain
        // `horizontal` would push the last buttons out of reach instead of
        // starting a second line.
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "{} recording(s) need attention",
                self.issues_recs.len()
                    + n_missing
                    + n_errors
                    + n_stuck
                    + self.issues_muted_vod.len()
                    + self.issues_unmerged.len()
                    + self.issues_head_mismatch.len()
                    + self.issues_stale_recording.len()
            ));
            if ui.button("⟳ Refresh").clicked() {
                self.refresh = true;
            }
            ui.separator();
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("filter channel / file")
                    .desired_width(160.0),
            )
            .on_hover_text(
                "Show only rows whose channel name or file path contains this text \
                 (case-insensitive). Filters the table below, not the sections above.",
            );
            if !self.filter.is_empty() && ui.small_button("✕").on_hover_text("Clear the filter.").clicked() {
                self.filter.clear();
            }
            let mut kind = self.kind_filter;
            egui::ComboBox::from_id_salt("issues_kind_filter")
                .selected_text(kind.label())
                .width(140.0)
                .show_ui(ui, |ui| {
                    for k in IssueKind::ALL {
                        ui.selectable_value(&mut kind, k, k.label()).on_hover_text(k.hover());
                    }
                });
            self.kind_filter = kind;
            if shown != total {
                ui.weak(format!("showing {shown} of {total}")).on_hover_text(
                    "The filters narrow the table only. The bulk buttons to the right \
                     still act on their whole category — their labels carry the real \
                     count, which does not change when you filter.",
                );
            }
            ui.separator();
            if n_empty > 0 {
                if ui.button(format!("🗑 Delete {} empty", n_empty))
                    .on_hover_text("Delete all 0-byte captures — they contain no data.")
                    .clicked()
                {
                    *act = Some(Act::ClearEmpties);
                }
            }
            if !self.issues_recs.is_empty() {
                if confirm_clear {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 80, 80),
                        format!("Delete all {} capture files?", self.issues_recs.len()),
                    );
                    if ui.button("✓ Yes, delete all").clicked() {
                        *act = Some(Act::ClearAll);
                    }
                    if ui.button("✗ Cancel").clicked() {
                        *act = Some(Act::ConfirmClear);
                    }
                } else if ui.button("🗑 Delete all")
                    .on_hover_text("Delete all .ts capture files and remove them from the list.")
                    .clicked()
                {
                    *act = Some(Act::ConfirmClear);
                }
            }
            if n_missing > 0 {
                if ui.button(format!("🔗 Clear {} missing", n_missing))
                    .on_hover_text("Clear DB path for recordings whose output file was deleted from disk.")
                    .clicked()
                {
                    *act = Some(Act::ClearAllMissing);
                }
            }
            if n_missing_errors > 0 {
                if ui.button(format!("✕ Clear {} no-file failed", n_missing_errors))
                    .on_hover_text("Remove DB records for failed recordings whose output file no longer exists on disk.")
                    .clicked()
                {
                    *act = Some(Act::ClearFilelessErrors);
                }
            }
            if n_errors > 0 {
                if ui.button(format!("✕ Clear all {} failed", n_errors))
                    .on_hover_text("Delete DB records for all failed/aborted/orphaned recordings that still have a file. Files are deleted too.")
                    .clicked()
                {
                    *act = Some(Act::ClearAllErrors);
                }
            }
        });
    }

    /// The Issues grid: shared column header + the five row shapes
    /// (needs-remux / stuck-in-cache / file-missing / failed-no-file /
    /// failed), all drawn in the SAME column order so they stay aligned.
    #[allow(clippy::too_many_arguments)]
    fn issues_table(
        &mut self,
        ui: &mut egui::Ui,
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        now: i64,
        act: &mut Option<Act>,
        issues_entries: &mut [grid_columns::ColumnEntry],
        issues_order: &[usize],
        issues_reset: bool,
    ) {
        use egui_extras::{Column, TableBuilder};
        let mut tb = TableBuilder::new(ui)
            .id_salt(GridTableId::Issues.key())
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        if issues_reset {
            tb.reset();
        }
        for &i in issues_order {
            let c = &ISSUES_COLUMNS[i];
            let col = if c.stretch {
                Column::remainder().clip(true).at_least(c.min_width)
            } else {
                Column::auto().at_least(c.min_width)
            };
            tb = tb.column(col);
        }
        tb.header(20.0, |mut h| {
            for &i in issues_order {
                let c = &ISSUES_COLUMNS[i];
                h.col(|ui| {
                    if grid_header_cell_plain(ui, GridTableId::Issues, c, issues_entries, &ISSUES_COLUMNS) {
                        self.reorder_columns = Some(Arc::new(Mutex::new(ReorderColumnsState {
                            table: GridTableId::Issues,
                            draft: issues_entries.to_vec(),
                            apply: false,
                            cancel: false,
                        })));
                    }
                });
            }
        })
            .body(|mut body| {
                // Lowercased once here, not once per row: this runs for every
                // row of every repaint, and there can be several hundred.
                let filter = self.filter.to_lowercase();
                let kind = self.kind_filter;
                let show = |k: IssueKind| kind == IssueKind::All || kind == k;
                if show(IssueKind::NeedsRemux) {
                    self.issues_remux_rows(&mut body, issues_order, mon_info, ptex, now, &filter, act);
                }
                if show(IssueKind::StuckInCache) {
                    self.issues_stuck_rows(&mut body, issues_order, mon_info, ptex, &filter, act);
                }
                if show(IssueKind::FileMissing) {
                    self.issues_missing_rows(&mut body, issues_order, mon_info, ptex, &filter, act);
                }
                if show(IssueKind::FailedNoFile) {
                    self.issues_fileless_error_rows(&mut body, issues_order, mon_info, ptex, &filter, act);
                }
                if show(IssueKind::Failed) {
                    self.issues_error_rows(&mut body, issues_order, mon_info, ptex, &filter, act);
                }
            });
    }

    /// How many table rows survive the toolbar's two filters, out of how many
    /// there are — the toolbar's "showing N of M". Counted with the exact same
    /// predicate the row builders use, so the two can't disagree.
    fn issues_visible_count(
        &self,
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        filter: &str,
    ) -> (usize, usize) {
        let kind = self.kind_filter;
        let lists: [(IssueKind, &Vec<crate::models::Recording>); 5] = [
            (IssueKind::NeedsRemux, &self.issues_recs),
            (IssueKind::StuckInCache, &self.issues_stuck),
            (IssueKind::FileMissing, &self.issues_missing),
            (IssueKind::FailedNoFile, &self.issues_errors_no_file),
            (IssueKind::Failed, &self.issues_errors),
        ];
        let mut shown = 0;
        let mut total = 0;
        for (k, list) in lists {
            total += list.len();
            if kind != IssueKind::All && kind != k {
                continue;
            }
            shown += list
                .iter()
                .filter(|rec| {
                    let ch = mon_info.get(&rec.monitor_id).map(|(n, _)| n.as_str()).unwrap_or("?");
                    issue_filter_hit(filter, ch, &rec.output_path)
                })
                .count();
        }
        (shown, total)
    }

    /// Rows for recordings whose output is still a `.ts` in the capture
    /// cache — re-remuxable to MKV.
    // The per-column `match ISSUES_COLUMNS[ci].id { "actions" => { if ... } }`
    // arms are single-`if` bodies by nature of the column-dispatch pattern
    // (see `issues_window`).
    #[allow(clippy::collapsible_match)]
    #[allow(clippy::too_many_arguments)]
    fn issues_remux_rows(
        &mut self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        now: i64,
        filter: &str,
        act: &mut Option<Act>,
    ) {
        for (i, rec) in self.issues_recs.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            if !issue_filter_hit(filter, ch_name, &rec.output_path) {
                continue;
            }
            let path = std::path::Path::new(&rec.output_path);
            let fname = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let file_bytes = self.fs_probes.lock().unwrap().len(path);
            let empty = file_bytes == 0;
            // Parse the recording mode from "(p <mode>  )" in the filename.
            let mode = parse_capture_mode(&fname).unwrap_or_default();
            let remux_task = self.background_tasks.iter().find(|bt| {
                matches!(bt.kind, crate::events::BackgroundTaskKind::Remux(_))
                    && bt.id == rec.id as u64
            });
            let remuxing = remux_task.is_some();
            // Check finished_tasks for a prior failed remux attempt.
            let remux_err = self.finished_tasks.iter().find_map(|(t, outcome, _)| {
                if matches!(t.kind, crate::events::BackgroundTaskKind::Remux(_))
                    && t.id == rec.id as u64
                {
                    if let crate::events::TaskOutcome::Failed(msg) = outcome {
                        return Some(msg.clone());
                    }
                }
                None
            });
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname)
                                .on_hover_text(&rec.output_path);
                        }
                        "size" => {
                            if empty {
                                ui.colored_label(
                                    egui::Color32::from_rgb(180, 60, 60),
                                    "empty",
                                );
                            } else {
                                ui.label(fmt_bytes(file_bytes as i64));
                            }
                        }
                        "type" => {
                            // "TS" is implicit for all rows; show the mode qualifier if present.
                            let type_str = if mode.is_empty() {
                                "TS".to_string()
                            } else {
                                format!("TS · {mode}")
                            };
                            ui.label(type_str)
                                .on_hover_text(format!("status: {}", rec.status));
                        }
                        "status" => {
                            if let Some(bt) = remux_task {
                                let elapsed = (now - bt.started_at).max(0);
                                let hover = bt.progress_info.as_deref()
                                    .map(|i| format!("{}\nElapsed: {}", i, fmt_duration(elapsed)))
                                    .unwrap_or_else(|| fmt_duration(elapsed));
                                if let Some(p) = bt.progress {
                                    ui.add(
                                        egui::ProgressBar::new(p)
                                            .show_percentage()
                                            .desired_width(110.0),
                                    )
                                    .on_hover_text(hover);
                                } else if let Some(ref info) = bt.progress_info {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 160, 220),
                                        info,
                                    )
                                    .on_hover_text(format!("Elapsed: {}", fmt_duration(elapsed)));
                                } else {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 160, 220),
                                        format!("⏳ remuxing… {}", fmt_duration(elapsed)),
                                    );
                                }
                            } else if empty {
                                ui.colored_label(
                                    egui::Color32::from_rgb(180, 60, 60),
                                    "✗ empty — no data",
                                ).on_hover_text("Capture wrote 0 bytes. Delete this file.");
                            } else if let Some(ref err) = remux_err {
                                ui.colored_label(
                                    egui::Color32::from_rgb(180, 60, 60),
                                    "✗ remux failed",
                                ).on_hover_text(err.as_str());
                            } else {
                                let (icon, color) = state_icon(&rec.status);
                                ui.colored_label(color, icon)
                                    .on_hover_text(&rec.status);
                            }
                        }
                        "actions" => {
                            if !remuxing {
                                if empty {
                                    ui.add_enabled(false, egui::Button::new("🔄").small())
                                        .on_hover_text("Empty capture — nothing to remux.");
                                } else if remux_err.is_some() {
                                    ui.add_enabled(false, egui::Button::new("🔄").small())
                                        .on_hover_text("Remux failed — see status cell.");
                                } else if ui
                                    .button("🔄")
                                    .on_hover_text("Re-remux: convert .ts → .mkv via ffmpeg.")
                                    .clicked()
                                {
                                    *act = Some(Act::Remux(i));
                                }
                                if ui.button("🗑")
                                    .on_hover_text(
                                        if empty {
                                            "Delete this empty capture file."
                                        } else {
                                            "Delete the .ts capture file and remove from list."
                                        }
                                    )
                                    .clicked()
                                {
                                    *act = Some(Act::Delete(i));
                                }
                                if ui.button("🔍")
                                    .on_hover_text("View error details in a window.")
                                    .clicked()
                                {
                                    let details = if empty {
                                        "Capture wrote 0 bytes. Delete this file.".to_string()
                                    } else if let Some(ref err) = remux_err {
                                        err.clone()
                                    } else {
                                        rec.status.clone()
                                    };
                                    *act = Some(Act::ViewError(fname.clone(), details));
                                }
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// Stuck-in-cache rows: capture succeeded but the promote-to-output-dir
    /// move never completed (non-.ts, so distinct from the re-remux rows) —
    /// most commonly a filename-length overflow. "Recover" retries the move
    /// with a shortened name if that's what's blocking it.
    #[allow(clippy::collapsible_match)]
    fn issues_stuck_rows(
        &mut self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        filter: &str,
        act: &mut Option<Act>,
    ) {
        for (k, rec) in self.issues_stuck.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            if !issue_filter_hit(filter, ch_name, &rec.output_path) {
                continue;
            }
            let path = std::path::Path::new(&rec.output_path);
            let fname = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let file_bytes = self.fs_probes.lock().unwrap().len(path);
            let mode = parse_capture_mode(&fname).unwrap_or_default();
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname).on_hover_text(&rec.output_path);
                        }
                        "size" => { ui.label(fmt_bytes(file_bytes as i64)); }
                        "type" => {
                            let ext = path
                                .extension()
                                .map(|e| e.to_string_lossy().to_uppercase())
                                .unwrap_or_else(|| "?".into());
                            let type_str = if mode.is_empty() {
                                ext
                            } else {
                                format!("{ext} · {mode}")
                            };
                            ui.label(type_str).on_hover_text(format!("status: {}", rec.status));
                        }
                        "status" => {
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 150, 60),
                                "⚠ stuck in cache",
                            ).on_hover_text(STUCK_IN_CACHE_DETAILS);
                        }
                        "actions" => {
                            if ui
                                .button("📦")
                                .on_hover_text("Recover: move it to its output folder, shortening the name if needed.")
                                .clicked()
                            {
                                *act = Some(Act::RecoverStuck(k));
                            }
                            if ui.button("🔍")
                                .on_hover_text("View error details in a window.")
                                .clicked()
                            {
                                *act = Some(Act::ViewError(
                                    fname.clone(),
                                    format!(
                                        "{STUCK_IN_CACHE_DETAILS}\nPath: {}",
                                        rec.output_path
                                    ),
                                ));
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// Missing-output-file rows (completed/failed/ended but file gone from disk).
    #[allow(clippy::collapsible_match)]
    fn issues_missing_rows(
        &self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        filter: &str,
        act: &mut Option<Act>,
    ) {
        for (j, rec) in self.issues_missing.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            if !issue_filter_hit(filter, ch_name, &rec.output_path) {
                continue;
            }
            let path = std::path::Path::new(&rec.output_path);
            let fname = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_uppercase())
                .unwrap_or_else(|| "?".into());
            let details = format!(
                "Output file was deleted from disk.\nDB status: {}\nPath: {}",
                rec.status, rec.output_path
            );
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname).on_hover_text(&rec.output_path);
                        }
                        "size" => {
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 130, 30),
                                "gone",
                            );
                        }
                        "type" => { ui.label(ext.as_str()); }
                        "status" => {
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 130, 30),
                                "✗ file missing",
                            ).on_hover_text(&details);
                        }
                        "actions" => {
                            if ui.button("🔗 Clear path")
                                .on_hover_text("Remove the stale path from the database record.")
                                .clicked()
                            {
                                *act = Some(Act::ClearPath(j));
                            }
                            if ui.button("🔍")
                                .on_hover_text("View error details in a window.")
                                .clicked()
                            {
                                *act = Some(Act::ViewError(fname.clone(), details.clone()));
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// ── Failed but file gone (treated as missing) ──
    #[allow(clippy::collapsible_match)]
    fn issues_fileless_error_rows(
        &self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        filter: &str,
        act: &mut Option<Act>,
    ) {
        for (j2, rec) in self.issues_errors_no_file.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            if !issue_filter_hit(filter, ch_name, &rec.output_path) {
                continue;
            }
            let path = std::path::Path::new(&rec.output_path);
            let fname = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_uppercase())
                .unwrap_or_else(|| "?".to_string());
            let details = {
                let mut parts = vec![
                    format!("status: {}", rec.status),
                    format!("path: {}", rec.output_path),
                ];
                if let Some(hint) = network_failure_hint(&rec.log_excerpt) {
                    parts.push(format!("\n{hint}"));
                }
                if !rec.log_excerpt.is_empty() {
                    parts.push(rec.log_excerpt.trim().to_string());
                }
                parts.join("\n")
            };
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname).on_hover_text(&rec.output_path);
                        }
                        "size" => {
                            ui.colored_label(egui::Color32::from_rgb(200, 130, 30), "gone");
                        }
                        "type" => { ui.label(ext.as_str()); }
                        "status" => {
                            let exit_str = rec.exit_code
                                .map(|c| format!(" (exit {c})"))
                                .unwrap_or_default();
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 80, 80),
                                format!("✗ {}{} — file missing", rec.status, exit_str),
                            ).on_hover_text(&details);
                        }
                        "actions" => {
                            if ui.button("✓ Ack")
                                .on_hover_text(
                                    "Acknowledge: stop this take's ⚠ bubbling up to the \
                                     instance/channel row (it stays visible, muted, on the \
                                     take's own row) without deleting anything.",
                                )
                                .clicked()
                            {
                                *act = Some(Act::AckMissingError(j2));
                            }
                            if ui.button("✕ Clear")
                                .on_hover_text("Permanently remove this failed recording from the database.")
                                .clicked()
                            {
                                *act = Some(Act::ClearMissingError(j2));
                            }
                            if ui.button("🔍")
                                .on_hover_text("View error details in a window.")
                                .clicked()
                            {
                                *act = Some(Act::ViewError(fname.clone(), details.clone()));
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// ── Failed / aborted / orphaned rows ──
    #[allow(clippy::collapsible_match)]
    fn issues_error_rows(
        &mut self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        filter: &str,
        act: &mut Option<Act>,
    ) {
        for (k, rec) in self.issues_errors.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            if !issue_filter_hit(filter, ch_name, &rec.output_path) {
                continue;
            }
            let has_file = !rec.output_path.is_empty()
                && self.fs_probes.lock().unwrap().is_file(std::path::Path::new(&rec.output_path));
            let has_ts = rec.output_path.ends_with(".ts");
            let path = std::path::Path::new(&rec.output_path);
            let fname = if rec.output_path.is_empty() {
                "—".to_string()
            } else {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| rec.output_path.clone())
            };
            let file_size = if has_file {
                self.fs_probes.lock().unwrap().len(path)
            } else {
                0
            };
            let exit_str = match rec.exit_code {
                Some(c) => format!("exit {c}"),
                None => String::new(),
            };
            // Build a hover text from whatever info we have.
            let hover = {
                let mut parts = vec![format!("status: {}", rec.status)];
                if !exit_str.is_empty() { parts.push(exit_str.clone()); }
                if !rec.output_path.is_empty() { parts.push(format!("path: {}", rec.output_path)); }
                if let Some(hint) = network_failure_hint(&rec.log_excerpt) {
                    parts.push(format!("\n{hint}"));
                }
                if !rec.log_excerpt.is_empty() { parts.push(format!("\n{}", rec.log_excerpt.trim())); }
                parts.join("\n")
            };
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname).on_hover_text(&rec.output_path);
                        }
                        "size" => {
                            if has_file && file_size > 0 {
                                ui.label(fmt_bytes(file_size as i64));
                            } else if has_file {
                                ui.colored_label(egui::Color32::from_rgb(180, 60, 60), "empty");
                            } else {
                                ui.weak("—");
                            }
                        }
                        "type" => {
                            let ext = if rec.output_path.is_empty() {
                                "—".to_string()
                            } else {
                                path.extension()
                                    .map(|e| e.to_string_lossy().to_uppercase())
                                    .unwrap_or_else(|| "?".to_string())
                            };
                            ui.label(ext);
                        }
                        "status" => {
                            let color = egui::Color32::from_rgb(200, 80, 80);
                            let label = if exit_str.is_empty() {
                                format!("✗ {}", rec.status)
                            } else {
                                format!("✗ {} ({})", rec.status, exit_str)
                            };
                            ui.colored_label(color, label)
                                .on_hover_text(&hover);
                        }
                        "actions" => {
                            // Remux if there's a .ts file on disk.
                            if has_file && has_ts {
                                if ui.button("🔄")
                                    .on_hover_text("Attempt to remux this partial .ts to MKV.")
                                    .clicked()
                                {
                                    *act = Some(Act::RemuxError(k));
                                }
                            }
                            // Delete file + clear path.
                            if has_file {
                                if ui.button("🗑")
                                    .on_hover_text("Delete the output file and clear it from the database.")
                                    .clicked()
                                {
                                    *act = Some(Act::DeleteError(k));
                                }
                            }
                            if ui.button("✓ Ack")
                                .on_hover_text(
                                    "Acknowledge: stop this take's ⚠ bubbling up to the \
                                     instance/channel row (it stays visible, muted, on the \
                                     take's own row) without deleting anything.",
                                )
                                .clicked()
                            {
                                *act = Some(Act::AckError(k));
                            }
                            // Remove DB record entirely.
                            if ui.button("✕ Clear")
                                .on_hover_text("Permanently remove this failed recording from the database.")
                                .clicked()
                            {
                                *act = Some(Act::ClearError(k));
                            }
                            if ui.button("🔍")
                                .on_hover_text("View error details in a window.")
                                .clicked()
                            {
                                *act = Some(Act::ViewError(fname.clone(), hover.clone()));
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }
}

impl StreamArchiverApp {
    /// Resolve per-row channel identity — the instance's own avatar plus the
    /// name colour the Streams grid gives that channel — for a feed of rows
    /// keyed by row id (`(key, monitor_id, channel name)`). Shared by the 🔔
    /// Notifications and 🚨 Warnings windows so both label rows with the same
    /// face and colour a channel has everywhere else. Rows whose monitor was
    /// deleted still match by channel name where one is still tracked.
    /// Resolved once per distinct instance, not per row — either feed holds up
    /// to 500 rows and this runs every frame its window is open.
    fn feed_identities(
        &mut self,
        ctx: &egui::Context,
        keys: &[(i64, Option<i64>, String)],
    ) -> HashMap<i64, (Option<egui::TextureHandle>, (egui::Color32, bool))> {
        let by_mid: HashMap<i64, usize> =
            self.rows.iter().enumerate().map(|(i, r)| (r.monitor.id, i)).collect();
        let mut by_name: HashMap<&str, usize> = HashMap::new();
        for (i, r) in self.rows.iter().enumerate() {
            by_name.entry(r.channel.name.as_str()).or_insert(i);
        }
        let resolved: Vec<(i64, usize)> = keys
            .iter()
            .filter_map(|(key, mid, name)| {
                let ri = mid
                    .and_then(|m| by_mid.get(&m).copied())
                    .or_else(|| {
                        (!name.is_empty())
                            .then(|| by_name.get(name.as_str()).copied())
                            .flatten()
                    })?;
                Some((*key, ri))
            })
            .collect();
        let mut avatars: HashMap<usize, egui::TextureHandle> = HashMap::new();
        let mut colors: HashMap<i64, (egui::Color32, bool)> = HashMap::new();
        for &(_, ri) in &resolved {
            let (mid, cid) = (self.rows[ri].monitor.id, self.rows[ri].channel.id);
            if let std::collections::hash_map::Entry::Vacant(e) = avatars.entry(ri) {
                if !self.instance_icons_small.contains_key(&mid) {
                    let tex = resolve_instance_icon_small(&self.rows[ri], ctx);
                    self.instance_icons_small.insert(mid, tex);
                }
                if let Some(tex) = self.instance_icons_small.get(&mid).and_then(|t| t.clone()) {
                    e.insert(tex);
                }
            }
            if let std::collections::hash_map::Entry::Vacant(e) = colors.entry(cid) {
                // Can't `or_insert_with` — resolving a name colour needs `&mut
                // self` (it fills the fetched-colour cache) and the closure
                // would already be holding the map.
                let color = self.channel_name_color(cid);
                e.insert(color);
            }
        }
        resolved
            .iter()
            .map(|&(key, ri)| {
                let cid = self.rows[ri].channel.id;
                (key, (avatars.get(&ri).cloned(), colors[&cid]))
            })
            .collect()
    }

    /// Returns the list of active (non-dismissed) quota warning keys.
    /// Each key is a stable string used for both display and dismissal tracking.
    pub(super) fn active_quota_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.yt_quota_today >= self.yt_quota_cutoff {
            let key = "youtube_units_exceeded".to_string();
            if !self.dismissed_quota_warnings.contains(&key) {
                warnings.push(key);
            }
        } else if self.yt_quota_cutoff > 0
            && self.yt_quota_today as f32 / self.yt_quota_cutoff as f32 >= 0.9
        {
            let key = "youtube_units_near_cutoff".to_string();
            if !self.dismissed_quota_warnings.contains(&key) {
                warnings.push(key);
            }
        }
        if self.yt_search_today >= self.yt_search_cutoff {
            let key = "youtube_search_exceeded".to_string();
            if !self.dismissed_quota_warnings.contains(&key) {
                warnings.push(key);
            }
        } else if self.yt_search_cutoff > 0
            && self.yt_search_today as f32 / self.yt_search_cutoff as f32 >= 0.9
        {
            let key = "youtube_search_near_cutoff".to_string();
            if !self.dismissed_quota_warnings.contains(&key) {
                warnings.push(key);
            }
        }
        warnings
    }
    /// The notifications feed window (bell button). A persisted, filterable,
    /// searchable aggregation of went-live / recording / error / schedule /
    /// YouTube-post / task-failure events. Mirrors `issues_window`: the unread
    /// badge count is refreshed on a throttle even while closed, so the header
    /// bell stays live. Both the count and the row list are cheap SQLite reads,
    /// done synchronously.
    #[allow(deprecated)] // CentralPanel::show inside a viewport (matches issues_window)
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn notifications_window(&mut self, ctx: &egui::Context) {
        use std::time::{Duration, Instant};
        let interval = if self.show_notifications {
            Duration::from_secs(3)
        } else {
            Duration::from_secs(60)
        };
        let stale = self
            .notif_refreshed
            .map(|t| t.elapsed() >= interval)
            .unwrap_or(true);
        // All three throttles here also hold while the user has text selected
        // anywhere — swapping the model out from under a selection is what
        // cancels it (see `text_selection_hold`). The overdue refresh runs the
        // frame after the hold clears; none of these feed anything but the UI.
        if stale && !super::text_selection_hold(ctx) {
            self.notif_unread = self.core.store.unread_notification_count().unwrap_or(0);
            // The 📣 Posts tab badge rides the same throttle — it's the same
            // `notification` table, just one kind of it.
            self.posts_unread = self
                .core
                .store
                .unread_notification_count_by_kind(crate::models::NotificationKind::YoutubePost.id())
                .unwrap_or(0);
            if self.show_notifications {
                self.notifications = self.core.store.list_notifications(500).unwrap_or_default();
            }
            self.notif_refreshed = Some(Instant::now());
        }
        if !self.show_notifications {
            self.notifications_popup = None;
            return;
        }

        // Ensure a popup instance exists, seeded from the persisted filter
        // fields (remembered across a close/reopen, same as before).
        if self.notifications_popup.is_none() {
            self.notifications_popup = Some(Arc::new(Mutex::new(NotificationsPopupState {
                search: self.notif_search.clone(),
                kind_filter: self.notif_kind_filter,
                bgcolor: self.notif_bgcolor,
                rows: Vec::new(),
                ident: HashMap::new(),
                live_mids: HashMap::new(),
                have_player: false,
                act: None,
                closed: false,
            })));
        }
        let popup_state = self.notifications_popup.clone().unwrap();

        // Refresh rows/channel-identity/live-player-eligibility into the popup
        // on the same throttle as the DB reload above — not every frame.
        if stale {
            let keys: Vec<(i64, Option<i64>, String)> = self
                .notifications
                .iter()
                .map(|r| (r.id, r.monitor_id, r.channel.clone()))
                .collect();
            let ident = self.feed_identities(ctx, &keys);
            // Which feed rows can offer "Watch in player" — only rows that NAMED a
            // still-tracked monitor (a name-only identity match resolves a colour,
            // not a source URL to tune into).
            let tracked: std::collections::HashSet<i64> =
                self.rows.iter().map(|r| r.monitor.id).collect();
            let live_mids: HashMap<i64, i64> = self
                .notifications
                .iter()
                .filter_map(|r| Some((r.id, r.monitor_id.filter(|m| tracked.contains(m))?)))
                .collect();
            let have_player = !self.settings.media_player_path.trim().is_empty();
            let mut s = popup_state.lock().unwrap();
            s.rows = self.notifications.clone();
            s.ident = ident;
            s.live_mids = live_mids;
            s.have_player = have_player;
        }

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("notifications_vp"),
            egui::ViewportBuilder::default()
                .with_title("🔔 Notifications")
                .with_inner_size([720.0, 520.0]),
            popup_state.clone(),
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }

                // Session-only category + text filter over the loaded rows → surviving
                // indices (recomputed each frame from last frame's filter values).
                let q = s.search.trim().to_lowercase();
                let kind_filter = s.kind_filter;
                let visible: Vec<usize> = s
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        kind_filter.map(|k| r.kind == k.id()).unwrap_or(true)
                            && (q.is_empty()
                                || r.title.to_lowercase().contains(&q)
                                || r.body.to_lowercase().contains(&q)
                                || r.channel.to_lowercase().contains(&q))
                    })
                    .map(|(i, _)| i)
                    .collect();

                egui::CentralPanel::default().show(ctx, |ui| {
                    // ── Toolbar: kind filter + search + mark-all-read ──
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("notif_kind_filter")
                            .selected_text(match s.kind_filter {
                                None => "All kinds".to_string(),
                                Some(k) => k.label().to_string(),
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.kind_filter, None, "All kinds");
                                for k in crate::models::NotificationKind::ALL {
                                    ui.selectable_value(
                                        &mut s.kind_filter,
                                        Some(k),
                                        format!("{} {}", k.icon(), k.label()),
                                    );
                                }
                            });
                        ui.add(
                            egui::TextEdit::singleline(&mut s.search)
                                .hint_text("Filter…")
                                .desired_width(180.0),
                        );
                        if !s.search.is_empty()
                            && ui.button("✕").on_hover_text("Clear filter").clicked()
                        {
                            s.search.clear();
                        }
                        ui.checkbox(&mut s.bgcolor, "Row colors").on_hover_text(
                            "Paint each row in its kind's colour (live purple, finished \
                             blue, error red, …) so the feed can be skimmed by colour. \
                             Off = plain rows; the coloured icons and dots still carry \
                             the kind.",
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("✔ Mark all read").clicked() {
                                s.act = Some(NotifAct::MarkAllRead);
                            }
                        });
                    });
                    ui.separator();

                    if s.rows.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| ui.weak("No notifications yet."));
                        return;
                    }
                    if visible.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| ui.weak("No notifications match the filter."));
                        return;
                    }

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for &i in &visible {
                                let r = &s.rows[i];
                                let kind = crate::models::NotificationKind::from_id(&r.kind);
                                let icon = kind.map(|k| k.icon()).unwrap_or("•");
                                // Row tint by severity/kind; read rows keep the
                                // hue but fade back, so the feed still reads as
                                // a list rather than a wall of paint.
                                let (rgb, accent) = notif_colors(kind, &r.severity);
                                let alpha = if r.read { 22 } else { 70 };
                                let tint = if s.bgcolor {
                                    egui::Color32::from_rgba_unmultiplied(
                                        rgb.0, rgb.1, rgb.2, alpha,
                                    )
                                } else {
                                    egui::Color32::TRANSPARENT
                                };
                                let (avatar, name_color) = match s.ident.get(&r.id) {
                                    Some((a, (base, adjust))) => (
                                        a.as_ref(),
                                        Some(if *adjust {
                                            readable_color(*base, tint)
                                        } else {
                                            *base
                                        }),
                                    ),
                                    None => (None, None),
                                };
                                // Same stable-id treatment as the Warnings
                                // rows — see the comment there.
                                ui.push_id(r.id, |ui| {
                                egui::Frame::group(ui.style()).fill(tint).show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        // Unread rows show a filled accent dot; read rows a dim one.
                                        ui.label(
                                            egui::RichText::new(if r.read { "○" } else { "●" })
                                                .small()
                                                .color(accent),
                                        )
                                        .on_hover_text(if r.read {
                                            "Read"
                                        } else {
                                            "Unread — counts toward the 🔔 badge until \
                                             marked read (opening the feed alone doesn't \
                                             clear it)"
                                        });
                                        ui.label(egui::RichText::new(icon).color(accent))
                                            .on_hover_text(
                                                kind.map(|k| k.label()).unwrap_or("Notification"),
                                            );
                                        if let Some(tex) = avatar {
                                            let resp = ui.add(
                                                egui::Image::from_texture(tex)
                                                    .fit_to_exact_size(egui::vec2(24.0, 24.0))
                                                    .corner_radius(egui::CornerRadius::same(4)),
                                            );
                                            queue_alt_image_preview(ui.ctx(), &resp, tex);
                                            ui.add_space(2.0);
                                        }
                                        ui.vertical(|ui| {
                                            ui.horizontal(|ui| {
                                                let when = fmt_datetime_short(r.created_at);
                                                if !when.is_empty() {
                                                    ui.label(egui::RichText::new(when).weak());
                                                }
                                                notif_title(ui, &r.title, &r.channel, name_color, None);
                                            });
                                            // Capture alerts keep their body OUT
                                            // of the feed: the 🚨 Warnings window
                                            // is the authoritative view of the
                                            // same alert (explanation, log line,
                                            // Ack/Log actions), and repeating its
                                            // whole paragraph per feed row just
                                            // duplicated it. The 🚨 Details
                                            // button on the right jumps there.
                                            let is_capture_alert = kind
                                                == Some(
                                                    crate::models::NotificationKind::CaptureAlert,
                                                );
                                            if !r.body.is_empty() && !is_capture_alert {
                                                ui.label(egui::RichText::new(&r.body).weak());
                                            }
                                        });
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                // Rightmost first (right-to-left layout):
                                                // the web link, then the in-app companion.
                                                if !r.action_url.is_empty() {
                                                    let label =
                                                        notif_action_label(&r.action_label);
                                                    if ui
                                                        .button(label)
                                                        .on_hover_text(format!(
                                                            "Open in your browser:\n{}",
                                                            r.action_url
                                                        ))
                                                        .clicked()
                                                    {
                                                        s.act =
                                                            Some(NotifAct::OpenUrl(r.action_url.clone()));
                                                    }
                                                }
                                                if kind
                                                    == Some(
                                                        crate::models::NotificationKind::YoutubePost,
                                                    )
                                                    && let Some(pid) = notif_post_id(r)
                                                    && ui
                                                        .button("View post")
                                                        .on_hover_text(
                                                            "Show this post in the app's own 📣 \
                                                             Posts window — full text, images \
                                                             and poll, archived locally.",
                                                        )
                                                        .clicked()
                                                {
                                                    s.act = Some(NotifAct::ViewPost(pid));
                                                }
                                                if kind
                                                    == Some(
                                                        crate::models::NotificationKind::CaptureAlert,
                                                    )
                                                    && ui
                                                        .button("🚨 Details")
                                                        .on_hover_text(
                                                            "Open the 🚨 Capture warnings \
                                                             window — the full view of this \
                                                             alert (explanation, matched log \
                                                             line, Ack / Log actions).",
                                                        )
                                                        .clicked()
                                                {
                                                    s.act = Some(NotifAct::OpenWarnings);
                                                }
                                                if s.have_player
                                                    && notif_is_live_stream(kind)
                                                    && let Some(&mid) = s.live_mids.get(&r.id)
                                                    && ui
                                                        .button("Watch in player")
                                                        .on_hover_text(
                                                            "Tune into this channel's live edge \
                                                             in your media player — same as \
                                                             ▶ Play in the Streams grid. Only \
                                                             works while the channel is still \
                                                             live.",
                                                        )
                                                        .clicked()
                                                {
                                                    s.act = Some(NotifAct::WatchInPlayer(mid));
                                                }
                                            },
                                        );
                                    });
                                });
                                });
                            }
                        });
                });
                // Child viewports draw their own copy of the Alt-hover overlay —
                // the main viewport's draw call can't reach here.
                draw_alt_image_preview(ctx);
            },
        );

        // Filter fields are remembered across a close/reopen (mirrors the
        // pre-migration code, which read them straight off `self` every
        // frame); `bgcolor` also persists to settings on change.
        let (search, kind_filter, bgcolor, closed, act) = {
            let mut s = popup_state.lock().unwrap();
            (s.search.clone(), s.kind_filter, s.bgcolor, s.closed, s.act.take())
        };
        self.notif_search = search;
        self.notif_kind_filter = kind_filter;
        if bgcolor != self.notif_bgcolor {
            self.notif_bgcolor = bgcolor;
            let _ = self
                .core
                .store
                .set_setting(K_NOTIF_BGCOLOR, if bgcolor { "1" } else { "0" });
        }
        if closed {
            self.show_notifications = false;
            self.notifications_popup = None;
        }
        match act {
            Some(NotifAct::OpenUrl(url)) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Some(NotifAct::MarkAllRead) => {
                let now = crate::models::now_unix();
                let _ = self.core.store.mark_notifications_read_before(now);
                self.notif_unread = 0;
                for r in &mut self.notifications {
                    r.read = true;
                }
                for r in &mut popup_state.lock().unwrap().rows {
                    r.read = true;
                }
            }
            Some(NotifAct::ViewPost(post_id)) => {
                // Focus overrides the feed's own filters (see `posts_focus_post`)
                // so the post can't be hidden by whatever the Posts window was
                // last filtered to.
                self.posts_focus_post = Some(post_id);
                self.posts_render_limit = POSTS_PAGE_SIZE;
                self.posts_refreshed = None; // pick up a just-ingested post
                self.show_posts_window = true;
            }
            Some(NotifAct::WatchInPlayer(mid)) => {
                let player = self.settings.media_player_path.trim().to_string();
                match self.rows.iter().find(|r| r.monitor.id == mid) {
                    Some(row) => {
                        if let Some(msg) = spawn_play_new_instance(
                            row,
                            &player,
                            &self.settings,
                            &self.core.store,
                            false,
                            None,
                            crate::ui::player::LiveMetaCtx::from_core(&self.core).as_ref(),
                            false,
                            None,
                        ) {
                            self.status = msg;
                        }
                    }
                    None => {
                        self.status = "That channel instance no longer exists.".into();
                    }
                }
            }
            Some(NotifAct::OpenWarnings) => {
                self.show_warnings = true;
                // The window is often ALREADY open — just buried under the
                // notifications window that hosts this button — and setting
                // the flag again is invisible. Raise and focus it explicitly
                // (a not-yet-open window gets the command next frame, once
                // `warnings_window` has created the viewport; commands to a
                // nonexistent viewport id are dropped harmlessly).
                let vp = egui::ViewportId::from_hash_of("warnings_vp");
                ctx.send_viewport_cmd_to(vp, egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd_to(vp, egui::ViewportCommand::Focus);
            }
            None => {}
        }
    }

    /// 🚨 Warnings window: capture alerts scraped from the tools' own log
    /// files (streamlink sequence gaps / failed fetches = lost data, yt-dlp
    /// ERROR/WARNING lines). One aggregated row per (take, kind); red rows are
    /// errors, yellow rows warnings; acknowledging clears the header badge but
    /// keeps the row — new occurrences un-acknowledge automatically. The badge
    /// counts refresh on the same open/closed throttle as the bell.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn warnings_window(&mut self, ctx: &egui::Context) {
        use std::time::{Duration, Instant};
        let interval = if self.show_warnings {
            Duration::from_secs(3)
        } else {
            Duration::from_secs(60)
        };
        let stale = self.warn_refreshed.map(|t| t.elapsed() >= interval).unwrap_or(true);
        if stale && !super::text_selection_hold(ctx) {
            self.warn_badge = self.core.store.alert_badge_counts().unwrap_or((0, 0));
            // The Streams-grid take/stream badges ride the same throttle.
            self.rec_alert_badges =
                self.core.store.alert_badges_by_recording().unwrap_or_default();
            // The 🗑 tab badge rides along: a trash folder is only emptied by
            // hand, so it needs to be visible without opening the view.
            self.trash_badge = self.core.store.trashed_file_count().unwrap_or(0);
            if self.show_warnings {
                self.warnings_rows = self.core.store.list_capture_alerts(500).unwrap_or_default();
            }
            self.warn_refreshed = Some(Instant::now());
        }
        if !self.show_warnings {
            self.warnings_popup = None;
            return;
        }

        // Ensure a popup instance exists, seeded from the persisted filter
        // fields (remembered across a close/reopen, same as before).
        if self.warnings_popup.is_none() {
            self.warnings_popup = Some(Arc::new(Mutex::new(WarningsPopupState {
                search: self.warn_search.clone(),
                sev_filter: self.warn_sev_filter,
                hide_acked: self.warn_hide_acked,
                bgcolor: self.warn_bgcolor,
                rows: Vec::new(),
                ident: HashMap::new(),
                act: None,
                closed: false,
            })));
        }
        let popup_state = self.warnings_popup.clone().unwrap();

        // Refresh rows/channel-identity into the popup on the same throttle
        // as the DB reload above — not every frame (up to 500 rows isn't
        // free to clone at 60fps for no reason).
        if stale {
            let keys: Vec<(i64, Option<i64>, String)> = self
                .warnings_rows
                .iter()
                .map(|r| (r.id, r.monitor_id, r.channel.clone()))
                .collect();
            let ident = self.feed_identities(ctx, &keys);
            let mut s = popup_state.lock().unwrap();
            s.rows = self.warnings_rows.clone();
            s.ident = ident;
        }

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("warnings_vp"),
            egui::ViewportBuilder::default()
                .with_title("🚨 Capture warnings")
                .with_inner_size([860.0, 520.0]),
            popup_state.clone(),
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                let q = s.search.trim().to_lowercase();
                let sev = s.sev_filter;
                let visible: Vec<usize> = s
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        let cat = crate::downloader::alert_category(&r.kind, &r.last_line).1;
                        (!s.hide_acked || !r.acked)
                            && sev.map(|errs_only| (r.severity == "error") == errs_only).unwrap_or(true)
                            && (q.is_empty()
                                || r.channel.to_lowercase().contains(&q)
                                || r.kind.to_lowercase().contains(&q)
                                || cat.to_lowercase().contains(&q)
                                || r.last_line.to_lowercase().contains(&q)
                                || r.take_key.to_lowercase().contains(&q))
                    })
                    .map(|(i, _)| i)
                    .collect();
                // Unacked alerts grouped by category, for the "Ack group" menu —
                // plus a state-based "Fixed" group covering every green row (fully
                // recovered or superseded), so healed history clears in one click.
                let mut ack_groups: std::collections::BTreeMap<(&str, &str), Vec<i64>> =
                    std::collections::BTreeMap::new();
                let mut fixed_ids: Vec<i64> = Vec::new();
                for r in &s.rows {
                    if !r.acked {
                        let cat = crate::downloader::alert_category(&r.kind, &r.last_line);
                        ack_groups.entry(cat).or_default().push(r.id);
                        // Mirrors the row-tint logic below: healed (every lost range
                        // re-fetched) or superseded (a later completed take covers
                        // the dead one).
                        let healed = r.ranges_total > 0 && r.recovered == r.ranges_total;
                        let superseded = !healed
                            && r.severity == "error"
                            && r.superseded
                            && r.ranges_total == 0;
                        if healed || superseded {
                            fixed_ids.push(r.id);
                        }
                    }
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("warn_sev_filter")
                            .selected_text(match s.sev_filter {
                                None => "All severities",
                                Some(true) => "Errors only",
                                Some(false) => "Warnings only",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.sev_filter, None, "All severities");
                                ui.selectable_value(&mut s.sev_filter, Some(true), "Errors only");
                                ui.selectable_value(&mut s.sev_filter, Some(false), "Warnings only");
                            })
                            .response
                            .on_hover_text(
                                "Errors mean data is missing from a capture (lost segments, \
                                 failed fetches, tool errors); warnings are non-fatal tool \
                                 complaints.",
                            );
                        ui.add(
                            egui::TextEdit::singleline(&mut s.search)
                                .hint_text("Filter…")
                                .desired_width(180.0),
                        )
                        .on_hover_text("Matches channel, kind, file path, and the last log line.");
                        if !s.search.is_empty()
                            && ui.button("✕").on_hover_text("Clear filter").clicked()
                        {
                            s.search.clear();
                        }
                        ui.checkbox(&mut s.hide_acked, "Hide acknowledged").on_hover_text(
                            "Only show alerts that still need attention — acknowledged rows \
                             (including healed/superseded ones you've cleared) drop out of \
                             the list until new damage un-acknowledges them again.",
                        );
                        ui.checkbox(&mut s.bgcolor, "Row colors").on_hover_text(
                            "Paint each row in its state colour — red = data missing, \
                             yellow = warning, green = recovered/superseded; dimmed once \
                             acknowledged. Off = plain rows; the coloured icon and title \
                             still carry the state.",
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("✔ Acknowledge all")
                                .on_hover_text(
                                    "Clears the header badge for every listed alert. Rows stay \
                                     here for reference; an alert that keeps occurring will \
                                     re-light the badge on its next occurrence.",
                                )
                                .clicked()
                            {
                                s.act = Some(WarningsAct::AckAll);
                            }
                            ui.menu_button("✔ Ack group…", |ui| {
                                if ack_groups.is_empty() {
                                    ui.weak("Nothing unacknowledged.");
                                }
                                if !fixed_ids.is_empty() {
                                    if ui
                                        .button(format!("✅ Fixed ({})", fixed_ids.len()))
                                        .on_hover_text(
                                            "Acknowledge every green row at once — alerts whose \
                                             damage was fully recovered from the VOD, or whose \
                                             failed take was superseded by a later completed \
                                             take. Red (unhealed) and yellow rows are left \
                                             untouched.",
                                        )
                                        .clicked()
                                    {
                                        s.act = Some(WarningsAct::AckGroup(fixed_ids.clone()));
                                        ui.close();
                                    }
                                    ui.separator();
                                }
                                for ((icon, label), ids) in &ack_groups {
                                    if ui
                                        .button(format!("{icon} {label} ({})", ids.len()))
                                        .on_hover_text(format!(
                                            "Acknowledge all {} unacknowledged '{label}' \
                                             alert(s) at once.",
                                            ids.len()
                                        ))
                                        .clicked()
                                    {
                                        s.act = Some(WarningsAct::AckGroup(ids.clone()));
                                        ui.close();
                                    }
                                }
                            })
                            .response
                            .on_hover_text(
                                "Acknowledge one whole category at once — e.g. every 'Disk \
                                 full' alert from one bad night, without touching the rest.",
                            );
                        });
                    });
                    ui.separator();

                    if s.rows.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| {
                            ui.weak("No capture warnings — the tools' logs are clean.")
                        });
                        return;
                    }
                    if visible.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| ui.weak("No alerts match the filter."));
                        return;
                    }

                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for &i in &visible {
                            let r = &s.rows[i];
                            let error = r.severity == "error";
                            // Fully healed: every lost range was re-fetched
                            // from the VOD — the row flips green so recovered
                            // damage doesn't keep reading as an open wound.
                            let healed = r.ranges_total > 0 && r.recovered == r.ranges_total;
                            // Superseded: the take died, but a later take of
                            // the same broadcast completed — the failure
                            // healed itself at the stream level. (Takes with
                            // lost ranges keep normal recovery rendering.)
                            let superseded =
                                !healed && error && r.superseded && r.ranges_total == 0;
                            // Row tint: green when healed, red for errors,
                            // yellow for warnings — dimmed once acknowledged.
                            let (rgb, accent) = if healed || superseded {
                                ((25, 95, 45), egui::Color32::from_rgb(110, 200, 130))
                            } else if error {
                                ((120, 25, 25), egui::Color32::from_rgb(230, 100, 100))
                            } else {
                                ((120, 95, 10), egui::Color32::from_rgb(220, 175, 60))
                            };
                            let alpha = if r.acked { 25 } else { 70 };
                            let tint = if s.bgcolor {
                                egui::Color32::from_rgba_unmultiplied(rgb.0, rgb.1, rgb.2, alpha)
                            } else {
                                egui::Color32::TRANSPARENT
                            };
                            // Channel identity: avatar + the channel's own name
                            // colour (same as the Streams grid / 🔔 feed).
                            let (avatar, name_color) = match s.ident.get(&r.id) {
                                Some((a, (base, adjust))) => (
                                    a.as_ref(),
                                    Some(if *adjust { readable_color(*base, tint) } else { *base }),
                                ),
                                None => (None, None),
                            };
                            // Stable per-row id scope: without it, every
                            // widget id below derives from the row's ORDER,
                            // so a new alert arriving on the 3s refresh
                            // shifted every id and egui dropped any text
                            // selection in progress — copying was impossible
                            // while alerts were coming in.
                            ui.push_id(r.id, |ui| {
                            egui::Frame::group(ui.style()).fill(tint).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (icon, kind_label) = alert_kind_label(&r.kind);
                                    let icon = if healed {
                                        "✅"
                                    } else if superseded {
                                        "🔁"
                                    } else {
                                        icon
                                    };
                                    ui.label(egui::RichText::new(icon).color(accent))
                                        .on_hover_text(if superseded {
                                            "Superseded — this capture attempt died, but a later \
                                             take of the same broadcast completed. New takes \
                                             re-fetch the full stream head (deep rewind / VOD \
                                             backfill), so the completed take should cover this \
                                             one's content. This alert no longer counts toward \
                                             the 🚨 badge."
                                        } else if healed {
                                            "Recovered — every lost range was re-fetched from the \
                                             VOD; the content exists as patch files next to the \
                                             recording. Ranges that only survived as DMCA-muted \
                                             copies are fetched anyway (video intact, audio \
                                             silenced) — a muted patch beats no patch — and are \
                                             marked '-muted' in the filename."
                                        } else if error {
                                            "ERROR — content is missing from this capture."
                                        } else {
                                            "Warning — the tool complained, no data loss detected."
                                        });
                                    if let Some(tex) = avatar {
                                        let resp = ui.add(
                                            egui::Image::from_texture(tex)
                                                .fit_to_exact_size(egui::vec2(24.0, 24.0))
                                                .corner_radius(egui::CornerRadius::same(4)),
                                        );
                                        queue_alt_image_preview(ui.ctx(), &resp, tex);
                                        ui.add_space(2.0);
                                    }
                                    ui.vertical(|ui| {
                                        let mut title = if r.channel.is_empty() {
                                            kind_label.to_string()
                                        } else {
                                            format!("{kind_label} — {}", r.channel)
                                        };
                                        if healed {
                                            title.push_str(if r.recovered_muted > 0 {
                                                " — recovered (partly muted)"
                                            } else {
                                                " — recovered"
                                            });
                                        } else if superseded {
                                            title.push_str(" — superseded by a later take");
                                        }
                                        // Title line with the metadata inline
                                        // at normal size (wrapping on a narrow
                                        // window) — the old three-line layout
                                        // put this in small print at the
                                        // bottom, and the occurrence count on
                                        // its own line whose detail only lived
                                        // in a tooltip.
                                        let span = if r.last_at > r.first_at {
                                            format!(
                                                "{} — {}",
                                                fmt_datetime_short(r.first_at),
                                                fmt_datetime_short(r.last_at)
                                            )
                                        } else {
                                            fmt_datetime_short(r.first_at)
                                        };
                                        let (cicon, clabel) = crate::downloader::alert_category(
                                            &r.kind,
                                            &r.last_line,
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            ui.spacing_mut().item_spacing.x = 6.0;
                                            // Timestamp leads, exactly like the
                                            // 🔔 feed's rows, so the two windows
                                            // align when read side by side.
                                            ui.label(egui::RichText::new(&span).weak())
                                                .on_hover_text("First and most recent occurrence.");
                                            let base = (!r.acked).then_some(accent);
                                            notif_title(ui, &title, &r.channel, name_color, base);
                                            if r.count > 1 {
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "×{}",
                                                        crate::models::group_thousands(r.count)
                                                    ))
                                                    .weak(),
                                                )
                                                .on_hover_text("Occurrences folded into this row.");
                                            }
                                            if !r.source.is_empty() {
                                                ui.label(
                                                    egui::RichText::new(format!("·  {}", r.source))
                                                        .weak(),
                                                )
                                                .on_hover_text(
                                                    "The capture tool whose log reported this.",
                                                );
                                            }
                                            ui.label(
                                                egui::RichText::new(format!("·  {cicon} {clabel}"))
                                                    .weak(),
                                            )
                                            .on_hover_text(
                                                "Alert category — the ✔ Ack group menu \
                                                 acknowledges every alert of one category \
                                                 at once, and the filter box matches \
                                                 category names.",
                                            );
                                        });
                                        // Damage/recovery summary, only when
                                        // there is any (lost segments, VOD
                                        // recovery progress).
                                        if let Some(damage) = alert_damage_summary(r) {
                                            ui.label(egui::RichText::new(damage).weak());
                                        }
                                        // The matched log line, IN the row —
                                        // selectable text (tooltips made it
                                        // uncopyable), right-click to copy.
                                        if !r.last_line.is_empty() {
                                            // Rows persisted before the
                                            // strip-on-ingest fix can still
                                            // carry ANSI colour codes.
                                            let line: std::borrow::Cow<'_, str> =
                                                if r.last_line.contains('\x1b') {
                                                    crate::logfmt::strip_ansi(&r.last_line).into()
                                                } else {
                                                    r.last_line.as_str().into()
                                                };
                                            let resp = ui
                                                .add(
                                                    egui::Label::new(
                                                        egui::RichText::new(line.as_ref()).weak(),
                                                    )
                                                    .truncate()
                                                    .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text(line.as_ref());
                                            resp.context_menu(|ui| {
                                                if ui.button("📋 Copy log line").clicked() {
                                                    ui.ctx().copy_text(line.to_string());
                                                    ui.close();
                                                }
                                                if ui.button("📋 Copy alert details").clicked() {
                                                    ui.ctx().copy_text(format!(
                                                        "{title}\n{span} · {}{cicon} {clabel}\n{}{}",
                                                        if r.source.is_empty() {
                                                            String::new()
                                                        } else {
                                                            format!("{} · ", r.source)
                                                        },
                                                        alert_damage_summary(r)
                                                            .map(|d| format!("{d}\n"))
                                                            .unwrap_or_default(),
                                                        line
                                                    ));
                                                    ui.close();
                                                }
                                            });
                                        }
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if !r.acked
                                                && ui
                                                    .button("✔ Ack")
                                                    .on_hover_text(
                                                        "Acknowledge: clears this alert from the \
                                                         header badge. It stays listed, and new \
                                                         occurrences will re-light it.",
                                                    )
                                                    .clicked()
                                            {
                                                s.act = Some(WarningsAct::Ack(r.id));
                                            }
                                            if ui
                                                .button("📂 Log")
                                                .on_hover_text(
                                                    "Open the capture tool's log file — every \
                                                     matched line, in context.",
                                                )
                                                .clicked()
                                            {
                                                s.act = Some(WarningsAct::OpenLog(r.take_key.clone()));
                                            }
                                            if r.recovered > 0
                                                && let Some(rec_id) = r.recording_id
                                                && ui
                                                    .button("🩹 Patches")
                                                    .on_hover_text(
                                                        "Open the folder with the recovered patch \
                                                         files ({stem}.recovered-….mkv) — the \
                                                         re-fetched content for each lost range.",
                                                    )
                                                    .clicked()
                                            {
                                                s.act = Some(WarningsAct::OpenPatches(rec_id));
                                            }
                                        },
                                    );
                                });
                            });
                            });
                        }
                    });
                });
                // Child viewports draw their own copy of the Alt-hover overlay —
                // the main viewport's draw call can't reach here.
                draw_alt_image_preview(ctx);
            },
        );

        // Filter fields are remembered across a close/reopen (mirrors the
        // pre-migration code, which read them straight off `self` every
        // frame); `bgcolor` also persists to settings on change.
        let (search, sev_filter, hide_acked, bgcolor, closed, act) = {
            let mut s = popup_state.lock().unwrap();
            (
                s.search.clone(),
                s.sev_filter,
                s.hide_acked,
                s.bgcolor,
                s.closed,
                s.act.take(),
            )
        };
        self.warn_search = search;
        self.warn_sev_filter = sev_filter;
        self.warn_hide_acked = hide_acked;
        if bgcolor != self.warn_bgcolor {
            self.warn_bgcolor = bgcolor;
            let _ = self
                .core
                .store
                .set_setting(K_WARN_BGCOLOR, if bgcolor { "1" } else { "0" });
        }
        if closed {
            self.show_warnings = false;
            self.warnings_popup = None;
        }
        match act {
            Some(WarningsAct::Ack(id)) => {
                let _ = self.core.store.ack_capture_alert(id);
                if let Some(r) = self.warnings_rows.iter_mut().find(|r| r.id == id) {
                    r.acked = true;
                }
                if let Some(r) = popup_state.lock().unwrap().rows.iter_mut().find(|r| r.id == id) {
                    r.acked = true;
                }
                self.warn_badge = self.core.store.alert_badge_counts().unwrap_or((0, 0));
            }
            Some(WarningsAct::AckAll) => {
                let _ = self.core.store.ack_all_capture_alerts();
                for r in &mut self.warnings_rows {
                    r.acked = true;
                }
                for r in &mut popup_state.lock().unwrap().rows {
                    r.acked = true;
                }
                self.warn_badge = (0, 0);
            }
            Some(WarningsAct::AckGroup(ids)) => {
                let _ = self.core.store.ack_capture_alerts(&ids);
                for r in &mut self.warnings_rows {
                    if ids.contains(&r.id) {
                        r.acked = true;
                    }
                }
                for r in &mut popup_state.lock().unwrap().rows {
                    if ids.contains(&r.id) {
                        r.acked = true;
                    }
                }
                self.warn_badge = self.core.store.alert_badge_counts().unwrap_or((0, 0));
            }
            Some(WarningsAct::OpenLog(path)) => {
                crate::platform::open_path(std::path::Path::new(&path));
            }
            Some(WarningsAct::OpenPatches(rec_id)) => {
                // The patch files sit next to the recording; take the first
                // done range's out_path and open its folder.
                let dir = self
                    .core
                    .store
                    .gap_ranges_in_state(rec_id, "done")
                    .unwrap_or_default()
                    .iter()
                    .find(|g| !g.out_path.is_empty())
                    .and_then(|g| {
                        std::path::Path::new(&g.out_path).parent().map(std::path::Path::to_path_buf)
                    });
                if let Some(dir) = dir {
                    crate::platform::open_path(&dir);
                }
            }
            None => {}
        }
    }

    /// Issues panel: lists all recordings whose output path is still a `.ts`
    /// file inside a `.cache` directory, and lets the user re-remux them to MKV.
    /// See [`IssuesScan`] for the parts computed off-thread.
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn issues_window(&mut self, ctx: &egui::Context) {
        use std::time::Duration;
        self.issues_drain_scan(ctx);
        self.issues_refresh_scan(ctx);
        if !self.show_issues {
            self.issues_popup = None;
            return;
        }

        if self.issues_popup.is_none() {
            self.issues_popup = Some(Arc::new(Mutex::new(IssuesPopupState {
                issues_recs: Vec::new(),
                issues_missing: Vec::new(),
                issues_errors: Vec::new(),
                issues_errors_no_file: Vec::new(),
                issues_stuck: Vec::new(),
                issues_unmerged: Vec::new(),
                issues_head_mismatch: Vec::new(),
                issues_gap_splice: Vec::new(),
                issues_stale_recording: Vec::new(),
                issues_muted_vod: Vec::new(),
                yt_quota_today: 0,
                yt_quota_cutoff: 0,
                yt_search_today: 0,
                yt_search_cutoff: 0,
                background_tasks: Vec::new(),
                finished_tasks: Vec::new(),
                fs_probes: self.fs_probes.clone(),
                issues_confirm_clear: false,
                filter: String::new(),
                kind_filter: IssueKind::All,
                issues_entries: self.issues_grid.entries.clone(),
                reorder_columns: None,
                issues_error_view: None,
                act: None,
                refresh: false,
                closed: false,
            })));
        }
        let state = self.issues_popup.clone().unwrap();

        // Pick up anything the deferred closure changed since the last call
        // BEFORE overwriting the popup's snapshot below — same "sync back,
        // then refresh" order as `format_designer_window`'s draft, so an
        // in-flight header-click column edit is never clobbered by the
        // refresh that follows.
        {
            let mut s = state.lock().unwrap();
            if s.issues_entries != self.issues_grid.entries {
                self.issues_grid.entries = std::mem::take(&mut s.issues_entries);
                grid_columns::save_columns(&self.core.store, GridTableId::Issues, &self.issues_grid.entries);
            }
            if let Some(rc) = s.reorder_columns.take() {
                self.reorder_columns = Some(rc);
            }
            if s.refresh {
                s.refresh = false;
                self.issues_refreshed = None;
            }
        }
        if !self.show_issues {
            // A "View error" close etc. may have run issues_apply_act's
            // equivalent inline in the closure; nothing else to do here.
        }

        // Refresh the popup's data snapshot every call — these lists are
        // already re-read from `self.issues_*` every frame in the
        // pre-migration code, so cloning them in isn't a new cost, just a
        // relocated one.
        {
            let mut s = state.lock().unwrap();
            s.issues_recs = self.issues_recs.clone();
            s.issues_missing = self.issues_missing.clone();
            s.issues_errors = self.issues_errors.clone();
            s.issues_errors_no_file = self.issues_errors_no_file.clone();
            s.issues_stuck = self.issues_stuck.clone();
            s.issues_unmerged = self.issues_unmerged.clone();
            s.issues_head_mismatch = self.issues_head_mismatch.clone();
            s.issues_gap_splice = self.issues_gap_splice.clone();
            s.issues_stale_recording = self.issues_stale_recording.clone();
            s.issues_muted_vod = self.issues_muted_vod.clone();
            s.yt_quota_today = self.yt_quota_today;
            s.yt_quota_cutoff = self.yt_quota_cutoff;
            s.yt_search_today = self.yt_search_today;
            s.yt_search_cutoff = self.yt_search_cutoff;
            s.background_tasks = self.background_tasks.clone();
            s.finished_tasks = self.finished_tasks.clone();
            s.issues_confirm_clear = self.issues_confirm_clear;
            s.issues_entries = self.issues_grid.entries.clone();
        }

        // Build owned lookup: monitor_id -> (channel_name, platform) — the
        // deferred closure can't borrow self.rows.
        let mon_info: std::collections::HashMap<i64, (String, crate::models::Platform)> = self
            .rows
            .iter()
            .map(|r| {
                (
                    r.monitor.id,
                    (r.channel.name.clone(), r.monitor.platform()),
                )
            })
            .collect();
        let ptex = self.platform_tex.clone();
        let now = crate::models::now_unix();
        let has_active_remux = self
            .background_tasks
            .iter()
            .any(|bt| matches!(bt.kind, crate::events::BackgroundTaskKind::Remux(_)));
        let n_missing = self.issues_missing.len();
        let n_errors = self.issues_errors.len();
        let n_missing_errors = self.issues_errors_no_file.len();
        let n_stuck = self.issues_stuck.len();
        let confirm_clear = self.issues_confirm_clear;
        let quota_warnings = self.active_quota_warnings();
        // Column order/visibility, derived from the (just-synced) persisted
        // entries — shared by all 5 row-shape blocks so they stay aligned
        // with the header and with each other.
        let issues_order =
            grid_columns::effective_order(&ISSUES_COLUMNS, &self.issues_grid.entries, |_| true);
        let issues_reset = self.issues_grid.note_order(&issues_order);

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("issues_vp"),
            egui::ViewportBuilder::default()
                .with_title("⚠ Issues")
                .with_inner_size([1000.0, 420.0]),
            state.clone(),
            shared,
            move |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                if has_active_remux {
                    ctx.request_repaint_after(Duration::from_secs(1));
                }
                // Sizes go through the TTL probe cache — this runs every
                // repaint while the Issues window is open.
                let n_empty = {
                    let mut fs_guard = s.fs_probes.lock().unwrap();
                    s.issues_recs.iter().filter(|r| {
                        fs_guard.len(std::path::Path::new(&r.output_path)) == 0
                    }).count()
                    // `fs_guard` dropped here — the sections below (and
                    // everything they call) take their own `s.fs_probes`
                    // lock; a `std::sync::Mutex` is not reentrant.
                };
                let mut act: Option<Act> = s.act.take();
                // Panels, not one stacked CentralPanel: the sections get a
                // user-draggable share of the window (they used to be pinned
                // to a hardcoded 300px no matter how tall the window was),
                // the toolbar is always reachable, and the table gets every
                // pixel that's left — including any the window gains when
                // resized. Declaration order allocates the space, so the
                // sections and toolbar must be shown BEFORE the CentralPanel.
                let any_sections = !quota_warnings.is_empty()
                    || !s.issues_stale_recording.is_empty()
                    || !s.issues_muted_vod.is_empty()
                    || !s.issues_unmerged.is_empty()
                    || !s.issues_head_mismatch.is_empty()
                    || !s.issues_gap_splice.is_empty();
                if any_sections {
                    egui::Panel::top("issues_sections")
                        .resizable(true)
                        .default_size(260.0)
                        // Floor keeps a dragged-shut panel grabbable again;
                        // ceiling keeps it from swallowing the whole window.
                        .size_range(60.0..=600.0)
                        .show(ctx, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("issues_top_sections")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    s.issues_quota_section(ui, &quota_warnings, &mut act);
                                    s.issues_stale_recording_section(ui, &mut act);
                                    s.issues_muted_vod_section(ui, &mut act);
                                    s.issues_unmerged_section(ui, has_active_remux, &mut act);
                                    s.issues_head_mismatch_section(ui, &mut act);
                                    s.issues_gap_splice_section(ui, &mut act);
                                });
                        });
                }
                let filter_lc = s.filter.to_lowercase();
                let (shown, total) = s.issues_visible_count(&mon_info, &filter_lc);
                egui::Panel::top("issues_toolbar").show(ctx, |ui| {
                    s.issues_toolbar(
                        ui,
                        n_empty,
                        n_missing,
                        n_errors,
                        n_missing_errors,
                        n_stuck,
                        confirm_clear,
                        shown,
                        total,
                        &mut act,
                    );
                });
                egui::CentralPanel::default().show(ctx, |ui| {
                    if total == 0 {
                        if !any_sections {
                            ui.weak("No recording issues found — all recordings are in their final format.");
                        }
                        return;
                    }
                    if shown == 0 {
                        ui.weak(
                            "No rows match the current filter — clear the search box or \
                             pick a different type.",
                        );
                        return;
                    }
                    let mut issues_entries = std::mem::take(&mut s.issues_entries);
                    s.issues_table(
                        ui,
                        &mon_info,
                        &ptex,
                        now,
                        &mut act,
                        &mut issues_entries,
                        &issues_order,
                        issues_reset,
                    );
                    s.issues_entries = issues_entries;
                });
                s.act = act;
                // The 🔍 "View error details" popup — an embedded window
                // inside this SAME viewport (see `IssuesPopupState::issues_error_view`).
                // `Act` isn't `Clone`, so take + put-back-if-not-a-match.
                match s.act.take() {
                    Some(Act::ViewError(title, text)) => s.issues_error_view = Some((title, text)),
                    other => s.act = other,
                }
                if let Some((title, text)) = s.issues_error_view.clone() {
                    let mut open = true;
                    egui::Window::new(if title.is_empty() || title == "—" {
                        "Details".to_string()
                    } else {
                        format!("Details — {title}")
                    })
                    .id(egui::Id::new("issues_error_view"))
                    .open(&mut open)
                    .collapsible(false)
                    .default_size([640.0, 260.0])
                    .show(ctx, |ui| {
                        if ui.button("📋 Copy").clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                        ui.separator();
                        egui::ScrollArea::both()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let mut s = text.as_str();
                                ui.add(
                                    egui::TextEdit::multiline(&mut s)
                                        .font(egui::TextStyle::Monospace)
                                        .desired_width(f32::INFINITY),
                                );
                            });
                    });
                    if !open {
                        s.issues_error_view = None;
                    }
                }
            },
        );

        let (closed, act) = {
            let mut s = state.lock().unwrap();
            (s.closed, s.act.take())
        };
        if closed {
            self.show_issues = false;
            self.issues_popup = None;
        }
        self.issues_apply_act(act);
    }

    /// Drain any completed background missing-file check so the badge count
    /// stays current even when the panel is hidden.
    fn issues_drain_scan(&mut self, ctx: &egui::Context) {
        use std::time::Duration;
        if let Some(rx) = &self.issues_missing_load {
            match rx.try_recv() {
                Ok(scan) => {
                    self.issues_missing = scan.missing;
                    self.issues_errors = scan.errors_with_file;
                    self.issues_errors_no_file = scan.errors_no_file;
                    self.issues_unmerged = scan.unmerged;
                    self.issues_head_mismatch = scan.head_mismatch;
                    self.issues_stale_recording = scan.stale_recording;
                    self.issues_missing_load = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.issues_missing_load = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still in flight — keep repainting so we pick it up promptly.
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
            }
        }
    }

    /// Refresh the Issues lists when stale. DB-only queries (fast, system
    /// drive) run synchronously; everything that stats the recordings drive
    /// runs off-thread (see [`IssuesScan`]).
    fn issues_refresh_scan(&mut self, ctx: &egui::Context) {
        use std::time::{Duration, Instant};
        // Always refresh so the toolbar button count stays current even when the
        // panel is closed — but much less often then: the badge going stale for
        // a few minutes is fine, and each sweep stats every recording on the
        // recordings drive (real head seeks while captures are writing).
        let interval = if self.show_issues {
            Duration::from_secs(5)
        } else if self.issues_dirty {
            // Something changed recently — bring the badge up to date soon,
            // but never sweep-per-event (see pump_messages).
            Duration::from_secs(15)
        } else {
            Duration::from_secs(300)
        };
        let stale = self
            .issues_refreshed
            .map(|t| t.elapsed() >= interval)
            .unwrap_or(true);
        if stale && self.issues_missing_load.is_none() && !super::text_selection_hold(ctx) {
            self.issues_dirty = false;
            // DB-only queries (fast, system drive) stay synchronous.
            self.issues_recs = self.core.store.recordings_needing_remux().unwrap_or_default();
            self.issues_stuck = self.core.store.recordings_stuck_in_cache().unwrap_or_default();
            self.issues_muted_vod = self.core.store.recordings_muted_vod_unresolved().unwrap_or_default();
            self.issues_gap_splice = self.core.store.recordings_with_gap_splice_issue().unwrap_or_default();
            // Everything that stats the recordings drive — the up-to-500-path
            // missing-file sweep AND the error partition — runs off-thread
            // (one exists() there can block the frame for seconds under load).
            let core = self.core.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("issues-missing-check".into())
                .spawn(move || {
                    let candidates = core.store.recordings_with_final_path().unwrap_or_default();
                    let gone: Vec<_> = candidates
                        .into_iter()
                        .filter(|r| !crate::iomon::fs::exists_sync(crate::iomon::Cat::FsProbe, &r.output_path))
                        .collect();
                    // A "gone" take whose media survived as split per-format
                    // parts in `.cache\` (tool died before its own merge) is
                    // NOT lost — list it as recoverable, never as missing.
                    let (unmerged, missing): (Vec<_>, Vec<_>) = gone
                        .into_iter()
                        .map(|r| {
                            let capture = std::path::Path::new(&r.output_path);
                            let mut parts = crate::downloader::find_split_media(capture);
                            if parts.is_empty() {
                                // The tool died mid-write: the media may
                                // survive only as unfinished `.part`
                                // sequences (largest one per format).
                                parts = crate::downloader::find_split_parts(capture);
                            }
                            (r, parts)
                        })
                        .partition(|(_, parts)| !parts.is_empty());
                    let missing: Vec<_> = missing.into_iter().map(|(r, _)| r).collect();
                    // Partition errors: file gone → treated as missing.
                    let all_errors = core.store.recordings_with_errors().unwrap_or_default();
                    let (with_file, no_file): (Vec<_>, Vec<_>) =
                        all_errors.into_iter().partition(|r| {
                            r.output_path.is_empty()
                                || crate::iomon::fs::exists_sync(crate::iomon::Cat::FsProbe, &r.output_path)
                        });
                    // A failed take whose media survives as split parts is
                    // recoverable — list it ONLY under unmerged (with the
                    // merge action), not as a plain dead "file missing" row.
                    let unmerged_ids: std::collections::HashSet<i64> =
                        unmerged.iter().map(|(r, _)| r.id).collect();
                    let no_file: Vec<_> = no_file
                        .into_iter()
                        .filter(|r| !unmerged_ids.contains(&r.id))
                        .collect();
                    // Rows still marked 'recording' whose files have gone
                    // quiet: capture died unnoticed. Takes whose finalize is
                    // already in flight (remux queued at the disk gate — the
                    // Streams grid shows them "finalizing") are excluded:
                    // offering "Finalize now" there would double-promote.
                    let now = crate::models::now_unix();
                    let finalizing_recs: std::collections::HashSet<i64> =
                        core.finalizing.lock().unwrap().values().copied().collect();
                    let stale_recording: Vec<_> = core
                        .store
                        .recordings_marked_recording()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|r| !finalizing_recs.contains(&r.id))
                        .filter_map(|r| {
                            // A capture in its first minutes may not have
                            // files yet — never list those.
                            if now - r.started_at < STALE_RECORDING_SECS {
                                return None;
                            }
                            if r.output_path.is_empty() {
                                return Some((r, None));
                            }
                            let age = crate::downloader::latest_capture_activity(&r.output_path)
                                .map(|t| (now - t).max(0));
                            match age {
                                Some(a) if a < STALE_RECORDING_SECS => None,
                                other => Some((r, other)),
                            }
                        })
                        .collect();
                    // Head/live joins blocked by codec parameters, with the
                    // actual stream params probed for the explainer.
                    let head_mismatch: Vec<_> = core
                        .store
                        .recordings_with_head_mismatch()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|r| {
                            let head = r
                                .backfill_path
                                .as_deref()
                                .map(probe_dims_sync)
                                .unwrap_or_default();
                            let live = probe_dims_sync(&r.output_path);
                            (r, head, live)
                        })
                        .collect();
                    let _ = tx.send(IssuesScan {
                        missing,
                        errors_with_file: with_file,
                        errors_no_file: no_file,
                        unmerged,
                        head_mismatch,
                        stale_recording,
                    });
                })
                .ok();
            self.issues_missing_load = Some(rx);
            self.issues_refreshed = Some(Instant::now());
        }
    }


    /// Apply the single action collected during this frame's render, after
    /// the viewport closure has released its borrows of `self`.
    fn issues_apply_act(&mut self, act: Option<Act>) {
        if let Some(Act::Remux(i)) = act {
            if let Some(rec) = self.issues_recs.get(i) {
                // The promoted location = the capture path minus its cache
                // component (handles per-dir AND central-root layouts).
                let dest = crate::downloader::strip_cache_component(std::path::Path::new(
                    &rec.output_path,
                ))
                .map(|p| p.with_extension("mkv"));
                if let Some(dest) = dest {
                    self.core.manual(crate::events::ManualCommand::ReRemux {
                        rec_id: rec.id,
                        capture: std::path::PathBuf::from(&rec.output_path),
                        final_: dest,
                    });
                    self.status = format!("Re-remux started for recording {}…", rec.id);
                }
            }
        }
        if let Some(Act::RecoverStuck(k)) = act
            && let Some(rec) = self.issues_stuck.get(k)
        {
            let capture = std::path::PathBuf::from(&rec.output_path);
            // The promoted location = the capture path minus its cache
            // component (handles per-dir AND central-root layouts); its parent
            // is the output dir the file should move to.
            let output_dir = crate::downloader::strip_cache_component(&capture)
                .and_then(|p| p.parent().map(Path::to_path_buf));
            if let Some(output_dir) = output_dir {
                self.core.manual(crate::events::ManualCommand::RecoverStuckCapture {
                    rec_id: rec.id,
                    capture,
                    output_dir,
                });
                self.status = format!("Recovering recording {}…", rec.id);
            }
        }
        if let Some(Act::Delete(i)) = act {
            if let Some(rec) = self.issues_recs.get(i).cloned() {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.clear_recording_capture(rec.id);
                self.issues_recs.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearEmpties) = act {
            let empties: Vec<_> = self.issues_recs.iter().filter(|r| {
                crate::iomon::fs::metadata_sync(crate::iomon::Cat::RecordingDelete, &r.output_path).map(|m| m.len()).unwrap_or(0) == 0
            }).cloned().collect();
            for rec in empties {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.clear_recording_capture(rec.id);
                self.issues_recs.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearPath(j)) = act {
            if let Some(rec) = self.issues_missing.get(j).cloned() {
                let _ = self.core.store.clear_recording_capture(rec.id);
                self.issues_missing.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearAllMissing) = act {
            let all: Vec<_> = self.issues_missing.drain(..).collect();
            for rec in all {
                let _ = self.core.store.clear_recording_capture(rec.id);
            }
        }
        if let Some(Act::ConfirmClear) = act {
            self.issues_confirm_clear = !self.issues_confirm_clear;
        }
        if let Some(Act::ClearAll) = act {
            let all: Vec<_> = self.issues_recs.drain(..).collect();
            for rec in all {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.clear_recording_capture(rec.id);
            }
            self.issues_confirm_clear = false;
        }
        if let Some(Act::DismissWarning(ref key)) = act {
            self.dismissed_quota_warnings.insert(key.clone());
        }
        if let Some(Act::OpenMutedLive(i)) = act
            && let Some(p) = self
                .issues_muted_vod
                .get(i)
                .map(|m| m.output_path.clone())
                .filter(|p| !p.is_empty())
        {
            open_path(std::path::Path::new(&p));
        }
        if let Some(Act::OpenMutedRecovered(i)) = act
            && let Some(rp) = self
                .issues_muted_vod
                .get(i)
                .and_then(|m| m.recovered_path.clone())
                .filter(|p| !p.is_empty())
        {
            open_path(std::path::Path::new(&rp));
        }
        if let Some(Act::RerunMuted(i)) = act
            && let Some(rec_id) = self.issues_muted_vod.get(i).map(|m| m.rec_id)
        {
            self.open_recover_vod_from_seed(rec_id);
        }
        if let Some(Act::DismissMuted(i)) = act
            && let Some(rec_id) = self.issues_muted_vod.get(i).map(|m| m.rec_id)
        {
            let _ = self.core.store.recording_vod_dl_acknowledge(rec_id);
            self.issues_refreshed = None; // force the list to refresh
        }
        if let Some(Act::MergeSplit(i)) = act
            && let Some((rec, _)) = self.issues_unmerged.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::MergeSplitCapture(rec.id));
            self.status = format!("Merging split capture for recording {}…", rec.id);
        }
        if let Some(Act::DownloadVodUnmerged(i)) = act
            && let Some((rec, _)) = self.issues_unmerged.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::ArchiveVodNow(rec.id));
            self.status = format!("Downloading the published VOD for recording {}…", rec.id);
        }
        if let Some(Act::FinalizeStale(i)) = act
            && let Some((rec, _)) = self.issues_stale_recording.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::FinalizeRecording(rec.id));
            self.status = format!("Finalizing recording {}…", rec.id);
            self.issues_refreshed = None;
        }
        if let Some(Act::RefetchHeadMatchLive(i)) = act
            && let Some((rec, _, _)) = self.issues_head_mismatch.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::BackfillHeadMatchLive(rec.id));
            self.status = format!("Re-fetching head at the live quality for recording {}…", rec.id);
            self.issues_refreshed = None;
        }
        if let Some(Act::FetchVodForMismatch(i)) = act
            && let Some((rec, _, _)) = self.issues_head_mismatch.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::ArchiveVodNow(rec.id));
            self.status = format!("Downloading the published VOD for recording {}…", rec.id);
        }
        if let Some(Act::DismissMismatch(i)) = act
            && let Some((rec, _, _)) = self.issues_head_mismatch.get(i)
        {
            // "mismatch_ack": still skips join re-attempts (any "mismatch*"
            // state does) but no longer lists in Issues.
            let _ = self.core.store.set_head_backfill_state(rec.id, "mismatch_ack");
            self.issues_refreshed = None;
        }
        if let Some(Act::DismissGapSplice(i)) = act
            && let Some(rec) = self.issues_gap_splice.get(i)
        {
            // Non-empty forever after — the precondition check requires
            // `gap_splice_state == ""`, so this permanently skips
            // re-attempts for this recording (the patch(es) stay as
            // sibling files, unaffected).
            let _ = self.core.store.set_gap_splice_state(rec.id, "dismissed");
            self.issues_refreshed = None;
        }
        if let Some(Act::OpenGapSplicePatchFolder(i)) = act
            && let Some(rec) = self.issues_gap_splice.get(i)
            && let Some(dir) = std::path::Path::new(&rec.output_path).parent()
        {
            crate::platform::open_path(dir);
        }
        if let Some(Act::RemuxError(k)) = act {
            if let Some(rec) = self.issues_errors.get(k) {
                let dest = std::path::Path::new(&rec.output_path)
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|d| {
                        std::path::Path::new(&rec.output_path)
                            .file_stem()
                            .map(|s| d.join(format!("{}.mkv", s.to_string_lossy())))
                    });
                if let Some(dest) = dest {
                    self.core.manual(crate::events::ManualCommand::ReRemux {
                        rec_id: rec.id,
                        capture: std::path::PathBuf::from(&rec.output_path),
                        final_: dest,
                    });
                    self.status = format!("Re-remux started for recording {}…", rec.id);
                }
            }
        }
        if let Some(Act::DeleteError(k)) = act {
            if let Some(rec) = self.issues_errors.get(k).cloned() {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.clear_recording_capture(rec.id);
                self.issues_errors.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearError(k)) = act {
            if let Some(rec) = self.issues_errors.get(k).cloned() {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.delete_recording(rec.id);
                self.issues_errors.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::AckError(k)) = act
            && let Some(rec) = self.issues_errors.get(k).cloned()
        {
            let _ = self.core.store.set_recording_err_ack(rec.id, true);
            self.issues_errors.retain(|r| r.id != rec.id);
        }
        if let Some(Act::AckMissingError(j2)) = act
            && let Some(rec) = self.issues_errors_no_file.get(j2).cloned()
        {
            let _ = self.core.store.set_recording_err_ack(rec.id, true);
            self.issues_errors_no_file.retain(|r| r.id != rec.id);
        }
        if let Some(Act::ClearAllErrors) = act {
            let all: Vec<_> = self.issues_errors.drain(..).collect();
            for rec in all {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.delete_recording(rec.id);
            }
        }
        if let Some(Act::ClearFilelessErrors) = act {
            // issues_errors_no_file holds all failed recordings where the file is gone.
            let all: Vec<_> = self.issues_errors_no_file.drain(..).collect();
            for rec in all {
                let _ = self.core.store.delete_recording(rec.id);
            }
        }
        if let Some(Act::ClearMissingError(j2)) = act {
            if let Some(rec) = self.issues_errors_no_file.get(j2).cloned() {
                let _ = self.core.store.delete_recording(rec.id);
                self.issues_errors_no_file.retain(|r| r.id != rec.id);
            }
        }
        // Act::ViewError is handled inline inside issues_window's deferred
        // closure (writes IssuesPopupState::issues_error_view directly) —
        // it never reaches here.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The toolbar filter matches on the two columns that identify a row, and
    /// on neither case nor alphabet: the archive is full of Japanese titles
    /// and mixed-case channel names, and a filter that only folded ASCII would
    /// silently drop the rows a user is most likely to be hunting for.
    #[test]
    fn table_filter_matches_channel_and_path_case_insensitively() {
        let path = r"G:\rec\Takanashi Kiara - 2026-08-04 - 【WATCHALONG】FIRE EMBLEM.ts";
        // Empty filter shows everything — the default state.
        assert!(issue_filter_hit("", "Zentreya", path));
        // Channel, lowercased by the caller (never by the row).
        assert!(issue_filter_hit("kiara", "Takanashi Kiara", path));
        // Filename fragments, including non-ASCII.
        assert!(issue_filter_hit("fire emblem", "Takanashi Kiara", path));
        assert!(issue_filter_hit("watchalong", "Takanashi Kiara", path));
        // A miss really misses — no substring anywhere.
        assert!(!issue_filter_hit("zentreya", "Takanashi Kiara", path));
    }

    /// `IssueKind::All` must show every shape; every other value must show
    /// exactly its own. A stray match arm here would silently hide a whole
    /// category of broken takes.
    #[test]
    fn kind_filter_all_shows_every_shape_and_others_show_one() {
        let shown = |sel: IssueKind| {
            IssueKind::ALL
                .iter()
                .filter(|k| **k != IssueKind::All)
                .filter(|k| sel == IssueKind::All || sel == **k)
                .count()
        };
        assert_eq!(shown(IssueKind::All), 5);
        for k in IssueKind::ALL.iter().filter(|k| **k != IssueKind::All) {
            assert_eq!(shown(*k), 1, "{} selects only itself", k.label());
        }
    }

    #[test]
    fn network_hint_matches_dns_failures() {
        // The exact shape of the Maid Mint / Anya failed-resume logs (2026-07-12).
        assert!(network_failure_hint(
            "WARNING: [youtube:tab] HTTPSConnection(host='www.youtube.com', port=443): \
             Failed to resolve 'www.youtube.com' ([Errno 11001] getaddrinfo failed). \
             Retrying (1/3)..."
        )
        .is_some());
        assert!(network_failure_hint("Temporary failure in name resolution").is_some());
        // Real tool errors must not be blamed on the network.
        assert!(network_failure_hint("ERROR: This live event has ended.").is_none());
        assert!(network_failure_hint("").is_none());
    }

    /// Rows written before the wording changed keep the old label in the DB —
    /// the feed must show the new one without a migration.
    #[test]
    fn notif_action_labels_remap_legacy_rows() {
        assert_eq!(notif_action_label("Watch stream"), "Watch on Web");
        assert_eq!(notif_action_label("Open post"), "View on YouTube");
        // Current labels and unrelated ones pass through untouched.
        assert_eq!(notif_action_label("Watch on Web"), "Watch on Web");
        assert_eq!(notif_action_label("Watch VOD"), "Watch VOD");
        assert_eq!(notif_action_label(""), "Open");
    }

    #[test]
    fn notif_post_id_prefers_ref_key_then_url() {
        let row = |ref_key: &str, action_url: &str| crate::store::NotificationRow {
            id: 1,
            created_at: 0,
            kind: "youtube_post".into(),
            severity: "info".into(),
            title: String::new(),
            body: String::new(),
            monitor_id: Some(7),
            channel: String::new(),
            recording_id: None,
            action_label: String::new(),
            action_url: action_url.into(),
            image_path: String::new(),
            ref_key: ref_key.into(),
            read: false,
        };
        assert_eq!(
            notif_post_id(&row("post:7:UgkxABC", "")).as_deref(),
            Some("UgkxABC")
        );
        // Post ids can themselves contain ':' — only the first two fields are ours.
        assert_eq!(
            notif_post_id(&row("post:7:Ugkx:ABC", "")).as_deref(),
            Some("Ugkx:ABC")
        );
        assert_eq!(
            notif_post_id(&row("", "https://www.youtube.com/post/UgkxDEF")).as_deref(),
            Some("UgkxDEF")
        );
        // A non-post URL must not be mined for an id.
        assert_eq!(notif_post_id(&row("", "https://twitch.tv/geega")), None);
        assert_eq!(notif_post_id(&row("", "")), None);
    }

    /// Severity outranks kind: an errored recording is red, not
    /// recording-finished blue.
    #[test]
    fn notif_colors_put_severity_first() {
        use crate::models::NotificationKind as K;
        let err = notif_colors(Some(K::RecordingFinished), "error");
        assert_eq!(err, notif_colors(Some(K::Error), "error"));
        assert_ne!(err, notif_colors(Some(K::RecordingFinished), "info"));
        // Unknown/stale kind ids still get a neutral tint rather than panicking.
        assert_eq!(notif_colors(None, "info").1, egui::Color32::from_rgb(170, 170, 170));
    }

    #[test]
    fn only_live_channel_kinds_offer_the_player_button() {
        use crate::models::NotificationKind as K;
        assert!(notif_is_live_stream(Some(K::WentLive)));
        assert!(notif_is_live_stream(Some(K::TriggerMatched)));
        // A finished recording's action URL is a VOD page, not a live stream.
        assert!(!notif_is_live_stream(Some(K::RecordingFinished)));
        assert!(!notif_is_live_stream(Some(K::YoutubePost)));
        assert!(!notif_is_live_stream(None));
    }
}
