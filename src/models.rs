//! Domain types for channels, monitors (per-tool instances), and recordings.
//!
//! The data model is `channel -> monitor -> recording`. "Multiple instances of
//! the same channel" is expressed as multiple [`Monitor`] rows pointing at one
//! [`Channel`].

use serde::{Deserialize, Serialize};

/// Streaming platform a channel belongs to. Drives default detection/tool choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// Parse a stored "preferred asset platform": an empty/unknown string means
    /// `None` ("auto — first available"). `Generic` is not an asset source, so it
    /// is never a valid preference and also maps to `None`.
    pub fn parse_opt(s: &str) -> Option<Platform> {
        match s {
            "twitch" => Some(Platform::Twitch),
            "youtube" => Some(Platform::YouTube),
            "kick" => Some(Platform::Kick),
            _ => None,
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
                DetectionMethod::WebSubOnly,
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
    /// Like `WebSub` but with **no polling fallback**: only push notifications
    /// trigger recording. Zero scheduled polls — ideal for monitoring channels
    /// with auto=off, or for any YouTube channel where push reliability is
    /// trusted and reducing HTTP traffic is a priority.
    WebSubOnly,
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
            DetectionMethod::WebSubOnly => "websub_only",
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
            DetectionMethod::WebSubOnly => "YouTube WebSub (push only)",
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
            DetectionMethod::WebSubOnly => "WebSub!",
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
            "websub_only" => DetectionMethod::WebSubOnly,
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
            DetectionMethod::WebSubOnly => {
                "Like WebSub but with zero polling: only push notifications from the yt-websub \
                 VPS relay trigger recording. No scheduled HTTP polls at all — good for channels \
                 with Auto off (info-only) or when push reliability is trusted and you want to \
                 reduce traffic. Set the VPS URL + token in Settings."
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
    /// Optional custom hex color for this channel (e.g. `"#ff9800"`).
    /// Empty string means "use the automatic palette color".
    pub color: String,
    /// Which platform's profile pic / banner represents this container (a
    /// container can hold the same creator on Twitch + YouTube + Kick, each with
    /// its own assets). `None` = auto: the first instance-platform that has a
    /// fetched icon. Set explicitly via the channel's Properties → icon source.
    pub preferred_platform: Option<Platform>,
    /// Channel-level enabled flag. Independent from each instance's `Monitor::enabled`;
    /// a monitor runs only when both this AND `monitor.enabled` are true.
    pub enabled: bool,
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
    /// Dual capture (YouTube only): in addition to the primary capture, run a
    /// second concurrent DASH process (system yt-dlp, live edge) when wanted
    /// formats span both SABR and DASH. Produces a second recording in the same
    /// take. See the SABR settings + the DASH companion format selector.
    pub dual_capture: bool,
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
    /// Download the stream thumbnail at recording start as `{stem}.thumbnail.jpg`.
    /// For yt-dlp, passes `--write-thumbnail`; for other tools fetches the URL
    /// reported by the platform API.
    pub fetch_thumbnail: bool,
    /// Use the stream thumbnail (when fetched) as the hero image in the
    /// recording-started desktop notification, instead of the channel's static
    /// banner. Useful for YouTube where each stream has a unique thumbnail.
    pub thumbnail_in_toast: bool,
    /// Download channel icon, banner, badges, and emotes (BTTV/FFZ/7TV for Twitch)
    /// into `{output_dir}/channel_assets/{name}/`. Refreshed at most once per 24h.
    pub fetch_chat_assets: bool,
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
    /// Current (latest-logged) stream title of the latest recording take — the
    /// most recent `title` [`StreamMetaChange`]. Empty when none was logged (only
    /// Twitch metadata is polled). Drives the Title column.
    pub last_recording_title: String,
    /// Current (latest-logged) game/category of the latest recording take — the
    /// most recent `category` [`StreamMetaChange`]. Empty when none. Drives the
    /// Game column.
    pub last_recording_category: String,
    /// Captured stderr tail of the latest recording (the `log_excerpt`), used to
    /// show *why* it failed on hover. Empty when there's no recording yet.
    pub last_recording_log: String,
    /// Cached auto Twitch-sub ad-free status: `None` unknown/not checked,
    /// `Some(false)` checked & not subscribed, `Some(true)` subscribed. Combined
    /// with `monitor.ad_free` (the manual flag) for the Ad-free column. (The
    /// refresher tracks the last-checked time via a separate lightweight query.)
    pub ad_free_sub: Option<bool>,
    /// Total recording takes for this monitor (drives the history-tree disclosure
    /// without loading the full history until a row is expanded).
    pub recording_count: i64,
    /// Start time of the next upcoming scheduled stream (soonest future,
    /// non-canceled [`ScheduleSegment`]), or `None` when none is known. Twitch
    /// (Helix schedule) + YouTube (scraped upcoming streams); drives the Next
    /// stream column. Filled by the UI from a separate lookup, not the row query.
    pub next_stream_at: Option<i64>,
    /// Title of that next scheduled stream (for the cell hover); empty when none.
    pub next_stream_title: String,
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

/// Which kind of download a [`DetachedRow`] tracks. The registry is shared across
/// the three process types the supervisor spawns so one startup sweep can
/// reconcile them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachedKind {
    /// A live stream capture (`recording` row).
    Recording,
    /// An on-demand video download (`video` row).
    Video,
    /// A live-chat sidecar (keyed by `monitor_id`, no own row).
    Chat,
}

impl DetachedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DetachedKind::Recording => "recording",
            DetachedKind::Video => "video",
            DetachedKind::Chat => "chat",
        }
    }
    pub fn from_str(s: &str) -> Option<DetachedKind> {
        match s {
            "recording" => Some(DetachedKind::Recording),
            "video" => Some(DetachedKind::Video),
            "chat" => Some(DetachedKind::Chat),
            _ => None,
        }
    }
}

