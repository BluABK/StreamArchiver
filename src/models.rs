//! Domain types for channels, monitors (per-tool instances), and recordings.
//!
//! The data model is `channel -> monitor -> recording`. "Multiple instances of
//! the same channel" is expressed as multiple [`Monitor`] rows pointing at one
//! [`Channel`].

use serde::{Deserialize, Serialize};

/// Streaming platform a channel belongs to. Drives default detection/tool choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    Twitch,
    YouTube,
    Kick,
    Generic,
}

impl Platform {
    pub const ALL: [Platform; 4] = [
        Platform::Twitch,
        Platform::YouTube,
        Platform::Kick,
        Platform::Generic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Twitch => "twitch",
            Platform::YouTube => "youtube",
            Platform::Kick => "kick",
            Platform::Generic => "generic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Platform::Twitch => "Twitch",
            Platform::YouTube => "YouTube",
            Platform::Kick => "Kick",
            Platform::Generic => "Generic",
        }
    }

    pub fn parse(s: &str) -> Platform {
        match s {
            "twitch" => Platform::Twitch,
            "youtube" => Platform::YouTube,
            "kick" => Platform::Kick,
            _ => Platform::Generic,
        }
    }

    /// Best-effort platform inference from a channel URL.
    pub fn detect(url: &str) -> Platform {
        let u = url.to_lowercase();
        if u.contains("twitch.tv") {
            Platform::Twitch
        } else if u.contains("youtube.com") || u.contains("youtu.be") {
            Platform::YouTube
        } else if u.contains("kick.com") {
            Platform::Kick
        } else {
            Platform::Generic
        }
    }

    /// Sensible default download tool for this platform (research-backed:
    /// streamlink for Twitch incl. 2K, yt-dlp for YouTube).
    pub fn default_tool(self) -> Tool {
        match self {
            Platform::Twitch => Tool::Streamlink,
            Platform::YouTube => Tool::YtDlp,
            Platform::Kick => Tool::Streamlink,
            Platform::Generic => Tool::Streamlink,
        }
    }

    /// Default detection method when no API credentials are configured.
    pub fn default_detection(self) -> DetectionMethod {
        match self {
            Platform::Twitch => DetectionMethod::TwitchApi,
            Platform::YouTube => DetectionMethod::Scrape,
            Platform::Kick => DetectionMethod::Scrape,
            Platform::Generic => DetectionMethod::GenericProbe,
        }
    }

    /// Detection methods currently implemented for this platform (offered in the
    /// UI). YouTube Data API and Twitch EventSub are planned for a later phase.
    pub fn detection_methods(self) -> &'static [DetectionMethod] {
        match self {
            Platform::Twitch => &[
                DetectionMethod::TwitchApi,
                DetectionMethod::EventSub,
                DetectionMethod::EventSubHelix,
                DetectionMethod::Scrape,
                DetectionMethod::GenericProbe,
            ],
            Platform::YouTube => &[
                DetectionMethod::Scrape,
                DetectionMethod::WebSub,
                DetectionMethod::YouTubeApi,
                DetectionMethod::GenericProbe,
            ],
            Platform::Kick => &[
                DetectionMethod::Scrape,
                DetectionMethod::KickApi,
                DetectionMethod::GenericProbe,
            ],
            Platform::Generic => &[DetectionMethod::GenericProbe],
        }
    }
}

/// External capture tool to invoke for downloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    Streamlink,
    YtDlp,
    Ffmpeg,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::Streamlink, Tool::YtDlp, Tool::Ffmpeg];

    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Streamlink => "streamlink",
            Tool::YtDlp => "yt-dlp",
            Tool::Ffmpeg => "ffmpeg",
        }
    }

    pub fn label(self) -> &'static str {
        self.as_str()
    }

    pub fn parse(s: &str) -> Tool {
        match s {
            "yt-dlp" => Tool::YtDlp,
            "ffmpeg" => Tool::Ffmpeg,
            _ => Tool::Streamlink,
        }
    }

    /// One-line explanation for UI hover tooltips.
    pub fn tooltip(self) -> &'static str {
        match self {
            Tool::Streamlink => {
                "Best for Twitch (reaches 1440p/2K HEVC via enhanced codecs) and Kick. \
                 Records to .ts, then remuxes to MKV."
            }
            Tool::YtDlp => {
                "Best for YouTube (supports capture-from-start). Also downloads VODs and many \
                 other sites."
            }
            Tool::Ffmpeg => {
                "Direct capture/remux of a known stream URL; a lowest-level fallback when \
                 streamlink/yt-dlp don't fit."
            }
        }
    }
}

