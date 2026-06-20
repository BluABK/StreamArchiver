//! The on-demand egui window: channel table, add/edit form, and settings.
//!
//! Runs reactive (repaints only on input/events). The tray thread wakes it via
//! `Context::request_repaint`. Closing the window hides it to the tray; the
//! tray "Quit" item triggers a real close.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use tracing::warn;
use tray_icon::TrayIcon;

use crate::app_core::AppCore;
use crate::events::{ManualCommand, UiCommand};
use crate::models::{
    AdBreak, AuthKind, Channel, Container, DetectionMethod, DownloadDefaults, Monitor,
    MonitorWithChannel, Platform, Recording, StreamGroup, Tool, Video, group_recordings,
};
use crate::oauth::{self, AuthFlow};
use crate::platform::AutoStart;

const K_TWITCH_ID: &str = "twitch_client_id";
const K_TWITCH_SECRET: &str = "twitch_client_secret";
const K_YT_KEY: &str = "youtube_api_key";
const K_KICK_ID: &str = "kick_client_id";
const K_KICK_SECRET: &str = "kick_client_secret";
const K_DEFAULT_OUT: &str = "default_output_dir";
const K_MAX_CONCURRENT: &str = "max_concurrent_downloads";
const K_DOWNLOAD_AUTH: &str = "download_auth_method";
const K_COOKIES_BROWSER: &str = "cookies_browser";
const K_WEBSUB_URL: &str = "websub_vps_url";
const K_WEBSUB_TOKEN: &str = "websub_token";
const K_WEBSUB_POLL: &str = "websub_poll_secs";

/// Browsers yt-dlp can read cookies from (for the Settings dropdown).
const COOKIE_BROWSERS: [&str; 8] = [
    "firefox", "chrome", "chromium", "edge", "brave", "opera", "vivaldi", "safari",
];

#[derive(PartialEq, Eq)]
enum View {
    Streams,
    Videos,
    Settings,
}

/// State of the on-demand "List formats" probe (Videos tab), shown in a window.
#[derive(Clone)]
enum FormatProbe {
    Idle,
    Running,
    Done(String),
    Failed(String),
}

/// A self-mutating action picked from a video row's context menu (open/copy are
/// handled inline; these need deferred access to `self`).
enum VideoMenuChoice {
    Stop,
    Retry,
    Delete,
}

/// Backing state for the create/rename channel-container dialog.
struct ChannelForm {
    /// `Some(id)` = renaming an existing channel; `None` = creating a new one.
    id: Option<i64>,
    name: String,
}

/// Backing state for the add/edit dialog. `name` is the channel (container) name;
/// `url` is this *instance's* source URL (the platform is derived from it).
struct MonitorForm {
    monitor_id: Option<i64>,
    channel_id: Option<i64>,
    name: String,
    url: String,
    tool: Tool,
    detection_method: DetectionMethod,
    poll_interval_secs: i64,
    quality: String,
    output_dir: String,
    filename_template: String,
    container: Container,
    capture_from_start: bool,
    /// Manually mark this instance ad-free (member/sub/Turbo) — drives the Ad-free
    /// column when auto detection isn't available.
    ad_free: bool,
    enabled: bool,
    auth_kind: AuthKind,
    auth_value: String,
    extra_args: String,
    /// Platform the tool/detection defaults were last set for; a URL change to a
    /// different platform re-applies that platform's defaults.
    last_platform: Option<Platform>,
}

impl MonitorForm {
    /// "Add stream": a new channel container + its first instance.
    fn new_channel(default_output_dir: String) -> MonitorForm {
        MonitorForm {
            monitor_id: None,
            channel_id: None,
            name: String::new(),
            url: String::new(),
            tool: Tool::Streamlink,
            detection_method: DetectionMethod::GenericProbe,
            poll_interval_secs: 60,
            quality: "best".into(),
            output_dir: default_output_dir,
            filename_template: "{name}_{date}_{time}".into(),
            container: Container::Mkv,
            capture_from_start: true,
            ad_free: false,
            enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            extra_args: String::new(),
            last_platform: None,
        }
    }

    fn from_existing(row: &MonitorWithChannel) -> MonitorForm {
        let m = &row.monitor;
        MonitorForm {
            monitor_id: Some(m.id),
            channel_id: Some(row.channel.id),
            name: row.channel.name.clone(),
            url: m.url.clone(),
            tool: m.tool,
            detection_method: m.detection_method,
            poll_interval_secs: m.poll_interval_secs,
            quality: m.quality.clone(),
            output_dir: m.output_dir.clone(),
            filename_template: m.filename_template.clone(),
            container: m.container,
            capture_from_start: m.capture_from_start,
            ad_free: m.ad_free,
            enabled: m.enabled,
            auth_kind: m.auth_kind,
            auth_value: m.auth_value.clone(),
            extra_args: m.extra_args.clone(),
            // Don't override the saved tool/detection just because the form opened.
            last_platform: Some(m.platform()),
        }
    }

    /// Add another instance to an existing channel container. The URL is blank so
    /// the user enters a (possibly different-platform) source.
    fn add_instance(channel: &Channel, default_output_dir: String) -> MonitorForm {
        MonitorForm {
            monitor_id: None,
            channel_id: Some(channel.id),
            name: channel.name.clone(),
            url: String::new(),
            tool: Tool::Streamlink,
            detection_method: DetectionMethod::GenericProbe,
            poll_interval_secs: 60,
            quality: "best".into(),
            output_dir: default_output_dir,
            filename_template: "{name}_{date}_{time}".into(),
            container: Container::Mkv,
            capture_from_start: true,
            ad_free: false,
            enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            extra_args: String::new(),
            last_platform: None,
        }
    }
}

/// Backing state for the always-visible "download a video" form on the Videos tab.
///
/// Fields are pre-filled from the detected platform's saved defaults whenever the
/// platform changes; the user can override any of them per download.
struct VideoForm {
    url: String,
    title: String,
    tool: Tool,
    quality: String,
    output_dir: String,
    filename_template: String,
    /// `None` = "Default (per-platform)": use the snapshotted platform-default
    /// auth below. `Some(kind)` overrides it with `auth_value` for this download.
    auth_override: Option<AuthKind>,
    auth_value: String,
    /// The platform default's auth, snapshotted at pre-fill (used when
    /// `auth_override` is `None`) so every field resolves from one snapshot.
    default_auth_kind: AuthKind,
    default_auth_value: String,
    extra_args: String,
    /// Resolve and use the real stream/video title (sticky across downloads).
    auto_title: bool,
    /// Platform the form is currently filled for; a change triggers a re-fill.
    last_platform: Option<Platform>,
}

impl VideoForm {
    fn new() -> VideoForm {
        VideoForm {
            url: String::new(),
            title: String::new(),
            tool: Tool::YtDlp,
            quality: "best".into(),
            output_dir: String::new(),
            filename_template: "{name}_{date}_{time}".into(),
            auth_override: None,
            auth_value: String::new(),
            default_auth_kind: AuthKind::Inherit,
            default_auth_value: String::new(),
            extra_args: String::new(),
            auto_title: false,
            last_platform: None,
        }
    }
}

#[derive(Default)]
struct SettingsForm {
    twitch_client_id: String,
    twitch_client_secret: String,
    youtube_api_key: String,
    kick_client_id: String,
    kick_client_secret: String,
    default_output_dir: String,
    max_concurrent_downloads: String,
    /// Global download-auth default: "none" or "cookies".
    download_auth_method: String,
    /// Browser to read cookies from (yt-dlp `--cookies-from-browser`).
    cookies_browser: String,
    /// Optional browser profile/session (the part after `browser:`).
    cookies_profile: String,
    /// YouTube WebSub VPS relay (yt-websub) — base URL, bearer token, poll secs.
    websub_vps_url: String,
    websub_token: String,
    websub_poll_secs: String,
}

pub struct StreamArchiverApp {
    core: Arc<AppCore>,
    _tray: TrayIcon,
    ui_rx: Receiver<UiCommand>,
    events_rx: crate::events::EventRx,
    autostart: AutoStart,
    autostart_on: bool,
    quitting: bool,

    view: View,
    rows: Vec<MonitorWithChannel>,
    /// All channel containers (incl. empty ones), for the Streams tree.
    channels: Vec<Channel>,
    videos: Vec<Video>,
    form: Option<MonitorForm>,
    video_form: VideoForm,
    /// Per-platform download defaults editable on the Videos tab (persisted JSON).
    download_defaults: DownloadDefaults,
    /// Shared state of the async "List formats" probe (Videos tab).
    format_probe: Arc<Mutex<FormatProbe>>,
    settings: SettingsForm,
    status: String,
    /// Monitor id of the currently selected row (target for keyboard shortcuts).
    selected_monitor: Option<i64>,
    /// Pending instance-delete confirmation: (monitor id, channel name).
    confirm_delete: Option<(i64, String)>,
    /// Pending channel-delete confirmation: (channel id, name).
    confirm_delete_channel: Option<(i64, String)>,
    /// Backing state for the create/rename-channel dialog.
    channel_form: Option<ChannelForm>,
    /// Sort + per-column filters for the Streams table.
    streams_sort: SortState,
    streams_filters: Vec<String>,
    /// Expansion state for the Streams history tree (channel id / monitor id /
    /// stream key), and a lazy cache of recordings per expanded monitor.
    expanded_channels: HashSet<i64>,
    expanded_instances: HashSet<i64>,
    expanded_streams: HashSet<String>,
    rec_cache: HashMap<i64, Vec<Recording>>,
    /// Recording id whose ad-break cut list is shown in a popup (None = closed).
    ad_popup: Option<i64>,
    /// Sort + per-column filters for the Videos table.
    videos_sort: SortState,
    videos_filters: Vec<String>,
    /// Shared state of the interactive "Connect Twitch" device-code flow.
    twitch_flow: Arc<Mutex<AuthFlow>>,
}

