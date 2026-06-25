//! Desktop notifications for recording lifecycle events.
//!
//! On Windows we build a rich toast directly via WinRT (`Windows.UI.Notifications`):
//! the channel's profile pic as a circular app-logo, its banner as the hero
//! image, the channel name + stream title + game as text, and a "Watch stream"
//! button that opens the channel URL (protocol activation — handled by Windows,
//! no app callback needed). On other platforms we fall back to `notify_rust`.
//!
//! Toasts are shown under the built-in Windows PowerShell AppUserModelID so they
//! appear without registering our own AUMID / Start-Menu shortcut; that's also
//! why Windows attributes them to "Windows PowerShell". Proper branding and
//! buttons that call *back* into the app would need a registered AUMID + COM
//! activator (a separate, packaging-adjacent piece).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tracing::debug;

use crate::events::{AppEvent, EventRx};
use crate::models::MonitorWithChannel;
use crate::store::Store;

#[cfg(not(windows))]
const APP_NAME: &str = "StreamArchiver";

/// `app_settings` key for the desktop-notification toggle. Absent/`"1"` = on
/// (the default); `"0"` = the user disabled toasts in Settings.
pub const K_NOTIFICATIONS: &str = "notifications_enabled";

/// The built-in PowerShell AppUserModelID — lets a Win32 app show toasts without
/// registering its own AUMID + Start-Menu shortcut.
#[cfg(windows)]
const POWERSHELL_AUMID: &str =
    r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

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

/// A protocol-activation button: clicking it asks Windows to open `url` (e.g. the
/// channel's stream page) — no callback into this app required.
struct ToastAction {
    label: String,
    url: String,
}

/// The resolved content of one toast, built from the event + store before the
/// (blocking) OS call.
struct ToastContent {
    heading: String,
    /// Extra text lines under the heading (stream title, game, or an error body).
    lines: Vec<String>,
    /// Profile pic (shown as a circular app-logo override).
    logo: Option<PathBuf>,
    /// Banner (shown as the hero image).
    hero: Option<PathBuf>,
    action: Option<ToastAction>,
}

impl ToastContent {
    /// A plain text toast (no images / actions) — used for errors and fallbacks.
    fn text(heading: String, line: String) -> ToastContent {
        ToastContent {
            heading,
            lines: vec![line],
            logo: None,
            hero: None,
            action: None,
        }
    }
}

/// Run the notification loop until the event bus closes. Each event is handled on
/// a blocking thread (store reads + the OS toast call) so the async loop stays
/// responsive.
pub async fn run(mut rx: EventRx, store: Arc<Store>) {
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        };
        let store = store.clone();
        tokio::task::spawn_blocking(move || handle(&store, ev));
    }
}

/// Build and show the toast for one event (runs on a blocking thread).
fn handle(store: &Store, ev: AppEvent) {
    if !enabled(store) {
        return;
    }
    let content = match ev {
        AppEvent::RecordingStarted { monitor_id, .. } => {
            let Some(row) = store.get_monitor_with_channel(monitor_id).ok().flatten() else {
                return;
            };
            let heading = format!("{} is live", row.channel.name);
            content_for(&row, heading)
        }
        AppEvent::RecordingFinished {
            recording_id,
            channel,
            status,
        } => {
            // An "ended" take captured nothing (stream had already concluded) — a
            // non-event, not worth a toast.
            if status == "ended" {
                return;
            }
            let row = store
                .monitor_id_for_recording(recording_id)
                .ok()
                .flatten()
                .and_then(|mid| store.get_monitor_with_channel(mid).ok().flatten());
            match row {
                Some(row) => {
                    let heading = format!("{} — {status}", row.channel.name);
                    content_for(&row, heading)
                }
                None => ToastContent::text(
                    "Recording finished".to_string(),
                    format!("{channel} — {status}"),
                ),
            }
        }
        AppEvent::Error { context, message } => ToastContent::text(
            "StreamArchiver error".to_string(),
            format!("{context}: {message}"),
        ),
        _ => return,
    };
    show_toast(content);
}

