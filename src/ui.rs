//! The on-demand egui window: channel table, add/edit form, and settings.
//!
//! Runs reactive (repaints only on input/events). The tray thread wakes it via
//! `Context::request_repaint`. Closing the window hides it to the tray; the
//! tray "Quit" item triggers a real close.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use tracing::warn;
use tray_icon::TrayIcon;

use crate::app_core::AppCore;
use crate::events::{ManualCommand, UiCommand};
use crate::models::{
    AdBreak, AuthKind, Channel, Container, DetectionMethod, DownloadDefaults, K_DISCORD_SCHEDULE,
    K_DISCORD_TOKEN, K_FILENAME_MEDIA, K_MONITOR_DEFAULTS, K_YT_API_DETECT, K_YT_API_SCHEDULE,
    MediaInfoMode, Monitor, MonitorDefaults, MonitorWithChannel, Platform,
    Recording, ScheduleSegment, StreamGroup, StreamMetaChange, Tool, UpcomingStream, Video,
    group_recordings, now_unix,
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
const K_YTDLP_ARGS: &str = "ytdlp_default_args";
/// Optional explicit path to the system yt-dlp binary; empty ⇒ `yt-dlp` on PATH.
const K_YTDLP_BINARY: &str = "ytdlp_binary_path";
/// Path to the SABR dev-build yt-dlp; empty ⇒ SABR capture disabled.
const K_SABR_BINARY: &str = "ytdlp_sabr_binary_path";
/// Master toggle: use the SABR build for YouTube capture-from-start.
const K_SABR_ENABLED: &str = "ytdlp_sabr_enabled";
/// SABR format selector (e.g. `ba[protocol=sabr]+bv[protocol=sabr]`).
const K_SABR_FORMAT: &str = "ytdlp_sabr_format";
/// SABR `--extractor-args` value.
const K_SABR_EXTRACTOR_ARGS: &str = "ytdlp_sabr_extractor_args";
/// Manual raw SABR args; when non-empty, replaces the format+extractor-args preset.
const K_SABR_RAW_ARGS: &str = "ytdlp_sabr_raw_args";
/// PO-token-provider `--extractor-args` (e.g. bgutil), a separate `--extractor-args`
/// entry on the SABR command. Absent ⇒ bgutil default; explicit empty ⇒ disabled.
const K_SABR_POT_ARGS: &str = "ytdlp_sabr_pot_args";
/// DASH-companion format selector for dual capture.
const K_DASH_FORMAT: &str = "ytdlp_dash_format";
const K_WEBSUB_URL: &str = "websub_vps_url";
const K_WEBSUB_TOKEN: &str = "websub_token";
const K_WEBSUB_POLL: &str = "websub_poll_secs";
/// Whether Streams rows get a status background tint (recording / ad / error).
const K_STATUS_BGCOLOR: &str = "status_bgcolor";
/// How dates/timestamps are formatted throughout the UI (see [`DateFmt`]).
const K_DATE_FORMAT: &str = "date_format";
/// Whether the per-row Actions column is shown (the row context menu has the same
/// actions, so it can be hidden to reclaim width).
const K_SHOW_ACTIONS: &str = "show_actions";

/// Browsers yt-dlp can read cookies from (for the Settings dropdown).
const COOKIE_BROWSERS: [&str; 8] = [
    "firefox", "chrome", "chromium", "edge", "brave", "opera", "vivaldi", "safari",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Streams,
    Videos,
    Schedule,
    Background,
    Settings,
}

/// The Schedule tab's calendar granularity.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScheduleMode {
    Month,
    Week,
    Day,
    Agenda,
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
    /// Hex color string (e.g. `"#ff9800"` or `"ff9800"`). Empty = auto palette.
    color: String,
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
    /// YouTube dual capture: also run a DASH companion process (system yt-dlp).
    dual_capture: bool,
    /// Manually mark this instance ad-free (member/sub/Turbo) — drives the Ad-free
    /// column when auto detection isn't available.
    ad_free: bool,
    enabled: bool,
    auth_kind: AuthKind,
    auth_value: String,
    /// Audio tracks to capture (streamlink `--hls-audio-select`): empty = default,
    /// `all`/`*` = every track, or a comma-separated list.
    audio_tracks: String,
    /// Subtitle tracks to capture (yt-dlp `--sub-langs`): empty = none, `all` =
    /// every subtitle, or a comma-separated list of language codes.
    subtitle_tracks: String,
    /// Capture chat alongside the recording (Twitch IRC sidecar / yt-dlp live_chat).
    chat_log: bool,
    /// Download stream thumbnail at recording start (yt-dlp: --write-thumbnail;
    /// Twitch/Kick/YouTube: fetch URL from detection metadata).
    fetch_thumbnail: bool,
    /// Download channel icon, banner, badges, and emotes (Twitch: BTTV/FFZ/7TV too)
    /// into channel_assets/ alongside recordings.
    fetch_chat_assets: bool,
    extra_args: String,
    /// Platform the tool/detection defaults were last set for; a URL change to a
    /// different platform re-applies that platform's defaults.
    last_platform: Option<Platform>,
}

impl MonitorForm {
    /// "Add stream": a new channel container + its first instance.
    fn new_channel(defaults: &MonitorDefaults, default_output_dir: &str) -> MonitorForm {
        // Use Generic platform as the starting point; once the user pastes a URL
        // the platform-change handler re-resolves tool/detection/etc. for that platform.
        let p = Platform::Generic;
        MonitorForm {
            monitor_id: None,
            channel_id: None,
            name: String::new(),
            url: String::new(),
            tool: defaults.resolve_tool(p),
            detection_method: defaults.resolve_detection(p),
            poll_interval_secs: defaults.resolve_poll_interval(p),
            quality: defaults.resolve_quality(p),
            output_dir: defaults.resolve_output_dir(p, default_output_dir),
            filename_template: defaults.resolve_filename_template(p),
            container: defaults.resolve_container(p),
            capture_from_start: defaults.resolve_from_start(p),
            dual_capture: false,
            ad_free: false,
            enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            // New monitors default to max-archival: every audio + subtitle track,
            // chat logging, thumbnails, and channel assets all on.
            audio_tracks: "all".into(),
            subtitle_tracks: "all".into(),
            chat_log: true,
            fetch_thumbnail: true,
            fetch_chat_assets: true,
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
            dual_capture: m.dual_capture,
            ad_free: m.ad_free,
            enabled: m.enabled,
            auth_kind: m.auth_kind,
            auth_value: m.auth_value.clone(),
            audio_tracks: m.audio_tracks.clone(),
            subtitle_tracks: m.subtitle_tracks.clone(),
            chat_log: m.chat_log,
            fetch_thumbnail: m.fetch_thumbnail,
            fetch_chat_assets: m.fetch_chat_assets,
            extra_args: m.extra_args.clone(),
            // Don't override the saved tool/detection just because the form opened.
            last_platform: Some(m.platform()),
        }
    }

    /// Add another instance to an existing channel container. The URL is blank so
    /// the user enters a (possibly different-platform) source.
    fn add_instance(channel: &Channel, defaults: &MonitorDefaults, default_output_dir: &str) -> MonitorForm {
        let p = Platform::Generic;
        MonitorForm {
            monitor_id: None,
            channel_id: Some(channel.id),
            name: channel.name.clone(),
            url: String::new(),
            tool: defaults.resolve_tool(p),
            detection_method: defaults.resolve_detection(p),
            poll_interval_secs: defaults.resolve_poll_interval(p),
            quality: defaults.resolve_quality(p),
            output_dir: defaults.resolve_output_dir(p, default_output_dir),
            filename_template: defaults.resolve_filename_template(p),
            container: defaults.resolve_container(p),
            capture_from_start: defaults.resolve_from_start(p),
            dual_capture: false,
            ad_free: false,
            enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            // New monitors default to max-archival: every audio + subtitle track,
            // chat logging, thumbnails, and channel assets all on.
            audio_tracks: "all".into(),
            subtitle_tracks: "all".into(),
            chat_log: true,
            fetch_thumbnail: true,
            fetch_chat_assets: true,
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
    /// Audio / subtitle track selection + chat logging (sticky across downloads,
    /// like `auto_title` — not reset when the platform changes). See [`Video`].
    audio_tracks: String,
    subtitle_tracks: String,
    chat_log: bool,
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
            // Archive all audio + subtitle tracks by default (matches new
            // monitors); chat is the niche extra, opt-in per download.
            audio_tracks: "all".into(),
            subtitle_tracks: "all".into(),
            chat_log: false,
            last_platform: None,
        }
    }
}

#[derive(Default)]
struct SettingsForm {
    twitch_client_id: String,
    twitch_client_secret: String,
    youtube_api_key: String,
    /// Per-operation opt-ins to use the YouTube Data API key instead of scraping
    /// (each costs quota — see the Settings section).
    youtube_api_detect: bool,
    youtube_api_schedule: bool,
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
    /// When to probe captures for the {resolution}/{fps}/… filename variables.
    filename_media_info: MediaInfoMode,
    /// How dates/timestamps are displayed throughout the UI.
    date_fmt: DateFmt,
    /// Global extra arguments prepended to every yt-dlp invocation (all monitors).
    /// Per-monitor extra_args are appended after these, so they take precedence.
    ytdlp_default_args: String,
    /// Explicit path to the system yt-dlp binary; empty ⇒ `yt-dlp` on PATH.
    ytdlp_binary_path: String,
    /// Path to the SABR dev-build yt-dlp (for YouTube capture-from-start). Empty ⇒
    /// SABR disabled (capture-from-start uses the system binary's normal path).
    sabr_binary_path: String,
    /// Master toggle: use the SABR build for YouTube capture-from-start.
    sabr_enabled: bool,
    /// SABR format selector + extractor-args preset.
    sabr_format: String,
    sabr_extractor_args: String,
    /// Manual raw SABR args; non-empty overrides the format+extractor-args preset.
    sabr_raw_args: String,
    /// PO-token-provider `--extractor-args` (e.g. bgutil) for the SABR command.
    sabr_pot_args: String,
    /// DASH-companion format selector used by dual (SABR+DASH) capture.
    dash_format: String,
    /// Discord user token + whether to import stream schedules from Discord events
    /// (opt-in; automating a user token is against Discord's ToS).
    discord_token: String,
    discord_schedule: bool,
}

/// Which field the Format Designer was opened from (for "Apply").
#[derive(Clone, PartialEq)]
enum FormatDesignerTarget {
    MonitorForm,
    VideoForm,
}

/// State for the floating Format Designer window.
struct FormatDesignerState {
    template: String,
    selected_monitor_idx: usize,
    /// Recordings for the currently selected monitor (oldest-first from the store).
    recordings: Vec<Recording>,
    selected_recording_idx: usize,
    /// Which field opened the designer (None = standalone / no write-back).
    target: Option<FormatDesignerTarget>,
}

impl FormatDesignerState {
    fn new(template: String, target: Option<FormatDesignerTarget>) -> Self {
        Self {
            template,
            selected_monitor_idx: 0,
            recordings: Vec::new(),
            selected_recording_idx: 0,
            target,
        }
    }
}

pub struct StreamArchiverApp {
    core: Arc<AppCore>,
    _tray: TrayIcon,
    ui_rx: Receiver<UiCommand>,
    events_rx: crate::events::EventRx,
    autostart: AutoStart,
    autostart_on: bool,
    /// When false (the default), quitting detaches downloads so they keep running
    /// across a restart/rebuild; when true, quitting stops them. Persisted as the
    /// `stop_downloads_on_quit` setting (stored inverted).
    keep_downloads_on_quit: bool,
    /// Show desktop notifications (toasts) on recording start/finish/error.
    /// Persisted as the `notifications_enabled` setting; default on.
    notifications_enabled: bool,
    /// The process-manager dialog: whether it's open, its last snapshot, and when
    /// that snapshot was taken (throttles the per-row `pid_alive`/DB queries).
    show_processes: bool,
    processes: Vec<crate::app_core::ProcInfo>,
    processes_refreshed: Option<std::time::Instant>,
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
    /// Per-platform monitor-creation defaults editable in Settings (persisted JSON).
    monitor_defaults: MonitorDefaults,
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
    /// Lazy per-recording ad-break detail (cut list), keyed by recording id;
    /// cleared on reload. Avoids a per-frame DB query for tooltips/the popup.
    ad_break_cache: HashMap<i64, Vec<AdBreak>>,
    /// Recording id whose ad-break cut list is shown in a popup (None = closed).
    ad_popup: Option<i64>,
    /// Lazy per-recording title/category change log, keyed by recording id;
    /// cleared on reload. Same caching role as `ad_break_cache`.
    meta_change_cache: HashMap<i64, Vec<StreamMetaChange>>,
    /// What the metadata-change popup shows (None = closed): a single take or a
    /// whole stream's aggregated takes.
    meta_popup: Option<MetaPopup>,
    /// Lazy per-monitor upcoming-schedule detail, keyed by monitor id; cleared on
    /// reload. Backs the Next stream popup.
    schedule_cache: HashMap<i64, Vec<ScheduleSegment>>,
    /// Monitor id whose upcoming schedule is shown in a popup (None = closed).
    schedule_popup: Option<i64>,
    /// All upcoming scheduled streams (across every monitor), backing the Schedule
    /// calendar. Loaded lazily on first view + on refresh; see [`Self::reload_schedule`].
    schedule_all: Vec<UpcomingStream>,
    /// Whether [`Self::schedule_all`] has been loaded yet (lazy on first view).
    schedule_loaded: bool,
    /// Schedule calendar granularity (month / week / day).
    schedule_mode: ScheduleMode,
    /// The focused date the Schedule calendar is centered on; `None` until set to
    /// today on first view. Month view uses its year+month, week view the week
    /// containing it, day view the date itself.
    schedule_anchor: Option<chrono::NaiveDate>,
    /// Channel ids hidden from the Schedule calendar (sidebar filter). Tracking
    /// *hidden* (not visible) means newly-added channels default to visible.
    schedule_hidden: HashSet<i64>,
    /// Whether to flag overlapping streams (time collisions) in the calendar.
    schedule_collisions: bool,
    /// The day whose full stream list is shown in a popup (local date; None = closed).
    schedule_day_popup: Option<chrono::NaiveDate>,
    /// Chat log viewer popup (None = closed).
    chat_popup: Option<ChatPopup>,
    /// Platform favicons, uploaded to the GPU on first use (None until then).
    platform_tex: Option<PlatformTextures>,
    /// Which monitor's Properties window is open (None = closed).
    properties_popup: Option<i64>,
    /// Per-channel cached icon textures loaded from disk for the Properties window.
    /// A `None` value means the lookup was attempted but no icon file was found.
    channel_icons: HashMap<i64, Option<egui::TextureHandle>>,
    /// Sort + per-column filters for the Videos table.
    videos_sort: SortState,
    videos_filters: Vec<String>,
    /// Shared state of the interactive "Connect Twitch" device-code flow.
    twitch_flow: Arc<Mutex<AuthFlow>>,
    /// Whether Streams rows show a status background tint (recording / ad / error).
    /// Toggled from the top bar; persisted under [`K_STATUS_BGCOLOR`]. Keyboard
    /// row selection is still highlighted regardless.
    status_bgcolor: bool,
    /// Whether the per-row Actions column (inline action buttons) is shown in the
    /// Streams + Videos tables. Off reclaims width; every action is also on the
    /// row's right-click context menu. Persisted under [`K_SHOW_ACTIONS`].
    show_actions: bool,
    /// Currently running background tasks (asset fetches, thumbnail downloads).
    background_tasks: Vec<crate::events::BackgroundTask>,
    /// Completed/failed background tasks (task, outcome, finished-at unix), newest
    /// first; capped at 100.
    finished_tasks: Vec<(crate::events::BackgroundTask, crate::events::TaskOutcome, i64)>,
    /// Enable/disable state for the periodic jobs (`events::TOGGLEABLE_JOBS`),
    /// mirrored from settings; edited via the Background "Scheduled" checkboxes.
    job_toggles: std::collections::HashMap<String, bool>,
    /// Format Designer: an interactive template preview/editor window.
    format_designer: Option<FormatDesignerState>,
    /// Pending "Stop recordings & quit" confirmation (triggered by the tray item).
    confirm_quit_stop: bool,
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
        // Detach-on-quit is the default; only `=="1"` opts into stopping downloads.
        let keep_downloads_on_quit = core
            .store
            .get_setting("stop_downloads_on_quit")
            .ok()
            .flatten()
            .as_deref()
            != Some("1");
        // Desktop notifications default on; only `=="0"` disables them.
        let notifications_enabled = core
            .store
            .get_setting(crate::notifications::K_NOTIFICATIONS)
            .ok()
            .flatten()
            .as_deref()
            != Some("0");

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
            youtube_api_detect: setting_or_empty(&core, K_YT_API_DETECT) == "1",
            youtube_api_schedule: setting_or_empty(&core, K_YT_API_SCHEDULE) == "1",
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
            filename_media_info: MediaInfoMode::parse(&setting_or_empty(&core, K_FILENAME_MEDIA)),
            date_fmt: DateFmt::parse(&setting_or_empty(&core, K_DATE_FORMAT)),
            ytdlp_default_args: setting_or_empty(&core, K_YTDLP_ARGS),
            ytdlp_binary_path: setting_or_empty(&core, K_YTDLP_BINARY),
            sabr_binary_path: setting_or_empty(&core, K_SABR_BINARY),
            // Absent ⇒ enabled by default; an explicit "0" disables it.
            sabr_enabled: setting_or_empty(&core, K_SABR_ENABLED) != "0",
            sabr_format: {
                let v = setting_or_empty(&core, K_SABR_FORMAT);
                if v.is_empty() {
                    crate::downloader::SABR_DEFAULT_FORMAT.to_string()
                } else {
                    v
                }
            },
            sabr_extractor_args: {
                let v = setting_or_empty(&core, K_SABR_EXTRACTOR_ARGS);
                if v.is_empty() {
                    crate::downloader::SABR_DEFAULT_EXTRACTOR_ARGS.to_string()
                } else {
                    v
                }
            },
            sabr_raw_args: setting_or_empty(&core, K_SABR_RAW_ARGS),
            // Absent ⇒ bgutil default; present (even empty) ⇒ honor it verbatim so
            // the user can disable it and rely on the plugin's auto-detection.
            sabr_pot_args: match core.store.get_setting(K_SABR_POT_ARGS) {
                Ok(Some(v)) => v,
                _ => crate::downloader::SABR_DEFAULT_POT_ARGS.to_string(),
            },
            dash_format: {
                let v = setting_or_empty(&core, K_DASH_FORMAT);
                if v.is_empty() {
                    crate::downloader::DASH_DEFAULT_FORMAT.to_string()
                } else {
                    v
                }
            },
            discord_token: setting_or_empty(&core, K_DISCORD_TOKEN),
            discord_schedule: setting_or_empty(&core, K_DISCORD_SCHEDULE) == "1",
        };
        // Apply the loaded date format before the first render.
        set_active_date_fmt(settings.date_fmt);

        let twitch_flow = Arc::new(Mutex::new(match oauth::connected_login(&core.store) {
            Some(login) => AuthFlow::Connected { login },
            None => AuthFlow::Idle,
        }));

        // Status row tint defaults on; only an explicit "0" disables it.
        let status_bgcolor = core
            .store
            .get_setting(K_STATUS_BGCOLOR)
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true);
        // The Actions column defaults on; only an explicit "0" hides it.
        let show_actions = core
            .store
            .get_setting(K_SHOW_ACTIONS)
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true);

        let download_defaults = core
            .store
            .get_setting("download_defaults")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<DownloadDefaults>(&s).ok())
            .unwrap_or_else(|| DownloadDefaults::seeded(&settings.default_output_dir));

        let monitor_defaults = core
            .store
            .get_setting(K_MONITOR_DEFAULTS)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<MonitorDefaults>(&s).ok())
            .unwrap_or_default();

        // Snapshot job enable/disable state before `core` is moved into the struct.
        let job_toggles: std::collections::HashMap<String, bool> =
            crate::events::TOGGLEABLE_JOBS
                .iter()
                .map(|(_, key)| (key.to_string(), core.store.job_enabled(key)))
                .collect();

        let mut app = StreamArchiverApp {
            core,
            _tray: tray,
            ui_rx,
            events_rx,
            autostart,
            autostart_on,
            keep_downloads_on_quit,
            notifications_enabled,
            show_processes: false,
            processes: Vec::new(),
            processes_refreshed: None,
            quitting: false,
            view: View::Streams,
            rows: Vec::new(),
            channels: Vec::new(),
            videos: Vec::new(),
            form: None,
            video_form: VideoForm::new(),
            download_defaults,
            monitor_defaults,
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
            ad_break_cache: HashMap::new(),
            ad_popup: None,
            meta_change_cache: HashMap::new(),
            meta_popup: None,
            schedule_cache: HashMap::new(),
            schedule_popup: None,
            schedule_all: Vec::new(),
            schedule_loaded: false,
            schedule_mode: ScheduleMode::Month,
            schedule_anchor: None,
            schedule_hidden: HashSet::new(),
            schedule_collisions: true,
            schedule_day_popup: None,
            chat_popup: None,
            platform_tex: None,
            properties_popup: None,
            channel_icons: HashMap::new(),
            videos_sort: SortState::default(),
            videos_filters: vec![String::new(); VIDEO_COLS],
            twitch_flow,
            status_bgcolor,
            show_actions,
            background_tasks: Vec::new(),
            finished_tasks: Vec::new(),
            job_toggles,
            format_designer: None,
            confirm_quit_stop: false,
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
        // Merge in each monitor's next upcoming scheduled stream (the row query
        // doesn't carry it — it's refreshed on a separate cadence).
        if let Ok(next) = self.core.store.next_scheduled_streams(now_unix()) {
            let by_mid: HashMap<i64, (i64, String)> =
                next.into_iter().map(|(mid, at, title)| (mid, (at, title))).collect();
            for row in &mut self.rows {
                if let Some((at, title)) = by_mid.get(&row.monitor.id) {
                    row.next_stream_at = Some(*at);
                    row.next_stream_title = title.clone();
                }
            }
        }
        // Load all containers (incl. empty ones) so they show in the tree.
        match self.core.store.list_channels() {
            Ok(chs) => self.channels = chs,
            Err(e) => warn!("failed to load channels: {e:#}"),
        }
        // History may have changed; re-fetch lazily on next expand.
        self.rec_cache.clear();
        self.ad_break_cache.clear();
        self.meta_change_cache.clear();
        self.schedule_cache.clear();
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

    /// (Re)load every upcoming scheduled stream for the Schedule calendar. Loaded
    /// from the start of today (local) so today's full day shows even past streams.
    fn reload_schedule(&mut self) {
        match self.core.store.all_upcoming_schedule(today_start_unix()) {
            Ok(v) => {
                self.schedule_all = v;
                // Drop hide choices only for channels that no longer EXIST (deleted),
                // not ones merely without an upcoming stream right now — otherwise a
                // channel temporarily off the schedule would silently un-hide.
                let live: HashSet<i64> = self.channels.iter().map(|c| c.id).collect();
                if !live.is_empty() {
                    self.schedule_hidden.retain(|id| live.contains(id));
                }
                // Only latch on success, so a transient DB error retries on the next
                // frame via the lazy-load guard instead of stranding the empty state.
                self.schedule_loaded = true;
            }
            Err(e) => {
                warn!("failed to load schedule: {e:#}");
                self.status = format!("Error loading schedule: {e}");
            }
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

    fn persist_monitor_defaults(&self) {
        match serde_json::to_string(&self.monitor_defaults) {
            Ok(json) => {
                let _ = self.core.store.set_setting(K_MONITOR_DEFAULTS, &json);
            }
            Err(e) => warn!("failed to serialize monitor defaults: {e:#}"),
        }
    }

    /// Handle tray commands and bus events; returns true if a repaint is needed.
    fn pump_messages(&mut self, ctx: &egui::Context) {
        // One-shot startup notice (e.g. detached downloads recovered on launch).
        if let Some(msg) = self.core.startup_notice.lock().unwrap().take() {
            self.status = msg;
        }
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
                UiCommand::QuitAndStop => {
                    // Show confirmation before stopping active recordings.
                    self.confirm_quit_stop = true;
                    // Bring the window to the foreground so the dialog is visible.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
                Ok(crate::events::AppEvent::BackgroundTaskStarted(task)) => {
                    self.background_tasks.push(task);
                    dirty = true;
                }
                Ok(crate::events::AppEvent::BackgroundTaskFinished { id, outcome }) => {
                    if let Some(pos) = self.background_tasks.iter().position(|t| t.id == id) {
                        let task = self.background_tasks.remove(pos);
                        // A finished asset fetch may have produced a new channel
                        // icon — drop the avatar cache so it reloads from disk.
                        if task.kind == crate::events::BackgroundTaskKind::AssetFetch {
                            self.channel_icons.clear();
                        }
                        self.finished_tasks.insert(0, (task, outcome, now_unix()));
                        self.finished_tasks.truncate(100);
                    }
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
            // Keep the Schedule calendar in sync with background schedule fetches
            // (which emit a state event) — but only once it has been loaded, so we
            // don't pull it in before the user ever opens the tab.
            if self.schedule_loaded {
                self.reload_schedule();
            }
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
            dual_capture: form.dual_capture,
            ad_free: form.ad_free,
            auth_kind: form.auth_kind,
            auth_value: form.auth_value.clone(),
            audio_tracks: form.audio_tracks.trim().to_string(),
            subtitle_tracks: form.subtitle_tracks.trim().to_string(),
            chat_log: form.chat_log,
            fetch_thumbnail: form.fetch_thumbnail,
            fetch_chat_assets: form.fetch_chat_assets,
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
        // Discord import counts as on only when a token backs the toggle.
        let discord_on = s.discord_schedule && !s.discord_token.trim().is_empty();
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
            (K_FILENAME_MEDIA, s.filename_media_info.as_str()),
            (K_DATE_FORMAT, s.date_fmt.as_str()),
            (K_YT_API_DETECT, if s.youtube_api_detect { "1" } else { "0" }),
            (K_YT_API_SCHEDULE, if s.youtube_api_schedule { "1" } else { "0" }),
            (K_YTDLP_ARGS, s.ytdlp_default_args.trim()),
            (K_YTDLP_BINARY, s.ytdlp_binary_path.trim()),
            (K_SABR_BINARY, s.sabr_binary_path.trim()),
            (K_SABR_ENABLED, if s.sabr_enabled { "1" } else { "0" }),
            (K_SABR_FORMAT, s.sabr_format.trim()),
            (K_SABR_EXTRACTOR_ARGS, s.sabr_extractor_args.trim()),
            (K_SABR_RAW_ARGS, s.sabr_raw_args.trim()),
            (K_SABR_POT_ARGS, s.sabr_pot_args.trim()),
            (K_DASH_FORMAT, s.dash_format.trim()),
            (K_DISCORD_TOKEN, s.discord_token.trim()),
            // Only persist the import as on when a token actually backs it, so the
            // consent flag can't be left latched with no token.
            (
                K_DISCORD_SCHEDULE,
                if discord_on { "1" } else { "0" },
            ),
        ];
        for (k, v) in pairs {
            if let Err(e) = self.core.store.set_setting(k, v) {
                self.status = format!("Error saving settings: {e}");
                return;
            }
        }
        // If Discord import is now off (toggled off or token cleared), purge any
        // previously-imported Discord events so they don't linger on the calendar.
        if !discord_on {
            let _ = self.core.store.clear_schedule_source("discord");
            self.reload_schedule();
        }
        // Apply the (possibly changed) date format to the live UI.
        set_active_date_fmt(self.settings.date_fmt);
        self.persist_monitor_defaults();
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

/// Coarse human duration: `45s` / `5m` / `6h` / `2d`.
fn fmt_duration_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s < 90 {
        format!("{s}s")
    } else if s < 90 * 60 {
        format!("{}m", (s + 30) / 60)
    } else if s < 36 * 3600 {
        format!("{}h", (s + 1800) / 3600)
    } else {
        format!("{}d", (s + 12 * 3600) / 86_400)
    }
}

/// `now` / `in 3m` for a future delta in seconds.
fn fmt_relative_future(delta: i64) -> String {
    if delta <= 0 {
        "now".to_string()
    } else {
        format!("in {}", fmt_duration_secs(delta))
    }
}

/// Label a take as "SABR"/"DASH" when it's part of a dual capture (two recordings
/// sharing a `take_group`). Returns `None` for ordinary single-recording takes.
fn dual_take_variant(g: &StreamGroup, t: &Recording) -> Option<&'static str> {
    // Only label takes that belong to a multi-recording (dual) capture cluster.
    let in_dual = g
        .take_groups()
        .iter()
        .any(|grp| grp.len() >= 2 && grp.iter().any(|r| r.id == t.id));
    if !in_dual {
        return None;
    }
    if t.output_path.contains(".dash.") {
        Some("DASH")
    } else {
        Some("SABR")
    }
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

/// User-selectable display format for dates/timestamps (the Settings "Date
/// format" control). Read globally via [`active_date_fmt`] so the free-function
/// formatters can honor it without threading the setting through every call site.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum DateFmt {
    /// ISO 8601-style `2026-06-21` / `2026-06-21 14:02:33` (the default).
    #[default]
    Iso,
    /// ISO without seconds: `2026-06-21 14:02`.
    IsoNoSecs,
    /// US `06/21/2026` / `06/21/2026 02:02 PM`.
    Us,
    /// European `21.06.2026` / `21.06.2026 14:02`.
    Eu,
    /// Compact, year-less `06-21` / `06-21 14:02:33` (narrowest).
    Compact,
}

impl DateFmt {
    const ALL: [DateFmt; 5] = [
        DateFmt::Iso,
        DateFmt::IsoNoSecs,
        DateFmt::Us,
        DateFmt::Eu,
        DateFmt::Compact,
    ];

    fn as_str(self) -> &'static str {
        match self {
            DateFmt::Iso => "iso",
            DateFmt::IsoNoSecs => "iso_no_secs",
            DateFmt::Us => "us",
            DateFmt::Eu => "eu",
            DateFmt::Compact => "compact",
        }
    }

    fn parse(s: &str) -> DateFmt {
        match s {
            "iso_no_secs" => DateFmt::IsoNoSecs,
            "us" => DateFmt::Us,
            "eu" => DateFmt::Eu,
            "compact" => DateFmt::Compact,
            _ => DateFmt::Iso,
        }
    }

    /// chrono pattern for a date-only value.
    fn date_pattern(self) -> &'static str {
        match self {
            DateFmt::Iso | DateFmt::IsoNoSecs => "%Y-%m-%d",
            DateFmt::Us => "%m/%d/%Y",
            DateFmt::Eu => "%d.%m.%Y",
            DateFmt::Compact => "%m-%d",
        }
    }

    /// chrono pattern for a full timestamp.
    fn datetime_pattern(self) -> &'static str {
        match self {
            DateFmt::Iso => "%Y-%m-%d %H:%M:%S",
            DateFmt::IsoNoSecs => "%Y-%m-%d %H:%M",
            DateFmt::Us => "%m/%d/%Y %I:%M %p",
            DateFmt::Eu => "%d.%m.%Y %H:%M",
            DateFmt::Compact => "%m-%d %H:%M:%S",
        }
    }

    /// chrono pattern for a time-only value (12-hour for US, else 24-hour). Used
    /// by the Schedule calendar chips, which only have room for the time.
    fn time_pattern(self) -> &'static str {
        match self {
            DateFmt::Us => "%I:%M %p",
            _ => "%H:%M",
        }
    }

    fn label(self) -> &'static str {
        match self {
            DateFmt::Iso => "ISO — 2026-06-21 14:02:33",
            DateFmt::IsoNoSecs => "ISO, no seconds — 2026-06-21 14:02",
            DateFmt::Us => "US — 06/21/2026 02:02 PM",
            DateFmt::Eu => "EU — 21.06.2026 14:02",
            DateFmt::Compact => "Compact — 06-21 14:02:33",
        }
    }
}

