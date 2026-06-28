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
}

impl BackgroundTaskKind {
    pub fn label(&self) -> &'static str {
        match self {
            BackgroundTaskKind::AssetFetch => "Asset fetch",
            BackgroundTaskKind::ThumbnailFetch => "Thumbnail",
            BackgroundTaskKind::OcrCall => "Schedule OCR",
        }
    }
}

#[derive(Clone, Debug)]
pub enum TaskOutcome {
    Completed,
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
}

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualCommand {
    /// Check the channel now and, if live, start recording (bypassing backoff).
    Start(i64),
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
}