/// Per-channel live-detection strategy (the UI "method" dropdown).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionMethod {
    /// Official platform API polling (Twitch Helix Get Streams). Needs creds.
    TwitchApi,
    /// YouTube Data API polling (videos.list / playlistItems). Needs API key.
    YouTubeApi,
    /// Lightweight page/JSON scrape (e.g. YouTube `/live`, Kick channel JSON).
    Scrape,
    /// CLI self-poll loop (streamlink --retry-streams / yt-dlp --wait-for-video).
    CliSelfPoll,
    /// Generic HTTP probe / `streamlink --can-handle-url`.
    GenericProbe,
    /// Twitch EventSub push (websocket/conduit). Phase 4.
    EventSub,
    /// Twitch EventSub push **and** Helix polling together: whichever detects
    /// live first starts the recording (the supervisor dedupes). The poll is a
    /// safety net for missed events (network drop, app started after go-live).
    EventSubHelix,
    /// Kick official API polling (client-credentials app token). Needs creds.
    KickApi,
    /// YouTube WebSub (PubSubHubbub) push via an external VPS relay: a go-live
    /// notification triggers an immediate liveness check. Polls (scrape) as a
    /// fallback. Needs the `yt-websub` server URL + token in Settings.
    WebSub,
}

impl DetectionMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            DetectionMethod::TwitchApi => "twitch_api",
            DetectionMethod::YouTubeApi => "youtube_api",
            DetectionMethod::Scrape => "scrape",
            DetectionMethod::CliSelfPoll => "cli_selfpoll",
            DetectionMethod::GenericProbe => "generic_probe",
            DetectionMethod::EventSub => "eventsub",
            DetectionMethod::EventSubHelix => "eventsub_helix",
            DetectionMethod::KickApi => "kick_api",
            DetectionMethod::WebSub => "websub",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DetectionMethod::TwitchApi => "Twitch API (Helix polling)",
            DetectionMethod::YouTubeApi => "YouTube Data API (key)",
            DetectionMethod::Scrape => "Scrape poll (no API)",
            DetectionMethod::CliSelfPoll => "CLI self-poll loop",
            DetectionMethod::GenericProbe => "Generic HTTP probe",
            DetectionMethod::EventSub => "Twitch EventSub push",
            DetectionMethod::EventSubHelix => "Twitch EventSub + Helix",
            DetectionMethod::KickApi => "Kick official API",
            DetectionMethod::WebSub => "YouTube WebSub (VPS push)",
        }
    }

    /// Compact label for the channel table column.
    pub fn short_label(self) -> &'static str {
        match self {
            DetectionMethod::TwitchApi => "Helix API",
            DetectionMethod::YouTubeApi => "YT API",
            DetectionMethod::Scrape => "Scrape",
            DetectionMethod::CliSelfPoll => "CLI",
            DetectionMethod::GenericProbe => "Probe",
            DetectionMethod::EventSub => "EventSub",
            DetectionMethod::EventSubHelix => "ES+Helix",
            DetectionMethod::KickApi => "Kick API",
            DetectionMethod::WebSub => "WebSub",
        }
    }

    pub fn parse(s: &str) -> DetectionMethod {
        match s {
            "twitch_api" => DetectionMethod::TwitchApi,
            "youtube_api" => DetectionMethod::YouTubeApi,
            "scrape" => DetectionMethod::Scrape,
            "cli_selfpoll" => DetectionMethod::CliSelfPoll,
            "eventsub" => DetectionMethod::EventSub,
            "eventsub_helix" => DetectionMethod::EventSubHelix,
            "kick_api" => DetectionMethod::KickApi,
            "websub" => DetectionMethod::WebSub,
            _ => DetectionMethod::GenericProbe,
        }
    }

    /// One-line explanation for UI hover tooltips.
    pub fn tooltip(self) -> &'static str {
        match self {
            DetectionMethod::TwitchApi => {
                "Polls Twitch Helix (Get Streams) each interval, batched up to 100 channels. \
                 Reliable; detects within one poll interval. Needs Twitch Client ID + Secret, \
                 or a connected Twitch account."
            }
            DetectionMethod::EventSub => {
                "Real-time push over a WebSocket (conduit + app token): catches go-live within \
                 seconds and ignores the poll interval. Needs Twitch Client ID + Secret; \
                 reconciles current state on (re)connect."
            }
            DetectionMethod::EventSubHelix => {
                "Best of both: EventSub push for instant go-live AND Helix polling on the \
                 interval as a safety net. Whichever fires first starts the recording, so a \
                 missed event (network drop, or the app started after go-live) is still caught. \
                 Needs Twitch Client ID + Secret. Tip: a longer interval is fine here."
            }
            DetectionMethod::YouTubeApi => {
                "YouTube Data API (search.list eventType=live). Reports the real go-live time but \
                 is quota-limited (~100 checks/day) — use a long interval. Needs a YouTube API key."
            }
            DetectionMethod::Scrape => {
                "Fetches the channel page/JSON each interval (YouTube /live, Kick). No credentials \
                 needed; can break when sites change."
            }
            DetectionMethod::KickApi => {
                "Kick official API via a client-credentials app token — more reliable than \
                 scraping. Needs Kick Client ID + Secret."
            }
            DetectionMethod::WebSub => {
                "YouTube push via an external yt-websub VPS relay: a go-live notification \
                 triggers an immediate liveness check (records only if actually live), with \
                 scrape polling as a safety-net fallback. Set the VPS URL + token in Settings; \
                 a longer poll interval is fine."
            }
            DetectionMethod::GenericProbe => {
                "Probes the URL with streamlink (--stream-url) each interval. No credentials; works \
                 for anything streamlink/yt-dlp supports."
            }
            DetectionMethod::CliSelfPoll => {
                "A resident streamlink/yt-dlp retry loop per channel. Higher footprint; intended \
                 only for a few channels."
            }
        }
    }
}