/// The active [`DateFmt`] discriminant (index into [`DateFmt::ALL`]). The UI runs
/// single-threaded; this is a cheap shared cell set at startup and on save so the
/// formatters below don't need the setting passed in.
static ACTIVE_DATE_FMT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn active_date_fmt() -> DateFmt {
    let i = ACTIVE_DATE_FMT.load(std::sync::atomic::Ordering::Relaxed) as usize;
    DateFmt::ALL.get(i).copied().unwrap_or(DateFmt::Iso)
}

fn set_active_date_fmt(f: DateFmt) {
    let i = DateFmt::ALL.iter().position(|&x| x == f).unwrap_or(0) as u8;
    ACTIVE_DATE_FMT.store(i, std::sync::atomic::Ordering::Relaxed);
}

/// Format a unix timestamp as a local date in the active [`DateFmt`] (empty if
/// unset).
fn fmt_date(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format(active_date_fmt().date_pattern())
                .to_string()
        })
        .unwrap_or_default()
}

/// Local timestamp in the active [`DateFmt`] (empty if unset). Used for the
/// Polled / Went Live / Started On columns and the history tree.
fn fmt_datetime_short(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format(active_date_fmt().datetime_pattern())
                .to_string()
        })
        .unwrap_or_default()
}

/// "Polled" cell text: the last-checked timestamp with the poll interval in
/// parentheses, e.g. `2026-06-21 14:02:33 (60s)`. When never polled, shows just
/// the interval `(60s)` so the configured cadence is still visible.
fn fmt_polled(last_checked: Option<i64>, interval_secs: i64) -> String {
    let when = fmt_datetime_short(last_checked.unwrap_or(0));
    if when.is_empty() {
        format!("({interval_secs}s)")
    } else {
        format!("{when} ({interval_secs}s)")
    }
}

/// Format a duration in seconds as `HH:MM:SS`.
fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Local time-of-day for a unix timestamp in the active [`DateFmt`] (e.g. `14:02`
/// or `02:02 PM`). Empty if unset. Used by the Schedule calendar chips.
fn fmt_time_short(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format(active_date_fmt().time_pattern())
                .to_string()
        })
        .unwrap_or_default()
}

/// The local calendar date a unix timestamp falls on (for bucketing schedule
/// entries into calendar cells).
fn local_date(secs: i64) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.with_timezone(&chrono::Local).date_naive())
}

/// Build a filename preview for the Format Designer using real monitor/recording
/// data. Media info is synthetic (1920×1080/60fps/h264/aac) since probing requires
/// async work the UI thread doesn't do. Extension is NOT included.
fn build_preview_filename(
    monitor: &MonitorWithChannel,
    recording: Option<&Recording>,
    template: &str,
) -> String {
    let ch_name = monitor.channel.name.as_str();
    let platform_s = monitor.monitor.platform().as_str().to_string();
    let tool_s = monitor.monitor.tool.label().to_string();
    let quality_s = monitor.monitor.quality.clone();
    let take_s = (monitor.recording_count + 1).to_string();
    let mode_s = match monitor.monitor.tool {
        Tool::Streamlink => "live".to_string(),
        Tool::Ffmpeg => "direct".to_string(),
        Tool::YtDlp => {
            if monitor.monitor.platform() == Platform::YouTube {
                if monitor.monitor.capture_from_start { "sabr".to_string() } else { "dash".to_string() }
            } else {
                "live".to_string()
            }
        }
    };
    let (started_at, went_live, stream_id_s, title_s, games_s) = match recording {
        Some(r) => (
            r.started_at,
            r.went_live_at.unwrap_or(0),
            r.stream_id.clone().unwrap_or_default(),
            r.title.clone(),
            r.category.clone(),
        ),
        None => (now_unix(), 0i64, String::new(), "Stream Title".to_string(), "Sample Game".to_string()),
    };
    let vars = crate::downloader::TemplateVars {
        name: ch_name,
        title: &title_s,
        channel: ch_name,
        video_id: &stream_id_s,
        quality: &quality_s,
        resolution: "1920x1080",
        height: "1080",
        width: "1920",
        fps: "60",
        vcodec: "h264",
        acodec: "aac",
        tool: &tool_s,
        mode: &mode_s,
        platform: &platform_s,
        take: &take_s,
        games: &games_s,
        secs: started_at,
        went_live,
    };
    crate::downloader::preview_filename(template, &vars)
}

/// Unix seconds at the start (00:00 local) of today. Used to load schedule
/// entries from the beginning of today so today's full day shows in the calendar.
fn today_start_unix() -> i64 {
    use chrono::TimeZone;
    let now = chrono::Local::now();
    let today = now.date_naive();
    let at = |h: u32| {
        today
            .and_hms_opt(h, 0, 0)
            .and_then(|t| chrono::Local.from_local_datetime(&t).earliest())
            .map(|dt| dt.timestamp())
    };
    // Local midnight, falling back to 01:00 when midnight lands in a DST
    // spring-forward gap (a handful of zones transition at 00:00), and finally to
    // `now` if even that doesn't resolve — always ≤ now so "today" stays inclusive.
    at(0).or_else(|| at(1)).unwrap_or_else(|| now.timestamp())
}

/// When a scheduled stream has no end time (YouTube upcoming streams don't carry
/// one), assume this duration when checking for time collisions.
const COLLISION_DEFAULT_SECS: i64 = 2 * 3600;

/// The effective end time of a stream: its stated end if valid, else 2 hours past start.
/// Used by collision detection and the time-grid painter.
fn effective_end(s: &UpcomingStream) -> i64 {
    s.end_time
        .filter(|&e| e > s.start_time)
        .unwrap_or(s.start_time + COLLISION_DEFAULT_SECS)
}

/// Format a time range for display: "HH:MM – HH:MM" when end is valid, else "HH:MM".
fn fmt_time_range(start: i64, end: Option<i64>) -> String {
    let s = fmt_time_short(start);
    match end.filter(|&e| e > start) {
        Some(e) => format!("{s} – {}", fmt_time_short(e)),
        None => s,
    }
}

/// 16-color palette for calendar event blocks. Indexed by `channel_id % 16` so each
/// channel gets a consistent, distinct color across all schedule views.
/// Parse a CSS-style hex color string (`"#rrggbb"` or `"rrggbb"`) into an egui
/// color. Returns `None` for any other input so callers can fall back gracefully.
fn parse_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.trim().trim_start_matches('#');
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(egui::Color32::from_rgb(r, g, b))
    } else {
        None
    }
}

/// Return the display color for a channel: the custom hex `color` if set and
/// valid, otherwise a deterministic palette color keyed on `channel_id`.
fn channel_event_color(channel_id: i64, color: &str) -> egui::Color32 {
    if !color.is_empty() {
        if let Some(c) = parse_hex_color(color) {
            return c;
        }
    }
    const PALETTE: &[egui::Color32] = &[
        egui::Color32::from_rgb(0x42, 0x88, 0xc4), // steel blue
        egui::Color32::from_rgb(0x9c, 0x27, 0xb0), // purple
        egui::Color32::from_rgb(0xe9, 0x1e, 0x63), // pink
        egui::Color32::from_rgb(0xf4, 0x43, 0x36), // red
        egui::Color32::from_rgb(0xff, 0x98, 0x00), // orange
        egui::Color32::from_rgb(0xc0, 0x90, 0x00), // amber (darkened for readability)
        egui::Color32::from_rgb(0x38, 0x8e, 0x3c), // green
        egui::Color32::from_rgb(0x00, 0x83, 0x8f), // cyan (darkened)
        egui::Color32::from_rgb(0x00, 0x79, 0x6b), // teal
        egui::Color32::from_rgb(0x30, 0x3f, 0x9f), // indigo
        egui::Color32::from_rgb(0x02, 0x77, 0xbd), // sky blue
        egui::Color32::from_rgb(0x55, 0x8b, 0x2f), // light green
        egui::Color32::from_rgb(0x82, 0x77, 0x17), // lime (darkened)
        egui::Color32::from_rgb(0x6d, 0x40, 0x2f), // brown
        egui::Color32::from_rgb(0x45, 0x5a, 0x64), // blue-grey
        egui::Color32::from_rgb(0x75, 0x75, 0x75), // grey
    ];
    PALETTE[(channel_id.unsigned_abs() as usize) % PALETTE.len()]
}

/// Indices into `all` whose visible (channel not in `hidden`) time window overlaps
/// another visible stream's — i.e. two streams scheduled at the same time. A
/// stream with no/invalid end time is treated as [`COLLISION_DEFAULT_SECS`] long.
fn schedule_collisions(all: &[UpcomingStream], hidden: &HashSet<i64>) -> HashSet<usize> {
    // (original index, start, effective end) for visible streams, sorted by start.
    let mut spans: Vec<(usize, i64, i64)> = all
        .iter()
        .enumerate()
        .filter(|(_, s)| !hidden.contains(&s.channel_id))
        .map(|(i, s)| (i, s.start_time, effective_end(s)))
        .collect();
    spans.sort_by_key(|&(_, start, _)| start);
    let mut out = HashSet::new();
    for a in 0..spans.len() {
        let (ia, _, ea) = spans[a];
        for &(ib, sb, _) in &spans[a + 1..] {
            // Sorted by start, so once a later stream begins at/after `a` ends, no
            // remaining stream can overlap `a`.
            if sb >= ea {
                break;
            }
            out.insert(ia);
            out.insert(ib);
        }
    }
    out
}

/// Shift a date by `delta` whole months, clamping the day to the target month's
/// length (e.g. Jan 31 + 1 month → Feb 28). Used by month-view navigation.
fn shift_month(d: chrono::NaiveDate, delta: i32) -> chrono::NaiveDate {
    use chrono::Datelike;
    let total = d.year() * 12 + (d.month() as i32 - 1) + delta;
    let ny = total.div_euclid(12);
    let nm = total.rem_euclid(12) as u32 + 1;
    // Try the same day, then earlier, until one is valid for the target month.
    for day in (1..=d.day()).rev() {
        if let Some(nd) = chrono::NaiveDate::from_ymd_opt(ny, nm, day) {
            return nd;
        }
    }
    d
}

/// The Monday of the week containing `d` (weeks start on Monday).
fn week_start(d: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    let lead = d.weekday().num_days_from_monday();
    d.checked_sub_days(chrono::Days::new(lead as u64)).unwrap_or(d)
}

/// Offset a date by `n` days (negative = backwards), saturating on overflow.
fn add_days(d: chrono::NaiveDate, n: i64) -> chrono::NaiveDate {
    if n >= 0 {
        d.checked_add_days(chrono::Days::new(n as u64))
    } else {
        d.checked_sub_days(chrono::Days::new(n.unsigned_abs()))
    }
    .unwrap_or(d)
}

/// Calendar header title, e.g. `June 2026`.
fn month_title(y: i32, m: u32) -> String {
    const NAMES: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    let name = NAMES.get((m.max(1) - 1) as usize).copied().unwrap_or("");
    format!("{name} {y}")
}

/// Multi-line detail for one upcoming stream — the calendar hover text and the
/// "Copy details" payload.
fn schedule_detail_line(s: &UpcomingStream) -> String {
    let mut parts = vec![
        format!("{}  {}", fmt_datetime_short(s.start_time), s.channel_name),
        format!("Platform: {}", s.platform().label()),
    ];
    if let Some(end) = s.end_time.filter(|&e| e > s.start_time) {
        parts.push(format!("Until: {}", fmt_datetime_short(end)));
    }
    if !s.title.is_empty() {
        parts.push(format!("Title: {}", s.title));
    }
    if !s.category.is_empty() {
        parts.push(format!("Category: {}", s.category));
    }
    if !s.url.is_empty() {
        parts.push(s.url.clone());
    }
    if s.is_discord() {
        parts.push("Source: Discord event".to_string());
    }
    parts.join("\n")
}

/// Right-click menu for an upcoming-stream entry (calendar chip + day popup): copy
/// its URL / platform / title / channel / full details, or open it in the browser.
/// All actions are immediate (copy/open go straight through the egui context).
fn schedule_copy_menu(ui: &mut egui::Ui, s: &UpcomingStream) {
    ui.set_min_width(160.0);
    if ui
        .add_enabled(!s.url.is_empty(), egui::Button::new("📋  Copy URL"))
        .clicked()
    {
        ui.ctx().copy_text(s.url.clone());
        ui.close();
    }
    if ui.button("📋  Copy platform").clicked() {
        ui.ctx().copy_text(s.platform().label().to_string());
        ui.close();
    }
    if ui
        .add_enabled(!s.title.is_empty(), egui::Button::new("📋  Copy title"))
        .clicked()
    {
        ui.ctx().copy_text(s.title.clone());
        ui.close();
    }
    if ui.button("📋  Copy channel").clicked() {
        ui.ctx().copy_text(s.channel_name.clone());
        ui.close();
    }
    if ui.button("📋  Copy details").clicked() {
        ui.ctx().copy_text(schedule_detail_line(s));
        ui.close();
    }
    ui.separator();
    if ui
        .add_enabled(!s.url.is_empty(), egui::Button::new("🌐  Open in browser"))
        .clicked()
    {
        ui.ctx().open_url(egui::OpenUrl::new_tab(s.url.clone()));
        ui.close();
    }
    ui.separator();
    if ui.button("📺  Go to channel").clicked() {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_jump"), s.monitor_id));
        ui.close();
    }
    if ui.button("▶  Start recording").clicked() {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_start"), s.monitor_id));
        ui.close();
    }
}

/// One detailed schedule row (⚠ if colliding · time · platform · channel — title
/// (category)) with a hover detail and the copy context menu. Shared by the day
/// popup and the Day view.
fn schedule_detail_row(
    ui: &mut egui::Ui,
    s: &UpcomingStream,
    colliding: bool,
    ptex: &PlatformTextures,
) {
    let color = channel_event_color(s.channel_id, &s.channel_color);
    let resp = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            // 3px colored left stripe
            let (stripe_rect, _) = ui.allocate_exact_size(
                egui::vec2(3.0, ui.text_style_height(&egui::TextStyle::Body)),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(stripe_rect, egui::CornerRadius::same(2), color);
            if colliding {
                ui.colored_label(HL_COLLISION, "⚠");
            }
            ui.label(egui::RichText::new(fmt_time_range(s.start_time, s.end_time)).monospace());
            platform_icon(ui, ptex, s.platform());
            let mut line = s.channel_name.clone();
            if !s.title.is_empty() {
                line.push_str(" — ");
                line.push_str(&s.title);
            }
            if !s.category.is_empty() {
                line.push_str(&format!("  ({})", s.category));
            }
            ui.add(egui::Label::new(line).truncate());
        })
        .response
        .interact(egui::Sense::click());
    resp.on_hover_text(schedule_detail_line(s))
        .context_menu(|ui| schedule_copy_menu(ui, s));
}

/// Subtle background tint for today's calendar cell (low-alpha accent, so it reads
/// on both light and dark themes).
const TODAY_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x2c, 0x4a, 0x6e, 0x40);
/// Marker color for a stream that overlaps another (a scheduling collision).
const HL_COLLISION: egui::Color32 = egui::Color32::from_rgb(0xff, 0x8a, 0x5c);

// ── Time-grid calendar constants ─────────────────────────────────────────────
/// Pixels per hour in the time-grid (Week + Day) views.
const SCHED_HOUR_PX: f32 = 60.0;
/// Total scrollable height of the 24-hour time grid.
const SCHED_TOTAL_H: f32 = 24.0 * SCHED_HOUR_PX;
/// Width of the left-side "HH:MM" hour-label column in the time grid.
const SCHED_TIME_COL_W: f32 = 44.0;
/// Gap between day columns in the time grid.
const SCHED_COL_GAP: f32 = 4.0;
/// Minimum event block height so zero/short-duration events remain clickable.
const SCHED_MIN_BLOCK_H: f32 = 22.0;

/// Seconds past local midnight for a unix timestamp (for positioning on the time grid).
fn secs_since_midnight(unix: i64) -> f32 {
    use chrono::Timelike;
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| {
            let local = dt.with_timezone(&chrono::Local);
            (local.hour() * 3600 + local.minute() * 60 + local.second()) as f32
        })
        .unwrap_or(0.0)
}

/// Assign events to non-overlapping lanes (columns within a day column) so that
/// concurrent events are displayed side-by-side. Returns `(stream_idx, lane, total_lanes)`.
fn layout_event_lanes(
    indices: &[usize],
    all: &[UpcomingStream],
) -> Vec<(usize, usize, usize)> {
    if indices.is_empty() {
        return vec![];
    }
    // lane_end[i] = effective end of the last event assigned to lane i
    let mut lane_end: Vec<i64> = Vec::new();
    let mut assignments: Vec<(usize, usize)> = Vec::new(); // (stream_idx, lane)

    for &idx in indices {
        let s = &all[idx];
        let end = effective_end(s);
        // Find the first lane that is free at s.start_time.
        let lane = lane_end
            .iter()
            .position(|&le| le <= s.start_time)
            .unwrap_or_else(|| {
                lane_end.push(0);
                lane_end.len() - 1
            });
        lane_end[lane] = end;
        assignments.push((idx, lane));
    }

    let total = lane_end.len().max(1);
    assignments.into_iter().map(|(i, l)| (i, l, total)).collect()
}

/// Draw a 24-hour time-grid for one or more day columns. Called by both the Week
/// and Day views. `days` lists the calendar dates; `col_w` is the per-column
/// content width (excluding the time label column and gaps).
#[allow(clippy::too_many_arguments)]
fn schedule_time_grid(
    ui: &mut egui::Ui,
    id: &str,
    days: &[chrono::NaiveDate],
    col_w: f32,
    all: &[UpcomingStream],
    by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
    collide: &HashSet<usize>,
    open_day: &mut Option<chrono::NaiveDate>,
) {
    use chrono::Timelike;
    // Scroll to show the current local hour, but only the first time the view
    // appears so subsequent frames don't fight the user's manual scroll position.
    let init_id = egui::Id::new(id).with("scroll_init");
    let already_init: bool = ui.ctx().data(|d| d.get_temp(init_id).unwrap_or(false));
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false]);
    if !already_init {
        let now_hour = chrono::Local::now().hour() as f32;
        let initial_offset = (now_hour * SCHED_HOUR_PX - 120.0).max(0.0);
        scroll = scroll.vertical_scroll_offset(initial_offset);
        ui.ctx().data_mut(|d| d.insert_temp(init_id, true));
    }

    let grid_w = SCHED_TIME_COL_W + days.len() as f32 * (col_w + SCHED_COL_GAP);

    let mut hovered_tip: Option<String> = None;
    let mut clicked_day: Option<chrono::NaiveDate> = None;
    let mut ctx_stream: Option<usize> = None; // stream idx for context menu

    scroll.show(ui, |ui| {
            let (response, painter) = ui.allocate_painter(
                egui::vec2(grid_w, SCHED_TOTAL_H),
                egui::Sense::click(),
            );
            let origin = response.rect.min;

            // ── Hour grid lines + labels ──────────────────────────────────
            let grid_line_color = egui::Color32::from_white_alpha(18);
            let half_line_color = egui::Color32::from_white_alpha(8);
            let label_color = ui.visuals().weak_text_color();
            let font = egui::FontId::proportional(10.0);

            for hour in 0u32..24 {
                let y = origin.y + hour as f32 * SCHED_HOUR_PX;
                // Hour label
                painter.text(
                    egui::pos2(origin.x + 2.0, y + 2.0),
                    egui::Align2::LEFT_TOP,
                    format!("{hour:02}:00"),
                    font.clone(),
                    label_color,
                );
                // Full-hour line across all columns
                painter.line_segment(
                    [
                        egui::pos2(origin.x + SCHED_TIME_COL_W, y),
                        egui::pos2(origin.x + grid_w, y),
                    ],
                    egui::Stroke::new(1.0, grid_line_color),
                );
                // Half-hour line (lighter)
                let yh = y + SCHED_HOUR_PX / 2.0;
                painter.line_segment(
                    [
                        egui::pos2(origin.x + SCHED_TIME_COL_W, yh),
                        egui::pos2(origin.x + grid_w, yh),
                    ],
                    egui::Stroke::new(1.0, half_line_color),
                );
            }

            // ── Vertical column dividers ──────────────────────────────────
            let divider_color = egui::Color32::from_white_alpha(22);
            for col in 0..=days.len() {
                let x = origin.x + SCHED_TIME_COL_W + col as f32 * (col_w + SCHED_COL_GAP);
                painter.line_segment(
                    [egui::pos2(x, origin.y), egui::pos2(x, origin.y + SCHED_TOTAL_H)],
                    egui::Stroke::new(1.0, divider_color),
                );
            }

            // ── Current-time indicator (red line) ─────────────────────────
            {
                let now = chrono::Local::now();
                let today = now.date_naive();
                let now_secs = (now.hour() * 3600 + now.minute() * 60 + now.second()) as f32;
                if let Some(col_idx) = days.iter().position(|&d| d == today) {
                    let x_start = origin.x + SCHED_TIME_COL_W + col_idx as f32 * (col_w + SCHED_COL_GAP);
                    let x_end = x_start + col_w;
                    let y = origin.y + now_secs / 3600.0 * SCHED_HOUR_PX;
                    painter.line_segment(
                        [egui::pos2(x_start, y), egui::pos2(x_end, y)],
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(0xff, 0x44, 0x44)),
                    );
                    // Small circle at left edge
                    painter.circle_filled(
                        egui::pos2(x_start, y),
                        4.0,
                        egui::Color32::from_rgb(0xff, 0x44, 0x44),
                    );
                }
            }

            // ── Event blocks ──────────────────────────────────────────────
            let hover_pos = response.hover_pos();

            // Collect all event rects for hit-testing after painting.
            let mut event_rects: Vec<(egui::Rect, usize, chrono::NaiveDate)> = Vec::new();

            for (col_idx, &day) in days.iter().enumerate() {
                let col_x = origin.x + SCHED_TIME_COL_W + col_idx as f32 * (col_w + SCHED_COL_GAP);
                let indices = by_day.get(&day).map(Vec::as_slice).unwrap_or(&[]);
                let layout = layout_event_lanes(indices, all);

                for (stream_idx, lane, total_lanes) in layout {
                    let s = &all[stream_idx];
                    let start_secs = secs_since_midnight(s.start_time);
                    let end_secs = secs_since_midnight(effective_end(s));

                    // Clip to day boundaries (midnight transitions handled by bucketing).
                    let end_secs = if end_secs <= start_secs {
                        // Event ends on next day — clip at midnight.
                        SCHED_TOTAL_H / SCHED_HOUR_PX * 3600.0
                    } else {
                        end_secs
                    };
                    let duration_secs = (end_secs - start_secs).max(0.0);

                    let top = origin.y + start_secs / 3600.0 * SCHED_HOUR_PX;
                    let block_h = (duration_secs / 3600.0 * SCHED_HOUR_PX).max(SCHED_MIN_BLOCK_H);
                    let lane_w = (col_w - 2.0 * (total_lanes as f32 - 1.0)) / total_lanes as f32;
                    let left = col_x + 1.0 + lane as f32 * (lane_w + 2.0);

                    let block_rect = egui::Rect::from_min_size(
                        egui::pos2(left, top),
                        egui::vec2(lane_w, block_h),
                    );
                    event_rects.push((block_rect, stream_idx, day));

                    let color = channel_event_color(s.channel_id, &s.channel_color);
                    let hovered = hover_pos.is_some_and(|p| block_rect.contains(p));
                    let fill = if hovered {
                        color // brighter on hover — stroke already provides accent
                    } else {
                        egui::Color32::from_rgba_unmultiplied(
                            color.r(), color.g(), color.b(), 210,
                        )
                    };
                    let rounding = egui::CornerRadius::same(4);
                    painter.rect_filled(block_rect, rounding, fill);
                    // Slightly darker left edge strip for depth
                    painter.rect_filled(
                        egui::Rect::from_min_size(block_rect.min, egui::vec2(3.0, block_h)),
                        egui::CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 },
                        egui::Color32::from_rgba_unmultiplied(
                            (color.r() as i32 - 30).max(0) as u8,
                            (color.g() as i32 - 30).max(0) as u8,
                            (color.b() as i32 - 30).max(0) as u8,
                            255,
                        ),
                    );
                    if collide.contains(&stream_idx) {
                        painter.rect_stroke(
                            block_rect,
                            rounding,
                            egui::Stroke::new(1.5, HL_COLLISION),
                            egui::StrokeKind::Inside,
                        );
                    }

                    // Text inside the block (white, clipped to block height)
                    let text_rect = block_rect.shrink2(egui::vec2(5.0, 3.0));
                    if text_rect.height() >= 12.0 {
                        let name_font = egui::FontId::proportional(11.0);
                        let time_font = egui::FontId::proportional(10.0);
                        let white = egui::Color32::WHITE;
                        let text_y = text_rect.top();
                        // Channel name
                        painter.text(
                            egui::pos2(text_rect.left(), text_y),
                            egui::Align2::LEFT_TOP,
                            &s.channel_name,
                            name_font,
                            white,
                        );
                        // Time range (below channel name if enough space)
                        if block_h >= 36.0 {
                            painter.text(
                                egui::pos2(text_rect.left(), text_y + 13.0),
                                egui::Align2::LEFT_TOP,
                                fmt_time_range(s.start_time, s.end_time),
                                time_font.clone(),
                                egui::Color32::from_white_alpha(200),
                            );
                        }
                        // Title (if enough height)
                        if block_h >= 56.0 && !s.title.is_empty() {
                            painter.text(
                                egui::pos2(text_rect.left(), text_y + 26.0),
                                egui::Align2::LEFT_TOP,
                                &s.title,
                                time_font,
                                egui::Color32::from_white_alpha(180),
                            );
                        }
                    }

                    if hovered {
                        hovered_tip = Some(schedule_detail_line(s));
                    }
                }
            }

            // ── Hit-testing: click + right-click ─────────────────────────
            if response.clicked() {
                if let Some(pos) = response.interact_pointer_pos() {
                    for &(rect, _idx, day) in &event_rects {
                        if rect.contains(pos) {
                            clicked_day = Some(day);
                            break;
                        }
                    }
                }
            }
            if response.secondary_clicked() {
                if let Some(pos) = response.interact_pointer_pos() {
                    for &(rect, idx, _day) in &event_rects {
                        if rect.contains(pos) {
                            ctx_stream = Some(idx);
                            break;
                        }
                    }
                }
            }

            // Tooltip for hovered event — only set when pointer is over an event block.
            if let Some(tip) = hovered_tip {
                response.on_hover_text(tip);
            }
        });

    // Context menu (must be outside the closure since it borrows ui)
    if let Some(idx) = ctx_stream {
        let s = &all[idx];
        // Use a small invisible area near the mouse for the context menu.
        // egui doesn't easily support per-painted-rect context menus, so we
        // surface a right-click popup via a dummy response.
        let resp = ui.allocate_rect(
            egui::Rect::from_min_size(
                ui.ctx().pointer_interact_pos().unwrap_or_default(),
                egui::vec2(1.0, 1.0),
            ),
            egui::Sense::click(),
        );
        resp.context_menu(|ui| schedule_copy_menu(ui, s));
    }

    if let Some(day) = clicked_day {
        *open_day = Some(day);
    }
}

