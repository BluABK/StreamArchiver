//! The internal event bus (core -> UI) and tray -> UI command channel.
//!
//! The core emits [`AppEvent`]s on a `tokio::broadcast` channel; the UI
//! subscribes so it never has to hot-poll the core. The tray sends
//! [`UiCommand`]s on a plain mpsc channel and wakes the egui loop via
//! `Context::request_repaint`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::models::now_unix;

// ---------- Background task tracking ----------

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_task_id() -> u64 {
    NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackgroundTaskKind {
    AssetFetch,
    ThumbnailFetch,
    OcrCall,
    Remux,
    ReRemuxAll,
    EmbedMissingThumbnails,
    FetchMissingThumbnails,
    ReorganizeAll,
    ReorganizeTake(i64),
    ReorganizeMonitor(i64),
    ReorganizeChannel(i64),
    /// A single Twitch VOD recovery (CDN probe + segment salvage + mux).
    /// Carries the recording id when recovering into an existing take
    /// (`None` for a standalone recovery not tied to a recording).
    RecoverVod(Option<i64>),
    /// A bulk sweep recovering all eligible deleted/muted recordings.
    RecoverVodScan,
    /// Harvesting current CDN hosts from published VODs via GQL.
    RefreshCdnHosts,
    /// Backfilling a late-joined capture's missed beginning from the growing
    /// live VOD playlist (and the post-stream head+live concat). Carries the
    /// recording id so the Streams grid can match a specific take's "⏳
    /// backfilling…" indicator to this task (mirrors `ReorganizeTake`/
    /// `RecoverVod`).
    HeadBackfill(i64),
}

impl BackgroundTaskKind {
    pub fn label(&self) -> &'static str {
        match self {
            BackgroundTaskKind::AssetFetch => "Asset fetch",
            BackgroundTaskKind::ThumbnailFetch => "Thumbnail",
            BackgroundTaskKind::OcrCall => "Schedule OCR",
            BackgroundTaskKind::Remux => "Re-remux",
            BackgroundTaskKind::ReRemuxAll => "Re-remux all",
            BackgroundTaskKind::EmbedMissingThumbnails => "Embed thumbnails",
            BackgroundTaskKind::FetchMissingThumbnails => "Fetch thumbnails",
            BackgroundTaskKind::ReorganizeAll => "Re-organize all",
            BackgroundTaskKind::ReorganizeTake(_) => "Re-organize take",
            BackgroundTaskKind::ReorganizeMonitor(_) => "Re-organize monitor",
            BackgroundTaskKind::ReorganizeChannel(_) => "Re-organize channel",
            BackgroundTaskKind::RecoverVod(_) => "VOD recovery",
            BackgroundTaskKind::RecoverVodScan => "VOD recovery scan",
            BackgroundTaskKind::RefreshCdnHosts => "Refresh CDN hosts",
            BackgroundTaskKind::HeadBackfill(_) => "Head backfill",
        }
    }
}

