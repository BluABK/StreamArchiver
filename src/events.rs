//! The internal event bus (core -> UI) and tray -> UI command channel.
//!
//! The core emits [`AppEvent`]s on a `tokio::broadcast` channel; the UI
//! subscribes so it never has to hot-poll the core. The tray sends
//! [`UiCommand`]s on a plain mpsc channel and wakes the egui loop via
//! `Context::request_repaint`.

use tokio::sync::broadcast;

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
}

/// Commands delivered from the tray thread to the UI/app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiCommand {
    /// Show and focus the main window.
    ShowWindow,
    /// Begin application shutdown.
    Quit,
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
}
