//! The on-demand egui window: channel table, add/edit form, and settings.
//!
//! Runs reactive (repaints only on input/events). The tray thread wakes it via
//! `Context::request_repaint`. Closing the window hides it to the tray; the
//! tray "Quit" item triggers a real close.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use tracing::{debug, warn};
use tray_icon::TrayIcon;

use crate::app_core::AppCore;
use crate::events::{ManualCommand, UiCommand};
use crate::models::{
    AdBreak, AuthKind, Channel, Container, DetectionMethod, DownloadDefaults, GlobalStats,
    K_DIALOG_ICON, K_DISCORD_SCHEDULE, K_DISCORD_TOKEN, K_FILENAME_MEDIA, K_MONITOR_DEFAULTS,
    K_OCR_COMMAND, K_OCR_EFFORT, K_OCR_FALLBACK_MODEL, K_OCR_MAX_BUDGET, K_OCR_MODEL,
    K_OCR_OFFSET, K_OCR_STATS, K_OCR_TIMEOUT_SECS, K_OCR_TIMEZONE, K_SCHEDULE_TITLE_FILL,
    K_YT_API_DETECT, K_YT_API_SCHEDULE, K_YT_COMMUNITY_MAX_POSTS, K_YT_API_QUOTA_CUTOFF, K_YT_SEARCH_QUOTA_CUTOFF,
    K_REMUX_EMBED_THUMBNAIL, K_REMUX_EMBED_TITLE, K_REMUX_TITLE_TEMPLATE, K_REMUX_EMBED_SUBS,
    K_FILE_SPLIT_ENABLED, K_FILE_SPLIT_VIDEOS, K_FILE_SPLIT_SUBS, K_FILE_SPLIT_CHAT,
    K_FILE_SPLIT_THUMBS, K_FILE_SPLIT_LOGS,
    MediaInfoMode, Monitor, MonitorDefaults, MonitorWithChannel, OcrStats, Platform, PollStats, RecurrenceKind,
    Recording, SabrCodecPref, ScheduleSegment, ScheduledRecording, ScheduledRecordingWithNames,
    StreamGroup, StreamMetaChange, Tool, UpcomingStream, Video, group_recordings, now_unix,
};
use crate::google_oauth;
use crate::grid_columns::{self, ColumnEntry, GridCol, GridState, GridTableId};
use crate::imports::{self, ImportCandidate};
use crate::inspector::Inspectable;
use crate::oauth::{self, AuthFlow};
use crate::platform::AutoStart;
use crate::schedule_source::{
    ScheduleSourceKind, SourceEntry, load_channel_cfg, load_channel_scope, load_monitor_scope,
    load_source_order, save_channel_cfg, save_channel_scope, save_monitor_scope, save_source_order,
    source_badge,
};

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
/// Experimental: append `enable_live_deep_rewind=true` to the SABR extractor-args
/// (rewinds past the normal DVR window; dev-build-only). Absent ⇒ off.
const K_SABR_DEEP_REWIND: &str = "ytdlp_sabr_deep_rewind";
/// DASH-companion format selector for dual capture.
const K_DASH_FORMAT: &str = "ytdlp_dash_format";
/// GLOBAL default SABR video codec/quality preference (a [`SabrCodecPref`] id,
/// e.g. `auto`/`best`/`h264`). Per-monitor `Inherit` falls through to this.
const K_SABR_CODEC_PREF: &str = "ytdlp_sabr_codec_pref";
/// GLOBAL raw `-S` string used when `K_SABR_CODEC_PREF == custom`.
const K_SABR_CODEC_CUSTOM: &str = "ytdlp_sabr_codec_custom";
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
/// Whether timestamp columns use the compact short format (off = full datetime).
const K_SHORT_TIMESTAMPS: &str = "short_timestamps";
/// chrono format pattern used for the compact timestamp display; default `%d/%m %H:%M`.
const K_SHORT_TS_FMT: &str = "short_ts_fmt";
/// Last-selected Settings category tab (restored on launch).
const K_SETTINGS_TAB: &str = "settings_tab";
/// Whether chat-replay emote codes render as inline images (off ⇒ show the code
/// text). Default on; only an explicit `"0"` disables. Needs "Fetch chat assets".
const K_RENDER_EMOTES: &str = "render_emotes_in_chat";
/// Whether animated emotes play (off ⇒ a static first frame). Default on; only an
/// explicit `"0"` disables. Off is the perf/RAM escape hatch for heavy channels.
const K_ANIMATE_EMOTES: &str = "animate_emotes_in_chat";
/// Path to the media player binary used by "Stream in player" on recording rows.
const K_MEDIA_PLAYER: &str = "media_player_path";

/// Browsers yt-dlp can read cookies from (for the Settings dropdown).
const COOKIE_BROWSERS: [&str; 8] = [
    "firefox", "chrome", "chromium", "edge", "brave", "opera", "vivaldi", "safari",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Streams,
    Videos,
    Schedule,
    Posts,
    Background,
    Files,
    Settings,
    Stats,
    IoMonitor,
    Debug,
}

mod app;
mod assets_helpers;
mod background;
mod calendar;
mod chat;
mod debug;
mod dialogs;
mod files;
mod format;
mod grid;
mod io_view;
mod issues;
mod player;
mod posts;
mod properties;
mod schedule;
mod settings;
mod streams;
mod videos;

#[allow(unused_imports)]
use {app::*, assets_helpers::*, background::*, calendar::*, chat::*, debug::*, dialogs::*, files::*, format::*, grid::*, io_view::*, issues::*, player::*, posts::*, properties::*, schedule::*, settings::*, streams::*, videos::*};

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
    /// Auto-record toggle (the "Auto" column) — disk recording only.
    enabled: bool,
    /// Master automation toggle (the "Enabled" column) — off = fully dormant.
    automation_enabled: bool,
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
    /// Use the stream thumbnail (when fetched) as the hero image in the
    /// recording-started notification instead of the channel's static banner.
    thumbnail_in_toast: bool,
    /// Download channel icon, banner, badges, and emotes (Twitch: BTTV/FFZ/7TV too)
    /// into channel_assets/ alongside recordings.
    fetch_chat_assets: bool,
    extra_args: String,
    /// YouTube SABR video codec/quality preference (Inherit ⇒ follow the global
    /// default) + its raw `-S` sort when `Custom`.
    sabr_codec_pref: SabrCodecPref,
    sabr_codec_custom: String,
    /// Platform the tool/detection defaults were last set for; a URL change to a
    /// different platform re-applies that platform's defaults.
    last_platform: Option<Platform>,
    /// Post-stream VOD-download overrides for this instance (`None` = inherit the
    /// channel/global default). Loaded from / saved to the monitor scope map.
    vod_download: Option<bool>,
    vod_replace: Option<bool>,
    /// Head-backfill-on-new-take overrides for this instance (`None` = inherit
    /// the channel/global default). Loaded from / saved to the monitor scope map.
    head_backfill_fetch: Option<bool>,
    head_backfill_replace: Option<bool>,
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
            automation_enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            // New monitors default to max-archival: every audio + subtitle track,
            // chat logging, thumbnails, and channel assets all on.
            audio_tracks: "all".into(),
            subtitle_tracks: "all".into(),
            chat_log: true,
            fetch_thumbnail: true,
            thumbnail_in_toast: false,
            fetch_chat_assets: true,
            extra_args: String::new(),
            sabr_codec_pref: SabrCodecPref::Inherit,
            sabr_codec_custom: String::new(),
            last_platform: None,
            vod_download: None,
            vod_replace: None,
            head_backfill_fetch: None,
            head_backfill_replace: None,
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
            automation_enabled: m.automation_enabled,
            auth_kind: m.auth_kind,
            auth_value: m.auth_value.clone(),
            audio_tracks: m.audio_tracks.clone(),
            subtitle_tracks: m.subtitle_tracks.clone(),
            chat_log: m.chat_log,
            fetch_thumbnail: m.fetch_thumbnail,
            thumbnail_in_toast: m.thumbnail_in_toast,
            fetch_chat_assets: m.fetch_chat_assets,
            extra_args: m.extra_args.clone(),
            sabr_codec_pref: m.sabr_codec_pref,
            sabr_codec_custom: m.sabr_codec_custom.clone(),
            // Don't override the saved tool/detection just because the form opened.
            last_platform: Some(m.platform()),
            // Overridden by the caller from the monitor scope map (needs the store).
            vod_download: None,
            vod_replace: None,
            head_backfill_fetch: None,
            head_backfill_replace: None,
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
            automation_enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            // New monitors default to max-archival: every audio + subtitle track,
            // chat logging, thumbnails, and channel assets all on.
            audio_tracks: "all".into(),
            subtitle_tracks: "all".into(),
            chat_log: true,
            fetch_thumbnail: true,
            thumbnail_in_toast: false,
            fetch_chat_assets: true,
            extra_args: String::new(),
            sabr_codec_pref: SabrCodecPref::Inherit,
            sabr_codec_custom: String::new(),
            last_platform: None,
            vod_download: None,
            vod_replace: None,
            head_backfill_fetch: None,
            head_backfill_replace: None,
        }
    }
}

