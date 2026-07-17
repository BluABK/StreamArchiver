//! Desktop notifications for recording lifecycle events.
//!
//! On Windows we build a rich toast directly via WinRT (`Windows.UI.Notifications`):
//! the channel's profile pic as a circular app-logo, its banner as the hero
//! image, the channel name + stream title + game as text, and a "Watch stream"
//! button that opens the channel URL (protocol activation — handled by Windows,
//! no app callback needed). On other platforms we fall back to `notify_rust`.
//!
//! Toasts are shown under our registered `BluABK.StreamArchiver` AUMID (see
//! [`crate::toast_activation`]) so Windows attributes them to "StreamArchiver"
//! with our icon, and clicking the toast *body* is a foreground activation
//! delivered back into the app via the registered COM activator (focus the
//! window, or open the 🔔 feed — the `launch` argument decides). If that
//! registration failed this launch, toasts fall back to the borrowed
//! PowerShell AUMID and keep working, just unbranded and without body clicks.

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

/// Do Not Disturb: `"1"` = suppress toasts right now, regardless of the
/// schedule below. Absent/`"0"` = off (the default).
pub const K_DND_ENABLED: &str = "dnd_enabled";
/// `"1"` = also suppress toasts automatically during the `[K_DND_START,
/// K_DND_END)` daily local-time window, independent of `K_DND_ENABLED`.
pub const K_DND_SCHEDULE_ENABLED: &str = "dnd_schedule_enabled";
/// `"HH:MM"` local time the scheduled DND window begins. `start > end` spans
/// midnight (e.g. `22:00`–`08:00`).
pub const K_DND_START: &str = "dnd_start";
/// `"HH:MM"` local time the scheduled DND window ends (exclusive).
pub const K_DND_END: &str = "dnd_end";

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

/// Whether Do Not Disturb is suppressing toasts right now: either manually
/// enabled, or the schedule is on and the local wall-clock time falls in its
/// window. Only gates the OS toast — the in-app notifications feed row is
/// still recorded either way, same as the master `enabled()` toggle. Read
/// live (cheap, infrequent) so a Settings change takes effect immediately.
fn dnd_active(store: &Store) -> bool {
    let manual = store.get_setting(K_DND_ENABLED).ok().flatten().as_deref() == Some("1");
    if manual {
        return true;
    }
    let schedule_on =
        store.get_setting(K_DND_SCHEDULE_ENABLED).ok().flatten().as_deref() == Some("1");
    if !schedule_on {
        return false;
    }
    let start = store.get_setting(K_DND_START).ok().flatten().unwrap_or_default();
    let end = store.get_setting(K_DND_END).ok().flatten().unwrap_or_default();
    match (parse_hhmm(&start), parse_hhmm(&end)) {
        (Some(start), Some(end)) => in_time_range(chrono::Local::now().time(), start, end),
        _ => false,
    }
}

fn parse_hhmm(s: &str) -> Option<chrono::NaiveTime> {
    chrono::NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

/// True when `now` falls in the half-open `[start, end)` window. `start >
/// end` is treated as spanning midnight (e.g. `22:00`–`08:00` covers both
/// 23:00 and 04:00). Equal bounds cover the whole day — there's no useful
/// "zero-width" reading for a DND window.
fn in_time_range(now: chrono::NaiveTime, start: chrono::NaiveTime, end: chrono::NaiveTime) -> bool {
    if start == end {
        true
    } else if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
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
    /// Foreground-activation argument for a toast-*body* click, delivered to
    /// the app's COM activator (`toast_activation::parse_activation_args`).
    #[cfg_attr(not(windows), allow(dead_code))]
    launch: &'static str,
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
            launch: "action=focus",
        }
    }
}