/// The kind of media/content an artifact represents, used to pick a UI icon +
/// label (currently the Processes window's Type column). Kept separate from
/// [`DetachedKind`] — which is the *process* role and drives DB serialization —
/// so the icon vocabulary can grow to cover content we don't yet spawn a
/// dedicated process for (audio-only tracks, subtitle sidecars, thumbnails…)
/// without touching the registry schema.
///
/// IMPORTANT — glyph constraint: egui draws only the monochrome glyph OUTLINES
/// from its bundled font subset (NotoEmoji-Regular.ttf, an 887-glyph subset). An
/// emoji absent from that subset renders as a blank "tofu" box. Every glyph
/// below was verified present in the subset. Notably 🖼 (U+1F5BC framed picture)
/// is NOT in it — `Image` uses 📷 (camera) instead. When adding a variant, pick a
/// glyph confirmed in the subset (e.g. via fontTools) or it will render as tofu.
///
/// `Audio`/`Subtitles`/`Image`/`Metadata` aren't wired to a process role yet —
/// they're the "all sorts of possible types" the Type column should be ready to
/// show, hence `#[allow(dead_code)]` until something constructs them.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentType {
    /// A live stream capture / video stream in progress.
    Video,
    /// An on-demand (already-finished) video download.
    Vod,
    /// An audio-only stream or extracted audio track.
    Audio,
    /// A live-chat sidecar / chat log.
    Chat,
    /// A subtitle / caption track.
    Subtitles,
    /// A thumbnail, poster, or other still image.
    Image,
    /// A metadata sidecar (info JSON, description, etc.).
    Metadata,
}

impl ContentType {
    /// Font-safe emoji glyph (verified present in egui's NotoEmoji subset).
    pub fn icon(self) -> &'static str {
        match self {
            ContentType::Video => "🎥",
            ContentType::Vod => "📼",
            ContentType::Audio => "🎵",
            ContentType::Chat => "💬",
            ContentType::Subtitles => "🔤",
            ContentType::Image => "📷",
            ContentType::Metadata => "📄",
        }
    }

    /// Short human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            ContentType::Video => "video",
            ContentType::Vod => "VOD",
            ContentType::Audio => "audio",
            ContentType::Chat => "chat",
            ContentType::Subtitles => "subtitles",
            ContentType::Image => "image",
            ContentType::Metadata => "metadata",
        }
    }

    /// `"🎥 video"` — icon + space + label, ready to hand to `ui.label`.
    pub fn tag(self) -> String {
        format!("{} {}", self.icon(), self.label())
    }
}

