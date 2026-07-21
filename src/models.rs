//! Domain types for channels, monitors (per-tool instances), and recordings.
//!
//! The data model is `channel -> monitor -> recording`. "Multiple instances of
//! the same channel" is expressed as multiple [`Monitor`] rows pointing at one
//! [`Channel`].

use serde::{Deserialize, Serialize};

/// Streaming platform a channel belongs to. Drives default detection/tool choice.
///
/// `Nrk` (nrk.no, the Norwegian public broadcaster) and `Nebula` (nebula.tv)
/// are "branded yt-dlp platforms": recognized URLs, own icon/label/log tag and
/// per-platform defaults, but no platform-specific detectors or asset/meta
/// fetchers — live detection falls back to the generic probe, exactly like
/// `Generic`. Unknown platform strings parse to `Generic`, so rows written by
/// a build with more variants degrade gracefully on an older one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    Twitch,
    YouTube,
    Kick,
    Nrk,
    Nebula,
    Generic,
}

impl Platform {
    pub const ALL: [Platform; 6] = [
        Platform::Twitch,
        Platform::YouTube,
        Platform::Kick,
        Platform::Nrk,
        Platform::Nebula,
        Platform::Generic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Twitch => "twitch",
            Platform::YouTube => "youtube",
            Platform::Kick => "kick",
            Platform::Nrk => "nrk",
            Platform::Nebula => "nebula",
            Platform::Generic => "generic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Platform::Twitch => "Twitch",
            Platform::YouTube => "YouTube",
            Platform::Kick => "Kick",
            Platform::Nrk => "NRK",
            Platform::Nebula => "Nebula",
            Platform::Generic => "Generic",
        }
    }

    pub fn parse(s: &str) -> Platform {
        match s {
            "twitch" => Platform::Twitch,
            "youtube" => Platform::YouTube,
            "kick" => Platform::Kick,
            "nrk" => Platform::Nrk,
            "nebula" => Platform::Nebula,
            _ => Platform::Generic,
        }
    }

    /// Parse a stored "preferred asset platform": an empty/unknown string means
    /// `None` ("auto — first available"). Only platforms with asset fetchers
    /// (Twitch/YouTube/Kick) are valid preferences; everything else maps to
    /// `None`.
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
        } else if u.contains("nrk.no") {
            // nrk.no + subdomains (tv.nrk.no, radio.nrk.no).
            Platform::Nrk
        } else if u.contains("nebula.tv") || u.contains("watchnebula.com") {
            Platform::Nebula
        } else {
            Platform::Generic
        }
    }

    /// Sensible default LIVE-capture tool for this platform (research-backed:
    /// streamlink for Twitch incl. 2K, yt-dlp for YouTube). Generic gets
    /// streamlink because a generic *live* URL is typically a raw HLS page or
    /// one of streamlink's supported live sites.
    pub fn default_tool(self) -> Tool {
        match self {
            Platform::Twitch => Tool::Streamlink,
            Platform::YouTube => Tool::YtDlp,
            Platform::Kick => Tool::Streamlink,
            // yt-dlp has real NRK/Nebula extractors; streamlink has neither.
            Platform::Nrk => Tool::YtDlp,
            Platform::Nebula => Tool::YtDlp,
            Platform::Generic => Tool::Streamlink,
        }
    }

    /// Sensible default ON-DEMAND download tool (the Videos tab). Differs from
    /// [`Platform::default_tool`] for Generic: an arbitrary video-page URL
    /// (NRK, Vimeo, …) is almost always one of yt-dlp's ~1800 extractors,
    /// while streamlink only handles live streams on its supported sites —
    /// pasting a plain video page into a streamlink download just fails with
    /// "error: No plugin can handle URL".
    pub fn default_download_tool(self) -> Tool {
        match self {
            Platform::Generic => Tool::YtDlp,
            other => other.default_tool(),
        }
    }

    /// Default detection method when no API credentials are configured.
    pub fn default_detection(self) -> DetectionMethod {
        match self {
            Platform::Twitch => DetectionMethod::TwitchApi,
            Platform::YouTube => DetectionMethod::Scrape,
            Platform::Kick => DetectionMethod::Scrape,
            // No platform-specific detector — the tool-based liveness probe is
            // the only live check that works for these.
            Platform::Nrk | Platform::Nebula => DetectionMethod::GenericProbe,
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
                DetectionMethod::Disabled,
            ],
            Platform::YouTube => &[
                DetectionMethod::Scrape,
                DetectionMethod::WebSub,
                DetectionMethod::WebSubOnly,
                DetectionMethod::YouTubeApi,
                DetectionMethod::GenericProbe,
                DetectionMethod::Disabled,
            ],
            Platform::Kick => &[
                DetectionMethod::Scrape,
                DetectionMethod::KickApi,
                DetectionMethod::GenericProbe,
                DetectionMethod::Disabled,
            ],
            Platform::Nrk | Platform::Nebula | Platform::Generic => {
                &[DetectionMethod::GenericProbe, DetectionMethod::Disabled]
            }
        }
    }

    /// Whether a live title/game/viewer metadata fetcher exists for this
    /// platform (drives the in-recording meta watcher, trigger matching, and
    /// the meta-refresh warning). Branded-generic platforms (NRK/Nebula) have
    /// none, same as `Generic`.
    pub fn has_stream_meta(self) -> bool {
        matches!(self, Platform::Twitch | Platform::YouTube | Platform::Kick)
    }

    /// Whether a channel-asset fetcher (icon/banner/emotes/about) exists for
    /// this platform. Mirrors [`Platform::parse_opt`]'s valid set.
    pub fn has_asset_fetcher(self) -> bool {
        matches!(self, Platform::Twitch | Platform::YouTube | Platform::Kick)
    }

    /// Whether streamlink has no plugin for this platform's video pages, so
    /// picking it for an on-demand download warrants a UI warning: it fails
    /// with "No plugin can handle URL" on NRK/Nebula (yt-dlp has the real
    /// extractors), and on Generic it only works for live streams on
    /// streamlink's own supported-site list — not plain video pages.
    pub fn streamlink_unsupported(self) -> bool {
        matches!(self, Platform::Nrk | Platform::Nebula | Platform::Generic)
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
    /// No automatic liveness checking at all: the scheduler skips this
    /// instance entirely (no API polls, scrapes, or probes), and no
    /// push mechanism (WebSub/EventSub) is subscribed either. State only
    /// changes via a manual "▶ Start" (which records immediately, trusting
    /// the user, since there's no configured way to check first) or another
    /// manual action. For channels you only ever want to record by hand.
    Disabled,
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
            DetectionMethod::Disabled => "disabled",
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
            DetectionMethod::Disabled => "Disabled (manual only)",
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
            DetectionMethod::Disabled => "Disabled",
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
            "disabled" => DetectionMethod::Disabled,
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
            DetectionMethod::Disabled => {
                "No automatic liveness checking at all — not polled by the scheduler, and no \
                 push subscription either. State only changes from a manual action: \"▶ Start\" \
                 records immediately (there's no configured way to check first, so it trusts \
                 you). Use for channels you only ever want to record by hand."
            }
        }
    }
}

/// Whether a [`ScheduledRecording`] fires once or on a weekly repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecurrenceKind {
    /// Fires once at `start_at`.
    Once,
    /// Fires every week on the matching `days_of_week` bits at `time_of_day_secs`.
    Weekly,
}

impl RecurrenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RecurrenceKind::Once => "once",
            RecurrenceKind::Weekly => "weekly",
        }
    }

    pub fn parse(s: &str) -> RecurrenceKind {
        match s {
            "weekly" => RecurrenceKind::Weekly,
            _ => RecurrenceKind::Once,
        }
    }
}

