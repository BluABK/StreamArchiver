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
                DetectionMethod::Scrape,
                DetectionMethod::GenericProbe,
            ],
            Platform::YouTube => &[DetectionMethod::Scrape, DetectionMethod::GenericProbe],
            Platform::Kick => &[DetectionMethod::Scrape, DetectionMethod::GenericProbe],
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
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DetectionMethod::TwitchApi => "Twitch API (Helix polling)",
            DetectionMethod::YouTubeApi => "YouTube Data API",
            DetectionMethod::Scrape => "Scrape poll (no API)",
            DetectionMethod::CliSelfPoll => "CLI self-poll loop",
            DetectionMethod::GenericProbe => "Generic HTTP probe",
            DetectionMethod::EventSub => "Twitch EventSub push (Phase 4)",
        }
    }

    pub fn parse(s: &str) -> DetectionMethod {
        match s {
            "twitch_api" => DetectionMethod::TwitchApi,
            "youtube_api" => DetectionMethod::YouTubeApi,
            "scrape" => DetectionMethod::Scrape,
            "cli_selfpoll" => DetectionMethod::CliSelfPoll,
            "eventsub" => DetectionMethod::EventSub,
            _ => DetectionMethod::GenericProbe,
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

    pub fn ext(self) -> &'static str {
        self.as_str()
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
}

/// Current unix timestamp in seconds.
pub fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