/// Success/affirmative green, shared across the table (recording "completed",
/// video "completed", ad-free "Yes").
const SUCCESS_GREEN: egui::Color32 = egui::Color32::from_rgb(0x57, 0xc7, 0x57);

/// Streams-row background tint while an ad is playing (amber) / after an error
/// (red). Recording + keyboard-selected rows reuse the theme's selection accent.
const HL_AD: egui::Color32 = egui::Color32::from_rgb(0x7a, 0x5a, 0x12);
const HL_ERROR: egui::Color32 = egui::Color32::from_rgb(0x6e, 0x2f, 0x2f);

/// Background tint for a Streams row, by state (highest priority first): an ad is
/// playing > recording > last poll/recording errored > keyboard-selected.
/// `accent` is the theme's selection color (so recording/selected keep the
/// existing look). When `status_colors` is off, the status tints (ad / recording
/// / error) are suppressed but a keyboard-`selected` row is still highlighted.
/// `None` = no tint.
fn row_tint(
    recording: bool,
    ad_running: bool,
    errored: bool,
    selected: bool,
    accent: egui::Color32,
    status_colors: bool,
) -> Option<egui::Color32> {
    if status_colors {
        if recording && ad_running {
            return Some(HL_AD);
        } else if recording {
            return Some(accent);
        } else if errored {
            return Some(HL_ERROR);
        }
    }
    selected.then_some(accent)
}

/// Background tint for a Videos row, by download status: in-flight = the theme
/// accent, failed = the error red. `None` (incl. when `status_colors` is off) =
/// no tint. Mirrors [`row_tint`] for the Streams table.
fn video_row_tint(status: &str, accent: egui::Color32, status_colors: bool) -> Option<egui::Color32> {
    if !status_colors {
        return None;
    }
    match status {
        "downloading" | "queued" => Some(accent),
        "failed" => Some(HL_ERROR),
        _ => None,
    }
}

/// Whether a monitor's last poll or recording ended in an error/failure.
fn monitor_errored(m: &MonitorWithChannel) -> bool {
    matches!(m.monitor.last_state.as_str(), "error" | "failed")
        || m.last_recording_status.as_deref() == Some("failed")
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
    if let Some(b) = breaks.filter(|b| !b.is_empty()) {
        s.push('\n');
        s.push_str(&ad_cut_lines(b).join("\n"));
    }
    s
}

/// Render one Ads / Ad-time table cell. Blank `text` renders nothing. The hover
/// tooltip (the cut list) is built lazily, only when hovered. When `clickable_rec`
/// is set the cell senses a double-click and returns that recording id (to open
/// its cut-list popup); `detail` is the per-break list when loaded.
fn ad_cell(
    ui: &mut egui::Ui,
    text: String,
    count: i64,
    secs: i64,
    detail: Option<&Vec<AdBreak>>,
    clickable_rec: Option<i64>,
) -> Option<i64> {
    if text.is_empty() {
        return None;
    }
    let label = if clickable_rec.is_some() {
        egui::Label::new(text).sense(egui::Sense::click())
    } else {
        egui::Label::new(text)
    };
    let resp = ui
        .add(label)
        .on_hover_ui(|ui| {
            ui.label(ad_tooltip(count, secs, detail));
        });
    match clickable_rec {
        Some(rec) if resp.double_clicked() => Some(rec),
        _ => None,
    }
}

/// Count string for a Changes cell ("" when zero, so empty cells render nothing).
fn fmt_meta_count(n: i64) -> String {
    if n > 0 { n.to_string() } else { String::new() }
}

/// Render a "Next stream" cell: blank when no upcoming stream is known, else the
/// scheduled start datetime. When `clickable`, a double-click returns true so the
/// caller can open the full-schedule popup; the hover shows the title.
fn next_stream_cell(ui: &mut egui::Ui, at: Option<i64>, title: &str, clickable: bool) -> bool {
    let Some(at) = at.filter(|&a| a > 0) else {
        return false;
    };
    let label = if clickable {
        egui::Label::new(fmt_datetime_short(at)).sense(egui::Sense::click())
    } else {
        egui::Label::new(fmt_datetime_short(at))
    };
    let resp = ui.add(label).on_hover_ui(|ui| {
        if title.is_empty() {
            ui.label("Next scheduled stream.");
        } else {
            ui.label(format!("Next: {title}"));
        }
        if clickable {
            ui.label("Double-click for the full upcoming schedule.");
        }
    });
    clickable && resp.double_clicked()
}

/// One human-readable line per *actual* metadata change (offset + kind +
/// `old → new`). The initial value of each field (logged with an empty
/// `old_value`) is the starting state, not a change, so it's skipped — it still
/// shows as the `old` side of the first real change.
fn meta_change_lines(changes: &[StreamMetaChange]) -> Vec<String> {
    changes
        .iter()
        .filter(|c| !c.old_value.is_empty())
        .map(|c| {
            let at = fmt_duration(c.at_secs.max(0));
            let kind = if c.kind == "category" { "Category" } else { "Title" };
            let new = if c.new_value.is_empty() {
                "(cleared)"
            } else {
                c.new_value.as_str()
            };
            format!("{at}  {kind}: {} → {new}", c.old_value)
        })
        .collect()
}

/// What the metadata-change popup shows.
#[derive(Clone)]
enum MetaPopup {
    /// A single take's change log (recording id).
    Take(i64),
    /// A whole stream's takes — `(recording id, started_at)`, oldest-first —
    /// aggregated chronologically with the per-take re-baselines omitted.
    Stream(Vec<(i64, i64)>),
}

/// Source platform of a captured chat message (drives username colouring).
#[derive(Clone)]
enum ChatPlatform {
    YouTube,
    Twitch,
}

/// A single parsed chat message (YouTube `.live_chat.json` or Twitch `.chat.jsonl`).
#[derive(Clone)]
struct ChatMessage {
    /// Seconds from stream start (negative = chat arrived before we started recording).
    timestamp_secs: f64,
    author: String,
    text: String,
    /// Twitch: raw IRC badge segment per entry, e.g. `"subscriber/12"`.
    /// YouTube: badge tooltip text, e.g. `"Member"`.
    badges: Vec<String>,
    /// Explicit hex colour from Twitch USERCOLOR; `None` when unset or YouTube.
    color_override: Option<egui::Color32>,
    platform: ChatPlatform,
}

#[derive(Clone)]
enum ChatLoadState {
    Loading,
    Loaded(Vec<ChatMessage>),
    NoFile,
    Error(String),
}

struct ChatPopup {
    monitor_name: String,
    /// Currently-viewed recording (`None` = monitor has no recordings at all).
    recording: Option<Recording>,
    all_recordings: Vec<Recording>,
    load_state: Arc<Mutex<ChatLoadState>>,
    search: String,
    /// When `true`: show the entire log from the top (no cap, stick-to-bottom off).
    /// When `false` (default): show the last 500 msgs and stick to bottom.
    full_view: bool,
    /// When the popup last triggered a background re-read of the chat file.
    /// Used to tail a live recording: the file is re-parsed every few seconds
    /// while `recording.ended_at` is `None`.
    last_reload: std::time::Instant,
}

/// Merge a stream's takes into one chronological change list. Each take's offsets
/// (`at_secs`, relative to that take's start) are rebased onto the whole stream's
/// timeline (`take.started_at - stream_start + at_secs`); the rows are then sorted
/// and run through [`meta_change_lines`], which drops each take's initial value
/// (empty `old_value`) — so a take re-observing the value the previous take ended
/// on adds no duplicate line, while genuine changes are kept.
fn aggregate_stream_changes(takes: &[(i64, Vec<StreamMetaChange>)]) -> Vec<StreamMetaChange> {
    let stream_start = takes.iter().map(|(s, _)| *s).min().unwrap_or(0);
    let mut all: Vec<StreamMetaChange> = Vec::new();
    for (started_at, rows) in takes {
        for r in rows {
            let mut adj = r.clone();
            adj.at_secs = (started_at - stream_start) + r.at_secs;
            all.push(adj);
        }
    }
    all.sort_by_key(|c| (c.at_secs, c.id));
    all
}

/// Multi-line tooltip body for a Changes cell: a heading plus the change list
/// (just the heading when the detail isn't loaded or there are no changes).
fn meta_tooltip(count: i64, changes: Option<&Vec<StreamMetaChange>>) -> String {
    let mut s = format!("{count} title/category change(s) during this recording.");
    if let Some(lines) = changes.map(|c| meta_change_lines(c)).filter(|l| !l.is_empty()) {
        s.push('\n');
        s.push_str(&lines.join("\n"));
    }
    s
}

/// Render one Changes table cell, mirroring [`ad_cell`]: blank when the count is
/// zero, a lazily-built hover list, and (when `clickable`) a double-click to open
/// the change-log popup. Returns whether it was double-clicked so the caller can
/// open the right popup (a single take, or a whole stream's aggregated takes).
fn meta_cell(
    ui: &mut egui::Ui,
    count: i64,
    detail: Option<&Vec<StreamMetaChange>>,
    clickable: bool,
) -> bool {
    let text = fmt_meta_count(count);
    if text.is_empty() {
        return false;
    }
    let label = if clickable {
        egui::Label::new(text).sense(egui::Sense::click())
    } else {
        egui::Label::new(text)
    };
    let resp = ui.add(label).on_hover_ui(|ui| {
        ui.label(meta_tooltip(count, detail));
    });
    clickable && resp.double_clicked()
}

/// Render a current-Title / current-Game cell: blank when empty, otherwise a
/// label truncated to the (width-capped) column. egui shows the full text on
/// hover automatically when the label is elided (`show_tooltip_when_elided`
/// defaults to true), so we add no explicit tooltip — a second one would just
/// stack a duplicate.
fn meta_value_cell(ui: &mut egui::Ui, value: &str) {
    if value.is_empty() {
        return;
    }
    ui.add(egui::Label::new(value).truncate());
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
        "completed" => SUCCESS_GREEN,
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
// Both tables share a tiny model: each row is turned into a `Vec<Cell>` in header
// order. Videos excludes its trailing Actions column (`VIDEO_COLS` = 9); Streams
// keeps a (non-sortable, empty) Actions placeholder slot so the model indices line
// up with `STREAM_COLUMNS` (`STREAM_COLS`). The header renders a click-to-sort
// title + a per-column filter box; `ordered_rows` filters then sorts and returns
// the surviving original-row indices in display order. The data cells themselves
// are still drawn by the existing per-row code, indexed by those original indices.
// (The optional Actions column is skipped at render time, not in the model.)

/// Sortable/filterable Videos columns (Video..File; excludes Actions).
const VIDEO_COLS: usize = 9;

/// One Streams-table column: header title, hover tooltip, minimum width, and
/// whether it takes part in sort/filter (the Actions column doesn't). This list
/// is the single source of truth for the `TableBuilder` columns, the header, and
/// the per-row sort/filter model, so they can't drift — and an extra cell can't
/// be emitted against a missing column width (which panics egui_extras).
struct StreamCol {
    title: &'static str,
    tooltip: &'static str,
    min_width: f32,
    /// Starting (and clipped) width for content-capped columns whose value can be
    /// long (the current Title / Game); the cell truncates to it and shows the
    /// full text on hover. `0.0` = auto-size to content (the default).
    initial: f32,
    sortable: bool,
}

/// The Streams columns in display order: Actions and the platform-icon column sit
/// just left of Name; the current Game/Title sit just right of State. Widths are
/// floors — `Column::auto` shrinks tight columns to their content — except the
/// `initial`-width columns, which start narrow and truncate (full value on hover).
const STREAM_COLUMNS: [StreamCol; 20] = [
    StreamCol { title: "Auto", tooltip: "Enable/disable monitoring. A channel's checkbox toggles all its instances at once.", min_width: 36.0, initial: 0.0, sortable: true },
    StreamCol { title: "Actions", tooltip: "Per-row actions: start/stop recording, edit, add instance, open folder, delete.", min_width: 126.0, initial: 0.0, sortable: false },
    StreamCol { title: "Plat", tooltip: "Source platform (icon): Twitch, YouTube, Kick, or a generic URL. A channel shows every platform among its instances.", min_width: 52.0, initial: 0.0, sortable: true },
    StreamCol { title: "Name", tooltip: "Channel (container) name. Expand it to see its instances and recording history.", min_width: 130.0, initial: 0.0, sortable: true },
    StreamCol { title: "Tool", tooltip: "Capture tool: streamlink, yt-dlp, or ffmpeg.", min_width: 60.0, initial: 0.0, sortable: true },
    StreamCol { title: "Detection", tooltip: "How a live stream is detected (API poll, page scrape, Twitch EventSub, or a generic probe).", min_width: 70.0, initial: 0.0, sortable: true },
    StreamCol { title: "Polled", tooltip: "When this instance was last checked, with its poll interval in parentheses (e.g. \"2026-06-21 14:02:33 (60s)\").", min_width: 100.0, initial: 0.0, sortable: true },
    StreamCol { title: "State", tooltip: "Current state (idle / live / recording / failed). Hover a failed row to see why it failed.", min_width: 66.0, initial: 0.0, sortable: true },
    StreamCol { title: "Next stream", tooltip: "Next scheduled stream (Twitch schedule / YouTube upcoming; blank for Kick & generic, or when nothing is scheduled). Hover for its title; double-click for the full upcoming schedule.", min_width: 96.0, initial: 0.0, sortable: true },
    StreamCol { title: "Game", tooltip: "Current game / category of the most recent recording (Twitch, Kick & YouTube — YouTube shows its broad content category; blank for generic URLs). Truncated — hover for the full name.", min_width: 60.0, initial: 96.0, sortable: true },
    StreamCol { title: "Title", tooltip: "Current stream title of the most recent recording (Twitch, Kick & YouTube; blank for generic URLs). Truncated — hover for the full title. Its full change history is in the Changes column.", min_width: 80.0, initial: 170.0, sortable: true },
    StreamCol { title: "Went Live", tooltip: "When the stream went live on the platform (a \"~\" prefix means it's our approximate time).", min_width: 96.0, initial: 0.0, sortable: true },
    StreamCol { title: "Started On", tooltip: "When recording started.", min_width: 92.0, initial: 0.0, sortable: true },
    StreamCol { title: "Lost time", tooltip: "How much of the start was missed. Drops to 0 once a from-start capture catches up to the live edge.", min_width: 52.0, initial: 0.0, sortable: true },
    StreamCol { title: "Duration", tooltip: "How long we've recorded (ticks while live).", min_width: 56.0, initial: 0.0, sortable: true },
    StreamCol { title: "Ads", tooltip: "Ad breaks detected (Twitch + streamlink); each is a hard cut. Hover or double-click for the list.", min_width: 38.0, initial: 0.0, sortable: true },
    StreamCol { title: "Ad time", tooltip: "Total advertisement time skipped.", min_width: 52.0, initial: 0.0, sortable: true },
    StreamCol { title: "Ad-free", tooltip: "Marked or auto-detected ad-free (sub / Turbo / Premium) — captures have no ad-break cuts.", min_width: 54.0, initial: 0.0, sortable: true },
    StreamCol { title: "Changes", tooltip: "Title / game-category changes logged during the recording. Hover or double-click for the log.", min_width: 56.0, initial: 0.0, sortable: true },
    StreamCol { title: "Added", tooltip: "When the channel was added.", min_width: 84.0, initial: 0.0, sortable: true },
];

/// Total Streams columns (includes the non-sortable Actions slot).
const STREAM_COLS: usize = STREAM_COLUMNS.len();

/// Index of the optional Actions column in [`STREAM_COLUMNS`]. When the Actions
/// column is hidden, this column is skipped in the builder, header, and every row
/// renderer (kept in the sort/filter model — it's a non-sortable, empty slot).
const STREAM_ACTIONS_COL: usize = 1;

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
    tooltip: &str,
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
        let hover = if tooltip.is_empty() {
            "Click to sort (click again to reverse)".to_string()
        } else {
            format!("{tooltip}\n\n(click to sort; click again to reverse)")
        };
        let resp = ui
            .add(egui::Button::new(egui::RichText::new(format!("{title}{arrow}")).strong()).frame(false))
            .on_hover_text(hover);
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
        "completed" => SUCCESS_GREEN,
        "failed" => Color32::from_rgb(0xe0, 0x6c, 0x6c),
        // Cut short by app shutdown — amber, not an error but not clean either.
        "aborted" => Color32::from_rgb(0xe0, 0xa8, 0x50),
        // "ended" (stream had ended / wasn't live), "stopped", "orphaned": neutral
        // gray — terminal but not an error.
        _ => Color32::from_gray(0xa0),
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

/// The channel's most-recent-activity instance, which drives the container row's
/// time + ad columns. `None` only for an empty container. Shared by the
/// sort/filter model and the row render so they can't drift.
fn channel_primary<'a>(monitors: &[&'a MonitorWithChannel]) -> Option<&'a MonitorWithChannel> {
    monitors
        .iter()
        .copied()
        .max_by_key(|m| m.last_recording_started.unwrap_or(0))
}

/// How many of the channel's instances are ad-free (manual flag or detected sub).
fn channel_ad_free_count(monitors: &[&MonitorWithChannel]) -> usize {
    monitors
        .iter()
        .filter(|m| ad_free_status(m.monitor.ad_free, m.ad_free_sub).is_some())
        .count()
}

/// Sort/filter cells for a channel container's top-level row (matches the table
/// columns). `channel` is the container; `monitors` are its instances (possibly
/// none, for an empty container).
fn channel_cells(channel: &Channel, monitors: &[&MonitorWithChannel], now: i64) -> Vec<Cell> {
    if monitors.is_empty() {
        // Empty container: just the name + "added"; everything else blank. Index
        // order matches STREAM_COLUMNS: On=0, Name=3, Added=last.
        let mut cells: Vec<Cell> = (0..STREAM_COLS).map(|_| Cell::text(String::new())).collect();
        cells[0] = Cell::num(0.0, "off");
        cells[3] = Cell::text(channel.name.clone());
        cells[STREAM_COLS - 1] = Cell::num(channel.created_at as f64, fmt_date(channel.created_at));
        return cells;
    }
    let all_enabled = monitors.iter().all(|m| m.monitor.enabled);
    let any_recording = monitors
        .iter()
        .any(|m| m.last_recording_status.as_deref() == Some("recording"));
    // The most recent recording across instances drives the time columns.
    let primary = channel_primary(monitors).unwrap_or(monitors[0]);
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
    // In STREAM_COLUMNS order: On, Actions(empty), Plat, Name, Tool, Detection,
    // Polled, State, Next stream, Game, Title, Went Live, Started On, Lost,
    // Duration, Ads, Ad time, Ad-free, Changes, Added.
    vec![
        Cell::num(
            if all_enabled { 1.0 } else { 0.0 },
            if all_enabled { "on" } else { "off" },
        ),
        Cell::text(String::new()), // actions (not sortable/filterable)
        Cell::text(
            channel_platform(monitors)
                .map(|p| p.label().to_string())
                .unwrap_or_else(|| "mixed".into()),
        ),
        Cell::text(channel.name.clone()),
        Cell::text(tool),
        Cell::text(String::new()), // detection
        Cell::num(last as f64, fmt_datetime_short(last)), // polled (datetime only)
        Cell::text(if any_recording { "recording".to_string() } else { String::new() }),
        {
            // Sort/show the channel's SOONEST upcoming stream across its instances.
            let next_at = monitors.iter().filter_map(|m| m.next_stream_at).min();
            Cell::num(
                next_at.unwrap_or(0) as f64,
                next_at.map(fmt_datetime_short).unwrap_or_default(),
            )
        },
        Cell::text(primary.last_recording_category.clone()),
        Cell::text(primary.last_recording_title.clone()),
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
            let (label, key) =
                ad_free_summary(channel_ad_free_count(monitors), monitors.len());
            Cell::num(key, label)
        },
        Cell::num(
            primary.last_recording_meta_changes as f64,
            fmt_meta_count(primary.last_recording_meta_changes),
        ),
        Cell::num(channel.created_at as f64, fmt_date(channel.created_at)),
    ]
}

/// Self-mutating actions collected while rendering a capture-instance row.
#[derive(Default)]
struct RowActions {
    start: Option<i64>,                 // monitor id
    stop: Option<i64>,                  // monitor id
    stop_chat: Option<i64>,             // monitor id
    view_chat: Option<i64>,             // monitor id
    edit: Option<i64>,                  // monitor id
    add_instance: Option<i64>,          // channel id
    delete: Option<(i64, String)>,      // (monitor id, channel name)
    toggle_enabled: Option<(i64, bool)>,
    select: Option<i64>,                // monitor id
    open_schedule: Option<i64>,         // monitor id (open its Next stream popup)
    properties: Option<i64>,            // monitor id
}

/// Render one capture-instance (monitor) row across all columns, with the Name
/// column carrying the tree disclosure. Returns true if the disclosure (the
/// row's stream history) was toggled. Self-mutating picks land in `a`.
#[allow(clippy::too_many_arguments)]
fn render_instance_row(
    tr: &mut egui_extras::TableRow<'_, '_>,
    row: &MonitorWithChannel,
    ptex: &PlatformTextures,
    now: i64,
    recording: bool,
    chat_active: bool,
    highlight: bool,
    depth: usize,
    has_history: bool,
    expanded: bool,
    show_actions: bool,
    a: &mut RowActions,
) -> bool {
    let m = &row.monitor;
    let rec = recording_cells(row, now);
    // The caller set the row's tint color (recording/ad/error/selected); paint it.
    tr.set_selected(highlight);

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
        if chat_active {
            if ui.button("💬  Stop chat download").clicked() {
                a.stop_chat = Some(m.id);
                ui.close();
            }
        }
        if m.chat_log {
            if ui.button("💬  View chat").clicked() {
                a.view_chat = Some(m.id);
                ui.close();
            }
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
        ui.separator();
        if ui.button("ℹ  Properties").clicked() {
            a.properties = Some(m.id);
            ui.close();
        }
    };

    let mut disclosure_clicked = false;
    // Column order: On · Actions · Platform · Name · Tool · Detection · Polled ·
    // State · Next stream · Game · Title · Went Live · Started On · Lost · Duration
    // · Ads · Ad time · Ad-free · Changes · Added.
    tr.col(|ui| {
        let mut on = m.enabled;
        let cb = ui.checkbox(&mut on, "");
        if cb.changed() {
            a.toggle_enabled = Some((m.id, on));
        }
        cb.context_menu(|ui| add_menu(ui, a));
    });
    if show_actions {
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
    }
    tr.col(|ui| {
        platform_icon(ui, ptex, m.platform()).on_hover_text(m.platform().label());
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
        ui.label(fmt_polled(m.last_checked_at, m.poll_interval_secs))
            .on_hover_text(format!(
                "Last checked {} · polled every {}s",
                if m.last_checked_at.unwrap_or(0) > 0 {
                    fmt_datetime_short(m.last_checked_at.unwrap_or(0))
                } else {
                    "never".to_string()
                },
                m.poll_interval_secs,
            ));
    });
    tr.col(|ui| {
        ui.horizontal(|ui| {
            let resp = ui.label(&m.last_state);
            if m.last_state == "failed" || row.last_recording_status.as_deref() == Some("failed") {
                resp.on_hover_text(fail_hover(&row.last_recording_log));
            }
            if chat_active {
                ui.colored_label(
                    egui::Color32::from_rgb(0x4a, 0xc2, 0xff),
                    egui::RichText::new("Chat ●").small(),
                )
                .on_hover_text(
                    "Live-chat download is still running.\n\
                     Right-click → Stop chat download to abort it.",
                );
            }
        });
    });
    tr.col(|ui| {
        if next_stream_cell(ui, row.next_stream_at, &row.next_stream_title, true) {
            a.open_schedule = Some(m.id);
        }
    });
    tr.col(|ui| {
        meta_value_cell(ui, &row.last_recording_category);
    });
    tr.col(|ui| {
        meta_value_cell(ui, &row.last_recording_title);
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
    let (ad_c, ad_s) = (row.last_recording_ad_count, row.last_recording_ad_secs);
    tr.col(|ui| {
        ad_cell(ui, fmt_ad_count(ad_c), ad_c, ad_s, None, None);
    });
    tr.col(|ui| {
        ad_cell(ui, fmt_ad_time(ad_s), ad_c, ad_s, None, None);
    });
    tr.col(|ui| {
        if let Some((label, hover)) = ad_free_status(m.ad_free, row.ad_free_sub) {
            ui.colored_label(SUCCESS_GREEN, label).on_hover_text(hover);
        }
    });
    tr.col(|ui| {
        meta_cell(ui, row.last_recording_meta_changes, None, false);
    });
    tr.col(|ui| {
        ui.label(fmt_date(row.channel.created_at));
    });

    let row_resp = tr.response();
    if row_resp.clicked() || row_resp.secondary_clicked() {
        a.select = Some(m.id);
    }
    row_resp.context_menu(|ui| add_menu(ui, a));
    disclosure_clicked
}

/// Draw a small colored brand badge for the platform.
fn platform_badge(ui: &mut egui::Ui, platform: Platform) -> egui::Response {
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
    )
}

/// Platform favicons, decoded to raw 32×32 RGBA at build time (see build.rs) and
/// embedded here so no image decoder ships in the binary.
const ICON_SRC: usize = 32;
/// On-screen icon size (favicons are designed for small sizes).
const ICON_PX: f32 = 16.0;
static TWITCH_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_twitch.rgba"));
static YOUTUBE_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_youtube.rgba"));
static KICK_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_kick.rgba"));

/// GPU textures for the platform favicons, uploaded once and cheaply cloned
/// (each `TextureHandle` is reference-counted).
#[derive(Clone)]
struct PlatformTextures {
    twitch: egui::TextureHandle,
    youtube: egui::TextureHandle,
    kick: egui::TextureHandle,
}

impl PlatformTextures {
    fn load(ctx: &egui::Context) -> PlatformTextures {
        let mk = |name: &str, rgba: &[u8]| {
            let image = egui::ColorImage::from_rgba_unmultiplied([ICON_SRC, ICON_SRC], rgba);
            ctx.load_texture(format!("platform_{name}"), image, egui::TextureOptions::LINEAR)
        };
        PlatformTextures {
            twitch: mk("twitch", TWITCH_ICON),
            youtube: mk("youtube", YOUTUBE_ICON),
            kick: mk("kick", KICK_ICON),
        }
    }

    /// The favicon for a platform, or `None` for `Generic` (no favicon → badge).
    fn get(&self, p: Platform) -> Option<&egui::TextureHandle> {
        match p {
            Platform::Twitch => Some(&self.twitch),
            Platform::YouTube => Some(&self.youtube),
            Platform::Kick => Some(&self.kick),
            Platform::Generic => None,
        }
    }
}

/// Draw one platform's icon (its favicon, or the colored badge for Generic),
/// returning the response so callers can attach the platform name on hover.
fn platform_icon(ui: &mut egui::Ui, ptex: &PlatformTextures, platform: Platform) -> egui::Response {
    match ptex.get(platform) {
        Some(handle) => {
            let tex = egui::load::SizedTexture::new(handle.id(), egui::vec2(ICON_PX, ICON_PX));
            ui.image(tex)
        }
        None => platform_badge(ui, platform),
    }
}

/// Draw the platform icon(s) for a cell: one per distinct platform, side by side
/// (a channel may span several). Each shows the platform name on hover.
fn platform_icons(ui: &mut egui::Ui, ptex: &PlatformTextures, platforms: &[Platform]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for &p in platforms {
            platform_icon(ui, ptex, p).on_hover_text(p.label());
        }
    });
}