/// A three-state **Inherit / On / Off** dropdown for an `Option<bool>` override
/// (`None` = inherit the level above). Returns the combo's response for hovers.
fn tristate_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<bool>) -> egui::Response {
    let text = match value {
        None => "Inherit",
        Some(true) => "On",
        Some(false) => "Off",
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(text)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Inherit");
            ui.selectable_value(value, Some(true), "On");
            ui.selectable_value(value, Some(false), "Off");
        })
        .response
}

/// Backing state for the always-visible "download a video" form on the Videos tab.
///
/// Fields are pre-filled from the detected platform's saved defaults whenever the
/// platform changes; the user can override any of them per download.
struct VideoForm {
    url: String,
    title: String,
    tool: Tool,
    /// See [`Video::tool_binary`]. Reset alongside `tool` on a platform change.
    tool_binary: String,
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
            tool_binary: String::new(),
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
    /// Google OAuth client (TV/device type) for "Connect YouTube" → subscriptions
    /// import. Separate from the YouTube Data API key (which can't read user data).
    google_client_id: String,
    google_client_secret: String,
    youtube_api_key: String,
    /// Per-operation opt-ins to use the YouTube Data API key instead of scraping
    /// (each costs quota — see the Settings section).
    youtube_api_detect: bool,
    youtube_api_schedule: bool,
    /// Daily quota cutoff for the YouTube Data API (units). Empty = default (9000).
    youtube_api_quota_cutoff: String,
    /// Daily search.list query cutoff (queries). Empty = default (90).
    youtube_search_quota_cutoff: String,
    kick_client_id: String,
    kick_client_secret: String,
    default_output_dir: String,
    max_concurrent_downloads: String,
    /// VOD/video download rate limit (yt-dlp `--limit-rate` syntax, e.g. `4M`);
    /// empty = unlimited (the default). Never applied to live captures.
    download_rate_limit: String,
    capture_cache_root: String,
    /// yt-dlp `--postprocessor-args` specs (`;;`-separated); empty = none.
    /// Throttles yt-dlp's internal ffmpeg passes (e.g. the SABR merge).
    ytdlp_ppa: String,
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
    /// chrono format string used for the compact timestamp mode (K_SHORT_TS_FMT).
    short_ts_fmt: String,
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
    /// Experimental deep-rewind toggle (appends enable_live_deep_rewind=true).
    sabr_deep_rewind: bool,
    /// Manual raw SABR args; non-empty overrides the format+extractor-args preset.
    sabr_raw_args: String,
    /// PO-token-provider `--extractor-args` (e.g. bgutil) for the SABR command.
    sabr_pot_args: String,
    /// GLOBAL default video codec/quality preference + its raw `-S` (when Custom).
    sabr_codec_pref: SabrCodecPref,
    sabr_codec_custom: String,
    /// DASH-companion format selector used by dual (SABR+DASH) capture.
    dash_format: String,
    /// Discord user token + whether to import stream schedules from Discord events
    /// (opt-in; automating a user token is against Discord's ToS).
    discord_token: String,
    discord_schedule: bool,
    /// Image→schedule OCR pipeline (shells out to an LLM CLI). `ocr_command` is the
    /// executable; `ocr_model`/`ocr_fallback_model` the primary + retry models;
    /// `ocr_timezone`/`ocr_offset` the timezone/UTC offset to assume for banner
    /// times. Empty fields fall back to the built-in defaults.
    ocr_command: String,
    ocr_model: String,
    ocr_fallback_model: String,
    ocr_timezone: String,
    ocr_offset: String,
    /// Per-call USD budget cap passed as `--max-budget-usd` (empty = no cap).
    ocr_max_budget: String,
    /// Process timeout in seconds (empty/0 = default 150 s).
    ocr_timeout_secs: String,
    /// Effort level passed as `--effort` (empty = omit; low/medium/high/xhigh/max).
    ocr_effort: String,
    /// File path to a PNG used as the main icon in crash and freeze dialogs.
    /// Empty = standard Windows error/warning icon. Requires a restart to take effect.
    dialog_icon: String,
    /// Global "go to the next schedule source when an event has no title" toggle:
    /// after a winner is found, keep querying lower-priority sources to fill in
    /// blank titles (e.g. a Twitch schedule with times but no titles).
    schedule_title_fill: bool,
    /// How many recent YouTube community posts to scan for a schedule image
    /// (backlog depth). Empty = built-in default (5). Per-channel override in
    /// channel Properties.
    youtube_community_max_posts: String,
    // --- Remux embedding options ---
    /// Embed the thumbnail sidecar as MKV cover art on remux.
    remux_embed_thumbnail: bool,
    /// Embed a title metadata tag in the MKV on remux.
    remux_embed_title: bool,
    /// Template used to generate the MKV title tag.
    remux_title_template: String,
    /// Embed subtitle sidecar files as MKV subtitle streams on remux.
    remux_embed_subs: bool,
    /// Post-processing disk throttle: ffmpeg `-readrate` multiplier for
    /// finalize remuxes/concats/embeds (0 = unthrottled).
    postproc_readrate: f64,
    /// Write 1 s I/O-monitor samples to a JSONL log under the appdata logs
    /// dir (default on — post-mortems need the data to already exist).
    iomon_sample_log: bool,
    // --- File management ---
    /// Split output files into per-type subdirectories.
    file_split_enabled: bool,
    file_split_videos: String,
    file_split_subs: String,
    file_split_chat: String,
    file_split_thumbs: String,
    file_split_logs: String,
    /// Checkbox for "fetch missing thumbnails" in the Maintenance section.
    fetch_thumb_embed: bool,
    /// Selected preset template for the "Set filename default" row in Maintenance.
    maintenance_filename_preset: String,
    /// Apply the preset to all existing monitors when "Set as Default" is clicked.
    maintenance_apply_all: bool,
    /// Path to the media player binary (e.g. `C:\Progs\mpv\mpv.exe`).
    media_player_path: String,
    // --- Twitch VOD recovery ---
    /// Auto-recover a Twitch VOD when the VOD checker finds it DMCA-muted.
    auto_recover_muted: bool,
    /// Auto-recover a Twitch VOD when the VOD checker finds it was never published.
    auto_recover_deleted: bool,
    /// Newline/comma CDN host override (empty = built-in list).
    recovery_cdn_hosts: String,
    /// Default recovery quality (empty/`chunked` = source, else e.g. `720p60`).
    recovery_quality: String,
    /// Concurrent-HEAD cap for the CDN probes (empty = default 8).
    recovery_max_conc: String,
    // --- Post-stream VOD download (global defaults for the 3-level chain) ---
    /// Download the platform's published VOD after a stream ends (alongside).
    vod_dl_enabled: bool,
    /// Replace the live recording with the VOD when the download succeeds.
    vod_dl_replace: bool,
    // --- Head backfill on new takes (global defaults for the 3-level chain) ---
    /// Fetch a fresh, full head backfill for a later take (a reconnect
    /// mid-broadcast), not just the stream's first take. Default on.
    head_backfill_fetch_new_take: bool,
    /// Restart a young Twitch `best` capture when a better rendition appears
    /// after join (Twitch lists the source quality late). Default on.
    quality_upgrade_restart: bool,
    /// Once a fresh head passes its integrity checks, delete older takes'
    /// now-redundant head files for the same stream. Default on.
    head_backfill_replace_old: bool,
    /// Global trigger-word rules (start recording on title/game match even with
    /// Auto off). Channel/instance Properties can extend/replace/disable them.
    trigger_rules: Vec<crate::triggers::TriggerRule>,
    /// Global blacklist trigger rules (PREVENT automatic recording on title/game
    /// match; manual Start still records). Same scope inheritance as above.
    trigger_block_rules: Vec<crate::triggers::TriggerRule>,
    /// User-defined alternate yt-dlp-compatible binaries (alias + path),
    /// selectable alongside the system yt-dlp / SABR build in the Videos-tab
    /// download form.
    custom_tools: Vec<crate::downloader::CustomTool>,
    /// Default concurrent local full-file ffmpeg passes per disk (min 1).
    disk_default_local: u32,
    /// Default concurrent CDN-fed muxes per disk (min 1).
    disk_default_cdn: u32,
    /// Per-drive I/O limit overrides: (drive letter, limits). The default
    /// readrate/rate-limit live in `postproc_readrate`/`download_rate_limit`.
    disk_overrides: Vec<(String, crate::io_gate::DiskLimits)>,
}
/// Lazy cache of decoded post images, keyed by content hash: an egui texture +
/// its pixel dimensions, or `None` when the decode failed.
type PostImageCache = HashMap<String, Option<(egui::TextureHandle, (u32, u32))>>;

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
    /// In-flight background load of the process list (spawned off the UI thread
    /// to avoid blocking on the store mutex during `list_processes()`).
    processes_load: Option<std::sync::mpsc::Receiver<Vec<crate::app_core::ProcInfo>>>,
    /// The issues panel: whether it's open, its last snapshot of recordings
    /// that still have a `.ts` path, and when that snapshot was taken.
    show_issues: bool,
    issues_recs: Vec<crate::models::Recording>,
    issues_missing: Vec<crate::models::Recording>,
    /// Failed/aborted/orphaned recordings that have an output file on disk (or no path at all).
    issues_errors: Vec<crate::models::Recording>,
    /// Failed/aborted/orphaned recordings whose output path is set but the file is gone from disk.
    /// Partitioned out of issues_errors at load time; rendered alongside the missing-file section.
    issues_errors_no_file: Vec<crate::models::Recording>,
    /// Completed recordings whose promote-to-output-dir move never finished (a
    /// non-`.ts` file, e.g. a SABR/DASH `.mkv`, still sitting in `.cache\`) —
    /// most commonly because the filename overflowed the filesystem's length
    /// limit. Distinct from issues_recs (a `.ts` awaiting a remux).
    issues_stuck: Vec<crate::models::Recording>,
    /// Recordings whose published VOD came back DMCA-muted (post-stream archive) —
    /// un-muted via recovery, awaiting acknowledgement.
    issues_muted_vod: Vec<crate::models::MutedVodIssue>,
    /// Takes that finalized 0-byte / file-gone but whose media SURVIVED as
    /// split per-format files in `.cache\` (the tool died before its own
    /// merge) — recoverable, so never shown as plain "gone". Each entry
    /// carries the discovered part files.
    issues_unmerged: Vec<(crate::models::Recording, Vec<std::path::PathBuf>)>,
    /// Head backfills that can't be losslessly joined with their live capture
    /// (codec/resolution mismatch), with display strings: (rec, head params,
    /// live params).
    issues_head_mismatch: Vec<(crate::models::Recording, String, String)>,
    /// Rows still marked `recording` whose files have gone quiet (capture died
    /// unnoticed, or the finalize is pending) + seconds since the last write
    /// (`None` = nothing on disk).
    issues_stale_recording: Vec<(crate::models::Recording, Option<i64>)>,
    /// In-flight background Issues scan (see [`IssuesScan`]). Every
    /// `path.exists()`/ffprobe the Issues panel needs runs on this thread —
    /// against the recordings drive a single stat can block for seconds, so
    /// the UI thread must never do one (see `FsProbes`).
    issues_missing_load: Option<std::sync::mpsc::Receiver<IssuesScan>>,
    issues_refreshed: Option<std::time::Instant>,
    /// A dirty-marking app event landed since the last issues sweep — shortens
    /// the closed-panel refresh interval instead of forcing an immediate one.
    issues_dirty: bool,
    issues_confirm_clear: bool,
    /// Error-details window inside the Issues viewport: (title, full text —
    /// the same text the status-column hover shows), opened via the 🔍 row
    /// button.
    issues_error_view: Option<(String, String)>,
    /// The notifications feed window: whether it's open, its last-loaded rows,
    /// the throttle timestamp, an off-thread load, the cached unread count (the
    /// header badge), and the session-only category + text filters.
    show_notifications: bool,
    notifications: Vec<crate::store::NotificationRow>,
    notif_refreshed: Option<std::time::Instant>,
    notif_unread: i64,
    notif_search: String,
    notif_kind_filter: Option<crate::models::NotificationKind>,
    /// The YouTube posts feed (a top-level tab AND a pop-out window sharing one
    /// render fn): loaded rows, load throttle, session-only channel + text
    /// filters, and a lazy visible-only texture cache keyed by content hash.
    show_posts_window: bool,
    posts: Vec<crate::store::CommunityPostRow>,
    posts_refreshed: Option<std::time::Instant>,
    posts_search: String,
    posts_channel_filter: Option<i64>,
    /// Whether the posts feed also shows viewer posts (fans posting in the
    /// channel's Community space). Off by default — session-only, like the
    /// other feed filters.
    posts_show_viewer: bool,
    /// How many of the filtered posts to actually lay out this frame. The feed
    /// can hold up to 500 rows, each a rich multi-widget card (links, N
    /// images) — laying all of them out every frame regardless of scroll
    /// position is the main cost of the tab, so only this many render up
    /// front; a "Show more" button at the bottom raises it. Session-only,
    /// reset to the default whenever the filter/search narrows the visible set.
    posts_render_limit: usize,
    post_img_cache: PostImageCache,
    /// The widget inspector (F12): whether the window is open (session-only,
    /// like the other window flags) and its tab/selection/snapshot state.
    show_inspector: bool,
    inspector: crate::inspector::InspectorState,
    quitting: bool,
    /// UI-freeze watchdog heartbeat: stamped each frame so a background thread can
    /// detect (and surface as a native dialog) a hung UI thread. See [`crate::watchdog`].
    heartbeat: crate::watchdog::Heartbeat,

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
    /// Active Settings category tab (persisted via `K_SETTINGS_TAB`).
    settings_tab: SettingsTab,
    /// Settings search-box query — when non-empty, matching sections across all
    /// categories are shown instead of the selected tab.
    settings_search: String,
    /// Shared state of the async "List formats" probe (Videos tab).
    format_probe: Arc<Mutex<FormatProbe>>,
    /// Backing state for the "Recover VOD" dialog (`None` = closed).
    recover_form: Option<RecoverVodForm>,
    /// Shared state of the async Recover-VOD CDN probe.
    recover_probe: Arc<Mutex<RecoverProbe>>,
    /// Shared state of the async "Parse URL" start-time scrape.
    recover_scrape: Arc<Mutex<RecoverScrape>>,
    settings: SettingsForm,
    status: String,
    /// Monitor id of the currently selected row (target for keyboard shortcuts).
    selected_monitor: Option<i64>,
    /// Pending instance-delete confirmation: (monitor id, channel name).
    confirm_delete: Option<(i64, String)>,
    /// Pending channel-delete confirmation: (channel id, name).
    confirm_delete_channel: Option<(i64, String)>,
    /// Pending schedule-segment-delete confirmation: segment id.
    confirm_delete_segment: Option<i64>,
    /// Backing state for the create/rename-channel dialog.
    channel_form: Option<ChannelForm>,
    /// Scheduled recordings (schema v51): the management window's open flag +
    /// last-loaded rows (refreshed in `reload_rows`, cheap — one small table),
    /// the add/edit dialog (`None` = closed), and a pending delete confirmation.
    show_scheduled_recordings: bool,
    scheduled_recordings: Vec<crate::models::ScheduledRecordingWithNames>,
    scheduled_recording_form: Option<ScheduledRecordingForm>,
    confirm_delete_scheduled_recording: Option<(i64, String)>,
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
    ad_popups: Vec<i64>,
    /// Lazy per-recording title/category change log, keyed by recording id;
    /// cleared on reload. Same caching role as `ad_break_cache`.
    meta_change_cache: HashMap<i64, Vec<StreamMetaChange>>,
    /// What the metadata-change popup shows (None = closed): a single take or a
    /// whole stream's aggregated takes.
    meta_popups: Vec<MetaPopup>,
    /// Lazy per-monitor upcoming-schedule detail, keyed by monitor id; cleared on
    /// reload. Backs the Next stream popup.
    schedule_cache: HashMap<i64, Vec<ScheduleSegment>>,
    /// Monitor id whose upcoming schedule is shown in a popup (None = closed).
    schedule_popups: Vec<i64>,
    /// All upcoming scheduled streams (across every monitor), backing the Schedule
    /// calendar. Loaded lazily on first view + on refresh; see [`Self::spawn_reload_schedule`].
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
    /// Individual segment IDs the user has soft-hidden (not tombstoned). Reset
    /// on app restart; use Delete for permanent suppression.
    schedule_hidden_segments: HashSet<i64>,
    /// When true, soft-hidden segments are shown dimmed instead of filtered out.
    schedule_show_hidden: bool,
    /// Whether to flag overlapping streams (time collisions) in the calendar.
    schedule_collisions: bool,
    /// Font/element zoom for the calendar body only (toolbar + sidebar stay
    /// normal size). 1.0 = 100%; Ctrl+0 resets. Session-only, like `schedule_mode`.
    schedule_zoom: f32,
    /// Per-channel display colour for every Schedule surface (event blocks,
    /// chips, stripes, sidebar legend) — the SAME resolution as the Streams
    /// list (custom colour > fetched Twitch broadcaster colour > palette),
    /// rebuilt each frame the Schedule view renders. Twitch colours are
    /// darkened for white-on-block readability (`block_safe_color`).
    schedule_chan_colors: HashMap<i64, egui::Color32>,
    /// Compact calendar events: collapse each Week/Day event block to a
    /// one-line chip at its start time (quick overview when many streams
    /// overlap). Persisted under [`K_SCHEDULE_COMPACT`].
    schedule_compact: bool,
    /// The day whose full stream list is shown in a popup (local date; None = closed).
    schedule_day_popup: Option<chrono::NaiveDate>,
    /// Whether the "Schedule sources" dialog is open.
    show_schedule_sources: bool,
    /// Editable draft of the ordered source list, shown in the dialog. Loaded from
    /// settings when the dialog opens; saved (+ refresh requested) on every change.
    schedule_sources_draft: Vec<SourceEntry>,
    /// The source id selected in the dialog (drives the →/← / ▲/▼ buttons).
    schedule_sources_selected: Option<String>,
    /// Editable per-channel schedule-source configs shown in the Properties
    /// windows — one draft per open window, keyed by channel id.
    channel_cfg_drafts: HashMap<i64, crate::schedule_source::ChannelSourceConfig>,
    /// Editable per-channel schedule-source *scope* overrides (custom order +
    /// title-fill) shown in channel Properties, keyed by channel id.
    channel_scope_drafts: HashMap<i64, crate::schedule_source::SourceScopeConfig>,
    /// Editable per-instance (monitor) schedule-source *scope* overrides shown
    /// in instance Properties — one draft per open window, keyed by monitor id.
    instance_scope_drafts: HashMap<i64, crate::schedule_source::SourceScopeConfig>,
    /// Per-open channel-Properties trigger-word scope drafts (saved on change).
    channel_trigger_drafts: HashMap<i64, crate::triggers::TriggerScope>,
    /// Per-open instance-Properties trigger-word scope drafts (saved on change).
    instance_trigger_drafts: HashMap<i64, crate::triggers::TriggerScope>,
    /// Per-open channel-Properties BLACKLIST-trigger scope drafts (saved on change).
    channel_block_drafts: HashMap<i64, crate::triggers::TriggerScope>,
    /// Per-open instance-Properties BLACKLIST-trigger scope drafts (saved on change).
    instance_block_drafts: HashMap<i64, crate::triggers::TriggerScope>,
    /// Draft for the "Edit schedule item" dialog (None = closed). Saving converts
    /// the row to a protected `"manual"` source so refreshes don't overwrite it.
    edit_schedule: Option<EditScheduleDraft>,
    /// Segment IDs selected in the schedule calendar (Ctrl+click multi-select).
    schedule_selected: HashSet<i64>,
    /// Open merge-preview dialog (None = closed).
    merge_preview: Option<MergePreviewDraft>,
    /// Pending multi-delete confirmation for schedule segments (None = closed).
    confirm_delete_segments: Option<Vec<i64>>,
    /// Computed from `schedule_all`: primary segment_id → merge badge text.
    /// Built by [`Self::recompute_merge_state`]; drives the 🔀 indicator.
    schedule_merge_labels: HashMap<i64, String>,
    /// Computed from `schedule_all`: segment IDs that are auto-merge secondaries
    /// (hidden in favour of their primary). Built by [`Self::recompute_merge_state`].
    schedule_auto_secondary: HashSet<i64>,
    /// User-defined filename template presets loaded from the DB.
    custom_presets: Vec<(i64, String, String)>,
    /// Open "Save preset" naming dialog (None = closed).
    save_preset_dialog: Option<SavePresetDraft>,
    /// Chat log viewer popup (None = closed).
    /// Open chat windows, one per monitor (each is its own OS viewport).
    chat_popups: Vec<ChatPopup>,
    /// Platform favicons, uploaded to the GPU on first use (None until then).
    platform_tex: Option<PlatformTextures>,
    /// Which monitor's Properties window is open (None = closed).
    properties_popups: Vec<i64>,
    /// Open channel-Properties windows (one per channel).
    channel_properties_popups: Vec<i64>,
    /// Open recording-properties windows, one per take (each carries its own
    /// notes draft, synced from the DB on open and written back per keystroke).
    rec_props_popups: Vec<RecPropsPopup>,
    /// Per-channel cached icon textures loaded from disk for the Properties window.
    /// A `None` value means the lookup was attempted but no icon file was found.
    channel_icons: HashMap<i64, Option<egui::TextureHandle>>,
    /// Pre-scaled (64 px) icon textures for the streams table avatar column.
    /// Separate from `channel_icons` so the small slot can use a properly
    /// Lanczos-downscaled thumbnail while Properties loads the full source.
    channel_icons_small: HashMap<i64, Option<egui::TextureHandle>>,
    /// Pre-scaled (64 px) per-INSTANCE icon textures for the instance rows of the
    /// streams table, keyed by monitor id — each instance shows the avatar fetched
    /// for its own account dir (GEEGA main vs alt). Same lifecycle as
    /// `channel_icons_small` (cleared on AssetFetch completion / channel rename).
    instance_icons_small: HashMap<i64, Option<egui::TextureHandle>>,
    /// Decoded + downscaled chat-emote frames, keyed by absolute image path. Shared
    /// with background decode tasks (`Arc<Mutex<…>>`). Animated GIF/WebP cycle; the
    /// frames are downscaled to render size to bound RAM, and the map is LRU-evicted
    /// against [`EMOTE_BUDGET_BYTES`] + cleared on asset refetch / popup close.
    emote_anim: Arc<Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>>,
    /// Bumped whenever `emote_anim` is cleared; in-flight decode tasks capture it at
    /// spawn and skip their insert if it changed, so a decode finishing after a
    /// popup-close / asset-refetch can't resurrect a stale (leaked) cache entry.
    emote_epoch: Arc<AtomicU64>,
    /// Loaded mainpage image assets (icon + banner per platform) for the channel
    /// Properties thumbnail strip, keyed by channel id. Full-resolution textures
    /// (so Alt-preview is crisp); loaded on window open, dropped on close/refetch.
    channel_asset_thumbs: HashMap<i64, Vec<AssetThumb>>,
    /// Per-provider *viewable* emote counts for the open Properties window, keyed by
    /// channel id. Cached because the count is derived from the same enumeration the
    /// viewer uses (one `fs::metadata` per emote) — recomputing it every frame would
    /// be hundreds of stat calls per repaint. Invalidated wherever `channel_asset_thumbs`
    /// is (open/rename/refetch/close).
    channel_emote_counts: HashMap<i64, Vec<(AssetAccount, [(EmoteProvider, usize); 4])>>,
    /// Per-platform asset-status rows for the open Properties window, keyed by channel
    /// id. Cached for the same reason as `channel_emote_counts`: each row is built from
    /// blocking filesystem I/O (`read_dir` + per-file `metadata` + full JSON manifest
    /// parse), and the status grid is rebuilt every frame — so doing the I/O per frame is
    /// dozens of syscalls per repaint and can freeze the UI thread on slow/AV-scanned
    /// storage. Invalidated wherever `channel_asset_thumbs` is (open/rename/refetch/close).
    channel_asset_status: HashMap<i64, Vec<PlatformAssetStatus>>,
    /// Snapshot of the global schedule-source order for the open Properties window. Taken
    /// once on open so `scope_override_editor` reads it from memory instead of doing a
    /// settings DB read (store mutex) every frame.
    props_source_order: Vec<SourceEntry>,
    /// In-flight background load of the channel Properties window's per-open data (icon +
    /// asset-thumbnail decode/upload, per-platform asset enumeration, and the schedule
    /// -source config/scope/order DB reads). Run OFF the UI thread so a slow disk, an AV
    /// scan, or the store mutex being held by a background task can't freeze the GUI on
    /// open — the window shows a "Loading…" placeholder until the bundle lands. `None`
    /// when no load is running.
    props_loads: Vec<PropsLoad>,
    /// In-flight native file/folder picker (background thread). The OS dialog blocks
    /// until the user picks or cancels; running it off the UI thread keeps egui alive.
    /// At most one picker open at a time (a second Browse click replaces any existing).
    pending_browse: Option<PendingBrowse>,
    /// In-flight form save (background thread). The INSERT/UPDATE + reload queries can
    /// block on the store mutex when a detection pass holds it; running off the UI
    /// thread prevents a visible freeze on "Save".
    pending_save: Option<PendingSave>,
    /// In-flight F5 / manual reload (background thread). Same DB queries as
    /// `pending_save` but no write — avoids blocking the UI thread on the store
    /// mutex while a schedule-refresh Tokio task holds it.
    pending_reload: Option<std::sync::mpsc::Receiver<Option<SaveRows>>>,
    /// A reload was requested while one was already in flight. The in-flight
    /// thread may have read the DB *before* the change that triggered the new
    /// request, so drop-and-forget would leave the UI stale (until F5) — run
    /// one more reload as soon as the current one lands instead.
    reload_queued: bool,
    /// Unix time of the last timer-driven background reload. Routine polls
    /// update the DB (e.g. `last_checked_at`) without emitting an event, so a
    /// slow cadence reload keeps sorted columns correct without F5.
    last_auto_reload: i64,
    /// TTL cache for per-row filesystem probes (see [`FsProbes`]).
    fs_probes: FsProbes,
    /// When the Videos list was last re-read from the store. The tab shows
    /// live progress, but a 1s TTL replaces the old full SELECT every frame.
    videos_refreshed: Option<std::time::Instant>,
    /// Bumped whenever `self.videos` is reloaded — keys the sort-model cache.
    videos_rev: u64,
    /// Videos sort/filter model cache: (videos_rev, unix second, model).
    /// Second granularity keeps speed cells ticking without per-frame rebuild.
    videos_model_cache: Option<(u64, i64, Vec<Vec<Cell>>)>,
    /// Lowercased `settings_search`, kept in sync on edit — `section_shown`
    /// runs per section per frame and must not re-lowercase each call.
    settings_search_lc: String,
    /// Cached recovery CDN host count for the Settings label (5s TTL) — the
    /// old code re-read + re-parsed the host list from the store every frame.
    recovery_host_count: Option<(std::time::Instant, usize)>,
    /// Frame-invariant Streams-view data (see [`StreamsViewCache`]); rebuilt
    /// once per second or when `streams_cache_rev` bumps.
    streams_cache: Option<StreamsViewCache>,
    /// Bumped whenever data feeding the Streams view changes NOW (reload
    /// installed, expansion toggled, F5, settings saved) so the cache rebuilds
    /// immediately instead of waiting for the next second tick.
    streams_cache_rev: u64,
    /// Cached YouTube Data API quota for today and the configured daily cutoff.
    /// Updated by the background reload-rows thread; never read from DB on the
    /// render thread (which would block if the DB mutex is held elsewhere).
    yt_quota_today: i64,
    yt_quota_cutoff: i64,
    /// Daily search.list query count and its cutoff (separate from unit quota).
    yt_search_today: i64,
    yt_search_cutoff: i64,
    /// Keys of quota warning issues the user has dismissed this session.
    dismissed_quota_warnings: HashSet<String>,
    /// In-flight schedule calendar reload (background thread). `all_upcoming_schedule`
    /// can hold the DB mutex for several seconds when historical rows accumulate;
    /// running it off the UI thread prevents frame freezes and unblocks the delete action.
    pending_schedule: Option<std::sync::mpsc::Receiver<Option<Vec<UpcomingStream>>>>,
    /// Open emote-viewer windows (one per channel+provider). Reuse the shared
    /// `emote_anim` decode cache, so emotes animate on the chat-replay clock.
    emote_viewers: Vec<EmoteViewer>,
    /// Open asset change-history popup (None = closed). Holds the channel's
    /// `asset_changes.jsonl` parsed + formatted once on open (newest first).
    asset_histories: Vec<AssetHistoryView>,
    /// Open About-page viewers (one per channel + platform + account): the
    /// account's archived about versions with a picker + rendered content.
    about_views: Vec<AboutView>,
    /// Channel Properties "About pages" rows: latest snapshot + version count
    /// per (platform, account), loaded off-thread with the props bundle.
    channel_about_latest: HashMap<i64, Vec<(crate::store::AboutSnapshotRow, i64)>>,
    /// GPU textures for the third-party emote-provider logos (7TV/BTTV), uploaded
    /// once on first use of the emote launcher buttons.
    provider_tex: Option<ProviderTextures>,
    /// Per-channel Twitch broadcaster name colour (from `name_color.txt`, fetched
    /// via Helix). `None` = looked up but the streamer set no colour / not Twitch.
    /// Tints the channel name in the Streams list; cleared with `channel_icons`.
    channel_twitch_colors: HashMap<i64, Option<egui::Color32>>,
    /// Sort + per-column filters for the Videos table.
    videos_sort: SortState,
    videos_filters: Vec<String>,
    /// Shared state of the interactive "Connect Twitch" device-code flow.
    twitch_flow: Arc<Mutex<AuthFlow>>,
    /// Shared state of the interactive "Connect YouTube" (Google) device-code flow.
    google_flow: Arc<Mutex<AuthFlow>>,
    /// Open "Import followed/subscriptions" confirmation dialog, if any.
    import_dialog: Option<ImportDialog>,
    /// Whether Streams rows show a status background tint (recording / ad / error).
    /// Toggled from the top bar; persisted under [`K_STATUS_BGCOLOR`]. Keyboard
    /// row selection is still highlighted regardless.
    status_bgcolor: bool,
    /// Whether the per-row Actions column (inline action buttons) is shown in the
    /// Streams + Videos tables. Off reclaims width; every action is also on the
    /// row's right-click context menu. Persisted under [`K_SHOW_ACTIONS`].
    show_actions: bool,
    /// Whether timestamp columns show a compact short format (e.g. `21/06 14:02`)
    /// instead of the full datetime. The full value appears in a tooltip. Persisted
    /// under [`K_SHORT_TIMESTAMPS`].
    shorten_timestamps: bool,
    /// Render chat emotes as inline images in the chat replay (off ⇒ show the
    /// emote code as text). Default on. Persisted under [`K_RENDER_EMOTES`].
    render_emotes: bool,
    /// Play animated emotes (off ⇒ static first frame). Default on. Persisted under
    /// [`K_ANIMATE_EMOTES`].
    animate_emotes: bool,
    /// Set to true by the "⇔ Fit columns" button; consumed in `channels_view`
    /// to call `TableBuilder::reset()` so columns revert to content-fit widths.
    reset_streams_columns: bool,
    /// Persisted column order/visibility for every grid table (Streams, Videos,
    /// Background Active/Recent, Processes, Issues); see [`crate::grid_columns`].
    streams_grid: GridState,
    videos_grid: GridState,
    bg_active_grid: GridState,
    bg_recent_grid: GridState,
    /// Background view: whether the disk-gate queue list is expanded
    /// (session-only).
    bg_show_gate_queue: bool,
    processes_grid: GridState,
    issues_grid: GridState,
    /// Backing state for the "⇕ Reorder columns…" window (`None` = closed) —
    /// a working copy of one table's entries, only written back + persisted
    /// (and only forcing one table reset, not one per intermediate move) when
    /// the user hits Apply. See [`ReorderColumnsState`].
    reorder_columns: Option<ReorderColumnsState>,
    /// Currently running background tasks (asset fetches, thumbnail downloads).
    background_tasks: Vec<crate::events::BackgroundTask>,
    /// Completed/failed background tasks (task, outcome, finished-at unix), newest
    /// first; capped at 100.
    finished_tasks: Vec<(crate::events::BackgroundTask, crate::events::TaskOutcome, i64)>,
    /// Enable/disable state for the periodic jobs (`events::TOGGLEABLE_JOBS`),
    /// mirrored from settings; edited via the Background "Scheduled" checkboxes.
    job_toggles: std::collections::HashMap<String, bool>,
    /// Debug view state — persisted across frames; fields are always present but
    /// only rendered when [`debug_view_enabled`] (debug build or `--debug`).
    debug_monitor_idx: usize,
    debug_test_title: String,
    debug_test_game: String,
    /// Format Designer: an interactive template preview/editor window.
    format_designer: Option<FormatDesignerState>,
    /// Pending "Stop recordings & quit" confirmation (triggered by the tray item).
    confirm_quit_stop: bool,
    /// Cached (ocr_stats, global_stats, poll_stats) for the Stats view; None = not yet loaded.
    stats_snapshot: Option<(OcrStats, GlobalStats, PollStats)>,
    /// I/O tab: cached sampler history + counters snapshot (refreshed ~1×/s
    /// while the tab is open — never cloned per frame).
    io_hist: Vec<crate::iomon::Sample>,
    io_snap: Option<crate::iomon::CountersSnapshot>,
    io_refreshed: Option<std::time::Instant>,
    /// I/O tab: which sub-tab is shown (Disks / Database).
    io_tab: IoTab,
    /// I/O tab: which series the rate graph shows.
    io_plot_metric: IoPlotMetric,
    /// I/O tab: recent-operations log filters.
    io_ops_cat: Option<crate::iomon::Cat>,
    io_ops_region: Option<crate::iomon::Region>,
    /// I/O tab: category-table sort (column index, ascending).
    io_cat_sort: (usize, bool),
    /// Files tab: off-thread path/drive scan (None = needs a (re)load).
    files_scan: Option<FilesScan>,
    files_scan_rx: Option<std::sync::mpsc::Receiver<FilesScan>>,
    /// Files tab: per-instance output-dir edit buffers (monitor id → draft).
    files_edit: std::collections::HashMap<i64, String>,
    /// Files tab: selected instances for batch actions.
    files_selected: std::collections::HashSet<i64>,
    /// Files tab: batch "set folder for selected" draft.
    files_batch_dir: String,
    /// Files tab: relocate-paths dialog drafts.
    files_reloc_from: String,
    files_reloc_to: String,
    files_reloc_monitors: bool,
    /// Files tab: last relocate preview (from-string, rec/video/monitor counts).
    files_reloc_preview: Option<(String, i64, i64, i64)>,
    files_status: String,
    /// Channel id to scroll into view on the next Streams render, after a save
    /// adds a new channel. Cleared once consumed. None = no pending scroll.
    scroll_to_channel: Option<i64>,
    /// Rename dialog: whether the dialog is open.
    show_rename_dialog: bool,
    /// Rename dialog: the recording id being renamed.
    rename_rec_id: Option<i64>,
    /// Rename dialog: the current template/stem string the user is editing.
    rename_draft: String,
    /// Rename dialog: live-expanded preview of `rename_draft`.
    rename_preview: String,
}
/// Handle to the background thread loading a channel Properties window's per-open data.
/// Polled each frame the window is open until the [`PropsLoaded`] bundle arrives. See
/// the `props_loads` field for why this work is off the UI thread.
struct PropsLoad {
    /// The channel being loaded; lets us ignore a bundle that arrives after the user
    /// switched the window to a different channel.
    channel_id: i64,
    rx: std::sync::mpsc::Receiver<PropsLoaded>,
}