/// Per-monitor authentication source for the downloader tools. `Inherit` uses
/// the global Settings default; anything else overrides it for this channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthKind {
    /// Use the global default from Settings.
    Inherit,
    /// Force no authentication for this channel.
    Disabled,
    /// `--cookies-from-browser <value or global browser>` (yt-dlp).
    CookiesBrowser,
    /// `--cookies <path>` (yt-dlp) — a cookies.txt file.
    CookiesFile,
    /// An auth token (Twitch: `--twitch-api-header=Authorization=OAuth <value>`).
    Token,
}

impl AuthKind {
    pub const ALL: [AuthKind; 5] = [
        AuthKind::Inherit,
        AuthKind::Disabled,
        AuthKind::CookiesBrowser,
        AuthKind::CookiesFile,
        AuthKind::Token,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AuthKind::Inherit => "inherit",
            AuthKind::Disabled => "none",
            AuthKind::CookiesBrowser => "cookies_browser",
            AuthKind::CookiesFile => "cookies_file",
            AuthKind::Token => "token",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AuthKind::Inherit => "Inherit (global)",
            AuthKind::Disabled => "None",
            AuthKind::CookiesBrowser => "Browser cookies",
            AuthKind::CookiesFile => "Cookies file",
            AuthKind::Token => "Auth token",
        }
    }

    pub fn parse(s: &str) -> AuthKind {
        match s {
            "none" => AuthKind::Disabled,
            "cookies_browser" => AuthKind::CookiesBrowser,
            "cookies_file" => AuthKind::CookiesFile,
            "token" => AuthKind::Token,
            _ => AuthKind::Inherit,
        }
    }
}

/// When to probe a capture for **actual** media info (resolution/fps/codec) used
/// by the filename `{resolution}`/`{height}`/`{width}`/`{fps}`/`{vcodec}`
/// variables. These aren't known when the name is first chosen (before capture),
/// so the user picks how to obtain them. Only matters when the template uses one
/// of those variables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MediaInfoMode {
    /// Don't probe; media variables stay empty.
    #[default]
    Off,
    /// Probe the stream just before recording so the name is final from the start
    /// (adds a little latency; can be wrong if the chosen format shifts).
    PreProbe,
    /// Probe the finished file and rename it afterwards (most accurate; the final
    /// name only appears once the capture completes).
    PostRename,
    /// Pre-probe for an initial name, then correct it by renaming after capture.
    Both,
}

