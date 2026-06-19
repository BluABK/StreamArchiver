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

/// A monitored channel/link.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub platform: Platform,
    pub created_at: i64,
}

/// One capture instance for a channel (tool + quality + detection + output).
#[derive(Clone, Debug)]
pub struct Monitor {
    pub id: i64,
    pub channel_id: i64,
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
    /// Per-channel auth override for the downloader (Inherit = use global).
    pub auth_kind: AuthKind,
    /// Value for `auth_kind`: browser name, cookies.txt path, or token.
    pub auth_value: String,
    pub extra_args: String,
    pub max_concurrent: i64,
    pub last_checked_at: Option<i64>,
    pub last_state: String,
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

/// Current unix timestamp in seconds.
pub fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