#[derive(Clone, Debug)]
pub enum TaskOutcome {
    Completed,
    /// Completed successfully, with a short human-readable note (e.g. "5 events decoded").
    CompletedWithNote(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct BackgroundTask {
    pub id: u64,
    pub kind: BackgroundTaskKind,
    /// Channel name or recording label shown in the Background view.
    pub label: String,
    /// Extra context for the Background view (e.g. "Twitch · icon, badges, emotes").
    pub detail: String,
    /// Unix timestamp when the task was started.
    pub started_at: i64,
    /// Fractional progress 0.0–1.0, if the task reports it (e.g. remux via ffmpeg).
    /// `None` when duration is unknown (progress block still fires with `info`).
    pub progress: Option<f32>,
    /// Latest per-step detail from the task (e.g. "fps=60 speed=2.5x pos=00:03:27").
    pub progress_info: Option<String>,
}

// ---------- Periodic background jobs ("scheduled") ----------

/// A recurring background job (poll scheduler, schedule/ad-free refreshers, WebSub
/// poll) with its next estimated run time, for the Background view's "Scheduled"
/// section. Kept in a shared [`JobRegistry`] the loops update before each sleep.
#[derive(Clone, Debug)]
pub struct ScheduledJob {
    pub name: String,
    pub interval_secs: i64,
    pub next_run_at: i64,
}

pub type JobRegistry = Arc<Mutex<Vec<ScheduledJob>>>;

/// Periodic jobs that can be enabled/disabled from the Background view, as
/// `(display name, settings key)`. The display name matches the [`mark_job`] name
/// so the UI can join a toggle with its next-run estimate. Default: enabled
/// (`get_setting(key) != "0"`); each loop skips its work when disabled.
pub const TOGGLEABLE_JOBS: &[(&str, &str)] = &[
    ("Live poll", "job_live_poll"),
    ("Schedule refresh", "job_schedule_refresh"),
    ("Ad-free / sub refresh", "job_ad_free_refresh"),
    ("YouTube WebSub poll", "job_websub_poll"),
    ("Channel asset refresh", "job_asset_refresh"),
    ("YouTube posts refresh", "job_community_posts"),
    ("Scheduled recordings", "job_scheduled_recordings"),
];

pub fn job_registry() -> JobRegistry {
    Arc::new(Mutex::new(Vec::new()))
}

/// Upsert a job's next-run estimate (`now + interval_secs`). Called by each
/// periodic loop right before it sleeps.
pub fn mark_job(reg: &JobRegistry, name: &str, interval_secs: i64) {
    let next_run_at = now_unix() + interval_secs.max(0);
    let mut jobs = reg.lock().unwrap();
    if let Some(j) = jobs.iter_mut().find(|j| j.name == name) {
        j.interval_secs = interval_secs;
        j.next_run_at = next_run_at;
    } else {
        jobs.push(ScheduledJob {
            name: name.to_string(),
            interval_secs,
            next_run_at,
        });
    }
}

// ---------- App event bus ----------

/// State-change notifications emitted by the core for the UI to render.
///
/// The UI reloads its state wholesale on any event (it doesn't diff), and
/// `notifications` only reads `channel`/`status`. The remaining payload fields
/// (ids, state) are part of the event contract and Debug output rather than
/// consumed today — hence the allow.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum AppEvent {
    /// A monitor's live/recording state changed (e.g. "idle" -> "live").
    MonitorState {
        monitor_id: i64,
        state: String,
    },
    /// A trigger-word rule matched a live stream's title/game and a recording
    /// is starting because of it (or, with Auto on, its per-rule overrides
    /// were applied to the normal start).
    TriggerMatched {
        monitor_id: i64,
        /// Human description of the match, e.g. `title ~ "karaoke"`.
        desc: String,
        /// The full field value that matched (the title/game text).
        matched: String,
        /// Go-live time — part of the notification dedupe key (one per stream).
        went_live_at: i64,
        /// True when Auto-record was off and only the trigger started this.
        forced_start: bool,
    },
    RecordingStarted {
        monitor_id: i64,
        recording_id: i64,
        channel: String,
        /// Expected path of the stream thumbnail (`{capture_path}.thumbnail.jpg`).
        /// The file is fetched concurrently after the event fires, so it may not
        /// exist yet when the notification handler runs; check with `Path::exists`.
        thumbnail_path: Option<std::path::PathBuf>,
    },
    RecordingFinished {
        recording_id: i64,
        channel: String,
        status: String,
    },
    /// A background task updated VOD state or other derived fields on a recording
    /// row — the UI should reload recording history to reflect the change.
    RecordingUpdated {
        recording_id: i64,
    },
    /// A recording's published Twitch VOD came back DMCA-muted: the post-stream
    /// archive feature ran recovery instead of a plain download and never replaces
    /// the live capture. Surfaced as a toast + an Issues-panel entry.
    VodMuted {
        recording_id: i64,
        channel: String,
        muted_secs: i64,
    },
    Error {
        context: String,
        message: String,
    },
    /// A background task (asset fetch, thumbnail download, etc.) has started.
    BackgroundTaskStarted(BackgroundTask),
    /// A background task has finished.
    BackgroundTaskFinished {
        id: u64,
        outcome: TaskOutcome,
    },
    /// A background task reported incremental progress.
    /// `progress` is `None` when total duration is unknown; `info` is always present.
    BackgroundTaskProgress {
        id: u64,
        /// Fractional 0.0–1.0, or `None` if duration is unknown.
        progress: Option<f32>,
        /// Human-readable step detail, e.g. "fps=60 speed=2.5x pos=00:03:27".
        info: String,
    },
}

pub type EventTx = broadcast::Sender<AppEvent>;
pub type EventRx = broadcast::Receiver<AppEvent>;

/// Create the broadcast bus with a bounded backlog.
pub fn bus() -> (EventTx, EventRx) {
    broadcast::channel(256)
}

/// A "monitor is live" signal from a detector to the download supervisor.
#[derive(Clone, Debug)]
pub struct LiveSignal {
    pub monitor_id: i64,
    /// Platform-reported go-live time (unix seconds), if known.
    pub went_live_at: Option<i64>,
    /// True when `went_live_at` is our first-detected time, not platform-reported.
    pub approximate: bool,
    /// Platform stream/video id, if known (groups recording takes of one stream).
    pub stream_id: Option<String>,
    /// Live stream thumbnail URL, if the platform provided it.
    pub thumbnail_url: Option<String>,
    /// Platform user/channel identifier (Twitch user_id, YouTube UC… ID, Kick slug).
    pub broadcaster_id: Option<String>,
    /// Stream title at go-live time, if the platform provided it at detection.
    pub stream_title: Option<String>,
    /// Game/category at go-live time, if the platform provided it at detection.
    pub stream_game: Option<String>,
}

impl LiveSignal {
    pub fn new(monitor_id: i64, went_live_at: Option<i64>, approximate: bool) -> LiveSignal {
        LiveSignal {
            monitor_id,
            went_live_at,
            approximate,
            stream_id: None,
            thumbnail_url: None,
            broadcaster_id: None,
            stream_title: None,
            stream_game: None,
        }
    }