impl MediaInfoMode {
    pub const ALL: [MediaInfoMode; 4] = [
        MediaInfoMode::Off,
        MediaInfoMode::PreProbe,
        MediaInfoMode::PostRename,
        MediaInfoMode::Both,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MediaInfoMode::Off => "off",
            MediaInfoMode::PreProbe => "preprobe",
            MediaInfoMode::PostRename => "postrename",
            MediaInfoMode::Both => "both",
        }
    }

    pub fn parse(s: &str) -> MediaInfoMode {
        match s {
            "preprobe" => MediaInfoMode::PreProbe,
            "postrename" => MediaInfoMode::PostRename,
            "both" => MediaInfoMode::Both,
            _ => MediaInfoMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MediaInfoMode::Off => "Off",
            MediaInfoMode::PreProbe => "Pre-probe (before recording)",
            MediaInfoMode::PostRename => "Post-capture rename",
            MediaInfoMode::Both => "Pre-probe + rename",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            MediaInfoMode::Off => {
                "Don't probe; the {resolution}/{height}/{width}/{fps}/{vcodec} \
                 filename variables stay empty."
            }
            MediaInfoMode::PreProbe => {
                "Probe the stream just before recording so the filename is final from \
                 the start. Adds a little latency and can be wrong if the chosen \
                 format shifts mid-stream."
            }
            MediaInfoMode::PostRename => {
                "Record first, then probe the finished file and rename it to the \
                 template. Most accurate; the final name only appears once the \
                 capture completes."
            }
            MediaInfoMode::Both => {
                "Pre-probe for an initial name, then correct it by renaming after \
                 capture if the actual media differs."
            }
        }
    }

    /// Whether to probe the stream before recording.
    pub fn pre(self) -> bool {
        matches!(self, MediaInfoMode::PreProbe | MediaInfoMode::Both)
    }

    /// Whether to probe the finished file and rename.
    pub fn post(self) -> bool {
        matches!(self, MediaInfoMode::PostRename | MediaInfoMode::Both)
    }
}

/// Output container. Default MKV (robust to mid-write kills, seekable). Never MP4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    Mkv,
    Ts,
}

impl Container {
    pub const ALL: [Container; 2] = [Container::Mkv, Container::Ts];

    pub fn as_str(self) -> &'static str {
        match self {
            Container::Mkv => "mkv",
            Container::Ts => "ts",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::Mkv => "MKV (recommended)",
            Container::Ts => "TS",
        }
    }

    pub fn parse(s: &str) -> Container {
        match s {
            "ts" => Container::Ts,
            _ => Container::Mkv,
        }
    }
}

/// A channel container. `name` groups its instances; `url`/`platform` are legacy
/// (the source URL/platform now live on each [`Monitor`]) and kept only for the
/// schema/`find_channel_by_url`. New containers store an empty URL.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    #[allow(dead_code)]
    pub url: String,
    #[allow(dead_code)]
    pub platform: Platform,
    pub created_at: i64,
}

/// One capture instance for a channel (source URL + tool + quality + detection +
/// output). The URL lives here (not on the channel) so one channel/container can
/// hold instances on different platforms (e.g. the same creator on Twitch + YT).
#[derive(Clone, Debug)]
pub struct Monitor {
    pub id: i64,
    pub channel_id: i64,
    /// Source URL for this instance (platform is derived from it).
    pub url: String,
    pub enabled: bool,
    pub tool: Tool,
    pub detection_method: DetectionMethod,
    pub poll_interval_secs: i64,
    pub quality: String,
    pub output_dir: String,
    pub filename_template: String,
    pub container: Container,
    /// Capture from the start of the broadcast (yt-dlp `--live-from-start`,
    /// streamlink `--hls-live-restart`) vs. from the live edge. Default true.
    pub capture_from_start: bool,
    /// Manually marked ad-free for our account (YouTube membership/Premium,
    /// Twitch Turbo/sub) — captures won't have ad-break hard cuts. Auto Twitch-sub
    /// detection can also set the displayed status; see `MonitorWithChannel`.
    pub ad_free: bool,
    /// Per-channel auth override for the downloader (Inherit = use global).
    pub auth_kind: AuthKind,
    /// Value for `auth_kind`: browser name, cookies.txt path, or token.
    pub auth_value: String,
    /// Audio tracks to capture. Empty = the tool's default (one track); `all`/`*`
    /// = every audio track; otherwise a comma-separated pass-through list of
    /// language codes/names. Honored by streamlink (`--hls-audio-select`); the
    /// ffmpeg tool keeps all video+audio tracks via its capture mapping
    /// (subset selection not supported), and yt-dlp ignores it.
    pub audio_tracks: String,
    /// Subtitle tracks to capture. Empty = none; `all` = every subtitle;
    /// otherwise a comma-separated pass-through list of language codes. Honored by
    /// yt-dlp (`--sub-langs` + write sidecars); streamlink can't mux subtitles.
    pub subtitle_tracks: String,
    /// Capture chat to a sidecar alongside the recording. Twitch: a native
    /// anonymous IRC-over-WebSocket logger (`.chat.jsonl`). YouTube (yt-dlp tool):
    /// yt-dlp `--sub-langs live_chat` (`.live_chat.json`). Other combinations
    /// don't capture chat.
    pub chat_log: bool,
    pub extra_args: String,
    pub max_concurrent: i64,
    pub last_checked_at: Option<i64>,
    pub last_state: String,
}