/// The fully-loaded per-open Properties data, produced on a background thread and
/// installed into the per-channel caches on the UI thread. Every field is the result of
/// blocking work (disk reads + image decode/upload, asset-dir enumeration, store-mutex
/// DB reads) that previously ran inline on the UI thread and could freeze the GUI.
struct PropsLoaded {
    channel_id: i64,
    /// `None` = no icon file found (a successful "no icon" result, not a failure).
    icon: Option<egui::TextureHandle>,
    thumbs: Vec<AssetThumb>,
    emote_counts: Vec<(AssetAccount, [(EmoteProvider, usize); 4])>,
    asset_status: Vec<PlatformAssetStatus>,
    cfg: crate::schedule_source::ChannelSourceConfig,
    source_order: Vec<SourceEntry>,
    scope: crate::schedule_source::SourceScopeConfig,
    /// Latest About snapshot + version count per (platform, account).
    about_latest: Vec<(crate::store::AboutSnapshotRow, i64)>,
}

/// In-flight native file/folder picker spawned on a background thread so the UI
/// thread is never blocked by the OS dialog. Polled each frame via `try_recv`.
struct PendingBrowse {
    rx: std::sync::mpsc::Receiver<Option<String>>,
    /// Called on the UI thread once the picker returns a path. Receives `&mut App`
    /// and the selected path; skipped when the user cancels (dialog returns `None`).
    apply: Box<dyn FnOnce(&mut StreamArchiverApp, String)>,
}