/// Bits for `ScheduledRecording::days_of_week` (Mon=bit0..Sun=bit6).
pub const DOW_MON: i64 = 1 << 0;
pub const DOW_TUE: i64 = 1 << 1;
pub const DOW_WED: i64 = 1 << 2;
pub const DOW_THU: i64 = 1 << 3;
pub const DOW_FRI: i64 = 1 << 4;
pub const DOW_SAT: i64 = 1 << 5;
pub const DOW_SUN: i64 = 1 << 6;
/// `days_of_week` bit for a `chrono::Weekday` (Mon..Sun, matching `DOW_*` order).
pub fn dow_bit(w: chrono::Weekday) -> i64 {
    use chrono::Weekday::*;
    match w {
        Mon => DOW_MON,
        Tue => DOW_TUE,
        Wed => DOW_WED,
        Thu => DOW_THU,
        Fri => DOW_FRI,
        Sat => DOW_SAT,
        Sun => DOW_SUN,
    }
}

/// A force-start recording rule for one monitor: fires at a specific time
/// (`Once`) or weekly (`Weekly`), bypassing the Auto-record flag the same way
/// a trigger-word match does (see `Supervisor::try_begin`'s `forced` param).
/// Lets a channel stay Auto-off — or even Detection: [`DetectionMethod::Disabled`]
/// — while still guaranteeing a recording at a known time. See
/// `scheduled_recordings.rs` for the recurrence math and the background job
/// that fires these.
#[derive(Clone, Debug)]
pub struct ScheduledRecording {
    pub id: i64,
    pub monitor_id: i64,
    pub label: String,
    pub kind: RecurrenceKind,
    /// Unix ts; used when `kind == Once`.
    pub start_at: Option<i64>,
    /// Weekday bitmask (see `DOW_*`); used when `kind == Weekly`.
    pub days_of_week: Option<i64>,
    /// Seconds since local midnight; used when `kind == Weekly`.
    pub time_of_day_secs: Option<i64>,
    /// Optional unix ts: stop recurring after this time (`Weekly` only).
    pub until: Option<i64>,
    /// Optional auto-stop duration; `None` = record until the stream ends naturally.
    pub duration_secs: Option<i64>,
    pub enabled: bool,
    /// Cached next occurrence; `None` once a `Once` rule has fired (or a
    /// `Weekly` rule has passed its `until`).
    pub next_run_at: Option<i64>,
    /// Occurrence start ts last actually fired — dedupes a tick from re-firing
    /// the same occurrence. Round-tripped from the DB but not read by the UI
    /// yet (no "last fired" column); the dedupe logic itself works off the
    /// raw column in `due_scheduled_stops`/`mark_scheduled_recording_fired`,
    /// not this struct field.
    #[allow(dead_code)]
    pub last_fired_at: Option<i64>,
    /// Set while a duration-bound occurrence is actively recording; cleared
    /// once the auto-stop has run. Same story as `last_fired_at` — the
    /// background job queries the column directly (`due_scheduled_stops`).
    #[allow(dead_code)]
    pub pending_stop_at: Option<i64>,
    #[allow(dead_code)]
    pub created_at: i64,
}

/// A [`ScheduledRecording`] joined with its channel/monitor names, for the
/// management window and the Streams grid column.
#[derive(Clone, Debug)]
pub struct ScheduledRecordingWithNames {
    pub rec: ScheduledRecording,
    pub channel_name: String,
    pub monitor_url: String,
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

/// YouTube SABR video codec/quality preference. YouTube serves the same 1080p60
/// source as several SABR renditions (H.264, VP9, AV1) at different bitrates;
/// yt-dlp's default sort prefers the smaller VP9/AV1. This maps to a yt-dlp
/// format-sort (`-S`) layered on the SABR `-f` selector, so it only changes which
/// rendition each `b*` selector resolves to. `Inherit` (per-instance only) uses
/// the global Settings default; the others are concrete presets or a raw custom
/// `-S` string. Mirrors [`AuthKind`]'s Inherit/global pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SabrCodecPref {
    /// Per-instance: use the global default from Settings.
    Inherit,
    /// yt-dlp's default codec preference (efficiency — VP9/AV1). No `-S` added.
    #[default]
    Auto,
    /// Highest bitrate at the best resolution/fps, regardless of codec.
    BestQuality,
    /// Prefer H.264 (AVC) as the codec tiebreaker.
    H264,
    /// Prefer VP9.
    Vp9,
    /// Prefer AV1.
    Av1,
    /// A raw `-S` sort string supplied by the user (see `sabr_codec_custom`).
    Custom,
}

impl SabrCodecPref {
    /// Every variant, in dropdown display order (Inherit first for per-instance).
    pub const ALL: [SabrCodecPref; 7] = [
        SabrCodecPref::Inherit,
        SabrCodecPref::Auto,
        SabrCodecPref::BestQuality,
        SabrCodecPref::H264,
        SabrCodecPref::Vp9,
        SabrCodecPref::Av1,
        SabrCodecPref::Custom,
    ];

    /// The presets offered at the GLOBAL level (no `Inherit` — nothing to inherit).
    pub const GLOBAL: [SabrCodecPref; 6] = [
        SabrCodecPref::Auto,
        SabrCodecPref::BestQuality,
        SabrCodecPref::H264,
        SabrCodecPref::Vp9,
        SabrCodecPref::Av1,
        SabrCodecPref::Custom,
    ];

    /// Stable persisted id (never change once shipped).
    pub fn id(self) -> &'static str {
        match self {
            SabrCodecPref::Inherit => "inherit",
            SabrCodecPref::Auto => "auto",
            SabrCodecPref::BestQuality => "best",
            SabrCodecPref::H264 => "h264",
            SabrCodecPref::Vp9 => "vp9",
            SabrCodecPref::Av1 => "av01",
            SabrCodecPref::Custom => "custom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SabrCodecPref::Inherit => "Inherit (global)",
            SabrCodecPref::Auto => "Auto (efficiency — VP9/AV1)",
            SabrCodecPref::BestQuality => "Best quality (highest bitrate)",
            SabrCodecPref::H264 => "Prefer H.264",
            SabrCodecPref::Vp9 => "Prefer VP9",
            SabrCodecPref::Av1 => "Prefer AV1",
            SabrCodecPref::Custom => "Custom (-S sort)",
        }
    }

    /// Resolve a persisted id back to a variant (unknown ⇒ `Inherit`).
    pub fn parse(s: &str) -> SabrCodecPref {
        match s {
            "auto" => SabrCodecPref::Auto,
            "best" => SabrCodecPref::BestQuality,
            "h264" => SabrCodecPref::H264,
            "vp9" => SabrCodecPref::Vp9,
            "av01" => SabrCodecPref::Av1,
            "custom" => SabrCodecPref::Custom,
            _ => SabrCodecPref::Inherit,
        }
    }

    /// The yt-dlp `-S` format-sort value for this preference (`""` = add no `-S`).
    /// `res,fps` lead so resolution/fps always dominate and codec/bitrate is only
    /// the tiebreaker. `custom` is the raw string for [`SabrCodecPref::Custom`].
    pub fn sort_arg(self, custom: &str) -> String {
        match self {
            SabrCodecPref::BestQuality => "res,fps,br".to_string(),
            SabrCodecPref::H264 => "res,fps,vcodec:h264".to_string(),
            SabrCodecPref::Vp9 => "res,fps,vcodec:vp9".to_string(),
            SabrCodecPref::Av1 => "res,fps,vcodec:av01".to_string(),
            SabrCodecPref::Custom => custom.trim().to_string(),
            SabrCodecPref::Auto | SabrCodecPref::Inherit => String::new(),
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
    /// Which platform's (and optionally which account's) profile pic / banner
    /// represents this container (a container can hold the same creator on
    /// Twitch + YouTube + Kick — and multiple ACCOUNTS on one platform). `None`
    /// = auto: the first instance with a fetched icon. Set explicitly via the
    /// channel's Properties → icon source.
    pub preferred_asset: Option<PreferredAssetSource>,
    /// Channel-level **Auto-record** flag. Independent from each instance's
    /// `Monitor::enabled`; a monitor auto-records only when both this AND
    /// `monitor.enabled` are true. Auto gates disk recording ONLY — not
    /// detection, metadata, posts, or any other fetch.
    pub enabled: bool,
    /// Channel-level **master automation** switch. Off = fully dormant (no
    /// detection/recording/asset/about/posts/schedule fetch for any instance;
    /// only manual actions work). Automation runs only when both this AND
    /// `monitor.automation_enabled` are true.
    pub automation_enabled: bool,
}

/// The channel's chosen icon/banner source, persisted in the legacy
/// `channel.preferred_platform` TEXT column as `platform[:account]` —
/// backward-compatible: a bare `"twitch"` (pre-account rows) parses with
/// `account: None`, meaning "the first Twitch account of the container".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreferredAssetSource {
    pub platform: Platform,
    /// [`crate::assets::account_slug`] of the chosen instance's URL; `None` =
    /// whichever account of `platform` comes first.
    pub account: Option<String>,
}