/// Distinct platforms among a channel's instances, in first-seen order.
fn channel_platforms(monitors: &[&MonitorWithChannel]) -> Vec<Platform> {
    let mut out: Vec<Platform> = Vec::new();
    for m in monitors {
        let p = m.monitor.platform();
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Tooltip for a failed row: the captured reason (last stderr line), if any.
fn fail_hover(log: &str) -> String {
    match log.lines().map(str::trim).rev().find(|l| !l.is_empty()) {
        Some(reason) => format!("Failed: {reason}"),
        None => "Failed (no captured output).".to_string(),
    }
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
                            env!("APP_VERSION"),
                            env!("GIT_HASH"),
                        ))
                        .small()
                        .color(egui::Color32::from_gray(0x90)),
                    )
                    .on_hover_text(format!(
                        "StreamArchiver v{} · build {}\ncommit {}\ncompiled {built_full}",
                        env!("APP_VERSION"),
                        env!("BUILD_NUMBER"),
                        env!("GIT_HASH"),
                    ));
                    ui.separator();
                    ui.selectable_value(&mut self.view, View::Streams, "Streams");
                    ui.selectable_value(&mut self.view, View::Videos, "Videos");
                    ui.selectable_value(&mut self.view, View::Schedule, "Schedule");
                    ui.selectable_value(&mut self.view, View::Background, "Background");
                    ui.selectable_value(&mut self.view, View::Settings, "Settings");
                    ui.separator();
                    if ui
                        .checkbox(&mut self.status_bgcolor, "Status bgcolor")
                        .on_hover_text(
                            "Tint Streams rows by status (recording / ad playing / failed). \
                             Row selection is still highlighted when this is off.",
                        )
                        .changed()
                    {
                        let _ = self.core.store.set_setting(
                            K_STATUS_BGCOLOR,
                            if self.status_bgcolor { "1" } else { "0" },
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.view == View::Streams {
                            if ui
                                .button("➕ Add stream")
                                .on_hover_text("Create a channel with its first instance (a URL to record)")
                                .clicked()
                            {
                                self.form = Some(MonitorForm::new_channel(
                                    &self.monitor_defaults,
                                    &self.settings.default_output_dir,
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
                                    color: String::new(),
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

        let panel_resp = egui::CentralPanel::default().show_inside(ui, |ui| match self.view {
            View::Streams => self.channels_view(ui),
            View::Videos => self.videos_view(ui),
            View::Schedule => self.schedule_view(ui),
            View::Background => self.background_view(ui),
            View::Settings => self.settings_view(ui),
        });

        // ── Main-panel context menu (right-click on empty space) ──
        let view = self.view;
        let mut ctx_add_stream = false;
        let mut ctx_add_channel = false;
        let mut ctx_refresh_schedule = false;
        let mut ctx_open_proc_mgr = false;
        let mut ctx_save_settings = false;
        panel_resp.response.context_menu(|ui| {
            match view {
                View::Streams => {
                    if ui.button("➕  Add stream").clicked() {
                        ctx_add_stream = true;
                        ui.close();
                    }
                    if ui.button("➕  Add channel").clicked() {
                        ctx_add_channel = true;
                        ui.close();
                    }
                }
                View::Schedule => {
                    if ui.button("⟳  Fetch now").clicked() {
                        ctx_refresh_schedule = true;
                        ui.close();
                    }
                }
                View::Background => {
                    if ui.button("🖥  Process manager").clicked() {
                        ctx_open_proc_mgr = true;
                        ui.close();
                    }
                }
                View::Settings => {
                    if ui.button("💾  Save settings").clicked() {
                        ctx_save_settings = true;
                        ui.close();
                    }
                }
                View::Videos => {}
            }
        });
        if ctx_add_stream {
            self.form = Some(MonitorForm::new_channel(
                &self.monitor_defaults,
                &self.settings.default_output_dir,
            ));
        }
        if ctx_add_channel {
            self.channel_form = Some(ChannelForm {
                id: None,
                name: String::new(),
                color: String::new(),
            });
        }
        if ctx_refresh_schedule {
            self.core.request_schedule_refresh();
            self.reload_schedule();
            self.status = "Fetching latest schedules…".into();
        }
        if ctx_open_proc_mgr {
            self.show_processes = true;
            self.processes_refreshed = None;
        }
        if ctx_save_settings {
            self.save_settings();
        }

        self.form_window(ui.ctx());
        self.channel_form_window(ui.ctx());
        self.confirm_delete_window(ui.ctx());
        self.confirm_delete_channel_window(ui.ctx());
        self.format_probe_window(ui.ctx());
        self.ad_popup_window(ui.ctx());
        self.meta_popup_window(ui.ctx());
        self.schedule_popup_window(ui.ctx());
        self.schedule_day_window(ui.ctx());
        self.chat_popup_window(ui.ctx());
        self.properties_window(ui.ctx());
        self.processes_window(ui.ctx());
        self.format_designer_window(ui.ctx());
        self.confirm_quit_stop_window(ui.ctx());
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
                &self.monitor_defaults,
                &self.settings.default_output_dir,
            ));
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SETTINGS)) {
            self.view = View::Settings;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&REFRESH)) {
            self.reload_rows();
            if self.view == View::Schedule {
                // Force a network re-fetch (not just a DB reload) + show current data.
                self.core.request_schedule_refresh();
                self.reload_schedule();
            }
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
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("del_monitor_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete monitor")
                .with_inner_size([380.0, 130.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
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
            },
        );

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
    #[allow(deprecated)]
    fn confirm_delete_channel_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete_channel.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("del_channel_vp"),
            egui::ViewportBuilder::default()
                .with_title("Delete channel")
                .with_inner_size([400.0, 130.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
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
            },
        );

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

    /// Confirmation dialog for "Quit & stop recordings" tray action.
    #[allow(deprecated)]
    fn confirm_quit_stop_window(&mut self, ctx: &egui::Context) {
        if !self.confirm_quit_stop {
            return;
        }
        let mut open = true;
        let mut confirmed = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("confirm_quit_stop_vp"),
            egui::ViewportBuilder::default()
                .with_title("Stop recordings and quit?")
                .with_inner_size([380.0, 130.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label("This will terminate all active recordings immediately.");
                    ui.label("In-progress captures will be finalized from whatever was written.");
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        let stop_btn = egui::Button::new("Stop & Quit")
                            .fill(egui::Color32::from_rgb(180, 40, 40));
                        if ui.add(stop_btn).clicked() {
                            confirmed = true;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                    });
                });
            },
        );

        if confirmed {
            self.core
                .force_stop_on_quit
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.quitting = true;
            self.confirm_quit_stop = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if !open {
            self.confirm_quit_stop = false;
        }
    }

    /// Modal for creating a new channel container or renaming an existing one.
    #[allow(deprecated)]
    fn channel_form_window(&mut self, ctx: &egui::Context) {
        if self.channel_form.is_none() {
            return;
        }
        let renaming = self.channel_form.as_ref().unwrap().id.is_some();
        let title = if renaming { "Rename channel" } else { "Add channel" };
        let mut open = true;
        let mut do_save = false;
        let mut do_cancel = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("channel_form_vp"),
            egui::ViewportBuilder::default()
                .with_title(title.to_string())
                .with_inner_size([380.0, 220.0])
                .with_resizable(false),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    let f = self.channel_form.as_mut().unwrap();
                    egui::Grid::new("channel_form_grid")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Name");
                            ui.text_edit_singleline(&mut f.name);
                            ui.end_row();

                            ui.label("Color");
                            ui.horizontal(|ui| {
                                // Colored swatch preview
                                let swatch_color = if f.color.is_empty() {
                                    egui::Color32::from_gray(0x60)
                                } else {
                                    parse_hex_color(&f.color)
                                        .unwrap_or(egui::Color32::from_gray(0x60))
                                };
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(20.0, 20.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(rect, 4.0, swatch_color);
                                ui.painter().rect_stroke(
                                    rect,
                                    4.0,
                                    egui::Stroke::new(1.0, egui::Color32::from_gray(0x80)),
                                    egui::StrokeKind::Inside,
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut f.color)
                                        .hint_text("#rrggbb")
                                        .desired_width(80.0),
                                );
                                if !f.color.is_empty() && ui.small_button("✕").clicked() {
                                    f.color.clear();
                                }
                            });
                            ui.end_row();
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
            },
        );

        if do_save {
            let f = self.channel_form.as_ref().unwrap();
            let name = f.name.trim().to_string();
            if name.is_empty() {
                self.status = "Name is required.".into();
            } else {
                let id_opt = f.id;
                let color = f.color.trim().to_string();
                let res = match id_opt {
                    Some(id) => self
                        .core
                        .store
                        .rename_channel(id, &name)
                        .and_then(|()| self.core.store.set_channel_color(id, &color)),
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

                            ui.label("Detect title");
                            if ui.checkbox(&mut d.auto_title, "Detect title + channel")
                                .on_hover_text(
                                    "Default state of the \"Detect title + channel\" checkbox \
                                     for new downloads on this platform.",
                                )
                                .changed()
                            {
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
        // Platform favicons (uploaded once, cheaply cloned per frame) + whether to
        // tint rows by status — captured before the table closures borrow `self`.
        let ptex = self
            .platform_tex
            .get_or_insert_with(|| PlatformTextures::load(ui.ctx()))
            .clone();
        let status_bgcolor = self.status_bgcolor;
        // Whether the Actions column is shown (Settings → Display). Skipped in the
        // builder, header, and each row when off so the column counts match.
        let show_actions = self.show_actions;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Non-selectable labels so a right-click reaches the row (menu).
                ui.style_mut().interaction.selectable_labels = false;
                let sel_color = ui.visuals().selection.bg_fill;
                let mut tb = TableBuilder::new(ui)
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
                    .column(Column::auto().at_least(160.0)); // file
                if show_actions {
                    tb = tb.column(Column::remainder().at_least(150.0)); // actions
                }
                let table = tb
                    .header(46.0, |mut header| {
                        // (title, hover tooltip) per sortable column.
                        let cols: [(&str, &str); 9] = [
                            ("Video", "The video's title (or the URL until detected). Hover a row for the full URL."),
                            ("Channel", "Uploader / channel name (filled when Auto-detect is on)."),
                            ("Platform", "Source platform: YouTube, Twitch, Kick, or a generic URL."),
                            ("Tool", "Download tool: yt-dlp, streamlink, or ffmpeg."),
                            ("Status", "queued / downloading / completed / failed / stopped. Hover a failed row to see why."),
                            ("Speed", "Current download speed (shown while downloading)."),
                            ("Size", "Size of the output file (grows while downloading)."),
                            ("Added", "When the download was added."),
                            ("File", "Output file path once written. Hover for the full path."),
                        ];
                        for (i, (t, tip)) in cols.into_iter().enumerate() {
                            header.col(|ui| {
                                sort_filter_header(ui, i, t, tip, true, &mut sort, &mut filters[i]);
                            });
                        }
                        if show_actions {
                            header.col(|ui| {
                                ui.strong("Actions").on_hover_text(
                                    "Per-row actions: stop / retry, open file, open folder, copy URL, delete.",
                                );
                            });
                        }
                    });
                table.body(|mut body| {
                        let order = ordered_rows(&model, &sort, &filters);
                        for &vi in &order {
                            let v = &self.videos[vi];
                            // Tint by status (in-flight = accent, failed = red),
                            // honoring the top-bar "Status bgcolor" toggle.
                            let tint = video_row_tint(&v.status, sel_color, status_bgcolor);
                            if let Some(c) = tint {
                                body.ui_mut().visuals_mut().selection.bg_fill = c;
                            }
                            body.row(24.0, |mut tr| {
                                tr.set_selected(tint.is_some());
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
                                    platform_icon(ui, &ptex, v.platform)
                                        .on_hover_text(v.platform.label());
                                    ui.label(v.platform.label());
                                });
                                tr.col(|ui| {
                                    ui.label(v.tool.label()).on_hover_text(v.tool.tooltip());
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
                                        let resp = ui
                                            .colored_label(video_status_color(&v.status), &v.status);
                                        if v.status == "failed" {
                                            let mut msg = fail_hover(&v.log_excerpt);
                                            if let Some(code) = v.exit_code {
                                                msg = format!("{msg}\n(exit code {code})");
                                            }
                                            resp.on_hover_text(msg);
                                        }
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
                                if show_actions {
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
                                }

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
            vf.auto_title = d.auto_title;
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
        let mut open_vf_designer = false;

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

            // Four columns (label · field · label · field) so two fields share a
            // row — the form flows across the available width instead of stacking
            // into a tall, scrolling single column.
            egui::Grid::new("video_form_grid")
                .num_columns(4)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    // URL (wide) + Name.
                    ui.label("URL");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut vf.url)
                                .desired_width(340.0)
                                .hint_text(
                                    "YouTube video, Twitch VOD, or any streamlink/yt-dlp URL",
                                ),
                        );
                        platform_badge(ui, platform);
                        ui.label(platform.label());
                    });
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut vf.title).hint_text(
                        "optional — used for the filename (default: the title, else \"video\")",
                    ));
                    ui.end_row();

                    // Auto-detect + Tool.
                    ui.label("Auto-detect");
                    ui.checkbox(&mut vf.auto_title, "Detect title + channel")
                        .on_hover_text(
                            "Looks up the real title and channel via yt-dlp at download time: \
                             fills the Channel column and the {title}/{channel} variables (and \
                             {name} when Name is left blank).",
                        );
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

                    // Quality + Auth.
                    ui.label("Quality");
                    ui.add(
                        egui::TextEdit::singleline(&mut vf.quality)
                            .hint_text("best, 1080p, or a yt-dlp -f selector"),
                    );
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

                    // Auth value (only for cookie/token overrides) — its own row.
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

                    // Output folder + Filename template.
                    ui.label("Output folder");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut vf.output_dir);
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = browse_folder(&vf.output_dir) {
                                vf.output_dir = p;
                            }
                        }
                    });
                    let tmpl_hint = "Variables: {name} {title} {channel} {date} {time} {timestamp} {year} {month} {day} {hour} {minute} {second} {tool} {mode} {platform} {video_id} {quality} {resolution} {height} {width} {fps} {vcodec} {acodec} {take} {games} {went_live_date} {went_live_time}";
                    ui.label("Filename template").on_hover_text(tmpl_hint);
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut vf.filename_template).on_hover_text(tmpl_hint);
                        if ui.button("Design…").on_hover_text("Open the Format Designer").clicked() {
                            open_vf_designer = true;
                        }
                    });
                    ui.end_row();

                    // Extra args + Audio tracks.
                    ui.label("Extra args");
                    ui.text_edit_singleline(&mut vf.extra_args);
                    ui.label("Audio tracks");
                    ui.text_edit_singleline(&mut vf.audio_tracks).on_hover_text(
                        "Audio tracks to capture (streamlink --hls-audio-select). \
                         Empty = the tool's default; 'all' (or '*') = every track; or a \
                         comma-separated list of language codes/names. streamlink-only; \
                         yt-dlp/ffmpeg keep the chosen format's tracks.",
                    );
                    ui.end_row();

                    // Subtitle tracks + Log chat.
                    ui.label("Subtitle tracks");
                    ui.text_edit_singleline(&mut vf.subtitle_tracks).on_hover_text(
                        "Subtitle tracks to download (yt-dlp --sub-langs, written as sidecar \
                         files next to the video). Empty = none; 'all' (or '*') = every \
                         subtitle; or a comma-separated list of language codes. yt-dlp-only.",
                    );
                    ui.label("Log chat");
                    ui.checkbox(&mut vf.chat_log, "").on_hover_text(
                        "Download chat alongside the video (yt-dlp's live_chat → a \
                         .live_chat.json sidecar, e.g. a YouTube VOD's chat replay). \
                         Sources without a chat track simply produce none.",
                    );
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
        if open_vf_designer {
            let tmpl = self.video_form.filename_template.clone();
            self.open_format_designer(tmpl, Some(FormatDesignerTarget::VideoForm));
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
            audio_tracks: self.video_form.audio_tracks.trim().to_string(),
            subtitle_tracks: self.video_form.subtitle_tracks.trim().to_string(),
            chat_log: self.video_form.chat_log,
            extra_args: self.video_form.extra_args.clone(),
            auto_title: self.video_form.auto_title,
            status: "queued".into(),
            output_path: String::new(),
            bytes: 0,
            exit_code: None,
            log_excerpt: String::new(),
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
    #[allow(deprecated)]
    fn format_probe_window(&mut self, ctx: &egui::Context) {
        let probe = self.format_probe.lock().unwrap().clone();
        let (title, body, done) = match &probe {
            FormatProbe::Idle => return,
            FormatProbe::Running => ("Listing formats…", "Running…".to_string(), false),
            FormatProbe::Done(s) => ("Available formats", s.clone(), true),
            FormatProbe::Failed(e) => ("Format probe failed", e.clone(), true),
        };
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("format_probe_vp"),
            egui::ViewportBuilder::default()
                .with_title(title.to_string())
                .with_inner_size([680.0, 460.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
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
            },
        );
        if !open {
            *self.format_probe.lock().unwrap() = FormatProbe::Idle;
        }
    }

    // ── Format Designer ──────────────────────────────────────────────────────

    /// Open (or replace) the Format Designer window, pre-loading recordings for
    /// the first monitor in the list.
    fn open_format_designer(&mut self, template: String, target: Option<FormatDesignerTarget>) {
        let mut state = FormatDesignerState::new(template, target);
        // Default to the first monitor and pre-load its recordings.
        if let Some(m) = self.rows.first() {
            state.selected_recording_idx = 0;
            state.recordings = self.core.store.recordings_for_monitor(m.monitor.id).unwrap_or_default();
            // Default to the most recent recording (last in oldest-first list).
            if !state.recordings.is_empty() {
                state.selected_recording_idx = state.recordings.len() - 1;
            }
        }
        self.format_designer = Some(state);
    }

    /// The floating Format Designer window: token reference, live preview, and
    /// optional write-back to the field that opened it.
    #[allow(deprecated)]
    fn format_designer_window(&mut self, ctx: &egui::Context) {
        if self.format_designer.is_none() {
            return;
        }

        // Token catalogue: (category label, &[(token, tooltip)])
        const TOKENS: &[(&str, &[(&str, &str)])] = &[
            ("Identity", &[
                ("{name}", "Channel / stream name"),
                ("{channel}", "Channel name (VOD downloads)"),
                ("{video_id}", "Stream or video ID"),
                ("{take}", "Recording attempt number"),
            ]),
            ("Capture time", &[
                ("{date}", "Date YYYYMMDD"),
                ("{time}", "Time HHMMSS"),
                ("{year}", "4-digit year"),
                ("{month}", "2-digit month"),
                ("{day}", "2-digit day"),
                ("{hour}", "2-digit hour (UTC)"),
                ("{minute}", "2-digit minute"),
                ("{second}", "2-digit second"),
                ("{timestamp}", "Unix timestamp"),
            ]),
            ("Live timing", &[
                ("{went_live_date}", "Broadcast go-live date YYYYMMDD"),
                ("{went_live_time}", "Broadcast go-live time HHMMSS"),
            ]),
            ("Stream info", &[
                ("{title}", "Stream title"),
                ("{games}", "Games / categories played"),
                ("{quality}", "Configured quality selector"),
                ("{platform}", "twitch · youtube · kick · generic"),
                ("{mode}", "live · sabr · dash · hybrid · direct · vod"),
                ("{tool}", "streamlink · yt-dlp · ffmpeg"),
            ]),
            ("Media (post-probe)", &[
                ("{resolution}", "e.g. 1920x1080"),
                ("{height}", "e.g. 1080"),
                ("{width}", "e.g. 1920"),
                ("{fps}", "e.g. 60"),
                ("{vcodec}", "Video codec e.g. h264 · hevc · av1"),
                ("{acodec}", "Audio codec e.g. aac · opus"),
            ]),
        ];

        // ── Snapshot state before closure (avoids borrow conflicts) ──────────
        let template = self.format_designer.as_ref().unwrap().template.clone();
        let selected_monitor_idx = self.format_designer.as_ref().unwrap().selected_monitor_idx;
        let selected_recording_idx = self.format_designer.as_ref().unwrap().selected_recording_idx;
        let recordings = self.format_designer.as_ref().unwrap().recordings.clone();
        let target = self.format_designer.as_ref().unwrap().target.clone();

        let monitor_names: Vec<String> = self.rows.iter()
            .map(|r| r.channel.name.clone())
            .collect();
        let selected_monitor = self.rows.get(selected_monitor_idx).cloned();
        let selected_recording = recordings.get(selected_recording_idx).cloned();

        // Pre-compute preview (stale by one frame on fast typing — acceptable).
        let preview = selected_monitor.as_ref()
            .map(|m| build_preview_filename(m, selected_recording.as_ref(), &template))
            .unwrap_or_default();

        // ── Mutable locals for the closure to write into ─────────────────────
        let mut new_template = template.clone();
        let mut new_monitor_idx = selected_monitor_idx;
        let mut new_recording_idx = selected_recording_idx;
        let mut close = false;
        let mut apply = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("format_designer_vp"),
            egui::ViewportBuilder::default()
                .with_title("Format Designer")
                .with_inner_size([820.0, 600.0])
                .with_resizable(true),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(2.0);
                    ui.label("Listing of all possible {formatter} options — highlighted when in use in the template below.");
                    ui.add_space(6.0);

                    // ── Channel + Recording dropdowns ────────────────────────
                    ui.horizontal(|ui| {
                        ui.label("Channel:");
                        let ch_label = monitor_names.get(new_monitor_idx)
                            .cloned()
                            .unwrap_or_else(|| "— none —".to_string());
                        egui::ComboBox::from_id_salt("fd_channel_cb")
                            .selected_text(&ch_label)
                            .width(180.0)
                            .show_ui(ui, |ui| {
                                for (i, name) in monitor_names.iter().enumerate() {
                                    if ui.selectable_value(&mut new_monitor_idx, i, name).clicked() {
                                        new_recording_idx = usize::MAX; // sentinel → reload
                                    }
                                }
                            });

                        ui.add_space(12.0);
                        ui.label("Recording:");
                        let rec_label = recordings.get(new_recording_idx)
                            .map(|r| {
                                chrono::DateTime::from_timestamp(r.started_at, 0)
                                    .map(|dt| dt.with_timezone(&chrono::Local)
                                        .format("%Y-%m-%d %H:%M").to_string())
                                    .unwrap_or_else(|| r.started_at.to_string())
                            })
                            .unwrap_or_else(|| "— sample data —".to_string());
                        egui::ComboBox::from_id_salt("fd_recording_cb")
                            .selected_text(&rec_label)
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                // "— sample data —" option (no real recording)
                                let no_rec_label = "— sample data —";
                                if ui.selectable_label(new_recording_idx == usize::MAX, no_rec_label).clicked() {
                                    new_recording_idx = usize::MAX;
                                }
                                // Recordings newest-first
                                for (i, r) in recordings.iter().enumerate().rev() {
                                    let label = chrono::DateTime::from_timestamp(r.started_at, 0)
                                        .map(|dt| dt.with_timezone(&chrono::Local)
                                            .format("%Y-%m-%d %H:%M").to_string())
                                        .unwrap_or_else(|| r.started_at.to_string());
                                    let label = format!("{label}  ({})", r.status);
                                    if ui.selectable_value(&mut new_recording_idx, i, &label).clicked() {}
                                }
                            });
                    });

                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // ── Token grid ───────────────────────────────────────────
                    let accent = ui.style().visuals.selection.bg_fill;
                    let dim = egui::Color32::from_gray(45);
                    let text_col = ui.style().visuals.text_color();

                    for (category, tokens) in TOKENS {
                        ui.horizontal_wrapped(|ui| {
                            ui.strong(*category);
                            ui.label("  ");
                            for (tok, desc) in *tokens {
                                let in_use = new_template.contains(tok);
                                let fill = if in_use { accent } else { dim };
                                let label_text = egui::RichText::new(*tok)
                                    .monospace()
                                    .small()
                                    .color(text_col);
                                let btn = egui::Button::new(label_text)
                                    .fill(fill)
                                    .corner_radius(3.0);
                                if ui.add(btn).on_hover_text(*desc).clicked() {
                                    new_template.push_str(tok);
                                }
                            }
                        });
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // ── Template input ───────────────────────────────────────
                    ui.horizontal(|ui| {
                        ui.label("Template:");
                        ui.add_sized(
                            [ui.available_width(), 20.0],
                            egui::TextEdit::singleline(&mut new_template)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("{name}_{date}_{time}"),
                        );
                    });

                    ui.add_space(6.0);

                    // ── Preview ──────────────────────────────────────────────
                    ui.horizontal(|ui| {
                        ui.label("Preview:");
                        let ext = ".mkv";
                        let preview_str = if preview.is_empty() {
                            format!("(no channel selected){ext}")
                        } else {
                            format!("{preview}{ext}")
                        };
                        egui::Frame::new()
                            .fill(egui::Color32::from_gray(28))
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width() - 4.0);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&preview_str)
                                            .monospace()
                                    ).selectable(true),
                                );
                            });
                    });

                    ui.add_space(10.0);

                    // ── Action buttons ───────────────────────────────────────
                    ui.horizontal(|ui| {
                        if target.is_some() && ui.button("Apply").on_hover_text("Write this template back to the field that opened the designer").clicked() {
                            apply = true;
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
            },
        );

        // ── Apply closure results back to state ──────────────────────────────
        let monitor_changed = new_monitor_idx != selected_monitor_idx;
        let new_recordings = if monitor_changed {
            self.rows.get(new_monitor_idx).map(|m| {
                let recs = self.core.store.recordings_for_monitor(m.monitor.id).unwrap_or_default();
                // Default to the most recent recording.
                let default_idx = recs.len().saturating_sub(1);
                (recs, default_idx)
            })
        } else {
            None
        };

        if let Some(fd) = self.format_designer.as_mut() {
            fd.template = new_template.clone();
            fd.selected_monitor_idx = new_monitor_idx;
            if let Some((recs, default_idx)) = new_recordings {
                fd.recordings = recs;
                fd.selected_recording_idx = default_idx;
            } else {
                fd.selected_recording_idx = new_recording_idx;
            }
        }

        if apply {
            match &target {
                Some(FormatDesignerTarget::MonitorForm) => {
                    if let Some(form) = self.form.as_mut() {
                        form.filename_template = new_template.clone();
                    }
                }
                Some(FormatDesignerTarget::VideoForm) => {
                    self.video_form.filename_template = new_template.clone();
                }
                None => {}
            }
            self.format_designer = None;
        } else if close {
            self.format_designer = None;
        }
    }

    /// Window listing where ad breaks cause hard cuts in a take's finished file.
    /// Opened by double-clicking an Ads / Ad time cell.
    #[allow(deprecated)]
    fn ad_popup_window(&mut self, ctx: &egui::Context) {
        let Some(rid) = self.ad_popup else {
            return;
        };
        // Reuse the cached cut list (cleared on reload) rather than re-querying
        // every frame the popup is open.
        if !self.ad_break_cache.contains_key(&rid) {
            let v = self
                .core
                .store
                .ad_breaks_for_recording(rid)
                .unwrap_or_default();
            self.ad_break_cache.insert(rid, v);
        }
        let breaks = self.ad_break_cache.get(&rid).cloned().unwrap_or_default();
        let total: i64 = breaks.iter().map(|b| b.duration_secs).sum();
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("ad_breaks_vp"),
            egui::ViewportBuilder::default()
                .with_title("Ad breaks — cut points")
                .with_inner_size([360.0, 260.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
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
            },
        );
        if !open {
            self.ad_popup = None;
        }
    }

    /// Load a recording's metadata-change rows into the cache if absent.
    fn ensure_meta_cached(&mut self, rid: i64) {
        if !self.meta_change_cache.contains_key(&rid) {
            let v = self
                .core
                .store
                .meta_changes_for_recording(rid)
                .unwrap_or_default();
            self.meta_change_cache.insert(rid, v);
        }
    }

    #[allow(deprecated)]
    fn meta_popup_window(&mut self, ctx: &egui::Context) {
        let Some(popup) = self.meta_popup.clone() else {
            return;
        };
        // Build the change list: one take directly, or a stream's takes merged
        // chronologically with the per-take re-baselines dropped.
        let (changes, multi) = match &popup {
            MetaPopup::Take(rid) => {
                self.ensure_meta_cached(*rid);
                (self.meta_change_cache.get(rid).cloned().unwrap_or_default(), false)
            }
            MetaPopup::Stream(takes) => {
                for (rid, _) in takes {
                    self.ensure_meta_cached(*rid);
                }
                let loaded: Vec<(i64, Vec<StreamMetaChange>)> = takes
                    .iter()
                    .map(|(rid, started)| {
                        (*started, self.meta_change_cache.get(rid).cloned().unwrap_or_default())
                    })
                    .collect();
                (aggregate_stream_changes(&loaded), takes.len() > 1)
            }
        };
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("title_changes_vp"),
            egui::ViewportBuilder::default()
                .with_title("Title & category changes")
                .with_inner_size([460.0, 280.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    // Only actual changes (the initial value of each field is the
                    // starting state, not a change); shown as `old → new`.
                    let lines = meta_change_lines(&changes);
                    if lines.is_empty() {
                        ui.label("No title or category changes recorded.");
                        return;
                    }
                    let scope = if multi {
                        "across this stream's takes"
                    } else {
                        "during this recording"
                    };
                    ui.label(format!(
                        "{} change(s) {scope} (offset from the start; each shows the \
                         value before → after).",
                        lines.len(),
                    ));
                    ui.add_space(6.0);
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
            },
        );
        if !open {
            self.meta_popup = None;
        }
    }

    /// Window listing a monitor's upcoming scheduled streams (datetime — title).
    /// Opened by double-clicking a Next stream cell.
    #[allow(deprecated)]
    fn schedule_popup_window(&mut self, ctx: &egui::Context) {
        let Some(mid) = self.schedule_popup else {
            return;
        };
        if !self.schedule_cache.contains_key(&mid) {
            let v = self
                .core
                .store
                .schedule_for_monitor(mid, now_unix())
                .unwrap_or_default();
            self.schedule_cache.insert(mid, v);
        }
        let segs = self.schedule_cache.get(&mid).cloned().unwrap_or_default();
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("upcoming_streams_vp"),
            egui::ViewportBuilder::default()
                .with_title("Upcoming streams")
                .with_inner_size([460.0, 280.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if segs.is_empty() {
                        ui.label("No upcoming scheduled streams.");
                        return;
                    }
                    ui.label(format!("{} upcoming scheduled stream(s).", segs.len()));
                    ui.add_space(6.0);
                    let lines: Vec<String> = segs
                        .iter()
                        .map(|s| {
                            let when = fmt_datetime_short(s.start_time);
                            if s.category.is_empty() {
                                format!("{when}  —  {}", s.title)
                            } else {
                                format!("{when}  —  {}  ({})", s.title, s.category)
                            }
                        })
                        .collect();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for l in &lines {
                                ui.label(egui::RichText::new(l).monospace());
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(lines.join("\n"));
                    }
                });
            },
        );
        if !open {
            self.schedule_popup = None;
        }
    }

    /// The Schedule tab: a month/week/day calendar of all upcoming scheduled
    /// streams, with a left sidebar to filter channels and a collision highlight.
    fn schedule_view(&mut self, ui: &mut egui::Ui) {
        use chrono::Datelike;

        // Lazy load on first view + initialize the focused date to today.
        if !self.schedule_loaded {
            self.reload_schedule();
        }
        let anchor = *self
            .schedule_anchor
            .get_or_insert_with(|| chrono::Local::now().date_naive());

        // Empty state: nothing scheduled across any monitor.
        if self.schedule_all.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No upcoming streams scheduled.");
                ui.label(
                    "Schedules come from a channel's published Twitch/YouTube upcoming \
                     streams — channels without one (Twitch returns no segments) show nothing.",
                );
                ui.add_space(8.0);
                if ui.button("⟳  Fetch now").clicked() {
                    self.core.request_schedule_refresh();
                    self.reload_schedule();
                }
            });
            return;
        }

        // Platform favicons (cheap Arc-backed clone) so the panel closures below can
        // borrow `self` immutably — they only read schedule data.
        let ptex = self
            .platform_tex
            .get_or_insert_with(|| PlatformTextures::load(ui.ctx()))
            .clone();

        // Precompute (immutable reads of `self`) what the closures need: collisions
        // and the per-day buckets of visible streams (indices into `schedule_all`).
        let collide: HashSet<usize> = if self.schedule_collisions {
            schedule_collisions(&self.schedule_all, &self.schedule_hidden)
        } else {
            HashSet::new()
        };
        let mut by_day: HashMap<chrono::NaiveDate, Vec<usize>> = HashMap::new();
        for (i, s) in self.schedule_all.iter().enumerate() {
            if self.schedule_hidden.contains(&s.channel_id) {
                continue;
            }
            if let Some(d) = local_date(s.start_time) {
                by_day.entry(d).or_default().push(i);
            }
        }
        // `schedule_all` is sorted by start_time, so each day's list is time-sorted.

        // Actions collected during rendering, applied after the borrowing closures.
        let mut open_day: Option<chrono::NaiveDate> = None;
        let mut nav_anchor: Option<chrono::NaiveDate> = None;
        let mut set_mode: Option<ScheduleMode> = None;
        let mut do_refresh = false;
        let mut clear_hidden = false;
        let mut hide_all = false;
        let mut toggle_channel: Option<i64> = None;
        let mut set_collisions: Option<bool> = None;

        // ── Left sidebar: per-channel filter + collision toggle. ──
        egui::Panel::left("schedule_sidebar")
            .resizable(true)
            .default_size(210.0)
            .size_range(160.0..=380.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Channels");
                ui.add_space(2.0);

                // Distinct channels with upcoming streams, sorted by name.
                let mut chans: Vec<(i64, &str)> = Vec::new();
                let mut seen: HashSet<i64> = HashSet::new();
                for s in &self.schedule_all {
                    if seen.insert(s.channel_id) {
                        chans.push((s.channel_id, s.channel_name.as_str()));
                    }
                }
                chans.sort_by_key(|(_, name)| name.to_lowercase());

                let mut all = self.schedule_hidden.is_empty();
                if ui
                    .checkbox(&mut all, "All channels")
                    .on_hover_text("Show streams from every channel")
                    .changed()
                {
                    if all {
                        clear_hidden = true;
                    } else {
                        hide_all = true;
                    }
                }
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (id, name) in &chans {
                            // Count + distinct platforms for this channel's upcoming streams.
                            let mut count = 0usize;
                            let mut plats: Vec<Platform> = Vec::new();
                            for s in self.schedule_all.iter().filter(|s| s.channel_id == *id) {
                                count += 1;
                                let p = s.platform();
                                if !plats.contains(&p) {
                                    plats.push(p);
                                }
                            }
                            ui.horizontal(|ui| {
                                let mut vis = !self.schedule_hidden.contains(id);
                                if ui.checkbox(&mut vis, "").changed() {
                                    toggle_channel = Some(*id);
                                }
                                for &p in &plats {
                                    platform_icon(ui, &ptex, p).on_hover_text(p.label());
                                }
                                ui.add(
                                    egui::Label::new(format!("{name}  ({count})")).truncate(),
                                );
                            });
                        }
                    });
            });

        // Visible date range + header title for the current mode.
        let mode = self.schedule_mode;
        let today = chrono::Local::now().date_naive();
        let (range_start, range_end, title) = match mode {
            ScheduleMode::Month => {
                let first = chrono::NaiveDate::from_ymd_opt(anchor.year(), anchor.month(), 1)
                    .unwrap_or(anchor);
                let gs = week_start(first);
                (gs, add_days(gs, 42), month_title(anchor.year(), anchor.month()))
            }
            ScheduleMode::Week => {
                let ws = week_start(anchor);
                let we = add_days(ws, 6);
                // Honor the date-format setting on both ends (so the start year shows
                // for a cross-year week, and the order matches the user's preference).
                let pat = active_date_fmt().date_pattern();
                let title = format!("{} – {}", ws.format(pat), we.format(pat));
                (ws, add_days(ws, 7), title)
            }
            ScheduleMode::Day => {
                let title = anchor
                    .format(&format!("%A, {}", active_date_fmt().date_pattern()))
                    .to_string();
                (anchor, add_days(anchor, 1), title)
            }
            ScheduleMode::Agenda => {
                // Show all upcoming from anchor; use far-future end so the collision
                // badge counts everything visible.
                (anchor, add_days(anchor, 365), "Agenda".to_string())
            }
        };
        let (prev_date, next_date) = match mode {
            // Snap month nav to the 1st: the grid only uses the month anyway, and it
            // keeps paging idempotent (no day-of-month drift across short months).
            ScheduleMode::Month => (
                shift_month(anchor, -1).with_day(1).unwrap_or(anchor),
                shift_month(anchor, 1).with_day(1).unwrap_or(anchor),
            ),
            ScheduleMode::Week | ScheduleMode::Agenda => (add_days(anchor, -7), add_days(anchor, 7)),
            ScheduleMode::Day => (add_days(anchor, -1), add_days(anchor, 1)),
        };
        // Collisions visible in the current view (the per-chip ⚠ uses the global
        // `collide` set; the badge counts only what's on screen).
        let collisions_in_view = collide
            .iter()
            .filter(|&&i| {
                local_date(self.schedule_all[i].start_time)
                    .is_some_and(|d| d >= range_start && d < range_end)
            })
            .count();

        // ── Center: the calendar for the selected mode. ──
        egui::CentralPanel::default().show_inside(ui, |ui| {
            // Header: view mode + navigation + title + collision controls.
            ui.horizontal(|ui| {
                let mut m = mode;
                ui.selectable_value(&mut m, ScheduleMode::Month, "Month");
                ui.selectable_value(&mut m, ScheduleMode::Week, "Week");
                ui.selectable_value(&mut m, ScheduleMode::Day, "Day");
                ui.selectable_value(&mut m, ScheduleMode::Agenda, "Agenda");
                if m != mode {
                    set_mode = Some(m);
                }
                ui.separator();
                if ui.button("◀").on_hover_text("Previous").clicked() {
                    nav_anchor = Some(prev_date);
                }
                if ui.button("Today").clicked() {
                    nav_anchor = Some(today);
                }
                if ui.button("▶").on_hover_text("Next").clicked() {
                    nav_anchor = Some(next_date);
                }
                ui.add_space(8.0);
                ui.heading(title);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("⟳")
                        .on_hover_text("Fetch the latest schedules now (F5)")
                        .clicked()
                    {
                        do_refresh = true;
                    }
                    let mut hc = self.schedule_collisions;
                    if ui
                        .checkbox(&mut hc, "Highlight collisions")
                        .on_hover_text("Flag streams whose scheduled times overlap")
                        .changed()
                    {
                        set_collisions = Some(hc);
                    }
                    if self.schedule_collisions && collisions_in_view > 0 {
                        ui.colored_label(HL_COLLISION, format!("⚠ {collisions_in_view}"))
                            .on_hover_text("Overlapping streams in view");
                    }
                });
            });
            ui.separator();

            match mode {
                ScheduleMode::Month => {
                    self.schedule_month_grid(ui, anchor, today, &by_day, &collide, &ptex, &mut open_day)
                }
                ScheduleMode::Week => {
                    self.schedule_week_grid(ui, anchor, today, &by_day, &collide, &ptex, &mut open_day)
                }
                ScheduleMode::Day => {
                    self.schedule_day_grid(ui, anchor, today, &by_day, &collide, &mut open_day)
                }
                ScheduleMode::Agenda => {
                    self.schedule_agenda_view(ui, anchor, &by_day, &collide, &ptex, &mut open_day)
                }
            }
        });

        // ── Apply collected actions. ──
        if let Some(m) = set_mode {
            self.schedule_mode = m;
        }
        if let Some(d) = nav_anchor {
            self.schedule_anchor = Some(d);
        }
        if clear_hidden {
            self.schedule_hidden.clear();
        }
        if hide_all {
            let ids: Vec<i64> = self.schedule_all.iter().map(|s| s.channel_id).collect();
            self.schedule_hidden.extend(ids);
        }
        if let Some(id) = toggle_channel {
            // Toggle this channel's visibility.
            if self.schedule_hidden.contains(&id) {
                self.schedule_hidden.remove(&id);
            } else {
                self.schedule_hidden.insert(id);
            }
        }
        if let Some(v) = set_collisions {
            self.schedule_collisions = v;
        }
        if let Some(d) = open_day {
            self.schedule_day_popup = Some(d);
        }
        if do_refresh {
            // Trigger a real network re-fetch (not just a DB reload); the refresher
            // emits an event when done, which reloads the calendar. Also reload now
            // so the current stored data shows immediately.
            self.core.request_schedule_refresh();
            self.reload_schedule();
            self.status = "Fetching latest schedules…".into();
        }
        // Context-menu actions written into egui temp storage by schedule_copy_menu
        // (closures can't borrow `self` directly when deep inside panel closures).
        if let Some(mid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_jump")))
        {
            self.view = View::Streams;
            self.selected_monitor = Some(mid);
        }
        if let Some(mid) = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<i64>(egui::Id::new("sched_start")))
        {
            self.core.manual(ManualCommand::Start(mid));
            self.status = "Checking channel… will record if live.".into();
        }
    }

    /// One compact calendar chip (colored stripe · ⚠ · platform icon · time range · channel)
    /// with a hover detail and the copy context menu. Returns the click response so
    /// the caller can react (e.g. open the day popup). Shared by month + week views.
    fn schedule_chip(
        &self,
        ui: &mut egui::Ui,
        i: usize,
        colliding: bool,
        ptex: &PlatformTextures,
    ) -> egui::Response {
        let s = &self.schedule_all[i];
        let color = channel_event_color(s.channel_id, &s.channel_color);
        let resp = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                // 3px colored left stripe
                let (stripe_rect, _) = ui.allocate_exact_size(
                    egui::vec2(3.0, ui.text_style_height(&egui::TextStyle::Body)),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(stripe_rect, egui::CornerRadius::same(2), color);
                if colliding {
                    ui.colored_label(HL_COLLISION, "⚠");
                }
                platform_icon(ui, ptex, s.platform());
                ui.add(
                    egui::Label::new(format!(
                        "{}  {}",
                        fmt_time_range(s.start_time, s.end_time),
                        s.channel_name
                    ))
                    .truncate(),
                );
            })
            .response
            .interact(egui::Sense::click());
        let resp = resp.on_hover_text(schedule_detail_line(s));
        resp.context_menu(|ui| schedule_copy_menu(ui, s));
        resp
    }

    /// Month view: a 6×7 grid of day cells.
    #[allow(clippy::too_many_arguments)]
    fn schedule_month_grid(
        &self,
        ui: &mut egui::Ui,
        anchor: chrono::NaiveDate,
        today: chrono::NaiveDate,
        by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
        collide: &HashSet<usize>,
        ptex: &PlatformTextures,
        open_day: &mut Option<chrono::NaiveDate>,
    ) {
        use chrono::Datelike;
        let month = anchor.month();
        let first = chrono::NaiveDate::from_ymd_opt(anchor.year(), month, 1).unwrap_or(anchor);
        let grid_start = week_start(first);

        let spacing = 4.0;
        let cell_h = 108.0;
        const WD: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        const MAX_CHIPS: usize = 3;
        // Reserve room for a vertical scrollbar so the columns don't shift when it
        // appears, and floor the width so a too-narrow panel gets a horizontal
        // scrollbar instead of clipping the weekend columns.
        let usable = (ui.available_width() - 16.0).max(160.0);
        let col_w = ((usable - spacing * 6.0) / 7.0).floor().max(72.0);

        // Header + weeks share one scroll viewport so their columns stay aligned.
        egui::ScrollArea::both()
            .id_salt("sched_month")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
                ui.horizontal(|ui| {
                    for &wd in &WD {
                        ui.allocate_ui_with_layout(
                            egui::vec2(col_w, 16.0),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                ui.label(egui::RichText::new(wd).strong());
                            },
                        );
                    }
                });
                for week in 0..6u64 {
                    ui.horizontal(|ui| {
                        for dow in 0..7u64 {
                            let day = add_days(grid_start, (week * 7 + dow) as i64);
                            ui.allocate_ui_with_layout(
                                egui::vec2(col_w, cell_h),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    self.schedule_cell(
                                        ui,
                                        day,
                                        month,
                                        today,
                                        col_w,
                                        cell_h,
                                        MAX_CHIPS,
                                        by_day.get(&day),
                                        collide,
                                        ptex,
                                        open_day,
                                    );
                                },
                            );
                        }
                    });
                }
            });
    }

    /// Week view: 7-column time-grid with a 24-hour vertical axis.
    #[allow(clippy::too_many_arguments)]
    fn schedule_week_grid(
        &self,
        ui: &mut egui::Ui,
        anchor: chrono::NaiveDate,
        today: chrono::NaiveDate,
        by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
        collide: &HashSet<usize>,
        _ptex: &PlatformTextures,
        open_day: &mut Option<chrono::NaiveDate>,
    ) {
        use chrono::Datelike;
        let ws = week_start(anchor);
        let days: Vec<chrono::NaiveDate> = (0..7).map(|d| add_days(ws, d)).collect();

        // Day-header row (outside the scroll area so it stays fixed).
        let time_col_w = SCHED_TIME_COL_W;
        let avail_w = ui.available_width();
        let col_w = ((avail_w - time_col_w - 6.0 * SCHED_COL_GAP) / 7.0).max(60.0);
        ui.horizontal(|ui| {
            ui.add_space(time_col_w);
            for &day in &days {
                let is_today = day == today;
                let hdr = format!("{}\n{}", day.format("%a"), day.day());
                let text = if is_today {
                    egui::RichText::new(hdr).strong().color(ui.visuals().hyperlink_color)
                } else {
                    egui::RichText::new(hdr).strong()
                };
                let resp = ui.allocate_ui_with_layout(
                    egui::vec2(col_w + SCHED_COL_GAP, 36.0),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        if is_today {
                            let r = ui.max_rect();
                            ui.painter().rect_filled(r, egui::CornerRadius::ZERO, TODAY_BG);
                        }
                        ui.add(egui::Label::new(text).sense(egui::Sense::click()))
                            .on_hover_text("Open day detail")
                    },
                );
                if resp.inner.clicked() {
                    *open_day = Some(day);
                }
            }
        });
        ui.separator();

        schedule_time_grid(
            ui,
            "sched_week",
            &days,
            col_w,
            &self.schedule_all,
            by_day,
            collide,
            open_day,
        );
    }

    /// Day view: single-column time-grid with full-width event blocks.
    #[allow(clippy::too_many_arguments)]
    fn schedule_day_grid(
        &self,
        ui: &mut egui::Ui,
        anchor: chrono::NaiveDate,
        today: chrono::NaiveDate,
        by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
        collide: &HashSet<usize>,
        open_day: &mut Option<chrono::NaiveDate>,
    ) {
        let is_today = anchor == today;
        let hdr = anchor
            .format(&format!("%A, {}", active_date_fmt().date_pattern()))
            .to_string();
        let text = if is_today {
            egui::RichText::new(hdr).strong().color(ui.visuals().hyperlink_color)
        } else {
            egui::RichText::new(hdr).strong()
        };
        ui.label(text);
        ui.separator();

        let avail_w = ui.available_width();
        let col_w = (avail_w - SCHED_TIME_COL_W - 2.0).max(80.0);
        schedule_time_grid(
            ui,
            "sched_day",
            &[anchor],
            col_w,
            &self.schedule_all,
            by_day,
            collide,
            open_day,
        );
    }

    /// Render one month-grid day cell: a bordered box with the day number and up to
    /// `max_chips` stream chips (overflow folds into a clickable "+N more"). A
    /// left-click on the day or a chip opens the day popup (`open_day`).
    #[allow(clippy::too_many_arguments)]
    fn schedule_cell(
        &self,
        ui: &mut egui::Ui,
        day: chrono::NaiveDate,
        month: u32,
        today: chrono::NaiveDate,
        col_w: f32,
        cell_h: f32,
        max_chips: usize,
        entries: Option<&Vec<usize>>,
        collide: &HashSet<usize>,
        ptex: &PlatformTextures,
        open_day: &mut Option<chrono::NaiveDate>,
    ) {
        use chrono::Datelike;
        let in_month = day.month() == month;
        let is_today = day == today;

        let mut frame = egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(4));
        if is_today {
            frame = frame.fill(TODAY_BG);
        }
        frame.show(ui, |ui| {
            ui.set_min_size(egui::vec2(col_w - 10.0, cell_h - 10.0));
            ui.vertical(|ui| {
                // Day number: strong in-month (today is set off by its tinted
                // cell background), dimmed for the leading/trailing days that spill
                // in from the neighbouring months.
                let num = egui::RichText::new(day.day().to_string());
                let num = if is_today || in_month {
                    num.strong()
                } else {
                    num.weak()
                };
                if ui
                    .add(egui::Label::new(num).sense(egui::Sense::click()))
                    .on_hover_text("Show this day's streams")
                    .clicked()
                {
                    *open_day = Some(day);
                }

                let entries = entries.map(Vec::as_slice).unwrap_or(&[]);
                let shown = entries.len().min(max_chips);
                for &i in &entries[..shown] {
                    let colliding = collide.contains(&i);
                    if self.schedule_chip(ui, i, colliding, ptex).clicked() {
                        *open_day = Some(day);
                    }
                }
                if entries.len() > shown {
                    let more = entries.len() - shown;
                    if ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(format!("+{more} more…")).weak(),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .clicked()
                    {
                        *open_day = Some(day);
                    }
                }
            });
        });
    }

    /// Agenda view: date-grouped chronological list of all upcoming streams from `anchor`.
    #[allow(clippy::too_many_arguments)]
    fn schedule_agenda_view(
        &self,
        ui: &mut egui::Ui,
        anchor: chrono::NaiveDate,
        by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
        collide: &HashSet<usize>,
        ptex: &PlatformTextures,
        open_day: &mut Option<chrono::NaiveDate>,
    ) {
        // Collect and sort the days from `anchor` forward that have visible entries.
        let mut days: Vec<chrono::NaiveDate> = by_day
            .keys()
            .filter(|&&d| d >= anchor)
            .copied()
            .collect();
        days.sort();

        if days.is_empty() {
            ui.add_space(12.0);
            ui.label(egui::RichText::new("No streams scheduled from this date.").weak());
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("sched_agenda")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for day in &days {
                    let Some(indices) = by_day.get(day) else { continue };
                    if indices.is_empty() { continue }

                    // Date group header
                    let heading = day
                        .format(&format!("%A, {}", active_date_fmt().date_pattern()))
                        .to_string();
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.strong(heading);
                    });
                    ui.separator();

                    for &i in indices {
                        let s = &self.schedule_all[i];
                        let color = channel_event_color(s.channel_id, &s.channel_color);
                        let colliding = collide.contains(&i);

                        let row_resp = ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;

                            // Colored stripe
                            let (stripe_rect, _) = ui.allocate_exact_size(
                                egui::vec2(4.0, ui.text_style_height(&egui::TextStyle::Body) * 1.4),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(stripe_rect, egui::CornerRadius::same(2), color);

                            // Time range
                            if colliding {
                                ui.colored_label(HL_COLLISION, "⚠");
                            }
                            ui.add(egui::Label::new(
                                egui::RichText::new(fmt_time_range(s.start_time, s.end_time))
                                    .monospace()
                                    .size(12.0),
                            ));

                            // Platform icon
                            platform_icon(ui, ptex, s.platform());

                            // Channel name (bold)
                            ui.strong(&s.channel_name);

                            // Title (muted, truncated)
                            if !s.title.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("— {}", s.title))
                                            .weak()
                                            .size(12.0),
                                    )
                                    .truncate(),
                                );
                            }

                            // Category in parens (weak)
                            if !s.category.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("({})", s.category))
                                            .weak()
                                            .size(11.0),
                                    )
                                    .truncate(),
                                );
                            }
                        })
                        .response
                        .interact(egui::Sense::click());

                        let row_resp = row_resp.on_hover_text(schedule_detail_line(s));
                        row_resp.context_menu(|ui| schedule_copy_menu(ui, s));
                        if row_resp.clicked() {
                            *open_day = Some(*day);
                        }
                        ui.add_space(1.0);
                    }
                }
            });
    }

    /// Popup listing every (visible) stream on one calendar day, with the same
    /// per-entry copy menu as the calendar chips.
    #[allow(deprecated)]
    fn schedule_day_window(&mut self, ctx: &egui::Context) {
        let Some(date) = self.schedule_day_popup else {
            return;
        };
        let ptex = self
            .platform_tex
            .get_or_insert_with(|| PlatformTextures::load(ctx))
            .clone();
        // Visible streams on that local date (respects the sidebar filter).
        let entries: Vec<&UpcomingStream> = self
            .schedule_all
            .iter()
            .filter(|s| !self.schedule_hidden.contains(&s.channel_id))
            .filter(|s| local_date(s.start_time) == Some(date))
            .collect();
        // Weekday + the user's chosen date format (so the heading matches the chips).
        let heading = date
            .format(&format!("%A, {}", active_date_fmt().date_pattern()))
            .to_string();

        let mut open = true;
        let mut copy_all: Option<String> = None;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("schedule_day_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("Streams · {heading}"))
                .with_inner_size([480.0, 360.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if entries.is_empty() {
                        ui.label("No streams scheduled this day.");
                        return;
                    }
                    ui.label(format!("{} scheduled stream(s).", entries.len()));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for s in &entries {
                                // The popup doesn't carry the collision set; the calendar
                                // surfaces ⚠ markers, so rows here are shown unmarked.
                                schedule_detail_row(ui, s, false, &ptex);
                            }
                        });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy all").clicked() {
                        copy_all = Some(
                            entries
                                .iter()
                                .map(|s| schedule_detail_line(s))
                                .collect::<Vec<_>>()
                                .join("\n\n"),
                        );
                    }
                });
            },
        );
        if let Some(t) = copy_all {
            ctx.copy_text(t);
        }
        if !open {
            self.schedule_day_popup = None;
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
        let mut clear_channel_err: Option<i64> = None;

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

        // Resolve each container's avatar (its chosen-platform profile pic) up
        // front — it needs `&mut self` (texture cache), so the read-only table
        // closure below can just look it up by channel id.
        let mut channel_avatars: HashMap<i64, egui::TextureHandle> = HashMap::new();
        for e in &chan_entries {
            let platforms = {
                let mons: Vec<&MonitorWithChannel> =
                    e.rows.iter().map(|&i| &self.rows[i]).collect();
                channel_platforms(&mons)
            };
            let tex = self
                .channel_icons
                .entry(e.channel.id)
                .or_insert_with(|| resolve_channel_icon(&e.channel, &platforms, ui.ctx()))
                .clone();
            if let Some(t) = tex {
                channel_avatars.insert(e.channel.id, t);
            }
        }

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
        // expanded history rows. Cached (cleared on reload) so we issue the SELECT
        // once per take with ads, not every frame; bounded by what's expanded.
        for &mid in &expanded_monitors {
            let need: Vec<i64> = match self.rec_cache.get(&mid) {
                Some(recs) => recs
                    .iter()
                    .filter(|r| r.ad_count > 0 && !self.ad_break_cache.contains_key(&r.id))
                    .map(|r| r.id)
                    .collect(),
                None => Vec::new(),
            };
            for rid in need {
                let v = self
                    .core
                    .store
                    .ad_breaks_for_recording(rid)
                    .unwrap_or_default();
                self.ad_break_cache.insert(rid, v);
            }
        }
        let ad_breaks = &self.ad_break_cache;
        // Same lazy caching for per-recording title/category change logs.
        for &mid in &expanded_monitors {
            let need: Vec<i64> = match self.rec_cache.get(&mid) {
                Some(recs) => recs
                    .iter()
                    .filter(|r| {
                        r.meta_change_count > 0 && !self.meta_change_cache.contains_key(&r.id)
                    })
                    .map(|r| r.id)
                    .collect(),
                None => Vec::new(),
            };
            for rid in need {
                let v = self
                    .core
                    .store
                    .meta_changes_for_recording(rid)
                    .unwrap_or_default();
                self.meta_change_cache.insert(rid, v);
            }
        }
        let meta_logs = &self.meta_change_cache;
        // Collected in the table closure (which borrows `self` immutably), applied
        // afterwards: a double-click on an ad / changes cell opens that take's popup.
        let mut open_ad_popup: Option<i64> = None;
        let mut open_meta_popup: Option<MetaPopup> = None;
        let mut open_schedule_popup: Option<i64> = None;

        // Snapshot expansion state for read-only use inside the table closure.
        let exp_channels = self.expanded_channels.clone();
        let exp_instances = self.expanded_instances.clone();
        let exp_streams = self.expanded_streams.clone();
        // Snapshot which monitors currently have an ad playing (for the row tint).
        let ad_active = self.core.ad_active.lock().unwrap().clone();
        let ad_running = |mid: i64| ad_active.get(&mid).is_some_and(|&end| now < end);

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
        // Whether status row tints are drawn (top-bar "Status bgcolor" toggle).
        let status_bgcolor = self.status_bgcolor;
        // Whether the Actions column is shown (Settings → Display). When off it's
        // skipped in the builder, header, and every renderer so the counts match.
        let show_actions = self.show_actions;

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
                // Theme accent used for recording/selected rows; ad/error states
                // override the per-row selection color before each row.
                let sel_color = ui.visuals().selection.bg_fill;
                // Platform favicons, uploaded once and cheaply cloned per frame.
                let ptex = self
                    .platform_tex
                    .get_or_insert_with(|| PlatformTextures::load(ui.ctx()))
                    .clone();
                let mut tb = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    // Make rows sense clicks so they can be selected and carry a
                    // right-click context menu.
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                // One column per descriptor — count is guaranteed to match the
                // header and the per-row cells (all driven by STREAM_COLUMNS). The
                // Actions column is skipped (here, in the header, and in every
                // renderer) when hidden, so the counts still match.
                for (i, c) in STREAM_COLUMNS.iter().enumerate() {
                    if !show_actions && i == STREAM_ACTIONS_COL {
                        continue;
                    }
                    let col = if c.initial > 0.0 {
                        // Content-capped column (Title / Game): start narrow and
                        // clip — the cell truncates and shows the full text on hover.
                        Column::initial(c.initial).at_least(c.min_width).clip(true)
                    } else {
                        Column::auto().at_least(c.min_width)
                    };
                    tb = tb.column(col);
                }
                let table = tb.header(46.0, |mut header| {
                    for (i, c) in STREAM_COLUMNS.iter().enumerate() {
                        if !show_actions && i == STREAM_ACTIONS_COL {
                            continue;
                        }
                        header.col(|ui| {
                            if c.sortable {
                                sort_filter_header(
                                    ui, i, c.title, c.tooltip, true, &mut sort, &mut filters[i],
                                );
                            } else {
                                ui.strong(c.title).on_hover_text(c.tooltip);
                            }
                        });
                    }
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
                                let platforms = channel_platforms(&mons);
                                let last_poll = mons
                                    .iter()
                                    .filter_map(|m| m.monitor.last_checked_at)
                                    .max()
                                    .unwrap_or(0);
                                // Latest activity across instances drives the time columns.
                                let primary = channel_primary(&mons);
                                let rec = primary.map(|m| recording_cells(m, now));
                                let ads = primary.map(|m| {
                                    (m.last_recording_ad_count, m.last_recording_ad_secs)
                                });
                                let meta_changes =
                                    primary.map(|m| m.last_recording_meta_changes);
                                let cur_category = primary
                                    .map(|m| m.last_recording_category.clone())
                                    .unwrap_or_default();
                                let cur_title = primary
                                    .map(|m| m.last_recording_title.clone())
                                    .unwrap_or_default();
                                // The channel's next stream = the SOONEST upcoming
                                // across its instances (the past-recording primary
                                // may be a different platform with no schedule).
                                let next_mon = mons
                                    .iter()
                                    .filter(|m| m.next_stream_at.is_some())
                                    .min_by_key(|m| m.next_stream_at.unwrap());
                                let next_stream_at = next_mon.and_then(|m| m.next_stream_at);
                                let next_stream_title = next_mon
                                    .map(|m| m.next_stream_title.clone())
                                    .unwrap_or_default();
                                let next_stream_mid = next_mon.map(|m| m.monitor.id);
                                let ad_free =
                                    ad_free_summary(channel_ad_free_count(&mons), ninst);
                                // Tint the container row by the rolled-up state of
                                // its instances (ad playing / recording / errored).
                                let any_ad = mons.iter().any(|m| ad_running(m.monitor.id));
                                let any_err = mons.iter().copied().any(monitor_errored);
                                let tint =
                                    row_tint(any_rec, any_ad, any_err, false, sel_color, status_bgcolor);
                                if let Some(c) = tint {
                                    body.ui_mut().visuals_mut().selection.bg_fill = c;
                                }
                                body.row(24.0, |mut tr| {
                                    tr.set_selected(tint.is_some());
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
                                    if show_actions {
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
                                    }
                                    tr.col(|ui| {
                                        platform_icons(ui, &ptex, &platforms);
                                    });
                                    tr.col(|ui| {
                                        // Disclosure triangle, then the chosen-platform
                                        // avatar, then the channel name.
                                        let mut clicked = false;
                                        if ninst > 0 {
                                            let tri = if expanded { "▼" } else { "▶" };
                                            if ui
                                                .add(egui::Button::new(tri).small().frame(false))
                                                .on_hover_text("Expand / collapse")
                                                .clicked()
                                            {
                                                clicked = true;
                                            }
                                        } else {
                                            ui.add_space(16.0);
                                        }
                                        if let Some(tex) = channel_avatars.get(&cid) {
                                            ui.add(
                                                egui::Image::from_texture(tex)
                                                    .fit_to_exact_size(egui::vec2(18.0, 18.0))
                                                    .corner_radius(egui::CornerRadius::same(3)),
                                            );
                                            ui.add_space(3.0);
                                        }
                                        ui.label(
                                            egui::RichText::new(&ch.name)
                                                .strong()
                                                .color(channel_event_color(cid, &ch.color)),
                                        );
                                        disc = clicked;
                                    });
                                    tr.col(|ui| {
                                        ui.weak(if ninst == 1 {
                                            "1 instance".to_string()
                                        } else {
                                            format!("{ninst} instances")
                                        });
                                    });
                                    tr.col(|_ui| {}); // detection
                                    tr.col(|ui| {
                                        ui.label(fmt_datetime_short(last_poll));
                                    });
                                    tr.col(|ui| {
                                        if any_rec {
                                            ui.colored_label(
                                                rec_status_color("recording"),
                                                "recording",
                                            );
                                        } else if let Some(p) = primary {
                                            if p.last_recording_status.as_deref() == Some("failed") {
                                                ui.colored_label(rec_status_color("failed"), "failed")
                                                    .on_hover_text(fail_hover(&p.last_recording_log));
                                            }
                                        }
                                    });
                                    tr.col(|ui| {
                                        if next_stream_cell(ui, next_stream_at, &next_stream_title, true) {
                                            open_schedule_popup = next_stream_mid;
                                        }
                                    });
                                    tr.col(|ui| {
                                        meta_value_cell(ui, &cur_category);
                                    });
                                    tr.col(|ui| {
                                        meta_value_cell(ui, &cur_title);
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
                                            ad_cell(ui, fmt_ad_count(c), c, s, None, None);
                                        }
                                    });
                                    tr.col(|ui| {
                                        if let Some((c, s)) = ads {
                                            ad_cell(ui, fmt_ad_time(s), c, s, None, None);
                                        }
                                    });
                                    tr.col(|ui| {
                                        if !ad_free.0.is_empty() {
                                            ui.colored_label(SUCCESS_GREEN, ad_free.0);
                                        }
                                    });
                                    tr.col(|ui| {
                                        if let Some(c) = meta_changes {
                                            meta_cell(ui, c, None, false);
                                        }
                                    });
                                    tr.col(|ui| {
                                        ui.label(fmt_date(ch.created_at));
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
                                        if any_err {
                                            ui.separator();
                                            if ui.button("✖  Clear error").clicked() {
                                                clear_channel_err = Some(cid);
                                                ui.close();
                                            }
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
                                let mid = row.monitor.id;
                                let recording = self
                                    .core
                                    .active
                                    .lock()
                                    .unwrap()
                                    .contains_key(&mid);
                                let chat_active = self
                                    .core
                                    .active_chats
                                    .lock()
                                    .unwrap()
                                    .contains_key(&mid);
                                let is_selected = selected_monitor == Some(mid);
                                let has_hist = row.recording_count > 0;
                                let expanded = exp_instances.contains(&mid);
                                // Tint by state: ad playing / recording / errored /
                                // keyboard-selected.
                                let tint = row_tint(
                                    recording,
                                    ad_running(mid),
                                    monitor_errored(row),
                                    is_selected,
                                    sel_color,
                                    status_bgcolor,
                                );
                                if let Some(c) = tint {
                                    body.ui_mut().visuals_mut().selection.bg_fill = c;
                                }
                                body.row(24.0, |mut tr| {
                                    if render_instance_row(
                                        &mut tr, row, &ptex, now, recording, chat_active,
                                        tint.is_some(), depth, has_hist, expanded,
                                        show_actions, &mut acts,
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
                                // A single-take stream maps to one file (offer it in
                                // the context menu); multi-take streams don't.
                                let single_file = (g.takes.len() == 1
                                    && !g.takes[0].output_path.is_empty())
                                .then(|| g.takes[0].output_path.clone());
                                let ad_count = g.ad_count();
                                let ad_secs = g.ad_secs();
                                // A single-take stream carries the cut detail on its
                                // one take; multi-take streams show per-take cuts when
                                // expanded.
                                let ad_rec =
                                    if g.takes.len() == 1 { Some(g.takes[0].id) } else { None };
                                let meta_count = g.meta_change_count();
                                // Same rule as ads: a single-take stream carries its
                                // detail directly; multi-take shows per-take on expand.
                                let meta_rec =
                                    if g.takes.len() == 1 { Some(g.takes[0].id) } else { None };
                                body.row(24.0, |mut tr| {
                                    let mut disc = false;
                                    tr.col(|_ui| {}); // on
                                    if show_actions {
                                        tr.col(|ui| {
                                            let ok =
                                                dir.as_ref().map(|d| d.is_dir()).unwrap_or(false);
                                            if ui
                                                .add_enabled(ok, egui::Button::new("📂").small())
                                                .on_hover_text("Open folder")
                                                .clicked()
                                            {
                                                open_path = dir.clone();
                                            }
                                        });
                                    }
                                    tr.col(|_ui| {}); // platform
                                    tr.col(|ui| {
                                        disc = tree_name(
                                            ui, depth, has_takes, expanded,
                                            egui::RichText::new(label.clone()),
                                        );
                                        if has_takes {
                                            ui.weak(format!("· {} takes", g.takes.len()));
                                        }
                                    });
                                    tr.col(|_ui| {}); // tool
                                    tr.col(|_ui| {}); // detection
                                    tr.col(|_ui| {}); // polled
                                    tr.col(|ui| {
                                        let resp =
                                            ui.colored_label(rec_status_color(g.status()), g.status());
                                        if g.status() == "failed" {
                                            let log = g
                                                .takes
                                                .last()
                                                .map(|t| t.log_excerpt.as_str())
                                                .unwrap_or("");
                                            resp.on_hover_text(fail_hover(log));
                                        }
                                    });
                                    tr.col(|_ui| {}); // next stream (n/a per stream)
                                    tr.col(|ui| {
                                        meta_value_cell(ui, g.category());
                                    });
                                    tr.col(|ui| {
                                        meta_value_cell(ui, g.title());
                                    });
                                    tr.col(|ui| {
                                        ui.label(fmt_went_live(g.went_live_at, g.went_live_approx));
                                    });
                                    tr.col(|ui| {
                                        ui.label(fmt_datetime_short(g.started_at()));
                                    });
                                    tr.col(|ui| {
                                        // Resolved lost time when known; else the
                                        // provisional started - went_live (so the stream
                                        // row matches the monitor row instead of going
                                        // blank while a capture is still catching up).
                                        let lost = match g.lost_secs() {
                                            Some(l) => Some(fmt_duration(l.max(0))),
                                            None => g
                                                .went_live_at
                                                .map(|w| fmt_duration((g.started_at() - w).max(0))),
                                        };
                                        if let Some(s) = lost {
                                            ui.label(s);
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
                                        let det = ad_rec.and_then(|id| ad_breaks.get(&id));
                                        if let Some(r) = ad_cell(
                                            ui, fmt_ad_count(ad_count), ad_count, ad_secs, det, ad_rec,
                                        ) {
                                            open_ad_popup = Some(r);
                                        }
                                    });
                                    tr.col(|ui| {
                                        let det = ad_rec.and_then(|id| ad_breaks.get(&id));
                                        if let Some(r) = ad_cell(
                                            ui, fmt_ad_time(ad_secs), ad_count, ad_secs, det, ad_rec,
                                        ) {
                                            open_ad_popup = Some(r);
                                        }
                                    });
                                    tr.col(|_ui| {}); // ad-free (n/a per stream)
                                    tr.col(|ui| {
                                        // Single-take streams show the change list on
                                        // hover; double-click (any take count) opens the
                                        // popup with all takes aggregated chronologically.
                                        let det = meta_rec.and_then(|id| meta_logs.get(&id));
                                        if meta_cell(ui, meta_count, det, true) {
                                            open_meta_popup = Some(MetaPopup::Stream(
                                                g.takes.iter().map(|t| (t.id, t.started_at)).collect(),
                                            ));
                                        }
                                    });
                                    tr.col(|_ui| {}); // added
                                    tr.response().context_menu(|ui| {
                                        ui.set_min_width(180.0);
                                        let dir_ok =
                                            dir.as_ref().map(|d| d.is_dir()).unwrap_or(false);
                                        if ui
                                            .add_enabled(dir_ok, egui::Button::new("📂  Open folder"))
                                            .clicked()
                                        {
                                            open_path = dir.clone();
                                            ui.close();
                                        }
                                        if let Some(f) = &single_file {
                                            let file_ok = std::path::Path::new(f).is_file();
                                            if ui
                                                .add_enabled(
                                                    file_ok,
                                                    egui::Button::new("▶  Open file"),
                                                )
                                                .clicked()
                                            {
                                                open_path = Some(std::path::PathBuf::from(f));
                                                ui.close();
                                            }
                                            if ui.button("📋  Copy file path").clicked() {
                                                copy_text = Some(f.clone());
                                                ui.close();
                                            }
                                        }
                                        if ui
                                            .add_enabled(
                                                dir.is_some(),
                                                egui::Button::new("📋  Copy folder path"),
                                            )
                                            .clicked()
                                        {
                                            copy_text =
                                                dir.as_ref().map(|d| d.to_string_lossy().into_owned());
                                            ui.close();
                                        }
                                    });
                                    if disc {
                                        toggle_stream = Some(g.key.clone());
                                    }
                                });
                            }
                            Vis::Take { mid, gi, ti, depth } => {
                                let g = &groups[&mid][gi];
                                let t = &g.takes[ti];
                                let take_variant = dual_take_variant(g, t);
                                let dir = std::path::Path::new(&t.output_path)
                                    .parent()
                                    .map(|p| p.to_path_buf());
                                body.row(24.0, |mut tr| {
                                    tr.col(|_ui| {}); // on
                                    if show_actions {
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
                                    }
                                    tr.col(|_ui| {}); // platform
                                    tr.col(|ui| {
                                        let label = match take_variant {
                                            Some(v) => format!("Take {} · {}", ti + 1, v),
                                            None => format!("Take {}", ti + 1),
                                        };
                                        tree_name(
                                            ui, depth, false, false,
                                            egui::RichText::new(label).weak(),
                                        );
                                    });
                                    tr.col(|_ui| {}); // tool
                                    tr.col(|_ui| {}); // detection
                                    tr.col(|_ui| {}); // polled
                                    tr.col(|ui| {
                                        let resp =
                                            ui.colored_label(rec_status_color(&t.status), &t.status);
                                        if t.status == "failed" {
                                            let mut msg = fail_hover(&t.log_excerpt);
                                            if let Some(code) = t.exit_code {
                                                msg = format!("{msg}\n(exit code {code})");
                                            }
                                            resp.on_hover_text(msg);
                                        } else if t.status == "ended" {
                                            resp.on_hover_text(
                                                "The stream had already ended or wasn't live when we \
                                                 tried — nothing to capture (not a failure).",
                                            );
                                        } else if let Some(code) = t.exit_code {
                                            resp.on_hover_text(format!("exit code {code}"));
                                        }
                                    });
                                    tr.col(|_ui| {}); // next stream (n/a per take)
                                    tr.col(|ui| {
                                        meta_value_cell(ui, &t.category);
                                    });
                                    tr.col(|ui| {
                                        meta_value_cell(ui, &t.title);
                                    });
                                    tr.col(|_ui| {}); // went live
                                    tr.col(|ui| {
                                        ui.label(fmt_datetime_short(t.started_at));
                                    });
                                    tr.col(|ui| {
                                        // Resolved lost time when known; else the
                                        // provisional started - went_live (matches the
                                        // monitor row, so a re-attached/in-progress take
                                        // isn't blank while it's still catching up).
                                        let lost = match t.lost_secs {
                                            Some(l) => Some(fmt_duration(l.max(0))),
                                            None => t
                                                .went_live_at
                                                .map(|w| fmt_duration((t.started_at - w).max(0))),
                                        };
                                        if let Some(s) = lost {
                                            ui.label(s);
                                        }
                                    });
                                    tr.col(|ui| {
                                        let d = ui.label(fmt_duration(t.duration_secs(now)));
                                        if t.bytes > 0 {
                                            d.on_hover_text(fmt_bytes(t.bytes));
                                        }
                                    });
                                    tr.col(|ui| {
                                        let det = ad_breaks.get(&t.id);
                                        if let Some(r) = ad_cell(
                                            ui, fmt_ad_count(t.ad_count), t.ad_count, t.ad_secs,
                                            det, Some(t.id),
                                        ) {
                                            open_ad_popup = Some(r);
                                        }
                                    });
                                    tr.col(|ui| {
                                        let det = ad_breaks.get(&t.id);
                                        if let Some(r) = ad_cell(
                                            ui, fmt_ad_time(t.ad_secs), t.ad_count, t.ad_secs,
                                            det, Some(t.id),
                                        ) {
                                            open_ad_popup = Some(r);
                                        }
                                    });
                                    tr.col(|_ui| {}); // ad-free (n/a per take)
                                    tr.col(|ui| {
                                        let det = meta_logs.get(&t.id);
                                        if meta_cell(ui, t.meta_change_count, det, true) {
                                            open_meta_popup = Some(MetaPopup::Take(t.id));
                                        }
                                    });
                                    tr.col(|_ui| {}); // added
                                    tr.response().context_menu(|ui| {
                                        ui.set_min_width(180.0);
                                        let file_ok = !t.output_path.is_empty()
                                            && std::path::Path::new(&t.output_path).is_file();
                                        if ui
                                            .add_enabled(file_ok, egui::Button::new("▶  Open file"))
                                            .clicked()
                                        {
                                            open_path =
                                                Some(std::path::PathBuf::from(&t.output_path));
                                            ui.close();
                                        }
                                        let dir_ok =
                                            dir.as_ref().map(|d| d.is_dir()).unwrap_or(false);
                                        if ui
                                            .add_enabled(dir_ok, egui::Button::new("📂  Open folder"))
                                            .clicked()
                                        {
                                            open_path = dir.clone();
                                            ui.close();
                                        }
                                        if ui
                                            .add_enabled(
                                                !t.output_path.is_empty(),
                                                egui::Button::new("📋  Copy file path"),
                                            )
                                            .clicked()
                                        {
                                            copy_text = Some(t.output_path.clone());
                                            ui.close();
                                        }
                                        ui.separator();
                                        let del_hint = if t.is_active() {
                                            "Stop the recording before removing this take"
                                        } else {
                                            "Remove this take from the list (keeps the file)"
                                        };
                                        if ui
                                            .add_enabled(
                                                !t.is_active(),
                                                egui::Button::new("🗑  Delete from list"),
                                            )
                                            .on_hover_text(del_hint)
                                            .clicked()
                                        {
                                            delete_recording = Some(t.id);
                                            ui.close();
                                        }
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
        if let Some(p) = open_meta_popup {
            self.meta_popup = Some(p);
        }
        // Next stream double-click: a channel/stream/take row sets the local; an
        // instance row routes through RowActions.
        if let Some(mid) = open_schedule_popup.or(acts.open_schedule) {
            self.schedule_popup = Some(mid);
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
        if let Some(mid) = acts.properties {
            self.properties_popup = Some(mid);
            // Invalidate cached icon so it reloads (assets may have been fetched since last open).
            if let Some(r) = self.rows.iter().find(|r| r.monitor.id == mid) {
                self.channel_icons.remove(&r.channel.id);
            }
        }
        if let Some(cid) = acts.add_instance {
            // Look up the container in `channels` (not `rows`) so this also works
            // for an empty container that has no instances yet.
            if let Some(c) = self.channels.iter().find(|c| c.id == cid) {
                self.form = Some(MonitorForm::add_instance(
                    c,
                    &self.monitor_defaults,
                    &self.settings.default_output_dir,
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
                    color: c.color.clone(),
                });
            }
        }
        if let Some((cid, name)) = delete_channel {
            self.confirm_delete_channel = Some((cid, name));
        }
        if let Some(cid) = clear_channel_err {
            if let Err(e) = self.core.store.clear_channel_errors(cid) {
                self.status = format!("Error: {e}");
            } else {
                self.reload_rows();
            }
        }
        if let Some(id) = acts.start {
            self.core.manual(ManualCommand::Start(id));
            self.status = "Checking channel… will record if live.".into();
        }
        if let Some(id) = acts.stop {
            self.core.manual(ManualCommand::Stop(id));
            self.status = "Stopping recording…".into();
        }
        if let Some(id) = acts.stop_chat {
            self.core.manual(ManualCommand::StopChat(id));
            self.status = "Stopping chat download…".into();
        }
        if let Some(mid) = acts.view_chat {
            self.open_chat_popup(mid);
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
            // The take (and its cascaded ad breaks / meta changes) is gone; close
            // any popup that referenced it (a take popup for it, or a stream popup
            // that included it).
            if self.ad_popup == Some(rid) {
                self.ad_popup = None;
            }
            let meta_refs_rid = match &self.meta_popup {
                Some(MetaPopup::Take(id)) => *id == rid,
                Some(MetaPopup::Stream(takes)) => takes.iter().any(|(id, _)| *id == rid),
                None => false,
            };
            if meta_refs_rid {
                self.meta_popup = None;
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

    fn background_view(&mut self, ui: &mut egui::Ui) {
        use egui_extras::{Column, TableBuilder};
        let now = now_unix();
        // Next-run estimates, plus the editable enable/disable state for each job.
        let reg = self.core.jobs.lock().unwrap().clone();
        let mut toggles: Vec<(&'static str, &'static str, bool)> = crate::events::TOGGLEABLE_JOBS
            .iter()
            .map(|(name, key)| (*name, *key, self.job_toggles.get(*key).copied().unwrap_or(true)))
            .collect();
        let before: Vec<bool> = toggles.iter().map(|t| t.2).collect();

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui
                    .button("🖥 Process manager")
                    .on_hover_text(
                        "All spawned download tool processes (recordings, videos, chat) — \
                         PIDs, status, and manual Stop / Kill.",
                    )
                    .clicked()
                {
                    self.show_processes = true;
                    self.processes_refreshed = None; // force an immediate refresh
                }
            });
            ui.add_space(8.0);

            // ── Scheduled (periodic jobs) ────────────────────────────────
            ui.strong("Scheduled");
            ui.label(
                egui::RichText::new(
                    "Recurring background jobs. Untick to disable — turning off Live poll \
                     pauses all detection/recording.",
                )
                .small()
                .weak(),
            );
            ui.add_space(4.0);
            egui::Grid::new("bg_scheduled_grid")
                .num_columns(4)
                .striped(true)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    ui.strong("On");
                    ui.strong("Job");
                    ui.strong("Every");
                    ui.strong("Next run");
                    ui.end_row();
                    for (name, _key, en) in toggles.iter_mut() {
                        ui.checkbox(en, "");
                        ui.label(*name);
                        let r = reg.iter().find(|j| j.name == *name);
                        ui.label(
                            r.map(|j| fmt_duration_secs(j.interval_secs))
                                .unwrap_or_else(|| "—".into()),
                        );
                        if !*en {
                            ui.weak("disabled");
                        } else {
                            ui.label(
                                r.map(|j| fmt_relative_future(j.next_run_at - now))
                                    .unwrap_or_else(|| "pending".into()),
                            );
                        }
                        ui.end_row();
                    }
                });

            ui.add_space(12.0);

            // ── Active tasks ─────────────────────────────────────────────
            ui.strong("Active");
            ui.add_space(4.0);

            if self.background_tasks.is_empty() {
                ui.weak("No tasks running.");
            } else {
                ui.push_id("bg_active", |ui| {
                    TableBuilder::new(ui)
                        .striped(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                        .column(Column::auto())                  // Channel
                        .column(Column::auto())                  // Task
                        .column(Column::remainder().clip(true))  // Detail
                        .column(Column::auto())                  // Elapsed
                        .header(20.0, |mut h| {
                            h.col(|ui| { ui.strong("Channel / Label"); });
                            h.col(|ui| { ui.strong("Task"); });
                            h.col(|ui| { ui.strong("Detail"); });
                            h.col(|ui| { ui.strong("Elapsed"); });
                        })
                        .body(|mut body| {
                            for task in &self.background_tasks {
                                body.row(20.0, |mut row| {
                                    row.col(|ui| { ui.label(&task.label); });
                                    row.col(|ui| { ui.label(task.kind.label()); });
                                    row.col(|ui| { ui.label(&task.detail); });
                                    row.col(|ui| {
                                        ui.label(format!(
                                            "⏳ {}",
                                            fmt_duration_secs(now - task.started_at)
                                        ));
                                    });
                                });
                            }
                        });
                });
            }

            ui.add_space(12.0);

            // ── Recent completed / failed ────────────────────────────────
            ui.strong("Recent");
            ui.add_space(4.0);

            if self.finished_tasks.is_empty() {
                ui.weak("No completed tasks yet.");
            } else {
                ui.push_id("bg_recent", |ui| {
                    TableBuilder::new(ui)
                        .striped(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                        .column(Column::auto())
                        .column(Column::auto())
                        .column(Column::remainder().clip(true))
                        .column(Column::auto())
                        .header(20.0, |mut h| {
                            h.col(|ui| { ui.strong("Channel / Label"); });
                            h.col(|ui| { ui.strong("Task"); });
                            h.col(|ui| { ui.strong("Detail"); });
                            h.col(|ui| { ui.strong("Outcome"); });
                        })
                        .body(|mut body| {
                            for (task, outcome, finished_at) in &self.finished_tasks {
                                let dur = fmt_duration_secs(finished_at - task.started_at);
                                let outcome_str = match outcome {
                                    crate::events::TaskOutcome::Completed => {
                                        format!("✔ Completed ({dur})")
                                    }
                                    crate::events::TaskOutcome::Failed(e) => {
                                        format!("✘ Failed: {e}")
                                    }
                                };
                                body.row(20.0, |mut row| {
                                    row.col(|ui| { ui.label(&task.label); });
                                    row.col(|ui| { ui.label(task.kind.label()); });
                                    row.col(|ui| { ui.label(&task.detail); });
                                    row.col(|ui| { ui.label(&outcome_str); });
                                });
                            }
                        });
                });
            }

            ui.add_space(8.0);
        });

        // Persist any toggle changes (after the closure releases its borrows).
        for ((_, key, en), was) in toggles.iter().zip(before.iter()) {
            if en != was {
                self.job_toggles.insert((*key).to_string(), *en);
                let _ = self.core.store.set_setting(key, if *en { "1" } else { "0" });
            }
        }
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
            ui.heading("YouTube Data API usage");
            let key_set = !self.settings.youtube_api_key.trim().is_empty();
            ui.label(
                "By default these YouTube features scrape public pages (free, but can break \
                 when YouTube changes them). With the YouTube API Key set above you can use the \
                 Data API instead for more reliable results — but each call spends quota (the \
                 free daily quota is ~10,000 units).",
            );
            if !key_set {
                ui.colored_label(
                    egui::Color32::from_rgb(0xe0, 0xb0, 0x6c),
                    "⚠ Set a YouTube API Key above to enable these.",
                );
            }
            ui.add_enabled_ui(key_set, |ui| {
                ui.checkbox(
                    &mut self.settings.youtube_api_detect,
                    "Live detection (instead of scraping /live)",
                )
                .on_hover_text(
                    "Use search.list for liveness on YouTube monitors whose detection method is \
                     'Scrape'. ~100 quota units per check — with a long poll interval. (Monitors \
                     already set to the 'YouTube Data API' method use it regardless.)",
                );
                ui.checkbox(
                    &mut self.settings.youtube_api_schedule,
                    "Upcoming schedule — exact times via videos.list",
                )
                .on_hover_text(
                    "Scraping /streams parses human-readable text so times are approximate. \
                     With this enabled, scheduled stream video IDs are collected during scraping \
                     and batched into a single videos.list call (~1 quota unit for ALL channels \
                     combined) to get exact scheduled start times from the API.",
                );
                if self.settings.youtube_api_schedule {
                    if ui
                        .button("Re-fetch missing video IDs")
                        .on_hover_text(
                            "Re-scrape YouTube channels whose schedule entries are missing video \
                             IDs (needed for exact times). Only fetches channels with gaps — \
                             others keep their cached schedules.",
                        )
                        .clicked()
                    {
                        self.core.request_yt_video_id_refetch();
                    }
                }
            });

            ui.add_space(12.0);
            ui.heading("Discord schedule import");
            ui.label(
                "Import upcoming streams from Discord scheduled events in the servers you're in. \
                 Events whose location/description contains a monitored channel's stream URL are \
                 attached to it — useful for streamers who post their schedule on Discord but \
                 don't publish a Twitch/YouTube one.",
            );
            ui.colored_label(
                egui::Color32::from_rgb(0xe0, 0x6c, 0x6c),
                "⚠ This uses your personal Discord token. Automating a user token is against \
                 Discord's Terms of Service and could get your account banned — use at your own risk.",
            );
            ui.horizontal(|ui| {
                ui.label("Discord user token");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.discord_token)
                        .password(true)
                        .desired_width(280.0),
                );
            });
            let token_set = !self.settings.discord_token.trim().is_empty();
            if !token_set {
                ui.colored_label(
                    egui::Color32::from_rgb(0xe0, 0xb0, 0x6c),
                    "⚠ Paste your Discord token above to enable import.",
                );
            }
            ui.add_enabled_ui(token_set, |ui| {
                ui.checkbox(
                    &mut self.settings.discord_schedule,
                    "Import schedules from Discord events",
                )
                .on_hover_text(
                    "Sweeps your Discord servers a few hours apart (and on a manual reload), \
                     matching scheduled events to your monitors by stream URL. Discord events are \
                     only used for channels without a published Twitch/YouTube schedule.",
                );
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
                    ui.label("Filename media info")
                        .on_hover_text(
                            "How the {resolution}/{height}/{width}/{fps}/{vcodec} filename \
                             variables get their values. Only applies when the filename \
                             template uses one of them.",
                        );
                    let mode = &mut self.settings.filename_media_info;
                    egui::ComboBox::from_id_salt("media_info_cb")
                        .selected_text(mode.label())
                        .show_ui(ui, |ui| {
                            for m in MediaInfoMode::ALL {
                                ui.selectable_value(mode, m, m.label())
                                    .on_hover_text(m.tooltip());
                            }
                        });
                    ui.end_row();

                    ui.label("Date format").on_hover_text(
                        "How dates and timestamps are shown throughout the app \
                         (the Polled / Went Live / Started On / Added columns, the \
                         history tree, etc.). Applies on Save.",
                    );
                    let df = &mut self.settings.date_fmt;
                    egui::ComboBox::from_id_salt("date_fmt_cb")
                        .selected_text(df.label())
                        .show_ui(ui, |ui| {
                            for f in DateFmt::ALL {
                                ui.selectable_value(df, f, f.label());
                            }
                        });
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.heading("Display");
            if ui
                .checkbox(&mut self.show_actions, "Show Actions column")
                .on_hover_text(
                    "Show the per-row Actions buttons column in the Streams and Videos \
                     tables. Turn it off to reclaim width — every action is also on each \
                     row's right-click context menu. Applies immediately.",
                )
                .changed()
            {
                let _ = self.core.store.set_setting(
                    K_SHOW_ACTIONS,
                    if self.show_actions { "1" } else { "0" },
                );
            }

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
            ui.heading("yt-dlp default arguments");
            ui.label("Prepended to every yt-dlp invocation. Per-channel extra args are appended after and override these.");
            egui::Grid::new("ytdlp_args_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Extra args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ytdlp_default_args)
                            .hint_text("e.g. --js-runtimes node --cookies-from-browser firefox:dmrf6eed.YouTube")
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(
                        "Shell-style space-separated arguments. Quoted strings are supported \
                         (e.g. \"value with spaces\"). Applied to all yt-dlp monitors; \
                         useful for --js-runtimes node, --cookies-from-browser, \
                         --concurrent-fragments, --throttled-rate, etc.",
                    );
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.heading("YouTube SABR (live-from-start)");
            ui.label(
                "YouTube live capture-from-start needs the SABR protocol, which only the \
                 yt-dlp dev build provides. Point to that binary below; it is used ONLY for \
                 YouTube monitors with Capture-from-start. Chat, assets, VODs, and every other \
                 capture keep using the system yt-dlp.",
            );
            egui::Grid::new("sabr_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("System yt-dlp path");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.ytdlp_binary_path)
                                .hint_text("(empty = yt-dlp on PATH)")
                                .desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = browse_file(&self.settings.ytdlp_binary_path) {
                                self.settings.ytdlp_binary_path = p;
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("SABR build path");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.sabr_binary_path)
                                .hint_text(r"e.g. C:\git\yt-dlp-dev\dist\yt-dlp.exe")
                                .desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = browse_file(&self.settings.sabr_binary_path) {
                                self.settings.sabr_binary_path = p;
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "The yt-dlp dev fork with SABR support (bashonly's feat/youtube/sabr). \
                         A moving target — re-point this after rebuilding it. Empty = SABR off.",
                    );
                    ui.end_row();

                    ui.label("Use SABR for capture-from-start");
                    ui.checkbox(&mut self.settings.sabr_enabled, "").on_hover_text(
                        "When on (and a SABR build is set), YouTube monitors with \
                         Capture-from-start record via the SABR build.",
                    );
                    ui.end_row();

                    ui.label("SABR format");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.sabr_format)
                            .hint_text(crate::downloader::SABR_DEFAULT_FORMAT)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("SABR extractor-args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.sabr_extractor_args)
                            .hint_text(crate::downloader::SABR_DEFAULT_EXTRACTOR_ARGS)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("SABR manual args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.sabr_raw_args)
                            .hint_text("(optional — overrides format + extractor-args above)")
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(
                        "When set, these raw args replace the SABR format + extractor-args \
                         preset entirely (put your own -f / --extractor-args here).",
                    );
                    ui.end_row();

                    ui.label("PO token extractor-args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.sabr_pot_args)
                            .hint_text(crate::downloader::SABR_DEFAULT_POT_ARGS)
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(
                        "Passed as a SEPARATE --extractor-args entry on the SABR command \
                         (different extractor key than the format args above), for a GVS \
                         PO-token provider such as bgutil. Default points at the bgutil HTTP \
                         server on its standard port 4416. Leave empty to rely on the \
                         provider plugin's own auto-detection. Requires the provider plugin \
                         installed for the SABR build + its server running.",
                    );
                    ui.end_row();

                    ui.label("DASH companion format");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.dash_format)
                            .hint_text(crate::downloader::DASH_DEFAULT_FORMAT)
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(
                        "Format selector for the DASH companion process when a monitor has \
                         Dual capture (SABR + DASH) enabled. Uses the system yt-dlp.",
                    );
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.heading("Stream monitor defaults");
            ui.label(
                "Applied when creating a new monitor. Platform settings override the global; \
                 leave a field unset / empty to inherit from the global (or the built-in fallback).",
            );
            ui.add_space(4.0);

            // Work on a local clone to avoid borrow-checker issues (cross-field access
            // for hint text vs mutable edit access for the combo/text widgets).
            let mut md = self.monitor_defaults.clone();

            for (label, platform_opt) in [
                ("🌐  Global", None),
                ("  Twitch",   Some(Platform::Twitch)),
                ("  YouTube",  Some(Platform::YouTube)),
                ("  Kick",     Some(Platform::Kick)),
                ("  Generic",  Some(Platform::Generic)),
            ] {
                let default_open = platform_opt.is_none();
                egui::CollapsingHeader::new(label)
                    .default_open(default_open)
                    .show(ui, |ui| {
                        let inherit = if platform_opt.is_some() { "Inherit" } else { "Not set" };

                        let methods: &[DetectionMethod] = match platform_opt {
                            None => &[
                                DetectionMethod::TwitchApi,
                                DetectionMethod::EventSubHelix,
                                DetectionMethod::YouTubeApi,
                                DetectionMethod::WebSub,
                                DetectionMethod::Scrape,
                                DetectionMethod::KickApi,
                                DetectionMethod::GenericProbe,
                            ],
                            Some(p) => p.detection_methods(),
                        };

                        // Pre-compute hints from global for per-platform sections.
                        let q_hint: String = match platform_opt {
                            None => "best".to_string(),
                            Some(_) => md.global.quality.clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "best".to_string()),
                        };
                        let pi_hint: String = match platform_opt {
                            None => "60".to_string(),
                            Some(_) => md.global.poll_interval_secs
                                .unwrap_or(60)
                                .to_string(),
                        };
                        let ft_hint: String = match platform_opt {
                            None => "{name}_{date}_{time}".to_string(),
                            Some(_) => md.global.filename_template.clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "{name}_{date}_{time}".to_string()),
                        };
                        let od_hint: String = match platform_opt {
                            None => self.settings.default_output_dir.clone(),
                            Some(_) => md.global.output_dir.clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| self.settings.default_output_dir.clone()),
                        };
                        let fs_hint: String = if platform_opt.is_some() {
                            match md.global.from_start {
                                Some(true) => "Inherit (on)".to_string(),
                                Some(false) => "Inherit (off)".to_string(),
                                None => "Inherit (on)".to_string(),
                            }
                        } else {
                            "on".to_string()
                        };

                        let d = match platform_opt {
                            None => &mut md.global,
                            Some(p) => md.get_mut(p),
                        };

                        egui::Grid::new(format!("mdef_{label}"))
                            .num_columns(4)
                            .spacing([8.0, 6.0])
                            .show(ui, |ui| {
                                // Row 1: Tool, Detection
                                ui.label("Tool");
                                egui::ComboBox::from_id_salt(format!("mdef_tool_{label}"))
                                    .selected_text(match d.tool {
                                        None => inherit,
                                        Some(t) => t.label(),
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut d.tool, None, inherit);
                                        for &t in &Tool::ALL {
                                            ui.selectable_value(&mut d.tool, Some(t), t.label());
                                        }
                                    });
                                ui.label("Detection");
                                egui::ComboBox::from_id_salt(format!("mdef_det_{label}"))
                                    .selected_text(match d.detection_method {
                                        None => inherit,
                                        Some(m) => m.label(),
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut d.detection_method, None, inherit);
                                        for &m in methods {
                                            ui.selectable_value(&mut d.detection_method, Some(m), m.label());
                                        }
                                    });
                                ui.end_row();

                                // Row 2: Container, Quality
                                ui.label("Container");
                                egui::ComboBox::from_id_salt(format!("mdef_cont_{label}"))
                                    .selected_text(match d.container {
                                        None => inherit,
                                        Some(c) => c.label(),
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut d.container, None, inherit);
                                        for &c in &Container::ALL {
                                            ui.selectable_value(&mut d.container, Some(c), c.label());
                                        }
                                    });
                                ui.label("Quality");
                                let q_ref = d.quality.get_or_insert_with(String::new);
                                ui.add(
                                    egui::TextEdit::singleline(q_ref)
                                        .hint_text(q_hint)
                                        .desired_width(100.0),
                                );
                                ui.end_row();

                                // Row 3: Poll interval
                                ui.label("Poll interval (s)");
                                let mut pi_str = d.poll_interval_secs
                                    .map(|v| v.to_string())
                                    .unwrap_or_default();
                                if ui.add(
                                    egui::TextEdit::singleline(&mut pi_str)
                                        .hint_text(pi_hint)
                                        .desired_width(80.0),
                                ).changed() {
                                    d.poll_interval_secs = pi_str.trim().parse::<i64>().ok()
                                        .filter(|&v| v > 0);
                                }
                                ui.label("");
                                ui.label("");
                                ui.end_row();

                                // Row 4: Filename template
                                ui.label("Filename");
                                let ft_ref = d.filename_template.get_or_insert_with(String::new);
                                ui.add(
                                    egui::TextEdit::singleline(ft_ref)
                                        .hint_text(ft_hint)
                                        .desired_width(200.0),
                                ).on_hover_text(
                                    "Tokens: {name} {date} {time} {timestamp} {year} {month} {day} {hour} {minute} {second} {title} {games} {video_id} {quality} {resolution} {height} {width} {fps} {vcodec} {acodec} {take} {tool} {mode} {platform} {went_live_date} {went_live_time}",
                                );
                                ui.label("");
                                ui.label("");
                                ui.end_row();

                                // Row 5: Output directory
                                ui.label("Output dir");
                                let od_ref = d.output_dir.get_or_insert_with(String::new);
                                ui.add(
                                    egui::TextEdit::singleline(od_ref)
                                        .hint_text(od_hint)
                                        .desired_width(200.0),
                                );
                                ui.label("");
                                ui.label("");
                                ui.end_row();

                                // Row 6: Capture from start
                                ui.label("Capture from start")
                                    .on_hover_text(
                                        "yt-dlp --live-from-start / streamlink --hls-live-restart.\n\
                                         Default for new stream monitors on this platform.",
                                    );
                                egui::ComboBox::from_id_salt(format!("mdef_fs_{label}"))
                                    .selected_text(match d.from_start {
                                        None => inherit,
                                        Some(true) => "On",
                                        Some(false) => "Off",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut d.from_start, None, format!("{inherit} ({fs_hint})"));
                                        ui.selectable_value(&mut d.from_start, Some(true), "On");
                                        ui.selectable_value(&mut d.from_start, Some(false), "Off");
                                    });
                                ui.label("");
                                ui.label("");
                                ui.end_row();
                            });
                    });
            }

            // Write back the (possibly edited) clone.
            self.monitor_defaults = md;

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

            ui.add_space(12.0);
            ui.heading("Notifications");
            let mut notify_on = self.notifications_enabled;
            if ui
                .checkbox(&mut notify_on, "Show desktop notifications")
                .on_hover_text(
                    "Show a desktop toast when a recording starts, finishes, or errors. \
                     Uncheck to silence all pop-up alerts (the in-app status line and \
                     Background view still update). Takes effect immediately.",
                )
                .changed()
            {
                self.notifications_enabled = notify_on;
                let _ = self
                    .core
                    .store
                    .set_setting(
                        crate::notifications::K_NOTIFICATIONS,
                        if notify_on { "1" } else { "0" },
                    );
                self.status = if notify_on {
                    "Desktop notifications enabled.".into()
                } else {
                    "Desktop notifications disabled.".into()
                };
            }

            ui.add_space(12.0);
            ui.heading("Shutdown");
            let mut keep = self.keep_downloads_on_quit;
            if ui
                .checkbox(&mut keep, "Keep downloads running when the app closes")
                .on_hover_text(
                    "Default. Quitting detaches the recording tools so they keep running and \
                     writing — the app re-attaches to them on the next launch, so you can \
                     restart or rebuild without stopping a recording. Uncheck to stop all \
                     downloads on quit instead. (The tray's \"Quit & stop recordings\" always \
                     stops them, regardless of this.)",
                )
                .changed()
            {
                self.keep_downloads_on_quit = keep;
                // Stored inverted: the setting names the opt-IN to stopping.
                let _ = self
                    .core
                    .store
                    .set_setting("stop_downloads_on_quit", if keep { "0" } else { "1" });
                self.status = if keep {
                    "Downloads will keep running when the app closes.".into()
                } else {
                    "Downloads will stop when the app closes.".into()
                };
            }

            ui.add_space(16.0);
            if ui.button("💾 Save settings").clicked() {
                self.save_settings();
            }
        });
    }

    /// Task-manager-style dialog listing every spawned download tool process with
    /// its PID, status, and uptime, plus per-process Stop (graceful) / Kill (force)
    /// and reveal-log/folder actions. Doubles as a live list of spawned processes.
    #[allow(deprecated)]
    fn processes_window(&mut self, ctx: &egui::Context) {
        use crate::models::DetachedKind;
        use egui_extras::{Column, TableBuilder};
        use std::time::{Duration, Instant};
        if !self.show_processes {
            return;
        }
        // Throttle the snapshot (each row does a pid_alive + a couple DB reads).
        let stale = self
            .processes_refreshed
            .map(|t| t.elapsed() >= Duration::from_millis(1500))
            .unwrap_or(true);
        if stale {
            self.processes = self.core.list_processes();
            self.processes_refreshed = Some(Instant::now());
        }
        ctx.request_repaint_after(Duration::from_millis(1500));

        let now = now_unix();
        let mut open = true;
        enum Act {
            Refresh,
            Stop(usize),
            Kill(usize),
            RevealLog(usize),
            RevealDir(usize),
        }
        let mut act: Option<Act> = None;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("processes_vp"),
            egui::ViewportBuilder::default()
                .with_title("🖥 Processes")
                .with_inner_size([800.0, 440.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} spawned process(es)", self.processes.len()));
                        if ui.button("⟳ Refresh").clicked() {
                            act = Some(Act::Refresh);
                        }
                        ui.weak("Stop = graceful (file finalized) · Kill = force-terminate the tree");
                    });
                    ui.separator();
                    if self.processes.is_empty() {
                        ui.weak("No download tool processes are running.");
                        return;
                    }
                    TableBuilder::new(ui)
                        .striped(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                        .column(Column::auto()) // PID
                        .column(Column::auto()) // Type
                        .column(Column::remainder().clip(true)) // Name
                        .column(Column::auto()) // Tool
                        .column(Column::auto()) // Status
                        .column(Column::auto()) // Uptime
                        .column(Column::auto()) // Actions
                        .header(20.0, |mut h| {
                            h.col(|ui| { ui.strong("PID"); });
                            h.col(|ui| { ui.strong("Type"); });
                            h.col(|ui| { ui.strong("Name"); });
                            h.col(|ui| { ui.strong("Tool"); });
                            h.col(|ui| { ui.strong("Status"); });
                            h.col(|ui| { ui.strong("Uptime"); });
                            h.col(|ui| { ui.strong("Actions"); });
                        })
                        .body(|mut body| {
                            for (i, p) in self.processes.iter().enumerate() {
                                body.row(22.0, |mut row| {
                                    row.col(|ui| { ui.monospace(p.pid.to_string()); });
                                    row.col(|ui| {
                                        let t = match p.kind {
                                            DetachedKind::Recording => {
                                                if p.secondary { "rec · dash" } else { "recording" }
                                            }
                                            DetachedKind::Video => "video",
                                            DetachedKind::Chat => "chat",
                                        };
                                        ui.label(t);
                                    });
                                    row.col(|ui| {
                                        ui.label(&p.name).on_hover_text(&p.capture_path);
                                    });
                                    row.col(|ui| { ui.label(&p.tool); });
                                    row.col(|ui| {
                                        if p.reattached {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(0x6c, 0xb0, 0xe0),
                                                "⛓ re-attached",
                                            )
                                            .on_hover_text(format!(
                                                "running under a prior build: {}",
                                                p.spawn_build
                                            ));
                                        } else {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(0x6c, 0xe0, 0x8c),
                                                "● running",
                                            );
                                        }
                                    });
                                    row.col(|ui| {
                                        ui.label(fmt_duration_secs((now - p.started_at).max(0)));
                                    });
                                    row.col(|ui| {
                                        if ui
                                            .small_button("Stop")
                                            .on_hover_text(
                                                "Graceful: stop the tool and let the app finalize \
                                                 (remux + mark the take stopped).",
                                            )
                                            .clicked()
                                        {
                                            act = Some(Act::Stop(i));
                                        }
                                        if ui
                                            .small_button("Kill")
                                            .on_hover_text(
                                                "Force-terminate the whole process tree now — the \
                                                 capture may be left un-finalized.",
                                            )
                                            .clicked()
                                        {
                                            act = Some(Act::Kill(i));
                                        }
                                        if ui.small_button("Log").on_hover_text(&p.log_path).clicked() {
                                            act = Some(Act::RevealLog(i));
                                        }
                                        if ui.small_button("Folder").clicked() {
                                            act = Some(Act::RevealDir(i));
                                        }
                                    });
                                });
                            }
                        });
                });
            },
        );

        if !open {
            self.show_processes = false;
        }
        match act {
            Some(Act::Refresh) => self.processes_refreshed = None,
            Some(Act::Stop(i)) => {
                if let Some(p) = self.processes.get(i) {
                    self.core.stop_process(p);
                    self.status = format!("Stopping pid {} ({})…", p.pid, p.name);
                    self.processes_refreshed = None;
                }
            }
            Some(Act::Kill(i)) => {
                if let Some(p) = self.processes.get(i) {
                    self.core.force_kill(p.pid, &p.job_name);
                    self.status = format!("Killed pid {} ({}).", p.pid, p.name);
                    self.processes_refreshed = None;
                }
            }
            Some(Act::RevealLog(i)) => {
                if let Some(p) = self.processes.get(i) {
                    crate::platform::open_path(std::path::Path::new(&p.log_path));
                }
            }
            Some(Act::RevealDir(i)) => {
                if let Some(p) = self.processes.get(i) {
                    if let Some(dir) = std::path::Path::new(&p.capture_path).parent() {
                        crate::platform::open_path(dir);
                    }
                }
            }
            None => {}
        }
    }

    #[allow(deprecated)]
    fn form_window(&mut self, ctx: &egui::Context) {
        if self.form.is_none() {
            return;
        }
        let mut open = true;
        let mut do_save = false;
        let mut do_cancel = false;
        let mut open_format_designer = false;

        let f = self.form.as_ref().unwrap();
        let title = if f.monitor_id.is_some() {
            "Edit instance"
        } else if f.channel_id.is_some() {
            "Add instance"
        } else {
            "Add stream (new channel)"
        };

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("monitor_form_vp"),
            egui::ViewportBuilder::default()
                .with_title(title.to_string())
                .with_inner_size([700.0, 600.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                let form = self.form.as_mut().unwrap();
                let platform = Platform::detect(&form.url);
                // When the URL's platform changes, re-apply that platform's
                // defaults (tool, detection, container, quality, poll interval,
                // filename template, output dir). User overrides afterwards stick.
                if form.last_platform != Some(platform) {
                    let md = &self.monitor_defaults;
                    form.tool = md.resolve_tool(platform);
                    form.detection_method = md.resolve_detection(platform);
                    form.container = md.resolve_container(platform);
                    form.quality = md.resolve_quality(platform);
                    form.poll_interval_secs = md.resolve_poll_interval(platform);
                    form.filename_template = md.resolve_filename_template(platform);
                    form.output_dir = md.resolve_output_dir(platform, &self.settings.default_output_dir);
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

                        ui.label("Audio tracks");
                        ui.text_edit_singleline(&mut form.audio_tracks).on_hover_text(
                            "Audio tracks to capture (streamlink --hls-audio-select). \
                             Empty = the tool's default single track; 'all' (or '*') = \
                             every track; or a comma-separated list of language \
                             codes/names. streamlink-only; ffmpeg copy keeps all tracks.",
                        );
                        ui.end_row();

                        ui.label("Subtitle tracks");
                        ui.text_edit_singleline(&mut form.subtitle_tracks).on_hover_text(
                            "Subtitle tracks to capture (yt-dlp --sub-langs, written as \
                             sidecar files next to the recording). Empty = none; 'all' \
                             (or '*') = every subtitle; or a comma-separated list of \
                             language codes. yt-dlp-only; streamlink can't mux subtitles. \
                             Best-effort for live streams.",
                        );
                        ui.end_row();

                        ui.label("Log chat");
                        ui.checkbox(&mut form.chat_log, "").on_hover_text(
                            "Save chat alongside the recording. Twitch: a built-in \
                             anonymous chat logger writes a .chat.jsonl sidecar. YouTube \
                             (yt-dlp tool): yt-dlp's live_chat writes a .live_chat.json \
                             sidecar. Other platforms/tools don't capture chat.",
                        );
                        ui.end_row();

                        ui.label("Fetch thumbnail");
                        ui.checkbox(&mut form.fetch_thumbnail, "").on_hover_text(
                            "Download the stream thumbnail alongside the recording \
                             ({stem}.thumbnail.jpg). For yt-dlp, passes --write-thumbnail; \
                             for Twitch/Kick/YouTube, fetches the URL from detection metadata.",
                        );
                        ui.end_row();

                        ui.label("Fetch chat assets");
                        ui.checkbox(&mut form.fetch_chat_assets, "").on_hover_text(
                            "Download channel icon, offline banner, Twitch badges, and \
                             emotes (including BTTV, FFZ, 7TV) into channel_assets/ \
                             alongside recordings. Needed for full offline chat replay. \
                             Refreshed at most once per 24 hours.",
                        );
                        ui.end_row();

                        ui.label("Capture from start");
                        ui.checkbox(&mut form.capture_from_start, "").on_hover_text(
                            "yt-dlp --live-from-start / streamlink --hls-live-restart",
                        );
                        ui.end_row();

                        if Platform::detect(&form.url) == Platform::YouTube {
                            ui.label("Dual capture (SABR + DASH)");
                            ui.checkbox(&mut form.dual_capture, "").on_hover_text(
                                "YouTube only: also run a second concurrent DASH capture \
                                 (system yt-dlp, live edge) when wanted formats span both SABR \
                                 and DASH. Produces a second recording in the same take. \
                                 Needs Capture-from-start and a configured SABR build.",
                            );
                            ui.end_row();
                        }

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

                        let fn_tmpl_hint = "{name} {date} {time} {year} {month} {day} {hour} {minute} {second} {title} {games} {video_id} {quality} {resolution} {height} {width} {fps} {vcodec} {acodec} {take} {tool} {mode} {platform} {went_live_date} {went_live_time} {timestamp}";
                        ui.label("Filename template").on_hover_text(fn_tmpl_hint);
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut form.filename_template).on_hover_text(fn_tmpl_hint);
                            if ui.button("Design…").on_hover_text("Open the Format Designer to preview and compose the template").clicked() {
                                open_format_designer = true;
                            }
                        });
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
            },
        );

        if do_save {
            self.save_form();
        } else if do_cancel || !open {
            self.form = None;
        }

        if open_format_designer {
            let tmpl = self.form.as_ref().map(|f| f.filename_template.clone()).unwrap_or_default();
            self.open_format_designer(tmpl, Some(FormatDesignerTarget::MonitorForm));
        }
    }

    // ── Chat log viewer ──────────────────────────────────────────────────────

    fn open_chat_popup(&mut self, monitor_id: i64) {
        let monitor_name = self
            .rows
            .iter()
            .find(|r| r.monitor.id == monitor_id)
            .map(|r| r.channel.name.clone())
            .unwrap_or_default();
        let recs = self
            .core
            .store
            .recordings_for_monitor(monitor_id)
            .unwrap_or_default();
        let rec = recs
            .iter()
            .rev()
            .find(|r| chat_file_for_recording(r).is_some())
            .or_else(|| recs.last())
            .cloned();

        let state = Arc::new(Mutex::new(ChatLoadState::Loading));
        if let Some(r) = &rec {
            let state2 = state.clone();
            let path_opt = chat_file_for_recording(r);
            let start_ts = r.went_live_at.unwrap_or(r.started_at);
            self.core.rt.spawn(async move {
                let result = tokio::task::spawn_blocking(move || match path_opt {
                    None => ChatLoadState::NoFile,
                    Some(p) => match parse_chat_file(&p, start_ts) {
                        Ok(msgs) => ChatLoadState::Loaded(msgs),
                        Err(e) => ChatLoadState::Error(e.to_string()),
                    },
                })
                .await
                .unwrap_or_else(|e| ChatLoadState::Error(e.to_string()));
                *state2.lock().unwrap() = result;
            });
        } else {
            *state.lock().unwrap() = ChatLoadState::NoFile;
        }
        self.chat_popup = Some(ChatPopup {
            monitor_name,
            recording: rec,
            all_recordings: recs,
            load_state: state,
            search: String::new(),
            full_view: false,
            last_reload: std::time::Instant::now(),
        });
    }

    #[allow(deprecated)]
    fn chat_popup_window(&mut self, ctx: &egui::Context) {
        const CHAT_RELOAD_SECS: u64 = 3;
        let Some(popup) = &mut self.chat_popup else {
            return;
        };
        let mut open = true;
        let title = format!("💬  Chat — {}", popup.monitor_name);

        // Whether the selected recording is still in progress (chat file is growing).
        let rec_active = popup.recording.as_ref().map_or(false, |r| r.ended_at.is_none());
        // Collect everything needed for a tail-reload before the `show` closure
        // borrows `popup` so we can act on it cleanly afterwards.
        let reload_info: Option<(std::path::PathBuf, i64, Arc<Mutex<ChatLoadState>>)> =
            if rec_active
                && popup.last_reload.elapsed()
                    >= std::time::Duration::from_secs(CHAT_RELOAD_SECS)
            {
                popup.recording.as_ref().and_then(|r| {
                    chat_file_for_recording(r).map(|path| {
                        (
                            path,
                            r.went_live_at.unwrap_or(r.started_at),
                            popup.load_state.clone(),
                        )
                    })
                })
            } else {
                None
            };

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("chat_popup_vp"),
            egui::ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size([480.0, 600.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    // ── Toolbar ──────────────────────────────────────────────
                    ui.horizontal(|ui| {
                        // Recording picker: only if >1 recording has a chat file.
                        let recs_with_chat: Vec<_> = popup
                            .all_recordings
                            .iter()
                            .filter(|r| chat_file_for_recording(r).is_some())
                            .collect();
                        if recs_with_chat.len() > 1 {
                            let cur_label = popup
                                .recording
                                .as_ref()
                                .map(fmt_recording_label)
                                .unwrap_or_default();
                            egui::ComboBox::from_id_salt("chat_rec_pick")
                                .selected_text(cur_label)
                                .show_ui(ui, |ui| {
                                    for rec in &recs_with_chat {
                                        let label = fmt_recording_label(rec);
                                        let selected = popup
                                            .recording
                                            .as_ref()
                                            .map(|r| r.id == rec.id)
                                            .unwrap_or(false);
                                        if ui.selectable_label(selected, &label).clicked() {
                                            let new_rec = (*rec).clone();
                                            let state = Arc::new(Mutex::new(ChatLoadState::Loading));
                                            let state2 = state.clone();
                                            let path_opt = chat_file_for_recording(&new_rec);
                                            let start_ts =
                                                new_rec.went_live_at.unwrap_or(new_rec.started_at);
                                            popup.load_state = state;
                                            popup.recording = Some(new_rec);
                                            popup.last_reload = std::time::Instant::now();
                                            self.core.rt.spawn(async move {
                                                let r = tokio::task::spawn_blocking(move || {
                                                    match path_opt {
                                                        None => ChatLoadState::NoFile,
                                                        Some(p) => {
                                                            match parse_chat_file(&p, start_ts) {
                                                                Ok(m) => ChatLoadState::Loaded(m),
                                                                Err(e) => {
                                                                    ChatLoadState::Error(e.to_string())
                                                                }
                                                            }
                                                        }
                                                    }
                                                })
                                                .await
                                                .unwrap_or_else(|e| {
                                                    ChatLoadState::Error(e.to_string())
                                                });
                                                *state2.lock().unwrap() = r;
                                            });
                                        }
                                    }
                                });
                            ui.separator();
                        }

                        // Search filter
                        ui.label("🔍");
                        ui.add(
                            egui::TextEdit::singleline(&mut popup.search)
                                .hint_text("Filter…")
                                .desired_width(150.0),
                        );
                        if !popup.search.is_empty() && ui.small_button("✕").clicked() {
                            popup.search.clear();
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.toggle_value(&mut popup.full_view, "View full");
                        });
                    });
                    ui.separator();

                    // ── Content ──────────────────────────────────────────────
                    let state = popup.load_state.lock().unwrap().clone();
                    match state {
                        ChatLoadState::Loading => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Loading chat…");
                            });
                            ctx.request_repaint();
                        }
                        ChatLoadState::NoFile => {
                            ui.add_space(8.0);
                            ui.label("No chat file found for this recording.");
                            ui.weak("Chat logging must be enabled and a recording must exist.");
                        }
                        ChatLoadState::Error(ref e) => {
                            ui.colored_label(egui::Color32::RED, format!("Failed to load: {e}"));
                        }
                        ChatLoadState::Loaded(ref msgs) => {
                            let q = popup.search.to_lowercase();
                            let all_filtered: Vec<&ChatMessage> = msgs
                                .iter()
                                .filter(|m| {
                                    q.is_empty()
                                        || m.text.to_lowercase().contains(&q)
                                        || m.author.to_lowercase().contains(&q)
                                })
                                .collect();

                            const DEFAULT_CAP: usize = 500;
                            let (visible, capped) =
                                if popup.full_view || all_filtered.len() <= DEFAULT_CAP {
                                    (all_filtered.as_slice(), false)
                                } else {
                                    (&all_filtered[all_filtered.len() - DEFAULT_CAP..], true)
                                };

                            if capped {
                                ui.horizontal(|ui| {
                                    ui.weak(format!(
                                        "Showing last {DEFAULT_CAP} of {} messages",
                                        all_filtered.len()
                                    ));
                                    if ui.small_button("View full").clicked() {
                                        popup.full_view = true;
                                    }
                                });
                            } else {
                                ui.weak(format!("{} messages", all_filtered.len()));
                            }

                            let stick = q.is_empty() && !popup.full_view;
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .stick_to_bottom(stick)
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    for msg in visible {
                                        render_chat_message(ui, msg);
                                    }
                                });
                        }
                    }
                });
            },
        );

        // Tail-reload: re-parse the chat file in the background while the
        // recording is still live so new messages appear without re-opening.
        if let Some((path, start_ts, state)) = reload_info {
            if let Some(p) = &mut self.chat_popup {
                p.last_reload = std::time::Instant::now();
            }
            self.core.rt.spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    match parse_chat_file(&path, start_ts) {
                        Ok(msgs) => ChatLoadState::Loaded(msgs),
                        Err(e) => ChatLoadState::Error(e.to_string()),
                    }
                })
                .await
                .unwrap_or_else(|e| ChatLoadState::Error(e.to_string()));
                *state.lock().unwrap() = result;
            });
        }
        // Keep the UI alive while a live recording is open so the next
        // interval check fires automatically.
        if rec_active {
            ctx.request_repaint_after(std::time::Duration::from_secs(CHAT_RELOAD_SECS));
        }

        if !open {
            self.chat_popup = None;
        }
    }

    #[allow(deprecated)]
    fn properties_window(&mut self, ctx: &egui::Context) {
        let Some(mid) = self.properties_popup else { return };
        let Some(row) = self.rows.iter().find(|r| r.monitor.id == mid).cloned() else {
            self.properties_popup = None;
            return;
        };
        let ch = &row.channel;
        let m = &row.monitor;

        // Every platform this container spans (first-seen order) — drives the
        // chosen-platform avatar and the per-platform asset status below.
        let platforms: Vec<Platform> = {
            let mons: Vec<&MonitorWithChannel> =
                self.rows.iter().filter(|r| r.channel.id == ch.id).collect();
            channel_platforms(&mons)
        };
        // Platforms that actually have an asset source (Generic has none) — used
        // for the icon-source picker and the per-platform status grid.
        let asset_platforms: Vec<Platform> =
            platforms.iter().copied().filter(|p| *p != Platform::Generic).collect();

        // Lazy-load (or serve cached) the chosen-platform channel avatar.
        let icon_tex = self
            .channel_icons
            .entry(ch.id)
            .or_insert_with(|| resolve_channel_icon(ch, &platforms, ctx))
            .clone();

        // Set inside the icon-source combo, applied after the panel renders.
        let mut pref_change: Option<Option<Platform>> = None;

        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("properties_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("Properties — {}", ch.name))
                .with_inner_size([480.0, 600.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                // ── Header ──────────────────────────────────────────────
                ui.horizontal(|ui| {
                    if let Some(tex) = &icon_tex {
                        ui.add(
                            egui::Image::from_texture(tex)
                                .max_size(egui::vec2(96.0, 96.0))
                                .corner_radius(egui::CornerRadius::same(8)),
                        );
                    } else {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(96.0, 96.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(
                            rect,
                            8.0,
                            ui.visuals().weak_text_color(),
                        );
                    }
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.add_space(4.0);
                        ui.heading(&ch.name);
                        ui.horizontal(|ui| {
                            let ptex = self
                                .platform_tex
                                .get_or_insert_with(|| PlatformTextures::load(ui.ctx()));
                            for &p in &platforms {
                                if let Some(t) = ptex.get(p) {
                                    ui.add(
                                        egui::Image::from_texture(t)
                                            .max_size(egui::vec2(14.0, 14.0)),
                                    );
                                }
                            }
                            let names: Vec<&str> =
                                platforms.iter().map(|p| p.label()).collect();
                            ui.label(if names.is_empty() {
                                "—".to_string()
                            } else {
                                names.join(" · ")
                            });
                        });
                    });
                });

                ui.separator();

                // ── Channel ─────────────────────────────────────────────
                ui.strong("Channel");
                egui::Grid::new("props_ch")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("DB channel ID");
                        ui.label(ch.id.to_string());
                        ui.end_row();

                        ui.label("URL");
                        if ui.link(&ch.url).clicked() {
                            ui.ctx()
                                .open_url(egui::OpenUrl::new_tab(ch.url.clone()));
                        }
                        ui.end_row();

                        if ch.platform == Platform::YouTube {
                            let yt_id = extract_yt_channel_id(&ch.url)
                                .unwrap_or_else(|| "— (handle URL, ID not in URL)".into());
                            ui.label("Channel ID");
                            ui.horizontal(|ui| {
                                ui.label(&yt_id);
                                if !yt_id.starts_with('—')
                                    && ui
                                        .small_button("⧉")
                                        .on_hover_text("Copy")
                                        .clicked()
                                {
                                    ui.ctx().copy_text(yt_id.clone());
                                }
                            });
                            ui.end_row();
                        }
                    });

                ui.add_space(6.0);

                // ── Monitor (instance) ───────────────────────────────────
                ui.strong("Monitor (instance)");
                egui::Grid::new("props_mon")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("DB monitor ID");
                        ui.label(m.id.to_string());
                        ui.end_row();
                        ui.label("Detection");
                        ui.label(m.detection_method.as_str());
                        ui.end_row();
                        ui.label("Tool");
                        ui.label(format!("{:?}", m.tool));
                        ui.end_row();
                        ui.label("Poll interval");
                        ui.label(format!("{}s", m.poll_interval_secs));
                        ui.end_row();
                        ui.label("Quality");
                        ui.label(&m.quality);
                        ui.end_row();
                        ui.label("Max concurrent");
                        ui.label(m.max_concurrent.to_string());
                        ui.end_row();
                        ui.label("Last state");
                        ui.label(&m.last_state);
                        ui.end_row();
                        ui.label("Recordings");
                        ui.label(row.recording_count.to_string());
                        ui.end_row();
                        ui.label("Output dir");
                        ui.horizontal(|ui| {
                            ui.label(prop_truncate_path(&m.output_dir, 28));
                            if ui
                                .small_button("📂")
                                .on_hover_text("Open folder")
                                .clicked()
                            {
                                crate::platform::open_path(
                                    std::path::Path::new(&m.output_dir),
                                );
                            }
                        });
                        ui.end_row();
                        ui.label("Fetch thumbnail");
                        ui.label(prop_bool(m.fetch_thumbnail));
                        ui.end_row();
                        ui.label("Fetch assets");
                        ui.label(prop_bool(m.fetch_chat_assets));
                        ui.end_row();
                    });

                // ── Assets ───────────────────────────────────────────────
                {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.strong("Channel assets");
                        if ui
                            .button("⟳ Refetch")
                            .on_hover_text(format!(
                                "Fetch icon / banner / badges / emotes for this instance's \
                                 platform ({}) now — ignores the 24h cache and the per-monitor \
                                 Fetch-assets toggle.",
                                m.platform().label(),
                            ))
                            .clicked()
                        {
                            self.core.manual(ManualCommand::RefetchAssets(m.id));
                            self.channel_icons.remove(&ch.id); // reload after fetch
                            self.status = format!("Refetching assets for {}…", ch.name);
                        }
                        if ui
                            .button("📂")
                            .on_hover_text(
                                "Open the channel's asset folder (per-platform icons, banners, \
                                 and the history/ archive of older versions).",
                            )
                            .clicked()
                        {
                            let root = crate::app_paths::asset_cache_dir()
                                .join("channel_assets")
                                .join(crate::downloader::sanitize_filename(&ch.name));
                            crate::platform::open_path(&root);
                        }
                    });
                    if !m.fetch_chat_assets {
                        ui.label(
                            egui::RichText::new(
                                "Auto-fetch is off for this monitor; Refetch pulls them on demand.",
                            )
                            .small()
                            .weak(),
                        );
                    }

                    // Icon source: which platform's profile pic represents this
                    // container in the Streams list and in this window's header.
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Icon source:");
                        let cur = ch.preferred_platform;
                        egui::ComboBox::from_id_salt("pref_plat_cb")
                            .selected_text(match cur {
                                Some(p) => p.label(),
                                None => "Auto (first available)",
                            })
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(cur.is_none(), "Auto (first available)")
                                    .clicked()
                                {
                                    pref_change = Some(None);
                                }
                                for &p in &asset_platforms {
                                    if ui
                                        .selectable_label(cur == Some(p), p.label())
                                        .clicked()
                                    {
                                        pref_change = Some(Some(p));
                                    }
                                }
                            })
                            .response
                            .on_hover_text(
                                "Which platform's profile pic represents this channel. \
                                 Auto uses the first platform that has a fetched icon.",
                            );
                    });

                    // Per-platform asset status (each platform has its own dir).
                    ui.add_space(4.0);
                    egui::Grid::new("props_assets")
                        .num_columns(6)
                        .spacing([12.0, 4.0])
                        .striped(true)
                        .show(ui, |ui| {
                            ui.strong("Platform");
                            ui.strong("Icon");
                            ui.strong("Banner");
                            ui.strong("Badges");
                            ui.strong("Emotes");
                            ui.strong("Updated");
                            ui.end_row();
                            for &p in &asset_platforms {
                                let pdir = channel_asset_dir(&ch.name, p);
                                ui.horizontal(|ui| {
                                    let ptex = self.platform_tex.get_or_insert_with(|| {
                                        PlatformTextures::load(ui.ctx())
                                    });
                                    if let Some(t) = ptex.get(p) {
                                        ui.add(
                                            egui::Image::from_texture(t)
                                                .max_size(egui::vec2(13.0, 13.0)),
                                        );
                                    }
                                    ui.label(p.label());
                                });
                                asset_status_cell(
                                    ui,
                                    prop_find_first(&pdir, "icon.").is_some(),
                                    prop_variant_count(&pdir, "icon"),
                                );
                                asset_status_cell(
                                    ui,
                                    prop_find_first(&pdir, "banner.").is_some(),
                                    prop_variant_count(&pdir, "banner"),
                                );
                                // Badges + emotes are Twitch-only concepts.
                                let badges = prop_count_nested_dirs(&pdir.join("badges"), 2);
                                ui.label(if badges > 0 {
                                    badges.to_string()
                                } else {
                                    "—".into()
                                });
                                let mut emotes = prop_count_dir_files(
                                    &pdir.join("emotes").join("twitch"),
                                );
                                for src in &["bttv", "ffz", "7tv"] {
                                    emotes += prop_read_manifest_count(
                                        &pdir.join("emotes").join(format!("{src}.json")),
                                    );
                                }
                                ui.label(if emotes > 0 {
                                    emotes.to_string()
                                } else {
                                    "—".into()
                                });
                                ui.label(fmt_asset_stamp(&pdir));
                                ui.end_row();
                            }
                        });
                }

                // Apply an icon-source change picked in the combo above.
                if let Some(newp) = pref_change {
                    if let Err(e) =
                        self.core.store.set_channel_preferred_platform(ch.id, newp)
                    {
                        self.status = format!("Error: {e}");
                    } else {
                        // Reload so the avatar + container row reflect the choice.
                        self.channel_icons.remove(&ch.id);
                        self.reload_rows();
                    }
                }
                });
            },
        );

        if !open {
            self.properties_popup = None;
        }
    }
}