impl StreamArchiverApp {
    pub fn new(
        core: Arc<AppCore>,
        tray: TrayIcon,
        ui_rx: Receiver<UiCommand>,
    ) -> StreamArchiverApp {
        let events_rx = core.subscribe();
        let autostart = AutoStart::new();
        let autostart_on = autostart.is_enabled();

        let default_out = core
            .store
            .get_setting(K_DEFAULT_OUT)
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                crate::app_paths::default_output_dir()
                    .to_string_lossy()
                    .to_string()
            });

        let settings = SettingsForm {
            twitch_client_id: setting_or_empty(&core, K_TWITCH_ID),
            twitch_client_secret: setting_or_empty(&core, K_TWITCH_SECRET),
            youtube_api_key: setting_or_empty(&core, K_YT_KEY),
            kick_client_id: setting_or_empty(&core, K_KICK_ID),
            kick_client_secret: setting_or_empty(&core, K_KICK_SECRET),
            default_output_dir: default_out,
            max_concurrent_downloads: core
                .store
                .get_setting(K_MAX_CONCURRENT)
                .ok()
                .flatten()
                .unwrap_or_else(|| "3".into()),
            download_auth_method: core
                .store
                .get_setting(K_DOWNLOAD_AUTH)
                .ok()
                .flatten()
                .unwrap_or_else(|| "none".into()),
            cookies_browser: split_browser_profile(&setting_or_empty(&core, K_COOKIES_BROWSER)).0,
            cookies_profile: split_browser_profile(&setting_or_empty(&core, K_COOKIES_BROWSER)).1,
            websub_vps_url: setting_or_empty(&core, K_WEBSUB_URL),
            websub_token: setting_or_empty(&core, K_WEBSUB_TOKEN),
            websub_poll_secs: core
                .store
                .get_setting(K_WEBSUB_POLL)
                .ok()
                .flatten()
                .unwrap_or_else(|| "15".into()),
        };

        let twitch_flow = Arc::new(Mutex::new(match oauth::connected_login(&core.store) {
            Some(login) => AuthFlow::Connected { login },
            None => AuthFlow::Idle,
        }));

        let download_defaults = core
            .store
            .get_setting("download_defaults")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<DownloadDefaults>(&s).ok())
            .unwrap_or_else(|| DownloadDefaults::seeded(&settings.default_output_dir));

        let mut app = StreamArchiverApp {
            core,
            _tray: tray,
            ui_rx,
            events_rx,
            autostart,
            autostart_on,
            quitting: false,
            view: View::Streams,
            rows: Vec::new(),
            channels: Vec::new(),
            videos: Vec::new(),
            form: None,
            video_form: VideoForm::new(),
            download_defaults,
            format_probe: Arc::new(Mutex::new(FormatProbe::Idle)),
            settings,
            status: String::new(),
            selected_monitor: None,
            confirm_delete: None,
            confirm_delete_channel: None,
            channel_form: None,
            streams_sort: SortState::default(),
            streams_filters: vec![String::new(); STREAM_COLS],
            expanded_channels: HashSet::new(),
            expanded_instances: HashSet::new(),
            expanded_streams: HashSet::new(),
            rec_cache: HashMap::new(),
            ad_popup: None,
            videos_sort: SortState::default(),
            videos_filters: vec![String::new(); VIDEO_COLS],
            twitch_flow,
        };
        app.reload_rows();
        app.reload_videos();
        app
    }

    fn reload_rows(&mut self) {
        match self.core.store.list_monitors_with_channels() {
            Ok(rows) => self.rows = rows,
            Err(e) => {
                warn!("failed to load monitors: {e:#}");
                self.status = format!("Error loading channels: {e}");
            }
        }
        // Load all containers (incl. empty ones) so they show in the tree.
        match self.core.store.list_channels() {
            Ok(chs) => self.channels = chs,
            Err(e) => warn!("failed to load channels: {e:#}"),
        }
        // History may have changed; re-fetch lazily on next expand.
        self.rec_cache.clear();
        // Drop expansion state for channels/monitors that no longer exist (avoids
        // an unbounded leak and "sticky" expansion if a row id is later reused).
        let live_channels: HashSet<i64> = self.channels.iter().map(|c| c.id).collect();
        let live_monitors: HashSet<i64> = self.rows.iter().map(|r| r.monitor.id).collect();
        self.expanded_channels.retain(|id| live_channels.contains(id));
        self.expanded_instances.retain(|id| live_monitors.contains(id));
        // Stream keys are "s<mid>:…" / "t<mid>:…"; keep only live monitors'.
        self.expanded_streams
            .retain(|k| stream_key_monitor(k).is_some_and(|mid| live_monitors.contains(&mid)));
    }

    fn reload_videos(&mut self) {
        match self.core.store.list_videos() {
            Ok(v) => self.videos = v,
            Err(e) => warn!("failed to load videos: {e:#}"),
        }
    }

    fn persist_download_defaults(&self) {
        match serde_json::to_string(&self.download_defaults) {
            Ok(json) => {
                let _ = self.core.store.set_setting("download_defaults", &json);
            }
            Err(e) => warn!("failed to serialize download defaults: {e:#}"),
        }
    }

    /// Handle tray commands and bus events; returns true if a repaint is needed.
    fn pump_messages(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.ui_rx.try_recv() {
            match cmd {
                UiCommand::ShowWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                UiCommand::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        let mut dirty = false;
        loop {
            match self.events_rx.try_recv() {
                Ok(crate::events::AppEvent::Error { context, message }) => {
                    self.status = format!("{context}: {message}");
                    dirty = true;
                }
                Ok(_) => dirty = true,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        if dirty {
            self.reload_rows();
            self.reload_videos();
        }
    }

    fn save_form(&mut self) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.url.trim().is_empty() {
            self.status = "An instance URL is required.".into();
            return;
        }
        let channel_id = match form.channel_id {
            // Existing channel container (add instance / edit instance) — the
            // channel is unchanged here; rename it from the channel row instead.
            Some(cid) => cid,
            // "Add stream": create a new channel container for this first instance.
            None => {
                if form.name.trim().is_empty() {
                    self.status = "A channel name is required.".into();
                    return;
                }
                match self.core.store.create_container(form.name.trim()) {
                    Ok(id) => id,
                    Err(e) => {
                        self.status = format!("Error saving channel: {e}");
                        return;
                    }
                }
            }
        };

        let monitor = Monitor {
            id: form.monitor_id.unwrap_or(0),
            channel_id,
            url: form.url.trim().to_string(),
            enabled: form.enabled,
            tool: form.tool,
            detection_method: form.detection_method,
            poll_interval_secs: form.poll_interval_secs.max(5),
            quality: form.quality.clone(),
            output_dir: form.output_dir.clone(),
            filename_template: form.filename_template.clone(),
            container: form.container,
            capture_from_start: form.capture_from_start,
            ad_free: form.ad_free,
            auth_kind: form.auth_kind,
            auth_value: form.auth_value.clone(),
            extra_args: form.extra_args.clone(),
            max_concurrent: 1,
            last_checked_at: None,
            last_state: "idle".into(),
        };

        let result = match form.monitor_id {
            Some(_) => self.core.store.update_monitor(&monitor),
            None => self.core.store.insert_monitor(&monitor).map(|_| ()),
        };
        match result {
            Ok(()) => {
                self.status = "Saved.".into();
                self.form = None;
                self.reload_rows();
            }
            Err(e) => self.status = format!("Error saving monitor: {e}"),
        }
    }

    fn save_settings(&mut self) {
        let s = &self.settings;
        // Persist the browser + optional profile as one `browser:profile` value.
        let cookies_value = compose_browser_profile(&s.cookies_browser, &s.cookies_profile);
        let pairs = [
            (K_TWITCH_ID, s.twitch_client_id.trim()),
            (K_TWITCH_SECRET, s.twitch_client_secret.trim()),
            (K_YT_KEY, s.youtube_api_key.trim()),
            (K_KICK_ID, s.kick_client_id.trim()),
            (K_KICK_SECRET, s.kick_client_secret.trim()),
            (K_DEFAULT_OUT, s.default_output_dir.trim()),
            (K_MAX_CONCURRENT, s.max_concurrent_downloads.trim()),
            (K_DOWNLOAD_AUTH, s.download_auth_method.trim()),
            (K_COOKIES_BROWSER, cookies_value.as_str()),
            (K_WEBSUB_URL, s.websub_vps_url.trim()),
            (K_WEBSUB_TOKEN, s.websub_token.trim()),
            (K_WEBSUB_POLL, s.websub_poll_secs.trim()),
        ];
        for (k, v) in pairs {
            if let Err(e) = self.core.store.set_setting(k, v) {
                self.status = format!("Error saving settings: {e}");
                return;
            }
        }
        self.status = "Settings saved.".into();
    }
}

fn setting_or_empty(core: &AppCore, key: &str) -> String {
    core.store
        .get_setting(key)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Split a stored `--cookies-from-browser` value into `(browser, profile)`.
/// `profile` is everything after the first `:` — a profile/session name or an
/// absolute path (which may itself contain a `:` drive letter, hence split-once).
/// yt-dlp parses the same way. Empty profile when there's no `:`.
fn split_browser_profile(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((b, p)) => (b.trim().to_string(), p.trim().to_string()),
        None => (raw.trim().to_string(), String::new()),
    }
}

/// Compose a `--cookies-from-browser` value from a browser + optional profile
/// (`firefox` or `firefox:<profile>`). Empty browser → empty (no cookies).
fn compose_browser_profile(browser: &str, profile: &str) -> String {
    let b = browser.trim();
    let p = profile.trim();
    if b.is_empty() {
        String::new()
    } else if p.is_empty() {
        b.to_string()
    } else {
        format!("{b}:{p}")
    }
}

/// Format a unix timestamp as a local `YYYY-MM-DD` date (empty if unset).
fn fmt_date(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

/// Compact local timestamp `MM-DD HH:MM:SS` (drops the year to save table width).
fn fmt_datetime_short(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// Format a duration in seconds as `HH:MM:SS`.
fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Ad-break count for a cell (blank when there are none, so empty rows stay clean).
fn fmt_ad_count(n: i64) -> String {
    if n > 0 { n.to_string() } else { String::new() }
}

/// Total ad time for a cell (blank when zero).
fn fmt_ad_time(secs: i64) -> String {
    if secs > 0 { fmt_duration(secs) } else { String::new() }
}

/// Resolve an instance's ad-free status into a (label, tooltip) for display.
/// Manual flag wins; otherwise the auto Twitch-sub result (`Some(true)` = sub'd,
/// `Some(false)` = checked & not sub'd, `None` = unknown/not checked). Returns
/// `None` when there's nothing to show.
fn ad_free_status(manual: bool, sub: Option<bool>) -> Option<(&'static str, &'static str)> {
    if manual {
        Some((
            "Yes",
            "Marked ad-free for your account (member/sub/Turbo) — captures won't have \
             ad-break hard cuts.",
        ))
    } else {
        match sub {
            Some(true) => Some((
                "Yes (sub)",
                "Your connected Twitch account is subscribed to this channel — \
                 subscriber captures have no ad breaks.",
            )),
            _ => None,
        }
    }
}

/// Channel-row ad-free summary (label + numeric sort key) from how many of the
/// channel's instances are ad-free.
fn ad_free_summary(ad_free_count: usize, total: usize) -> (&'static str, f64) {
    if total == 0 || ad_free_count == 0 {
        ("", 0.0)
    } else if ad_free_count == total {
        ("Yes", 2.0)
    } else {
        ("some", 1.0)
    }
}

/// Human-readable lines describing where ad breaks cause hard cuts in the
/// finished file. `at_secs` is already the cut's position in the captured file
/// (ad segments are filtered out), so it's shown directly as a seek timestamp.
/// `breaks` must be ordered by offset.
fn ad_cut_lines(breaks: &[AdBreak]) -> Vec<String> {
    breaks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "#{}  cut at {}  ({}s ad)",
                i + 1,
                fmt_duration(b.at_secs.max(0)),
                b.duration_secs
            )
        })
        .collect()
}

/// Multi-line tooltip body for an ad cell: a heading plus the per-break cut list
/// (or a fallback when the details aren't loaded yet).
fn ad_tooltip(count: i64, secs: i64, breaks: Option<&Vec<AdBreak>>) -> String {
    let mut s = format!(
        "{count} ad break(s), {} total — each is a hard cut in the file.",
        fmt_duration(secs)
    );
    match breaks {
        Some(b) if !b.is_empty() => {
            s.push('\n');
            s.push_str(&ad_cut_lines(b).join("\n"));
        }
        _ => {}
    }
    s
}

/// Parse the monitor id out of a [`StreamGroup`] key (`s<mid>:…` / `t<mid>:…`).
fn stream_key_monitor(key: &str) -> Option<i64> {
    let rest = key.strip_prefix('s').or_else(|| key.strip_prefix('t'))?;
    rest.split(':').next()?.parse().ok()
}

/// Format a go-live time (`~`-prefixed when only our approximate time is known).
fn fmt_went_live(at: Option<i64>, approx: bool) -> String {
    match at {
        Some(w) => {
            let s = fmt_datetime_short(w);
            if approx { format!("~{s}") } else { s }
        }
        None => String::new(),
    }
}

/// Human-readable byte size (B / KB / MB / GB).
fn fmt_bytes(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}

/// Theme color for a video download status string.
fn video_status_color(status: &str) -> egui::Color32 {
    use egui::Color32;
    match status {
        "downloading" => Color32::from_rgb(0x4d, 0x9b, 0xff),
        "completed" => Color32::from_rgb(0x57, 0xc7, 0x57),
        "failed" => Color32::from_rgb(0xe0, 0x6c, 0x6c),
        _ => Color32::from_gray(0xa0), // queued / stopped / orphaned
    }
}

/// Human-readable download speed (e.g. `1.2 MB/s`); empty when not downloading.
fn fmt_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 {
        return String::new();
    }
    format!("{}/s", fmt_bytes(bytes_per_sec as i64))
}

// ─── Sortable + filterable tables ───────────────────────────────────────────
//
// Both tables share a tiny model: each row is turned into a `Vec<Cell>` (one per
// sortable/filterable column, in header order; the trailing "Actions" column is
// excluded). The header renders a click-to-sort title + a per-column filter box;
// `ordered_rows` filters then sorts and returns the surviving original-row
// indices in display order. The data cells themselves are still drawn by the
// existing per-row code, indexed by those original indices.

/// Sortable/filterable Videos columns (Video..File; excludes Actions).
const VIDEO_COLS: usize = 9;
/// Sortable/filterable Streams columns (On..Added; excludes Actions).
const STREAM_COLS: usize = 16;

/// Which column a table is sorted by and in what direction. `col == None` keeps
/// the natural (database) order.
#[derive(Clone, Copy, Default)]
struct SortState {
    col: Option<usize>,
    ascending: bool,
}

/// A cell's sort key: numeric columns sort numerically, text columns sort
/// case-insensitively. (Filtering always uses the cell's displayed `text`.)
enum SortKey {
    Num(f64),
    Text(String),
}

/// A precomputed cell: `text` is what's shown/filtered (case-insensitive
/// substring), `key` is what's sorted.
struct Cell {
    text: String,
    key: SortKey,
}

impl Cell {
    /// A text cell — filter and sort both use the string.
    fn text(s: impl Into<String>) -> Cell {
        let s = s.into();
        Cell {
            key: SortKey::Text(s.clone()),
            text: s,
        }
    }
    /// A numeric cell — sorts by `n`, filters/shows `display`.
    fn num(n: f64, display: impl Into<String>) -> Cell {
        Cell {
            text: display.into(),
            key: SortKey::Num(n),
        }
    }
}