impl PreferredAssetSource {
    /// Parse the DB text form (`""` → None handled by the caller via
    /// `parse_opt`-style emptiness checks; here `"twitch"` / `"twitch:geega"`).
    pub fn parse(s: &str) -> Option<PreferredAssetSource> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let (plat, account) = match s.split_once(':') {
            Some((p, a)) => (p, (!a.trim().is_empty()).then(|| a.trim().to_string())),
            None => (s, None),
        };
        Platform::parse_opt(plat).map(|platform| PreferredAssetSource { platform, account })
    }

    /// The DB text form (`platform` or `platform:account`).
    pub fn to_db(&self) -> String {
        match &self.account {
            Some(a) => format!("{}:{a}", self.platform.as_str()),
            None => self.platform.as_str().to_string(),
        }
    }
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
    /// **Auto-record** flag (disk-space control): gates ONLY whether a live
    /// stream is automatically recorded to disk. Does NOT gate detection,
    /// metadata, posts, or any other fetch. See [`Channel::enabled`].
    pub enabled: bool,
    /// **Master automation** switch for this instance. Off = fully dormant
    /// (no detection/recording/fetch; manual only). See
    /// [`Channel::automation_enabled`].
    pub automation_enabled: bool,
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
    /// YouTube SABR video codec/quality preference (a yt-dlp `-S` sort layered on
    /// the SABR selector). `Inherit` = use the global Settings default.
    pub sabr_codec_pref: SabrCodecPref,
    /// Raw `-S` sort string used when `sabr_codec_pref == Custom` (else ignored).
    pub sabr_codec_custom: String,
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
    /// Go-live time of the CURRENTLY live broadcast, tracked purely from
    /// detection polling — independent of whether a recording exists — so
    /// Went Live/Started On/Duration have data for a live-but-not-recording
    /// (Auto off) instance. `None` when offline (cleared on the same poll that
    /// detects it, then re-stamped fresh the next time it's seen live).
    pub last_live_since: Option<i64>,
    /// True when `last_live_since` is our poll-time approximation rather than
    /// a platform-reported go-live time (mirrors `Recording::went_live_approx`).
    pub last_live_since_approx: bool,
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
    /// The latest recording's trigger-word match description (empty when it
    /// started normally) — drives the ⚡ badge in the state cell.
    pub last_recording_trigger: String,
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
    /// Last-detected live stream title, persisted on every poll (regardless of
    /// Auto). Shown in the Title column when the channel is live but NOT
    /// recording; empty when offline/unknown.
    pub last_title: String,
    /// Last-detected live game/category (Twitch + Kick; YouTube has none).
    pub last_game: String,
    /// Last-detected live thumbnail URL — stored on every poll; groundwork for
    /// a hover preview (no grid column consumes it yet).
    #[allow(dead_code)]
    pub last_thumbnail_url: String,
    /// Last-detected live viewer count; `-1` = unknown/not applicable.
    pub last_viewers: i64,
    /// Live Twitch "Stream Together" collab state, parsed from the
    /// `monitor.last_collab` JSON column (written on every poll like
    /// `last_title`). `None` = offline or not collabing.
    pub live_collab: Option<CollabLive>,
}

impl MonitorWithChannel {
    /// Whether background automation may run for this instance: the master
    /// switch on both the channel and the instance. Off = fully dormant
    /// (detection, recording, and all fetches paused; manual actions still
    /// work). Distinct from the Auto-record flag (`enabled`).
    pub fn automation_on(&self) -> bool {
        self.channel.automation_enabled && self.monitor.automation_enabled
    }
}

/// One collaborator in a live Twitch "Stream Together" session (or a
/// title-@mention heuristic hit). The monitored channel itself is never a
/// partner — only the *other* people. Names are captured at observation time
/// (a later rename doesn't rewrite history).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CollabPartner {
    /// Twitch broadcaster id; empty for title-mention partners.
    #[serde(default)]
    pub id: String,
    /// Login (lowercase); for title mentions this is the @name as typed.
    #[serde(default)]
    pub login: String,
    /// Display name shown in the UI.
    #[serde(default)]
    pub name: String,
    /// True when this partner came from an `@mention` in the stream title
    /// rather than the Shared Chat session (heuristic, shown as `@name`).
    #[serde(default)]
    pub from_title: bool,
}

/// The live collab state persisted on `monitor.last_collab` as JSON (like
/// `last_title`): current Shared Chat partners + title-mention partners.
/// Cleared (empty column) when the channel is offline or not collabing.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CollabLive {
    /// Host broadcaster id of the shared-chat session ('' = no shared chat,
    /// i.e. title mentions only). May be the monitored channel itself.
    #[serde(default)]
    pub host_id: String,
    /// When the shared-chat session was created (Twitch `created_at`), or when
    /// we first observed the title mention. 0 = unknown.
    #[serde(default)]
    pub since_unix: i64,
    #[serde(default)]
    pub partners: Vec<CollabPartner>,
}

/// Comma-joined partner names: shared-chat partners first (display name),
/// then title-mention partners as `@name`.
pub fn collab_partner_names(partners: &[CollabPartner]) -> String {
    let mut parts: Vec<String> = partners
        .iter()
        .filter(|p| !p.from_title)
        .map(|p| p.name.clone())
        .collect();
    parts.extend(
        partners
            .iter()
            .filter(|p| p.from_title)
            .map(|p| format!("@{}", p.name)),
    );
    parts.join(", ")
}