/// Loaded rows returned by a background save-form thread; installed by
/// `drain_pending_save` once the thread completes.
struct SaveRows {
    rows: Vec<MonitorWithChannel>,
    channels: Vec<Channel>,
    next_streams: Vec<(i64, i64, String)>,
    yt_quota_today: i64,
    yt_quota_cutoff: i64,
    yt_search_today: i64,
    yt_search_cutoff: i64,
    /// Id of a newly-INSERTED monitor (a fresh add, not an edit) — the UI fires
    /// an immediate asset/About fetch for it so a new channel isn't blank until
    /// the hourly sweep. `None` for an edit.
    new_monitor_id: Option<i64>,
}

/// In-flight form-save spawned on a background thread. The thread holds the store
/// mutex while doing the INSERT/UPDATE + reload queries, keeping the UI thread free.
struct PendingSave {
    rx: std::sync::mpsc::Receiver<Result<SaveRows, String>>,
}
/// Spawn a native folder picker on a background thread. The picker blocks until
/// the user chooses or cancels; keeping it off the UI thread lets egui keep
/// painting (and the watchdog heartbeat keep beating). Returns a [`PendingBrowse`]
/// that the caller stores in `app.pending_browse`; the `apply` closure is called
/// on the UI thread once the user confirms a selection.
fn spawn_browse_folder(
    current: &str,
    apply: impl FnOnce(&mut StreamArchiverApp, String) + 'static,
) -> PendingBrowse {
    let (tx, rx) = std::sync::mpsc::channel();
    let current = current.to_string();
    std::thread::Builder::new()
        .name("browse-folder".into())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if !current.trim().is_empty() && crate::iomon::fs::exists_sync(crate::iomon::Cat::FsProbe, &current) {
                dialog = dialog.set_directory(&current);
            }
            let _ = tx.send(dialog.pick_folder().map(|p| p.to_string_lossy().to_string()));
        })
        .ok();
    PendingBrowse { rx, apply: Box::new(apply) }
}