/// A persisted record of a still-running download tool process, written right
/// after the tool spawns and deleted at finalize/stop. On the next launch the
/// supervisor reconciles every row: re-attach to the live ones (wait + tail the
/// log), finalize ones that finished while the app was down, or orphan the rest.
/// `pid` + `proc_start` (process creation time) make adoption PID-reuse-safe.
#[derive(Clone, Debug)]
pub struct DetachedRow {
    pub kind: DetachedKind,
    /// `recording.id` / `video.id` / `monitor_id` (chat).
    pub ref_id: i64,
    pub monitor_id: Option<i64>,
    pub pid: u32,
    /// OS process creation time (FILETIME 100ns ticks); guards against PID reuse.
    pub proc_start: u64,
    /// Named Win32 job object the tool was assigned to (re-openable by name).
    pub job_name: String,
    /// The tool's combined stdout/stderr log, tailed for progress/ads/log-tail.
    pub log_path: String,
    /// The working capture file under `.cache\`.
    pub capture_path: String,
    /// The final promoted path in the output dir.
    pub final_path: String,
    pub remux_to_mkv: bool,
    pub take_group: Option<String>,
    /// [`crate::version::build_id`] of the build that spawned the tool.
    pub spawn_build: String,
    /// The recording's/video's start time (unix secs) — the take timeline anchor,
    /// not the registration time.
    pub started_at: i64,
    /// True only for the DASH companion leg of a dual capture (it occupies the
    /// secondary active map). Lets re-attach assign the right slot deterministically
    /// instead of guessing from registry row order.
    pub secondary: bool,
    /// Platform stream/video id (for the `{video_id}` filename var on re-attach).
    pub stream_id: Option<String>,
    /// Broadcast go-live time, if known (ad-cut anchor + lost-time accounting).
    pub went_live_at: Option<i64>,
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

/// One upcoming scheduled stream for a monitor — a Twitch schedule segment or a
/// YouTube upcoming livestream. Refreshed periodically and stored in
/// `schedule_segment`; drives the Next stream column + popup. `start_time` is unix
/// seconds; `end_time` is set by Twitch (its segments are bounded) but not by
/// YouTube. `category`/`end_time` may be empty/None depending on the source.
/// Fetchers leave `id`/`monitor_id` at 0 (the store assigns them on insert).
///
/// `Serialize`/`Deserialize` let the community-post archive cache decoded events
/// as `decoded_json`, so an unchanged post image hits the archive instead of
/// re-running OCR. See [`crate::store`]'s `community_post_archive`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ScheduleSegment {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub monitor_id: i64,
    pub start_time: i64,
    pub end_time: Option<i64>,
    pub title: String,
    pub category: String,
    /// A specific occurrence the broadcaster canceled (Twitch). Excluded from the
    /// Next stream column + popup.
    pub canceled: bool,
    /// Platform video ID (YouTube: `dQw4w9WgXcQ`). Populated by the lockupViewModel
    /// scraper so we can batch `videos.list` calls for exact scheduled times.
    /// `None` for Twitch, Discord, and old YouTube rows.
    pub video_id: Option<String>,
}

/// One upcoming scheduled stream joined with its channel + monitor, for the
/// Schedule calendar. Flattens a [`ScheduleSegment`] with the channel name and
/// the monitor's source URL so the calendar can show who streams when and offer
/// the URL/platform on a right-click. `platform` is derived from [`Self::url`].
#[derive(Clone, Debug)]
pub struct UpcomingStream {
    /// `schedule_segment.id` — the stable DB primary key of this occurrence, so
    /// the calendar can edit / delete / open-source a specific row.
    pub segment_id: i64,
    // Carried from the join for a future "jump to this monitor" action; the
    // calendar itself filters/derives everything from the channel + URL.
    pub monitor_id: i64,
    pub channel_id: i64,
    pub channel_name: String,
    /// The monitor's source URL (right-click → copy; platform derived from it).
    pub url: String,
    pub start_time: i64,
    pub end_time: Option<i64>,
    pub title: String,
    pub category: String,
    /// Where this segment came from — the stable `schedule_segment.source` id:
    /// `"platform"` (Twitch schedule), `"youtube"`/`"youtube_api"`, an OCR source
    /// (`"twitch_banner_ocr"`, …), `"discord"`, or `"manual"` (a user-edited row,
    /// protected from automatic-refresh overwrites). See
    /// [`crate::schedule_source::source_badge`] for the per-source badge.
    pub source: String,
    /// Custom hex color for this channel (mirrors `Channel::color`). Empty
    /// means "use the automatic palette color".
    pub channel_color: String,
    /// For manual merges: the `segment_id` of the primary this segment was merged
    /// into. `None` = standalone or IS the primary. Segments with a non-None value
    /// are hidden from the calendar in favor of their primary.
    pub merged_into: Option<i64>,
    /// When true, this segment is excluded from automatic time-overlap merge
    /// grouping with same-channel events.
    pub auto_merge_excluded: bool,
}

impl UpcomingStream {
    /// Platform inferred from the source URL.
    pub fn platform(&self) -> Platform {
        Platform::detect(&self.url)
    }