fn cmp_sort_key(a: &SortKey, b: &SortKey) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (SortKey::Num(x), SortKey::Num(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (SortKey::Text(x), SortKey::Text(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        _ => Ordering::Equal,
    }
}

/// Filter then sort `rows`, returning surviving original indices in display
/// order. `filters[c]` is a case-insensitive substring filter for column `c`.
fn ordered_rows(rows: &[Vec<Cell>], sort: &SortState, filters: &[String]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..rows.len())
        .filter(|&i| {
            filters.iter().enumerate().all(|(c, f)| {
                let f = f.trim().to_lowercase();
                f.is_empty()
                    || rows[i]
                        .get(c)
                        .map(|cell| cell.text.to_lowercase().contains(&f))
                        .unwrap_or(true)
            })
        })
        .collect();
    if let Some(c) = sort.col {
        // `sort_by` is stable, so equal keys keep their natural order.
        idx.sort_by(|&a, &b| {
            let o = match (rows[a].get(c), rows[b].get(c)) {
                (Some(x), Some(y)) => cmp_sort_key(&x.key, &y.key),
                _ => std::cmp::Ordering::Equal,
            };
            if sort.ascending { o } else { o.reverse() }
        });
    }
    idx
}

/// Render one sortable + optionally filterable header cell for column `idx`: a
/// click-to-sort title (with ▲/▼ when active) above a filter box.
fn sort_filter_header(
    ui: &mut egui::Ui,
    idx: usize,
    title: &str,
    filterable: bool,
    sort: &mut SortState,
    filter: &mut String,
) {
    ui.vertical(|ui| {
        let active = sort.col == Some(idx);
        let arrow = if active {
            if sort.ascending { " ▲" } else { " ▼" }
        } else {
            ""
        };
        let resp = ui
            .add(egui::Button::new(egui::RichText::new(format!("{title}{arrow}")).strong()).frame(false))
            .on_hover_text("Click to sort (click again to reverse)");
        if resp.clicked() {
            if active {
                sort.ascending = !sort.ascending;
            } else {
                *sort = SortState {
                    col: Some(idx),
                    ascending: true,
                };
            }
        }
        if filterable {
            ui.add(
                egui::TextEdit::singleline(filter)
                    .hint_text("filter")
                    .desired_width(f32::INFINITY),
            );
        }
    });
}

/// Sort/filter cells for one video row, in Videos-table column order:
/// Video, Channel, Platform, Tool, Status, Speed, Size, Added, File.
fn video_cells(
    v: &Video,
    speed: &std::collections::HashMap<i64, f64>,
) -> Vec<Cell> {
    let label = if v.title.trim().is_empty() {
        v.url.clone()
    } else {
        v.title.clone()
    };
    // Speed is only meaningful while actively downloading.
    let spd = if v.status == "downloading" {
        speed.get(&v.id).copied().unwrap_or(0.0)
    } else {
        0.0
    };
    vec![
        Cell::text(label),
        Cell::text(v.channel.clone()),
        Cell::text(v.platform.label()),
        Cell::text(v.tool.label()),
        Cell::text(v.status.clone()),
        Cell::num(spd, fmt_speed(spd)),
        Cell::num(
            v.bytes as f64,
            if v.bytes > 0 { fmt_bytes(v.bytes) } else { String::new() },
        ),
        Cell::num(v.created_at as f64, fmt_date(v.created_at)),
        Cell::text(v.output_path.clone()),
    ]
}

/// Columns derived from a monitor's latest recording.
struct RecordingCells {
    /// When *we* started recording.
    started_on: String,
    /// How long we've recorded (ticks while active; final length otherwise).
    duration: String,
    /// When the stream went live on the platform (`~`-prefixed if approximate).
    went_live: String,
    /// How much of the beginning we missed: the resolved lost time when known
    /// (0 once a from-start capture caught up), else provisional started - went_live.
    lost: String,
}

fn recording_cells(row: &MonitorWithChannel, now: i64) -> RecordingCells {
    let active = row.last_recording_status.as_deref() == Some("recording");
    let started = row.last_recording_started;
    let dur = match (started, row.last_recording_ended, active) {
        (Some(s), _, true) => Some(now - s),
        (Some(s), Some(e), false) => Some(e - s),
        _ => None,
    };
    let went_live = match row.last_recording_went_live {
        Some(w) => {
            let s = fmt_datetime_short(w);
            if row.last_recording_went_live_approx {
                format!("~{s}")
            } else {
                s
            }
        }
        None => String::new(),
    };
    // Prefer the resolved lost time (0 once a from-start capture caught up, or
    // the exact residual) when known; else fall back to started - went_live.
    let lost = match row.last_recording_lost_secs {
        Some(s) => fmt_duration(s.max(0)),
        None => match (started, row.last_recording_went_live) {
            (Some(s), Some(w)) => fmt_duration((s - w).max(0)),
            _ => String::new(),
        },
    };
    RecordingCells {
        started_on: started.map(fmt_datetime_short).unwrap_or_default(),
        duration: dur.map(fmt_duration).unwrap_or_default(),
        went_live,
        lost,
    }
}

/// Theme color for a recording / stream status string.
fn rec_status_color(status: &str) -> egui::Color32 {
    use egui::Color32;
    match status {
        "recording" => Color32::from_rgb(0x4d, 0x9b, 0xff),
        "completed" => Color32::from_rgb(0x57, 0xc7, 0x57),
        "failed" => Color32::from_rgb(0xe0, 0x6c, 0x6c),
        _ => Color32::from_gray(0xa0), // stopped / orphaned
    }
}

/// Render the Streams-tree Name cell: indent by `depth`, a clickable ▶/▼ when
/// `has_children`, then `label`. Returns true if the disclosure was clicked.
fn tree_name(
    ui: &mut egui::Ui,
    depth: usize,
    has_children: bool,
    expanded: bool,
    label: impl Into<egui::WidgetText>,
) -> bool {
    let mut clicked = false;
    ui.add_space(depth as f32 * 16.0);
    if has_children {
        let tri = if expanded { "▼" } else { "▶" };
        if ui
            .add(egui::Button::new(tri).small().frame(false))
            .on_hover_text("Expand / collapse")
            .clicked()
        {
            clicked = true;
        }
    } else {
        ui.add_space(16.0); // align with rows that have a triangle
    }
    ui.label(label);
    clicked
}

/// Compact, readable form of an instance's source URL for the Name cell (drops
/// the scheme and a leading `www.`).
fn instance_label(url: &str) -> String {
    let s = url.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let s = s.strip_prefix("www.").unwrap_or(s);
    let s = s.trim_end_matches('/');
    if s.is_empty() { "(no URL)".to_string() } else { s.to_string() }
}

/// The platform shared by all of a channel's instances, or `None` if they differ
/// (or there are none) — drives the container row's badge.
fn channel_platform(monitors: &[&MonitorWithChannel]) -> Option<Platform> {
    let mut it = monitors.iter().map(|m| m.monitor.platform());
    let first = it.next()?;
    if it.all(|p| p == first) { Some(first) } else { None }
}

/// Sort/filter cells for a channel container's top-level row (matches the table
/// columns). `channel` is the container; `monitors` are its instances (possibly
/// none, for an empty container).
fn channel_cells(channel: &Channel, monitors: &[&MonitorWithChannel], now: i64) -> Vec<Cell> {
    if monitors.is_empty() {
        // Empty container: just the name + "added"; everything else blank.
        let mut cells: Vec<Cell> = (0..STREAM_COLS).map(|_| Cell::text(String::new())).collect();
        cells[0] = Cell::num(0.0, "off");
        cells[1] = Cell::text(channel.name.clone());
        cells[STREAM_COLS - 1] = Cell::num(channel.created_at as f64, fmt_date(channel.created_at));
        return cells;
    }
    let all_enabled = monitors.iter().all(|m| m.monitor.enabled);
    let any_recording = monitors
        .iter()
        .any(|m| m.last_recording_status.as_deref() == Some("recording"));
    // The most recent recording across instances drives the time columns.
    let primary = monitors
        .iter()
        .copied()
        .max_by_key(|m| m.last_recording_started.unwrap_or(0))
        .unwrap_or(monitors[0]);
    let rec = recording_cells(primary, now);
    let ninst = monitors.len();
    let tool = if ninst == 1 {
        "1 instance".to_string()
    } else {
        format!("{ninst} instances")
    };
    let last = monitors
        .iter()
        .filter_map(|m| m.monitor.last_checked_at)
        .max()
        .unwrap_or(0);
    vec![
        Cell::num(
            if all_enabled { 1.0 } else { 0.0 },
            if all_enabled { "on" } else { "off" },
        ),
        Cell::text(channel.name.clone()),
        Cell::text(
            channel_platform(monitors)
                .map(|p| p.label().to_string())
                .unwrap_or_else(|| "mixed".into()),
        ),
        Cell::text(tool),
        Cell::text(String::new()), // detection
        Cell::text(String::new()), // every
        Cell::num(last as f64, fmt_datetime_short(last)),
        Cell::text(if any_recording { "recording".to_string() } else { String::new() }),
        Cell::num(
            primary.last_recording_went_live.unwrap_or(0) as f64,
            rec.went_live.clone(),
        ),
        Cell::num(
            primary.last_recording_started.unwrap_or(0) as f64,
            rec.started_on.clone(),
        ),
        Cell::num(0.0, rec.lost.clone()),
        Cell::num(0.0, rec.duration.clone()),
        Cell::num(
            primary.last_recording_ad_count as f64,
            fmt_ad_count(primary.last_recording_ad_count),
        ),
        Cell::num(
            primary.last_recording_ad_secs as f64,
            fmt_ad_time(primary.last_recording_ad_secs),
        ),
        {
            let n = monitors
                .iter()
                .filter(|m| ad_free_status(m.monitor.ad_free, m.ad_free_sub).is_some())
                .count();
            let (label, key) = ad_free_summary(n, monitors.len());
            Cell::num(key, label)
        },
        Cell::num(channel.created_at as f64, fmt_date(channel.created_at)),
    ]
}

/// Self-mutating actions collected while rendering a capture-instance row.
#[derive(Default)]
struct RowActions {
    start: Option<i64>,                 // monitor id
    stop: Option<i64>,                  // monitor id
    edit: Option<i64>,                  // monitor id
    add_instance: Option<i64>,          // channel id
    delete: Option<(i64, String)>,      // (monitor id, channel name)
    toggle_enabled: Option<(i64, bool)>,
    select: Option<i64>,                // monitor id
}

/// Render one capture-instance (monitor) row across all columns, with the Name
/// column carrying the tree disclosure. Returns true if the disclosure (the
/// row's stream history) was toggled. Self-mutating picks land in `a`.
#[allow(clippy::too_many_arguments)]
fn render_instance_row(
    tr: &mut egui_extras::TableRow<'_, '_>,
    row: &MonitorWithChannel,
    now: i64,
    recording: bool,
    is_selected: bool,
    depth: usize,
    has_history: bool,
    expanded: bool,
    a: &mut RowActions,
) -> bool {
    let m = &row.monitor;
    let rec = recording_cells(row, now);
    tr.set_selected(recording || is_selected);

    // Right-click context menu (shared with the inline action buttons).
    let add_menu = |ui: &mut egui::Ui, a: &mut RowActions| {
        ui.set_min_width(180.0);
        if recording {
            if ui.button("⏹  Stop recording").clicked() {
                a.stop = Some(m.id);
                ui.close();
            }
        } else if ui.button("▶  Start recording").clicked() {
            a.start = Some(m.id);
            ui.close();
        }
        ui.separator();
        if ui.button("🔗  Open channel URL").clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(row.monitor.url.clone()));
            ui.close();
        }
        let folder_exists = std::path::Path::new(&m.output_dir).is_dir();
        if ui
            .add_enabled(folder_exists, egui::Button::new("📂  Open output folder"))
            .clicked()
        {
            crate::platform::open_path(std::path::Path::new(&m.output_dir));
            ui.close();
        }
        if ui.button("📋  Copy URL").clicked() {
            ui.ctx().copy_text(row.monitor.url.clone());
            ui.close();
        }
        ui.separator();
        if ui.button("✏  Edit instance…").clicked() {
            a.edit = Some(m.id);
            ui.close();
        }
        if ui.button("➕  Add instance to channel").clicked() {
            a.add_instance = Some(row.channel.id);
            ui.close();
        }
        let toggle_label = if m.enabled { "⏸  Disable" } else { "✔  Enable" };
        if ui.button(toggle_label).clicked() {
            a.toggle_enabled = Some((m.id, !m.enabled));
            ui.close();
        }
        ui.separator();
        if ui.button("🗑  Delete").clicked() {
            a.delete = Some((m.id, row.channel.name.clone()));
            ui.close();
        }
    };

    let mut disclosure_clicked = false;
    tr.col(|ui| {
        let mut on = m.enabled;
        let cb = ui.checkbox(&mut on, "");
        if cb.changed() {
            a.toggle_enabled = Some((m.id, on));
        }
        cb.context_menu(|ui| add_menu(ui, a));
    });
    tr.col(|ui| {
        disclosure_clicked = tree_name(
            ui,
            depth,
            has_history,
            expanded,
            egui::RichText::new(instance_label(&row.monitor.url)),
        );
        ui.response().on_hover_text(&row.monitor.url);
    });
    tr.col(|ui| {
        platform_badge(ui, m.platform());
        ui.label(m.platform().label());
    });
    tr.col(|ui| {
        ui.label(m.tool.label()).on_hover_text(m.tool.tooltip());
    });
    tr.col(|ui| {
        ui.label(m.detection_method.short_label()).on_hover_text(format!(
            "{}\n\n{}",
            m.detection_method.label(),
            m.detection_method.tooltip()
        ));
    });
    tr.col(|ui| {
        ui.label(format!("{}s", m.poll_interval_secs));
    });
    tr.col(|ui| {
        ui.label(fmt_datetime_short(m.last_checked_at.unwrap_or(0)));
    });
    tr.col(|ui| {
        ui.label(&m.last_state);
    });
    tr.col(|ui| {
        ui.label(&rec.went_live);
    });
    tr.col(|ui| {
        ui.label(&rec.started_on);
    });
    tr.col(|ui| {
        let resp = ui.label(&rec.lost);
        if m.capture_from_start {
            resp.on_hover_text(
                "How much of the beginning we missed. Capturing from start, so this drops \
                 to 0 once the capture catches up to the live edge; until then it's an \
                 estimate (the gap before recording began).",
            );
        }
    });
    tr.col(|ui| {
        ui.label(&rec.duration);
    });
    tr.col(|ui| {
        if row.last_recording_ad_count > 0 {
            ui.label(fmt_ad_count(row.last_recording_ad_count)).on_hover_text(
                ad_tooltip(row.last_recording_ad_count, row.last_recording_ad_secs, None),
            );
        }
    });
    tr.col(|ui| {
        if row.last_recording_ad_secs > 0 {
            ui.label(fmt_ad_time(row.last_recording_ad_secs)).on_hover_text(
                ad_tooltip(row.last_recording_ad_count, row.last_recording_ad_secs, None),
            );
        }
    });
    tr.col(|ui| {
        if let Some((label, hover)) = ad_free_status(m.ad_free, row.ad_free_sub) {
            ui.colored_label(egui::Color32::from_rgb(0x57, 0xc7, 0x57), label)
                .on_hover_text(hover);
        }
    });
    tr.col(|ui| {
        ui.label(fmt_date(row.channel.created_at));
    });
    tr.col(|ui| {
        ui.push_id(m.id, |ui| {
            let mut btns: Vec<egui::Response> = Vec::with_capacity(4);
            if recording {
                let b = ui.small_button("⏹").on_hover_text("Stop / abort recording");
                if b.clicked() {
                    a.stop = Some(m.id);
                }
                btns.push(b);
            } else {
                let b = ui
                    .small_button("▶")
                    .on_hover_text("Start recording now (checks if live)");
                if b.clicked() {
                    a.start = Some(m.id);
                }
                btns.push(b);
            }
            let b = ui.small_button("✏").on_hover_text("Edit");
            if b.clicked() {
                a.edit = Some(m.id);
            }
            btns.push(b);
            let b = ui.small_button("➕").on_hover_text("Add another tool instance");
            if b.clicked() {
                a.add_instance = Some(row.channel.id);
            }
            btns.push(b);
            let b = ui.small_button("🗑").on_hover_text("Delete this instance");
            if b.clicked() {
                a.delete = Some((m.id, row.channel.name.clone()));
            }
            btns.push(b);
            for b in &btns {
                b.context_menu(|ui| add_menu(ui, a));
            }
        });
    });

    let row_resp = tr.response();
    if row_resp.clicked() || row_resp.secondary_clicked() {
        a.select = Some(m.id);
    }
    row_resp.context_menu(|ui| add_menu(ui, a));
    disclosure_clicked
}