// ── Properties window helpers ────────────────────────────────────────────────

/// Per-platform channel asset directory: `…/channel_assets/{name}/{platform}/`.
/// One container can hold the same creator on several platforms, each with its
/// own icon/banner/badges/emotes — so assets are namespaced by platform.
fn channel_asset_dir(name: &str, platform: Platform) -> std::path::PathBuf {
    crate::app_paths::asset_cache_dir()
        .join("channel_assets")
        .join(crate::downloader::sanitize_filename(name))
        .join(platform.as_str())
}

/// Resolve a container's avatar — the chosen-platform profile pic. An explicit
/// `preferred_platform` wins when it's one of the container's instance platforms
/// (showing a placeholder until that platform's icon is fetched); otherwise auto:
/// the first instance-platform (first-seen order) whose icon loads. `None` when
/// no platform has a fetched icon yet.
fn resolve_channel_icon(
    channel: &Channel,
    platforms: &[Platform],
    ctx: &egui::Context,
) -> Option<egui::TextureHandle> {
    let key = channel.id.to_string();
    let load =
        |p: Platform| load_channel_icon(&channel_asset_dir(&channel.name, p), ctx, &key);
    if let Some(p) = channel.preferred_platform {
        if platforms.contains(&p) {
            return load(p);
        }
    }
    platforms.iter().copied().find_map(load).or_else(|| {
        // Legacy fallback (auto mode only): assets fetched before per-platform
        // namespacing lived in the flat channel_assets/{name}/ dir. Show that
        // icon until a per-platform refetch repopulates the namespaced dir, so an
        // existing container's avatar doesn't go blank on upgrade.
        let flat = crate::app_paths::asset_cache_dir()
            .join("channel_assets")
            .join(crate::downloader::sanitize_filename(&channel.name));
        load_channel_icon(&flat, ctx, &key)
    })
}