    /// Attach a platform stream id (builder-style).
    pub fn with_stream_id(mut self, stream_id: Option<String>) -> LiveSignal {
        self.stream_id = stream_id;
        self
    }

    pub fn with_thumbnail_url(mut self, thumbnail_url: Option<String>) -> LiveSignal {
        self.thumbnail_url = thumbnail_url;
        self
    }

    pub fn with_broadcaster_id(mut self, broadcaster_id: Option<String>) -> LiveSignal {
        self.broadcaster_id = broadcaster_id;
        self
    }

    pub fn with_stream_title(mut self, stream_title: Option<String>) -> LiveSignal {
        self.stream_title = stream_title;
        self
    }

    pub fn with_stream_game(mut self, stream_game: Option<String>) -> LiveSignal {
        self.stream_game = stream_game;
        self
    }
}

/// A "monitor went offline" push (currently: EventSub `stream.offline`). Payload
/// is just the monitor id — the consumer re-derives everything else from the
/// store, and re-checks whether a recording currently owns the monitor before
/// writing anything (a push racing an in-progress capture must never clobber
/// "recording").
pub type OfflineSignal = i64;

/// Commands delivered from the tray thread to the UI/app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiCommand {
    /// Show and focus the main window.
    ShowWindow,
    /// Begin application shutdown, leaving downloads running (the default).
    Quit,
    /// Begin shutdown and stop all active downloads (don't detach them).
    QuitAndStop,
}