    /// Whether this segment was manually added/corrected by the user. Manual rows
    /// are protected from automatic-refresh overwrites (see
    /// [`crate::store::Store::clear_other_schedule_sources`]).
    pub fn is_manual(&self) -> bool {
        self.source == "manual"
    }
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
    /// Audio tracks to capture. Empty = the tool's default; `all`/`*` = every
    /// track; otherwise a comma-separated list. Honored by streamlink
    /// (`--hls-audio-select`); yt-dlp/ffmpeg keep what the chosen format carries.
    pub audio_tracks: String,
    /// Subtitle tracks to write as sidecars. Empty = none; `all` = every subtitle;
    /// otherwise a comma-separated language list. Honored by yt-dlp (`--sub-langs`
    /// + `--write-subs`); streamlink can't mux subtitles.
    pub subtitle_tracks: String,
    /// Capture chat to a sidecar (yt-dlp `--sub-langs live_chat` → a
    /// `.live_chat.json`, e.g. a YouTube VOD's chat replay). Other tools/sources
    /// without a chat track simply produce none.
    pub chat_log: bool,
    pub extra_args: String,
    /// Resolve the real stream/video title at download time (via yt-dlp) and use
    /// it for `{title}` (and `{name}` when no Name is set).
    pub auto_title: bool,
    pub status: String,
    pub output_path: String,
    pub bytes: i64,
    pub created_at: i64,
    /// Process exit code of the last run (`None` until it finishes); shown with
    /// the failure reason on hover of a failed row.
    pub exit_code: Option<i64>,
    /// Captured stderr tail of the last run — the failure reason for a failed
    /// download. Empty until/unless it failed.
    pub log_excerpt: String,
    // Persisted run metadata, round-tripped through the store but not yet shown
    // in the UI (kept so the data is available without a schema/struct change).
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
    #[serde(default)]
    pub auto_title: bool,
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
            auto_title: false,
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

/// Per-platform (or global) monitor-creation defaults stored as part of
/// [`MonitorDefaults`]. Every field is `Option<T>` — `None` means "not set;
/// inherit from the global defaults (or the built-in hardcoded fallback)."
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlatformMonitorDefault {
    pub tool: Option<Tool>,
    pub detection_method: Option<DetectionMethod>,
    pub container: Option<Container>,
    /// `None` or empty string = use the hardcoded/global default (`"best"`).
    #[serde(default)]
    pub quality: Option<String>,
    /// `None` = inherit (global default is 60 s).
    pub poll_interval_secs: Option<i64>,
    /// `None` or empty string = use the hardcoded/global default.
    #[serde(default)]
    pub filename_template: Option<String>,
    /// `None` or empty string = use the global `default_output_dir`.
    #[serde(default)]
    pub output_dir: Option<String>,
    /// `None` = inherit; `Some(true/false)` = capture from start on/off.
    #[serde(default)]
    pub from_start: Option<bool>,
}

/// Global + per-platform monitor-creation defaults, persisted as JSON in
/// `app_settings` under [`K_MONITOR_DEFAULTS`].
///
/// Resolution order when creating a monitor for platform P:
///   platform-specific value → global value → hardcoded fallback
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MonitorDefaults {
    pub global: PlatformMonitorDefault,
    pub twitch: PlatformMonitorDefault,
    pub youtube: PlatformMonitorDefault,
    pub kick: PlatformMonitorDefault,
    pub generic: PlatformMonitorDefault,
}

impl MonitorDefaults {
    pub fn get(&self, platform: Platform) -> &PlatformMonitorDefault {
        match platform {
            Platform::Twitch => &self.twitch,
            Platform::YouTube => &self.youtube,
            Platform::Kick => &self.kick,
            Platform::Generic => &self.generic,
        }
    }

    pub fn get_mut(&mut self, platform: Platform) -> &mut PlatformMonitorDefault {
        match platform {
            Platform::Twitch => &mut self.twitch,
            Platform::YouTube => &mut self.youtube,
            Platform::Kick => &mut self.kick,
            Platform::Generic => &mut self.generic,
        }
    }

    /// Resolve tool: platform override → global → hardcoded platform default.
    pub fn resolve_tool(&self, platform: Platform) -> Tool {
        self.get(platform)
            .tool
            .or(self.global.tool)
            .unwrap_or_else(|| platform.default_tool())
    }

    /// Resolve detection method: platform override → global → hardcoded platform default.
    pub fn resolve_detection(&self, platform: Platform) -> DetectionMethod {
        self.get(platform)
            .detection_method
            .or(self.global.detection_method)
            .unwrap_or_else(|| platform.default_detection())
    }

    /// Resolve container: platform override → global → MKV.
    pub fn resolve_container(&self, platform: Platform) -> Container {
        self.get(platform)
            .container
            .or(self.global.container)
            .unwrap_or(Container::Mkv)
    }

    /// Resolve quality string: platform override → global → `"best"`.
    pub fn resolve_quality(&self, platform: Platform) -> String {
        self.get(platform)
            .quality
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.global.quality.clone().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "best".to_string())
    }

    /// Resolve poll interval: platform override → global → 60 s.
    pub fn resolve_poll_interval(&self, platform: Platform) -> i64 {
        self.get(platform)
            .poll_interval_secs
            .or(self.global.poll_interval_secs)
            .unwrap_or(60)
    }