/// Count archived historical variants of a singular asset (`history/{stem}_*`).
/// These are the older profile pics / banners kept when the channel changed them.
fn prop_variant_count(asset_dir: &std::path::Path, stem: &str) -> usize {
    let prefix = format!("{stem}_");
    std::fs::read_dir(asset_dir.join("history"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                .count()
        })
        .unwrap_or(0)
}

/// One asset-status cell: `—` (absent), `✔` (present), or `✔ +N` when `N` older
/// versions are archived. Hovering a `+N` cell explains the kept history.
fn asset_status_cell(ui: &mut egui::Ui, present: bool, variants: usize) {
    let text = if !present {
        "—".to_string()
    } else if variants > 0 {
        format!("✔ +{variants}")
    } else {
        "✔".to_string()
    };
    let resp = ui.label(text);
    if variants > 0 {
        resp.on_hover_text(format!(
            "{variants} older version(s) archived under history/ — kept, never overwritten."
        ));
    }
}

/// Short "last fetched" label for an asset dir's `.assets_fetched_at` stamp.
/// Returns `"never"` when the stamp is missing/unparseable.
fn fmt_asset_stamp(asset_dir: &std::path::Path) -> String {
    std::fs::read_to_string(asset_dir.join(".assets_fetched_at"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "never".into())
}

/// Load a channel icon from `asset_dir/icon.*` into an egui texture.
/// Returns `None` when no icon file is found or decoding fails.
fn load_channel_icon(
    asset_dir: &std::path::Path,
    ctx: &egui::Context,
    key: &str,
) -> Option<egui::TextureHandle> {
    let entry = prop_find_first(asset_dir, "icon.")?;
    let bytes = std::fs::read(&entry).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied(size, &img.into_raw());
    Some(ctx.load_texture(
        format!("chan_icon_{key}"),
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}

/// Try to extract a YouTube UC… channel ID from a channel URL.
fn extract_yt_channel_id(url: &str) -> Option<String> {
    // Matches "/channel/UCxxxxxxxxx" style URLs.
    let idx = url.find("/channel/")?;
    let after = &url[idx + "/channel/".len()..];
    let id: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect();
    if id.starts_with("UC") && id.len() > 10 {
        Some(id)
    } else {
        None
    }
}

/// Find the first entry in `dir` whose filename starts with `prefix`.
fn prop_find_first(dir: &std::path::Path, prefix: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
}

/// Count files (non-recursive) in `dir`.
fn prop_count_dir_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .count()
}

/// Count directories at exactly `depth` levels below `root`.
fn prop_count_nested_dirs(root: &std::path::Path, depth: usize) -> usize {
    if depth == 0 || !root.is_dir() {
        return 0;
    }
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            if depth == 1 {
                if e.path().is_dir() { 1 } else { 0 }
            } else {
                prop_count_nested_dirs(&e.path(), depth - 1)
            }
        })
        .sum()
}