impl CollabLive {
    /// Parse the `monitor.last_collab` JSON; `None` when empty/invalid or no
    /// partners (an empty session is displayed as "no collab").
    pub fn parse(json: &str) -> Option<CollabLive> {
        if json.is_empty() {
            return None;
        }
        serde_json::from_str::<CollabLive>(json)
            .ok()
            .filter(|c| !c.partners.is_empty())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Comma-joined partner names for the Collab cell.
    pub fn names(&self) -> String {
        collab_partner_names(&self.partners)
    }

    /// Name-cell decoration: `" × A × B"` (shared-chat partners only — title
    /// mentions stay in the Collab column, the name cell is for confirmed
    /// sessions).
    pub fn name_suffix(&self) -> String {
        let mut out = String::new();
        for p in self.partners.iter().filter(|p| !p.from_title) {
            out.push_str(" × ");
            out.push_str(&p.name);
        }
        out
    }
}

/// One stored collab session (`collab_session` table, schema v58): a Twitch
/// "Stream Together" shared-chat session (`source = "shared_chat"`) or a
/// title-@mention heuristic hit (`source = "title"`), per monitor. See the
/// v58 migration comment for the full semantics.
#[derive(Clone, Debug)]
pub struct CollabSessionRow {
    #[allow(dead_code)]
    pub id: i64,
    // monitor_id/stream_id: carried for future "jump to that broadcast"
    // linkage; the popup currently keys everything by channel.
    #[allow(dead_code)]
    pub monitor_id: i64,
    /// `"shared_chat"` | `"title"`.
    pub source: String,
    /// Twitch shared-chat session id ('' for title source).
    #[allow(dead_code)]
    pub session_id: String,
    /// Broadcast (Helix stream id) during which the session was observed.
    #[allow(dead_code)]
    pub stream_id: String,
    /// Host broadcaster id ('' for title source). May be the monitored channel.
    pub host_id: String,
    /// Partners (the monitored channel itself is never included).
    pub partners: Vec<CollabPartner>,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    /// `None` = still active.
    pub ended_at: Option<i64>,
}

/// `@mention` handles parsed from a stream/schedule title — the heuristic
/// collab signal for streams that collab without (or before) Shared Chat.
/// Twitch login shape: 3–25 chars of `[A-Za-z0-9_]`; an `@` glued to a
/// preceding word (e.g. an email) is not a mention. Case-insensitive dedup,
/// original casing and order preserved. `own` (the channel's own login/name,
/// case-insensitive) is excluded — channels sometimes @mention themselves.
pub fn title_mentions(title: &str, own: &str) -> Vec<String> {
    let bytes = title.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (i, _) in title.match_indices('@') {
        if i > 0 {
            // Boundary check: reject `foo@bar` (previous char is a word char).
            let prev = bytes[i - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        let rest = &title[i + 1..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let handle = &rest[..end];
        if !(3..=25).contains(&handle.len()) {
            continue;
        }
        let lower = handle.to_lowercase();
        if lower == own.to_lowercase() || seen.contains(&lower) {
            continue;
        }
        seen.push(lower);
        out.push(handle.to_string());
    }
    out
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// A category of in-app notification (the `notification.kind` column). The `id()`
/// string is the stable persisted value — never change one once shipped. Drives
/// the notifications-feed row icon/label and the category filter dropdown.
/// Modeled on [`crate::events::BackgroundTaskKind`] and `GridTableId::ALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationKind {
    /// A monitored channel went live (recording or not).
    WentLive,
    /// A recording finished (info; the row carries `error` severity when failed).
    RecordingFinished,
    /// A capture/detection error.
    Error,
    /// A background task (remux, asset fetch, …) failed.
    TaskFailed,
    /// A new upcoming schedule item appeared.
    ScheduleAdded,
    /// An existing schedule item changed (time / title / category / canceled).
    ScheduleUpdated,
    /// A new YouTube community post was ingested.
    YoutubePost,
    /// A recording's published Twitch VOD came back DMCA-muted.
    VodMuted,
    /// A trigger-word rule matched the live title/game and started a recording.
    TriggerMatched,
    /// A blacklist trigger matched the live title/game and vetoed the
    /// automatic recording.
    TriggerBlocked,
    /// A better rendition appeared after the capture joined (Twitch lists the
    /// source quality late) — the take was restarted to record at it.
    QualityUpgrade,
}

impl NotificationKind {
    /// Every kind, in feed-filter display order.
    pub const ALL: [NotificationKind; 11] = [
        NotificationKind::WentLive,
        NotificationKind::TriggerMatched,
        NotificationKind::TriggerBlocked,
        NotificationKind::RecordingFinished,
        NotificationKind::QualityUpgrade,
        NotificationKind::Error,
        NotificationKind::TaskFailed,
        NotificationKind::ScheduleAdded,
        NotificationKind::ScheduleUpdated,
        NotificationKind::YoutubePost,
        NotificationKind::VodMuted,
    ];

    /// Stable persisted id (the `notification.kind` column). NEVER change.
    pub fn id(self) -> &'static str {
        match self {
            NotificationKind::WentLive => "went_live",
            NotificationKind::RecordingFinished => "recording_finished",
            NotificationKind::Error => "error",
            NotificationKind::TaskFailed => "task_failed",
            NotificationKind::ScheduleAdded => "schedule_added",
            NotificationKind::ScheduleUpdated => "schedule_updated",
            NotificationKind::YoutubePost => "youtube_post",
            NotificationKind::VodMuted => "vod_muted",
            NotificationKind::TriggerMatched => "trigger_matched",
            NotificationKind::TriggerBlocked => "trigger_blocked",
            NotificationKind::QualityUpgrade => "quality_upgrade",
        }
    }

    /// Resolve a persisted id back to a kind (`None` for an unknown/stale id).
    pub fn from_id(s: &str) -> Option<NotificationKind> {
        NotificationKind::ALL.into_iter().find(|k| k.id() == s)
    }

    /// Short human-readable label for the feed + filter dropdown.
    pub fn label(self) -> &'static str {
        match self {
            NotificationKind::WentLive => "Went live",
            NotificationKind::RecordingFinished => "Recording finished",
            NotificationKind::Error => "Error",
            NotificationKind::TaskFailed => "Task failed",
            NotificationKind::ScheduleAdded => "Schedule added",
            NotificationKind::ScheduleUpdated => "Schedule updated",
            NotificationKind::YoutubePost => "YouTube post",
            NotificationKind::VodMuted => "VOD muted",
            NotificationKind::TriggerMatched => "Trigger matched",
            NotificationKind::TriggerBlocked => "Blacklist blocked",
            NotificationKind::QualityUpgrade => "Quality upgrade",
        }
    }

    /// Font-safe emoji glyph (verified present in egui's NotoEmoji subset —
    /// see [`ContentType::icon`]).
    pub fn icon(self) -> &'static str {
        match self {
            NotificationKind::WentLive => "🔴",
            NotificationKind::RecordingFinished => "⏹",
            NotificationKind::Error => "⚠",
            NotificationKind::TaskFailed => "❌",
            NotificationKind::ScheduleAdded => "🗓",
            NotificationKind::ScheduleUpdated => "🗓",
            NotificationKind::YoutubePost => "📣",
            NotificationKind::VodMuted => "✂",
            NotificationKind::TriggerMatched => "⚡",
            NotificationKind::TriggerBlocked => "🚫",
            NotificationKind::QualityUpgrade => "⬆",
        }
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

/// One title or game/category change observed for a MONITOR, independent of
/// any recording — the continuous, always-on counterpart to
/// [`StreamMetaChange`]. `at_unix` is an absolute wall-clock timestamp (not an
/// offset — there's no "take start" to be relative to when nothing may ever be
/// recorded). Fed both by the scheduler's own live poll (while not recording)
/// and by `meta_watcher` (while recording), so this is a single unbroken
/// history regardless of Auto/Enabled state. As with `StreamMetaChange`, the
/// first entry for each `kind` per "session" has an empty `old_value` and
/// isn't a real transition — display code filters those out the same way.
#[derive(Clone, Debug)]
pub struct MonitorStreamChange {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub monitor_id: i64,
    pub at_unix: i64,
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
    /// Collaborator names for this scheduled stream, comma-joined ('' = none):
    /// the OCR source's explicit collab field, or `@mentions` parsed from the
    /// segment title. `serde(default)` is load-bearing — segments are cached
    /// as `decoded_json` in the community-post OCR archive, and old blobs
    /// don't have this field.
    #[serde(default)]
    pub collab: String,
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
    /// Collaborator names, comma-joined ('' = none). See
    /// [`ScheduleSegment::collab`].
    pub collab: String,
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
    /// Which binary to invoke when `tool == Tool::YtDlp`: empty = the system
    /// yt-dlp, `"sabr"` = the built-in SABR dev build, anything else = the
    /// alias of a user-defined [`crate::downloader::CustomTool`]. Ignored for
    /// Streamlink/Ffmpeg. Resolved to a program path at download time via
    /// [`crate::downloader::resolve_ytdlp_program`] (falls back to the system
    /// binary if the alias no longer exists).
    pub tool_binary: String,
    pub quality: String,
    pub output_dir: String,
    pub filename_template: String,
    pub auth_kind: AuthKind,
    pub auth_value: String,
    /// Audio tracks to download. Empty = the tool's default (one track);
    /// `all`/`*` = every track; otherwise a comma-separated list of language
    /// codes, muxed together as separate audio streams (YouTube VODs can carry
    /// a dub/descriptive-audio track alongside the original). Honored by
    /// streamlink (`--hls-audio-select`) and yt-dlp (synthesizes a `-f` format
    /// selector — see `downloader::yt_audio_format_selector` — ignored when
    /// `quality` is a custom format string, which always wins); ffmpeg keeps
    /// what the chosen format carries either way.
    pub audio_tracks: String,
    /// Subtitle tracks to download and embed into the file. Empty = none;
    /// `all` = every subtitle; otherwise a comma-separated language list.
    /// Honored by yt-dlp (`--sub-langs` fetches them as sidecars, then
    /// `downloader::embed_subtitles_into_mkv` embeds and deletes them — a Video
    /// download lands in one flat folder, unlike a live recording's per-channel
    /// subdir, so a lingering sidecar is just clutter); streamlink can't mux
    /// subtitles.
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
            tool: platform.default_download_tool(),
            quality: "best".into(),
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            output_dir: default_output_dir.to_string(),
            filename_template: "{name}_{date}_{time}".into(),
            extra_args: String::new(),
            auto_title: false,
        }
    }

    /// Serde-default seeds for fields added after [`DownloadDefaults`] first
    /// shipped (old persisted JSON lacks them). The output dir is unknown at
    /// deserialize time — [`DownloadDefaults::fill_empty_output_dirs`] fills
    /// it at load.
    fn seeded_nrk() -> PlatformDownloadDefault {
        PlatformDownloadDefault::seeded(Platform::Nrk, "")
    }
    fn seeded_nebula() -> PlatformDownloadDefault {
        PlatformDownloadDefault::seeded(Platform::Nebula, "")
    }
}

/// Per-platform download defaults for the Videos tab, persisted as JSON in
/// `app_settings`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadDefaults {
    pub twitch: PlatformDownloadDefault,
    pub youtube: PlatformDownloadDefault,
    pub kick: PlatformDownloadDefault,
    // Added after the struct first shipped: persisted JSON from older builds
    // lacks these fields, so they deserialize to a placeholder seed (empty
    // output dir) that `fill_empty_output_dirs` completes at load.
    #[serde(default = "PlatformDownloadDefault::seeded_nrk")]
    pub nrk: PlatformDownloadDefault,
    #[serde(default = "PlatformDownloadDefault::seeded_nebula")]
    pub nebula: PlatformDownloadDefault,
    pub generic: PlatformDownloadDefault,
}