    /// Resolve filename template: platform override → global → `"{name}_{date}_{time}"`.
    pub fn resolve_filename_template(&self, platform: Platform) -> String {
        self.get(platform)
            .filename_template
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.global
                    .filename_template
                    .clone()
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "{name}_{date}_{time}".to_string())
    }

    /// Resolve capture-from-start: platform override → global → `true`.
    pub fn resolve_from_start(&self, platform: Platform) -> bool {
        self.get(platform)
            .from_start
            .or(self.global.from_start)
            .unwrap_or(true)
    }

    /// Resolve output dir: platform override → global → `fallback`.
    pub fn resolve_output_dir(&self, platform: Platform, fallback: &str) -> String {
        self.get(platform)
            .output_dir
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.global.output_dir.clone().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| fallback.to_string())
    }
}

/// `app_settings` key under which [`MonitorDefaults`] is persisted as JSON.
pub const K_MONITOR_DEFAULTS: &str = "monitor_defaults";

/// `app_settings` key for the global [`MediaInfoMode`] (filename media probing).
pub const K_FILENAME_MEDIA: &str = "filename_media_info";

/// `app_settings` keys for opting individual YouTube operations into the Data API
/// (instead of scraping). Each is gated on a non-empty `youtube_api_key`.
pub const K_YT_API_DETECT: &str = "youtube_api_detect";
pub const K_YT_API_SCHEDULE: &str = "youtube_api_schedule";

/// `app_settings` keys for importing stream schedules from Discord scheduled
/// events. `K_DISCORD_TOKEN` is the user's Discord token (sent raw in the
/// Authorization header — automating it is against Discord's ToS); the import is
/// gated on `K_DISCORD_SCHEDULE == "1"` and a non-empty token.
pub const K_DISCORD_TOKEN: &str = "discord_user_token";
pub const K_DISCORD_SCHEDULE: &str = "discord_schedule";

/// `app_settings` key for the ordered list of schedule sources (JSON
/// `Vec<SourceEntry>`); see [`crate::schedule_source`]. Each entry is a stable
/// source id + an enabled flag; the schedule refresh walks enabled entries
/// top-to-bottom per channel and the first to resolve a non-empty schedule wins.
pub const K_SCHEDULE_SOURCES: &str = "schedule_sources";

/// `app_settings` key for every grid table's persisted column order/visibility
/// (JSON `{table_key -> Vec<ColumnEntry>}`); see [`crate::grid_columns`]. One
/// flat map (not one key per table) so the Settings reset buttons can update
/// every table atomically in a single row.
pub const K_GRID_COLUMNS: &str = "grid_columns_v1";

/// `app_settings` key for every grid table's persisted sort (JSON
/// `{table_key -> PersistedSort}`); see [`crate::grid_columns`].
pub const K_GRID_SORT: &str = "grid_sort_v1";

/// `app_settings` key for per-channel schedule-source config (JSON map
/// `{channel_id -> ChannelSourceConfig}`): Twitter/X handle, a manual schedule
/// image path/URL, and per-channel OCR overrides. See [`crate::schedule_source`].
pub const K_CHANNEL_SOURCE_CFG: &str = "channel_source_config";

/// `app_settings` keys for the image→schedule OCR pipeline ([`crate::schedule_ocr`]),
/// which shells out to an LLM CLI (default the `claude` CLI). `K_OCR_COMMAND` is
/// the executable; `K_OCR_MODEL`/`K_OCR_FALLBACK_MODEL` are the primary + retry
/// models; `K_OCR_TIMEZONE`/`K_OCR_OFFSET` are the IANA timezone + UTC offset to
/// assume for banner times (banners rarely show either). Empty = built-in default.
pub const K_OCR_COMMAND: &str = "ocr_command";
pub const K_OCR_MODEL: &str = "ocr_model";
pub const K_OCR_FALLBACK_MODEL: &str = "ocr_fallback_model";
pub const K_OCR_TIMEZONE: &str = "ocr_timezone";
pub const K_OCR_OFFSET: &str = "ocr_offset";
/// Per-call USD budget ceiling passed as `--max-budget-usd` (empty = no limit).
pub const K_OCR_MAX_BUDGET: &str = "ocr_max_budget_usd";
/// Process timeout in seconds for one `claude` CLI invocation (empty/0 = default 150).
pub const K_OCR_TIMEOUT_SECS: &str = "ocr_timeout_secs";
/// Effort level passed as `--effort` (empty = omit flag; valid: low/medium/high/xhigh/max).
pub const K_OCR_EFFORT: &str = "ocr_effort";
/// Cumulative OCR call stats (JSON blob).
pub const K_OCR_STATS: &str = "ocr_stats";
/// Persistent OCR image-hash cache: JSON `{"<monitor_id>:<source_id>": <fnv64_hash>}`.
/// Populated after every successful OCR run so a restart doesn't re-run OCR on
/// an unchanged banner/community-post image.
pub const K_OCR_IMAGE_HASHES: &str = "ocr_image_hashes";