/// On-demand recording commands from the UI to the download supervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManualCommand {
    /// Check the channel now and, if live, start recording.
    /// `user_initiated`: true for explicit UI button clicks — the start
    /// bypasses backoff AND the Auto gate (a user can always record an
    /// Auto-off instance), and a "not live" outcome toasts an error. False
    /// for automatic triggers (WebSub events) — those honor the Auto gate
    /// and discard a "not live" outcome silently.
    Start { id: i64, user_initiated: bool },
    /// Abort the channel's active recording.
    Stop(i64),
    /// Begin downloading a queued on-demand video (by `video` id).
    StartVideo(i64),
    /// Abort an in-flight on-demand video download (by `video` id).
    StopVideo(i64),
    /// Abort the live-chat sidecar download for a monitor (by monitor id).
    /// The chat download runs independently of the video recording so it needs
    /// its own stop command.
    StopChat(i64),
    /// Force a channel-asset refetch for a monitor (by monitor id), ignoring the
    /// 24h freshness stamp and the per-monitor `fetch_chat_assets` toggle.
    RefetchAssets(i64),
    /// Re-remux a captured `.ts` file to MKV in the background (for recordings
    /// where the automatic remux failed at finalization time). On success the
    /// source TS is deleted and `recording.output_path` is updated to the MKV.
    ReRemux {
        rec_id: i64,
        capture: std::path::PathBuf,
        final_: std::path::PathBuf,
    },
    /// Re-remux all recordings that still have a `.ts` source file.
    ReRemuxAll,
    /// Move a recording whose promote-to-output-dir step failed (a non-`.ts`
    /// file, e.g. a SABR/DASH `.mkv`, still sitting in `.cache\`) out to its
    /// output directory, shortening the name if that's what blocked it the
    /// first time (see `downloader::rename_or_shorten`). On success
    /// `recording.output_path` is updated to the new location.
    RecoverStuckCapture {
        rec_id: i64,
        capture: std::path::PathBuf,
        output_dir: std::path::PathBuf,
    },
    /// Embed the thumbnail sidecar into all MKV files that don't already have one.
    EmbedMissingThumbnails,
    /// Download missing thumbnails for recordings; if `embed` is true, immediately
    /// embed them into the MKV after download.
    FetchMissingThumbnails { embed: bool },
    /// Reorganize all recordings according to the current [`crate::models::SubdirConfig`].
    ReorganizeAll,
    /// Reorganize all recordings belonging to a specific take group (by recording id).
    ReorganizeTake(i64),
    /// Reorganize all recordings belonging to a monitor.
    ReorganizeMonitor(i64),
    /// Reorganize all recordings belonging to a channel.
    ReorganizeChannel(i64),
    /// Rename a recording's output file using the given new filename stem.
    RenameRecording { rec_id: i64, new_stem: String },
    /// Recover a Twitch VOD from surviving CDN segments and file the muxed MKV per
    /// `sink` (attach to a recording, or add to the Videos list). `probe_all` HEAD-
    /// validates every segment (deleted VOD) vs only the muted ones (muted VOD).
    RecoverVod {
        inputs: crate::recovery::RecoveryInputs,
        quality: String,
        sink: crate::recovery::RecoverySink,
        probe_all: bool,
    },
    /// Sweep all deleted/muted recordings inside `window_days` of the CDN window
    /// and recover each (bounded outer concurrency).
    ScanRecoverableVods { window_days: i64, quality: String },
    /// Harvest current Twitch CDN hosts from published VODs (via GQL) and persist
    /// any newly-seen ones, keeping the recovery host list current.
    RefreshCdnHosts,
    /// Download the published VOD for a recording now (manual trigger / retry of the
    /// post-stream archive feature), by recording id.
    ArchiveVodNow(i64),
    /// Manually (re)trigger a head backfill for a recording now, by recording id
    /// (Twitch capture-from-start only). User-initiated: forced regardless of the
    /// "fetch new head backfill on new take" setting — unlike the automatic path,
    /// this always attempts the fetch for a non-first take too. Disabled in the UI
    /// unless the owning channel is currently live (the CDN's growing live
    /// playlist this depends on stops being pre-mute-safe once the stream ends).
    BackfillHeadNow(i64),
    /// Re-fetch a mismatched head backfill at the LIVE capture's own rendition
    /// (the Issues fix for `head_backfill_state == "mismatch"`): the live take
    /// joined before Twitch listed the source rendition, so the source-quality
    /// head can't be losslessly concatenated with it.
    BackfillHeadMatchLive(i64),
    /// Merge a stranded split capture (bare per-format `.fN.*` files left in
    /// `.cache\` when the tool died before its own merge) into the final MKV
    /// and promote it — the Issues fix for a take that finalized 0-byte while
    /// its media survived as parts.
    MergeSplitCapture(i64),
}
