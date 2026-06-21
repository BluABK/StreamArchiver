//! Desktop notifications for recording lifecycle events.
//!
//! Subscribes to the event bus and shows a toast when a recording starts or
//! finishes. On Windows, correct branding requires an AppUserModelID + Start
//! Menu shortcut (installed by the packaging step); without one, Windows
//! attributes the toast to PowerShell.

use tokio::sync::broadcast::error::RecvError;
use tracing::debug;

use crate::events::{AppEvent, EventRx};

const APP_NAME: &str = "StreamArchiver";

/// Run the notification loop until the event bus closes.
pub async fn run(mut rx: EventRx) {
    loop {
        match rx.recv().await {
            Ok(AppEvent::RecordingStarted { channel, .. }) => {
                notify("Recording started", &channel);
            }
            Ok(AppEvent::RecordingFinished {
                channel, status, ..
            }) => {
                // An "ended" take captured nothing because the stream had already
                // concluded / wasn't live — a non-event, not worth a toast.
                if status != "ended" {
                    notify("Recording finished", &format!("{channel} — {status}"));
                }
            }
            Ok(AppEvent::Error { context, message }) => {
                notify("StreamArchiver error", &format!("{context}: {message}"));
            }
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

fn notify(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    // notify-rust's show() blocks; keep it off the async runtime.
    tokio::task::spawn_blocking(move || {
        if let Err(e) = notify_rust::Notification::new()
            .appname(APP_NAME)
            .summary(&summary)
            .body(&body)
            .show()
        {
            debug!("notification failed: {e}");
        }
    });
}