/// Persistent OCR re-check cadence stamps: JSON `{"<monitor_id>:<source_id>":
/// <unix_secs>}` recording when each OCR source was last consulted. Survives
/// restarts so the slow OCR cadence is enforced across launches, not just within
/// one session — a rebuild/restart can't trigger a fresh re-OCR sweep.
pub const K_OCR_LAST_ATTEMPT: &str = "ocr_last_attempt";

/// `app_settings` key for the last successful schedule-fetch timestamp per
/// monitor, persisted as a JSON `HashMap<String, i64>` (monitor_id → unix_secs).
/// Loaded at startup so the 6-hour staleness window survives app restarts —
/// without this, every restart triggers a full re-fetch of all YouTube API
/// schedule channels, burning expensive `search.list` quota.
pub const K_SCHEDULE_LAST_FETCHED: &str = "schedule_last_fetched";

/// `app_settings` key for the global "go to the next schedule source when an
/// event has no title" toggle (`"1"` = on). When on, the schedule walk keeps
/// querying lower-priority sources after a winner is found, to fill in blank
/// titles on the winning source's events (e.g. a Twitch schedule that publishes
/// times but no titles gets its titles from a banner / community-post OCR source).
/// Per-channel and per-monitor overrides live in the schedule-scope config.
/// See [`crate::schedule_source`].
pub const K_SCHEDULE_TITLE_FILL: &str = "schedule_title_fill";

/// `app_settings` key for how many recent YouTube community posts to scan for a
/// schedule image (the OCR backlog depth). Empty/0 = built-in default (5). A
/// per-channel override lives in `ChannelSourceConfig.max_community_posts`.
pub const K_YT_COMMUNITY_MAX_POSTS: &str = "youtube_community_max_posts";

/// `app_settings` keys for per-channel / per-monitor schedule-source *scope*
/// config (JSON maps `{id -> SourceScopeConfig}`): an optional source-order
/// override (replacing the global order for that channel/monitor) and an optional
/// title-fill override. Precedence: monitor over channel over global. See
/// [`crate::schedule_source`].
pub const K_CHANNEL_SCOPE_CFG: &str = "channel_schedule_scope";
pub const K_MONITOR_SCOPE_CFG: &str = "monitor_schedule_scope";

/// `app_settings` key for an absolute path to a PNG file used as the main icon
/// in crash and freeze dialogs. Empty = standard Windows error/warning icon.
pub const K_DIALOG_ICON: &str = "dialog_icon";

// ---------- Remux embedding options ----------

/// `app_settings` key — embed the thumbnail sidecar as MKV cover art on remux.
pub const K_REMUX_EMBED_THUMBNAIL: &str = "remux_embed_thumbnail";
/// `app_settings` key — embed a title metadata tag in the MKV on remux.
pub const K_REMUX_EMBED_TITLE: &str = "remux_embed_title";
/// `app_settings` key — filename-template used to generate the MKV title tag.
/// Supports the same tokens as the capture filename template (`{title}`, `{channel}`, …).
/// Default when empty: `"{title}"`.
pub const K_REMUX_TITLE_TEMPLATE: &str = "remux_title_template";
/// `app_settings` key — embed subtitle sidecar files (`.srt`/`.ass`/`.vtt`) into the MKV.
pub const K_REMUX_EMBED_SUBS: &str = "remux_embed_subs";

// ---------- File management / subdirectory splitting ----------

/// `app_settings` key — split output files into per-type subdirectories.
pub const K_FILE_SPLIT_ENABLED: &str = "file_split_enabled";
/// `app_settings` key — subdirectory name for video files (default `"videos"`).
pub const K_FILE_SPLIT_VIDEOS: &str = "file_split_videos";
/// `app_settings` key — subdirectory name for subtitle sidecars (default `"subs"`).
pub const K_FILE_SPLIT_SUBS: &str = "file_split_subs";
/// `app_settings` key — subdirectory name for chat logs (default `"chat"`).
pub const K_FILE_SPLIT_CHAT: &str = "file_split_chat";
/// `app_settings` key — subdirectory name for thumbnail sidecars (default `"thumbs"`).
pub const K_FILE_SPLIT_THUMBS: &str = "file_split_thumbs";
/// `app_settings` key — subdirectory name for process log files (default `"logs"`).
pub const K_FILE_SPLIT_LOGS: &str = "file_split_logs";
/// `app_settings` key — YouTube Data API daily quota cutoff (integer, units).
/// When set and today's usage meets or exceeds this value, API calls are skipped
/// to protect the remaining quota. Default (when absent) = 9000.
pub const K_YT_API_QUOTA_CUTOFF: &str = "youtube_api_quota_cutoff";
/// `app_settings` key — YouTube search.list daily query cutoff (integer, queries).
/// Default 90 (out of 100 free-tier queries/day). Triggers a dismissable warning
/// in the Issues panel when exceeded.
pub const K_YT_SEARCH_QUOTA_CUTOFF: &str = "youtube_search_quota_cutoff";