/// Draw a small colored brand badge for the platform.
fn platform_badge(ui: &mut egui::Ui, platform: Platform) {
    use egui::{Color32, RichText};
    let (label, bg, fg) = match platform {
        Platform::Twitch => ("T", Color32::from_rgb(0x91, 0x46, 0xFF), Color32::WHITE),
        Platform::YouTube => ("▶", Color32::from_rgb(0xFF, 0x00, 0x00), Color32::WHITE),
        Platform::Kick => ("K", Color32::from_rgb(0x53, 0xFC, 0x18), Color32::BLACK),
        Platform::Generic => ("●", Color32::from_gray(0x80), Color32::WHITE),
    };
    ui.label(
        RichText::new(format!(" {label} "))
            .monospace()
            .strong()
            .color(fg)
            .background_color(bg),
    );
}

/// Open a native folder picker, seeded at `current` if it exists.
fn browse_folder(current: &str) -> Option<String> {
    let mut dialog = rfd::FileDialog::new();
    if !current.trim().is_empty() && std::path::Path::new(current).exists() {
        dialog = dialog.set_directory(current);
    }
    dialog
        .pick_folder()
        .map(|p| p.to_string_lossy().to_string())
}

/// Open a native file picker (for a cookies.txt), seeded at `current`'s folder.
fn browse_file(current: &str) -> Option<String> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(parent) = std::path::Path::new(current).parent() {
        if parent.is_dir() {
            dialog = dialog.set_directory(parent);
        }
    }
    dialog.pick_file().map(|p| p.to_string_lossy().to_string())
}

impl eframe::App for StreamArchiverApp {
    /// Non-drawing logic. eframe also calls this while the window is hidden when
    /// `request_repaint` was called — which is how the tray's "Open" wakes us.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_messages(ctx);

        // Close button hides to tray unless we're really quitting.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_shortcuts(ui.ctx());

        egui::Panel::top("top")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("StreamArchiver");
                    let built_dt = env!("BUILD_UNIX")
                        .parse::<i64>()
                        .ok()
                        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
                        .map(|dt| dt.with_timezone(&chrono::Local));
                    let built_short = built_dt
                        .map(|dt| dt.format("%m-%d %H:%M").to_string())
                        .unwrap_or_default();
                    let built_full = built_dt
                        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default();
                    ui.label(
                        egui::RichText::new(format!(
                            "v{} · {} · built {built_short}",
                            env!("CARGO_PKG_VERSION"),
                            env!("GIT_HASH"),
                        ))
                        .small()
                        .color(egui::Color32::from_gray(0x90)),
                    )
                    .on_hover_text(format!(
                        "StreamArchiver v{} · build {}\ncommit {}\ncompiled {built_full}",
                        env!("CARGO_PKG_VERSION"),
                        env!("BUILD_NUMBER"),
                        env!("GIT_HASH"),
                    ));
                    ui.separator();
                    ui.selectable_value(&mut self.view, View::Streams, "Streams");
                    ui.selectable_value(&mut self.view, View::Videos, "Videos");
                    ui.selectable_value(&mut self.view, View::Settings, "Settings");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.view == View::Streams {
                            if ui
                                .button("➕ Add stream")
                                .on_hover_text("Create a channel with its first instance (a URL to record)")
                                .clicked()
                            {
                                self.form = Some(MonitorForm::new_channel(
                                    self.settings.default_output_dir.clone(),
                                ));
                            }
                            if ui
                                .button("➕ Add channel")
                                .on_hover_text("Create an empty channel container; add instances to it afterwards")
                                .clicked()
                            {
                                self.channel_form = Some(ChannelForm {
                                    id: None,
                                    name: String::new(),
                                });
                            }
                        }
                    });
                });
            });

        egui::Panel::bottom("status")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(if self.status.is_empty() {
                        "Ready."
                    } else {
                        &self.status
                    });
                });
            });

        egui::CentralPanel::default().show_inside(ui, |ui| match self.view {
            View::Streams => self.channels_view(ui),
            View::Videos => self.videos_view(ui),
            View::Settings => self.settings_view(ui),
        });

        self.form_window(ui.ctx());
        self.channel_form_window(ui.ctx());
        self.confirm_delete_window(ui.ctx());
        self.confirm_delete_channel_window(ui.ctx());
        self.format_probe_window(ui.ctx());
        self.ad_popup_window(ui.ctx());
    }
}