impl DownloadDefaults {
    pub fn seeded(default_output_dir: &str) -> DownloadDefaults {
        DownloadDefaults {
            twitch: PlatformDownloadDefault::seeded(Platform::Twitch, default_output_dir),
            youtube: PlatformDownloadDefault::seeded(Platform::YouTube, default_output_dir),
            kick: PlatformDownloadDefault::seeded(Platform::Kick, default_output_dir),
            nrk: PlatformDownloadDefault::seeded(Platform::Nrk, default_output_dir),
            nebula: PlatformDownloadDefault::seeded(Platform::Nebula, default_output_dir),
            generic: PlatformDownloadDefault::seeded(Platform::Generic, default_output_dir),
        }
    }

    pub fn get(&self, platform: Platform) -> &PlatformDownloadDefault {
        match platform {
            Platform::Twitch => &self.twitch,
            Platform::YouTube => &self.youtube,
            Platform::Kick => &self.kick,
            Platform::Nrk => &self.nrk,
            Platform::Nebula => &self.nebula,
            Platform::Generic => &self.generic,
        }
    }

    pub fn get_mut(&mut self, platform: Platform) -> &mut PlatformDownloadDefault {
        match platform {
            Platform::Twitch => &mut self.twitch,
            Platform::YouTube => &mut self.youtube,
            Platform::Kick => &mut self.kick,
            Platform::Nrk => &mut self.nrk,
            Platform::Nebula => &mut self.nebula,
            Platform::Generic => &mut self.generic,
        }
    }

    /// Complete placeholder entries that came from serde defaults (a platform
    /// added after the struct was first persisted): any entry with an empty
    /// output dir gets the app's current default. Run once at load, before
    /// the defaults reach the Videos form.
    pub fn fill_empty_output_dirs(&mut self, default_output_dir: &str) {
        for platform in Platform::ALL {
            let d = self.get_mut(platform);
            if d.output_dir.trim().is_empty() {
                d.output_dir = default_output_dir.to_string();
            }
        }
    }