/// "yes" / "no" display for boolean fields.
fn prop_bool(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

/// Truncate a long path string keeping its tail (for compact display).
fn prop_truncate_path(p: &str, max_chars: usize) -> String {
    if p.len() <= max_chars {
        p.to_string()
    } else {
        format!("…{}", &p[p.len() - max_chars..])
    }
}

/// Read the length of a JSON-array manifest file (BTTV/FFZ/7TV emote manifests).
/// Returns 0 if the file is absent or unparseable.
fn prop_read_manifest_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_array().map(|a| a.len()))
        .unwrap_or(0)
}

// ── Chat viewer helpers ──────────────────────────────────────────────────────

/// Derive the chat sidecar path from a recording's output path.
/// Locate a recording's chat sidecar. yt-dlp's `live_chat` writer **appends** to the
/// `-o` value (keeping the video extension), so the YouTube sidecar is
/// `<output_path>.live_chat.json` (e.g. `clip.mkv.live_chat.json`) — not a simple
/// extension swap. The Twitch native logger instead **replaces** the extension
/// (`clip.chat.jsonl`). We try both forms, plus the legacy pre-`.cache` YouTube name
/// (`clip.ts.live_chat.json`).
fn chat_file_for_recording(rec: &Recording) -> Option<std::path::PathBuf> {
    let base = Path::new(&rec.output_path);
    let candidates = [
        // YouTube (yt-dlp append form): `<output_path>.live_chat.json`.
        std::path::PathBuf::from(format!("{}.live_chat.json", rec.output_path)),
        // Twitch native logger (extension replace): `<stem>.chat.jsonl`.
        base.with_extension("chat.jsonl"),
        // Extension-replace live_chat form, just in case.
        base.with_extension("live_chat.json"),
        // Legacy pre-`.cache` YouTube name: `<stem>.ts.live_chat.json`.
        base.with_extension("ts.live_chat.json"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

fn fmt_recording_label(rec: &Recording) -> String {
    let dt = chrono::DateTime::from_timestamp(rec.started_at, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| rec.started_at.to_string());
    format!("{dt} ({})", rec.status)
}

fn fmt_chat_ts(secs: f64) -> String {
    if secs < 0.0 {
        return format!("-{}", fmt_chat_ts(-secs));
    }
    let s = secs as u64;
    format!("[{:02}:{:02}:{:02}]", s / 3600, (s % 3600) / 60, s % 60)
}

fn render_chat_message(ui: &mut egui::Ui, msg: &ChatMessage) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        // Timestamp — muted monospace
        ui.label(
            egui::RichText::new(fmt_chat_ts(msg.timestamp_secs))
                .monospace()
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        // Badges
        for badge in &msg.badges {
            let (sym, color) = badge_display(badge, &msg.platform);
            ui.label(egui::RichText::new(sym).small().color(color));
        }
        // Username — bold, platform/user colour
        let name_color = chat_username_color(msg);
        ui.label(
            egui::RichText::new(format!("{}:", msg.author))
                .strong()
                .color(name_color),
        );
        // Message text
        ui.label(&msg.text);
    });
}

fn badge_display(badge: &str, platform: &ChatPlatform) -> (&'static str, egui::Color32) {
    match platform {
        ChatPlatform::Twitch => {
            let name = badge.split('/').next().unwrap_or(badge);
            match name {
                "broadcaster" => ("📡", egui::Color32::from_rgb(0xe9, 0x1e, 0x63)),
                "moderator" | "mod" => ("⚔", egui::Color32::from_rgb(0x00, 0xad, 0x03)),
                "subscriber" => ("★", egui::Color32::from_rgb(0x96, 0x4b, 0xff)),
                "bits" => ("💎", egui::Color32::from_rgb(0x00, 0xc7, 0xac)),
                "premium" => ("👑", egui::Color32::from_rgb(0xff, 0xd7, 0x00)),
                "partner" => ("✓", egui::Color32::from_rgb(0x97, 0x45, 0xff)),
                _ => ("•", egui::Color32::GRAY),
            }
        }
        ChatPlatform::YouTube => {
            let lower = badge.to_lowercase();
            if lower.contains("member") {
                ("⭐", egui::Color32::from_rgb(0xff, 0xd7, 0x00))
            } else if lower.contains("moderator") {
                ("⚔", egui::Color32::from_rgb(0x00, 0xad, 0x03))
            } else if lower.contains("verified") || lower.contains("owner") {
                ("✓", egui::Color32::from_rgb(0x4a, 0xc2, 0xff))
            } else {
                ("•", egui::Color32::GRAY)
            }
        }
    }
}

fn chat_username_color(msg: &ChatMessage) -> egui::Color32 {
    if let Some(c) = msg.color_override {
        return c;
    }
    match msg.platform {
        ChatPlatform::YouTube => egui::Color32::from_rgb(0x4a, 0xc2, 0xff),
        ChatPlatform::Twitch => twitch_username_color(&msg.author),
    }
}

fn twitch_username_color(name: &str) -> egui::Color32 {
    const DEFAULTS: &[&str] = &[
        "#FF0000", "#0000FF", "#00FF00", "#B22222", "#FF7F50", "#9ACD32", "#FF4500", "#2E8B57",
        "#DAA520", "#D2691E", "#5F9EA0", "#1E90FF", "#FF69B4", "#8A2BE2", "#00FF7F",
    ];
    let b = name.as_bytes();
    if b.is_empty() {
        return egui::Color32::WHITE;
    }
    let n = (b[0] as usize + b[b.len() - 1] as usize) % DEFAULTS.len();
    parse_chat_hex_color(DEFAULTS[n]).unwrap_or(egui::Color32::WHITE)
}

fn parse_chat_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

fn parse_chat_file(path: &Path, start_unix_secs: i64) -> anyhow::Result<Vec<ChatMessage>> {
    let s = path.to_string_lossy();
    if s.ends_with("live_chat.json") {
        parse_yt_live_chat(path)
    } else {
        parse_twitch_chat(path, start_unix_secs)
    }
}

fn parse_yt_live_chat(path: &Path) -> anyhow::Result<Vec<ChatMessage>> {
    let mut out = Vec::new();
    let reader = BufReader::new(File::open(path)?);
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(replay) = v.get("replayChatItemAction") {
            // VOD replay format: replayChatItemAction.{videoOffsetTimeMsec, actions[]}
            let offset_ms = replay
                .get("videoOffsetTimeMsec")
                .and_then(|x| x.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| x.as_i64()));
            if let Some(actions) = replay.get("actions").and_then(|a| a.as_array()) {
                for action in actions {
                    if let Some(msg) = yt_action_to_msg(action, offset_ms) {
                        out.push(msg);
                    }
                }
            }
        } else if let Some(msg) = yt_action_to_msg(&v, None) {
            // Live format: addChatItemAction directly at the top level of each line.
            out.push(msg);
        }
    }
    Ok(out)
}

