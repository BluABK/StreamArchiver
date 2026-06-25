//! Desktop notifications for recording lifecycle events.
//!
//! Subscribes to the event bus and shows a toast when a recording starts or
//! finishes. On Windows, correct branding requires an AppUserModelID + Start
//! Menu shortcut (installed by the packaging step); without one, Windows
//! attributes the toast to PowerShell.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tracing::debug;

use crate::events::{AppEvent, EventRx};
use crate::store::Store;

const APP_NAME: &str = "StreamArchiver";

/// `app_settings` key for the desktop-notification toggle. Absent/`"1"` = on
/// (the default); `"0"` = the user disabled toasts in Settings.
pub const K_NOTIFICATIONS: &str = "notifications_enabled";

/// Whether desktop notifications are enabled. Read live so a Settings change
/// takes effect immediately (events are infrequent, so a DB read per event is
/// cheap). Defaults to enabled when the setting is missing or unreadable.
fn enabled(store: &Store) -> bool {
    store
        .get_setting(K_NOTIFICATIONS)
        .ok()
        .flatten()
        .as_deref()
        != Some("0")
}

/// Run the notification loop until the event bus closes.
pub async fn run(mut rx: EventRx, store: Arc<Store>) {
    loop {
        match rx.recv().await {
            Ok(AppEvent::RecordingStarted { channel, .. }) => {
                notify(&store, "Recording started", &channel);
            }
            Ok(AppEvent::RecordingFinished {
                channel, status, ..
            }) => {
                // An "ended" take captured nothing because the stream had already
                // concluded / wasn't live — a non-event, not worth a toast.
                if status != "ended" {
                    notify(&store, "Recording finished", &format!("{channel} — {status}"));
                }
            }
            Ok(AppEvent::Error { context, message }) => {
                notify(&store, "StreamArchiver error", &format!("{context}: {message}"));
            }
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

fn notify(store: &Store, summary: &str, body: &str) {
    if !enabled(store) {
        return;
    }
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
