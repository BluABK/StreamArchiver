//! The internal event bus (core -> UI) and tray -> UI command channel.
//!
//! The core emits [`AppEvent`]s on a `tokio::broadcast` channel; the UI
//! subscribes so it never has to hot-poll the core. The tray sends
//! [`UiCommand`]s on a plain mpsc channel and wakes the egui loop via
//! `Context::request_repaint`.

use tokio::sync::broadcast;

/// State-change notifications emitted by the core for the UI to render.
#[derive(Clone, Debug)]
pub enum AppEvent {
    /// A monitor's live/recording state changed (e.g. "idle" -> "live").
    MonitorState { monitor_id: i64, state: String },
    RecordingStarted {
        monitor_id: i64,
        recording_id: i64,
        channel: String,
    },
    Progress { recording_id: i64, bytes: u64 },
    RecordingFinished {
        recording_id: i64,
        channel: String,
        status: String,
    },
    Error { context: String, message: String },
}

pub type EventTx = broadcast::Sender<AppEvent>;
pub type EventRx = broadcast::Receiver<AppEvent>;

/// Create the broadcast bus with a bounded backlog.
pub fn bus() -> (EventTx, EventRx) {
    broadcast::channel(256)
}

/// Commands delivered from the tray thread to the UI/app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiCommand {
    /// Show and focus the main window.
    ShowWindow,
    /// Begin application shutdown.
    Quit,
}