fn yt_action_to_msg(action: &serde_json::Value, offset_ms: Option<i64>) -> Option<ChatMessage> {
    let r = action.pointer("/addChatItemAction/item/liveChatTextMessageRenderer")?;
    let ts_secs = if let Some(ms) = offset_ms {
        ms as f64 / 1000.0
    } else {
        r["timestampUsec"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1_000_000.0
    };
    let author = r
        .pointer("/authorName/simpleText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let text = r["message"]["runs"]
        .as_array()
        .map(|runs| {
            runs.iter()
                .map(|run| {
                    if let Some(t) = run["text"].as_str() {
                        t.to_string()
                    } else if let Some(emoji) = run.get("emoji") {
                        emoji["shortcuts"]
                            .as_array()
                            .and_then(|s| s.first())
                            .and_then(|e| e.as_str())
                            .or_else(|| emoji["emojiId"].as_str())
                            .unwrap_or("[emoji]")
                            .to_string()
                    } else {
                        String::new()
                    }
                })
                .collect::<String>()
        })
        .unwrap_or_default();
    let badges: Vec<String> = r["authorBadges"]
        .as_array()
        .map(|bs| {
            bs.iter()
                .filter_map(|b| {
                    b.pointer("/liveChatAuthorBadgeRenderer/tooltip")
                        .and_then(|t| t.as_str())
                        .map(|t| t.split('(').next().unwrap_or(t).trim().to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ChatMessage {
        timestamp_secs: ts_secs,
        author,
        text,
        badges,
        color_override: None,
        platform: ChatPlatform::YouTube,
    })
}

fn parse_twitch_chat(path: &Path, start_unix_secs: i64) -> anyhow::Result<Vec<ChatMessage>> {
    let start_ms = start_unix_secs as f64 * 1000.0;
    let mut out = Vec::new();
    let reader = BufReader::new(File::open(path)?);
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts_ms = v["ts"].as_f64().unwrap_or(0.0);
        let author = v["name"]
            .as_str()
            .or_else(|| v["login"].as_str())
            .unwrap_or("")
            .to_string();
        let text = v["text"].as_str().unwrap_or("").to_string();
        let color_override = v["color"].as_str().and_then(parse_chat_hex_color);
        // Split raw badge tag "subscriber/12,moderator/1" into one entry per badge.
        let badges = v["badges"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default();
        out.push(ChatMessage {
            timestamp_secs: (ts_ms - start_ms) / 1000.0,
            author,
            text,
            badges,
            color_override,
            platform: ChatPlatform::Twitch,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        DateFmt, StreamMetaChange, active_date_fmt, aggregate_stream_changes,
        chat_file_for_recording, compose_browser_profile, fmt_polled, meta_change_lines,
        set_active_date_fmt, split_browser_profile,
    };

    fn rec_with_output(path: &str) -> crate::models::Recording {
        crate::models::Recording {
            id: 1,
            monitor_id: 1,
            started_at: 0,
            ended_at: None,
            status: "recording".into(),
            bytes: 0,
            exit_code: None,
            output_path: path.into(),
            went_live_at: None,
            went_live_approx: false,
            lost_secs: None,
            stream_id: None,
            take_group: None,
            ad_count: 0,
            ad_secs: 0,
            meta_change_count: 0,
            title: String::new(),
            category: String::new(),
            log_excerpt: String::new(),
        }
    }

    #[test]
    fn finds_youtube_live_chat_append_form() {
        // yt-dlp appends `.live_chat.json` to the -o value, so the sidecar keeps the
        // video extension: `<output_path>.live_chat.json`.
        let dir = std::env::temp_dir().join(format!("sa-chat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("clip.mkv");
        std::fs::write(format!("{}.live_chat.json", out.to_string_lossy()), "{}").unwrap();

        let found = chat_file_for_recording(&rec_with_output(&out.to_string_lossy()));
        assert_eq!(found.as_deref(), Some(out.with_extension("mkv.live_chat.json").as_path()));

        // Twitch native logger uses the extension-replace form.
        let tout = dir.join("vod.mkv");
        std::fs::write(tout.with_extension("chat.jsonl"), "{}").unwrap();
        let tfound = chat_file_for_recording(&rec_with_output(&tout.to_string_lossy()));
        assert_eq!(tfound.as_deref(), Some(tout.with_extension("chat.jsonl").as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn actions_col_index_is_actions() {
        // The optional-Actions logic skips this index in the builder/header/rows;
        // guard that it actually points at the Actions column.
        assert_eq!(
            super::STREAM_COLUMNS[super::STREAM_ACTIONS_COL].title,
            "Actions"
        );
    }

    #[test]
    fn stream_meta_aggregation_dedups_rebaseline() {
        let smc = |id, at, old: &str, new: &str| StreamMetaChange {
            id,
            recording_id: 0,
            at_secs: at,
            kind: "title".into(),
            old_value: old.into(),
            new_value: new.into(),
        };
        // Take 1 (started 1000): initial "A", then A -> B at +300s.
        let t1 = vec![smc(1, 0, "", "A"), smc(2, 300, "A", "B")];
        // Take 2 (started 2000): re-observes "B" (the duplicate), then B -> C at +120s.
        let t2 = vec![smc(3, 0, "", "B"), smc(4, 120, "B", "C")];

        let agg = aggregate_stream_changes(&[(1000, t1), (2000, t2)]);
        // All rows kept, offsets rebased onto the stream timeline (min start 1000)
        // and sorted: 0, 300, (2000-1000)+0=1000, (2000-1000)+120=1120.
        assert_eq!(
            agg.iter().map(|c| c.at_secs).collect::<Vec<_>>(),
            vec![0, 300, 1000, 1120]
        );
        // The displayed list drops both initial values — including take 2's
        // re-baseline of "B" (the omitted duplicate) — and keeps the real changes.
        let lines = meta_change_lines(&agg);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("A → B"), "{:?}", lines[0]);
        assert!(lines[1].contains("B → C"), "{:?}", lines[1]);
    }

    #[test]
    fn date_fmt_parse_roundtrip() {
        for f in DateFmt::ALL {
            assert_eq!(DateFmt::parse(f.as_str()), f);
        }
        // Unknown / empty falls back to the ISO default.
        assert_eq!(DateFmt::parse("bogus"), DateFmt::Iso);
        assert_eq!(DateFmt::parse(""), DateFmt::Iso);
    }

    #[test]
    fn active_date_fmt_roundtrip() {
        for f in DateFmt::ALL {
            set_active_date_fmt(f);
            assert_eq!(active_date_fmt(), f);
        }
        set_active_date_fmt(DateFmt::Iso); // restore default for other tests
    }

    #[test]
    fn fmt_polled_shows_interval() {
        // Never polled -> just the interval, so the cadence is still visible.
        assert_eq!(fmt_polled(None, 60), "(60s)");
        assert_eq!(fmt_polled(Some(0), 30), "(30s)");
        // Polled -> "<timestamp> (Ns)"; the timestamp is local/tz-dependent, so
        // assert only the stable suffix and that a timestamp is present.
        let s = fmt_polled(Some(1_700_000_000), 45);
        assert!(s.ends_with(" (45s)"), "got {s:?}");
        assert!(s.len() > " (45s)".len());
    }

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