    /// One-shot heal for defaults persisted before generic on-demand downloads
    /// defaulted to yt-dlp: the old seed gave Generic streamlink, which cannot
    /// download a plain video page (any non-Twitch/YouTube/Kick site — NRK,
    /// Vimeo, … — failed with streamlink's "No plugin can handle URL" even
    /// though yt-dlp handles them fine). Returns true when it changed
    /// something; the caller persists the fix and sets a marker setting so a
    /// user who later *deliberately* picks streamlink for generic downloads
    /// isn't overridden again.
    pub fn heal_legacy_generic_tool(&mut self) -> bool {
        if self.generic.tool == Tool::Streamlink {
            self.generic.tool = Tool::YtDlp;
            true
        } else {
            false
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
    // Added later; `#[serde(default)]` (all-None) keeps old persisted JSON
    // loading — unset fields inherit from global/hardcoded like every other.
    #[serde(default)]
    pub nrk: PlatformMonitorDefault,
    #[serde(default)]
    pub nebula: PlatformMonitorDefault,
    pub generic: PlatformMonitorDefault,
}

impl MonitorDefaults {
    pub fn get(&self, platform: Platform) -> &PlatformMonitorDefault {
        match platform {
            Platform::Twitch => &self.twitch,
            Platform::YouTube => &self.youtube,
            Platform::Kick => &self.kick,
            Platform::Nrk => &self.nrk,
            Platform::Nebula => &self.nebula,
            Platform::Generic => &self.generic,
        }
    }

    pub fn get_mut(&mut self, platform: Platform) -> &mut PlatformMonitorDefault {
        match platform {
            Platform::Twitch => &mut self.twitch,
            Platform::YouTube => &mut self.youtube,
            Platform::Kick => &mut self.kick,
            Platform::Nrk => &mut self.nrk,
            Platform::Nebula => &mut self.nebula,
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
/// `app_settings` key — cumulative per-platform detection/poll request stats
/// (JSON blob). See [`PollStats`].
pub const K_POLL_STATS: &str = "poll_stats";
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

/// Cumulative detection/poll request counts for one platform, folded in by
/// the scheduler across every detection method used for it (batched Twitch
/// Helix polls, WebSub/scrape fallback checks, YouTube/Kick API probes,
/// generic HTTP probes). Tracks request *health* — success vs error — so
/// recurring instability (auth failures, DNS/network blips, rate limiting)
/// shows up in the Stats view instead of only being visible by combing logs.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlatformPollStats {
    /// Total poll/detect attempts (one per monitor-check — a single batched
    /// Twitch Helix call counts once per channel it covers, mirroring what
    /// the scheduler's per-monitor state-change log already does).
    pub polls: u64,
    /// Attempts that came back as an error (network/DNS, auth, rate-limit,
    /// parse failure — anything `DetectOutcome` flags as an error).
    pub errors: u64,
    /// Unix timestamp of the most recent error.
    pub last_error_at: Option<i64>,
    /// Detail string from the most recent error (mirrors what the
    /// scheduler's per-monitor state-change line logs for that outcome).
    pub last_error: String,
    /// Ring buffer of the most recent individual errors (oldest first,
    /// capped at [`MAX_RECENT_POLL_ERRORS`]) so the Stats view can list what
    /// actually failed instead of only counting. `serde(default)` keeps
    /// pre-existing persisted blobs loading (they simply start empty).
    #[serde(default)]
    pub recent_errors: Vec<PollErrorEntry>,
}

/// One individual failed poll/detect attempt, kept in
/// [`PlatformPollStats::recent_errors`].
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PollErrorEntry {
    /// Unix timestamp of the failed check.
    pub at: i64,
    /// Channel name of the monitor whose check failed.
    pub monitor: String,
    /// Detection-method short label ([`DetectionMethod::short_label`]).
    pub method: String,
    /// Error detail — the same string the per-poll log line and the grid's
    /// "Last error" hover show.
    pub detail: String,
}

/// One aggregated time bucket of poll/detect history for a
/// `(platform, method)` pair, as returned by `Store::poll_history` — the raw
/// material for the Stats view's error-rate / request-volume graphs. Backed
/// by the `poll_history` table (schema v56): stored at minute resolution,
/// aggregated to whatever bucket width the requested view span calls for at
/// query time. Kept flat (platform + method as fields) so the UI can group
/// by whichever axis a plot needs.
#[derive(Clone)]
pub struct PollBucket {
    /// Bucket start (unix secs, aligned down to the queried bucket width).
    pub t: i64,
    /// Platform key ([`Platform::as_str`]).
    pub platform: String,
    /// Detection-method short label ([`DetectionMethod::short_label`]).
    pub method: String,
    /// Poll/detect attempts that landed in this bucket.
    pub polls: u64,
    /// How many of those attempts errored.
    pub errors: u64,
}

/// One aggregated time bucket of a monitor's viewer/follower history, as
/// returned by `Store::viewer_history_range` — the raw material for the
/// Channel Stats graphs. Backed by the `viewer_history` table (schema v59):
/// stored per monitor at minute resolution while live (peak within the
/// minute), aggregated to the requested bucket width at query time with MAX
/// so re-bucketing and downsampling never flatten spikes.
#[derive(Clone)]
pub struct ViewerBucket {
    /// The monitor the samples came from (one plot line per monitor).
    pub monitor_id: i64,
    /// Bucket start (unix secs, aligned down to the queried bucket width).
    pub t: i64,
    /// Peak live viewer count within the bucket.
    pub viewers: i64,
    /// Platform-reported follower total, when the detection path carries one
    /// (Kick today; Twitch/YouTube expose none without owner credentials).
    pub followers: Option<i64>,
}

/// One discrete channel event from the `stream_event` table (schema v59):
/// subs/resubs/gift subs/bits parsed live out of the recorded Twitch chat,
/// raids from chat and/or EventSub `channel.raid`.
#[derive(Clone)]
pub struct StreamEventRow {
    /// Kept for future per-instance filtering; queries are channel-scoped.
    #[allow(dead_code)]
    pub monitor_id: i64,
    pub at: i64,
    /// Broadcast id when known (`''` otherwise — filter by time instead).
    #[allow(dead_code)]
    pub stream_id: String,
    /// `sub` | `resub` | `subgift` | `bits` | `raid_in` | `raid_out`, plus
    /// the chat-moderation kinds (v60): `msg_deleted` | `timeout` | `ban` |
    /// `chat_clear` | `chat_mode` | `role_change`.
    pub kind: String,
    /// Who did it: subscriber, gifter, cheerer, or incoming raider.
    pub actor: String,
    /// Gift recipient (`''` for a community batch) or outgoing-raid target.
    pub target: String,
    /// Bits cheered, gift-batch size, raid party size, resub months, or
    /// timeout seconds.
    pub amount: i64,
    /// Twitch sub plan (`1000`/`2000`/`3000`/`Prime`) where applicable.
    pub tier: String,
    /// Free-text payload (schema v60): deleted-message excerpt, chat-mode
    /// change description, or role-change description.
    pub detail: String,
}

/// One channel's aggregate line in the Channel Stats overview table, as
/// returned by `Store::channel_stats_overview` for the selected span.
#[derive(Clone)]
pub struct ChannelStatsRow {
    pub channel_id: i64,
    pub name: String,
    /// Highest sampled viewer count in the span.
    pub peak_viewers: i64,
    /// Span-weighted average viewers while live.
    pub avg_viewers: f64,
    /// Seconds of sampled live airtime (sum of sample spans).
    pub live_secs: i64,
    /// Latest known follower total (Kick only today).
    pub followers: Option<i64>,
}

/// Per-platform cap on [`PlatformPollStats::recent_errors`] — old entries are
/// dropped from the front once exceeded.
pub const MAX_RECENT_POLL_ERRORS: usize = 50;

/// Cumulative per-platform poll/detect stats (see [`PlatformPollStats`]),
/// keyed by [`Platform::as_str`]. Persisted as one JSON blob under
/// [`K_POLL_STATS`], accumulated once per scheduler tick rather than per
/// monitor — keeps disk writes to about one per poll interval regardless of
/// how many channels are configured.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct PollStats {
    pub by_platform: std::collections::HashMap<String, PlatformPollStats>,
}

impl PollStats {
    /// Fold one detect outcome in: the cumulative per-platform counters plus
    /// the per-platform recent-error ring. The single entry point the
    /// scheduler uses per outcome — pure, so the accumulation is
    /// unit-testable without a scheduler. (The time-bucketed graph history is
    /// separate — the `poll_history` table via `Store::record_poll_history`.)
    pub fn record(
        &mut self,
        at: i64,
        platform: Platform,
        method: &str,
        monitor: &str,
        error: bool,
        detail: &str,
    ) {
        let entry = self
            .by_platform
            .entry(platform.as_str().to_string())
            .or_default();
        entry.polls += 1;
        if error {
            entry.errors += 1;
            entry.last_error_at = Some(at);
            entry.last_error = detail.to_string();
            entry.recent_errors.push(PollErrorEntry {
                at,
                monitor: monitor.to_string(),
                method: method.to_string(),
                detail: detail.to_string(),
            });
            if entry.recent_errors.len() > MAX_RECENT_POLL_ERRORS {
                let excess = entry.recent_errors.len() - MAX_RECENT_POLL_ERRORS;
                entry.recent_errors.drain(..excess);
            }
        }
    }
}

/// Concrete values for the remux title-tag template tokens, filled by
/// recording-aware callers (see `remux_opts_for_recording`). Without them the
/// remux falls back to the destination file stem for `{title}`/`{name}` and
/// empty strings for the rest — a raw template with literal `{braces}` must
/// never end up as an MKV title tag.
#[derive(Clone, Debug, Default)]
pub struct TitleVars {
    /// Stream/video title (empty = unknown → falls back to the file stem).
    pub title: String,
    /// Channel display name.
    pub channel: String,
    /// Games/categories summary (same formatting as the `{games}` filename var).
    pub games: String,
    /// Capture start (unix secs) for `{date}`/`{year}`/`{month}`/`{day}`; 0 = unknown.
    pub started_at: i64,
}

/// Controls what gets embedded into the MKV container during a TS→MKV remux.
#[derive(Clone, Debug)]
pub struct RemuxOpts {
    /// Attach a thumbnail sidecar as cover art (image/jpeg or image/png).
    pub embed_thumbnail: bool,
    /// Write a `title` metadata tag. `title_template` must be non-empty.
    pub embed_title: bool,
    /// Template for the title tag. Tokens: `{title}` `{channel}` `{games}`
    /// `{date}` `{year}` `{month}` `{day}` `{name}`. `"{title}"` is the
    /// recommended default.
    pub title_template: String,
    /// Copy subtitle sidecar files (`.srt`/`.ass`/`.vtt`) as subtitle streams.
    pub embed_subs: bool,
    /// Values for the title-template tokens (see [`TitleVars`]).
    pub title_vars: Option<TitleVars>,
}

impl Default for RemuxOpts {
    fn default() -> Self {
        RemuxOpts {
            embed_thumbnail: true,
            embed_title: false,
            title_template: "{title}".into(),
            embed_subs: false,
            title_vars: None,
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
    /// CDN VOD-recovery status — a namespace disjoint from `status`/`vod_state`.
    /// `None` = never attempted. `"recovering"` = in progress; `"recovered"` =
    /// full timeline muxed; `"partial"` = some segments were gone; `"failed"` =
    /// error; `"unavailable"` = past the ~60-day CDN retention window.
    pub recovery_state: Option<String>,
    /// Path to the recovered MKV once `recovery_state` is `recovered`/`partial`.
    pub recovered_path: Option<String>,
    /// Post-stream published-VOD download status (the "archive the VOD after end"
    /// feature). `None` = not attempted. `"downloading"` = in progress; `"archived"`
    /// = downloaded alongside; `"replaced"` = the VOD replaced the live capture;
    /// `"muted"` = the Twitch VOD was DMCA-muted (recovery run, never replaced,
    /// pending acknowledgement); `"failed"`/`"skipped"`/`"acknowledged"`.
    pub vod_dl_state: Option<String>,
    /// Path to the downloaded published VOD once `vod_dl_state` is `archived`.
    pub vod_dl_path: Option<String>,
    /// The `video.id` of the in-flight/finished archive download job, when linked.
    /// Round-tripped through the store (the completion hook reverse-looks-up by the
    /// `vod_dl_video_id` column via SQL); not read off the struct today.
    #[allow(dead_code)]
    pub vod_dl_video_id: Option<i64>,
    /// A late-joined capture's missed beginning, downloaded from the growing
    /// published-VOD playlist while the stream was live (`{stem}.head.mkv`).
    pub backfill_path: Option<String>,
    /// Lossless post-stream concat of head + live capture (`{stem}.full.mkv`).
    pub full_path: Option<String>,
    /// Human description of the trigger-word match that started this recording
    /// (e.g. `title ~ "karaoke"`), empty when it started normally. Drives the
    /// ⚡ badge in the streams table.
    pub trigger_info: String,
    /// `"queued"` while a head-backfill decision is pending for this take (set
    /// the instant the job is spawned, cleared the moment it either starts
    /// fetching or determines nothing is needed) — empty otherwise. Drives the
    /// "⏳ backfill queued" badge, which exists specifically to cover
    /// `head_backfill_job`'s ~2 minute settle wait before it does anything
    /// visible (see `downloader::HEAD_BACKFILL_SETTLE_SECS`).
    pub head_backfill_state: String,
    /// The exact [`crate::triggers::TriggerRule`] (serde JSON) that started
    /// this recording, frozen at start time — empty = not trigger-started, or
    /// started by a trigger with no `lead_secs`/`stop_on_unmatch` behavior.
    /// Rules have no stable id and can be edited/reordered mid-broadcast, so
    /// this — not a live re-resolve of the current rule lists — is what the
    /// stop-on-unmatch watcher (and its re-attach-after-restart path) reads.
    /// `trigger_info` is the human-readable sibling of this column.
    pub trigger_rule_json: String,
}

/// A take awaiting a head-backfill decision — the Background view's "Planned"
/// section row. See [`Recording::head_backfill_state`].
#[derive(Clone, Debug)]
pub struct QueuedHeadBackfill {
    pub channel: String,
    pub started_at: i64,
}

/// A recording whose published VOD came back DMCA-muted — a row of the Issues
/// panel's muted-VOD category (with the recovered copy, when recovery produced one).
#[derive(Clone, Debug)]
pub struct MutedVodIssue {
    pub rec_id: i64,
    pub channel: String,
    pub output_path: String,
    pub recovered_path: Option<String>,
    pub recovery_state: Option<String>,
    pub muted_secs: i64,
}

/// A recording eligible for CDN VOD recovery, with just the fields the recovery
/// engine needs (login is derived from `monitor_url`). Returned by the store's
/// recovery queries so the bulk scan / auto-hook can build [`crate::recovery::RecoveryInputs`].
#[derive(Clone, Debug)]
pub struct RecoverableTake {
    pub rec_id: i64,
    pub monitor_url: String,
    pub stream_id: String,
    pub start_epoch: i64,
    pub went_live_approx: bool,
    /// `true` when the VOD is deleted (`not_published`) — probe every segment;
    /// `false` when merely muted (still online) — probe only muted segments.
    pub deleted: bool,
    /// The `/videos/<id>` archive id when known (muted-but-online VOD) — enables the
    /// GQL fast-path. `None` for a deleted VOD.
    pub vod_id: Option<String>,
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

    #[test]
    fn title_mentions_parses_twitch_handles() {
        // Basic extraction, order kept, case preserved.
        assert_eq!(
            title_mentions("COWGIRL DEBUT w/ @Shylily and @Ironmouse!", ""),
            vec!["Shylily", "Ironmouse"]
        );
        // Own login excluded case-insensitively; dedup case-insensitive.
        assert_eq!(
            title_mentions("@NIHMUNE x @shylily x @Shylily collab", "nihmune"),
            vec!["shylily"]
        );
        // Emails / glued @ are not mentions; length limits enforced.
        assert!(title_mentions("mail me at foo@bar.com", "").is_empty());
        assert!(title_mentions("just @ab or @x here", "").is_empty());
        // Trailing punctuation naturally terminates the handle.
        assert_eq!(title_mentions("with @zentreya!!!", ""), vec!["zentreya"]);
        // Underscores are valid handle characters.
        assert_eq!(title_mentions("ft. @girl_dm_", ""), vec!["girl_dm_"]);
    }

    #[test]
    fn collab_live_json_and_display() {
        let c = CollabLive {
            host_id: "650".into(),
            since_unix: 100,
            partners: vec![
                CollabPartner { id: "100".into(), login: "shylily".into(), name: "Shylily".into(), from_title: false },
                CollabPartner { id: String::new(), login: "zen".into(), name: "Zen".into(), from_title: true },
            ],
        };
        let parsed = CollabLive::parse(&c.to_json()).unwrap();
        assert_eq!(parsed, c);
        assert_eq!(parsed.names(), "Shylily, @Zen");
        // Name suffix shows confirmed shared-chat partners only.
        assert_eq!(parsed.name_suffix(), " × Shylily");
        // Empty / partnerless JSON parses to None (renders as "no collab").
        assert_eq!(CollabLive::parse(""), None);
        assert_eq!(
            CollabLive::parse(&CollabLive::default().to_json()),
            None
        );
    }

    #[test]
    fn schedule_segment_deserializes_old_blobs_without_collab() {
        // The community-post OCR archive caches segments as `decoded_json` —
        // blobs written before schema v58 have no `collab` field and must
        // keep deserializing (serde(default)).
        let old = r#"{"id":0,"monitor_id":0,"start_time":100,"end_time":null,
                      "title":"T","category":"","canceled":false,"video_id":null}"#;
        let seg: ScheduleSegment = serde_json::from_str(old).unwrap();
        assert_eq!(seg.collab, "");
    }

    #[test]
    fn nrk_nebula_platforms_detect_and_round_trip() {
        // URL inference, including subdomains and the legacy Nebula domain.
        assert_eq!(Platform::detect("https://www.nrk.no/video/77b5f517"), Platform::Nrk);
        assert_eq!(Platform::detect("https://tv.nrk.no/direkte/nrk1"), Platform::Nrk);
        // radio.nrk.no (podcasts / radio theatre / live radio) — yt-dlp's
        // NRKRadioPodkast extractor; audio-only downloads still land in MKV.
        assert_eq!(
            Platform::detect("https://radio.nrk.no/podkast/oppdatert/l_5005d62a"),
            Platform::Nrk
        );
        assert_eq!(Platform::detect("https://nebula.tv/videos/some-video"), Platform::Nebula);
        assert_eq!(Platform::detect("https://watchnebula.com/foo"), Platform::Nebula);
        assert_eq!(Platform::detect("https://example.com/x"), Platform::Generic);
        // as_str/parse round-trip for every platform (DB storage contract).
        for p in Platform::ALL {
            assert_eq!(Platform::parse(p.as_str()), p);
        }
        // yt-dlp is both the live and download tool (no streamlink plugin).
        for p in [Platform::Nrk, Platform::Nebula] {
            assert_eq!(p.default_tool(), Tool::YtDlp);
            assert_eq!(p.default_download_tool(), Tool::YtDlp);
            assert!(!p.has_stream_meta());
            assert!(!p.has_asset_fetcher());
            assert!(p.streamlink_unsupported());
            // Not valid asset-source preferences.
            assert_eq!(Platform::parse_opt(p.as_str()), None);
        }
    }

    #[test]
    fn poll_stats_record_rings_recent_errors() {
        let mut stats = PollStats::default();

        // A success bumps only the poll counter.
        stats.record(999_999, Platform::Twitch, "Helix API", "ch", false, "");
        assert!(stats.by_platform["twitch"].recent_errors.is_empty());

        // Ring cap: MAX+10 errors keep only the newest MAX, oldest dropped.
        for i in 0..(MAX_RECENT_POLL_ERRORS as i64 + 10) {
            stats.record(1_000_000 + i, Platform::Twitch, "Helix API", "ch", true, &format!("err {i}"));
        }
        let tw = &stats.by_platform["twitch"];
        assert_eq!(tw.recent_errors.len(), MAX_RECENT_POLL_ERRORS);
        assert_eq!(tw.recent_errors[0].detail, "err 10", "oldest dropped from the front");
        assert_eq!(tw.recent_errors.last().unwrap().detail, format!("err {}", MAX_RECENT_POLL_ERRORS as i64 + 9));
        assert_eq!(tw.last_error, tw.recent_errors.last().unwrap().detail, "last_error mirrors the newest entry");
        // Platforms ring independently.
        stats.record(2_000_000, Platform::YouTube, "Scrape", "yt", true, "yt boom");
        assert_eq!(stats.by_platform["youtube"].recent_errors.len(), 1);
        assert_eq!(stats.by_platform["twitch"].recent_errors.len(), MAX_RECENT_POLL_ERRORS);
    }

    #[test]
    fn poll_stats_json_backcompat_gains_recent_errors() {
        // JSON persisted by a build that predates recent_errors (the real
        // shape from app_settings.poll_stats) must still load.
        let old = r#"{"by_platform":{"twitch":{"polls":100,"errors":2,
            "last_error_at":12345,"last_error":"boom"}}}"#;
        let mut stats: PollStats = serde_json::from_str(old).expect("old JSON loads");
        let tw = &stats.by_platform["twitch"];
        assert_eq!(tw.polls, 100);
        assert!(tw.recent_errors.is_empty());
        // And keeps accumulating from there.
        stats.record(20_000, Platform::Twitch, "Helix API", "ch", true, "later");
        assert_eq!(stats.by_platform["twitch"].polls, 101);
        assert_eq!(stats.by_platform["twitch"].recent_errors.len(), 1);
    }

    #[test]
    fn download_defaults_json_backcompat_gains_nrk_nebula() {
        // JSON persisted by a build that predates the nrk/nebula fields (the
        // real shape from app_settings.download_defaults) must still load,
        // with the new platforms seeded and their output dir filled at load.
        let entry = r#"{"tool":"Streamlink","quality":"best","auth_kind":"Inherit",
            "auth_value":"","output_dir":"C:\\out","filename_template":"{name}",
            "extra_args":""}"#;
        let old = format!(
            r#"{{"twitch":{entry},"youtube":{entry},"kick":{entry},"generic":{entry}}}"#
        );
        let mut d: DownloadDefaults = serde_json::from_str(&old).expect("old JSON loads");
        assert_eq!(d.nrk.tool, Tool::YtDlp);
        assert_eq!(d.nebula.tool, Tool::YtDlp);
        assert!(d.nrk.output_dir.is_empty(), "serde default can't know the dir");
        d.fill_empty_output_dirs(r"D:\dl");
        assert_eq!(d.nrk.output_dir, r"D:\dl");
        assert_eq!(d.nebula.output_dir, r"D:\dl");
        // Existing entries keep their configured dir.
        assert_eq!(d.twitch.output_dir, r"C:\out");
    }