/// Cumulative statistics for all `claude` CLI OCR invocations.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct OcrStats {
    /// Successful CLI calls (got a result back, regardless of parse outcome).
    pub calls: u64,
    /// Times the CLI itself failed (bad exit, timeout, spawn error).
    pub cli_failures: u64,
    /// Times the CLI succeeded but the output couldn't be parsed as schedule JSON.
    pub parse_failures: u64,
    /// OCR skipped because the source image was byte-identical to the last run.
    pub cache_hits: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_usd: f64,
    /// Unix timestamp of the most recent CLI call.
    pub last_call_at: Option<i64>,
    pub by_model: std::collections::HashMap<String, OcrModelStats>,
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct OcrModelStats {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

/// Controls what gets embedded into the MKV container during a TS→MKV remux.
#[derive(Clone, Debug)]
pub struct RemuxOpts {
    /// Attach a thumbnail sidecar as cover art (image/jpeg or image/png).
    pub embed_thumbnail: bool,
    /// Write a `title` metadata tag. `title_template` must be non-empty.
    pub embed_title: bool,
    /// Template string for the title tag; same token set as the capture filename template.
    /// `"{title}"` is the recommended default.
    pub title_template: String,
    /// Copy subtitle sidecar files (`.srt`/`.ass`/`.vtt`) as subtitle streams.
    pub embed_subs: bool,
}

impl Default for RemuxOpts {
    fn default() -> Self {
        RemuxOpts {
            embed_thumbnail: true,
            embed_title: false,
            title_template: "{title}".into(),
            embed_subs: false,
        }
    }
}

/// Configuration for splitting captured files into per-type subdirectories under
/// each monitor's output directory (e.g. `output_dir/videos/`, `output_dir/chat/`, …).
#[derive(Clone, Debug)]
pub struct SubdirConfig {
    pub enabled: bool,
    pub videos: String,
    pub subs: String,
    pub chat: String,
    pub thumbs: String,
    pub logs: String,
}

impl Default for SubdirConfig {
    fn default() -> Self {
        SubdirConfig {
            enabled: false,
            videos: "videos".into(),
            subs: "subs".into(),
            chat: "chat".into(),
            thumbs: "thumbs".into(),
            logs: "logs".into(),
        }
    }
}

/// Aggregate counts/sizes across the whole DB — populated on demand for the Stats view.
#[derive(Default, Clone)]
pub struct GlobalStats {
    pub total_recordings: i64,
    pub total_bytes: i64,
    pub active_monitors: i64,
    pub total_monitors: i64,
    pub upcoming_segments: i64,
    pub total_channels: i64,
}

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
    /// Groups the recordings of one capture attempt (a "take"). Dual capture
    /// produces two recordings — a SABR primary and a DASH companion — that share
    /// this key. `None` for legacy/single recordings (each is its own take).
    pub take_group: Option<String>,
    /// Ad breaks detected during this take (count + total seconds). Each break is
    /// a hard cut in the finished file; the per-break offsets live in `ad_break`.
    pub ad_count: i64,
    pub ad_secs: i64,
    /// Title + game/category changes logged during this take; per-change rows live
    /// in `stream_meta_change`.
    pub meta_change_count: i64,
    /// Current (latest-logged) title/game for this take — the most recent `title`
    /// / `category` [`StreamMetaChange`]. Empty when none was logged.
    pub title: String,
    pub category: String,
    /// Captured stderr tail (`log_excerpt`) — the failure reason for a failed take.
    pub log_excerpt: String,
    /// User-entered free-text notes for this take (empty by default).
    pub notes: String,
    /// Twitch VOD ID once resolved (e.g. `"1234567890"`). `None` until the
    /// background checker confirms it, or for non-Twitch recordings.
    pub vod_id: Option<String>,
    /// Twitch VOD availability state. `None` = not tracked (non-Twitch / pre-v30
    /// row). `"pending"` = background check running; `"found"` = VOD published;
    /// `"not_published"` = streamer did not publish a VOD (local copy may be the
    /// only surviving record).
    pub vod_state: Option<String>,
    /// Total DMCA-muted seconds in the Twitch VOD. `None` = unknown (pending or
    /// N/A). `0` = clean copy online; `> 0` = damaged — the online copy is missing
    /// content, making the local recording the authoritative archive.
    pub vod_muted_secs: Option<i64>,
}

impl Recording {
    pub fn is_active(&self) -> bool {
        self.status == "recording"
    }