/// Fire a test toast for the given channel/monitor parameters. Constructs a rich
/// toast (with profile pic + banner from disk if available, a "Watch stream"
/// button, and the supplied title/game) and shows it immediately on the calling
/// thread. Intended for the debug view's "Send test toast" button.
pub fn send_test_toast(
    channel_name: &str,
    channel_url: &str,
    platform: crate::models::Platform,
    title: &str,
    game: &str,
) {
    let account = crate::assets::account_slug(channel_url, platform);
    let mut lines = Vec::new();
    if !title.is_empty() {
        lines.push(title.to_string());
    }
    if !game.is_empty() {
        lines.push(game.to_string());
    }
    let action = (!channel_url.trim().is_empty()).then(|| ToastAction {
        label: "Watch stream".to_string(),
        url: channel_url.to_string(),
    });
    show_toast(ToastContent {
        heading: format!("{channel_name} is live"),
        lines,
        logo: find_toast_asset(channel_name, platform, &account, "icon."),
        hero: find_toast_asset(channel_name, platform, &account, "banner."),
        action,
        launch: "action=focus",
    });
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

/// Per-event feed metadata that [`ToastContent`] doesn't itself carry.
struct NotifMeta {
    kind: crate::models::NotificationKind,
    severity: &'static str,
    monitor_id: Option<i64>,
    channel: String,
    recording_id: Option<i64>,
    /// Dedup key for the partial-unique index (`""` = never dedup).
    ref_key: String,
}

/// Resolve one event into a toast, record it in the in-app notifications feed,
/// then show the OS toast. The feed row is recorded **regardless** of the
/// desktop-notification toggle (Settings promises the in-app views keep
/// updating with toasts off); only the OS `show_toast` call is gated. Runs on a
/// blocking thread.
fn handle(store: &Store, ev: AppEvent) {
    use crate::models::NotificationKind;
    let (content, meta) = match ev {
        AppEvent::RecordingStarted { monitor_id, recording_id, thumbnail_path, .. } => {
            let Some(row) = store.get_monitor_with_channel(monitor_id).ok().flatten() else {
                return;
            };
            let heading = format!("{} is live", row.channel.name);
            let mut content = content_for(&row, heading, "Watch stream");
            // Prefer the stream thumbnail as the hero image when the monitor
            // opts in and the file has been fetched. The fetch is concurrent so
            // check existence here rather than assuming it's ready.
            if row.monitor.thumbnail_in_toast {
                if let Some(path) = thumbnail_path
                    .as_ref()
                    .filter(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, p))
                {
                    content.hero = Some(path.clone());
                }
            }
            let meta = NotifMeta {
                kind: NotificationKind::WentLive,
                severity: "info",
                monitor_id: Some(monitor_id),
                channel: row.channel.name.clone(),
                recording_id: Some(recording_id),
                ref_key: format!("went_live:{recording_id}"),
            };
            (content, meta)
        }
        AppEvent::TriggerMatched { monitor_id, desc, matched, went_live_at, forced_start } => {
            let Some(row) = store.get_monitor_with_channel(monitor_id).ok().flatten() else {
                return;
            };
            let heading = format!("⚡ {} — trigger matched", row.channel.name);
            let mut content = content_for(&row, heading, "Watch stream");
            content.lines = vec![
                format!("\u{201c}{matched}\u{201d}"),
                if forced_start {
                    format!("{desc} — recording started (Auto-record is off)")
                } else {
                    format!("{desc} — applied to the automatic recording")
                },
            ];
            let meta = NotifMeta {
                kind: NotificationKind::TriggerMatched,
                severity: "info",
                monitor_id: Some(monitor_id),
                channel: row.channel.name.clone(),
                recording_id: None,
                // One feed row per stream even if the capture restarts.
                ref_key: format!("trigger:{monitor_id}:{went_live_at}"),
            };
            (content, meta)
        }
        AppEvent::TriggerBlocked { monitor_id, desc, matched, went_live_at } => {
            let Some(row) = store.get_monitor_with_channel(monitor_id).ok().flatten() else {
                return;
            };
            let heading = format!("🚫 {} — blacklist blocked recording", row.channel.name);
            let mut content = content_for(&row, heading, "Watch stream");
            content.lines = vec![
                format!("\u{201c}{matched}\u{201d}"),
                format!("{desc} — automatic recording suppressed (manual ▶ Start still records)"),
            ];
            let meta = NotifMeta {
                kind: NotificationKind::TriggerBlocked,
                severity: "info",
                monitor_id: Some(monitor_id),
                channel: row.channel.name.clone(),
                recording_id: None,
                // One feed row per stream even if the veto re-fires.
                ref_key: format!("trigger_block:{monitor_id}:{went_live_at}"),
            };
            (content, meta)
        }
        AppEvent::RecordingFinished {
            recording_id,
            channel,
            status,
        } => {
            // An "ended" take captured nothing (stream had already concluded) — a
            // non-event, worth neither a toast nor a feed row.
            if status == "ended" {
                return;
            }
            let resolved = store.monitor_id_for_recording(recording_id).ok().flatten();
            let row = resolved
                .as_ref()
                .and_then(|(mid, _)| store.get_monitor_with_channel(*mid).ok().flatten());
            let severity = if status == "failed" { "error" } else { "info" };
            let mid = resolved.as_ref().map(|(mid, _)| *mid);
            let (content, chan_name) = match row {
                Some(row) => {
                    let heading = format!("{} — {status}", row.channel.name);
                    let vod_url = match row.monitor.platform() {
                        // YouTube: open the specific video when we have the stream id.
                        crate::models::Platform::YouTube => {
                            resolved.as_ref().and_then(|(_, sid)| sid.as_deref()).map(|sid| {
                                format!("https://www.youtube.com/watch?v={sid}")
                            })
                        }
                        // Twitch: VODs take minutes to appear; open the archive
                        // filter page so the user can find it once it's ready.
                        crate::models::Platform::Twitch => {
                            Some(format!("{}/videos?filter=archives", row.monitor.url.trim_end_matches('/')))
                        }
                        _ => None,
                    };
                    let name = row.channel.name.clone();
                    (content_for_url(&row, heading, "Watch VOD", vod_url), name)
                }
                None => (
                    ToastContent::text(
                        "Recording finished".to_string(),
                        format!("{channel} — {status}"),
                    ),
                    channel.clone(),
                ),
            };
            let meta = NotifMeta {
                kind: NotificationKind::RecordingFinished,
                severity,
                monitor_id: mid,
                channel: chan_name,
                recording_id: Some(recording_id),
                ref_key: format!("recfin:{recording_id}"),
            };
            (content, meta)
        }
        AppEvent::VodMuted { recording_id, channel, muted_secs } => {
            let mins = muted_secs / 60;
            let dur = if mins > 0 { format!("{mins} min") } else { format!("{muted_secs}s") };
            let mut content = ToastContent::text(
                "VOD is DMCA-muted".to_string(),
                format!(
                    "{channel}'s published VOD has {dur} of muted content — recovering the audio; \
                     the live recording is kept."
                ),
            );
            // Land a body click on the 🔔 feed where the details live.
            content.launch = "action=notifications";
            let mid = store
                .monitor_id_for_recording(recording_id)
                .ok()
                .flatten()
                .map(|(mid, _)| mid);
            let meta = NotifMeta {
                kind: NotificationKind::VodMuted,
                severity: "error",
                monitor_id: mid,
                channel,
                recording_id: Some(recording_id),
                ref_key: format!("vodmuted:{recording_id}"),
            };
            (content, meta)
        }
        AppEvent::Error { context, message } => {
            let mut content = ToastContent::text(
                "StreamArchiver error".to_string(),
                format!("{context}: {message}"),
            );
            content.launch = "action=notifications";
            let meta = NotifMeta {
                kind: NotificationKind::Error,
                severity: "error",
                monitor_id: None,
                channel: String::new(),
                recording_id: None,
                // Errors are treated as always-distinct (no dedup).
                ref_key: String::new(),
            };
            (content, meta)
        }
        AppEvent::QualityUpgraded { monitor_id, channel, from, to } => {
            let content = ToastContent::text(
                "⬆ Recording quality upgrade".to_string(),
                format!(
                    "{channel}: {to} appeared after the capture joined at {from} — \
                     restarting the take at the better quality (the head backfill \
                     covers the seam)."
                ),
            );
            let meta = NotifMeta {
                kind: NotificationKind::QualityUpgrade,
                severity: "info",
                monitor_id: Some(monitor_id),
                channel,
                recording_id: None,
                ref_key: format!("quality_upgrade:{monitor_id}:{to}"),
            };
            (content, meta)
        }
        _ => return,
    };

    // Record the in-app feed row (idempotent on ref_key) before the OS toast.
    let n = crate::store::NewNotification {
        kind: meta.kind.id().to_string(),
        severity: meta.severity.to_string(),
        title: content.heading.clone(),
        body: content.lines.join("\n"),
        monitor_id: meta.monitor_id,
        channel: meta.channel,
        recording_id: meta.recording_id,
        action_label: content.action.as_ref().map(|a| a.label.clone()).unwrap_or_default(),
        action_url: content.action.as_ref().map(|a| a.url.clone()).unwrap_or_default(),
        image_path: content
            .hero
            .as_ref()
            .or(content.logo.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ref_key: meta.ref_key,
    };
    let _ = store.insert_notification(&n);

    if enabled(store) && !dnd_active(store) {
        show_toast(content);
    }
}

/// Enrich a toast from a monitor+channel row: the channel's profile pic / banner
/// (from the per-platform asset dir), its current stream title + game, and an
/// action button labelled `action_label`. The button URL is `override_url` when
/// `Some`, otherwise the monitor's channel URL.
fn content_for_url(
    row: &MonitorWithChannel,
    heading: String,
    action_label: &str,
    override_url: Option<String>,
) -> ToastContent {
    let platform = row.monitor.platform();
    let account = crate::assets::account_slug(&row.monitor.url, platform);
    let mut lines = Vec::new();
    if !row.last_recording_title.is_empty() {
        lines.push(row.last_recording_title.clone());
    }
    if !row.last_recording_category.is_empty() {
        lines.push(row.last_recording_category.clone());
    }
    let url = override_url.unwrap_or_else(|| row.monitor.url.clone());
    let action = (!url.trim().is_empty()).then(|| ToastAction {
        label: action_label.to_string(),
        url,
    });
    ToastContent {
        heading,
        lines,
        logo: find_toast_asset(&row.channel.name, platform, &account, "icon."),
        hero: find_toast_asset(&row.channel.name, platform, &account, "banner."),
        action,
        launch: "action=focus",
    }
}

/// Locate a toast image: the monitor's own account dir first, then the legacy
/// per-platform dir (pre-migration layouts).
fn find_toast_asset(
    name: &str,
    platform: crate::models::Platform,
    account: &str,
    prefix: &str,
) -> Option<std::path::PathBuf> {
    crate::assets::asset_read_dirs(name, platform, account)
        .iter()
        .find_map(|d| crate::assets::find_asset(d, prefix))
}

fn content_for(row: &MonitorWithChannel, heading: String, action_label: &str) -> ToastContent {
    content_for_url(row, heading, action_label, None)
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
    // The root `launch` argument reaches our COM activator on a body click
    // (foreground activation) — a no-op under the fallback PowerShell AUMID.
    let xml = format!(
        r#"<toast launch="{launch}" activationType="foreground"><visual><binding template="ToastGeneric">{texts}{images}</binding></visual>{actions}</toast>"#,
        launch = xml_escape(c.launch),
    );

    let render = || -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml.as_str()))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
            crate::toast_activation::effective_aumid(),
        ))?;
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