impl Monitor {
    /// Platform inferred from this instance's source URL.
    pub fn platform(&self) -> Platform {
        Platform::detect(&self.url)
    }
}

/// A monitor joined with its parent channel, for table display and scheduling.
#[derive(Clone, Debug)]
pub struct MonitorWithChannel {
    pub channel: Channel,
    pub monitor: Monitor,
    /// Latest recording's start/end/status (for the Started/Duration columns).
    pub last_recording_started: Option<i64>,
    pub last_recording_ended: Option<i64>,
    pub last_recording_status: Option<String>,
    /// Platform-reported (or approximated) go-live time of the latest recording.
    pub last_recording_went_live: Option<i64>,
    pub last_recording_went_live_approx: bool,
    /// Resolved "missed beginning" for the latest recording, in seconds. Set to
    /// `0` once a from-start capture has covered the whole broadcast (caught up to
    /// the live edge). `None` until/unless confirmed — the UI then falls back to
    /// the provisional `started - went_live` estimate.
    pub last_recording_lost_secs: Option<i64>,
    /// Ad breaks detected during the latest recording take (count + total
    /// seconds). Each break is a hard cut in the finished file; see [`AdBreak`].
    pub last_recording_ad_count: i64,
    pub last_recording_ad_secs: i64,
    /// Title + game/category changes logged during the latest recording take (see
    /// [`StreamMetaChange`]). Drives the Changes column / popup.
    pub last_recording_meta_changes: i64,
    /// Cached auto Twitch-sub ad-free status: `None` unknown/not checked,
    /// `Some(false)` checked & not subscribed, `Some(true)` subscribed. Combined
    /// with `monitor.ad_free` (the manual flag) for the Ad-free column. (The
    /// refresher tracks the last-checked time via a separate lightweight query.)
    pub ad_free_sub: Option<bool>,
    /// Total recording takes for this monitor (drives the history-tree disclosure
    /// without loading the full history until a row is expanded).
    pub recording_count: i64,
}

/// One advertisement break detected during a recording take.
///
/// Streamlink *filters out* Twitch ad segments, so a break becomes a hard cut in
/// the captured file rather than recorded ad footage. `at_secs` is the
/// (approximate) position of that hard cut in the finished file — i.e. the
/// content captured up to the break — so it can be used directly as a seek
/// timestamp. `duration_secs` is the ad-pod length the tool reported.
#[derive(Clone, Debug)]
pub struct AdBreak {
    // Round-tripped from the DB row but not read in the UI yet (the cut list uses
    // only the offset + duration); kept so the data is available without a change.
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub recording_id: i64,
    pub at_secs: i64,
    pub duration_secs: i64,
}

/// One title or game/category change observed during a recording take.
///
/// `at_secs` is the offset from the take's start (wall clock) when the change was
/// detected. `kind` is `"title"` or `"category"`. The first entry for each kind
/// is the *initial* value (its `old_value` is empty); later entries record a
/// transition (`old_value` -> `new_value`). Cascades when the recording is
/// removed.
#[derive(Clone, Debug)]
pub struct StreamMetaChange {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub recording_id: i64,
    pub at_secs: i64,
    pub kind: String,
    pub old_value: String,
    pub new_value: String,
}