    /// Direct URL to the Twitch VOD (`None` until `vod_state == "found"`).
    pub fn vod_url(&self) -> Option<String> {
        self.vod_id.as_ref().map(|id| format!("https://www.twitch.tv/videos/{id}"))
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
    /// Current title for the stream: the newest take's logged title (takes are
    /// oldest-first, so the last one), or empty if none. Deliberately does NOT
    /// fall back to an older take when the newest is empty — that would show a
    /// stale value and disagree with the instance/channel rows, which read only
    /// the latest take. An emptied/cleared current title should read as empty.
    pub fn title(&self) -> &str {
        self.takes.last().map(|t| t.title.as_str()).unwrap_or("")
    }
    /// Current game/category for the stream (see [`StreamGroup::title`]).
    pub fn category(&self) -> &str {
        self.takes.last().map(|t| t.category.as_str()).unwrap_or("")
    }
    /// Rolled-up status for the stream row.
    pub fn status(&self) -> &'static str {
        if self.is_active() {
            "recording"
        } else if self.takes.iter().any(|t| t.status == "completed") {
            "completed"
        } else if self.takes.iter().any(|t| t.status == "aborted") {
            // All non-completed takes were cut short by app shutdown.
            "aborted"
        } else if self.takes.iter().all(|t| t.status == "orphaned") {
            "orphaned"
        } else if self
            .takes
            .iter()
            .all(|t| matches!(t.status.as_str(), "ended" | "orphaned"))
        {
            // No footage, but the broadcast had simply ended / wasn't live when we
            // tried — not a failure (see the downloader's `ended` classification).
            "ended"
        } else {
            "failed"
        }
    }

    /// Cluster the takes into capture attempts: recordings that share a non-null
    /// `take_group` (a dual SABR+DASH capture) are grouped together; `None` keys
    /// and distinct keys are their own singletons. Preserves take order (oldest
    /// first). The UI renders one take per inner vec.
    pub fn take_groups(&self) -> Vec<Vec<&Recording>> {
        let mut out: Vec<Vec<&Recording>> = Vec::new();
        for r in &self.takes {
            if let Some(key) = &r.take_group {
                if let Some(grp) = out
                    .iter_mut()
                    .find(|g| g.first().and_then(|f| f.take_group.as_ref()) == Some(key))
                {
                    grp.push(r);
                    continue;
                }
            }
            out.push(vec![r]);
        }
        out
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
            take_group: None,
            ad_count: 0,
            ad_secs: 0,
            meta_change_count: 0,
            title: String::new(),
            category: String::new(),
            log_excerpt: String::new(),
            notes: String::new(),
            vod_id: None,
            vod_state: None,
            vod_muted_secs: None,
        }
    }

    /// Build a finished take with a specific status (and no footage).
    fn take_with_status(id: i64, status: &str) -> Recording {
        let mut t = rec(id, 1000, Some(1100), None);
        t.status = status.into();
        t.bytes = 0;
        t
    }

    fn group_of(takes: Vec<Recording>) -> StreamGroup {
        StreamGroup {
            key: "k".into(),
            stream_id: None,
            went_live_at: None,
            went_live_approx: true,
            takes,
        }
    }

    #[test]
    fn take_groups_cluster_dual_recordings() {
        // Two recordings sharing a take_group (SABR + DASH) form one take; a
        // recording with a different take_group is its own take.
        let mut a = rec(1, 1000, Some(1100), Some("s1"));
        a.take_group = Some("m:1000".into());
        a.output_path = "stream.mkv".into();
        let mut b = rec(2, 1000, Some(1100), Some("s1"));
        b.take_group = Some("m:1000".into());
        b.output_path = "stream.dash.mkv".into();
        let mut c = rec(3, 2000, Some(2100), Some("s1"));
        c.take_group = Some("m:2000".into());

        let g = group_of(vec![a, b, c]);
        let groups = g.take_groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2); // SABR + DASH clustered together
        assert_eq!(groups[1].len(), 1); // the later take stands alone
        // The pair preserves order (SABR primary first, DASH companion second).
        assert_eq!(groups[0][0].id, 1);
        assert_eq!(groups[0][1].id, 2);
    }

    #[test]
    fn ended_takes_dont_roll_up_to_failed() {
        // A broadcast that had already ended when we tried = `ended`, not `failed`.
        assert_eq!(group_of(vec![take_with_status(1, "ended")]).status(), "ended");
        // ended + a crash-orphan (both footage-less, benign) still reads as ended.
        assert_eq!(
            group_of(vec![
                take_with_status(1, "ended"),
                take_with_status(2, "orphaned"),
            ])
            .status(),
            "ended",
        );
        // A real failure anywhere in the broadcast keeps it `failed`.
        assert_eq!(
            group_of(vec![
                take_with_status(1, "ended"),
                take_with_status(2, "failed"),
            ])
            .status(),
            "failed",
        );
        // A completed take wins regardless.
        assert_eq!(
            group_of(vec![
                take_with_status(1, "ended"),
                rec(2, 2000, Some(2100), None), // completed (bytes=1)
            ])
            .status(),
            "completed",
        );
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