    #[test]
    fn generic_download_tool_is_ytdlp_but_live_stays_streamlink() {
        // On-demand: an arbitrary video page (NRK, Vimeo, …) needs yt-dlp's
        // extractors — streamlink dies with "No plugin can handle URL".
        assert_eq!(Platform::Generic.default_download_tool(), Tool::YtDlp);
        // Live capture keeps streamlink (raw HLS / streamlink's live sites).
        assert_eq!(Platform::Generic.default_tool(), Tool::Streamlink);
        // The known platforms agree between the two.
        for p in [Platform::Twitch, Platform::YouTube, Platform::Kick] {
            assert_eq!(p.default_download_tool(), p.default_tool());
        }
        // And the seeded Videos-tab default actually picks it up.
        assert_eq!(
            PlatformDownloadDefault::seeded(Platform::Generic, "out").tool,
            Tool::YtDlp
        );
    }

    #[test]
    fn heal_legacy_generic_tool_flips_only_streamlink() {
        let mut d = DownloadDefaults::seeded("out");
        // Fresh seed is already yt-dlp — nothing to heal.
        assert!(!d.heal_legacy_generic_tool());
        // A legacy persisted config (old seed) gets flipped exactly once.
        d.generic.tool = Tool::Streamlink;
        assert!(d.heal_legacy_generic_tool());
        assert_eq!(d.generic.tool, Tool::YtDlp);
        assert!(!d.heal_legacy_generic_tool());
        // Other platforms are never touched.
        assert_eq!(d.twitch.tool, Tool::Streamlink);
    }

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
            recovery_state: None,
            recovered_path: None,
            vod_dl_state: None,
            vod_dl_path: None,
            vod_dl_video_id: None,
            backfill_path: None,
            full_path: None,
            trigger_info: String::new(),
            head_backfill_state: String::new(),
            trigger_rule_json: String::new(),
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

    #[test]
    fn preferred_asset_source_roundtrip() {
        // Legacy bare-platform value (pre-account rows): account = None.
        let bare = PreferredAssetSource::parse("twitch").unwrap();
        assert_eq!(bare.platform, Platform::Twitch);
        assert_eq!(bare.account, None);
        assert_eq!(bare.to_db(), "twitch");
        // Per-account form.
        let acc = PreferredAssetSource::parse("twitch:geega_alt").unwrap();
        assert_eq!(acc.platform, Platform::Twitch);
        assert_eq!(acc.account.as_deref(), Some("geega_alt"));
        assert_eq!(acc.to_db(), "twitch:geega_alt");
        // Empty / unknown → None (auto).
        assert_eq!(PreferredAssetSource::parse(""), None);
        assert_eq!(PreferredAssetSource::parse("generic"), None);
        // A dangling colon parses as a bare platform.
        assert_eq!(PreferredAssetSource::parse("kick:").unwrap().account, None);
    }
}