/// An on-demand, one-shot video/VOD download (a YouTube video, a Twitch VOD,
/// etc.) — distinct from a [`Monitor`], which watches a channel for live streams.
///
/// `status` is one of: `queued`, `downloading`, `completed`, `failed`,
/// `stopped`, `orphaned`. Output is always MKV.
#[derive(Clone, Debug)]
pub struct Video {
    pub id: i64,
    pub url: String,
    /// User-facing label (falls back to the URL in the UI when empty).
    pub title: String,
    /// Detected uploader/channel name (empty unless auto-detected).
    pub channel: String,
    pub platform: Platform,
    pub tool: Tool,
    pub quality: String,
    pub output_dir: String,
    pub filename_template: String,
    pub auth_kind: AuthKind,
    pub auth_value: String,
    pub extra_args: String,
    /// Resolve the real stream/video title at download time (via yt-dlp) and use
    /// it for `{title}` (and `{name}` when no Name is set).
    pub auto_title: bool,
    pub status: String,
    pub output_path: String,
    pub bytes: i64,
    pub created_at: i64,
    // Persisted run metadata, round-tripped through the store but not yet shown
    // in the UI (kept so the data is available without a schema/struct change).
    #[allow(dead_code)]
    pub exit_code: Option<i64>,
    #[allow(dead_code)]
    pub started_at: Option<i64>,
    #[allow(dead_code)]
    pub ended_at: Option<i64>,
}

impl Video {
    /// True while the download is queued or running (drives the UI live-refresh).
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "downloading")
    }
}

/// Default download settings for one platform, used to pre-fill the Videos-tab
/// download form. The form copies these in when the pasted URL's platform
/// changes; the user can then override any field per download.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlatformDownloadDefault {
    pub tool: Tool,
    pub quality: String,
    /// Auth used when the form's auth is left at "Default" (`Inherit` chains to
    /// the global Settings download-auth method).
    pub auth_kind: AuthKind,
    pub auth_value: String,
    pub output_dir: String,
    pub filename_template: String,
    pub extra_args: String,
}

impl PlatformDownloadDefault {
    /// Built-in starting values for a platform (best quality, platform's
    /// preferred tool, global auth, the app's default output folder).
    pub fn seeded(platform: Platform, default_output_dir: &str) -> PlatformDownloadDefault {
        PlatformDownloadDefault {
            tool: platform.default_tool(),
            quality: "best".into(),
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            output_dir: default_output_dir.to_string(),
            filename_template: "{name}_{date}_{time}".into(),
            extra_args: String::new(),
        }
    }
}

/// Per-platform download defaults for the Videos tab, persisted as JSON in
/// `app_settings`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadDefaults {
    pub twitch: PlatformDownloadDefault,
    pub youtube: PlatformDownloadDefault,
    pub kick: PlatformDownloadDefault,
    pub generic: PlatformDownloadDefault,
}

impl DownloadDefaults {
    pub fn seeded(default_output_dir: &str) -> DownloadDefaults {
        DownloadDefaults {
            twitch: PlatformDownloadDefault::seeded(Platform::Twitch, default_output_dir),
            youtube: PlatformDownloadDefault::seeded(Platform::YouTube, default_output_dir),
            kick: PlatformDownloadDefault::seeded(Platform::Kick, default_output_dir),
            generic: PlatformDownloadDefault::seeded(Platform::Generic, default_output_dir),
        }
    }

    pub fn get(&self, platform: Platform) -> &PlatformDownloadDefault {
        match platform {
            Platform::Twitch => &self.twitch,
            Platform::YouTube => &self.youtube,
            Platform::Kick => &self.kick,
            Platform::Generic => &self.generic,
        }
    }

    pub fn get_mut(&mut self, platform: Platform) -> &mut PlatformDownloadDefault {
        match platform {
            Platform::Twitch => &mut self.twitch,
            Platform::YouTube => &mut self.youtube,
            Platform::Kick => &mut self.kick,
            Platform::Generic => &mut self.generic,
        }
    }
}

/// `app_settings` key for the global [`MediaInfoMode`] (filename media probing).
pub const K_FILENAME_MEDIA: &str = "filename_media_info";