/// Same as [`spawn_browse_folder`] but opens a file picker instead.
fn spawn_browse_file(
    current: &str,
    apply: impl FnOnce(&mut StreamArchiverApp, String) + 'static,
) -> PendingBrowse {
    let (tx, rx) = std::sync::mpsc::channel();
    let current = current.to_string();
    std::thread::Builder::new()
        .name("browse-file".into())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(parent) = std::path::Path::new(&current).parent() {
                if crate::iomon::fs::is_dir_sync(crate::iomon::Cat::FsProbe, parent) {
                    dialog = dialog.set_directory(parent);
                }
            }
            let _ = tx.send(dialog.pick_file().map(|p| p.to_string_lossy().to_string()));
        })
        .ok();
    PendingBrowse { rx, apply: Box::new(apply) }
}
impl eframe::App for StreamArchiverApp {
    /// eframe's default is 30s, and egui state (scroll positions, window
    /// geometry) changes almost every interaction — so the default rewrites
    /// the whole ~260 KB `egui_state.ron` twice a minute for the app's entire
    /// uptime. State is also saved on exit, so a long interval loses nothing.
    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(300)
    }

    /// Non-drawing logic. eframe also calls this while the window is hidden when
    /// `request_repaint` was called — which is how the tray's "Open" wakes us.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── UI-freeze watchdog heartbeat ──────────────────────────────────────
        // Stamp "the UI thread is alive" at the start of every frame. A frame that
        // enters here and never returns — or whose subsequent egui paint hangs
        // (e.g. a GPU emote-texture stall) — stops beating, and the watchdog thread
        // surfaces a native dialog instead of a silent freeze. The ≥1 fps repaint
        // floor keeps a *healthy* idle (reactive) UI beating so it never
        // false-alarms; while minimised the OS legitimately stops delivering
        // frames, so we mark the heartbeat inactive to suppress the alarm there.
        self.heartbeat.beat();
        self.heartbeat.set_activity(crate::watchdog::Activity::Frame);
        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        self.heartbeat.set_active(!minimized);
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        self.pump_messages(ctx);
        // Install filesystem-probe results the background worker finished
        // since last frame (never blocks — see `FsProbes`).
        self.fs_probes.drain_results();
        self.drain_pending_browse();
        self.drain_pending_save();
        self.drain_pending_reload();
        self.drain_pending_schedule();

        // Slow-cadence background reload: routine polls update the DB (last
        // checked, recording metadata) without emitting an event, so sorted
        // columns would drift stale until F5. A 30s re-read keeps the grid —
        // and therefore its sort order — current without user action.
        let now = now_unix();
        if now - self.last_auto_reload >= 30 {
            self.last_auto_reload = now;
            self.spawn_pending_reload();
            // Bound the probe cache: age out entries no longer being rendered.
            // (Never clear() wholesale — that used to force every visible path
            // back through a probe in a single frame.)
            self.fs_probes.evict_unused();
        }

        // Keep repainting at 50ms while a background DB load is in-flight so
        // the result is shown as soon as it arrives, not after the 1s heartbeat.
        if self.pending_save.is_some()
            || self.pending_reload.is_some()
            || self.pending_schedule.is_some()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        // Close button hides to tray unless we're really quitting.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_shortcuts(ui.ctx());
        // Arm/disarm widget registration before anything draws, so pushes go
        // live the same frame F12 turns the inspector on.
        crate::inspector::set_enabled(self.show_inspector);

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
                    ui.selectable_value(&mut self.view, View::Streams, "Streams")
                        .inspect("View tab: Streams", &[]);
                    ui.selectable_value(&mut self.view, View::Videos, "Videos");
                    ui.selectable_value(&mut self.view, View::Schedule, "Schedule");
                    if ui.selectable_value(&mut self.view, View::Posts, "Posts").clicked() {
                        self.posts_refreshed = None; // force reload on tab open
                    }
                    ui.selectable_value(&mut self.view, View::Background, "Background");
                    if ui.selectable_value(&mut self.view, View::Files, "Files").clicked() {
                        self.files_scan = None; // force rescan on tab open
                    }
                    ui.selectable_value(&mut self.view, View::Settings, "Settings");
                    if ui.selectable_value(&mut self.view, View::Stats, "Stats").clicked() {
                        self.stats_snapshot = None; // force reload on tab open
                    }
                    ui.selectable_value(&mut self.view, View::IoMonitor, "I/O");
                    if debug_view_enabled() {
                        ui.selectable_value(&mut self.view, View::Debug, "Debug");
                    }
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
                    if ui
                        .checkbox(&mut self.shorten_timestamps, "Short timestamps")
                        .on_hover_text(
                            "Show timestamps in a compact short format (e.g. 21/06 14:02) \
                             instead of the full datetime. Hover any timestamp for the full value. \
                             The short format is configurable in Settings → Display.",
                        )
                        .changed()
                    {
                        set_short_ts(self.shorten_timestamps);
                        let _ = self.core.store.set_setting(
                            K_SHORT_TIMESTAMPS,
                            if self.shorten_timestamps { "1" } else { "0" },
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Pinned far-right and shown on every view — the process
                        // manager is a global utility, not Background-specific.
                        if ui
                            .button("🖥 Process manager")
                            .on_hover_text(
                                "All spawned download tool processes (recordings, videos, \
                                 chat) — PIDs, status, and manual Stop / Kill.",
                            )
                            .clicked()
                        {
                            self.show_processes = true;
                            self.processes_refreshed = None; // force an immediate refresh
                        }
                        {
                            let quota_warnings = self.active_quota_warnings();
                            let n = self.issues_recs.len() + self.issues_missing.len()
                                + self.issues_errors.len() + self.issues_errors_no_file.len()
                                + self.issues_stuck.len() + self.issues_muted_vod.len()
                                + self.issues_unmerged.len() + self.issues_head_mismatch.len()
                                + quota_warnings.len();
                            let label = if n > 0 {
                                format!("⚠ Issues ({})", n)
                            } else {
                                "⚠ Issues".to_string()
                            };
                            let btn = egui::Button::new(label).small();
                            let btn = if n > 0 {
                                btn.fill(egui::Color32::from_rgb(160, 90, 10))
                            } else {
                                btn
                            };
                            if ui
                                .add(btn)
                                .on_hover_text("Recordings and quota warnings that need attention")
                                .clicked()
                            {
                                self.show_issues = true;
                                self.issues_refreshed = None;
                            }
                        }
                        {
                            // Notifications feed (bell). Mirrors the Issues button:
                            // the unread badge count is cached (refreshed on the
                            // Issues-style throttle in `notifications_window`, even
                            // while the window is closed) so it stays live.
                            let n = self.notif_unread;
                            let label = if n > 0 {
                                format!("🔔 {n}")
                            } else {
                                "🔔".to_string()
                            };
                            let btn = egui::Button::new(label).small();
                            let btn = if n > 0 {
                                btn.fill(egui::Color32::from_rgb(160, 90, 10))
                            } else {
                                btn
                            };
                            if ui
                                .add(btn)
                                .on_hover_text("Notifications: went-live, recordings, errors, schedule changes, YouTube posts")
                                .clicked()
                            {
                                self.show_notifications = true;
                                self.notif_refreshed = None; // force an immediate refresh
                                // Mark-all-read on open so the badge clears when you
                                // look; items arriving while open stay unread.
                                let _ = self
                                    .core
                                    .store
                                    .mark_notifications_read_before(crate::models::now_unix());
                                self.notif_unread = 0;
                            }
                        }
                        {
                            let n = self.scheduled_recordings.iter().filter(|r| r.rec.enabled).count();
                            let label = if n > 0 {
                                format!("📅 Scheduled rec ({n})")
                            } else {
                                "📅 Scheduled rec".to_string()
                            };
                            if ui
                                .button(label)
                                .on_hover_text(
                                    "Recordings scheduled to force-start at a specific time or on \
                                     a weekly repeat, bypassing Auto — for channels you don't want \
                                     kept on Auto.",
                                )
                                .clicked()
                            {
                                self.show_scheduled_recordings = true;
                            }
                        }
                        if ui
                            .button("📣 Posts")
                            .on_hover_text("Pop out the YouTube posts feed in its own window")
                            .clicked()
                        {
                            self.show_posts_window = true;
                            self.posts_refreshed = None;
                        }
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
                                    vod_download: None,
                                    vod_replace: None,
                                    head_backfill_fetch: None,
                                    head_backfill_replace: None,
                                });
                            }
                            if ui
                                .button("⇔")
                                .on_hover_text("Auto-fit all columns to their content width")
                                .clicked()
                            {
                                self.reset_streams_columns = true;
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
            View::Posts => self.posts_view(ui),
            View::Background => self.background_view(ui),
            View::Files => self.files_view(ui),
            View::Settings => self.settings_view(ui),
            View::Stats => self.stats_view(ui),
            View::IoMonitor => self.io_view(ui),
            View::Debug => self.debug_view(ui),
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
                View::Videos | View::Stats | View::IoMonitor | View::Debug | View::Posts
                | View::Files => {}
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
                vod_download: None,
                vod_replace: None,
                head_backfill_fetch: None,
                head_backfill_replace: None,
            });
        }
        if ctx_refresh_schedule {
            self.core.request_schedule_refresh();
            self.spawn_reload_schedule();
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
        self.confirm_delete_segment_window(ui.ctx());
        self.merge_preview_window(ui.ctx());
        self.confirm_delete_segments_window(ui.ctx());
        self.save_preset_window(ui.ctx());
        self.format_probe_window(ui.ctx());
        self.recover_vod_window(ui.ctx());
        self.ad_popup_windows(ui.ctx());
        self.meta_popup_windows(ui.ctx());
        self.schedule_popup_windows(ui.ctx());
        self.schedule_sources_window(ui.ctx());
        self.schedule_day_window(ui.ctx());
        self.edit_schedule_window(ui.ctx());
        self.chat_popup_windows(ui.ctx());
        self.instance_properties_windows(ui.ctx());
        self.channel_properties_windows(ui.ctx());
        self.emote_viewer_windows(ui.ctx());
        self.rename_dialog_window(ui.ctx());
        self.asset_history_windows(ui.ctx());
        self.recording_properties_windows(ui.ctx());
        self.processes_window(ui.ctx());
        self.reorder_columns_window(ui.ctx());
        self.scheduled_recordings_window(ui.ctx());
        self.scheduled_recording_form_window(ui.ctx());
        self.confirm_delete_scheduled_recording_window(ui.ctx());
        self.issues_window(ui.ctx());
        self.notifications_window(ui.ctx());
        self.posts_window(ui.ctx());
        self.format_designer_window(ui.ctx());
        self.confirm_quit_stop_window(ui.ctx());
        self.import_window(ui.ctx());
        self.about_windows(ui.ctx());
        self.inspector_window(ui.ctx());

        draw_alt_image_preview(ui.ctx());

        // Must remain the FINAL statement of ui(): the child-viewport windows
        // above register their widgets after the root CentralPanel, so an
        // earlier drain would split one frame's widgets across two snapshots.
        self.inspector.end_frame(self.show_inspector);
    }
}