/// Enrich a toast from a monitor+channel row: the channel's profile pic / banner
/// (from the per-platform asset dir), its current stream title + game, and a
/// "Watch stream" button to the source URL.
fn content_for(row: &MonitorWithChannel, heading: String) -> ToastContent {
    let dir = crate::assets::channel_asset_dir(&row.channel.name, row.monitor.platform());
    let mut lines = Vec::new();
    if !row.last_recording_title.is_empty() {
        lines.push(row.last_recording_title.clone());
    }
    if !row.last_recording_category.is_empty() {
        lines.push(row.last_recording_category.clone());
    }
    let action = (!row.monitor.url.trim().is_empty()).then(|| ToastAction {
        label: "Watch stream".to_string(),
        url: row.monitor.url.clone(),
    });
    ToastContent {
        heading,
        lines,
        logo: find_asset(&dir, "icon."),
        hero: find_asset(&dir, "banner."),
        action,
    }
}

/// First file in `dir` whose name starts with `prefix` (e.g. `"icon."`).
fn find_asset(dir: &Path, prefix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
        })
}

// ---------- Windows: rich WinRT toast ----------

#[cfg(windows)]
fn show_toast(c: ToastContent) {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::HSTRING;

    // WinRT activation needs an initialized COM apartment on this thread. The
    // blocking-pool thread starts uninitialized; this is idempotent if reused.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let mut texts = format!("<text>{}</text>", xml_escape(&c.heading));
    for line in c.lines.iter().filter(|l| !l.is_empty()) {
        texts.push_str(&format!("<text>{}</text>", xml_escape(line)));
    }
    let mut images = String::new();
    if let Some(logo) = &c.logo {
        images.push_str(&format!(
            r#"<image placement="appLogoOverride" hint-crop="circle" src="{}"/>"#,
            xml_escape(&file_uri(logo)),
        ));
    }
    if let Some(hero) = &c.hero {
        images.push_str(&format!(
            r#"<image placement="hero" src="{}"/>"#,
            xml_escape(&file_uri(hero)),
        ));
    }
    let actions = match &c.action {
        Some(a) => format!(
            r#"<actions><action content="{}" activationType="protocol" arguments="{}"/></actions>"#,
            xml_escape(&a.label),
            xml_escape(&a.url),
        ),
        None => String::new(),
    };
    let xml = format!(
        r#"<toast><visual><binding template="ToastGeneric">{texts}{images}</binding></visual>{actions}</toast>"#,
    );

    let render = || -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml.as_str()))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(POWERSHELL_AUMID))?;
        notifier.Show(&toast)
    };
    if let Err(e) = render() {
        debug!("toast failed: {e}");
    }
}

/// Escape text/attribute values for inclusion in the toast XML.
#[cfg(windows)]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Turn a local path into a `file:///` URI for a toast image `src`.
///
/// Percent-encodes every byte outside an unreserved + path-safe allowlist, so a
/// `#` (URI fragment — would truncate the path and silently drop the image), a
/// space, an `&`, or a non-ASCII channel name all survive intact. `/` (separator)
/// and `:` (drive letter) are kept literal.
#[cfg(windows)]
fn file_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut enc = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b':' => enc.push(b as char),
            _ => enc.push_str(&format!("%{b:02X}")),
        }
    }
    format!("file:///{}", enc.trim_start_matches('/'))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn file_uri_encodes_reserved_and_keeps_path_chars() {
        // `#` (fragment) and space are encoded; drive `:` and separators kept.
        assert_eq!(
            file_uri(Path::new(r"C:\Users\a b\cool#1\icon.png")),
            "file:///C:/Users/a%20b/cool%231/icon.png",
        );
        // Non-ASCII channel names are UTF-8 percent-encoded (é = 0xC3 0xA9).
        assert_eq!(
            file_uri(Path::new("C:\\assets\\caf\u{e9}\\banner.jpg")),
            "file:///C:/assets/caf%C3%A9/banner.jpg",
        );
    }
}

// ---------- Other platforms: notify_rust ----------

#[cfg(not(windows))]
fn show_toast(c: ToastContent) {
    // Already on a blocking thread (`handle` runs via `spawn_blocking`), so the
    // blocking `show()` is fine to call directly.
    let body = c.lines.join("\n");
    if let Err(e) = notify_rust::Notification::new()
        .appname(APP_NAME)
        .summary(&c.heading)
        .body(&body)
        .show()
    {
        debug!("notification failed: {e}");
    }
}