/// Current unix timestamp in seconds.
pub fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One recording attempt ("take") of a stream — a single capture-process run.
/// Multiple takes (crash / network drop / manual stop+start) can belong to one
/// broadcast; see [`group_recordings`].
#[derive(Clone, Debug)]
pub struct Recording {
    pub id: i64,
    pub monitor_id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: String,
    pub bytes: i64,
    pub exit_code: Option<i64>,
    pub output_path: String,
    pub went_live_at: Option<i64>,
    pub went_live_approx: bool,
    pub lost_secs: Option<i64>,
    /// Platform stream/video id when detection knew it; `None` for id-less methods.
    pub stream_id: Option<String>,
    /// Ad breaks detected during this take (count + total seconds). Each break is
    /// a hard cut in the finished file; the per-break offsets live in `ad_break`.
    pub ad_count: i64,
    pub ad_secs: i64,
    /// Title + game/category changes logged during this take; per-change rows live
    /// in `stream_meta_change`.
    pub meta_change_count: i64,
}

impl Recording {
    pub fn is_active(&self) -> bool {
        self.status == "recording"
    }
    /// End time for duration math (`now` while the take is still in progress).
    pub fn duration_secs(&self, now: i64) -> i64 {
        (self.ended_at.unwrap_or(now) - self.started_at).max(0)
    }
}

/// A set of recording takes that belong to the same broadcast.
#[derive(Clone, Debug)]
pub struct StreamGroup {
    /// Stable key for display + UI expansion state.
    pub key: String,
    pub stream_id: Option<String>,
    pub went_live_at: Option<i64>,
    pub went_live_approx: bool,
    /// Takes, oldest first.
    pub takes: Vec<Recording>,
}

impl StreamGroup {
    pub fn is_active(&self) -> bool {
        self.takes.iter().any(Recording::is_active)
    }
    /// Earliest take start (takes are stored oldest-first).
    pub fn started_at(&self) -> i64 {
        self.takes.first().map(|t| t.started_at).unwrap_or(0)
    }
    /// Latest take end, or `None` while any take is still in progress.
    pub fn ended_at(&self) -> Option<i64> {
        if self.is_active() {
            return None;
        }
        self.takes.iter().filter_map(|t| t.ended_at).max()
    }
    pub fn total_bytes(&self) -> i64 {
        self.takes.iter().map(|t| t.bytes).sum()
    }
    /// Total captured time summed across takes.
    pub fn captured_secs(&self, now: i64) -> i64 {
        self.takes.iter().map(|t| t.duration_secs(now)).sum()
    }
    /// Resolved "missed beginning" for the stream: the first take's value when
    /// known (a from-start take that caught up makes it 0).
    pub fn lost_secs(&self) -> Option<i64> {
        self.takes.iter().find_map(|t| t.lost_secs)
    }
    /// Ad breaks across all takes of this stream.
    pub fn ad_count(&self) -> i64 {
        self.takes.iter().map(|t| t.ad_count).sum()
    }
    /// Total advertisement time (seconds) skipped across all takes.
    pub fn ad_secs(&self) -> i64 {
        self.takes.iter().map(|t| t.ad_secs).sum()
    }
    /// Title/category changes logged across all takes of this stream.
    pub fn meta_change_count(&self) -> i64 {
        self.takes.iter().map(|t| t.meta_change_count).sum()
    }
    /// Rolled-up status for the stream row.
    pub fn status(&self) -> &'static str {
        if self.is_active() {
            "recording"
        } else if self.takes.iter().any(|t| t.status == "completed") {
            "completed"
        } else if self.takes.iter().all(|t| t.status == "orphaned") {
            "orphaned"
        } else {
            "failed"
        }
    }
}

/// Gap (seconds) within which two id-less takes are treated as the same
/// interrupted broadcast (crash / retry / manual stop+restart).
pub const STREAM_CONTINUITY_GAP_SECS: i64 = 600;

/// True if take `r` continues the same broadcast as group `g`.
fn same_stream(g: &StreamGroup, r: &Recording) -> bool {
    match (&g.stream_id, &r.stream_id) {
        // A platform id is authoritative.
        (Some(a), Some(b)) => a == b,
        // Never merge an id-bearing stream with an id-less take (or vice-versa).
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => {
            // Same platform-reported (non-approx) go-live => same broadcast.
            if let (Some(a), Some(b)) = (g.went_live_at, r.went_live_at) {
                if !g.went_live_approx && !r.went_live_approx && a == b {
                    return true;
                }
            }
            // Otherwise: a new take that abuts the previous one in time is a
            // continuation; a still-active previous take is too.
            match g.takes.last().and_then(|t| t.ended_at) {
                Some(prev_end) => r.started_at - prev_end <= STREAM_CONTINUITY_GAP_SECS,
                None => true,
            }
        }
    }
}