#[cfg(test)]
mod dnd_tests {
    use super::{in_time_range, parse_hhmm};

    fn t(hhmm: &str) -> chrono::NaiveTime {
        parse_hhmm(hhmm).unwrap()
    }

    #[test]
    fn same_day_window() {
        // 09:00-17:00 "work hours": inside, at the start (inclusive), at the
        // end (exclusive), and outside.
        assert!(in_time_range(t("12:00"), t("09:00"), t("17:00")));
        assert!(in_time_range(t("09:00"), t("09:00"), t("17:00")));
        assert!(!in_time_range(t("17:00"), t("09:00"), t("17:00")));
        assert!(!in_time_range(t("08:59"), t("09:00"), t("17:00")));
        assert!(!in_time_range(t("20:00"), t("09:00"), t("17:00")));
    }

    #[test]
    fn overnight_window_spans_midnight() {
        // 22:00-08:00: covers late night and early morning, not the daytime gap.
        assert!(in_time_range(t("23:30"), t("22:00"), t("08:00")));
        assert!(in_time_range(t("04:00"), t("22:00"), t("08:00")));
        assert!(in_time_range(t("22:00"), t("22:00"), t("08:00")));
        assert!(in_time_range(t("07:59"), t("22:00"), t("08:00")), "still within the early-morning half");
        assert!(!in_time_range(t("21:59"), t("22:00"), t("08:00")), "not yet reached the evening start");
        assert!(!in_time_range(t("08:00"), t("22:00"), t("08:00")), "end is exclusive");
        assert!(!in_time_range(t("12:00"), t("22:00"), t("08:00")));
    }

    #[test]
    fn equal_bounds_cover_whole_day() {
        assert!(in_time_range(t("00:00"), t("00:00"), t("00:00")));
        assert!(in_time_range(t("13:37"), t("06:00"), t("06:00")));
    }

    #[test]
    fn parse_hhmm_rejects_garbage() {
        assert!(parse_hhmm("22:00").is_some());
        assert!(parse_hhmm(" 8:05 ").is_some());
        assert!(parse_hhmm("").is_none());
        assert!(parse_hhmm("25:00").is_none());
        assert!(parse_hhmm("not a time").is_none());
    }
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