impl StreamArchiverApp {
    /// Process global keyboard shortcuts once per frame, before drawing.
    ///
    /// While a modal (add/edit form or delete confirmation) is open, only `Esc`
    /// is handled — it dismisses the modal — and other shortcuts are suppressed.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        const ADD: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
        const SETTINGS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma);
        const REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F5);

        // A modal is open: Esc closes it, everything else is swallowed.
        if self.form.is_some()
            || self.channel_form.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_delete_channel.is_some()
        {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.form = None;
                self.channel_form = None;
                self.confirm_delete = None;
                self.confirm_delete_channel = None;
            }
            return;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&ADD)) {
            self.view = View::Streams;
            self.form = Some(MonitorForm::new_channel(
                self.settings.default_output_dir.clone(),
            ));
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SETTINGS)) {
            self.view = View::Settings;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&REFRESH)) {
            self.reload_rows();
            self.status = "Refreshed.".into();
        }

        // Row-targeted keys only fire on the channel list when not typing.
        if self.view == View::Streams && !ctx.egui_wants_keyboard_input() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete)) {
                if let Some(id) = self.selected_monitor {
                    if let Some(row) = self.rows.iter().find(|r| r.monitor.id == id) {
                        self.confirm_delete = Some((id, row.channel.name.clone()));
                    }
                }
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                if let Some(id) = self.selected_monitor {
                    if let Some(idx) = self.rows.iter().position(|r| r.monitor.id == id) {
                        self.form = Some(MonitorForm::from_existing(&self.rows[idx]));
                    }
                }
            }
        }
    }

    /// Modal confirmation for deleting a monitor (the only destructive action).
    fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        egui::Window::new("Delete monitor")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Delete this capture instance for “{name}”?"));
                ui.label("Removes the monitor and its settings. Recorded files are kept.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        do_delete = true;
                    }
                    if ui.button("Cancel").clicked() {
                        do_cancel = true;
                    }
                });
            });

        if do_delete {
            // Stop a running capture first so the process isn't orphaned when its
            // history row is cascade-deleted.
            if self.core.active.lock().unwrap().contains_key(&id) {
                self.core.manual(ManualCommand::Stop(id));
            }
            // The channel container is left in place even if this was its last
            // instance (you can add another instance to it).
            match self.core.store.delete_monitor(id) {
                Ok(()) => self.status = "Instance deleted.".into(),
                Err(e) => self.status = format!("Error: {e}"),
            }
            if self.selected_monitor == Some(id) {
                self.selected_monitor = None;
            }
            self.confirm_delete = None;
            self.reload_rows();
        } else if do_cancel || !open {
            self.confirm_delete = None;
        }
    }

    /// Modal confirmation for deleting a whole channel (and all its instances +
    /// their history rows; recorded files are kept).
    fn confirm_delete_channel_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete_channel.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        egui::Window::new("Delete channel")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Delete the channel “{name}” and all its instances?"));
                ui.label("Removes every instance and its history. Recorded files are kept.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        do_delete = true;
                    }
                    if ui.button("Cancel").clicked() {
                        do_cancel = true;
                    }
                });
            });

        if do_delete {
            // Stop any of this channel's instances that are recording, so no
            // capture is left running after its rows are cascade-deleted.
            let active: std::collections::HashSet<i64> =
                self.core.active.lock().unwrap().keys().copied().collect();
            for mid in self
                .rows
                .iter()
                .filter(|r| r.channel.id == id && active.contains(&r.monitor.id))
                .map(|r| r.monitor.id)
                .collect::<Vec<_>>()
            {
                self.core.manual(ManualCommand::Stop(mid));
            }
            match self.core.store.delete_channel(id) {
                Ok(()) => self.status = "Channel deleted.".into(),
                Err(e) => self.status = format!("Error: {e}"),
            }
            self.confirm_delete_channel = None;
            self.reload_rows();
        } else if do_cancel || !open {
            self.confirm_delete_channel = None;
        }
    }

    /// Modal for creating a new channel container or renaming an existing one.
    fn channel_form_window(&mut self, ctx: &egui::Context) {
        if self.channel_form.is_none() {
            return;
        }
        let renaming = self.channel_form.as_ref().unwrap().id.is_some();
        let title = if renaming { "Rename channel" } else { "Add channel" };
        let mut open = true;
        let mut do_save = false;
        let mut do_cancel = false;

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                let f = self.channel_form.as_mut().unwrap();
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut f.name);
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
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        do_save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        do_cancel = true;
                    }
                });
            });

        if do_save {
            let f = self.channel_form.as_ref().unwrap();
            let name = f.name.trim().to_string();
            if name.is_empty() {
                self.status = "Name is required.".into();
            } else {
                let res = match f.id {
                    Some(id) => self.core.store.rename_channel(id, &name),
                    None => self.core.store.create_container(&name).map(|_| ()),
                };
                match res {
                    Ok(()) => {
                        self.status = "Saved.".into();
                        self.channel_form = None;
                        self.reload_rows();
                    }
                    Err(e) => self.status = format!("Error: {e}"),
                }
            }
        } else if do_cancel || !open {
            self.channel_form = None;
        }
    }

    /// The "Videos" tab: a list of on-demand downloads with an always-visible
    /// "paste a URL" form pinned to the bottom.
    fn videos_view(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("video_add_panel")
            .resizable(true)
            .default_size(300.0)
            .show_inside(ui, |ui| {
                // Per-platform defaults on the right; download form on the left.
                egui::Panel::right("video_defaults_panel")
                    .resizable(true)
                    .default_size(360.0)
                    .show_inside(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("video_defaults_scroll")
                            .show(ui, |ui| {
                                self.video_defaults_editor(ui);
                            });
                    });
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("video_form_scroll")
                        .show(ui, |ui| {
                            self.video_add_form(ui);
                        });
                });
            });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.videos_list(ui);
        });
    }

    /// Collapsible per-platform download defaults editor (Twitch / YouTube /
    /// Kick / Generic). Edits persist immediately; the download form below
    /// pre-fills from these per detected platform.
    fn video_defaults_editor(&mut self, ui: &mut egui::Ui) {
        let mut dirty = false;
        // Borrow the defaults (not `self`) so the nested egui closures don't
        // alias `self`; persist afterwards.
        let defs = &mut self.download_defaults;

        ui.add_space(6.0);
        ui.strong("⚙  Per-platform defaults");
        ui.label(
            egui::RichText::new(
                "Downloads pre-fill from these by platform; override per download.",
            )
            .small()
            .color(egui::Color32::from_gray(0x90)),
        );
        ui.add_space(4.0);

        for platform in Platform::ALL {
            egui::CollapsingHeader::new(platform.label())
                .id_salt(("dl_def", platform.as_str()))
                .show(ui, |ui| {
                    let d = defs.get_mut(platform);
                    egui::Grid::new(("dl_def_grid", platform.as_str()))
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Tool").on_hover_text(d.tool.tooltip());
                            egui::ComboBox::from_id_salt(("dl_tool", platform.as_str()))
                                .selected_text(d.tool.label())
                                .show_ui(ui, |ui| {
                                    for t in Tool::ALL {
                                        if ui
                                            .selectable_value(&mut d.tool, t, t.label())
                                            .on_hover_text(t.tooltip())
                                            .changed()
                                        {
                                            dirty = true;
                                        }
                                    }
                                });
                            ui.end_row();

                            ui.label("Quality");
                            if ui.text_edit_singleline(&mut d.quality).changed() {
                                dirty = true;
                            }
                            ui.end_row();

                            ui.label("Auth");
                            egui::ComboBox::from_id_salt(("dl_auth", platform.as_str()))
                                .selected_text(d.auth_kind.label())
                                .show_ui(ui, |ui| {
                                    for k in AuthKind::ALL {
                                        if ui
                                            .selectable_value(&mut d.auth_kind, k, k.label())
                                            .changed()
                                        {
                                            dirty = true;
                                        }
                                    }
                                });
                            ui.end_row();

                            match d.auth_kind {
                                AuthKind::CookiesBrowser => {
                                    ui.label("Browser");
                                    if ui
                                        .text_edit_singleline(&mut d.auth_value)
                                        .on_hover_text("Browser, or browser:profile — e.g. firefox:dmrf6eed.YouTube (the folder under …/Firefox/Profiles, or an absolute path)")
                                        .changed()
                                    {
                                        dirty = true;
                                    }
                                    ui.end_row();
                                }
                                AuthKind::CookiesFile => {
                                    ui.label("Cookies file");
                                    ui.horizontal(|ui| {
                                        if ui.text_edit_singleline(&mut d.auth_value).changed() {
                                            dirty = true;
                                        }
                                        if ui.button("Browse…").clicked() {
                                            if let Some(p) = browse_file(&d.auth_value) {
                                                d.auth_value = p;
                                                dirty = true;
                                            }
                                        }
                                    });
                                    ui.end_row();
                                }
                                AuthKind::Token => {
                                    ui.label("Auth token");
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(&mut d.auth_value)
                                                .password(true),
                                        )
                                        .changed()
                                    {
                                        dirty = true;
                                    }
                                    ui.end_row();
                                }
                                AuthKind::Inherit | AuthKind::Disabled => {}
                            }

                            ui.label("Output folder");
                            ui.horizontal(|ui| {
                                if ui.text_edit_singleline(&mut d.output_dir).changed() {
                                    dirty = true;
                                }
                                if ui.button("Browse…").clicked() {
                                    if let Some(p) = browse_folder(&d.output_dir) {
                                        d.output_dir = p;
                                        dirty = true;
                                    }
                                }
                            });
                            ui.end_row();

                            ui.label("Filename template");
                            if ui.text_edit_singleline(&mut d.filename_template).changed() {
                                dirty = true;
                            }
                            ui.end_row();

                            ui.label("Extra args");
                            if ui.text_edit_singleline(&mut d.extra_args).changed() {
                                dirty = true;
                            }
                            ui.end_row();
                        });
                });
        }
        if dirty {
            self.persist_download_defaults();
        }
    }

    fn videos_list(&mut self, ui: &mut egui::Ui) {
        // Reflect background progress whenever the list is shown.
        self.reload_videos();

        if self.videos.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No videos yet.");
                ui.label("Paste a URL in the box below to download a video or VOD.");
            });
            return;
        }

        let mut to_stop: Option<i64> = None;
        let mut to_retry: Option<i64> = None;
        let mut to_delete: Option<i64> = None;
        let any_active = self.videos.iter().any(|v| v.is_active());
        // Snapshot live download progress (video_id -> 0.0..=1.0) and speed
        // (video_id -> bytes/sec) for the progress bar + Speed column.
        let progress: std::collections::HashMap<i64, f32> =
            self.core.video_progress.lock().unwrap().clone();
        let speed: std::collections::HashMap<i64, f64> =
            self.core.video_speed.lock().unwrap().clone();

        // Build the sort/filter model and take the persisted sort/filter state
        // into locals (written back after the table is drawn).
        let model: Vec<Vec<Cell>> = self.videos.iter().map(|v| video_cells(v, &speed)).collect();
        let mut sort = self.videos_sort;
        let mut filters = self.videos_filters.clone();
        if filters.len() != VIDEO_COLS {
            filters = vec![String::new(); VIDEO_COLS];
        }

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Non-selectable labels so a right-click reaches the row (menu).
                ui.style_mut().interaction.selectable_labels = false;
                let table = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::auto().at_least(180.0)) // video
                    .column(Column::auto().at_least(110.0)) // channel
                    .column(Column::auto().at_least(86.0)) // platform
                    .column(Column::auto().at_least(72.0)) // tool
                    .column(Column::auto().at_least(96.0)) // status
                    .column(Column::auto().at_least(82.0)) // speed
                    .column(Column::auto().at_least(72.0)) // size
                    .column(Column::auto().at_least(80.0)) // added
                    .column(Column::auto().at_least(160.0)) // file
                    .column(Column::remainder().at_least(150.0)) // actions
                    .header(46.0, |mut header| {
                        let titles = [
                            "Video", "Channel", "Platform", "Tool", "Status", "Speed", "Size",
                            "Added", "File",
                        ];
                        for (i, t) in titles.into_iter().enumerate() {
                            header.col(|ui| {
                                sort_filter_header(ui, i, t, true, &mut sort, &mut filters[i]);
                            });
                        }
                        header.col(|ui| {
                            ui.strong("Actions");
                        });
                    });
                table.body(|mut body| {
                        let order = ordered_rows(&model, &sort, &filters);
                        for &vi in &order {
                            let v = &self.videos[vi];
                            body.row(24.0, |mut tr| {
                                // Reusable menu body (a `Fn`), attached to the row and
                                // each inline action button so right-clicking anywhere
                                // on the row opens it. Open/copy are handled inline;
                                // self-mutating picks go through `menu_pick`.
                                let mut menu_pick: Option<VideoMenuChoice> = None;
                                let add_menu =
                                    |ui: &mut egui::Ui, pick: &mut Option<VideoMenuChoice>| {
                                        ui.set_min_width(180.0);
                                        if v.is_active() {
                                            if ui.button("⏹  Stop download").clicked() {
                                                *pick = Some(VideoMenuChoice::Stop);
                                                ui.close();
                                            }
                                        } else if ui.button("↻  Retry download").clicked() {
                                            *pick = Some(VideoMenuChoice::Retry);
                                            ui.close();
                                        }
                                        ui.separator();
                                        let file_ok = !v.output_path.is_empty()
                                            && std::path::Path::new(&v.output_path).is_file();
                                        if ui
                                            .add_enabled(file_ok, egui::Button::new("▶  Open file"))
                                            .clicked()
                                        {
                                            crate::platform::open_path(std::path::Path::new(
                                                &v.output_path,
                                            ));
                                            ui.close();
                                        }
                                        let dir_ok = std::path::Path::new(&v.output_dir).is_dir();
                                        if ui
                                            .add_enabled(
                                                dir_ok,
                                                egui::Button::new("📂  Open folder"),
                                            )
                                            .clicked()
                                        {
                                            crate::platform::open_path(std::path::Path::new(
                                                &v.output_dir,
                                            ));
                                            ui.close();
                                        }
                                        if ui.button("🔗  Copy URL").clicked() {
                                            ui.ctx().copy_text(v.url.clone());
                                            ui.close();
                                        }
                                        if ui
                                            .add_enabled(
                                                !v.output_path.is_empty(),
                                                egui::Button::new("📋  Copy file path"),
                                            )
                                            .clicked()
                                        {
                                            ui.ctx().copy_text(v.output_path.clone());
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("🗑  Delete from list").clicked() {
                                            *pick = Some(VideoMenuChoice::Delete);
                                            ui.close();
                                        }
                                    };

                                tr.col(|ui| {
                                    let label = if v.title.trim().is_empty() {
                                        v.url.as_str()
                                    } else {
                                        v.title.as_str()
                                    };
                                    ui.label(label).on_hover_text(&v.url);
                                });
                                tr.col(|ui| {
                                    if !v.channel.is_empty() {
                                        ui.label(&v.channel).on_hover_text(&v.channel);
                                    }
                                });
                                tr.col(|ui| {
                                    platform_badge(ui, v.platform);
                                    ui.label(v.platform.label());
                                });
                                tr.col(|ui| {
                                    ui.label(v.tool.label());
                                });
                                tr.col(|ui| match progress.get(&v.id) {
                                    Some(&f) if v.status == "downloading" => {
                                        ui.add(
                                            egui::ProgressBar::new(f)
                                                .desired_width(84.0)
                                                .text(format!("{:.0}%", f * 100.0)),
                                        );
                                    }
                                    _ => {
                                        ui.colored_label(video_status_color(&v.status), &v.status);
                                    }
                                });
                                tr.col(|ui| {
                                    if v.status == "downloading" {
                                        if let Some(&bps) = speed.get(&v.id) {
                                            if bps > 0.0 {
                                                ui.label(fmt_speed(bps));
                                            }
                                        }
                                    }
                                });
                                tr.col(|ui| {
                                    if v.bytes > 0 {
                                        ui.label(fmt_bytes(v.bytes));
                                    }
                                });
                                tr.col(|ui| {
                                    ui.label(fmt_date(v.created_at));
                                });
                                tr.col(|ui| {
                                    if !v.output_path.is_empty() {
                                        ui.label(&v.output_path).on_hover_text(&v.output_path);
                                    }
                                });
                                tr.col(|ui| {
                                    ui.push_id(v.id, |ui| {
                                        let mut btns: Vec<egui::Response> = Vec::with_capacity(5);
                                        if v.is_active() {
                                            let b =
                                                ui.small_button("⏹").on_hover_text("Stop download");
                                            if b.clicked() {
                                                to_stop = Some(v.id);
                                            }
                                            btns.push(b);
                                        } else {
                                            let b = ui
                                                .small_button("↻")
                                                .on_hover_text("Retry download");
                                            if b.clicked() {
                                                to_retry = Some(v.id);
                                            }
                                            btns.push(b);
                                        }
                                        let dir_ok = std::path::Path::new(&v.output_dir).is_dir();
                                        let b = ui
                                            .add_enabled(dir_ok, egui::Button::new("📂").small())
                                            .on_hover_text("Open output folder");
                                        if b.clicked() {
                                            crate::platform::open_path(std::path::Path::new(
                                                &v.output_dir,
                                            ));
                                        }
                                        btns.push(b);
                                        let file_ok = !v.output_path.is_empty()
                                            && std::path::Path::new(&v.output_path).is_file();
                                        let b = ui
                                            .add_enabled(file_ok, egui::Button::new("▶").small())
                                            .on_hover_text("Open file");
                                        if b.clicked() {
                                            crate::platform::open_path(std::path::Path::new(
                                                &v.output_path,
                                            ));
                                        }
                                        btns.push(b);
                                        let b = ui.small_button("📋").on_hover_text("Copy URL");
                                        if b.clicked() {
                                            ui.ctx().copy_text(v.url.clone());
                                        }
                                        btns.push(b);
                                        let b = ui
                                            .small_button("🗑")
                                            .on_hover_text("Delete from list (keeps the file)");
                                        if b.clicked() {
                                            to_delete = Some(v.id);
                                        }
                                        btns.push(b);
                                        // Buttons swallow the right-click, so give each
                                        // the row menu too.
                                        for b in &btns {
                                            b.context_menu(|ui| add_menu(ui, &mut menu_pick));
                                        }
                                    });
                                });

                                // Right-click anywhere on the row opens the menu.
                                tr.response()
                                    .context_menu(|ui| add_menu(ui, &mut menu_pick));

                                match menu_pick {
                                    Some(VideoMenuChoice::Stop) => to_stop = Some(v.id),
                                    Some(VideoMenuChoice::Retry) => to_retry = Some(v.id),
                                    Some(VideoMenuChoice::Delete) => to_delete = Some(v.id),
                                    None => {}
                                }
                            });
                        }
                    });
            });
        self.videos_sort = sort;
        self.videos_filters = filters;

        // Tick while a download is queued/running so the progress bar, status and
        // size update live (a bit faster than 1s for a smoother bar).
        if any_active {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
        }

        if let Some(id) = to_stop {
            self.core.manual(ManualCommand::StopVideo(id));
            self.status = "Stopping download…".into();
        }
        if let Some(id) = to_retry {
            match self.core.store.reset_video_for_retry(id) {
                Ok(()) => {
                    self.core.manual(ManualCommand::StartVideo(id));
                    self.status = "Re-queued download.".into();
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
            self.reload_videos();
        }
        if let Some(id) = to_delete {
            if let Err(e) = self.core.store.delete_video(id) {
                self.status = format!("Error: {e}");
            }
            self.reload_videos();
        }
    }

    /// The bottom "paste a URL + settings + Download" form on the Videos tab.
    fn video_add_form(&mut self, ui: &mut egui::Ui) {
        let platform = Platform::detect(&self.video_form.url);

        // Re-fill the form from this platform's saved defaults whenever the
        // detected platform changes; the user can then override any field.
        if self.video_form.last_platform != Some(platform) {
            let d = self.download_defaults.get(platform).clone();
            let vf = &mut self.video_form;
            vf.tool = d.tool;
            vf.quality = d.quality;
            vf.output_dir = d.output_dir;
            vf.filename_template = d.filename_template;
            vf.extra_args = d.extra_args;
            vf.auth_override = None; // "Default (per-platform)"
            vf.auth_value = String::new();
            // Snapshot the default auth too, so a later edit to the default
            // doesn't desync from the other (already snapshotted) fields.
            vf.default_auth_kind = d.auth_kind;
            vf.default_auth_value = d.auth_value;
            vf.last_platform = Some(platform);
        }

        let mut do_download = false;
        let mut do_list_formats = false;

        {
            let vf = &mut self.video_form;

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Download a video / VOD");
                ui.label(
                    egui::RichText::new("→ MKV")
                        .small()
                        .color(egui::Color32::from_gray(0x90)),
                );
            });

            egui::Grid::new("video_form_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("URL");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut vf.url)
                                .desired_width(360.0)
                                .hint_text(
                                    "YouTube video, Twitch VOD, or any streamlink/yt-dlp URL",
                                ),
                        );
                        platform_badge(ui, platform);
                        ui.label(platform.label());
                    });
                    ui.end_row();

                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut vf.title).hint_text(
                        "optional — used for the filename (default: the title, else \"video\")",
                    ));
                    ui.end_row();

                    ui.label("Auto-detect");
                    ui.checkbox(&mut vf.auto_title, "Detect title + channel")
                        .on_hover_text(
                            "Looks up the real title and channel via yt-dlp at download time: \
                             fills the Channel column and the {title}/{channel} variables (and \
                             {name} when Name is left blank).",
                        );
                    ui.end_row();

                    ui.label("Tool").on_hover_text(vf.tool.tooltip());
                    egui::ComboBox::from_id_salt("video_tool_cb")
                        .selected_text(vf.tool.label())
                        .show_ui(ui, |ui| {
                            for t in Tool::ALL {
                                ui.selectable_value(&mut vf.tool, t, t.label())
                                    .on_hover_text(t.tooltip());
                            }
                        });
                    ui.end_row();

                    ui.label("Quality");
                    ui.add(
                        egui::TextEdit::singleline(&mut vf.quality)
                            .hint_text("best, 1080p, or a yt-dlp -f selector"),
                    );
                    ui.end_row();

                    ui.label("Auth");
                    let auth_text = match vf.auth_override {
                        None => "Default (per-platform)".to_string(),
                        Some(k) => k.label().to_string(),
                    };
                    egui::ComboBox::from_id_salt("video_auth_cb")
                        .selected_text(auth_text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut vf.auth_override,
                                None,
                                "Default (per-platform)",
                            );
                            for k in AuthKind::ALL {
                                ui.selectable_value(&mut vf.auth_override, Some(k), k.label());
                            }
                        });
                    ui.end_row();

                    match vf.auth_override {
                        Some(AuthKind::CookiesBrowser) => {
                            ui.label("Browser");
                            ui.text_edit_singleline(&mut vf.auth_value)
                                .on_hover_text("Browser, or browser:profile — e.g. firefox:dmrf6eed.YouTube (the folder under …/Firefox/Profiles, or an absolute path)");
                            ui.end_row();
                        }
                        Some(AuthKind::CookiesFile) => {
                            ui.label("Cookies file");
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(&mut vf.auth_value);
                                if ui.button("Browse…").clicked() {
                                    if let Some(p) = browse_file(&vf.auth_value) {
                                        vf.auth_value = p;
                                    }
                                }
                            });
                            ui.end_row();
                        }
                        Some(AuthKind::Token) => {
                            ui.label("Auth token");
                            ui.add(egui::TextEdit::singleline(&mut vf.auth_value).password(true))
                                .on_hover_text("Twitch OAuth token (streamlink)");
                            ui.end_row();
                        }
                        // Default (None), Global (Inherit), and None (Disabled) need no value.
                        _ => {}
                    }

                    ui.label("Output folder");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut vf.output_dir);
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = browse_folder(&vf.output_dir) {
                                vf.output_dir = p;
                            }
                        }
                    });
                    ui.end_row();

                    let tmpl_hint = "Variables: {name} {title} {channel} {date} {time} {timestamp}";
                    ui.label("Filename template").on_hover_text(tmpl_hint);
                    ui.text_edit_singleline(&mut vf.filename_template)
                        .on_hover_text(tmpl_hint);
                    ui.end_row();

                    ui.label("Extra args");
                    ui.text_edit_singleline(&mut vf.extra_args);
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let can = !vf.url.trim().is_empty();
                if ui
                    .add_enabled(can, egui::Button::new("⬇  Download"))
                    .clicked()
                {
                    do_download = true;
                }
                if ui
                    .add_enabled(can, egui::Button::new("List formats"))
                    .on_hover_text(
                        "Show available formats/qualities for this URL using the selected tool",
                    )
                    .clicked()
                {
                    do_list_formats = true;
                }
            });
            ui.add_space(6.0);
        }

        if do_download {
            self.start_video_download();
        }
        if do_list_formats {
            self.start_format_probe(ui.ctx().clone());
        }
    }

    /// Insert the form's video as a queued download and kick off the supervisor.
    fn start_video_download(&mut self) {
        let url = self.video_form.url.trim().to_string();
        if url.is_empty() {
            return;
        }
        let platform = Platform::detect(&url);
        // "Default" auth uses the snapshotted platform default; an explicit
        // choice overrides it. (All fields resolve from the same snapshot.)
        let (auth_kind, auth_value) = match self.video_form.auth_override {
            Some(kind) => (kind, self.video_form.auth_value.clone()),
            None => (
                self.video_form.default_auth_kind,
                self.video_form.default_auth_value.clone(),
            ),
        };
        let video = Video {
            id: 0,
            platform,
            url,
            title: self.video_form.title.trim().to_string(),
            channel: String::new(),
            tool: self.video_form.tool,
            quality: self.video_form.quality.clone(),
            output_dir: self.video_form.output_dir.clone(),
            filename_template: self.video_form.filename_template.clone(),
            auth_kind,
            auth_value,
            extra_args: self.video_form.extra_args.clone(),
            auto_title: self.video_form.auto_title,
            status: "queued".into(),
            output_path: String::new(),
            bytes: 0,
            exit_code: None,
            created_at: 0,
            started_at: None,
            ended_at: None,
        };
        match self.core.store.insert_video(&video) {
            Ok(id) => {
                self.core.manual(ManualCommand::StartVideo(id));
                self.status = "Queued video download.".into();
                // Clear the URL/name; re-fill defaults for the next download.
                self.video_form.url.clear();
                self.video_form.title.clear();
                self.video_form.last_platform = None;
                self.reload_videos();
            }
            Err(e) => self.status = format!("Error: {e}"),
        }
    }

    /// Probe available formats/qualities for the form's URL with the selected
    /// tool, on the async runtime; the result appears in a window.
    fn start_format_probe(&mut self, ctx: egui::Context) {
        let url = self.video_form.url.trim().to_string();
        if url.is_empty() {
            self.status = "Enter a URL first.".into();
            return;
        }
        let tool = self.video_form.tool;
        let (auth_kind, auth_value) = match self.video_form.auth_override {
            Some(kind) => (kind, self.video_form.auth_value.clone()),
            None => (
                self.video_form.default_auth_kind,
                self.video_form.default_auth_value.clone(),
            ),
        };
        let global_method = setting_or_empty(&self.core, K_DOWNLOAD_AUTH);
        let global_browser = setting_or_empty(&self.core, K_COOKIES_BROWSER);
        let auth = crate::downloader::resolve_auth_for(
            auth_kind,
            &auth_value,
            &global_method,
            &global_browser,
        );

        let probe = self.format_probe.clone();
        *probe.lock().unwrap() = FormatProbe::Running;
        self.status = "Listing formats…".into();
        self.core.rt.spawn(async move {
            let result = crate::downloader::probe_formats(tool, &url, &auth).await;
            *probe.lock().unwrap() = match result {
                Ok(s) => FormatProbe::Done(s),
                Err(e) => FormatProbe::Failed(e),
            };
            ctx.request_repaint();
        });
    }

    /// Window showing the result of a "List formats" probe.
    fn format_probe_window(&mut self, ctx: &egui::Context) {
        let probe = self.format_probe.lock().unwrap().clone();
        let (title, body, done) = match &probe {
            FormatProbe::Idle => return,
            FormatProbe::Running => ("Listing formats…", "Running…".to_string(), false),
            FormatProbe::Done(s) => ("Available formats", s.clone(), true),
            FormatProbe::Failed(e) => ("Format probe failed", e.clone(), true),
        };
        let mut open = true;
        egui::Window::new(title)
            .collapsible(true)
            .resizable(true)
            .default_size([680.0, 460.0])
            .open(&mut open)
            .show(ctx, |ui| {
                if done && ui.button("📋  Copy").clicked() {
                    ui.ctx().copy_text(body.clone());
                }
                ui.add_space(4.0);
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&body).monospace())
                                .selectable(true),
                        );
                    });
            });
        if !open {
            *self.format_probe.lock().unwrap() = FormatProbe::Idle;
        }
    }

    /// Window listing where ad breaks cause hard cuts in a take's finished file.
    /// Opened by double-clicking an Ads / Ad time cell.
    fn ad_popup_window(&mut self, ctx: &egui::Context) {
        let Some(rid) = self.ad_popup else {
            return;
        };
        let breaks = self
            .core
            .store
            .ad_breaks_for_recording(rid)
            .unwrap_or_default();
        let total: i64 = breaks.iter().map(|b| b.duration_secs).sum();
        let mut open = true;
        egui::Window::new("Ad breaks — cut points")
            .collapsible(false)
            .resizable(true)
            .default_size([360.0, 240.0])
            .open(&mut open)
            .show(ctx, |ui| {
                if breaks.is_empty() {
                    ui.label("No ad breaks recorded for this take.");
                    return;
                }
                ui.label(format!(
                    "{} ad break(s), {} total. Each is a hard cut in the recorded file \
                     (streamlink filters ad segments out).",
                    breaks.len(),
                    fmt_duration(total),
                ));
                ui.add_space(6.0);
                let lines = ad_cut_lines(&breaks);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for line in &lines {
                            ui.label(egui::RichText::new(line).monospace());
                        }
                    });
                ui.add_space(6.0);
                if ui.button("📋  Copy").clicked() {
                    ui.ctx().copy_text(lines.join("\n"));
                }
            });
        if !open {
            self.ad_popup = None;
        }
    }

    fn channels_view(&mut self, ui: &mut egui::Ui) {
        if self.channels.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No channels yet.");
                ui.label("Click “Add stream” to add a channel + its first instance, or “Add channel” for an empty container.");
            });
            return;
        }

        // Self-mutating actions, collected during rendering and applied after the
        // table closure (which only borrows `self` immutably).
        let mut acts = RowActions::default();
        let mut toggle_channel: Option<i64> = None;
        let mut toggle_instance: Option<i64> = None;
        let mut toggle_stream: Option<String> = None;
        let mut open_path: Option<std::path::PathBuf> = None;
        let mut copy_text: Option<String> = None;
        let mut delete_recording: Option<i64> = None;
        // Container-level actions.
        let mut toggle_channel_enabled: Option<(i64, bool)> = None; // set all instances
        let mut rename_channel: Option<i64> = None;
        let mut delete_channel: Option<(i64, String)> = None;

        let selected_monitor = self.selected_monitor;
        let now = crate::models::now_unix();
        let any_active = self
            .rows
            .iter()
            .any(|r| r.last_recording_status.as_deref() == Some("recording"));

        // Build one entry per channel container (including empty ones), attaching
        // its instance rows (indices into self.rows).
        struct ChanEntry {
            channel: Channel,
            rows: Vec<usize>,
        }
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

        // Lazily load + cache recordings for currently-expanded monitors, then
        // group each monitor's takes into streams.
        // A channel always shows its instances when expanded; an instance shows
        // its stream history when *it* is expanded — so we only need recordings
        // for expanded instances inside expanded channels.
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
        for &mid in &expanded_monitors {
            if !self.rec_cache.contains_key(&mid) {
                let recs = self
                    .core
                    .store
                    .recordings_for_monitor(mid)
                    .unwrap_or_default();
                self.rec_cache.insert(mid, recs);
            }
        }
        let groups: HashMap<i64, Vec<StreamGroup>> = expanded_monitors
            .iter()
            .map(|&mid| {
                let recs = self.rec_cache.get(&mid).map(Vec::as_slice).unwrap_or(&[]);
                (mid, group_recordings(recs))
            })
            .collect();

        // Per-recording ad-break detail (offsets) for the cut-list tooltips on
        // expanded history rows. Only fetched for takes that actually had ads, so
        // it stays bounded by what's currently expanded.
        let mut ad_breaks: HashMap<i64, Vec<AdBreak>> = HashMap::new();
        for &mid in &expanded_monitors {
            if let Some(recs) = self.rec_cache.get(&mid) {
                for r in recs.iter().filter(|r| r.ad_count > 0) {
                    let v = self
                        .core
                        .store
                        .ad_breaks_for_recording(r.id)
                        .unwrap_or_default();
                    ad_breaks.insert(r.id, v);
                }
            }
        }
        // Collected in the table closure (which borrows `self` immutably), applied
        // afterwards: a double-click on an ad cell opens that take's cut-list popup.
        let mut open_ad_popup: Option<i64> = None;

        // Snapshot expansion state for read-only use inside the table closure.
        let exp_channels = self.expanded_channels.clone();
        let exp_instances = self.expanded_instances.clone();
        let exp_streams = self.expanded_streams.clone();

        // Channel-level sort/filter model (one entry per top-level channel row).
        let model: Vec<Vec<Cell>> = chan_entries
            .iter()
            .map(|e| {
                let mons: Vec<&MonitorWithChannel> =
                    e.rows.iter().map(|&i| &self.rows[i]).collect();
                channel_cells(&e.channel, &mons, now)
            })
            .collect();
        let mut sort = self.streams_sort;
        let mut filters = self.streams_filters.clone();
        if filters.len() != STREAM_COLS {
            filters = vec![String::new(); STREAM_COLS];
        }

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
                let table = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    // Make rows sense clicks so they can be selected and carry a
                    // right-click context menu.
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::auto().at_least(40.0)) // enabled
                    .column(Column::auto().at_least(100.0)) // name
                    .column(Column::auto().at_least(90.0)) // platform
                    .column(Column::auto().at_least(72.0)) // tool
                    .column(Column::auto().at_least(80.0)) // method
                    .column(Column::auto().at_least(52.0)) // interval
                    .column(Column::auto().at_least(104.0)) // last poll
                    .column(Column::auto().at_least(64.0)) // state
                    .column(Column::auto().at_least(112.0)) // went live
                    .column(Column::auto().at_least(104.0)) // started on
                    .column(Column::auto().at_least(58.0)) // lost time
                    .column(Column::auto().at_least(58.0)) // duration
                    .column(Column::auto().at_least(44.0)) // ads
                    .column(Column::auto().at_least(58.0)) // ad time
                    .column(Column::auto().at_least(64.0)) // ad-free
                    .column(Column::auto().at_least(80.0)) // added
                    .column(Column::remainder().at_least(140.0)) // actions
                    .header(46.0, |mut header| {
                        let titles = [
                            "On",
                            "Name",
                            "Platform",
                            "Tool",
                            "Detection",
                            "Every",
                            "Last poll",
                            "State",
                            "Went Live",
                            "Started On",
                            "Lost time",
                            "Duration",
                            "Ads",
                            "Ad time",
                            "Ad-free",
                            "Added",
                        ];
                        for (i, t) in titles.into_iter().enumerate() {
                            header.col(|ui| {
                                sort_filter_header(ui, i, t, true, &mut sort, &mut filters[i]);
                            });
                        }
                        header.col(|ui| {
                            ui.strong("Actions");
                        });
                    });
                table.body(|mut body| {
                    let order = ordered_rows(&model, &sort, &filters);

                    // Flatten the channel -> (instance) -> stream -> take tree into
                    // the rows currently visible (respecting expansion state).
                    #[derive(Clone, Copy)]
                    enum Vis {
                        Channel(usize),
                        Instance { row: usize, depth: usize },
                        Stream { mid: i64, gi: usize, depth: usize },
                        Take { mid: i64, gi: usize, ti: usize, depth: usize },
                    }
                    let mut vis: Vec<Vis> = Vec::new();
                    for &ci in &order {
                        let e = &chan_entries[ci];
                        vis.push(Vis::Channel(ci));
                        if !exp_channels.contains(&e.channel.id) {
                            continue;
                        }
                        // Channel container -> its instances -> each instance's
                        // stream history -> takes.
                        for &ri in &e.rows {
                            let mid = self.rows[ri].monitor.id;
                            vis.push(Vis::Instance { row: ri, depth: 1 });
                            if !exp_instances.contains(&mid) {
                                continue;
                            }
                            if let Some(grps) = groups.get(&mid) {
                                for (gi, g) in grps.iter().enumerate() {
                                    vis.push(Vis::Stream { mid, gi, depth: 2 });
                                    if g.takes.len() > 1 && exp_streams.contains(&g.key) {
                                        for ti in 0..g.takes.len() {
                                            vis.push(Vis::Take { mid, gi, ti, depth: 3 });
                                        }
                                    }
                                }
                            }
                        }
                    }

                    for v in &vis {
                        match *v {
                            Vis::Channel(ci) => {
                                let e = &chan_entries[ci];
                                let ch = &e.channel;
                                let cid = ch.id;
                                let mons: Vec<&MonitorWithChannel> =
                                    e.rows.iter().map(|&ri| &self.rows[ri]).collect();
                                let ninst = mons.len();
                                let any_rec = mons.iter().any(|m| {
                                    self.core.active.lock().unwrap().contains_key(&m.monitor.id)
                                });
                                let all_enabled =
                                    ninst > 0 && mons.iter().all(|m| m.monitor.enabled);
                                let expanded = exp_channels.contains(&cid);
                                let plat = channel_platform(&mons);
                                let last_poll = mons
                                    .iter()
                                    .filter_map(|m| m.monitor.last_checked_at)
                                    .max()
                                    .unwrap_or(0);
                                // Latest activity across instances drives the time columns.
                                let primary = mons
                                    .iter()
                                    .copied()
                                    .max_by_key(|m| m.last_recording_started.unwrap_or(0));
                                let rec = primary.map(|m| recording_cells(m, now));
                                let ads = primary.map(|m| {
                                    (m.last_recording_ad_count, m.last_recording_ad_secs)
                                });
                                let n_adfree = mons
                                    .iter()
                                    .filter(|m| {
                                        ad_free_status(m.monitor.ad_free, m.ad_free_sub).is_some()
                                    })
                                    .count();
                                let ad_free = ad_free_summary(n_adfree, ninst);
                                body.row(24.0, |mut tr| {
                                    tr.set_selected(any_rec);
                                    let mut disc = false;
                                    tr.col(|ui| {
                                        let mut on = all_enabled;
                                        let cb = ui
                                            .add_enabled(ninst > 0, egui::Checkbox::new(&mut on, ""))
                                            .on_hover_text("Enable/disable all of this channel's instances");
                                        if cb.changed() {
                                            toggle_channel_enabled = Some((cid, on));
                                        }
                                    });
                                    tr.col(|ui| {
                                        disc = tree_name(
                                            ui, 0, ninst > 0, expanded,
                                            egui::RichText::new(&ch.name).strong(),
                                        );
                                    });
                                    tr.col(|ui| match plat {
                                        Some(p) => {
                                            platform_badge(ui, p);
                                            ui.label(p.label());
                                        }
                                        None if ninst > 0 => {
                                            ui.weak("mixed");
                                        }
                                        None => {}
                                    });
                                    tr.col(|ui| {
                                        ui.weak(if ninst == 1 {
                                            "1 instance".to_string()
                                        } else {
                                            format!("{ninst} instances")
                                        });
                                    });
                                    tr.col(|_ui| {}); // detection
                                    tr.col(|_ui| {}); // every
                                    tr.col(|ui| {
                                        ui.label(fmt_datetime_short(last_poll));
                                    });
                                    tr.col(|ui| {
                                        if any_rec {
                                            ui.colored_label(
                                                rec_status_color("recording"),
                                                "recording",
                                            );
                                        }
                                    });
                                    tr.col(|ui| {
                                        if let Some(r) = &rec {
                                            ui.label(&r.went_live);
                                        }
                                    });
                                    tr.col(|ui| {
                                        if let Some(r) = &rec {
                                            ui.label(&r.started_on);
                                        }
                                    });
                                    tr.col(|ui| {
                                        if let Some(r) = &rec {
                                            ui.label(&r.lost);
                                        }
                                    });
                                    tr.col(|ui| {
                                        if let Some(r) = &rec {
                                            ui.label(&r.duration);
                                        }
                                    });
                                    tr.col(|ui| {
                                        if let Some((c, s)) = ads {
                                            if c > 0 {
                                                ui.label(fmt_ad_count(c))
                                                    .on_hover_text(ad_tooltip(c, s, None));
                                            }
                                        }
                                    });
                                    tr.col(|ui| {
                                        if let Some((c, s)) = ads {
                                            if s > 0 {
                                                ui.label(fmt_ad_time(s))
                                                    .on_hover_text(ad_tooltip(c, s, None));
                                            }
                                        }
                                    });
                                    tr.col(|ui| {
                                        if !ad_free.0.is_empty() {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(0x57, 0xc7, 0x57),
                                                ad_free.0,
                                            );
                                        }
                                    });
                                    tr.col(|ui| {
                                        ui.label(fmt_date(ch.created_at));
                                    });
                                    tr.col(|ui| {
                                        ui.push_id(cid, |ui| {
                                            if ui
                                                .small_button("➕")
                                                .on_hover_text("Add an instance to this channel")
                                                .clicked()
                                            {
                                                acts.add_instance = Some(cid);
                                            }
                                            if ui
                                                .small_button("✏")
                                                .on_hover_text("Rename channel")
                                                .clicked()
                                            {
                                                rename_channel = Some(cid);
                                            }
                                            if ui
                                                .small_button("🗑")
                                                .on_hover_text("Delete channel and all its instances")
                                                .clicked()
                                            {
                                                delete_channel = Some((cid, ch.name.clone()));
                                            }
                                        });
                                    });
                                    tr.response().context_menu(|ui| {
                                        ui.set_min_width(170.0);
                                        if ui.button("➕  Add instance").clicked() {
                                            acts.add_instance = Some(cid);
                                            ui.close();
                                        }
                                        if ui.button("✏  Rename channel").clicked() {
                                            rename_channel = Some(cid);
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("🗑  Delete channel").clicked() {
                                            delete_channel = Some((cid, ch.name.clone()));
                                            ui.close();
                                        }
                                    });
                                    if disc {
                                        toggle_channel = Some(cid);
                                    }
                                });
                            }
                            Vis::Instance { row: ri, depth } => {
                                let row = &self.rows[ri];
                                let recording = self
                                    .core
                                    .active
                                    .lock()
                                    .unwrap()
                                    .contains_key(&row.monitor.id);
                                let is_selected = selected_monitor == Some(row.monitor.id);
                                let has_hist = row.recording_count > 0;
                                let expanded = exp_instances.contains(&row.monitor.id);
                                let mid = row.monitor.id;
                                body.row(24.0, |mut tr| {
                                    if render_instance_row(
                                        &mut tr, row, now, recording, is_selected, depth, has_hist,
                                        expanded, &mut acts,
                                    ) {
                                        toggle_instance = Some(mid);
                                    }
                                });
                            }
                            Vis::Stream { mid, gi, depth } => {
                                let g = &groups[&mid][gi];
                                let has_takes = g.takes.len() > 1;
                                let expanded = exp_streams.contains(&g.key);
                                let when = fmt_went_live(g.went_live_at, g.went_live_approx);
                                let label = if when.is_empty() {
                                    format!("🎬 {}", fmt_datetime_short(g.started_at()))
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
                                let ad_count = g.ad_count();
                                let ad_secs = g.ad_secs();
                                // A single-take stream carries the cut detail on its
                                // one take; multi-take streams show per-take cuts when
                                // expanded.
                                let ad_rec =
                                    if g.takes.len() == 1 { Some(g.takes[0].id) } else { None };
                                body.row(24.0, |mut tr| {
                                    let mut disc = false;
                                    tr.col(|_ui| {});
                                    tr.col(|ui| {
                                        disc = tree_name(
                                            ui, depth, has_takes, expanded,
                                            egui::RichText::new(label.clone()),
                                        );
                                        if has_takes {
                                            ui.weak(format!("· {} takes", g.takes.len()));
                                        }
                                    });
                                    tr.col(|_ui| {}); // platform
                                    tr.col(|_ui| {}); // tool
                                    tr.col(|_ui| {}); // detection
                                    tr.col(|_ui| {}); // every
                                    tr.col(|_ui| {}); // last poll
                                    tr.col(|ui| {
                                        ui.colored_label(rec_status_color(g.status()), g.status());
                                    });
                                    tr.col(|ui| {
                                        ui.label(fmt_went_live(g.went_live_at, g.went_live_approx));
                                    });
                                    tr.col(|ui| {
                                        ui.label(fmt_datetime_short(g.started_at()));
                                    });
                                    tr.col(|ui| {
                                        if let Some(l) = g.lost_secs() {
                                            ui.label(fmt_duration(l));
                                        }
                                    });
                                    tr.col(|ui| {
                                        ui.label(fmt_duration(g.captured_secs(now))).on_hover_text(
                                            format!(
                                                "{} captured across {} take(s) · span {}",
                                                fmt_bytes(g.total_bytes()),
                                                g.takes.len(),
                                                fmt_duration(span),
                                            ),
                                        );
                                    });
                                    tr.col(|ui| {
                                        if ad_count > 0 {
                                            let det = ad_rec.and_then(|id| ad_breaks.get(&id));
                                            let resp = ui
                                                .add(
                                                    egui::Label::new(fmt_ad_count(ad_count))
                                                        .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text(ad_tooltip(ad_count, ad_secs, det));
                                            if ad_rec.is_some() && resp.double_clicked() {
                                                open_ad_popup = ad_rec;
                                            }
                                        }
                                    });
                                    tr.col(|ui| {
                                        if ad_secs > 0 {
                                            let det = ad_rec.and_then(|id| ad_breaks.get(&id));
                                            let resp = ui
                                                .add(
                                                    egui::Label::new(fmt_ad_time(ad_secs))
                                                        .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text(ad_tooltip(ad_count, ad_secs, det));
                                            if ad_rec.is_some() && resp.double_clicked() {
                                                open_ad_popup = ad_rec;
                                            }
                                        }
                                    });
                                    tr.col(|_ui| {}); // ad-free (n/a per stream)
                                    tr.col(|_ui| {}); // added
                                    tr.col(|ui| {
                                        let ok = dir.as_ref().map(|d| d.is_dir()).unwrap_or(false);
                                        if ui
                                            .add_enabled(ok, egui::Button::new("📂").small())
                                            .on_hover_text("Open folder")
                                            .clicked()
                                        {
                                            open_path = dir.clone();
                                        }
                                    });
                                    if disc {
                                        toggle_stream = Some(g.key.clone());
                                    }
                                });
                            }
                            Vis::Take { mid, gi, ti, depth } => {
                                let t = &groups[&mid][gi].takes[ti];
                                let dir = std::path::Path::new(&t.output_path)
                                    .parent()
                                    .map(|p| p.to_path_buf());
                                body.row(24.0, |mut tr| {
                                    tr.col(|_ui| {});
                                    tr.col(|ui| {
                                        tree_name(
                                            ui, depth, false, false,
                                            egui::RichText::new(format!("Take {}", ti + 1)).weak(),
                                        );
                                    });
                                    tr.col(|_ui| {}); // platform
                                    tr.col(|_ui| {}); // tool
                                    tr.col(|_ui| {}); // detection
                                    tr.col(|_ui| {}); // every
                                    tr.col(|_ui| {}); // last poll
                                    tr.col(|ui| {
                                        let resp =
                                            ui.colored_label(rec_status_color(&t.status), &t.status);
                                        if let Some(code) = t.exit_code {
                                            resp.on_hover_text(format!("exit code {code}"));
                                        }
                                    });
                                    tr.col(|_ui| {});
                                    tr.col(|ui| {
                                        ui.label(fmt_datetime_short(t.started_at));
                                    });
                                    tr.col(|ui| {
                                        if let Some(l) = t.lost_secs {
                                            ui.label(fmt_duration(l));
                                        }
                                    });
                                    tr.col(|ui| {
                                        let d = ui.label(fmt_duration(t.duration_secs(now)));
                                        if t.bytes > 0 {
                                            d.on_hover_text(fmt_bytes(t.bytes));
                                        }
                                    });
                                    tr.col(|ui| {
                                        if t.ad_count > 0 {
                                            let det = ad_breaks.get(&t.id);
                                            let resp = ui
                                                .add(
                                                    egui::Label::new(fmt_ad_count(t.ad_count))
                                                        .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text(ad_tooltip(
                                                    t.ad_count, t.ad_secs, det,
                                                ));
                                            if resp.double_clicked() {
                                                open_ad_popup = Some(t.id);
                                            }
                                        }
                                    });
                                    tr.col(|ui| {
                                        if t.ad_secs > 0 {
                                            let det = ad_breaks.get(&t.id);
                                            let resp = ui
                                                .add(
                                                    egui::Label::new(fmt_ad_time(t.ad_secs))
                                                        .sense(egui::Sense::click()),
                                                )
                                                .on_hover_text(ad_tooltip(
                                                    t.ad_count, t.ad_secs, det,
                                                ));
                                            if resp.double_clicked() {
                                                open_ad_popup = Some(t.id);
                                            }
                                        }
                                    });
                                    tr.col(|_ui| {}); // ad-free (n/a per take)
                                    tr.col(|_ui| {}); // added
                                    tr.col(|ui| {
                                        ui.push_id(t.id, |ui| {
                                            let file_ok = !t.output_path.is_empty()
                                                && std::path::Path::new(&t.output_path).is_file();
                                            if ui
                                                .add_enabled(file_ok, egui::Button::new("▶").small())
                                                .on_hover_text("Open file")
                                                .clicked()
                                            {
                                                open_path =
                                                    Some(std::path::PathBuf::from(&t.output_path));
                                            }
                                            let dir_ok =
                                                dir.as_ref().map(|d| d.is_dir()).unwrap_or(false);
                                            if ui
                                                .add_enabled(dir_ok, egui::Button::new("📂").small())
                                                .on_hover_text("Open folder")
                                                .clicked()
                                            {
                                                open_path = dir.clone();
                                            }
                                            if ui
                                                .add_enabled(
                                                    !t.output_path.is_empty(),
                                                    egui::Button::new("📋").small(),
                                                )
                                                .on_hover_text("Copy file path")
                                                .clicked()
                                            {
                                                copy_text = Some(t.output_path.clone());
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
                                                delete_recording = Some(t.id);
                                            }
                                        });
                                    });
                                });
                            }
                        }
                    }
                });
            });
        self.streams_sort = sort;
        self.streams_filters = filters;
        if let Some(rid) = open_ad_popup {
            self.ad_popup = Some(rid);
        }

        // Tick the live Duration column ~1/sec while anything is recording.
        if any_active {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
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
        if let Some(mid) = acts.edit {
            if let Some(r) = self.rows.iter().find(|r| r.monitor.id == mid) {
                self.form = Some(MonitorForm::from_existing(r));
            }
        }
        if let Some(cid) = acts.add_instance {
            // Look up the container in `channels` (not `rows`) so this also works
            // for an empty container that has no instances yet.
            if let Some(c) = self.channels.iter().find(|c| c.id == cid) {
                self.form = Some(MonitorForm::add_instance(
                    c,
                    self.settings.default_output_dir.clone(),
                ));
            }
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
        if let Some((id, name)) = acts.delete {
            self.confirm_delete = Some((id, name));
        }
        if let Some((cid, on)) = toggle_channel_enabled {
            if let Err(e) = self.core.store.set_channel_enabled(cid, on) {
                self.status = format!("Error: {e}");
            }
            self.reload_rows();
        }
        if let Some(cid) = rename_channel {
            if let Some(c) = self.channels.iter().find(|c| c.id == cid) {
                self.channel_form = Some(ChannelForm {
                    id: Some(cid),
                    name: c.name.clone(),
                });
            }
        }
        if let Some((cid, name)) = delete_channel {
            self.confirm_delete_channel = Some((cid, name));
        }
        if let Some(id) = acts.start {
            self.core.manual(ManualCommand::Start(id));
            self.status = "Checking channel… will record if live.".into();
        }
        if let Some(id) = acts.stop {
            self.core.manual(ManualCommand::Stop(id));
            self.status = "Stopping recording…".into();
        }
        if let Some(p) = open_path {
            crate::platform::open_path(&p);
        }
        if let Some(t) = copy_text {
            ui.ctx().copy_text(t);
        }
        if let Some(rid) = delete_recording {
            if let Err(e) = self.core.store.delete_recording(rid) {
                self.status = format!("Error: {e}");
            }
            // The take (and its cascaded ad breaks) is gone; close a popup for it.
            if self.ad_popup == Some(rid) {
                self.ad_popup = None;
            }
            self.reload_rows();
        }
    }

    /// Kick off the Twitch device-code flow on the async runtime, updating the
    /// shared `twitch_flow` state as it progresses and waking the UI.
    fn start_twitch_connect(&mut self, ctx: egui::Context) {
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

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Detection credentials (optional)");
            ui.label("Used only by monitors set to an API detection method.");
            egui::Grid::new("creds_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Twitch Client ID");
                    ui.text_edit_singleline(&mut self.settings.twitch_client_id);
                    ui.end_row();
                    ui.label("Twitch Client Secret");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.twitch_client_secret)
                            .password(true),
                    );
                    ui.end_row();
                    ui.label("YouTube API Key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.youtube_api_key)
                            .password(true),
                    );
                    ui.end_row();
                    ui.label("Kick Client ID");
                    ui.text_edit_singleline(&mut self.settings.kick_client_id);
                    ui.end_row();
                    ui.label("Kick Client Secret");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.kick_client_secret)
                            .password(true),
                    );
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.heading("Twitch account (OAuth)");
            ui.label("Connect to use a user token for detection (Client Secret then optional).");
            let flow = self.twitch_flow.lock().unwrap().clone();
            match flow {
                AuthFlow::Connected { login } => {
                    ui.horizontal(|ui| {
                        if login.is_empty() {
                            ui.label("✅ Connected");
                        } else {
                            ui.label(format!("✅ Connected as {login}"));
                        }
                        if ui.button("Disconnect").clicked() {
                            let _ = oauth::disconnect(&self.core.store);
                            *self.twitch_flow.lock().unwrap() = AuthFlow::Idle;
                            // disconnect() clears the cached ad-free (sub) results;
                            // reload so the Streams column drops the stale badges now.
                            self.reload_rows();
                        }
                    });
                    ui.small(
                        "Tip: if you connected before the Ad-free feature, reconnect to grant \
                         the subscriptions scope so ad-free (sub) detection works.",
                    );
                }
                AuthFlow::Pending { user_code, url } => {
                    ui.label("Authorize in your browser, then wait:");
                    if url.is_empty() {
                        ui.label("Requesting code…");
                    } else {
                        ui.hyperlink(&url);
                        ui.label(format!("Enter code: {user_code}"));
                    }
                }
                AuthFlow::Failed { message } => {
                    ui.colored_label(egui::Color32::from_rgb(0xE0, 0x6C, 0x6C), &message);
                    if ui.button("🔗 Connect Twitch").clicked() {
                        self.start_twitch_connect(ui.ctx().clone());
                    }
                }
                AuthFlow::Idle => {
                    if ui.button("🔗 Connect Twitch").clicked() {
                        self.start_twitch_connect(ui.ctx().clone());
                    }
                }
            }

            ui.add_space(12.0);
            ui.heading("YouTube WebSub (push via VPS)");
            ui.label(
                "Optional. Point at a running yt-websub relay to get near-instant \
                 go-live triggers for YouTube channels set to the WebSub method.",
            );
            egui::Grid::new("websub_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("VPS base URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.websub_vps_url)
                            .desired_width(320.0)
                            .hint_text("https://hooks.example.com"),
                    )
                    .on_hover_text("The yt-websub server's HTTPS base URL (no trailing /api).");
                    ui.end_row();
                    ui.label("Bearer token");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.websub_token).password(true),
                    )
                    .on_hover_text("YTWEBSUB_BEARER_TOKEN configured on the VPS.");
                    ui.end_row();
                    ui.label("Poll interval (s)");
                    ui.add(egui::TextEdit::singleline(&mut self.settings.websub_poll_secs))
                        .on_hover_text("How often to pull new events from the VPS (min 5).");
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.heading("Defaults");
            egui::Grid::new("defaults_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Default output folder");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.settings.default_output_dir);
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = browse_folder(&self.settings.default_output_dir) {
                                self.settings.default_output_dir = p;
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Max concurrent downloads");
                    ui.text_edit_singleline(&mut self.settings.max_concurrent_downloads);
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.heading("Download authentication");
            ui.label("Default for capturing sub-only / members-only / ad-reduced streams. Per-channel settings override this.");
            egui::Grid::new("auth_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Method");
                    let mut cookies = self.settings.download_auth_method == "cookies";
                    egui::ComboBox::from_id_salt("dl_auth_cb")
                        .selected_text(if cookies { "Browser cookies" } else { "None" })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut cookies, false, "None");
                            ui.selectable_value(&mut cookies, true, "Browser cookies");
                        });
                    self.settings.download_auth_method =
                        if cookies { "cookies".into() } else { "none".into() };
                    ui.end_row();

                    if cookies {
                        ui.label("Browser");
                        egui::ComboBox::from_id_salt("cookies_browser_cb")
                            .selected_text(if self.settings.cookies_browser.is_empty() {
                                "(choose)"
                            } else {
                                &self.settings.cookies_browser
                            })
                            .show_ui(ui, |ui| {
                                for b in COOKIE_BROWSERS {
                                    ui.selectable_value(
                                        &mut self.settings.cookies_browser,
                                        b.to_string(),
                                        b,
                                    );
                                }
                            });
                        ui.end_row();

                        ui.label("Profile / session");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.cookies_profile)
                                .hint_text("optional — e.g. dmrf6eed.YouTube"),
                        )
                        .on_hover_text(
                            "Which browser profile/session to read cookies from. Blank = the \
                             browser's default (most-recently-used) profile — which is why a \
                             dedicated login can be missed. For Firefox, use the profile folder \
                             name (the directory under …/Mozilla/Firefox/Profiles, e.g. \
                             dmrf6eed.YouTube) or an absolute path to it; find it at about:profiles.",
                        );
                        ui.end_row();
                    }
                });

            ui.add_space(12.0);
            ui.heading("Startup");
            let mut on = self.autostart_on;
            if ui
                .checkbox(&mut on, "Start StreamArchiver at login")
                .changed()
            {
                match self.autostart.set(on) {
                    Ok(()) => {
                        self.autostart_on = on;
                        self.status = if on {
                            "Autostart enabled.".into()
                        } else {
                            "Autostart disabled.".into()
                        };
                    }
                    Err(e) => self.status = format!("Autostart error: {e}"),
                }
            }

            ui.add_space(16.0);
            if ui.button("💾 Save settings").clicked() {
                self.save_settings();
            }
        });
    }

    fn form_window(&mut self, ctx: &egui::Context) {
        if self.form.is_none() {
            return;
        }
        let mut open = true;
        let mut do_save = false;
        let mut do_cancel = false;

        let f = self.form.as_ref().unwrap();
        let title = if f.monitor_id.is_some() {
            "Edit instance"
        } else if f.channel_id.is_some() {
            "Add instance"
        } else {
            "Add stream (new channel)"
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                let form = self.form.as_mut().unwrap();
                let platform = Platform::detect(&form.url);
                // When the URL's platform changes, re-apply that platform's
                // tool/detection defaults (e.g. pasting a YouTube URL picks
                // yt-dlp + a YouTube method). User overrides afterwards stick.
                if form.last_platform != Some(platform) {
                    form.tool = platform.default_tool();
                    form.detection_method = platform.default_detection();
                    form.last_platform = Some(platform);
                }
                // The name belongs to the channel container; it's editable only
                // when creating a new channel. For an instance it's the container's
                // (rename via the channel row's ✏). The URL is per-instance and
                // always editable.
                let name_editable = form.channel_id.is_none();

                egui::Grid::new("form_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        let name_resp =
                            ui.add_enabled(name_editable, egui::TextEdit::singleline(&mut form.name));
                        if !name_editable {
                            name_resp.on_hover_text(
                                "The channel name — rename it from the channel row's ✏.",
                            );
                        }
                        ui.end_row();

                        ui.label("URL");
                        ui.add(egui::TextEdit::singleline(&mut form.url).desired_width(320.0))
                            .on_hover_text("This instance's source URL (platform auto-detected).");
                        ui.end_row();

                        ui.label("Platform");
                        ui.label(platform.label());
                        ui.end_row();

                        ui.label("Tool").on_hover_text(form.tool.tooltip());
                        egui::ComboBox::from_id_salt("tool_cb")
                            .selected_text(form.tool.label())
                            .show_ui(ui, |ui| {
                                for t in Tool::ALL {
                                    ui.selectable_value(&mut form.tool, t, t.label())
                                        .on_hover_text(t.tooltip());
                                }
                            });
                        ui.end_row();

                        ui.label("Detection")
                            .on_hover_text(form.detection_method.tooltip());
                        let methods = platform.detection_methods();
                        if !methods.contains(&form.detection_method) {
                            form.detection_method = platform.default_detection();
                        }
                        egui::ComboBox::from_id_salt("method_cb")
                            .selected_text(form.detection_method.label())
                            .show_ui(ui, |ui| {
                                for &dm in methods {
                                    ui.selectable_value(&mut form.detection_method, dm, dm.label())
                                        .on_hover_text(dm.tooltip());
                                }
                            });
                        ui.end_row();

                        ui.label("Poll interval (s)");
                        ui.add(egui::DragValue::new(&mut form.poll_interval_secs).range(5..=86400));
                        ui.end_row();

                        ui.label("Quality");
                        ui.text_edit_singleline(&mut form.quality);
                        ui.end_row();

                        ui.label("Container");
                        egui::ComboBox::from_id_salt("container_cb")
                            .selected_text(form.container.label())
                            .show_ui(ui, |ui| {
                                for c in Container::ALL {
                                    ui.selectable_value(&mut form.container, c, c.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Capture from start");
                        ui.checkbox(&mut form.capture_from_start, "").on_hover_text(
                            "yt-dlp --live-from-start / streamlink --hls-live-restart",
                        );
                        ui.end_row();

                        ui.label("Ad-free");
                        ui.checkbox(&mut form.ad_free, "").on_hover_text(
                            "Mark this instance ad-free for your account (YouTube \
                             membership/Premium, Twitch Turbo/sub) so captures won't have \
                             ad-break hard cuts. For Twitch with a connected account, sub \
                             status is also detected automatically.",
                        );
                        ui.end_row();

                        ui.label("Enabled");
                        ui.checkbox(&mut form.enabled, "")
                            .on_hover_text("Monitor this channel for live streams");
                        ui.end_row();

                        ui.label("Auth");
                        egui::ComboBox::from_id_salt("auth_cb")
                            .selected_text(form.auth_kind.label())
                            .show_ui(ui, |ui| {
                                for k in AuthKind::ALL {
                                    ui.selectable_value(&mut form.auth_kind, k, k.label());
                                }
                            });
                        ui.end_row();

                        // Value field depends on the chosen auth kind.
                        match form.auth_kind {
                            AuthKind::CookiesBrowser => {
                                ui.label("Browser");
                                ui.text_edit_singleline(&mut form.auth_value)
                                    .on_hover_text("Browser, or browser:profile — e.g. firefox:dmrf6eed.YouTube (blank = global)");
                                ui.end_row();
                            }
                            AuthKind::CookiesFile => {
                                ui.label("Cookies file");
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(&mut form.auth_value);
                                    if ui.button("Browse…").clicked() {
                                        if let Some(p) = browse_file(&form.auth_value) {
                                            form.auth_value = p;
                                        }
                                    }
                                });
                                ui.end_row();
                            }
                            AuthKind::Token => {
                                ui.label("Auth token");
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.auth_value).password(true),
                                )
                                .on_hover_text("Twitch OAuth token (streamlink)");
                                ui.end_row();
                            }
                            AuthKind::Inherit | AuthKind::Disabled => {}
                        }

                        ui.label("Output folder");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut form.output_dir);
                            if ui.button("Browse…").clicked() {
                                if let Some(p) = browse_folder(&form.output_dir) {
                                    form.output_dir = p;
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Filename template");
                        ui.text_edit_singleline(&mut form.filename_template);
                        ui.end_row();

                        ui.label("Extra args");
                        ui.text_edit_singleline(&mut form.extra_args);
                        ui.end_row();
                    });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        do_save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        do_cancel = true;
                    }
                });
            });

        if do_save {
            self.save_form();
        } else if do_cancel || !open {
            self.form = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compose_browser_profile, split_browser_profile};

    #[test]
    fn browser_profile_roundtrip() {
        // No profile.
        assert_eq!(split_browser_profile("firefox"), ("firefox".into(), String::new()));
        assert_eq!(compose_browser_profile("firefox", ""), "firefox");

        // Named profile.
        assert_eq!(
            split_browser_profile("firefox:dmrf6eed.YouTube"),
            ("firefox".into(), "dmrf6eed.YouTube".into())
        );
        assert_eq!(
            compose_browser_profile("firefox", "dmrf6eed.YouTube"),
            "firefox:dmrf6eed.YouTube"
        );

        // Absolute-path profile: the drive-letter colon stays in the profile
        // (split on the FIRST colon only, matching yt-dlp).
        let raw = r"firefox:C:\Users\Blu\AppData\Roaming\Mozilla\Firefox\Profiles\dmrf6eed.YouTube";
        let (b, p) = split_browser_profile(raw);
        assert_eq!(b, "firefox");
        assert_eq!(p, r"C:\Users\Blu\AppData\Roaming\Mozilla\Firefox\Profiles\dmrf6eed.YouTube");
        assert_eq!(compose_browser_profile(&b, &p), raw);

        // Empty browser -> empty (no cookies), even with a profile.
        assert_eq!(compose_browser_profile("", "whatever"), "");
    }
}