fn stream_key(r: &Recording) -> String {
    match &r.stream_id {
        Some(id) => format!("s{}:{}", r.monitor_id, id),
        None => format!("t{}:{}", r.monitor_id, r.started_at),
    }
}

/// Group recording takes into streams. Returns newest stream first; takes within
/// a stream are oldest-first. Takes group by shared platform stream id; id-less
/// takes group by a shared platform go-live time or by abutting in time.
///
/// Caller should pass recordings for a single monitor (grouping is time-linear).
pub fn group_recordings(recordings: &[Recording]) -> Vec<StreamGroup> {
    let mut recs: Vec<&Recording> = recordings.iter().collect();
    recs.sort_by_key(|r| (r.started_at, r.id));

    let mut groups: Vec<StreamGroup> = Vec::new();
    for r in recs {
        if groups.last().is_some_and(|g| same_stream(g, r)) {
            let g = groups.last_mut().unwrap();
            if g.stream_id.is_none() {
                g.stream_id = r.stream_id.clone();
            }
            if g.went_live_at.is_none() && r.went_live_at.is_some() {
                g.went_live_at = r.went_live_at;
                g.went_live_approx = r.went_live_approx;
            }
            g.takes.push(r.clone());
        } else {
            groups.push(StreamGroup {
                key: stream_key(r),
                stream_id: r.stream_id.clone(),
                went_live_at: r.went_live_at,
                went_live_approx: r.went_live_approx,
                takes: vec![r.clone()],
            });
        }
    }
    groups.reverse(); // newest stream first
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: i64, started: i64, ended: Option<i64>, stream_id: Option<&str>) -> Recording {
        Recording {
            id,
            monitor_id: 1,
            started_at: started,
            ended_at: ended,
            status: if ended.is_some() { "completed".into() } else { "recording".into() },
            bytes: 1,
            exit_code: None,
            output_path: String::new(),
            went_live_at: None,
            went_live_approx: true,
            lost_secs: None,
            stream_id: stream_id.map(str::to_string),
            ad_count: 0,
            ad_secs: 0,
            meta_change_count: 0,
        }
    }

    #[test]
    fn groups_takes_by_platform_id() {
        // Two takes of stream "A", one of stream "B".
        let recs = vec![
            rec(1, 1000, Some(1100), Some("A")),
            rec(2, 1110, Some(2000), Some("A")),
            rec(3, 5000, Some(6000), Some("B")),
        ];
        let groups = group_recordings(&recs);
        assert_eq!(groups.len(), 2);
        // Newest first.
        assert_eq!(groups[0].stream_id.as_deref(), Some("B"));
        assert_eq!(groups[0].takes.len(), 1);
        assert_eq!(groups[1].stream_id.as_deref(), Some("A"));
        assert_eq!(groups[1].takes.len(), 2);
    }

    #[test]
    fn idless_takes_group_by_continuity_then_split_on_gap() {
        // 1 & 2 abut (crash+retry); 3 is hours later -> new stream.
        let recs = vec![
            rec(1, 1000, Some(1100), None),
            rec(2, 1150, Some(2000), None), // 50s gap -> same
            rec(3, 2000 + STREAM_CONTINUITY_GAP_SECS + 60, Some(9999), None), // beyond gap -> new
        ];
        let groups = group_recordings(&recs);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].takes.len(), 2); // oldest group = the two abutting takes
        assert_eq!(groups[0].takes.len(), 1);
    }

    #[test]
    fn idless_does_not_merge_into_id_stream() {
        let recs = vec![
            rec(1, 1000, Some(1100), Some("A")),
            rec(2, 1150, Some(2000), None), // abuts in time but has no id -> separate
        ];
        let groups = group_recordings(&recs);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn active_take_makes_stream_active_and_open_ended() {
        let recs = vec![rec(1, 1000, None, Some("A"))];
        let groups = group_recordings(&recs);
        assert!(groups[0].is_active());
        assert_eq!(groups[0].ended_at(), None);
    }
}
