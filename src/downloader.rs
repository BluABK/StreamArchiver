//! Download supervisor + per-tool adapters.
//!
//! When the scheduler reports a monitor live, the supervisor (bounded by a
//! global concurrency semaphore) spawns the configured tool as a child process,
//! captures its stderr into a ring buffer, waits for exit, classifies the
//! outcome, optionally remuxes TS -> MKV, and records the run in the store. A
//! Win32 Job Object guarantees the whole process tree is killed on stop/exit.
//!
//! Default container is MKV (never MP4): streamlink records to `.ts` then remuxes
//! losslessly to `.mkv`; yt-dlp merges straight to `.mkv`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::detectors::{DetectContext, DetectItem, DetectOutcome};
use crate::events::{AppEvent, EventTx, LiveSignal, ManualCommand};
use crate::models::{
    AuthKind, Container, DetachedKind, DetachedRow, DetectionMethod, K_FILENAME_MEDIA,
    MediaInfoMode, Monitor, MonitorWithChannel, Platform, Recording, SabrCodecPref, Tool, Video,
    now_unix,
};
use crate::platform::DetachedJob;
use crate::store::Store;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How a [`Supervisor::run_process`] call registers itself in the persistent
/// detached-process registry, so a later launch can re-attach if the tool
/// outlives the app. `ref_id == 0` (e.g. a recording row that failed to insert)
/// disables registration — there'd be nothing to reconcile against.
#[derive(Clone)]
struct DetachReg {
    kind: DetachedKind,
    ref_id: i64,
    monitor_id: Option<i64>,
    take_group: Option<String>,
    /// The take's start time (recording/video started_at), not the spawn time —
    /// the timeline anchor a re-attach finalize needs for the stem + lost-time.
    started_at: i64,
    /// True only for the DASH companion (occupies the secondary active map).
    secondary: bool,
    stream_id: Option<String>,
    went_live_at: Option<i64>,
}

/// Monitors currently being recorded, mapped to their child PID (0 until the
/// process has spawned). Shared with the scheduler (so it doesn't re-trigger an
/// active recording) and used at shutdown to kill the process trees.
pub type ActiveSet = Arc<Mutex<HashMap<i64, u32>>>;

/// video_id -> download progress fraction (0.0..=1.0), for the UI progress bar.
/// Populated by the tool's progress output while a video downloads; cleared when
/// it finishes. (Live recordings have no meaningful total, so they don't use it.)
pub type VideoProgress = Arc<Mutex<HashMap<i64, f32>>>;

/// video_id -> current download speed in bytes/sec, for the UI Speed column.
/// Populated alongside `VideoProgress` from the tool's progress output; cleared
/// when the download finishes.
pub type VideoSpeed = Arc<Mutex<HashMap<i64, f64>>>;

/// monitor_id -> unix time the current ad break ends, while one is playing
/// (Twitch+streamlink). Lets the UI tint a row "ad running"; entries expire
/// naturally (now >= value) and are removed when the recording ends.
pub type AdActive = Arc<Mutex<HashMap<i64, i64>>>;

/// Key for the per-stream SABR stall maps: `(monitor_id, stream_id)`. Fully
/// per-stream when a video ID is known; degrades to per-monitor otherwise.
type SabrKey = (i64, Option<String>);

const RING_MAX_LINES: usize = 80;
/// How often the from-start catch-up watcher probes the growing capture.
const CATCHUP_PROBE_INTERVAL_SECS: u64 = 20;
/// Treat a from-start capture as caught up once its media is within this many
/// seconds of the live edge (absorbs fragment lag + approximate go-live times).
const CATCHUP_TOLERANCE_SECS: i64 = 45;

/// Wiring for recording advertisement breaks parsed from a live capture's log.
///
/// Only Twitch+streamlink recordings pass this: streamlink filters Twitch ad
/// segments out of the capture (each break becomes a hard cut) and logs each one
/// as `Detected advertisement break of N second(s)`. yt-dlp/ffmpeg have no
/// equivalent, and on-demand video downloads never set it.
struct AdSink {
    store: Arc<Store>,
    events: EventTx,
    monitor_id: i64,
    /// Always > 0 (the sink is only built for a real recording row).
    recording_id: i64,
    /// Take start (unix secs); the live-edge wall-clock fallback anchor.
    started_at: i64,
    /// Broadcast go-live time when known — a better fallback anchor than the
    /// process start for capture-from-start/DVR takes (their file timeline begins
    /// at go-live, not at recording start).
    went_live_at: Option<i64>,
    /// Whether this take rewinds to the broadcast start (DVR), which decides the
    /// fallback anchor.
    from_start: bool,
    /// The growing capture file; its media duration is the true cut position
    /// (ad segments are filtered out, so captured content == the finished file).
    capture_path: PathBuf,
    /// Shared map the UI reads to tint a row while an ad is playing.
    ad_active: AdActive,
}

/// The plan for one recording: the command to run plus the files involved.
#[derive(Debug, Clone)]
pub struct DownloadPlan {
    pub program: String,
    pub args: Vec<String>,
    /// File the tool writes directly.
    pub capture_path: PathBuf,
    /// Final file after any remux (== capture_path when no remux).
    pub final_path: PathBuf,
    pub remux_to_mkv: bool,
    /// True when the tool writes its own thumbnail inline (normal yt-dlp with
    /// `--write-thumbnail`). When false and a thumbnail is wanted, the supervisor
    /// fetches it over HTTP instead (streamlink, ffmpeg, and SABR captures).
    pub writes_own_thumbnail: bool,
    /// Download mode string, e.g. "live", "sabr", "dash", "hybrid", "hybrid-dash", "direct", "vod", "chat".
    pub mode: String,
}

/// Default SABR format selector when the setting is unset/empty.
pub const SABR_DEFAULT_FORMAT: &str = "ba[protocol=sabr]+bv[protocol=sabr]";
/// Default SABR `--extractor-args` when the setting is unset/empty.
pub const SABR_DEFAULT_EXTRACTOR_ARGS: &str =
    "youtube:formats=duplicate,missing_pot;player-client=web;webpage-client=web";
/// Default PO-token-provider `--extractor-args` (bgutil HTTP server on its default
/// port). Passed as a *separate* `--extractor-args` entry because it targets a
/// different extractor key (`youtubepot-bgutilhttp`) than the `youtube:` args.
/// Used when the setting key has never been written; an explicit empty value
/// disables it (rely on the plugin's own auto-detection instead).
pub const SABR_DEFAULT_POT_ARGS: &str = "youtubepot-bgutilhttp:base_url=http://127.0.0.1:4416";
/// Consecutive from-start SABR stalls ("not near live head") tolerated with
/// deep-rewind enabled before giving up and falling back to live-edge capture.
/// Deep-rewind extends the DVR window, so the *first* stall may be transient;
/// but a persistent stall repeats every attempt (each re-downloading the opening
/// — observed ~190 MiB — then dying), so we tolerate one retry then fall back.
/// With deep-rewind off a stall is a true window expiry and we fall back at once.
const SABR_STALL_FALLBACK_TRIES: u32 = 2;
/// Default DASH-companion format selector when the setting is unset/empty.
pub const DASH_DEFAULT_FORMAT: &str = "bestvideo+bestaudio/best";

/// SABR (Server Adaptive Bit Rate) capture configuration for YouTube. SABR is the
/// only protocol that reliably supports `--live-from-start` today, but it lives in
/// a yt-dlp dev fork (a separate binary). See the YouTube SABR settings section.
#[derive(Clone, Debug, Default)]
pub struct SabrConfig {
    /// Master toggle (Settings). When false, YouTube capture-from-start uses the
    /// system binary's normal path.
    pub enabled: bool,
    /// Path to the SABR dev-build binary; empty ⇒ SABR unavailable.
    pub binary: String,
    /// Format selector injected by the preset (e.g. `ba[protocol=sabr]+bv[protocol=sabr]`).
    pub format: String,
    /// `--extractor-args` value injected by the preset.
    pub extractor_args: String,
    /// Manual raw args; when non-empty, replaces the format + extractor-args preset.
    pub raw_args: String,
    /// PO-token-provider `--extractor-args`, passed as its own `--extractor-args`
    /// entry (different extractor key than `extractor_args`). Empty ⇒ not passed.
    /// Applied regardless of the preset/`raw_args` choice (it's orthogonal to
    /// format selection).
    pub pot_args: String,
    /// GLOBAL default video codec/quality preference (a `-S` sort layered on the
    /// selector). A monitor's own pref overrides this unless it's `Inherit`.
    pub codec_pref: SabrCodecPref,
    /// GLOBAL raw `-S` string when `codec_pref == Custom`.
    pub codec_custom: String,
}

impl SabrConfig {
    /// True when SABR capture is configured and usable.
    pub(crate) fn usable(&self) -> bool {
        self.enabled && !self.binary.is_empty()
    }
}

/// The two yt-dlp binaries available to the supervisor: the system build (PATH or
/// an explicit path) and the optional SABR dev build.
#[derive(Clone, Debug, Default)]
pub struct YtDlpBins {
    /// Explicit system yt-dlp path; empty ⇒ `yt-dlp` on PATH.
    pub system: String,
    pub sabr: SabrConfig,
}

impl YtDlpBins {
    /// The program name/path for the system yt-dlp.
    pub fn system_program(&self) -> String {
        if self.system.is_empty() {
            "yt-dlp".to_string()
        } else {
            self.system.clone()
        }
    }
}

/// Read a setting as a string, defaulting to empty when absent.
fn setting_str(store: &Store, key: &str) -> String {
    store.get_setting(key).ok().flatten().unwrap_or_default()
}

/// Load the configured yt-dlp binaries + SABR preset from settings, applying the
/// built-in fallbacks for any empty preset fields.
pub(crate) fn load_ytdlp_bins(store: &Store) -> YtDlpBins {
    let enabled = store
        .get_setting("ytdlp_sabr_enabled")
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true);
    let fmt = setting_str(store, "ytdlp_sabr_format");
    let xargs = setting_str(store, "ytdlp_sabr_extractor_args");
    // Experimental deep-rewind: when on, append `enable_live_deep_rewind=true` to
    // the youtube extractor-args so SABR can rewind past YouTube's normal ~4h DVR
    // window (lets capture-from-start reach the start of a long stream instead of
    // stalling with "not near live head"). Dev-build-only feature; the upstream
    // code reads only the literal lowercase `true`. Off by default — a stock
    // yt-dlp would silently ignore it, and the upstream author marks it unstable.
    let deep_rewind = store
        .get_setting("ytdlp_sabr_deep_rewind")
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false);
    // PO-token args: absent (never written) ⇒ the bgutil default; present (even
    // empty) ⇒ honor it verbatim, so the user can deliberately disable it.
    let pot_args = match store.get_setting("ytdlp_sabr_pot_args") {
        Ok(Some(v)) => v,
        _ => SABR_DEFAULT_POT_ARGS.to_string(),
    };
    // Global codec/quality preference. Absent/unknown ⇒ Auto (yt-dlp default),
    // preserving prior behavior. (Only the per-monitor field uses `Inherit`.)
    let codec_pref = match SabrCodecPref::parse(&setting_str(store, "ytdlp_sabr_codec_pref")) {
        SabrCodecPref::Inherit => SabrCodecPref::Auto,
        other => other,
    };
    YtDlpBins {
        system: setting_str(store, "ytdlp_binary_path"),
        sabr: SabrConfig {
            enabled,
            binary: setting_str(store, "ytdlp_sabr_binary_path"),
            format: if fmt.is_empty() { SABR_DEFAULT_FORMAT.to_string() } else { fmt },
            extractor_args: {
                let base = if xargs.is_empty() {
                    SABR_DEFAULT_EXTRACTOR_ARGS.to_string()
                } else {
                    xargs
                };
                // Append under the same `youtube:` namespace (`;`-separated).
                // Guard against a double-append if the user already added it to
                // the extractor-args field by hand.
                if deep_rewind && !base.contains("enable_live_deep_rewind") {
                    format!("{base};enable_live_deep_rewind=true")
                } else {
                    base
                }
            },
            raw_args: setting_str(store, "ytdlp_sabr_raw_args"),
            pot_args,
            codec_pref,
            codec_custom: setting_str(store, "ytdlp_sabr_codec_custom"),
        },
    }
}

/// Resolve a monitor's effective SABR format-sort (`-S` value): the monitor's own
/// codec preference, or the global default when the monitor is set to `Inherit`.
/// `""` = add no `-S` (yt-dlp's default codec preference).
fn resolve_sabr_sort(m: &Monitor, sabr: &SabrConfig) -> String {
    let (pref, custom) = if m.sabr_codec_pref == SabrCodecPref::Inherit {
        (sabr.codec_pref, sabr.codec_custom.as_str())
    } else {
        (m.sabr_codec_pref, m.sabr_codec_custom.as_str())
    };
    pref.sort_arg(custom)
}

/// Load the DASH-companion format selector (dual capture), with fallback.
fn load_dash_format(store: &Store) -> String {
    let f = setting_str(store, "ytdlp_dash_format");
    if f.is_empty() { DASH_DEFAULT_FORMAT.to_string() } else { f }
}

/// Resolved download authentication for a monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthSource {
    None,
    /// yt-dlp `--cookies-from-browser <browser>`.
    CookiesBrowser(String),
    /// yt-dlp `--cookies <path>`.
    CookiesFile(String),
    /// Twitch `--twitch-api-header=Authorization=OAuth <token>` (streamlink).
    Token(String),
}

/// Resolve the effective auth for a monitor from its override + the global default.
pub fn resolve_auth(
    m: &MonitorWithChannel,
    global_method: &str,
    global_browser: &str,
) -> AuthSource {
    resolve_auth_for(
        m.monitor.auth_kind,
        &m.monitor.auth_value,
        global_method,
        global_browser,
    )
}

/// Resolve an auth source from an `(auth_kind, auth_value)` pair plus the global
/// default — shared by monitors and on-demand videos.
pub fn resolve_auth_for(
    auth_kind: AuthKind,
    auth_value: &str,
    global_method: &str,
    global_browser: &str,
) -> AuthSource {
    let val = auth_value.trim();
    let browser = global_browser.trim();
    match auth_kind {
        AuthKind::Inherit => match global_method {
            "cookies" if !browser.is_empty() => AuthSource::CookiesBrowser(browser.to_string()),
            _ => AuthSource::None,
        },
        AuthKind::Disabled => AuthSource::None,
        AuthKind::CookiesBrowser => {
            let b = if val.is_empty() { browser } else { val };
            if b.is_empty() {
                AuthSource::None
            } else {
                AuthSource::CookiesBrowser(b.to_string())
            }
        }
        AuthKind::CookiesFile if !val.is_empty() => AuthSource::CookiesFile(val.to_string()),
        AuthKind::Token if !val.is_empty() => AuthSource::Token(val.to_string()),
        _ => AuthSource::None,
    }
}

/// Build the command + file plan for a monitor.
///
/// All tools capture to a progressively-flushed `.ts` (so an abrupt
/// kill/crash leaves usable data) and remux losslessly to `.mkv` on clean
/// stop. If the user picked the TS container, the `.ts` is kept as-is.
/// Append audio/subtitle track-selection flags appropriate to `tool`.
///
/// `audio`/`subs` are the per-monitor selectors: empty = the tool's default, a
/// case-insensitive `all` (or `*`) = every track, otherwise a comma-separated
/// pass-through list. Audio selection is a streamlink feature
/// (`--hls-audio-select`); subtitle capture is a yt-dlp feature (`--sub-langs`,
/// written as sidecar files next to the recording). Each tool ignores the
/// selector it can't honor — streamlink can't mux subtitles, and ffmpeg has no
/// per-track selector (its capture maps all video+audio tracks regardless of the
/// value; see the `Tool::Ffmpeg` arm of `build_plan`). Pushed before the user's
/// `extra_args` so a power user can still override. Selector values use the
/// `--flag=value` form so a value can never be mis-parsed as a separate option.
///
/// `chat` requests yt-dlp's `live_chat` pseudo-subtitle (YouTube chat), folded
/// into the same `--sub-langs` list. Twitch chat is captured separately by a
/// native logger (see `chat::log_twitch_chat`), so callers pass `chat = false`
/// for Twitch yt-dlp monitors.
pub(crate) fn push_track_args(args: &mut Vec<String>, tool: Tool, audio: &str, subs: &str, chat: bool) {
    let audio = audio.trim();
    let subs = subs.trim();
    let is_all = |s: &str| s.eq_ignore_ascii_case("all") || s == "*";
    match tool {
        Tool::Streamlink => {
            if !audio.is_empty() {
                let sel = if is_all(audio) { "*" } else { audio };
                args.push(format!("--hls-audio-select={sel}"));
            }
        }
        Tool::YtDlp => {
            // Combine subtitle languages and (optionally) the live-chat pseudo-
            // track into one --sub-langs list; both are written as sidecar files
            // (`.vtt` / `.live_chat.json`) next to the capture — a lossless,
            // replayable archive, NOT embedded into the container. The media-rename
            // step moves these companions so they stay matched to the final file
            // (see `rename_companion_sidecars`). `all`/`*` mean every subtitle.
            let mut langs: Vec<&str> = Vec::new();
            if !subs.is_empty() {
                langs.push(if is_all(subs) { "all" } else { subs });
            }
            if chat {
                langs.push("live_chat");
            } else {
                // live_chat blocks video download on live streams: yt-dlp downloads
                // the chat stream indefinitely before starting the video. Exclude it
                // so `all` doesn't pull it in. The dedicated chat sidecar
                // (run_chat_download) handles it when chat_log is enabled.
                langs.retain(|l| *l != "live_chat");
                if langs.iter().any(|l| *l == "all") {
                    langs.push("-live_chat");
                }
            }
            if !langs.is_empty() {
                args.push(format!("--sub-langs={}", langs.join(",")));
                args.push("--write-subs".into());
            }
        }
        Tool::Ffmpeg => {}
    }
}

/// Returns the URL yt-dlp should receive for a live YouTube recording.
/// Extract a YouTube video ID from a URL (`watch?v=`, `youtu.be/`, `/live/ID`).
/// Returns `None` for channel or handle URLs that don't embed a specific video ID.
fn extract_yt_video_id(url: &str) -> Option<String> {
    for marker in &["?v=", "&v="] {
        if let Some(pos) = url.find(marker) {
            let rest = &url[pos + marker.len()..];
            let id: String = rest.chars().take_while(|c| *c != '&' && *c != '#').collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    if let Some(pos) = url.find("youtu.be/") {
        let rest = &url[pos + "youtu.be/".len()..];
        let id: String = rest.chars().take_while(|c| *c != '?' && *c != '#' && *c != '/').collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    if let Some(pos) = url.find("/live/") {
        let rest = &url[pos + "/live/".len()..];
        let id: String = rest.chars().take_while(|c| *c != '?' && *c != '#' && *c != '/').collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

/// Channel URLs (/@handle, /channel/UC…, /c/name, /user/name) are resolved to
/// their /live variant so yt-dlp goes straight to the active stream instead of
/// enumerating the whole channel. Specific-video URLs (watch?v=, youtu.be/,
/// `/live/<id>`) and already-suffixed /live URLs are left unchanged.
pub(crate) fn youtube_live_url(url: &str) -> String {
    let u = url.trim_end_matches('/');
    let is_specific = u.contains("/watch?")
        || u.contains("/live/")
        || u.contains("youtu.be/")
        || u.ends_with("/live");
    if is_specific {
        url.to_string()
    } else {
        format!("{u}/live")
    }
}

/// Build a yt-dlp chat-only plan: `--skip-download --sub-langs=live_chat
/// --write-subs` with the same output path as the video so the `.live_chat.json`
/// sidecar lands next to it. Auth and global defaults are forwarded as-is so the
/// cookies / token work the same as they do for the video process.
pub fn build_chat_plan(
    row: &MonitorWithChannel,
    capture_path: &Path,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    system_program: &str,
) -> DownloadPlan {
    let mut args = vec!["--no-part".to_string()];
    match auth {
        AuthSource::CookiesBrowser(b) => {
            args.push("--cookies-from-browser".into());
            args.push(b.clone());
        }
        AuthSource::CookiesFile(p) => {
            args.push("--cookies".into());
            args.push(p.clone());
        }
        _ => {}
    }
    // Global defaults first so our required args below can override them.
    args.extend_from_slice(ytdlp_global_args);
    args.push("--skip-download".into());
    args.push("--sub-langs=live_chat".into());
    args.push("--write-subs".into());
    args.push("-o".into());
    args.push(capture_path.to_string_lossy().into_owned());
    let url = if row.monitor.platform() == Platform::YouTube {
        youtube_live_url(&row.monitor.url)
    } else {
        row.monitor.url.clone()
    };
    args.push(url);
    DownloadPlan {
        program: system_program.to_string(),
        args,
        capture_path: capture_path.to_path_buf(),
        final_path: capture_path.to_path_buf(),
        remux_to_mkv: false,
        writes_own_thumbnail: false,
        mode: "chat".into(),
    }
}

/// Build the DASH companion plan for dual capture. Mirrors the system yt-dlp live
/// path (.ts → MKV remux) but forces the configured DASH format, captures from the
/// live edge (`--no-live-from-start` — the SABR primary owns capture-from-start),
/// and writes a sibling `{stem}.dash.{ts,mkv}` next to the primary so both files
/// belong to the same take.
fn build_dash_companion_plan(
    primary_final: &Path,
    row: &MonitorWithChannel,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    system_program: &str,
    dash_format: &str,
    pot_args: &str,
) -> DownloadPlan {
    let dir = primary_final.parent().unwrap_or_else(|| Path::new("."));
    let stem = primary_final
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Capture into the hidden `.cache\` (promoted up on finish); final lands in dir.
    let ts_path = cache_dir(dir).join(format!("{stem}.dash.ts"));
    let mkv_path = dir.join(format!("{stem}.dash.mkv"));
    let mut args = vec![
        "--no-part".to_string(),
        "--hls-use-mpegts".into(),
        "-o".into(),
        ts_path.to_string_lossy().into_owned(),
        // DASH can't reliably rewind; the SABR primary owns capture-from-start.
        "--no-live-from-start".into(),
    ];
    match auth {
        AuthSource::CookiesBrowser(b) => {
            args.push("--cookies-from-browser".into());
            args.push(b.clone());
        }
        AuthSource::CookiesFile(p) => {
            args.push("--cookies".into());
            args.push(p.clone());
        }
        _ => {}
    }
    args.extend_from_slice(ytdlp_global_args);
    if !pot_args.is_empty() {
        args.push("--extractor-args".into());
        args.push(pot_args.to_string());
    }
    args.push("-f".into());
    args.push(dash_format.to_string());
    args.extend(split_args(&row.monitor.extra_args));
    args.push(youtube_live_url(&row.monitor.url));
    DownloadPlan {
        program: system_program.to_string(),
        args,
        capture_path: ts_path,
        final_path: mkv_path,
        remux_to_mkv: true,
        writes_own_thumbnail: false,
        mode: "hybrid-dash".into(),
    }
}

/// The hidden working directory for in-progress captures, a `.cache` subfolder of
/// the output dir (same volume, so promotion to the parent is a fast rename). The
/// `.`-prefix hides it on Unix; [`crate::platform::set_hidden`] adds the Windows
/// hidden attribute when the dir is created.
fn cache_dir(output_dir: &Path) -> PathBuf {
    output_dir.join(".cache")
}

/// Stale `.cache\` working files are swept after this age on startup.
const CACHE_MAX_AGE_SECS: u64 = 24 * 3600;

/// True if a recording's `.cache\` still holds SABR resume state (`.state` /
/// `.sq0.part` / `.part`) for its stem — i.e. an interrupted SABR capture that can
/// be continued. Derived synchronously from the recording's stored output path.
fn sabr_state_exists(output_path: &str) -> bool {
    let p = Path::new(output_path);
    let (Some(dir), Some(stem)) = (
        p.parent(),
        p.file_stem().map(|s| s.to_string_lossy().into_owned()),
    ) else {
        return false;
    };
    let prefix = format!("{stem}.");
    let Ok(rd) = std::fs::read_dir(cache_dir(dir)) else {
        return false;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix)
            && (name.ends_with(".state") || name.ends_with(".sq0.part") || name.ends_with(".part"))
        {
            return true;
        }
    }
    false
}

/// Build the yt-dlp SABR capture args, shared by [`build_plan`]'s SABR branch and
/// resume. Writes the final MKV directly to `out_mkv`; forces the SABR formats +
/// extractor-args (or the manual raw override); applies cookies, the global Settings
/// args, and the PO-token provider args. `from_start` selects `--live-from-start`
/// (rewind to the broadcast start) vs `--no-live-from-start` (join at the live
/// edge). `sort` is the resolved codec/quality `-S` value (`""` = none). All
/// inputs deterministic, so a resume re-runs byte-identically and yt-dlp
/// continues from the surviving `.state`.
#[allow(clippy::too_many_arguments)]
fn sabr_capture_args(
    out_mkv: &Path,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    sabr: &SabrConfig,
    extra: &[String],
    url: &str,
    from_start: bool,
    sort: &str,
) -> Vec<String> {
    let mut args = vec![
        "--no-part".to_string(),
        "-o".into(),
        out_mkv.to_string_lossy().into_owned(),
        if from_start { "--live-from-start" } else { "--no-live-from-start" }.into(),
    ];
    match auth {
        AuthSource::CookiesBrowser(b) => {
            args.push("--cookies-from-browser".into());
            args.push(b.clone());
        }
        AuthSource::CookiesFile(p) => {
            args.push("--cookies".into());
            args.push(p.clone());
        }
        _ => {}
    }
    // Global Settings args (e.g. --js-runtimes node) still apply.
    args.extend_from_slice(ytdlp_global_args);
    // PO-token provider args (e.g. bgutil) — a separate --extractor-args entry,
    // applied regardless of the preset/raw choice below.
    if !sabr.pot_args.is_empty() {
        args.push("--extractor-args".into());
        args.push(sabr.pot_args.clone());
    }
    // Manual raw args override the format + extractor-args preset entirely.
    let raw = split_args(&sabr.raw_args);
    if raw.is_empty() {
        args.push("--extractor-args".into());
        args.push(sabr.extractor_args.clone());
        args.push("-f".into());
        args.push(sabr.format.clone());
    } else {
        args.extend(raw);
    }
    // Codec/quality preference: a `-S` sort layered on the `-f` selector, so it
    // only decides which format each `b*` selector resolves to. Before `extra`
    // so the dropdown wins over any user `-S` in the monitor's extra args.
    if !sort.is_empty() {
        args.push("-S".into());
        args.push(sort.to_string());
    }
    args.extend_from_slice(extra);
    args.push(youtube_live_url(url));
    args
}

/// Build the yt-dlp SABR args for a throwaway live-edge preview download
/// ("Play new instance"): identical to [`sabr_capture_args`] except it joins
/// at the live edge instead of rewinding to the start — the whole point is
/// that the preview files BEGIN at the edge, so the player needs no seeking —
/// and it prefers fMP4-compatible formats: the preview is served to the
/// player through a generated live HLS playlist of byteranges
/// ([`crate::hls_preview`]), which requires ISOBMFF per-format files (a VP9
/// pick lands in a Matroska container HLS can't address). Falls back to the
/// configured selector when no mp4+m4a pair exists (playback then degrades
/// to `appending://`, which stalls once caught up to the live edge).
pub(crate) fn sabr_preview_args(
    out_mkv: &Path,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    sabr: &SabrConfig,
    extra: &[String],
    url: &str,
) -> Vec<String> {
    // Live edge (`from_start=false`): the preview files BEGIN at the edge. No
    // codec `-S` here — the preview forces its own fMP4 `-f` for HLS-playlist
    // playback below, which a codec sort could fight.
    let mut args = sabr_capture_args(out_mkv, auth, ytdlp_global_args, sabr, extra, url, false, "");
    if let Some(pos) = args.iter().position(|a| a == "-f")
        && let Some(v) = args.get_mut(pos + 1)
    {
        *v = format!("bv[protocol=sabr][ext=mp4]+ba[protocol=sabr][ext=m4a]/{v}");
    }
    args
}

/// Whether a monitor's YouTube live capture goes through the SABR dev build.
///
/// SABR is used for **all** YouTube yt-dlp live captures (from-start AND live
/// edge), not just from-start: YouTube now serves live via SABR, and the system
/// build's default clients return "No video formats found" at the live edge.
/// `capture_from_start` controls only from-start-vs-edge (see `sabr_capture_args`),
/// not whether SABR is used. Requires the SABR dev build to be configured.
fn sabr_selected(m: &Monitor, ytdlp: &YtDlpBins) -> bool {
    m.tool == Tool::YtDlp && m.platform() == Platform::YouTube && ytdlp.sabr.usable()
}

/// Compute the download mode string for a monitor recording.
fn recording_mode(m: &Monitor, use_sabr: bool, secondary: bool) -> String {
    match m.tool {
        Tool::Streamlink => "live".into(),
        Tool::Ffmpeg => "direct".into(),
        Tool::YtDlp => {
            if use_sabr {
                if m.dual_capture {
                    if secondary { "hybrid-dash".into() } else { "hybrid".into() }
                } else {
                    "sabr".into()
                }
            } else {
                "dash".into()
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_plan(
    row: &MonitorWithChannel,
    started_at: i64,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    stream_id: Option<&str>,
    stream_title: &str,
    media: Option<&MediaInfo>,
    went_live_at: i64,
    ytdlp: &YtDlpBins,
) -> DownloadPlan {
    let m = &row.monitor;
    let ch = &row.channel;
    let dir = PathBuf::from(&m.output_dir);
    let quality = resolved_quality(&m.quality);
    let use_sabr = sabr_selected(m, ytdlp);
    let mode = recording_mode(m, use_sabr, false);
    let platform = m.platform().as_str().to_string();
    let tool_label = m.tool.label();
    // `{video_id}` (platform id when known), `{take}` (attempt number), and the
    // media vars (filled only when `media` is provided, i.e. pre-probe). Then
    // avoid clobbering an existing finished file of the same name.
    // `{games}` isn't known until the stream ends; it's filled at the post-rename.
    let stem = monitor_stem(
        m, &ch.name, started_at, stream_id, stream_title, row.recording_count, &quality, media, "",
        tool_label, &mode, &platform, went_live_at,
    );
    let extra = split_args(&m.extra_args);
    // SABR (YouTube capture-from-start via the dev build) writes the final MKV
    // directly — it merges separate SABR audio+video through ffmpeg, which the
    // mpegts/.ts intermediate can't hold. Everything else captures to .ts first.
    let final_ext = if use_sabr {
        "mkv"
    } else {
        match m.container {
            Container::Mkv => "mkv",
            Container::Ts => "ts",
        }
    };
    let stem = unique_stem(&dir, &stem, final_ext, None);

    // Working files capture into the hidden `.cache\` subdir; the finished file is
    // promoted up to the output dir on a clean finalize (same-volume rename).
    let cache = cache_dir(&dir);
    let (capture_path, final_path, remux_to_mkv) = if use_sabr {
        // SABR writes the final MKV directly (no .ts intermediate); promoted via a
        // move on finish.
        (cache.join(format!("{stem}.mkv")), dir.join(format!("{stem}.mkv")), false)
    } else {
        match m.container {
            Container::Mkv => (
                cache.join(format!("{stem}.ts")),
                dir.join(format!("{stem}.mkv")),
                true,
            ),
            Container::Ts => (
                cache.join(format!("{stem}.ts")),
                dir.join(format!("{stem}.ts")),
                false,
            ),
        }
    };
    let cap_str = capture_path.to_string_lossy().into_owned();

    let mut writes_own_thumbnail = false;
    let (program, args) = match m.tool {
        Tool::Streamlink => {
            let mut args = Vec::new();
            if m.platform() == Platform::Twitch {
                // Reach 1440p/2K (HEVC) enhanced-broadcasting sources.
                args.push("--twitch-supported-codecs=h264,h265,av1".to_string());
                // Authenticated capture (sub-only / Turbo ad-free) via the token.
                if let AuthSource::Token(t) = auth {
                    args.push(format!("--twitch-api-header=Authorization=OAuth {t}"));
                }
            }
            if m.capture_from_start {
                // Rewind to the start of the DVR window (best-effort on Twitch).
                args.push("--hls-live-restart".into());
            }
            args.push("--retry-streams".into());
            args.push("3".into());
            args.push("--retry-max".into());
            args.push("5".into());
            push_track_args(&mut args, Tool::Streamlink, &m.audio_tracks, &m.subtitle_tracks, false);
            args.extend(extra);
            args.push("-o".into());
            args.push(cap_str);
            args.push(m.url.clone());
            args.push(quality);
            ("streamlink".to_string(), args)
        }
        Tool::YtDlp if use_sabr => {
            // SABR capture via the dev build: writes the final MKV directly (SABR
            // merges separate audio+video, so no mpegts/.ts). Chat, assets, and
            // thumbnails are handled off this process (the dev build is a stale fork
            // — keep its surface minimal): no --write-thumbnail / --sub-langs here.
            let args = sabr_capture_args(
                &capture_path, auth, ytdlp_global_args, &ytdlp.sabr, &extra, &m.url,
                m.capture_from_start, &resolve_sabr_sort(m, &ytdlp.sabr),
            );
            (ytdlp.sabr.binary.clone(), args)
        }
        Tool::YtDlp => {
            let mut args = vec![
                "--no-part".to_string(),
                "--hls-use-mpegts".into(), // progressive .ts output
                "-o".into(),
                cap_str,
            ];
            if m.capture_from_start {
                args.push("--live-from-start".into());
            } else {
                args.push("--no-live-from-start".into());
            }
            // Authenticated capture (members-only / sub-only) via cookies.
            match auth {
                AuthSource::CookiesBrowser(b) => {
                    args.push("--cookies-from-browser".into());
                    args.push(b.clone());
                }
                AuthSource::CookiesFile(p) => {
                    args.push("--cookies".into());
                    args.push(p.clone());
                }
                _ => {}
            }
            // Global defaults from Settings → yt-dlp default arguments.
            // Per-monitor extra_args extend after and can override these.
            args.extend_from_slice(ytdlp_global_args);
            // PO-token provider (bgutil HTTP) — YouTube requires a proof-of-origin
            // token for video formats regardless of whether SABR is in use.
            if m.platform() == Platform::YouTube && !ytdlp.sabr.pot_args.is_empty() {
                args.push("--extractor-args".into());
                args.push(ytdlp.sabr.pot_args.clone());
            }
            // Never request live_chat for live recordings: yt-dlp's live_chat
            // downloader runs until the stream ends and blocks video download in the
            // same process. YouTube chat replay can be downloaded after the stream
            // via build_video_plan (which does pass chat_log through). Twitch chat
            // uses the native WS logger regardless.
            push_track_args(&mut args, Tool::YtDlp, &m.audio_tracks, &m.subtitle_tracks, false);
            if m.fetch_thumbnail {
                args.push("--write-thumbnail".to_string());
                writes_own_thumbnail = true;
            }
            args.extend(extra);
            let url = if m.platform() == Platform::YouTube {
                youtube_live_url(&m.url)
            } else {
                m.url.clone()
            };
            args.push(url);
            (ytdlp.system_program(), args)
        }
        Tool::Ffmpeg => {
            let mut args = vec![
                "-y".to_string(),
                "-i".into(),
                m.url.clone(),
                // Keep all video + audio tracks (ffmpeg's default copies only one
                // per type). TS can't reliably hold text subtitles, so subs are
                // left to the MKV remux.
                "-map".into(),
                "0:v?".into(),
                "-map".into(),
                "0:a?".into(),
                "-c".into(),
                "copy".into(),
            ];
            args.extend(extra);
            args.push(cap_str);
            ("ffmpeg".to_string(), args)
        }
    };

    DownloadPlan {
        program,
        args,
        capture_path,
        final_path,
        remux_to_mkv,
        writes_own_thumbnail,
        mode,
    }
}

/// Build the command + file plan for an on-demand video/VOD download.
///
/// Output is always MKV: yt-dlp downloads the full video and remuxes to MKV
/// directly; streamlink/ffmpeg capture to `.ts` then remux losslessly. Unlike
/// [`build_plan`], there are no live-stream flags (`--live-from-start`,
/// `--retry-streams`).
pub fn build_video_plan(
    v: &Video,
    started_at: i64,
    title: &str,
    channel: &str,
    video_id: &str,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    media: Option<&MediaInfo>,
    ytdlp: &YtDlpBins,
) -> DownloadPlan {
    let dir = PathBuf::from(&v.output_dir);
    let quality = resolved_quality(&v.quality);
    let stem = video_stem(v, started_at, title, channel, video_id, &quality, media, v.tool.label(), Platform::detect(&v.url).as_str());
    let extra = split_args(&v.extra_args);
    let platform = Platform::detect(&v.url);
    // Don't clobber an existing finished file (all video tools end at .mkv).
    let stem = unique_stem(&dir, &stem, "mkv", None);
    let final_path = dir.join(format!("{stem}.mkv"));
    // Working files capture into the hidden `.cache\`; promoted up on finish.
    let cache = cache_dir(&dir);

    match v.tool {
        Tool::YtDlp => {
            // yt-dlp downloads the complete video and remuxes to MKV. `%(ext)s`
            // becomes `mkv` after the remux, so the cache file is predictable.
            let out_tmpl = cache
                .join(format!("{stem}.%(ext)s"))
                .to_string_lossy()
                .into_owned();
            let mut args = vec![
                "--no-part".to_string(),
                "--no-playlist".into(),
                "--merge-output-format".into(),
                "mkv".into(),
                "--remux-video".into(),
                "mkv".into(),
                // Emit a parseable percent + speed per line for the UI progress
                // bar and Speed column (`;;` separates the two fields).
                "--newline".into(),
                "--progress-template".into(),
                "download:DLPCT=%(progress._percent_str)s;;SPEED=%(progress.speed)s".into(),
                "-o".into(),
                out_tmpl,
            ];
            if quality != "best" {
                args.push("-f".into());
                args.push(quality);
            }
            match auth {
                AuthSource::CookiesBrowser(b) => {
                    args.push("--cookies-from-browser".into());
                    args.push(b.clone());
                }
                AuthSource::CookiesFile(p) => {
                    args.push("--cookies".into());
                    args.push(p.clone());
                }
                _ => {}
            }
            // The stock build's default client mix is broken for YouTube VODs
            // (2026-07): tv_downgraded 403s even with a valid PO token, tv
            // serves DRM formats, web/web_safari are SABR-only (dropped as
            // unsupported), ios/android serve nothing — mweb + a GVS PO token
            // is the mix that downloads. First in the args so later
            // --extractor-args youtube:… entries (Settings global defaults or
            // per-video extra args) can override it if the landscape shifts.
            if platform == Platform::YouTube {
                args.push("--extractor-args".into());
                args.push("youtube:player_client=mweb".into());
            }
            // Global defaults from Settings → yt-dlp default arguments.
            args.extend_from_slice(ytdlp_global_args);
            // PO-token provider (bgutil HTTP) — YouTube 403s googlevideo media
            // URLs fetched without a proof-of-origin token, so VOD downloads
            // need it exactly like live captures do (the extraction step alone
            // still succeeds, which made these failures easy to miss).
            if platform == Platform::YouTube && !ytdlp.sabr.pot_args.is_empty() {
                args.push("--extractor-args".into());
                args.push(ytdlp.sabr.pot_args.clone());
            }
            // Subtitle + chat (live_chat) sidecars; the post-rename step moves them
            // with the file. audio_tracks is a no-op for yt-dlp (it keeps the
            // chosen format's tracks).
            push_track_args(
                &mut args,
                Tool::YtDlp,
                &v.audio_tracks,
                &v.subtitle_tracks,
                v.chat_log,
            );
            args.extend(extra);
            args.push(v.url.clone());
            DownloadPlan {
                program: ytdlp.system_program(),
                args,
                // yt-dlp writes the final MKV into .cache; promoted up via a move.
                capture_path: cache.join(format!("{stem}.mkv")),
                final_path,
                remux_to_mkv: false,
                writes_own_thumbnail: false,
                mode: "vod".into(),
            }
        }
        Tool::Streamlink => {
            let ts_path = cache.join(format!("{stem}.ts"));
            let mut args = Vec::new();
            if platform == Platform::Twitch {
                args.push("--twitch-supported-codecs=h264,h265,av1".to_string());
                if let AuthSource::Token(t) = auth {
                    args.push(format!("--twitch-api-header=Authorization=OAuth {t}"));
                }
            }
            // Audio-track selection (streamlink can't mux subtitles/chat).
            push_track_args(
                &mut args,
                Tool::Streamlink,
                &v.audio_tracks,
                &v.subtitle_tracks,
                v.chat_log,
            );
            args.extend(extra);
            args.push("-o".into());
            args.push(ts_path.to_string_lossy().into_owned());
            args.push(v.url.clone());
            args.push(quality);
            DownloadPlan {
                program: "streamlink".to_string(),
                args,
                capture_path: ts_path,
                final_path,
                remux_to_mkv: true,
                writes_own_thumbnail: false,
                mode: "vod".into(),
            }
        }
        Tool::Ffmpeg => {
            let ts_path = cache.join(format!("{stem}.ts"));
            let mut args = vec![
                "-y".to_string(),
                "-i".into(),
                v.url.clone(),
                // Keep all video + audio tracks (ffmpeg's default copies only one
                // per type); the MKV remux below preserves them.
                "-map".into(),
                "0:v?".into(),
                "-map".into(),
                "0:a?".into(),
                "-c".into(),
                "copy".into(),
            ];
            args.extend(extra);
            args.push(ts_path.to_string_lossy().into_owned());
            DownloadPlan {
                program: "ffmpeg".to_string(),
                args,
                capture_path: ts_path,
                final_path,
                remux_to_mkv: true,
                writes_own_thumbnail: false,
                mode: "vod".into(),
            }
        }
    }
}

#[derive(Clone)]
pub struct Supervisor {
    store: Arc<Store>,
    events: EventTx,
    active: ActiveSet,
    /// monitor_id -> child PID of the DASH companion capture (dual capture). The
    /// primary capture occupies `active`; this holds the second concurrent process
    /// so manual stop / shutdown can kill both.
    active_secondary: ActiveSet,
    /// video_id -> child PID of in-flight on-demand video downloads.
    active_videos: ActiveSet,
    /// video_id -> live download progress fraction, for the UI bar.
    video_progress: VideoProgress,
    /// video_id -> live download speed (bytes/sec), for the UI Speed column.
    video_speed: VideoSpeed,
    /// video_ids whose download was asked to stop (so it finalizes as `stopped`).
    stopping_videos: Arc<Mutex<HashSet<i64>>>,
    /// monitor_ids whose live recording was manually stopped (so it finalizes as
    /// `stopped` rather than `failed` even when bytes = 0).
    stopping_monitors: Arc<Mutex<HashSet<i64>>>,
    /// monitor_id -> child PID of in-flight live-chat sidecar downloads.
    /// Shared with AppCore so the UI can show the chat-active indicator.
    pub active_chats: Arc<Mutex<HashMap<i64, u32>>>,
    /// monitor_ids whose chat download was asked to stop.
    stopping_chats: Arc<Mutex<HashSet<i64>>>,
    shutdown: Arc<AtomicBool>,
    /// A clone of the manual-command sender, so background poller tasks (the VOD
    /// archivers) can enqueue+start a Video download without a Supervisor handle.
    manual_tx: mpsc::UnboundedSender<ManualCommand>,
    /// Shared detection context for on-demand (manual Start) liveness checks.
    ctx: Arc<DetectContext>,
    /// monitor_id -> unix time the current ad break ends (for the UI row tint).
    ad_active: AdActive,
    sem: Arc<Semaphore>,
    backoff: Arc<Mutex<HashMap<i64, BackoffEntry>>>,
    /// Streams where SABR live-from-start hit the DVR window limit ("not near
    /// live head"). The next attempt falls back to live-edge so we at least
    /// capture the ongoing stream instead of looping forever. Cleared on a
    /// successful capture (bytes > 0); in-memory only (resets on app restart).
    ///
    /// Keyed by `(monitor_id, stream_id)`. When a stable video ID is available
    /// (scraped from `videoDetails.videoId` or the YouTube Data API), the key is
    /// fully per-stream, so cross-broadcast stickiness cannot occur. When
    /// `stream_id` is `None` (non-YouTube monitors or degraded scrape), the key
    /// degrades to per-monitor — same as the previous behaviour.
    sabr_dvr_exceeded: Arc<Mutex<HashSet<SabrKey>>>,
    /// Per-stream count of *consecutive* from-start SABR stalls. Once it reaches
    /// [`SABR_STALL_FALLBACK_TRIES`] (with deep-rewind on) the stream is added to
    /// `sabr_dvr_exceeded` so the next attempt captures the live edge. Reset on
    /// any non-stall outcome (success, ended, manual) so it tracks back-to-back
    /// stalls only; in-memory only. Keyed by `(monitor_id, stream_id)`.
    sabr_stall_count: Arc<Mutex<HashMap<SabrKey, u32>>>,
    /// (channel_name, platform_str) pairs for which an asset-fetch task is currently
    /// in flight. Prevents stacking duplicate fetches when the user clicks
    /// "Re-fetch" repeatedly or a periodic fetch fires while one is already running.
    running_asset_fetches: Arc<Mutex<HashSet<(String, String)>>>,
}

#[derive(Clone, Copy)]
struct BackoffEntry {
    fails: u32,
    until: Instant,
}

impl Supervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        events: EventTx,
        active: ActiveSet,
        active_videos: ActiveSet,
        video_progress: VideoProgress,
        video_speed: VideoSpeed,
        active_chats: Arc<Mutex<HashMap<i64, u32>>>,
        shutdown: Arc<AtomicBool>,
        manual_tx: mpsc::UnboundedSender<ManualCommand>,
        ctx: Arc<DetectContext>,
        ad_active: AdActive,
        max_concurrent: usize,
    ) -> Supervisor {
        Supervisor {
            store,
            events,
            active,
            active_secondary: Arc::new(Mutex::new(HashMap::new())),
            active_videos,
            video_progress,
            video_speed,
            stopping_videos: Arc::new(Mutex::new(HashSet::new())),
            stopping_monitors: Arc::new(Mutex::new(HashSet::new())),
            active_chats,
            stopping_chats: Arc::new(Mutex::new(HashSet::new())),
            shutdown,
            manual_tx,
            ctx,
            ad_active,
            sem: Arc::new(Semaphore::new(max_concurrent.max(1))),
            backoff: Arc::new(Mutex::new(HashMap::new())),
            sabr_dvr_exceeded: Arc::new(Mutex::new(HashSet::new())),
            sabr_stall_count: Arc::new(Mutex::new(HashMap::new())),
            running_asset_fetches: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Consume live signals (from detectors) and manual Start/Stop commands.
    pub async fn run(
        self,
        mut live_rx: mpsc::UnboundedReceiver<LiveSignal>,
        mut manual_rx: mpsc::UnboundedReceiver<ManualCommand>,
    ) {
        loop {
            tokio::select! {
                Some(signal) = live_rx.recv() => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        continue; // draining: don't start new recordings
                    }
                    self.try_begin(signal.monitor_id, signal.went_live_at, signal.approximate, signal.stream_id, signal.thumbnail_url, signal.broadcaster_id, signal.stream_title, false);
                }
                Some(cmd) = manual_rx.recv() => match cmd {
                    ManualCommand::Start { id, notify_offline } => {
                        let this = self.clone();
                        tokio::spawn(async move { this.manual_start(id, notify_offline).await });
                    }
                    ManualCommand::Stop(id) => self.manual_stop(id),
                    ManualCommand::StartVideo(id) => {
                        if !self.shutdown.load(Ordering::SeqCst) {
                            let this = self.clone();
                            tokio::spawn(async move { this.start_video(id).await });
                        }
                    }
                    ManualCommand::StopVideo(id) => self.stop_video(id),
                    ManualCommand::StopChat(id) => self.stop_chat_download(id),
                    ManualCommand::RefetchAssets(id) => {
                        if let Ok(Some(row)) = self.store.get_monitor_with_channel(id) {
                            // Manual: bypass the 24h stamp + the fetch_chat_assets
                            // toggle, and resolve the platform id from the URL.
                            self.fetch_channel_assets(&row, None, true);
                        }
                    }
                    ManualCommand::ReRemux { rec_id, capture, final_ } => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        let task_id = rec_id as u64;
                        let src_name = capture
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let dst_name = final_
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let _ = tx.send(AppEvent::BackgroundTaskStarted(
                            crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::Remux,
                                label: src_name,
                                detail: format!("→ {dst_name}"),
                                started_at: now_unix(),
                                progress: None,
                progress_info: None,
                            },
                        ));
                        let tx2 = tx.clone();
                        tokio::spawn(async move {
                            info!("re-remux start: {}", capture.display());
                            // ffmpeg writes the destination directly, so shorten
                            // proactively — this also covers a Re-remux retry after
                            // the FIRST attempt failed because this exact name was
                            // too long (see path_with_safe_stem).
                            let final_ = path_with_safe_stem(&final_);
                            match remux_ts_to_mkv(&capture, &final_, Some((tx2, task_id)), &Default::default()).await {
                                Ok(()) => {
                                    let _ = tokio::fs::remove_file(&capture).await;
                                    let path_s = final_.to_string_lossy();
                                    if let Err(e) = store.update_recording_output_path(rec_id, &path_s) {
                                        warn!("re-remux: DB update failed for rec_id={rec_id}: {e:#}");
                                    }
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Completed,
                                    });
                                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                                    info!("re-remux done: {}", final_.display());
                                }
                                Err(e) => {
                                    warn!("re-remux failed for rec_id={rec_id}: {e:#}");
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                                    });
                                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                                }
                            }
                        });
                    }
                    ManualCommand::ReRemuxAll => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::ReRemuxAll,
                                label: "Re-remux all".into(),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: Some(0.0),
                                progress_info: None,
                            }));
                            let recs = match store.list_recordings_with_mkv() {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                                    });
                                    return;
                                }
                            };
                            let total = recs.len();
                            let mut done = 0usize;
                            let opts = store.remux_opts();
                            for (rec_id, output_path) in &recs {
                                let planned_mkv = PathBuf::from(output_path);
                                // The sibling .ts (the actual source to remux) lives
                                // under the ORIGINAL stem — only the destination we're
                                // about to write gets proactively shortened.
                                let ts = planned_mkv.with_extension("ts");
                                if !ts.exists() {
                                    done += 1;
                                    continue;
                                }
                                let mkv = path_with_safe_stem(&planned_mkv);
                                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                    id: task_id,
                                    progress: Some(done as f32 / total as f32),
                                    info: format!("{}/{total}: {}", done + 1, mkv.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
                                });
                                match remux_ts_to_mkv(&ts, &mkv, None, &opts).await {
                                    Ok(()) => {
                                        let _ = tokio::fs::remove_file(&ts).await;
                                        if mkv != planned_mkv {
                                            let _ = store.update_recording_output_path(*rec_id, &mkv.to_string_lossy());
                                        }
                                        let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                                    }
                                    Err(e) => warn!("re-remux-all failed for rec_id={rec_id}: {e:#}"),
                                }
                                done += 1;
                            }
                            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                id: task_id,
                                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
                            });
                        });
                    }
                    ManualCommand::RecoverVod { inputs, quality, sink, probe_all } => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        let client = self.ctx.http_client();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            crate::recovery::run_recovery(
                                client, store, tx, inputs, quality, sink, probe_all, task_id,
                            )
                            .await;
                        });
                    }
                    ManualCommand::ScanRecoverableVods { window_days, quality } => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        let client = self.ctx.http_client();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::RecoverVodScan,
                                label: "VOD recovery scan".into(),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: Some(0.0),
                                progress_info: None,
                            }));
                            let within = window_days.max(1) * 86_400;
                            let takes = store
                                .recordings_recoverable(within, now_unix())
                                .unwrap_or_default();
                            let total = takes.len();
                            // Bound concurrent recoveries; each keeps its own inner
                            // segment-HEAD semaphore, so total load stays sane.
                            let sem = Arc::new(Semaphore::new(2));
                            let mut set: JoinSet<()> = JoinSet::new();
                            for take in takes {
                                let Some(login) = crate::detectors::twitch_login(&take.monitor_url)
                                else {
                                    continue;
                                };
                                let (client, sem, store, tx, quality) = (
                                    client.clone(),
                                    sem.clone(),
                                    store.clone(),
                                    tx.clone(),
                                    quality.clone(),
                                );
                                set.spawn(async move {
                                    let _permit = sem.acquire().await.expect("semaphore");
                                    let sub = crate::events::next_task_id();
                                    let inputs = crate::recovery::RecoveryInputs {
                                        login,
                                        broadcast_id: take.stream_id,
                                        start_epoch: take.start_epoch,
                                        went_live_approx: take.went_live_approx,
                                        vod_id: take.vod_id,
                                    };
                                    crate::recovery::run_recovery(
                                        client,
                                        store,
                                        tx,
                                        inputs,
                                        quality,
                                        crate::recovery::RecoverySink::Recording(take.rec_id),
                                        take.deleted,
                                        sub,
                                    )
                                    .await;
                                });
                            }
                            while set.join_next().await.is_some() {}
                            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                id: task_id,
                                outcome: crate::events::TaskOutcome::CompletedWithNote(format!(
                                    "{total} recording(s) processed"
                                )),
                            });
                        });
                    }
                    ManualCommand::ArchiveVodNow(rec_id) => {
                        let store = self.store.clone();
                        let manual_tx = self.manual_tx.clone();
                        let ctx = self.ctx.clone();
                        tokio::spawn(async move {
                            let Ok(Some((murl, vod_id, stream_id, went_live))) =
                                store.recording_archive_now(rec_id)
                            else {
                                return;
                            };
                            let url = match Platform::detect(&murl) {
                                Platform::Twitch => vod_id
                                    .filter(|v| !v.is_empty())
                                    .map(|v| crate::vod_archive::twitch_vod_url(&v)),
                                Platform::YouTube => stream_id
                                    .filter(|s| !s.is_empty())
                                    .map(|s| crate::vod_archive::youtube_vod_url(&s)),
                                Platform::Kick => match crate::vod_archive::kick_slug(&murl) {
                                    Some(slug) => {
                                        crate::vod_archive::resolve_kick_vod(
                                            &ctx.http_client(),
                                            &slug,
                                            went_live,
                                        )
                                        .await
                                    }
                                    None => None,
                                },
                                _ => None,
                            };
                            match url {
                                Some(u) => {
                                    enqueue_vod_archive(&store, &manual_tx, rec_id, &u);
                                }
                                None => {
                                    let _ = store.set_recording_vod_dl(rec_id, "failed", None);
                                }
                            }
                        });
                    }
                    ManualCommand::RefreshCdnHosts => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        let client = self.ctx.http_client();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::RefreshCdnHosts,
                                label: "Refresh CDN hosts".into(),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: Some(0.0),
                                progress_info: None,
                            }));
                            let vod_ids = store.published_vod_ids(300).unwrap_or_default();
                            let (learned, checked) =
                                crate::recovery::harvest_hosts(&store, &client, &vod_ids).await;
                            let total = crate::recovery::load_hosts(&store).len();
                            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                id: task_id,
                                outcome: crate::events::TaskOutcome::CompletedWithNote(format!(
                                    "{learned} new host(s) from {checked} VOD(s) · {total} known"
                                )),
                            });
                        });
                    }
                    ManualCommand::RecoverStuckCapture { rec_id, capture, output_dir } => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            let Some(stem) =
                                capture.file_stem().map(|s| s.to_string_lossy().into_owned())
                            else {
                                warn!(rec_id, "recover stuck capture: no file stem for {}", capture.display());
                                return;
                            };
                            let ext = capture
                                .extension()
                                .map(|e| e.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let _ = tokio::fs::create_dir_all(&output_dir).await;
                            match rename_or_shorten(&capture, &output_dir, &stem, &ext).await {
                                Ok(actual) => {
                                    if let Err(e) = store
                                        .update_recording_output_path(rec_id, &actual.to_string_lossy())
                                    {
                                        warn!(rec_id, "recover stuck capture: DB update failed: {e:#}");
                                    }
                                    info!(rec_id, "recovered stuck capture -> {}", actual.display());
                                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                                }
                                Err(e) => warn!(rec_id, "recover stuck capture failed: {e:#}"),
                            }
                        });
                    }
                    ManualCommand::EmbedMissingThumbnails => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::EmbedMissingThumbnails,
                                label: "Embed missing thumbnails".into(),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: Some(0.0),
                                progress_info: None,
                            }));
                            let recs = match store.list_recordings_with_mkv() {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                                    });
                                    return;
                                }
                            };
                            let total = recs.len();
                            let mut embedded = 0usize;
                            for (i, (rec_id, output_path)) in recs.iter().enumerate() {
                                let mkv = PathBuf::from(output_path);
                                if !mkv.exists() { continue; }
                                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                    id: task_id,
                                    progress: Some(i as f32 / total as f32),
                                    info: format!("{}/{total}: {}", i + 1, mkv.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
                                });
                                let mkv2 = mkv.clone();
                                let has = tokio::task::spawn_blocking(move || mkv_has_thumbnail(&mkv2)).await.unwrap_or(false);
                                if has { continue; }
                                if let Some(thumb) = find_thumbnail_for(&mkv) {
                                    match embed_thumbnail_into_mkv(&mkv, &thumb).await {
                                        Ok(()) => {
                                            embedded += 1;
                                            let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                                        }
                                        Err(e) => warn!("embed-thumbnail failed for rec_id={rec_id}: {e:#}"),
                                    }
                                }
                            }
                            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                id: task_id,
                                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{embedded} embedded")),
                            });
                        });
                    }
                    ManualCommand::FetchMissingThumbnails { embed } => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::FetchMissingThumbnails,
                                label: "Fetch missing thumbnails".into(),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: Some(0.0),
                                progress_info: None,
                            }));
                            let recs = match store.list_recordings_with_stream_id() {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                                    });
                                    return;
                                }
                            };
                            let total = recs.len();
                            let mut fetched = 0usize;
                            for (i, (rec_id, output_path, _stream_id)) in recs.iter().enumerate() {
                                let output = PathBuf::from(output_path);
                                if !output.exists() { continue; }
                                // Skip if a thumbnail sidecar already exists.
                                if find_thumbnail_for(&output).is_some() { continue; }
                                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                    id: task_id,
                                    progress: Some(i as f32 / total as f32),
                                    info: format!("{}/{total}: {}", i + 1, output.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
                                });
                                // We don't have a standalone thumbnail-fetch API here;
                                // log a note for now — actual YouTube thumbnail fetching
                                // requires the YT API helpers which live in detectors.rs.
                                info!("fetch-missing-thumbnails: rec_id={rec_id} has no thumbnail sidecar (manual implementation required per-platform)");
                                if embed {
                                    if let Some(thumb) = find_thumbnail_for(&output) {
                                        if let Err(e) = embed_thumbnail_into_mkv(&output, &thumb).await {
                                            warn!("embed after fetch failed rec_id={rec_id}: {e:#}");
                                        } else {
                                            fetched += 1;
                                            let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                                        }
                                    }
                                }
                            }
                            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                id: task_id,
                                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{fetched} processed")),
                            });
                        });
                    }
                    ManualCommand::ReorganizeAll => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::ReorganizeAll,
                                label: "Re-organize all".into(),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: Some(0.0),
                                progress_info: None,
                            }));
                            let cfg = store.subdir_config();
                            let ids = match store.list_all_recording_ids() {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                                    });
                                    return;
                                }
                            };
                            let total = ids.len();
                            for (i, rec_id) in ids.iter().enumerate() {
                                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                    id: task_id,
                                    progress: Some(i as f32 / total.max(1) as f32),
                                    info: format!("{}/{total}", i + 1),
                                });
                                let reverse = !cfg.enabled;
                                match reorganize_recording_files(*rec_id, &store, &cfg, reverse).await {
                                    Ok(Some(_)) => { let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id }); }
                                    Ok(None) => {}
                                    Err(e) => warn!("reorganize-all rec_id={rec_id}: {e:#}"),
                                }
                            }
                            // Second pass: sweep every monitor output directory for companion
                            // files that aren't linked to any recording (e.g. chat logs from
                            // recordings that failed before an output_path was set).
                            if cfg.enabled {
                                if let Ok(dirs) = store.list_monitor_output_dirs() {
                                    for dir in dirs {
                                        sweep_companion_files(std::path::Path::new(&dir), &cfg).await;
                                    }
                                }
                            }
                            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                id: task_id,
                                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
                            });
                        });
                    }
                    ManualCommand::ReorganizeTake(rec_id) => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::ReorganizeTake(rec_id),
                                label: format!("Re-organize recording #{rec_id}"),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: None,
                                progress_info: None,
                            }));
                            let cfg = store.subdir_config();
                            let reverse = !cfg.enabled;
                            match reorganize_recording_files(rec_id, &store, &cfg, reverse).await {
                                Ok(_) => {
                                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Completed,
                                    });
                                }
                                Err(e) => {
                                    warn!("reorganize take {rec_id}: {e:#}");
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                                    });
                                }
                            }
                        });
                    }
                    ManualCommand::ReorganizeMonitor(mid) => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::ReorganizeMonitor(mid),
                                label: format!("Re-organize monitor #{mid}"),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: Some(0.0),
                                progress_info: None,
                            }));
                            let cfg = store.subdir_config();
                            let ids = match store.list_recording_ids_for_monitor(mid) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                                    });
                                    return;
                                }
                            };
                            let total = ids.len();
                            let reverse = !cfg.enabled;
                            for (i, rec_id) in ids.iter().enumerate() {
                                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                    id: task_id, progress: Some(i as f32 / total.max(1) as f32), info: format!("{}/{total}", i+1),
                                });
                                match reorganize_recording_files(*rec_id, &store, &cfg, reverse).await {
                                    Ok(Some(_)) => { let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id }); }
                                    Err(e) => warn!("reorganize monitor {mid} rec_id={rec_id}: {e:#}"),
                                    _ => {}
                                }
                            }
                            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                id: task_id,
                                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
                            });
                        });
                    }
                    ManualCommand::ReorganizeChannel(channel_id) => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            let task_id = crate::events::next_task_id();
                            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                                id: task_id,
                                kind: crate::events::BackgroundTaskKind::ReorganizeChannel(channel_id),
                                label: format!("Re-organize channel #{channel_id}"),
                                detail: String::new(),
                                started_at: now_unix(),
                                progress: Some(0.0),
                                progress_info: None,
                            }));
                            let cfg = store.subdir_config();
                            let ids = match store.list_recording_ids_for_channel(channel_id) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                        id: task_id,
                                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                                    });
                                    return;
                                }
                            };
                            let total = ids.len();
                            let reverse = !cfg.enabled;
                            for (i, rec_id) in ids.iter().enumerate() {
                                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                    id: task_id, progress: Some(i as f32 / total.max(1) as f32), info: format!("{}/{total}", i+1),
                                });
                                match reorganize_recording_files(*rec_id, &store, &cfg, reverse).await {
                                    Ok(Some(_)) => { let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id }); }
                                    Err(e) => warn!("reorganize channel {channel_id} rec_id={rec_id}: {e:#}"),
                                    _ => {}
                                }
                            }
                            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                                id: task_id,
                                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
                            });
                        });
                    }
                    ManualCommand::RenameRecording { rec_id, new_stem } => {
                        let store = self.store.clone();
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            match rename_recording_files(rec_id, &store, &new_stem).await {
                                Ok(Some(_)) => {
                                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    warn!("rename rec_id={rec_id}: {e:#}");
                                    let _ = tx.send(AppEvent::Error {
                                        context: format!("Rename recording #{rec_id}"),
                                        message: e.to_string(),
                                    });
                                }
                            }
                        });
                    }
                },
                else => break,
            }
        }
    }

    /// Fetch a monitor's channel assets (icon/banner/badges/emotes) as a background
    /// task. `force` skips the 24h freshness stamp (manual refetch). `broadcaster_id`
    /// is the platform id when detection supplied it; otherwise it's resolved from
    /// the channel URL, so an offline channel still fetches.
    fn fetch_channel_assets(
        &self,
        row: &MonitorWithChannel,
        broadcaster_id: Option<String>,
        force: bool,
    ) {
        let platform = row.monitor.platform();
        // Per-platform asset dir: one container can hold the same creator on
        // Twitch + YouTube + Kick, each with its own icon/banner/badges/emotes —
        // namespacing by platform keeps them from overwriting each other (and the
        // 24h freshness stamp becomes per-(channel, platform) for free).
        let asset_dir = crate::assets::channel_asset_dir(&row.channel.name, platform);
        if !force && !crate::assets::should_refetch_assets(&asset_dir) {
            return;
        }
        // Guard: skip if a fetch for this (channel, platform) is already in flight.
        let fetch_key = (row.channel.name.clone(), platform.as_str().to_string());
        {
            let mut running = self.running_asset_fetches.lock().unwrap();
            if running.contains(&fetch_key) {
                return;
            }
            running.insert(fetch_key.clone());
        }
        let http = self.ctx.http_client();
        let ctx = self.ctx.clone();
        let store = self.store.clone();
        let tx = self.events.clone();
        let url = row.monitor.url.clone();
        let known_bid = broadcaster_id.unwrap_or_default();
        let monitor_id = row.monitor.id;
        let running_asset_fetches = self.running_asset_fetches.clone();

        let task_id = crate::events::next_task_id();
        let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::AssetFetch,
            label: row.channel.name.clone(),
            detail: format!("{} · icon, banner, badges, emotes", platform.label()),
            started_at: now_unix(),
            progress: None,
            progress_info: None,
        }));

        tokio::spawn(async move {
            use crate::events::TaskOutcome;
            let outcome = match platform {
                Platform::Twitch => match ctx.twitch_helix_auth().await {
                    Ok((client_id, token)) => {
                        let bid = if !known_bid.is_empty() {
                            Some(known_bid)
                        } else if let Some(login) = crate::detectors::twitch_login(&url) {
                            ctx.twitch_user_id(&client_id, &token, &login).await
                        } else {
                            None
                        };
                        match bid {
                            Some(bid) => {
                                let platform_dir = crate::app_paths::platform_assets_dir();
                                if crate::assets::run_twitch_assets(
                                    &http, &client_id, &token, &bid, &asset_dir, &platform_dir,
                                )
                                .await
                                {
                                    TaskOutcome::Completed
                                } else {
                                    TaskOutcome::Failed("channel asset fetch failed".into())
                                }
                            }
                            None => TaskOutcome::Failed("could not resolve Twitch user id".into()),
                        }
                    }
                    Err(e) => TaskOutcome::Failed(format!("Twitch auth: {e}")),
                },
                Platform::YouTube => {
                    let api_key = store
                        .get_setting("youtube_api_key")
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    // Only resolve the UC channel ID when we have an API key to use it;
                    // the page-banner scrape only needs the channel URL.
                    let uc = if !known_bid.is_empty() {
                        Some(known_bid)
                    } else if !api_key.is_empty() {
                        crate::websub::resolve_channel_uc(&http, &url).await
                    } else {
                        None
                    };
                    let channel_id = uc.as_deref().unwrap_or("");
                    let browser = store
                        .get_setting("cookies_browser")
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    let browser_name = browser.split(':').next().unwrap_or("chrome");
                    let fp = crate::browser_ua::build_browser_fingerprint(
                        if browser_name.is_empty() { "chrome" } else { browser_name }
                    );
                    if crate::assets::run_youtube_assets(
                        &http, &api_key, channel_id, &url, &asset_dir, Some(&fp),
                    )
                    .await
                    {
                        TaskOutcome::Completed
                    } else {
                        TaskOutcome::Failed("YouTube channel asset fetch failed".into())
                    }
                }
                Platform::Kick => {
                    let slug = if !known_bid.is_empty() {
                        Some(known_bid)
                    } else {
                        crate::detectors::kick_slug(&url)
                    };
                    match slug {
                        Some(slug)
                            if crate::assets::run_kick_assets(&http, &slug, &asset_dir).await =>
                        {
                            TaskOutcome::Completed
                        }
                        Some(_) => TaskOutcome::Failed("channel asset fetch failed".into()),
                        None => TaskOutcome::Failed("could not resolve Kick slug".into()),
                    }
                }
                _ => TaskOutcome::Failed("no asset source for this platform".into()),
            };
            if let TaskOutcome::Failed(ref e) = outcome {
                tracing::warn!(monitor_id, "asset fetch failed: {e}");
            }
            running_asset_fetches.lock().unwrap().remove(&fetch_key);
            let _ = tx.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        });
    }

    /// Periodically refresh stale channel assets for enabled monitors that have
    /// asset fetching on, so channels that rarely (or never) record still keep a
    /// current icon/banner/badges/emotes. Cheap: a fresh channel is a no-op
    /// (`fetch_channel_assets` returns early when not stale), so only channels past
    /// the 24h window actually fetch.
    pub async fn asset_refresh_loop(
        &self,
        shutdown: Arc<AtomicBool>,
        jobs: crate::events::JobRegistry,
    ) {
        const INITIAL_DELAY_SECS: u64 = 45;
        const TICK_SECS: u64 = 3600; // re-scan hourly; per-channel staleness is 24h

        crate::app_core::sleep_cancellable(Duration::from_secs(INITIAL_DELAY_SECS), &shutdown).await;
        loop {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            if self.store.job_enabled("job_asset_refresh") {
                self.refresh_stale_assets_once();
                crate::events::mark_job(&jobs, "Channel asset refresh", TICK_SECS as i64);
            }
            crate::app_core::sleep_cancellable(Duration::from_secs(TICK_SECS), &shutdown).await;
        }
    }

    /// One asset-refresh pass: trigger a (staleness-gated) fetch for each eligible
    /// channel, de-duplicated across instances that share an asset dir.
    fn refresh_stale_assets_once(&self) {
        let rows = match self.store.list_monitors_with_channels() {
            Ok(r) => r,
            Err(e) => {
                warn!("asset refresh: failed to load monitors: {e:#}");
                return;
            }
        };
        // YouTube asset fetch needs the Data API; skip it without a key rather than
        // failing every pass (the manual Refetch button still surfaces the reason).
        let yt_key_set = !self
            .store
            .get_setting("youtube_api_key")
            .ok()
            .flatten()
            .unwrap_or_default()
            .is_empty();
        let recording: std::collections::HashSet<i64> =
            self.active.lock().unwrap().keys().copied().collect();
        let mut seen: std::collections::HashSet<(String, Platform)> =
            std::collections::HashSet::new();
        for row in &rows {
            if !row.monitor.enabled || !row.monitor.fetch_chat_assets {
                continue;
            }
            // A recording channel's record() path already handles its assets.
            if recording.contains(&row.monitor.id) {
                continue;
            }
            if row.monitor.platform() == Platform::YouTube && !yt_key_set {
                continue;
            }
            // Instances of one (channel, platform) share an asset dir — fetch it
            // once per pass. Keyed by platform too so a container that spans
            // Twitch + YouTube refreshes BOTH (not just the first one seen).
            if !seen.insert((sanitize_filename(&row.channel.name), row.monitor.platform())) {
                continue;
            }
            // force=false: a no-op when the channel's assets are still fresh.
            self.fetch_channel_assets(row, None, false);
        }
    }

    /// Reserve the monitor and spawn its recording task. Returns false if it was
    /// skipped (already active, or in backoff when not bypassing).
    fn try_begin(
        &self,
        monitor_id: i64,
        went_live_at: Option<i64>,
        approximate: bool,
        stream_id: Option<String>,
        thumbnail_url: Option<String>,
        broadcaster_id: Option<String>,
        stream_title: Option<String>,
        bypass_backoff: bool,
    ) -> bool {
        {
            let mut active = self.active.lock().unwrap();
            if active.contains_key(&monitor_id) {
                return false;
            }
            if !bypass_backoff && self.in_backoff(monitor_id) {
                return false;
            }
            active.insert(monitor_id, 0); // reserve; real PID set after spawn
        }
        if bypass_backoff {
            self.backoff.lock().unwrap().remove(&monitor_id);
        }

        let row = match self.store.get_monitor_with_channel(monitor_id) {
            Ok(Some(r)) => r,
            _ => {
                self.active.lock().unwrap().remove(&monitor_id);
                return false;
            }
        };
        if !row.channel.enabled || !row.monitor.enabled {
            // A push notification arrived for a disabled channel/monitor. Don't
            // record, but update last_state so the UI can show it as "live".
            self.active.lock().unwrap().remove(&monitor_id);
            let _ = self.store.set_monitor_check_result(monitor_id, "live", now_unix());
            return false;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.record(row, went_live_at, approximate, stream_id, thumbnail_url, broadcaster_id, stream_title).await;
        });
        true
    }

    /// Manual "Start": check the channel now and record if live.
    async fn manual_start(&self, monitor_id: i64, notify_offline: bool) {
        if self.active.lock().unwrap().contains_key(&monitor_id) {
            return; // already recording
        }
        let row = match self.store.get_monitor_with_channel(monitor_id) {
            Ok(Some(r)) => r,
            _ => return,
        };
        let enabled = row.channel.enabled && row.monitor.enabled;
        let name = row.channel.name.clone();
        let outcome = self.check_one(&row).await;
        if outcome.live {
            if enabled {
                let (went, approx) = match outcome.went_live_at {
                    Some(t) => (Some(t), false),
                    None => (Some(now_unix()), true),
                };
                self.try_begin(monitor_id, went, approx, outcome.stream_id, outcome.thumbnail_url, outcome.broadcaster_id, outcome.stream_title, true);
            } else {
                // Disabled: just update the state so the UI can show "live" for
                // channels we're monitoring passively via push notifications.
                let _ = self.store.set_monitor_check_result(monitor_id, "live", now_unix());
            }
        } else if enabled && notify_offline {
            let message = if outcome.error && !outcome.detail.is_empty() {
                format!("{name}: {}", outcome.detail)
            } else {
                format!("{name} is not live")
            };
            let _ = self.events.send(AppEvent::Error {
                context: "Start".into(),
                message,
            });
        } else {
            // Disabled and offline: update state silently.
            let _ = self.store.set_monitor_check_result(monitor_id, "offline", now_unix());
        }
    }

    /// Manual "Stop": abort the active recording and apply a short cooldown so it
    /// doesn't immediately restart on the next poll.
    fn manual_stop(&self, monitor_id: i64) {
        let pid = self.active.lock().unwrap().get(&monitor_id).copied();
        // Kill the DASH companion (dual capture) too, if one is running.
        let companion_pid = self.active_secondary.lock().unwrap().get(&monitor_id).copied();
        if let Some(p) = companion_pid {
            self.stopping_monitors.lock().unwrap().insert(monitor_id);
            if p > 0 {
                crate::platform::kill_process_tree(p);
            }
        }
        if let Some(pid) = pid {
            self.stopping_monitors.lock().unwrap().insert(monitor_id);
            if pid > 0 {
                crate::platform::kill_process_tree(pid);
            }
            self.backoff.lock().unwrap().insert(
                monitor_id,
                BackoffEntry {
                    fails: 0,
                    until: Instant::now() + Duration::from_secs(120),
                },
            );
            info!(monitor_id, "manual stop");
        }
    }

    /// Stop the live-chat sidecar download for a monitor, if one is running.
    fn stop_chat_download(&self, monitor_id: i64) {
        let pid = self.active_chats.lock().unwrap().get(&monitor_id).copied();
        let Some(pid) = pid else { return };
        self.stopping_chats.lock().unwrap().insert(monitor_id);
        if pid > 0 {
            crate::platform::kill_process_tree(pid);
        }
        info!(monitor_id, "stop chat download");
    }

    /// Stop the YouTube chat sidecar for `monitor_id` (if running) and wait
    /// up to `timeout` for it to release its `live_chat.json` file handle.
    /// Called before `rename_companion_sidecars` so the rename isn't blocked
    /// by an actively-writing chat process (Windows os error 32).
    async fn stop_and_wait_for_chat(&self, monitor_id: i64, timeout: Duration) {
        if !self.active_chats.lock().unwrap().contains_key(&monitor_id) {
            return;
        }
        self.stop_chat_download(monitor_id);
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if !self.active_chats.lock().unwrap().contains_key(&monitor_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// Run a live-chat sidecar yt-dlp process for `monitor_id`. Spawns yt-dlp
    /// with `--skip-download --sub-langs=live_chat --write-subs` so it captures
    /// only chat alongside the video recording. Registers its PID in
    /// `active_chats` (visible to the UI), and removes it when the process exits
    /// (either stream ended naturally, or the user called `stop_chat_download`).
    async fn run_chat_download(&self, monitor_id: i64, plan: DownloadPlan) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // Detached like every other download: a named job without kill-on-close,
        // no kill_on_drop, and output to a log file so the sidecar survives an app
        // restart and a relaunch can re-attach. yt-dlp writes the `.live_chat.json`
        // directly; this log only captures its diagnostics.
        let log_path = plan.capture_path.with_extension("chat.log");
        let (out_h, err_h) = match open_log_pair(&log_path) {
            Ok(p) => p,
            Err(e) => {
                warn!(monitor_id, "chat log open failed: {e}");
                return;
            }
        };
        let job_name = format!("Local\\StreamArchiver_chat_{monitor_id}");
        let job = DetachedJob::create(&job_name).ok();

        let mut cmd = Command::new(&plan.program);
        cmd.args(&plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_h))
            .stderr(Stdio::from(err_h));
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                warn!(monitor_id, "chat download spawn failed: {e}");
                return;
            }
        };
        if let Some(j) = &job {
            if let Err(e) = j.assign_child(&child) {
                warn!(monitor_id, "chat job assign failed: {e:#}");
            }
        }
        let pid = child.id().unwrap_or(0);
        if pid != 0 {
            self.active_chats.lock().unwrap().insert(monitor_id, pid);
            let row = DetachedRow {
                kind: DetachedKind::Chat,
                ref_id: monitor_id,
                monitor_id: Some(monitor_id),
                pid,
                proc_start: crate::platform::process_start_time(pid).unwrap_or(0),
                job_name: job_name.clone(),
                log_path: log_path.to_string_lossy().into_owned(),
                capture_path: plan.capture_path.to_string_lossy().into_owned(),
                final_path: plan.final_path.to_string_lossy().into_owned(),
                remux_to_mkv: false,
                take_group: None,
                spawn_build: crate::version::build_id().to_string(),
                started_at: now_unix(),
                secondary: false,
                stream_id: None,
                went_live_at: None,
            };
            if let Err(e) = self.store.register_detached(&row) {
                warn!(monitor_id, "register chat detached failed: {e:#}");
            }
        }
        info!(monitor_id, "chat download started");
        // Fire any event so the UI repaints and shows the chat-active indicator.
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "chat_active".into(),
        });

        let _ = child.wait().await;
        if let Some(j) = &job {
            j.kill(); // clean up any straggler before we drop the handle
        }
        drop(job);
        let _ = self.store.clear_detached(DetachedKind::Chat, monitor_id);
        let stopped = self.stopping_chats.lock().unwrap().remove(&monitor_id);
        self.active_chats.lock().unwrap().remove(&monitor_id);
        // Surface any yt-dlp diagnostics (auth failure, format unavailable, …).
        let tail = read_log_tail(&log_path, 12).await;
        if !tail.trim().is_empty() {
            warn!(monitor_id, "chat yt-dlp log tail:\n{tail}");
        }
        if stopped {
            info!(monitor_id, "chat download stopped by user");
        } else {
            info!(monitor_id, "chat download ended");
        }
        // Repaint so the indicator disappears.
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "idle".into(),
        });
    }

    /// Begin an on-demand video download: reserve it and spawn its task.
    async fn start_video(&self, video_id: i64) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut active = self.active_videos.lock().unwrap();
            if active.contains_key(&video_id) {
                return; // already downloading/queued
            }
            active.insert(video_id, 0); // reserve; real PID set after spawn
        }
        let video = match self.store.get_video(video_id) {
            Ok(Some(v)) => v,
            _ => {
                self.active_videos.lock().unwrap().remove(&video_id);
                return;
            }
        };
        let this = self.clone();
        tokio::spawn(async move { this.download_video(video).await });
    }

    /// Abort an in-flight (or queued) on-demand video download.
    ///
    /// The stop "tombstone" is recorded only while the download is actually
    /// active, and under the `active_videos` lock — so it can never linger after
    /// the task has finalized (which would otherwise silently cancel a later
    /// retry of the same id) and can never race the finalize into the wrong
    /// status. `download_video` consumes the tombstone under the same lock.
    fn stop_video(&self, video_id: i64) {
        let pid = {
            let active = self.active_videos.lock().unwrap();
            let Some(pid) = active.get(&video_id).copied() else {
                return; // not active: nothing to stop, don't leave a tombstone
            };
            self.stopping_videos.lock().unwrap().insert(video_id);
            pid
        };
        if pid > 0 {
            crate::platform::kill_process_tree(pid);
            // Already downloading: reflect the stop immediately (download_video
            // will re-confirm with the final byte count).
            let _ = self.store.set_video_status(video_id, "stopped");
        }
        info!(video_id, pid, "stop video download");
    }

    /// Atomically decide a video's final status and drop its `active_videos`
    /// membership: a stop tombstone (set under the same lock by `stop_video`)
    /// wins over the byte-count classification. Returns the chosen status.
    fn finalize_video(&self, id: i64, bytes: i64, shutting_down: bool) -> &'static str {
        let mut active = self.active_videos.lock().unwrap();
        let stopped = self.stopping_videos.lock().unwrap().remove(&id);
        active.remove(&id);
        self.video_progress.lock().unwrap().remove(&id);
        self.video_speed.lock().unwrap().remove(&id);
        if stopped {
            "stopped"
        } else if shutting_down {
            // We're quitting and killed the tree; treat any in-flight download as
            // incomplete regardless of how many bytes landed.
            "orphaned"
        } else if bytes > 0 {
            "completed"
        } else {
            "failed"
        }
    }

    async fn download_video(&self, video: Video) {
        let id = video.id;
        let _permit = self.sem.acquire().await.expect("semaphore");

        // Cancelled (or shutting down) before we got a slot: finalize and bail.
        if self.stopping_videos.lock().unwrap().contains(&id)
            || self.shutdown.load(Ordering::SeqCst)
        {
            let status = self.finalize_video(id, 0, self.shutdown.load(Ordering::SeqCst));
            let _ = self
                .store
                .finish_video(id, now_unix(), 0, None, status, "", "");
            return;
        }

        let started_at = now_unix();
        let _ = self.store.set_video_started(id, started_at);

        let global_method = self
            .store
            .get_setting("download_auth_method")
            .ok()
            .flatten()
            .unwrap_or_default();
        let global_browser = self
            .store
            .get_setting("cookies_browser")
            .ok()
            .flatten()
            .unwrap_or_default();
        let auth = resolve_auth_for(
            video.auth_kind,
            &video.auth_value,
            &global_method,
            &global_browser,
        );
        // Optionally resolve the real title + channel + id (for
        // {title}/{channel}/{video_id}/{name} and the list display).
        let (title, channel, mut video_id) = if video.auto_title {
            resolve_meta(&video, &auth).await
        } else {
            (String::new(), String::new(), String::new())
        };
        // Fall back to URL-extracted video ID so {video_id} is always filled when
        // the URL contains an explicit ID (YouTube watch?v=, youtu.be/, /live/ID).
        if video_id.is_empty() {
            video_id = extract_yt_video_id(&video.url).unwrap_or_default();
        }
        if !title.is_empty() && video.title.trim().is_empty() {
            let _ = self.store.set_video_title(id, &title);
        }
        if !channel.is_empty() {
            let _ = self.store.set_video_channel(id, &channel);
        }
        // Filename media-info ({resolution}/{fps}/…): pre-probe before download if
        // configured; the finished file is probed/renamed below for post modes.
        let media_mode = media_info_mode(&self.store);
        let want_media = template_wants_media(&video.filename_template);
        let pre_media = if want_media && media_mode.pre() {
            preprobe_media(video.tool, &video.url, &video.quality, &auth).await
        } else {
            None
        };
        let ytdlp_global_raw = self
            .store
            .get_setting("ytdlp_default_args")
            .ok()
            .flatten()
            .unwrap_or_default();
        let ytdlp_global_args = split_args(&ytdlp_global_raw);
        let ytdlp_bins = load_ytdlp_bins(&self.store);
        let plan = build_video_plan(
            &video, started_at, &title, &channel, &video_id, &auth, &ytdlp_global_args,
            pre_media.as_ref(), &ytdlp_bins,
        );
        if let Some(parent) = plan.capture_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
            crate::platform::set_hidden(parent); // mark the .cache\ working dir hidden
        }
        if let Some(out_dir) = plan.final_path.parent() {
            let _ = tokio::fs::create_dir_all(out_dir).await;
        }
        let label = if !video.title.trim().is_empty() {
            video.title.clone()
        } else if !title.is_empty() {
            title.clone()
        } else {
            video.url.clone()
        };
        info!(video = id, program = %plan.program, "downloading video -> {}", plan.final_path.display());

        let outcome = self
            .run_process(
                &self.active_videos,
                id,
                &plan,
                Some(self.video_progress.clone()),
                Some(self.video_speed.clone()),
                None, // on-demand downloads don't track ad breaks
                DetachReg {
                    kind: DetachedKind::Video,
                    ref_id: id,
                    monitor_id: None,
                    take_group: None,
                    started_at,
                    secondary: false,
                    stream_id: None,
                    went_live_at: None,
                },
            )
            .await;

        // Promote from .cache\ to the output dir. streamlink/ffmpeg remux .ts→.mkv;
        // yt-dlp already produced the (M)KV in .cache — but its extension may differ
        // from the predicted .mkv, so fall back to the newest {stem}.* in .cache\.
        let cache = plan.capture_path.parent().map(Path::to_path_buf);
        let capstem = plan
            .final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut final_path;
        if plan.remux_to_mkv {
            final_path = promote_capture(&plan).await;
        } else {
            let produced = if file_len(&plan.capture_path).await > 0 {
                Some(plan.capture_path.clone())
            } else {
                newest_with_stem(&plan.capture_path).await
            };
            match produced {
                Some(src) => {
                    let dest = plan.final_path.with_file_name(
                        src.file_name().map(|n| n.to_os_string()).unwrap_or_default(),
                    );
                    if let Some(p) = dest.parent() {
                        let _ = tokio::fs::create_dir_all(p).await;
                    }
                    // The download landing on disk matters more than a fully-
                    // descriptive name — see rename_or_shorten.
                    let dest_dir = dest.parent().unwrap_or_else(|| Path::new("."));
                    let dest_stem = dest
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dest_ext = dest
                        .extension()
                        .map(|e| e.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match rename_or_shorten(&src, dest_dir, &dest_stem, &dest_ext).await {
                        Ok(actual) => final_path = actual,
                        Err(e) => {
                            warn!(video = id, "promote move failed, keeping in cache: {e:#}");
                            final_path = src;
                        }
                    }
                }
                None => final_path = plan.capture_path.clone(),
            }
        }
        // Promoted iff the file now lives in the output dir (not still in .cache\).
        let promoted = final_path.parent() == plan.final_path.parent();
        if promoted {
            if let (Some(cache), Some(out_dir)) = (cache.as_deref(), final_path.parent()) {
                move_companions(cache, out_dir, &capstem).await;
            }
            // Post-capture: probe the finished file for actual media info and rename.
            if want_media && media_mode.post() {
                if let Some(mi) = probe_media(&final_path.to_string_lossy()).await {
                    let quality = resolved_quality(&video.quality);
                    let stem = video_stem(
                        &video, started_at, &title, &channel, &video_id, &quality, Some(&mi),
                        video.tool.label(), Platform::detect(&video.url).as_str(),
                    );
                    final_path = rename_for_media(final_path, &stem).await;
                }
            }
            if let Some(cache) = cache.as_deref() {
                purge_cache(cache, &capstem).await;
            }
        }

        let bytes = file_len(&final_path).await as i64;
        // Decide status + drop the active_videos entry atomically so a concurrent
        // stop can't be lost (and its tombstone can't outlive this task).
        let status = self.finalize_video(id, bytes, self.shutdown.load(Ordering::SeqCst));
        let _ = self.store.finish_video(
            id,
            now_unix(),
            bytes,
            outcome.exit_code,
            status,
            &final_path.to_string_lossy(),
            &outcome.log,
        );
        if status == "failed" {
            let _ = self.events.send(AppEvent::Error {
                context: "Video".into(),
                message: format!("{label}: download failed"),
            });
        }
        // If this download was a post-stream VOD archive, file it on the recording
        // (alongside) and optionally replace the live capture. No-op otherwise.
        self.finalize_vod_archive(id, &final_path, status).await;
        info!(video = id, bytes, status, "video download finished");
    }

    /// One-shot liveness check for a monitor, dispatched by detection method.
    async fn check_one(&self, row: &MonitorWithChannel) -> DetectOutcome {
        let item = DetectItem {
            monitor_id: row.monitor.id,
            url: row.monitor.url.clone(),
            platform: row.monitor.platform(),
        };
        match row.monitor.detection_method {
            // EventSub is push-only; check liveness now via Helix.
            DetectionMethod::TwitchApi
            | DetectionMethod::EventSub
            | DetectionMethod::EventSubHelix => self
                .ctx
                .detect_twitch(std::slice::from_ref(&item))
                .await
                .into_iter()
                .next()
                .unwrap_or_else(|| DetectOutcome {
                    monitor_id: item.monitor_id,
                    live: false,
                    detail: "no result".into(),
                    error: true,
                    went_live_at: None,
                    stream_id: None,
                    thumbnail_url: None,
                    broadcaster_id: None,
                    stream_title: None,
                }),
            DetectionMethod::GenericProbe => self.ctx.detect_generic(&item).await,
            DetectionMethod::YouTubeApi => self.ctx.detect_youtube_api(&item).await,
            DetectionMethod::KickApi => self.ctx.detect_kick_api(&item).await,
            _ => self.ctx.detect_scrape(&item).await,
        }
    }

    fn in_backoff(&self, monitor_id: i64) -> bool {
        self.backoff
            .lock()
            .unwrap()
            .get(&monitor_id)
            .map(|b| Instant::now() < b.until)
            .unwrap_or(false)
    }

    fn note_result(&self, monitor_id: i64, duration_secs: i64, ok: bool) {
        let mut map = self.backoff.lock().unwrap();
        // Back off on any capture that produced no footage (bytes == 0), even one
        // that ran a while before dying: a long run that wrote nothing is still a
        // failure (e.g. a SABR from-start stall that downloads ~hundreds of MiB to
        // its cache, then crashes without finalizing the MKV). Without this such a
        // capture would re-spawn on the very next poll and tight-loop, re-fetching
        // the same opening segments forever.
        if ok {
            map.remove(&monitor_id);
        } else {
            let entry = map.entry(monitor_id).or_insert(BackoffEntry {
                fails: 0,
                until: Instant::now(),
            });
            entry.fails = entry.fails.saturating_add(1);
            let mut wait = (30u64 * entry.fails as u64).min(600);
            // A capture that died almost immediately having produced nothing (a
            // few seconds — e.g. "No video formats found" during a transient
            // no-format window / pre-roll ad, or an unrecordable configuration)
            // shouldn't re-spawn every ~30s and tight-loop for the whole stream.
            // Apply a higher floor for these instant failures.
            const INSTANT_FAIL_SECS: i64 = 10;
            if duration_secs < INSTANT_FAIL_SECS {
                wait = wait.max(300);
            }
            entry.until = Instant::now() + Duration::from_secs(wait);
            warn!(
                monitor_id,
                fails = entry.fails,
                wait,
                duration_secs,
                "recording captured nothing; backing off"
            );
        }
    }

    async fn record(
        &self,
        row: MonitorWithChannel,
        went_live_at: Option<i64>,
        approximate: bool,
        stream_id: Option<String>,
        thumbnail_url: Option<String>,
        broadcaster_id: Option<String>,
        stream_title: Option<String>,
    ) {
        let monitor_id = row.monitor.id;
        // Per-stream key for the SABR stall maps. Fully per-stream when a video ID
        // is available (YouTube scrape / API); degrades to per-monitor when not.
        let sabr_key = (monitor_id, stream_id.clone());
        // SABR from-start fallback: prior attempts stalled "not near live head"
        // (DVR window expired, or persistent from-start stalls under deep-rewind).
        // Override capture_from_start so we capture the live edge this time instead
        // of stalling from the beginning again. Cleared when a capture succeeds
        // (bytes > 0).
        let mut row = row;
        if row.monitor.capture_from_start
            && self.sabr_dvr_exceeded.lock().unwrap().contains(&sabr_key)
        {
            row.monitor.capture_from_start = false;
            info!(monitor_id, "SABR from-start unavailable; capturing live edge");
        }
        let global_method = self
            .store
            .get_setting("download_auth_method")
            .ok()
            .flatten()
            .unwrap_or_default();
        let global_browser = self
            .store
            .get_setting("cookies_browser")
            .ok()
            .flatten()
            .unwrap_or_default();
        let auth = resolve_auth(&row, &global_method, &global_browser);
        // Filename media-info ({resolution}/{fps}/…): pre-probe the stream if the
        // template uses it and the mode asks for it. Do this BEFORE taking the
        // concurrency permit (so a slow probe can't block other recordings) and
        // BEFORE the start timestamp (so it reflects when capture actually begins).
        // The finished file is probed again (and renamed) below for post modes.
        let media_mode = media_info_mode(&self.store);
        let want_media = template_wants_media(&row.monitor.filename_template);
        let pre_media = if want_media && media_mode.pre() {
            preprobe_media(row.monitor.tool, &row.monitor.url, &row.monitor.quality, &auth).await
        } else {
            None
        };

        let _permit = self.sem.acquire().await.expect("semaphore");
        // The probe + permit wait may have spanned a shutdown; don't start new work.
        if self.shutdown.load(Ordering::SeqCst) {
            self.active.lock().unwrap().remove(&monitor_id);
            return;
        }
        let ytdlp_global_raw = self
            .store
            .get_setting("ytdlp_default_args")
            .ok()
            .flatten()
            .unwrap_or_default();
        let ytdlp_global_args = split_args(&ytdlp_global_raw);
        let ytdlp_bins = load_ytdlp_bins(&self.store);
        let started_at = now_unix();
        let plan = build_plan(&row, started_at, &auth, &ytdlp_global_args, stream_id.as_deref(), stream_title.as_deref().unwrap_or(""), pre_media.as_ref(), went_live_at.unwrap_or(0), &ytdlp_bins);
        if let Some(parent) = plan.capture_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
            crate::platform::set_hidden(parent); // mark the .cache\ working dir hidden
        }
        // Also ensure the output dir exists (the final file is promoted there).
        if let Some(out_dir) = plan.final_path.parent() {
            let _ = tokio::fs::create_dir_all(out_dir).await;
        }

        // A take key links the recordings of this capture attempt: the primary
        // and, in dual capture, the DASH companion share it (they're one "take").
        let take_group = format!("{monitor_id}:{started_at}");
        let rec_id = self
            .store
            .insert_recording(
                monitor_id,
                started_at,
                &plan.final_path.to_string_lossy(),
                went_live_at,
                approximate,
                stream_id.as_deref(),
                Some(&take_group),
            )
            .unwrap_or(0);
        let _ = self
            .store
            .set_monitor_check_result(monitor_id, "recording", started_at);
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "recording".into(),
        });
        // Compute the expected thumbnail path before the fire-and-forget fetch below
        // so the notification handler can find it (file may not exist yet).
        let toast_thumbnail = (row.monitor.fetch_thumbnail && !plan.writes_own_thumbnail)
            .then(|| plan.capture_path.with_extension("thumbnail.jpg"));
        let _ = self.events.send(AppEvent::RecordingStarted {
            monitor_id,
            recording_id: rec_id,
            channel: row.channel.name.clone(),
            thumbnail_path: toast_thumbnail,
        });

        // Dual capture: also run a DASH companion via the system yt-dlp for formats
        // that only DASH carries. It captures from the live edge (SABR owns
        // from-start), writes a sibling `{stem}.dash.mkv`, and finalizes as its own
        // recording sharing this take. Only meaningful when SABR drives the primary.
        if row.monitor.dual_capture
            && row.monitor.platform() == Platform::YouTube
            && row.monitor.capture_from_start
            && ytdlp_bins.sabr.usable()
        {
            let dash_plan = build_dash_companion_plan(
                &plan.final_path,
                &row,
                &auth,
                &ytdlp_global_args,
                &ytdlp_bins.system_program(),
                &load_dash_format(&self.store),
                &ytdlp_bins.sabr.pot_args,
            );
            let this = self.clone();
            let tg = take_group.clone();
            let sid = stream_id.clone();
            let cname = row.channel.name.clone();
            tokio::spawn(async move {
                this.run_dash_companion(
                    monitor_id, dash_plan, tg, sid, went_live_at, approximate, cname,
                )
                .await;
            });
        }
        // Asset fetching — fire-and-forget tasks that don't block the recording.
        // Normal yt-dlp writes its own thumbnail inline (`--write-thumbnail`); for
        // streamlink and SABR captures (which don't) we fetch it over HTTP instead.
        if row.monitor.fetch_thumbnail && !plan.writes_own_thumbnail {
            if let Some(ref url) = thumbnail_url {
                let http = self.ctx.http_client();
                let url = url.clone();
                // Into the .cache\ working dir; promoted up with the recording on
                // success (and dropped with it if the capture fails).
                let dest = plan.capture_path.with_extension("thumbnail.jpg");
                let task_id = crate::events::next_task_id();
                let task_label = row.channel.name.clone();
                let _ = self.events.send(AppEvent::BackgroundTaskStarted(
                    crate::events::BackgroundTask {
                        id: task_id,
                        kind: crate::events::BackgroundTaskKind::ThumbnailFetch,
                        label: task_label,
                        detail: "stream thumbnail".into(),
                        started_at: crate::models::now_unix(),
                        progress: None,
                progress_info: None,
                    },
                ));
                let tx = self.events.clone();
                tokio::spawn(async move {
                    let outcome = match crate::assets::fetch_stream_thumbnail(&http, &url, &dest).await {
                        Ok(_) => crate::events::TaskOutcome::Completed,
                        Err(e) => {
                            tracing::warn!(monitor_id, "thumbnail fetch failed: {e}");
                            crate::events::TaskOutcome::Failed(e.to_string())
                        }
                    };
                    let _ = tx.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
                });
            }
        }
        if row.monitor.fetch_chat_assets {
            self.fetch_channel_assets(&row, broadcaster_id.clone(), false);
        }

        info!(monitor_id, program = %plan.program, "starting recording -> {}", plan.capture_path.display());
        {
            let redacted: Vec<String> = plan.args.iter().map(|a| {
                if a.contains("Authorization=OAuth ") {
                    let prefix = &a[..a.find("OAuth ").map(|i| i + 6).unwrap_or(a.len())];
                    format!("{prefix}<redacted>")
                } else {
                    a.clone()
                }
            }).collect();
            info!(monitor_id, "args: {}", redacted.join(" "));
        }

        // When capturing from the start of the broadcast (live-from-start /
        // hls-live-restart), the early footage isn't lost — it's pulled from the
        // DVR. Watch the growing capture and zero out "lost time" once it catches
        // up to the live edge; finalize then recomputes the exact residual (in
        // case the stream ends before catch-up completes).
        let from_start = row.monitor.capture_from_start
            && matches!(row.monitor.tool, Tool::Streamlink | Tool::YtDlp);
        let resolve_lost = from_start && went_live_at.is_some();
        let watcher_done = Arc::new(AtomicBool::new(false));
        let watcher = resolve_lost.then(|| {
            tokio::spawn(catch_up_watcher(
                self.store.clone(),
                self.events.clone(),
                monitor_id,
                rec_id,
                plan.capture_path.clone(),
                went_live_at.unwrap_or(0),
                watcher_done.clone(),
            ))
        });

        // Twitch+streamlink filters ads into hard cuts and logs each break; record
        // them so the UI can show ad count/time and the cut timestamps. Skip when
        // the recording row failed to insert (rec_id 0) — an ad break with a 0
        // recording_id would violate the FK and be dropped anyway.
        let ad_sink = (rec_id != 0
            && row.monitor.tool == Tool::Streamlink
            && row.monitor.platform() == Platform::Twitch)
            .then(|| AdSink {
                store: self.store.clone(),
                events: self.events.clone(),
                monitor_id,
                recording_id: rec_id,
                started_at,
                went_live_at,
                from_start,
                capture_path: plan.capture_path.clone(),
                ad_active: self.ad_active.clone(),
            });

        // Log title / game-category changes during the take (the scheduler pauses
        // normal polling while recording, so poll the source directly). Supported
        // for Twitch (Helix), Kick (v2 JSON), and YouTube (/live scrape); no-ops
        // gracefully when the source is unavailable. Generic URLs have no source.
        let meta_platform = row.monitor.platform();
        let meta_done = Arc::new(AtomicBool::new(false));
        let meta_task = (rec_id != 0 && meta_platform != Platform::Generic).then(|| {
            tokio::spawn(meta_watcher(
                self.ctx.clone(),
                self.store.clone(),
                self.events.clone(),
                monitor_id,
                rec_id,
                started_at,
                row.monitor.url.clone(),
                meta_platform,
                meta_done.clone(),
                self.shutdown.clone(),
            ))
        });

        // Twitch chat -> a native anonymous IRC-over-WebSocket logger, written as
        // a `.chat.jsonl` sidecar in the OUTPUT dir (next to the final file, not in
        // .cache\) so it isn't promoted/purged from under a still-writing logger; it
        // follows the file's stem on the post-rename. Twitch only.
        let chat_done = Arc::new(AtomicBool::new(false));
        let chat_task = (row.monitor.chat_log && row.monitor.platform() == Platform::Twitch)
            .then(|| {
                let chat_path = plan.final_path.with_extension("chat.jsonl");
                tokio::spawn(crate::chat::log_twitch_chat(
                    row.monitor.url.clone(),
                    chat_path,
                    chat_done.clone(),
                    self.shutdown.clone(),
                ))
            });

        // YouTube chat -> separate yt-dlp sidecar process with --skip-download
        // --sub-langs=live_chat. Runs concurrently with (and outlives) the video
        // recording so the video download is never blocked by the chat stream.
        // Visible in the UI as a "Chat ●" indicator; user can stop it independently.
        if row.monitor.chat_log
            && row.monitor.tool == Tool::YtDlp
            && row.monitor.platform() != Platform::Twitch
        {
            // Base the YouTube chat sidecar on the final (output-dir) path, not the
            // .cache\ capture: this process outlives the video, so its
            // `.live_chat.json` must not be promoted/purged mid-write.
            let chat_plan = build_chat_plan(&row, &plan.final_path, &auth, &ytdlp_global_args, &ytdlp_bins.system_program());
            let this = self.clone();
            let mid = monitor_id;
            tokio::spawn(async move { this.run_chat_download(mid, chat_plan).await });
        }

        // If a manual stop arrived while we were setting up (pid was 0 so kill
        // couldn't fire yet), honour it now: skip spawning the process entirely.
        let outcome = if self.stopping_monitors.lock().unwrap().contains(&monitor_id) {
            ProcessOutcome { exit_code: None, log: String::new() }
        } else {
            self.run_process(
                &self.active,
                monitor_id,
                &plan,
                None,
                None,
                ad_sink,
                DetachReg {
                    kind: DetachedKind::Recording,
                    ref_id: rec_id,
                    monitor_id: Some(monitor_id),
                    take_group: Some(take_group.clone()),
                    started_at,
                    secondary: false,
                    stream_id: stream_id.clone(),
                    went_live_at,
                },
            )
            .await
        };

        // Stop the catch-up watcher before we touch the capture file (so it can't
        // race finalize's authoritative lost-time write). Abort rather than wait:
        // the watcher only checks its done flag at the start of each sleep tick, so
        // a mid-ffprobe call would otherwise block here for several seconds.
        watcher_done.store(true, Ordering::SeqCst);
        if let Some(w) = watcher {
            w.abort();
            let _ = w.await;
        }
        // Same for the metadata watcher: it only checks `done` between API poll
        // cycles, so if it's mid-request (youtube_stream_meta scrapes a full page,
        // twitch_stream_meta hits Helix) we'd stall here for up to 30 s — keeping
        // the monitor in `active` and the UI stuck on "Stop recording" even though
        // the process has already exited. Abort cancels the in-flight request
        // immediately; no finalized insert can race because the task is gone.
        meta_done.store(true, Ordering::SeqCst);
        if let Some(t) = meta_task {
            t.abort();
            let _ = t.await;
        }
        // Stop the chat logger and let it flush/close its sidecar before we touch
        // the capture file (the post-rename moves the .chat.jsonl alongside it).
        chat_done.store(true, Ordering::SeqCst);
        if let Some(t) = chat_task {
            let _ = t.await;
        }
        // Broadcast end ~= when the tool exited; snapshot it before remux so the
        // span (and thus lost-time) isn't inflated by remux duration.
        let ended = now_unix();

        // Promote the finished capture from the hidden `.cache\` up to the output
        // dir (remux .ts→.mkv, or move an already-final container); a failed/0-byte
        // capture is left in `.cache\` for the startup sweep.
        let mut final_path = promote_capture(&plan).await;
        let promoted = final_path != plan.capture_path;
        let cache = plan.capture_path.parent().map(Path::to_path_buf);
        // The capture stem (== final stem before any post-rename) used to match this
        // recording's files within `.cache\`.
        let capstem = plan
            .final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if promoted {
            // Promote subtitle/thumbnail companions up next to the video (chat
            // sidecars are already written in the output dir).
            if let (Some(cache), Some(out_dir)) = (cache.as_deref(), final_path.parent()) {
                move_companions(cache, out_dir, &capstem).await;
            }
            // Post-capture: fill in the filename bits that are only known now and
            // rename. `{resolution}/{fps}/…` come from probing the finished file;
            // `{games}` and `{title}` are only fully known after the stream ends, and
            // also trigger a rename even when probing is off.
            let want_games = template_wants_games(&row.monitor.filename_template);
            let want_title = template_wants_title(&row.monitor.filename_template);
            let want_went_live = template_wants_went_live(&row.monitor.filename_template);
            let do_post_media = want_media && media_mode.post();
            if do_post_media || want_games || want_title || want_went_live {
                let mi = if do_post_media {
                    probe_media(&final_path.to_string_lossy()).await
                } else {
                    None
                };
                let games = if want_games {
                    games_for_recording(&self.store, rec_id)
                } else {
                    String::new()
                };
                let title = if want_title {
                    title_for_recording(&self.store, rec_id)
                } else {
                    String::new()
                };
                let quality = resolved_quality(&row.monitor.quality);
                // Prefer the post-probe; fall back to the pre-probe so a {games}
                // rename in pre-probe mode doesn't drop already-resolved media vars.
                let stem = monitor_stem(
                    &row.monitor,
                    &row.channel.name,
                    started_at,
                    stream_id.as_deref(),
                    &title,
                    row.recording_count,
                    &quality,
                    mi.as_ref().or(pre_media.as_ref()),
                    &games,
                    row.monitor.tool.label(),
                    &plan.mode,
                    row.monitor.platform().as_str(),
                    went_live_at.unwrap_or(0),
                );
                // Stop the YouTube chat sidecar before renaming so its open
                // live_chat.json handle is released before companion rename.
                self.stop_and_wait_for_chat(monitor_id, Duration::from_secs(6)).await;
                final_path = rename_for_media(final_path, &stem).await;
            }
            // Drop this recording's working leftovers (SABR parts/state, etc.).
            if let Some(cache) = cache.as_deref() {
                purge_cache(cache, &capstem).await;
            }
        }

        let bytes = file_len(&final_path).await as i64;

        // Conclude "no footage missed" only when the capture actually spans the
        // whole broadcast (reached the live edge with the head intact). If it
        // ended before catching up (stopped/crashed/stream ended early), the gap
        // is the not-yet-downloaded *tail*, not missed *beginning* — so don't
        // record it as Lost time; leave it unset and let the UI fall back to the
        // provisional `started - went_live` estimate.
        if resolve_lost {
            if let (Some(wl), Some(captured)) =
                (went_live_at, media_duration_secs(&final_path).await)
            {
                let span = (ended - wl).max(0);
                if captured + CATCHUP_TOLERANCE_SECS >= span {
                    let _ = self.store.set_recording_lost_secs(rec_id, 0);
                }
            }
        }

        let duration = now_unix() - started_at;
        let ok = bytes > 0;
        let manually_stopped = self.stopping_monitors.lock().unwrap().remove(&monitor_id);
        let shutting_down = self.shutdown.load(Ordering::SeqCst);
        // SABR from-start stall ("not near live head"): YouTube only serves the
        // last ~4 hours of a live stream via SABR, so once a stream is older than
        // its DVR window each from-start attempt downloads the opening segments
        // then stalls. The next attempt should fall back to live-edge capture (see
        // override at top of fn) so we at least record the ongoing stream.
        //
        // With deep-rewind OFF this is a true window expiry — fall back on the very
        // first stall. With deep-rewind ON the flag extends the window, so an early
        // stall *might* be transient; tolerate a few consecutive stalls before
        // giving up. (Empirically a persistent stall repeats every attempt — each
        // re-fetching ~hundreds of MiB before dying — so without a bound we'd never
        // fall back and never record anything.)
        let deep_rewind = setting_str(&self.store, "ytdlp_sabr_deep_rewind") == "1";
        let sabr_stall = !ok
            && !manually_stopped
            && !shutting_down
            && sabr_dvr_window_exceeded(&outcome.log);
        if sabr_stall {
            let threshold = if deep_rewind { SABR_STALL_FALLBACK_TRIES } else { 1 };
            let stalls = {
                let mut counts = self.sabr_stall_count.lock().unwrap();
                let n = counts.entry(sabr_key.clone()).or_insert(0);
                *n = n.saturating_add(1);
                *n
            };
            if stalls >= threshold {
                self.sabr_dvr_exceeded.lock().unwrap().insert(sabr_key.clone());
                self.sabr_stall_count.lock().unwrap().remove(&sabr_key);
                warn!(monitor_id, stalls, "SABR stalled from-start; next attempt will use live-edge");
            } else {
                warn!(monitor_id, stalls, threshold, "SABR stalled from-start; will retry from-start");
            }
        } else {
            // Any non-stall outcome breaks the consecutive-stall streak, so the
            // counter only ever reflects *back-to-back* from-start stalls. Clear
            // the live-edge fallback flag only when the capture actually succeeded
            // — an "ended"/"aborted"/manual outcome shouldn't un-stick a stream
            // we already decided to capture at the live edge.
            self.sabr_stall_count.lock().unwrap().remove(&sabr_key);
            if ok {
                self.sabr_dvr_exceeded.lock().unwrap().remove(&sabr_key);
            }
        }
        // A 0-byte capture isn't always a failure: a livestream that had already
        // ended (or hadn't started, or exposed no live video formats) leaves
        // nothing to capture but isn't an error. Classify those as `ended` so they
        // don't show as red failures. (`ok` still drives backoff, so we don't
        // hammer an ended broadcast.)
        let status = if manually_stopped {
            // User explicitly stopped the recording; never show it as `failed`.
            if ok { "completed" } else { "stopped" }
        } else if shutting_down {
            // App shutdown killed the process tree; recording was cut short.
            "aborted"
        } else if ok {
            "completed"
        } else if sabr_stall || stream_ended_or_unavailable(&outcome.log) {
            "ended"
        } else {
            "failed"
        };
        let _ = self.store.finish_recording(
            rec_id,
            now_unix(),
            bytes,
            outcome.exit_code,
            status,
            &final_path.to_string_lossy(),
            &outcome.log,
        );
        let _ = self
            .store
            .set_monitor_check_result(monitor_id, status, now_unix());
        let _ = self.events.send(AppEvent::RecordingFinished {
            recording_id: rec_id,
            channel: row.channel.name.clone(),
            status: status.into(),
        });
        self.schedule_vod_check(rec_id, row.monitor.platform(), status, &row.monitor.url, went_live_at, approximate);
        self.schedule_vod_archive(rec_id, &row, went_live_at, status);
        info!(monitor_id, bytes, status, "recording finished");
        if status == "failed" && !outcome.log.is_empty() {
            warn!(monitor_id, "recording stderr:\n{}", outcome.log);
        }

        // A manual stop already installed its own 120s cooldown (see `manual_stop`);
        // don't let the subprocess's exit clobber it — a 0-byte stopped capture would
        // otherwise reset the wait to 30s, and a captured one would clear it entirely,
        // either way re-triggering the moment the next LIVE signal arrives.
        if !manually_stopped {
            self.note_result(monitor_id, duration, ok);
        }
        self.active.lock().unwrap().remove(&monitor_id);
        self.ad_active.lock().unwrap().remove(&monitor_id);
    }

    /// Run the DASH companion capture (dual capture): a self-contained second
    /// recording (system yt-dlp, live edge) that shares the primary's take. Inserts
    /// its own recording row, runs the process tracked under `active_secondary`,
    /// remuxes, and finalizes independently of the primary. Watchers, chat, and
    /// asset fetching all stay on the primary — this just grabs the extra formats.
    #[allow(clippy::too_many_arguments)]
    async fn run_dash_companion(
        &self,
        monitor_id: i64,
        plan: DownloadPlan,
        take_group: String,
        stream_id: Option<String>,
        went_live_at: Option<i64>,
        approximate: bool,
        channel_name: String,
    ) {
        if let Some(parent) = plan.capture_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
            crate::platform::set_hidden(parent);
        }
        if let Some(out_dir) = plan.final_path.parent() {
            let _ = tokio::fs::create_dir_all(out_dir).await;
        }
        let started_at = now_unix();
        let rec_id = self
            .store
            .insert_recording(
                monitor_id,
                started_at,
                &plan.final_path.to_string_lossy(),
                went_live_at,
                approximate,
                stream_id.as_deref(),
                Some(&take_group),
            )
            .unwrap_or(0);
        let _ = self.events.send(AppEvent::RecordingStarted {
            monitor_id,
            recording_id: rec_id,
            channel: channel_name.clone(),
            thumbnail_path: None,
        });

        let outcome = if self.stopping_monitors.lock().unwrap().contains(&monitor_id) {
            ProcessOutcome { exit_code: None, log: String::new() }
        } else {
            self.run_process(
                &self.active_secondary,
                monitor_id,
                &plan,
                None,
                None,
                None,
                DetachReg {
                    kind: DetachedKind::Recording,
                    ref_id: rec_id,
                    monitor_id: Some(monitor_id),
                    take_group: Some(take_group.clone()),
                    started_at,
                    secondary: true,
                    stream_id: stream_id.clone(),
                    went_live_at,
                },
            )
            .await
        };

        // Promote the companion out of .cache\ (remux .ts→.mkv) on success; a failed
        // one stays in .cache\ for the sweep.
        let final_path = promote_capture(&plan).await;
        if final_path != plan.capture_path {
            if let Some(cache) = plan.capture_path.parent() {
                let stem = plan
                    .final_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                purge_cache(cache, &stem).await;
            }
        }

        let bytes = file_len(&final_path).await as i64;
        let ok = bytes > 0;
        let manually_stopped = self.stopping_monitors.lock().unwrap().contains(&monitor_id);
        let shutting_down = self.shutdown.load(Ordering::SeqCst);
        let status = if manually_stopped {
            if ok { "completed" } else { "stopped" }
        } else if shutting_down {
            "aborted"
        } else if ok {
            "completed"
        } else if stream_ended_or_unavailable(&outcome.log) {
            "ended"
        } else {
            "failed"
        };
        let _ = self.store.finish_recording(
            rec_id,
            now_unix(),
            bytes,
            outcome.exit_code,
            status,
            &final_path.to_string_lossy(),
            &outcome.log,
        );
        let _ = self.events.send(AppEvent::RecordingFinished {
            recording_id: rec_id,
            channel: channel_name,
            status: status.into(),
        });
        info!(monitor_id, bytes, status, "dash companion finished");
        self.active_secondary.lock().unwrap().remove(&monitor_id);
    }

    /// At startup, decide what to do with each in-flight (crash-leftover) recording:
    /// resume the SABR-resumable ones (reserving their monitor so the scheduler
    /// won't double-start them) and mark the rest `orphaned`. Synchronous so the
    /// reservations are in place before detection can fire; returns the (recording,
    /// row) pairs to resume (spawned by the caller on the runtime).
    pub fn resume_inflight(&self) -> Vec<(Recording, MonitorWithChannel)> {
        let recs = match self.store.inflight_recordings() {
            Ok(r) => r,
            Err(e) => {
                warn!("resume: failed to load in-flight recordings: {e:#}");
                return Vec::new();
            }
        };
        let ytdlp = load_ytdlp_bins(&self.store);
        let mut to_resume = Vec::new();
        for rec in recs {
            let row = match self.store.get_monitor_with_channel(rec.monitor_id) {
                Ok(Some(r)) => r,
                _ => {
                    let _ = self.store.mark_recording_orphaned(rec.id);
                    continue;
                }
            };
            let m = &row.monitor;
            let resumable = m.platform() == Platform::YouTube
                && m.tool == Tool::YtDlp
                && m.capture_from_start
                && ytdlp.sabr.usable()
                && sabr_state_exists(&rec.output_path);
            if !resumable {
                let _ = self.store.mark_recording_orphaned(rec.id);
                continue;
            }
            // Reserve the monitor (mirrors try_begin) so a concurrent poll can't
            // start a fresh take while we resume this one.
            {
                let mut active = self.active.lock().unwrap();
                if active.contains_key(&m.id) {
                    continue;
                }
                active.insert(m.id, 0);
            }
            info!(monitor_id = m.id, rec_id = rec.id, "resuming interrupted SABR capture");
            to_resume.push((rec, row));
        }
        // Promote orphaned recordings that now have an intact non-TS output file
        // to 'completed'. These are captures where the app crashed after writing
        // finished but before the status column was updated — the content is fine.
        match self.store.promote_intact_orphans() {
            Ok(n) if n > 0 => info!("promoted {n} intact orphaned recording(s) to 'completed'"),
            Ok(_) => {}
            Err(e) => warn!("promote_intact_orphans failed: {e:#}"),
        }
        to_resume
    }

    /// Reconcile the persistent detached-process registry at startup (synchronously,
    /// so reservations land before detection can fire). For each row: re-attach to a
    /// still-running download, finalize one that finished while the app was down, or
    /// hand a SABR-resumable one to the resume path; orphan the rest. Registry-backed
    /// recordings are owned here — `resume_inflight` is fed a registry-excluded set.
    /// Returns the items to drive on the runtime plus the capture stems to protect
    /// from the cache sweep. The caller spawns `reattach_one` for each item.
    pub fn reconcile_detached(&self) -> (Vec<ReattachItem>, std::collections::HashSet<String>) {
        let rows = match self.store.list_detached() {
            Ok(r) => r,
            Err(e) => {
                warn!("reattach: failed to load detached registry: {e:#}");
                return (Vec::new(), HashSet::new());
            }
        };
        let ytdlp = load_ytdlp_bins(&self.store);
        let build = crate::version::build_id();
        let mut items = Vec::new();
        let mut skip = HashSet::new();
        for mut row in rows {
            let spawn_build = row.spawn_build.clone();
            if spawn_build != build {
                crate::compat::reattach_fixups(&spawn_build, &mut row);
            }
            if let Some(stem) = Path::new(&row.capture_path).file_stem() {
                skip.insert(stem.to_string_lossy().into_owned());
            }
            // PID-reuse-safe liveness: the PID must still be running *and* have the
            // creation time we recorded. If we couldn't read the creation time at
            // spawn (proc_start == 0), the identity is unverifiable — don't adopt a
            // possibly-recycled PID; fall through to finalize-from-file / resume / orphan.
            let alive = row.proc_start != 0
                && crate::platform::pid_alive(row.pid)
                && crate::platform::process_start_time(row.pid) == Some(row.proc_start);

            // The DASH companion of a dual capture occupies the secondary map; the
            // primary / videos / chat occupy their own. This is recorded explicitly
            // (`secondary`), not guessed from registry order (the two legs race to
            // register), so a re-attach can never swap their roles.
            let active = match (row.kind, row.secondary) {
                (DetachedKind::Video, _) => self.active_videos.clone(),
                (DetachedKind::Chat, _) => self.active_chats.clone(),
                (DetachedKind::Recording, true) => self.active_secondary.clone(),
                (DetachedKind::Recording, false) => self.active.clone(),
            };
            let key = match row.kind {
                DetachedKind::Video => row.ref_id,
                _ => row.monitor_id.unwrap_or(row.ref_id),
            };

            if alive {
                active.lock().unwrap().insert(key, row.pid);
                info!(
                    kind = row.kind.as_str(),
                    ref_id = row.ref_id,
                    pid = row.pid,
                    spawn_build = %row.spawn_build,
                    "re-attaching to detached download"
                );
                items.push(ReattachItem {
                    row,
                    active,
                    action: ReAction::Adopt,
                });
                continue;
            }

            // Process gone. A SABR-resumable recording is recovered FIRST — SABR
            // writes its media directly (`--no-part`), so it always has a non-empty
            // capture; checking `has_capture` before resumability would wrongly
            // finalize an interrupted SABR take and purge its `.state`.
            let resumable = row.kind == DetachedKind::Recording
                && !row.secondary
                && ytdlp.sabr.usable()
                && sabr_state_exists(&row.final_path)
                && self
                    .store
                    .get_monitor_with_channel(row.monitor_id.unwrap_or(0))
                    .ok()
                    .flatten()
                    .map(|r| {
                        r.monitor.platform() == Platform::YouTube
                            && r.monitor.tool == Tool::YtDlp
                            && r.monitor.capture_from_start
                    })
                    .unwrap_or(false);
            if resumable {
                let mid = row.monitor_id.unwrap_or(0);
                let reserve = {
                    let mut a = self.active.lock().unwrap();
                    if a.contains_key(&mid) {
                        false
                    } else {
                        a.insert(mid, 0);
                        true
                    }
                };
                if reserve {
                    // Make sure no straggler (e.g. a broken-away ffmpeg grandchild)
                    // still holds the `.state`/`.part` files before we re-run.
                    if let Some(job) = crate::platform::DetachedJob::open(&row.job_name) {
                        job.kill();
                    }
                    crate::platform::kill_process_tree(row.pid);
                    info!(
                        monitor_id = mid,
                        rec_id = row.ref_id,
                        "resuming detached SABR capture"
                    );
                    items.push(ReattachItem {
                        row,
                        active: self.active.clone(),
                        action: ReAction::Resume,
                    });
                    continue;
                }
            }

            // Otherwise finalize when there's a usable capture on disk…
            let has_capture = std::fs::metadata(&row.capture_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
                || std::fs::metadata(&row.final_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);
            if has_capture {
                info!(
                    kind = row.kind.as_str(),
                    ref_id = row.ref_id,
                    "detached download finished while app was down; finalizing"
                );
                items.push(ReattachItem {
                    row,
                    active,
                    action: ReAction::Finalize,
                });
                continue;
            }

            // …otherwise nothing to recover: orphan the row and drop the registry entry.
            match row.kind {
                DetachedKind::Recording => {
                    let _ = self.store.mark_recording_orphaned(row.ref_id);
                }
                DetachedKind::Video => {
                    let _ = self.store.set_video_status(row.ref_id, "orphaned");
                }
                DetachedKind::Chat => {}
            }
            let _ = self.store.clear_detached(row.kind, row.ref_id);
        }
        (items, skip)
    }

    /// Drive one reconciled detached download to completion on the runtime.
    pub async fn reattach_one(&self, item: ReattachItem) {
        let ReattachItem { row, active, action } = item;
        match action {
            ReAction::Resume => {
                let mid = row.monitor_id.unwrap_or(0);
                match self.store.get_monitor_with_channel(mid).ok().flatten() {
                    Some(mrow) => self.resume_recording(recording_from_detached(&row), mrow).await,
                    None => {
                        self.active.lock().unwrap().remove(&mid);
                        let _ = self.store.mark_recording_orphaned(row.ref_id);
                        let _ = self.store.clear_detached(row.kind, row.ref_id);
                    }
                }
            }
            ReAction::Finalize => self.finalize_reattached(&row, &active).await,
            ReAction::Adopt => self.adopt_detached(row, active).await,
        }
    }

    /// Re-attached to a still-running download: repaint the UI, resume full live
    /// detail by tailing the log from its current end (pre-restart ad breaks are
    /// already persisted), wait for the process, then finalize — unless we're
    /// quitting again (then leave the registry row for the next launch).
    async fn adopt_detached(&self, row: DetachedRow, active: ActiveSet) {
        let key = match row.kind {
            DetachedKind::Video => row.ref_id,
            _ => row.monitor_id.unwrap_or(row.ref_id),
        };
        if let Some(mid) = row.monitor_id {
            let state = if row.kind == DetachedKind::Chat {
                "chat_active"
            } else {
                "recording"
            };
            let _ = self.events.send(AppEvent::MonitorState {
                monitor_id: mid,
                state: state.into(),
            });
        }

        // Chat sidecars carry no live state to reconstruct — just wait and clear.
        if row.kind == DetachedKind::Chat {
            let exited = self.wait_for_exit(row.pid).await;
            // Free the slot either way: on a real exit it's done; on shutdown,
            // leaving it would block stop_all_recordings' drain loop for 120s.
            active.lock().unwrap().remove(&key);
            if !exited {
                return; // re-detaching on quit — keep the registry row
            }
            let _ = self.store.clear_detached(DetachedKind::Chat, row.ref_id);
            if let Some(mid) = row.monitor_id {
                let _ = self.events.send(AppEvent::MonitorState {
                    monitor_id: mid,
                    state: "idle".into(),
                });
            }
            return;
        }

        // Fetch the monitor row once (recordings only) to rebuild the ad pipeline
        // and the live watchers.
        let mrow = if row.kind == DetachedKind::Recording {
            row.monitor_id
                .and_then(|mid| self.store.get_monitor_with_channel(mid).ok().flatten())
        } else {
            None
        };

        let log_path = PathBuf::from(&row.log_path);
        // Start tailing at a line boundary so a partial line at the seek point
        // (e.g. an ad-break marker being written at the restart instant) isn't
        // split and lost; re-reading that single line is safe (ad inserts are
        // idempotent, progress is overwrite).
        let start_offset = line_aligned_tail_offset(&log_path).await;
        let done = Arc::new(AtomicBool::new(false));
        let (progress, speed) = if row.kind == DetachedKind::Video {
            (Some(self.video_progress.clone()), Some(self.video_speed.clone()))
        } else {
            (None, None)
        };
        // Rebuild the ad pipeline for a re-attached Twitch+streamlink recording so
        // new breaks keep being recorded (historical ones are already persisted).
        let ad_sink = mrow
            .as_ref()
            .filter(|m| m.monitor.tool == Tool::Streamlink && m.monitor.platform() == Platform::Twitch)
            .map(|m| AdSink {
                store: self.store.clone(),
                events: self.events.clone(),
                monitor_id: row.monitor_id.unwrap_or(0),
                recording_id: row.ref_id,
                started_at: row.started_at,
                went_live_at: row.went_live_at,
                from_start: m.monitor.capture_from_start,
                capture_path: PathBuf::from(&row.capture_path),
                ad_active: self.ad_active.clone(),
            });
        let (ad_tx, ad_task) = match ad_sink {
            Some(sink) => {
                let (tx, jh) = spawn_ad_processor(sink);
                (Some(tx), Some(jh))
            }
            None => (None, None),
        };

        // Re-spawn the live watchers exactly as the in-session record() path does —
        // otherwise a re-attached take's Lost-time never resolves (catch-up watcher)
        // and its Game/Title freeze (meta watcher) at the pre-restart values.
        let watcher_done = Arc::new(AtomicBool::new(false));
        let mut watchers: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        if let Some(m) = &mrow {
            let from_start = m.monitor.capture_from_start
                && matches!(m.monitor.tool, Tool::Streamlink | Tool::YtDlp);
            if from_start && row.went_live_at.is_some() {
                watchers.push(tokio::spawn(catch_up_watcher(
                    self.store.clone(),
                    self.events.clone(),
                    row.monitor_id.unwrap_or(0),
                    row.ref_id,
                    PathBuf::from(&row.capture_path),
                    row.went_live_at.unwrap_or(0),
                    watcher_done.clone(),
                )));
            }
            if m.monitor.platform() != Platform::Generic {
                watchers.push(tokio::spawn(meta_watcher(
                    self.ctx.clone(),
                    self.store.clone(),
                    self.events.clone(),
                    row.monitor_id.unwrap_or(0),
                    row.ref_id,
                    row.started_at,
                    m.monitor.url.clone(),
                    m.monitor.platform(),
                    watcher_done.clone(),
                    self.shutdown.clone(),
                )));
            }
            // Twitch chat logger is a native in-process task (not a tracked
            // process), so re-attach must restart it to keep appending to the
            // `.chat.jsonl` sidecar. (YouTube chat is a yt-dlp process and
            // re-attaches via its own registry row.)
            if m.monitor.chat_log && m.monitor.platform() == Platform::Twitch {
                let chat_path = PathBuf::from(&row.final_path).with_extension("chat.jsonl");
                watchers.push(tokio::spawn(crate::chat::log_twitch_chat(
                    m.monitor.url.clone(),
                    chat_path,
                    watcher_done.clone(),
                    self.shutdown.clone(),
                )));
            }
        }

        let tail = tokio::spawn(tail_log(
            log_path,
            start_offset,
            progress,
            speed,
            key,
            ad_tx,
            done.clone(),
        ));

        // Stall watchdog for the adopted process too — a re-attached capture
        // whose tool wedged after the stream ended would otherwise sit
        // "recording" with a growing uptime until the app is restarted.
        let stall = self.spawn_stall_watchdog(
            row.kind,
            key,
            row.pid,
            PathBuf::from(&row.log_path),
            PathBuf::from(&row.capture_path),
            done.clone(),
        );

        let exited = self.wait_for_exit(row.pid).await;
        done.store(true, Ordering::SeqCst);
        stall.abort();
        watcher_done.store(true, Ordering::SeqCst);
        let _ = tail.await;
        if let Some(t) = ad_task {
            let _ = t.await;
        }
        for w in watchers {
            let _ = w.await;
        }
        if !exited {
            // Quitting again: keep the registry row for the next launch, but free
            // the in-memory slot so a concurrent stop_all drain loop terminates.
            active.lock().unwrap().remove(&key);
            return;
        }
        self.finalize_reattached(&row, &active).await;
    }

    /// Block until `pid` truly exits, returning false if shutdown was requested
    /// first. Re-checks liveness after each wait so a spurious `WaitForSingleObject`
    /// failure can't trigger a premature finalize while the tool is still writing.
    /// Kill a capture/download whose output has completely stopped changing.
    ///
    /// Recovery for tools that wedge instead of exiting — yt-dlp hanging at the
    /// live edge after a stream ends, streamlink on a dead HLS session, a stuck
    /// VOD download. Without this, `child.wait()` / `wait_for_exit` blocks
    /// forever and the row shows "recording"/"live" with a growing uptime for
    /// hours after the stream ended.
    ///
    /// Every 60s it samples an activity signature — the log file's handle size
    /// plus the handle sizes of every capture-stem file in the capture dir
    /// (covers SABR `.part` files and ffmpeg merge temps; handle sizes because
    /// NTFS dir entries are stale for open files). If NOTHING changes for
    /// [`STALL_KILL_SECS`], it marks the id via the same stop-tombstone a user
    /// stop uses (so classification lands on completed/stopped — never a bogus
    /// "completed" for a truncated VOD download) and kills the process tree;
    /// the normal wait → finalize path then runs. If the channel is actually
    /// still live, the next detection poll simply starts a fresh take.
    fn spawn_stall_watchdog(
        &self,
        kind: DetachedKind,
        key: i64,
        pid: u32,
        log_path: PathBuf,
        capture_path: PathBuf,
        done: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut last_sig = u64::MAX;
            let mut last_change = Instant::now();
            loop {
                crate::app_core::sleep_cancellable(Duration::from_secs(60), &this.shutdown).await;
                if done.load(Ordering::SeqCst) || this.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let sig = stall_signature(&log_path, &capture_path).await;
                if sig != last_sig {
                    last_sig = sig;
                    last_change = Instant::now();
                    continue;
                }
                if last_change.elapsed() < Duration::from_secs(STALL_KILL_SECS) {
                    continue;
                }
                warn!(
                    ?kind,
                    key,
                    pid,
                    "no output for {} min — killing stalled process tree",
                    STALL_KILL_SECS / 60
                );
                match kind {
                    DetachedKind::Video => {
                        this.stopping_videos.lock().unwrap().insert(key);
                    }
                    DetachedKind::Recording => {
                        this.stopping_monitors.lock().unwrap().insert(key);
                    }
                    DetachedKind::Chat => {}
                }
                let _ = this.events.send(AppEvent::Error {
                    context: "Stall watchdog".into(),
                    message: format!(
                        "Download produced no output for {} minutes — stopping the stuck process (pid {pid}) and finalizing.",
                        STALL_KILL_SECS / 60
                    ),
                });
                crate::platform::kill_process_tree(pid);
                return;
            }
        })
    }

    async fn wait_for_exit(&self, pid: u32) -> bool {
        loop {
            let shutdown = self.shutdown.clone();
            let _ = tokio::task::spawn_blocking(move || crate::platform::wait_pid(pid, &shutdown))
                .await;
            if self.shutdown.load(Ordering::SeqCst) {
                return false;
            }
            if !crate::platform::pid_alive(pid) {
                return true;
            }
            // wait_pid returned while the process is still alive (spurious) — retry.
            crate::app_core::sleep_cancellable(Duration::from_millis(500), &self.shutdown).await;
        }
    }

    /// Promote and record a re-attached download once its process is gone: move/remux
    /// the capture into the output dir, do the same post-capture `{games}`/`{title}`/
    /// `{resolution}` rename + lost-time accounting the in-session path does, finalize
    /// the recording/video row, drop the registry entry, and free the active-map slot.
    async fn finalize_reattached(&self, row: &DetachedRow, active: &ActiveSet) {
        let capture_path = PathBuf::from(&row.capture_path);
        let final_pred = PathBuf::from(&row.final_path);
        let mut final_path = if row.remux_to_mkv {
            promote_capture(&DownloadPlan {
                program: String::new(),
                args: Vec::new(),
                capture_path: capture_path.clone(),
                final_path: final_pred.clone(),
                remux_to_mkv: true,
                writes_own_thumbnail: false,
                mode: String::new(),
            })
            .await
        } else {
            // Already-final container: move the predicted file, or the newest
            // `{stem}.*` the tool actually produced (yt-dlp may differ in extension).
            let produced = if file_len(&capture_path).await > 0 {
                Some(capture_path.clone())
            } else {
                newest_with_stem(&capture_path).await
            };
            match produced {
                Some(src) => {
                    let dest = final_pred.with_file_name(
                        src.file_name().map(|n| n.to_os_string()).unwrap_or_default(),
                    );
                    if let Some(p) = dest.parent() {
                        let _ = tokio::fs::create_dir_all(p).await;
                    }
                    // The download landing on disk matters more than a fully-
                    // descriptive name — see rename_or_shorten.
                    let dest_dir = dest.parent().unwrap_or_else(|| Path::new("."));
                    let dest_stem = dest
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dest_ext = dest
                        .extension()
                        .map(|e| e.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match rename_or_shorten(&src, dest_dir, &dest_stem, &dest_ext).await {
                        Ok(actual) => actual,
                        Err(_) => src,
                    }
                }
                None => final_pred.clone(),
            }
        };

        // For a recording, fetch the monitor row and apply the in-session post-capture
        // rename (fill {games}/{title}/{resolution}) so a re-attached take isn't named
        // differently from one finalized in-session (no leftover "tba" placeholders).
        let mrow = if row.kind == DetachedKind::Recording {
            self.store
                .get_monitor_with_channel(row.monitor_id.unwrap_or(0))
                .ok()
                .flatten()
        } else {
            None
        };
        if let Some(mrow) = &mrow {
            let want_games = template_wants_games(&mrow.monitor.filename_template);
            let want_title = template_wants_title(&mrow.monitor.filename_template);
            let want_went_live = template_wants_went_live(&mrow.monitor.filename_template);
            let do_post_media =
                template_wants_media(&mrow.monitor.filename_template) && media_info_mode(&self.store).post();
            if want_games || want_title || do_post_media || want_went_live {
                let mi = if do_post_media {
                    probe_media(&final_path.to_string_lossy()).await
                } else {
                    None
                };
                let games = if want_games {
                    games_for_recording(&self.store, row.ref_id)
                } else {
                    String::new()
                };
                let title = if want_title {
                    title_for_recording(&self.store, row.ref_id)
                } else {
                    String::new()
                };
                let quality = resolved_quality(&mrow.monitor.quality);
                let ytdlp_bins_fa = load_ytdlp_bins(&self.store);
                let use_sabr_fa = sabr_selected(&mrow.monitor, &ytdlp_bins_fa);
                let mode_fa = recording_mode(&mrow.monitor, use_sabr_fa, row.secondary);
                let stem = monitor_stem(
                    &mrow.monitor,
                    &mrow.channel.name,
                    row.started_at,
                    row.stream_id.as_deref(),
                    &title,
                    mrow.recording_count,
                    &quality,
                    mi.as_ref(),
                    &games,
                    mrow.monitor.tool.label(),
                    &mode_fa,
                    mrow.monitor.platform().as_str(),
                    row.went_live_at.unwrap_or(0),
                );
                // Stop the YouTube chat sidecar before renaming so its open
                // live_chat.json handle is released before companion rename.
                if let Some(mid) = row.monitor_id {
                    self.stop_and_wait_for_chat(mid, Duration::from_secs(6)).await;
                }
                final_path = rename_for_media(final_path, &stem).await;
            }
        }

        let ended = now_unix();
        // Lost-time: zero it only when the capture spans the whole broadcast (same
        // rule as the in-session finalize); else leave it for the provisional estimate.
        if row.kind == DetachedKind::Recording {
            if let (Some(wl), Some(captured)) =
                (row.went_live_at, media_duration_secs(&final_path).await)
            {
                if captured + CATCHUP_TOLERANCE_SECS >= (ended - wl).max(0) {
                    let _ = self.store.set_recording_lost_secs(row.ref_id, 0);
                }
            }
        }

        let bytes = file_len(&final_path).await as i64;
        let log = read_log_tail(&PathBuf::from(&row.log_path), RING_MAX_LINES).await;
        let key = match row.kind {
            DetachedKind::Video => row.ref_id,
            _ => row.monitor_id.unwrap_or(row.ref_id),
        };

        match row.kind {
            DetachedKind::Recording => {
                // Honour a manual stop that raced the re-attach so the take shows
                // 'stopped', not 'failed', when it ended empty.
                let manually_stopped = row
                    .monitor_id
                    .map(|mid| self.stopping_monitors.lock().unwrap().remove(&mid))
                    .unwrap_or(false);
                let status = if manually_stopped {
                    if bytes > 0 { "completed" } else { "stopped" }
                } else if bytes > 0 {
                    "completed"
                } else if stream_ended_or_unavailable(&log) {
                    "ended"
                } else {
                    "failed"
                };
                let _ = self.store.finish_recording(
                    row.ref_id,
                    ended,
                    bytes,
                    None,
                    status,
                    &final_path.to_string_lossy(),
                    &log,
                );
                active.lock().unwrap().remove(&key);
                let vod_platform = mrow.as_ref().map(|r| r.monitor.platform());
                let vod_url = mrow.as_ref().map(|r| r.monitor.url.clone()).unwrap_or_default();
                let channel = mrow.map(|r| r.channel.name).unwrap_or_default();
                let _ = self.events.send(AppEvent::RecordingFinished {
                    recording_id: row.ref_id,
                    channel,
                    status: status.into(),
                });
                if let Some(platform) = vod_platform {
                    let approx = self.store.recording_went_live_approx(row.ref_id);
                    self.schedule_vod_check(row.ref_id, platform, status, &vod_url, row.went_live_at, approx);
                }
                if let Some(mid) = row.monitor_id {
                    let _ = self.events.send(AppEvent::MonitorState {
                        monitor_id: mid,
                        state: "offline".into(),
                    });
                }
                info!(rec_id = row.ref_id, bytes, status, "re-attached recording finalized");
            }
            DetachedKind::Video => {
                let stopped = self.stopping_videos.lock().unwrap().remove(&row.ref_id);
                let status = if stopped {
                    "stopped"
                } else if bytes > 0 {
                    "completed"
                } else {
                    "failed"
                };
                let _ = self.store.finish_video(
                    row.ref_id,
                    ended,
                    bytes,
                    None,
                    status,
                    &final_path.to_string_lossy(),
                    &log,
                );
                active.lock().unwrap().remove(&key);
                self.video_progress.lock().unwrap().remove(&key);
                self.video_speed.lock().unwrap().remove(&key);
                info!(video = row.ref_id, bytes, status, "re-attached video finalized");
            }
            DetachedKind::Chat => {
                active.lock().unwrap().remove(&key);
            }
        }
        let _ = self.store.clear_detached(row.kind, row.ref_id);
        // Drop this download's `.cache\` working leftovers.
        if let Some(cache) = capture_path.parent() {
            if let Some(stem) = final_pred.file_stem().map(|s| s.to_string_lossy().into_owned()) {
                purge_cache(cache, &stem).await;
            }
        }
    }

    /// Resume an interrupted SABR capture, reusing the orphaned recording's `rec_id`
    /// and `.cache\` stem so yt-dlp continues from the surviving `.state`/`.part`
    /// (re-invoked with the identical `-o`). Chat and the DASH companion are not
    /// resumed. Caller must have already reserved `active[monitor_id]`.
    pub async fn resume_recording(&self, rec: Recording, row: MonitorWithChannel) {
        let monitor_id = row.monitor.id;
        let rec_id = rec.id;
        let out_path = PathBuf::from(&rec.output_path);
        let Some(out_dir) = out_path.parent().map(Path::to_path_buf) else {
            self.active.lock().unwrap().remove(&monitor_id);
            return;
        };
        let stem = out_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let capture = cache_dir(&out_dir).join(format!("{stem}.mkv"));

        // Resolve auth + args exactly as a fresh capture would, so the command — and
        // thus the SABR resume — matches the original (`-o` is identical).
        let global_method = self
            .store
            .get_setting("download_auth_method")
            .ok()
            .flatten()
            .unwrap_or_default();
        let global_browser = self
            .store
            .get_setting("cookies_browser")
            .ok()
            .flatten()
            .unwrap_or_default();
        let auth = resolve_auth(&row, &global_method, &global_browser);
        let ytdlp_global_raw = self
            .store
            .get_setting("ytdlp_default_args")
            .ok()
            .flatten()
            .unwrap_or_default();
        let ytdlp_global_args = split_args(&ytdlp_global_raw);
        let ytdlp_bins = load_ytdlp_bins(&self.store);
        let extra = split_args(&row.monitor.extra_args);
        // Only from-start takes leave resumable SABR `.state` (the resume gates
        // in `resume_inflight`/`reconcile_detached` still require capture_from_start),
        // so every take reaching here is from-start → this reproduces the original
        // `--live-from-start` byte-for-byte. (A from-start monitor whose *take* was
        // downgraded to the edge by the DVR fallback still persists
        // capture_from_start=1; yt-dlp's SABR resume continues from the `.state`
        // sequence cursor regardless of this flag, so it's safe.)
        let args = sabr_capture_args(
            &capture, &auth, &ytdlp_global_args, &ytdlp_bins.sabr, &extra, &row.monitor.url,
            row.monitor.capture_from_start, &resolve_sabr_sort(&row.monitor, &ytdlp_bins.sabr),
        );
        let use_sabr_resume = true; // resume is always SABR
        let mode_resume = recording_mode(&row.monitor, use_sabr_resume, false);
        let plan = DownloadPlan {
            program: ytdlp_bins.sabr.binary.clone(),
            args,
            capture_path: capture,
            final_path: out_path,
            remux_to_mkv: false,
            writes_own_thumbnail: false,
            mode: mode_resume,
        };

        let _permit = self.sem.acquire().await.expect("semaphore");
        if self.shutdown.load(Ordering::SeqCst) {
            self.active.lock().unwrap().remove(&monitor_id);
            return;
        }
        if let Some(parent) = plan.capture_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
            crate::platform::set_hidden(parent);
        }
        let _ = self
            .store
            .set_monitor_check_result(monitor_id, "recording", now_unix());
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "recording".into(),
        });
        let _ = self.events.send(AppEvent::RecordingStarted {
            monitor_id,
            recording_id: rec_id,
            channel: row.channel.name.clone(),
            thumbnail_path: None,
        });
        info!(monitor_id, rec_id, program = %plan.program, "resuming SABR capture -> {}", plan.final_path.display());

        let outcome = self
            .run_process(
                &self.active,
                monitor_id,
                &plan,
                None,
                None,
                None,
                DetachReg {
                    kind: DetachedKind::Recording,
                    ref_id: rec_id,
                    monitor_id: Some(monitor_id),
                    take_group: rec.take_group.clone(),
                    started_at: rec.started_at,
                    secondary: false,
                    stream_id: rec.stream_id.clone(),
                    went_live_at: rec.went_live_at,
                },
            )
            .await;
        let ended = now_unix();

        // Finalize: promote .cache → output dir, move companions, post-rename, purge.
        let mut final_path = promote_capture(&plan).await;
        let promoted = final_path != plan.capture_path;
        let cache = plan.capture_path.parent().map(Path::to_path_buf);
        let capstem = plan
            .final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if promoted {
            if let (Some(cache), Some(od)) = (cache.as_deref(), final_path.parent()) {
                move_companions(cache, od, &capstem).await;
            }
            let want_games = template_wants_games(&row.monitor.filename_template);
            let want_title = template_wants_title(&row.monitor.filename_template);
            let want_went_live = template_wants_went_live(&row.monitor.filename_template);
            let want_media = template_wants_media(&row.monitor.filename_template);
            let media_mode = media_info_mode(&self.store);
            let do_post_media = want_media && media_mode.post();
            if do_post_media || want_games || want_title || want_went_live {
                let mi = if do_post_media {
                    probe_media(&final_path.to_string_lossy()).await
                } else {
                    None
                };
                let games = if want_games {
                    games_for_recording(&self.store, rec_id)
                } else {
                    String::new()
                };
                let title = if want_title {
                    title_for_recording(&self.store, rec_id)
                } else {
                    String::new()
                };
                let quality = resolved_quality(&row.monitor.quality);
                let use_sabr_res = sabr_selected(&row.monitor, &ytdlp_bins);
                let mode_res = recording_mode(&row.monitor, use_sabr_res, false);
                let stem = monitor_stem(
                    &row.monitor,
                    &row.channel.name,
                    rec.started_at,
                    rec.stream_id.as_deref(),
                    &title,
                    row.recording_count,
                    &quality,
                    mi.as_ref(),
                    &games,
                    row.monitor.tool.label(),
                    &mode_res,
                    row.monitor.platform().as_str(),
                    rec.went_live_at.unwrap_or(0),
                );
                final_path = rename_for_media(final_path, &stem).await;
            }
            if let Some(cache) = cache.as_deref() {
                purge_cache(cache, &capstem).await;
            }
        }

        let bytes = file_len(&final_path).await as i64;
        // Lost-time: a from-start capture that reached the live edge missed nothing.
        if let (Some(wl), Some(captured)) =
            (rec.went_live_at, media_duration_secs(&final_path).await)
        {
            let span = (ended - wl).max(0);
            if captured + CATCHUP_TOLERANCE_SECS >= span {
                let _ = self.store.set_recording_lost_secs(rec_id, 0);
            }
        }
        let ok = bytes > 0;
        let manually_stopped = self.stopping_monitors.lock().unwrap().remove(&monitor_id);
        let shutting_down = self.shutdown.load(Ordering::SeqCst);
        let status = if manually_stopped {
            if ok { "completed" } else { "stopped" }
        } else if shutting_down {
            "aborted"
        } else if ok {
            "completed"
        } else if stream_ended_or_unavailable(&outcome.log) {
            "ended"
        } else {
            "failed"
        };
        let _ = self.store.finish_recording(
            rec_id,
            now_unix(),
            bytes,
            outcome.exit_code,
            status,
            &final_path.to_string_lossy(),
            &outcome.log,
        );
        let _ = self
            .store
            .set_monitor_check_result(monitor_id, status, now_unix());
        let _ = self.events.send(AppEvent::RecordingFinished {
            recording_id: rec_id,
            channel: row.channel.name.clone(),
            status: status.into(),
        });
        self.schedule_vod_check(rec_id, row.monitor.platform(), status, &row.monitor.url, rec.went_live_at, rec.went_live_approx);
        info!(monitor_id, rec_id, bytes, status, "resumed recording finished");
        self.active.lock().unwrap().remove(&monitor_id);
    }

    /// Delete stale working files from every output dir's `.cache\` (older than
    /// [`CACHE_MAX_AGE_SECS`]), skipping any stem currently being resumed. Removes a
    /// `.cache\` dir that ends up empty. Best-effort; runs once at startup.
    pub async fn sweep_caches(&self, skip_stems: std::collections::HashSet<String>) {
        let dirs = self.store.all_output_dirs().unwrap_or_default();
        let now = std::time::SystemTime::now();
        for d in dirs {
            let cache = cache_dir(Path::new(&d));
            let Ok(mut rd) = tokio::fs::read_dir(&cache).await else {
                continue;
            };
            let mut removed = 0u32;
            while let Ok(Some(entry)) = rd.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if skip_stems
                    .iter()
                    .any(|s| name.starts_with(&format!("{s}.")))
                {
                    continue; // belongs to a recording being resumed
                }
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                let stale = meta
                    .modified()
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .map(|age| age.as_secs() >= CACHE_MAX_AGE_SECS)
                    .unwrap_or(false);
                if stale && meta.is_file() && tokio::fs::remove_file(entry.path()).await.is_ok() {
                    removed += 1;
                }
            }
            if removed > 0 {
                info!("swept {removed} stale .cache file(s) from {d}");
            }
            let _ = tokio::fs::remove_dir(&cache).await; // only if now empty
        }
    }

    /// Mark a Twitch recording as VOD-pending and spawn the background poller.
    /// No-op for non-Twitch platforms, or statuses that imply the stream had
    /// already ended before capture began (`ended`).
    /// When `went_live_approx` is true the stored go-live time is our detection
    /// clock rather than the platform-reported start — pass `None` to the VOD
    /// matcher so it falls back to "most recent archive" instead of a stale anchor.
    fn schedule_vod_check(
        &self,
        rec_id: i64,
        platform: Platform,
        status: &str,
        monitor_url: &str,
        went_live_at: Option<i64>,
        went_live_approx: bool,
    ) {
        if platform != Platform::Twitch || status == "ended" {
            return;
        }
        let Some(login) = crate::detectors::twitch_login(monitor_url) else {
            return;
        };
        let anchor = if went_live_approx { None } else { went_live_at };
        let _ = self.store.set_recording_vod_pending(rec_id);
        tokio::spawn(check_twitch_vod(
            Arc::clone(&self.ctx),
            Arc::clone(&self.store),
            self.events.clone(),
            self.manual_tx.clone(),
            rec_id,
            login,
            anchor,
        ));
    }

    /// Post-stream published-VOD download for a **YouTube/Kick** recording (Twitch
    /// is handled inside [`check_twitch_vod`]). No-op unless the feature resolves ON
    /// (global < channel < instance). Waits for the platform's VOD to be ready, then
    /// enqueues a detached yt-dlp download linked to the recording.
    fn schedule_vod_archive(
        &self,
        rec_id: i64,
        row: &MonitorWithChannel,
        went_live_at: Option<i64>,
        status: &str,
    ) {
        if status == "ended" {
            return;
        }
        let platform = row.monitor.platform();
        if !matches!(platform, Platform::YouTube | Platform::Kick) {
            return;
        }
        if !crate::vod_archive::effective_vod_download(&self.store, row.monitor.channel_id, row.monitor.id) {
            return;
        }
        // stream_id (== the YouTube video id) travels on the recording row.
        let stream_id = self
            .store
            .monitor_id_for_recording(rec_id)
            .ok()
            .flatten()
            .and_then(|(_, sid)| sid);
        let _ = self.store.set_recording_vod_dl(rec_id, "downloading", None);
        let (store, manual_tx, ctx) = (self.store.clone(), self.manual_tx.clone(), self.ctx.clone());
        let monitor_url = row.monitor.url.clone();
        tokio::spawn(async move {
            let url = match platform {
                Platform::YouTube => {
                    // The VOD is the same video; give post-live processing some time.
                    tokio::time::sleep(Duration::from_secs(VOD_POLL_INTERVAL_SECS)).await;
                    stream_id.as_deref().map(crate::vod_archive::youtube_vod_url)
                }
                Platform::Kick => {
                    let Some(slug) = crate::vod_archive::kick_slug(&monitor_url) else {
                        let _ = store.set_recording_vod_dl(rec_id, "failed", None);
                        return;
                    };
                    let client = ctx.http_client();
                    let mut found = None;
                    for _ in 0..VOD_MAX_POLLS {
                        tokio::time::sleep(Duration::from_secs(VOD_POLL_INTERVAL_SECS)).await;
                        if let Some(u) = crate::vod_archive::resolve_kick_vod(&client, &slug, went_live_at).await {
                            found = Some(u);
                            break;
                        }
                    }
                    found
                }
                _ => None,
            };
            match url {
                Some(u) => {
                    enqueue_vod_archive(&store, &manual_tx, rec_id, &u);
                }
                None => {
                    let _ = store.set_recording_vod_dl(rec_id, "failed", None);
                }
            }
        });
    }

    /// Completion hook for a VOD-archive download: record the file on the recording
    /// and, when replace-on-success resolves ON and the VOD isn't muted, swap it in
    /// for the live capture. A no-op for ordinary (non-archive) video downloads.
    async fn finalize_vod_archive(&self, video_id: i64, final_path: &Path, status: &str) {
        let Ok(Some(rec_id)) = self.store.recording_for_vod_video(video_id) else {
            return;
        };
        if status != "completed" {
            let _ = self.store.set_recording_vod_dl(rec_id, "failed", Some(video_id));
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
            return;
        }
        let vod_path = final_path.to_string_lossy().into_owned();
        let _ = self.store.set_recording_vod_archived(rec_id, &vod_path, "archived");

        let Ok(Some((channel_id, monitor_id, live_path, muted_secs))) =
            self.store.recording_replace_info(rec_id)
        else {
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
            return;
        };
        let muted = muted_secs.unwrap_or(0) > 0;
        let replace = !muted
            && !live_path.is_empty()
            && crate::vod_archive::effective_vod_replace(&self.store, channel_id, monitor_id);
        if replace {
            // Only now that the VOD is confirmed good do we touch the live file.
            // Rename the VOD to the live stem so the live sidecars stay matched.
            let live = PathBuf::from(&live_path);
            let _ = tokio::fs::remove_file(&live).await;
            match tokio::fs::rename(final_path, &live).await {
                Ok(()) => {
                    let live_s = live.to_string_lossy().into_owned();
                    let _ = self.store.update_recording_output_path(rec_id, &live_s);
                    let _ = self.store.set_recording_vod_archived(rec_id, &live_s, "replaced");
                    info!(rec_id, "vod archive: replaced live recording with the published VOD");
                }
                Err(e) => warn!(rec_id, "vod archive: replace rename failed: {e:#} (kept alongside)"),
            }
        }
        let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
    }

    async fn run_process(
        &self,
        active: &ActiveSet,
        id: i64,
        plan: &DownloadPlan,
        progress: Option<VideoProgress>,
        speed: Option<VideoSpeed>,
        ads: Option<AdSink>,
        detach: DetachReg,
    ) -> ProcessOutcome {
        // The tool's combined stdout+stderr go to a log file the app TAILS rather
        // than a pipe it reads: a pipe dies with the parent, but a file the child
        // owns keeps growing after the app detaches/quits — and a later launch can
        // re-open and re-tail it. Lives next to the capture under `.cache\`.
        let log_path = plan.capture_path.with_extension("log");
        let (out_h, err_h) = match open_log_pair(&log_path) {
            Ok(p) => p,
            Err(e) => {
                return ProcessOutcome {
                    exit_code: None,
                    log: format!("failed to open log {}: {e}", log_path.display()),
                };
            }
        };

        // A *named* job WITHOUT kill-on-close: the tool stays alive when we exit,
        // and a relaunch can re-open the job by name to stop the whole tree.
        let job_name = format!(
            "Local\\StreamArchiver_{}_{}",
            detach.kind.as_str(),
            detach.ref_id
        );
        let job = DetachedJob::create(&job_name).ok();

        let mut cmd = Command::new(&plan.program);
        // No kill_on_drop: the child must survive this task being dropped (detach).
        // A regular file handle (like a pipe) reads as isatty()=False, so yt-dlp
        // skips the console-width probe that crashes on a NUL handle.
        cmd.args(&plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_h))
            .stderr(Stdio::from(err_h));
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ProcessOutcome {
                    exit_code: None,
                    log: format!("failed to spawn {}: {e}", plan.program),
                };
            }
        };
        if let Some(j) = &job {
            if let Err(e) = j.assign_child(&child) {
                warn!("job assign failed: {e:#}");
            }
        }
        // Register the real PID so the scheduler skips this work and shutdown
        // can kill the whole process tree.
        let pid = child.id().unwrap_or(0);
        if pid != 0 {
            active.lock().unwrap().insert(id, pid);
        }

        // Persist a registry row right after the spawn — synchronously, before the
        // first `.await` (child.wait below) — so a clean detach always has a row, and
        // only a crash in this sub-millisecond window could miss one. Deleted when
        // this download finalizes in-session below.
        if detach.ref_id != 0 && pid != 0 {
            let row = DetachedRow {
                kind: detach.kind,
                ref_id: detach.ref_id,
                monitor_id: detach.monitor_id,
                pid,
                proc_start: crate::platform::process_start_time(pid).unwrap_or(0),
                job_name: job_name.clone(),
                log_path: log_path.to_string_lossy().into_owned(),
                capture_path: plan.capture_path.to_string_lossy().into_owned(),
                final_path: plan.final_path.to_string_lossy().into_owned(),
                remux_to_mkv: plan.remux_to_mkv,
                take_group: detach.take_group.clone(),
                spawn_build: crate::version::build_id().to_string(),
                started_at: detach.started_at,
                secondary: detach.secondary,
                stream_id: detach.stream_id.clone(),
                went_live_at: detach.went_live_at,
            };
            if let Err(e) = self.store.register_detached(&row) {
                warn!("register detached process failed: {e:#}");
            }
        }

        // Ad-break processor (Twitch+streamlink only): the tailer forwards each
        // `(detected_at, duration)` over a channel; a dedicated task does the
        // (potentially slow) ffprobe + DB insert. Same helper drives re-attach.
        let (ad_tx, ad_task) = match ads {
            Some(sink) => {
                let (tx, jh) = spawn_ad_processor(sink);
                (Some(tx), Some(jh))
            }
            None => (None, None),
        };

        // One tailer reads the growing log file and does everything the two pipe
        // readers used to: parse progress/speed (yt-dlp `--progress-template`) and
        // streamlink ad-break lines. It stops once the process has exited (`done`)
        // and it has drained to EOF. Tailing a file (not a pipe) is what lets a
        // re-attach after restart reconstruct live state from the same code path.
        let done = Arc::new(AtomicBool::new(false));
        let tail = tokio::spawn(tail_log(
            log_path.clone(),
            0,
            progress.clone(),
            speed.clone(),
            id,
            ad_tx,
            done.clone(),
        ));

        // Stall watchdog: a wedged tool (hung live capture after stream end, a
        // stuck VOD download) never exits — kill it once output stops so this
        // wait returns and the normal finalize runs.
        let stall = self.spawn_stall_watchdog(
            detach.kind,
            id,
            pid,
            log_path.clone(),
            plan.capture_path.clone(),
            done.clone(),
        );

        let status = child.wait().await;
        // The process exited *within this session* (we weren't dropped/detached):
        // let the tailer drain the log to EOF, then the ad processor finish, so
        // every line and ad break is recorded before the caller touches the file.
        done.store(true, Ordering::SeqCst);
        stall.abort();
        let _ = tail.await;
        if let Some(t) = ad_task {
            let _ = t.await;
        }
        // Closing the job here terminates any stragglers (e.g. yt-dlp's ffmpeg).
        if let Some(j) = &job {
            j.kill();
        }
        drop(job);

        // Finalized in-session, so drop the registry row (nothing to re-attach).
        if detach.ref_id != 0 {
            if let Err(e) = self.store.clear_detached(detach.kind, detach.ref_id) {
                warn!("clear detached process failed: {e:#}");
            }
        }

        let exit_code = status.ok().and_then(|s| s.code()).map(|c| c as i64);
        // The failure-reason excerpt is the tail of the on-disk log (replaces the
        // old in-memory ring, and works identically after a re-attach).
        let log = read_log_tail(&log_path, RING_MAX_LINES).await;
        ProcessOutcome { exit_code, log }
    }
}

struct ProcessOutcome {
    exit_code: Option<i64>,
    log: String,
}

/// What the startup reconcile decided to do with one persisted detached download.
pub enum ReAction {
    /// Process still running — wait on it, tail its log, then finalize.
    Adopt,
    /// Process already exited (finished while the app was down) — finalize now.
    Finalize,
    /// Process gone but the capture is SABR-resumable — re-run from its `.state`.
    Resume,
}

/// One detached download to drive on the runtime after [`Supervisor::reconcile_detached`].
pub struct ReattachItem {
    row: DetachedRow,
    /// The active map this download occupies while in flight (so the UI shows it
    /// and the scheduler won't double-start its monitor).
    active: ActiveSet,
    action: ReAction,
}

impl ReattachItem {
    /// The OS PID of the detached tool this item drives.
    pub fn pid(&self) -> u32 {
        self.row.pid
    }
    /// Whether this item re-attaches to a *still-running* process (vs. finalizing
    /// one that already exited, or resuming a SABR capture).
    pub fn is_adopt(&self) -> bool {
        matches!(self.action, ReAction::Adopt)
    }
}

/// Open `path` for the child's combined stdout+stderr: truncate any prior log,
/// then return two **append** handles onto the one file (one for stdout, one for
/// stderr). Append mode lets both streams interleave into a single growing file
/// without clobbering each other, and the child keeps writing after we detach.
fn open_log_pair(path: &Path) -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::fs::OpenOptions;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::File::create(path)?; // truncate any leftover
    let out = OpenOptions::new().create(true).append(true).open(path)?;
    let err = out.try_clone()?;
    Ok((out, err))
}

/// Tail a tool's log file, parsing progress/speed and streamlink ad-break lines
/// exactly as the old pipe readers did. Starts at `start_offset` (0 for a fresh
/// spawn; end-of-file for a re-attach so already-persisted breaks aren't redone),
/// and returns once `done` is set and the file is drained to EOF.
async fn tail_log(
    path: PathBuf,
    start_offset: u64,
    progress: Option<VideoProgress>,
    speed: Option<VideoSpeed>,
    id: i64,
    ad_tx: Option<mpsc::UnboundedSender<(i64, i64)>>,
    done: Arc<AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    // The log is created before the spawn, but tolerate a brief absence.
    let mut file = loop {
        match tokio::fs::File::open(&path).await {
            Ok(f) => break f,
            Err(_) => {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    if start_offset > 0 {
        let _ = file.seek(std::io::SeekFrom::Start(start_offset)).await;
    }
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let emit = |line: &str| {
        let (f, s) = parse_progress_fields(line);
        if let (Some(f), Some(m)) = (f, progress.as_ref()) {
            m.lock().unwrap().insert(id, f);
        }
        if let (Some(s), Some(m)) = (s, speed.as_ref()) {
            m.lock().unwrap().insert(id, s);
        }
        if let Some(tx) = &ad_tx {
            if let Some(dur) = parse_ad_break_secs(line) {
                let _ = tx.send((now_unix(), dur));
            }
        }
        tracing::trace!(target: "streamarchiver::recproc", "{line}");
    };
    loop {
        let n = file.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            if done.load(Ordering::SeqCst) {
                // Process exited and we've drained to EOF: flush any trailing
                // partial line, then stop (dropping ad_tx ends the ad processor).
                let tail = String::from_utf8_lossy(&pending);
                let tail = tail.trim_end_matches('\r');
                if !tail.trim().is_empty() {
                    emit(tail);
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(idx) = pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = pending.drain(..=idx).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len().saturating_sub(1)]);
            emit(line.trim_end_matches('\r'));
        }
    }
}

/// Build the minimal [`Recording`] the SABR resume path needs from a detached
/// registry row (it reads `id`, `output_path`, and `take_group`).
fn recording_from_detached(row: &DetachedRow) -> Recording {
    Recording {
        id: row.ref_id,
        monitor_id: row.monitor_id.unwrap_or(0),
        started_at: row.started_at,
        ended_at: None,
        status: "recording".into(),
        bytes: 0,
        exit_code: None,
        output_path: row.final_path.clone(),
        went_live_at: row.went_live_at,
        went_live_approx: false,
        lost_secs: None,
        stream_id: row.stream_id.clone(),
        take_group: row.take_group.clone(),
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
    }
}

/// The byte offset of the start of the final (possibly partial) line of `path`,
/// so a re-attach tail begins on a line boundary and never splits a marker. If the
/// file ends exactly on a newline (nothing partial) or on any error, returns its
/// length. Only the trailing line is re-read, which is safe: ad inserts are
/// idempotent and progress parsing is overwrite-only.
async fn line_aligned_tail_offset(path: &Path) -> u64 {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let len = match tokio::fs::metadata(path).await {
        Ok(m) => m.len(),
        Err(_) => return 0,
    };
    if len == 0 {
        return 0;
    }
    let window = len.min(64 * 1024);
    let start = len - window;
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return len;
    };
    if f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return len;
    }
    let mut buf = vec![0u8; window as usize];
    if f.read_exact(&mut buf).await.is_err() {
        return len;
    }
    if buf.last() == Some(&b'\n') {
        return len; // ends on a newline — no partial trailing line
    }
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(idx) => start + idx as u64 + 1, // first byte after the last newline
        None => start,                       // no newline in window
    }
}

/// Read the last `max_lines` lines of a tool log — the failure-reason excerpt
/// stored on the finished recording/video row.
async fn read_log_tail(path: &Path, max_lines: usize) -> String {
    let data = tokio::fs::read(path).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&data);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// Spawn the ad-break processor for an [`AdSink`], returning the channel the log
/// tailer pushes `(detected_at, duration)` pairs onto plus the task handle. For
/// each break, the cut lands at the content captured so far — ad segments are
/// filtered out, so the capture's media duration is that position (correct for
/// live-edge and capture-from-start/DVR). Falls back to wall clock minus
/// already-skipped ad time when ffprobe can't read the still-growing file yet.
/// Shared by the in-session spawn and the re-attach path.
fn spawn_ad_processor(
    sink: AdSink,
) -> (
    mpsc::UnboundedSender<(i64, i64)>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<(i64, i64)>();
    let jh = tokio::spawn(async move {
        let mut prior_ad_secs: i64 = 0;
        let mut last_at: i64 = 0;
        while let Some((detected_at, dur)) = rx.recv().await {
            let mut probed = media_duration_secs(&sink.capture_path).await;
            if probed.is_none() {
                tokio::time::sleep(Duration::from_millis(500)).await;
                probed = media_duration_secs(&sink.capture_path).await;
            }
            let at = match probed {
                Some(d) => d,
                None => {
                    let anchor = match (sink.from_start, sink.went_live_at) {
                        (true, Some(wl)) => wl,
                        _ => sink.started_at,
                    };
                    (detected_at - anchor - prior_ad_secs).max(0)
                }
            };
            // Cut positions only move forward; guard against a probe that
            // momentarily reports a smaller duration.
            let at = at.max(last_at);
            last_at = at;
            prior_ad_secs += dur;
            // Mark the ad window so the UI can tint the row while it plays.
            sink.ad_active
                .lock()
                .unwrap()
                .insert(sink.monitor_id, detected_at + dur);
            match sink.store.insert_ad_break(sink.recording_id, at, dur) {
                Ok(_) => {
                    info!(
                        monitor_id = sink.monitor_id,
                        rec_id = sink.recording_id,
                        at,
                        secs = dur,
                        "ad break detected"
                    );
                    // Wake the UI so an expanded history tree refreshes its
                    // Ads / Ad time columns.
                    let _ = sink.events.send(AppEvent::MonitorState {
                        monitor_id: sink.monitor_id,
                        state: "recording".into(),
                    });
                }
                Err(e) => warn!("insert ad_break failed: {e:#}"),
            }
        }
    });
    (tx, jh)
}

/// Parse a yt-dlp progress line
/// (`--progress-template "download:DLPCT=%(progress._percent_str)s;;SPEED=%(progress.speed)s"`)
/// into `(percent fraction 0.0..=1.0, speed bytes/sec)`. Either may be `None`
/// (non-progress line or an unknown/`NA` value).
fn parse_progress_fields(line: &str) -> (Option<f32>, Option<f64>) {
    let mut pct = None;
    let mut speed = None;
    for part in line.trim().split(";;") {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("DLPCT=") {
            let s = rest.trim().trim_end_matches('%').trim();
            if let Ok(v) = s.parse::<f32>() {
                pct = Some((v / 100.0).clamp(0.0, 1.0));
            }
        } else if let Some(rest) = part.strip_prefix("SPEED=") {
            // yt-dlp's raw `speed` is bytes/sec (or "NA" when unknown).
            if let Ok(v) = rest.trim().parse::<f64>() {
                if v.is_finite() && v > 0.0 {
                    speed = Some(v);
                }
            }
        }
    }
    (pct, speed)
}

/// Percent-only convenience wrapper around [`parse_progress_fields`].
#[cfg(test)]
fn parse_progress(line: &str) -> Option<f32> {
    parse_progress_fields(line).0
}

/// Parse streamlink's Twitch `Detected advertisement break of N second(s)` log
/// line into the break duration in seconds. Returns `None` for any other line.
/// Tolerant of streamlink's `[plugins.twitch][info]` line prefix.
fn parse_ad_break_secs(line: &str) -> Option<i64> {
    const MARKER: &str = "advertisement break of ";
    let idx = line.find(MARKER)?;
    let rest = &line[idx + MARKER.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

/// Resolve a video/stream's real title, channel/uploader, and id via yt-dlp (no
/// download). Works for YouTube, Twitch VODs, Kick, and many sites; returns
/// `(title, channel, id)` with empty strings on any failure (caller falls back).
/// Each is truncated to keep filenames sane.
async fn resolve_meta(video: &Video, auth: &AuthSource) -> (String, String, String) {
    // Three `--print` templates -> three output lines, in order.
    let mut args: Vec<String> = vec![
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--skip-download".into(),
        "--print".into(),
        "%(title)s".into(),
        "--print".into(),
        "%(channel,uploader)s".into(),
        "--print".into(),
        "%(id)s".into(),
    ];
    match auth {
        AuthSource::CookiesBrowser(b) => {
            args.push("--cookies-from-browser".into());
            args.push(b.clone());
        }
        AuthSource::CookiesFile(p) => {
            args.push("--cookies".into());
            args.push(p.clone());
        }
        _ => {}
    }
    args.push(video.url.clone());

    let mut cmd = Command::new("yt-dlp");
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let out = match cmd.output().await {
        Ok(o) if o.status.success() => o,
        _ => return (String::new(), String::new(), String::new()),
    };
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut lines = raw.lines();
    let clean = |s: &str| -> String {
        let s = s.trim();
        if s.is_empty() || s == "NA" {
            String::new()
        } else {
            s.chars().take(120).collect()
        }
    };
    let title = clean(lines.next().unwrap_or(""));
    let channel = clean(lines.next().unwrap_or(""));
    let id = clean(lines.next().unwrap_or(""));
    (title, channel, id)
}

/// Probe available formats/qualities for a URL with the given tool, returning
/// combined stdout+stderr for the Videos-tab "List formats" window. yt-dlp gets
/// `--list-formats`, streamlink lists its stream qualities, ffmpeg uses ffprobe.
pub async fn probe_formats(tool: Tool, url: &str, auth: &AuthSource) -> Result<String, String> {
    let (program, mut args): (&str, Vec<String>) = match tool {
        Tool::YtDlp => (
            "yt-dlp",
            vec!["--list-formats".into(), "--no-playlist".into()],
        ),
        Tool::Streamlink => ("streamlink", Vec::new()),
        Tool::Ffmpeg => ("ffprobe", vec!["-hide_banner".into()]),
    };
    if let Tool::YtDlp = tool {
        match auth {
            AuthSource::CookiesBrowser(b) => {
                args.push("--cookies-from-browser".into());
                args.push(b.clone());
            }
            AuthSource::CookiesFile(p) => {
                args.push("--cookies".into());
                args.push(p.clone());
            }
            _ => {}
        }
    }
    args.push(url.to_string());

    let mut cmd = Command::new(program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let out = match tokio::time::timeout(Duration::from_secs(45), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("failed to run {program}: {e}")),
        Err(_) => return Err(format!("{program} timed out")),
    };
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !s.trim().is_empty() {
            s.push('\n');
        }
        s.push_str(&err);
    }
    let s = s.trim().to_string();
    if s.is_empty() {
        Ok(format!("(no output; exit {:?})", out.status.code()))
    } else {
        Ok(s)
    }
}

/// When `progress_tx` is `Some((tx, task_id))`, ffmpeg progress is streamed as
/// `AppEvent::BackgroundTaskProgress` events on `tx`.
pub async fn remux_ts_to_mkv(
    src: &Path,
    dst: &Path,
    progress_tx: Option<(EventTx, u64)>,
    opts: &crate::models::RemuxOpts,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Get total duration so we can compute a percentage from ffmpeg's output.
    let total_us: Option<i64> = if progress_tx.is_some() {
        media_duration_secs(src).await.map(|s| s * 1_000_000)
    } else {
        None
    };

    // Look for a thumbnail sidecar sitting next to the source TS (either our
    // HTTP fetch → `{stem}.thumbnail.jpg`, or yt-dlp's `--write-thumbnail` →
    // `{stem}.webp`/`.jpg`/`.png`). If found and embedding is enabled, attach
    // it as MKV cover art so media players (mpv, VLC, …) pick it up automatically.
    let thumbnail = if opts.embed_thumbnail { find_thumbnail_for(src) } else { None };

    // Collect subtitle sidecars if embedding is enabled.
    let subs: Vec<PathBuf> = if opts.embed_subs {
        collect_subtitle_sidecars(src)
    } else {
        Vec::new()
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i")
        .arg(src);

    // Add subtitle sidecar inputs before -map flags so we can reference them.
    for sub in &subs {
        cmd.arg("-i").arg(sub);
    }

    cmd
        // Keep EVERY video/audio/subtitle stream, not just ffmpeg's default
        // one-per-type — otherwise the extra audio tracks captured via
        // `--hls-audio-select=*` would be dropped here. Map by type (each `?`
        // optional) rather than `-map 0` so TS data streams (e.g. timed-ID3),
        // which MKV can't hold, don't fail the remux.
        .arg("-map").arg("0:v?")
        .arg("-map").arg("0:a?")
        .arg("-map").arg("0:s?");

    // Map each subtitle sidecar input stream.
    for i in 1..=subs.len() {
        cmd.arg("-map").arg(format!("{i}:s?"));
    }

    cmd.arg("-c").arg("copy");

    // Title metadata tag.
    if opts.embed_title && !opts.title_template.is_empty() {
        cmd.arg("-metadata").arg(format!("title={}", opts.title_template));
    }

    if let Some(ref thumb) = thumbnail {
        let ext = thumb
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jpg")
            .to_ascii_lowercase();
        let mime = match ext.as_str() {
            "png"  => "image/png",
            "webp" => "image/webp",
            _      => "image/jpeg",
        };
        let cover_name = format!("cover.{ext}");
        cmd.arg("-attach").arg(thumb)
            .arg("-metadata:s:t").arg(format!("mimetype={mime}"))
            .arg("-metadata:s:t").arg(format!("filename={cover_name}"));
    }

    cmd
        // Write structured key=value progress lines to stdout.
        .arg("-progress").arg("pipe:1")
        // Suppress the default per-frame stats line that goes to stderr.
        .arg("-nostats")
        .arg(dst)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Collect stderr in background so we can report it on failure.
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut lines: Vec<String> = Vec::new();
        while let Ok(Some(line)) = reader.next_line().await {
            lines.push(line);
        }
        lines
    });

    // Read stdout for progress events.  ffmpeg's -progress writes one key=value
    // per line; a block ends with `progress=continue` (or `progress=end`).
    // `out_time_ms` is microseconds despite the name (historical ffmpeg quirk).
    {
        let mut reader = BufReader::new(stdout).lines();
        // Per-block accumulators.
        let mut blk_frame = String::new();
        let mut blk_fps   = String::new();
        let mut blk_speed = String::new();
        let mut blk_pos   = String::new(); // out_time  HH:MM:SS.μs
        let mut blk_us: Option<i64> = None; // out_time_ms in microseconds

        while let Ok(Some(line)) = reader.next_line().await {
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                match k {
                    "frame"       => blk_frame = v.to_string(),
                    "fps"         => blk_fps   = v.to_string(),
                    "speed"       => blk_speed = v.to_string(),
                    "out_time"    => blk_pos   = v.to_string(),
                    "out_time_ms" => blk_us    = v.parse::<i64>().ok(),
                    "progress"    => {
                        // End of block — fire one event.
                        if let Some((ref tx, task_id)) = progress_tx {
                            let progress = blk_us.and_then(|us| {
                                total_us.filter(|&t| t > 0).map(|t| {
                                    (us as f64 / t as f64).clamp(0.0, 1.0) as f32
                                })
                            });
                            // Trim subsecond noise from out_time (keep HH:MM:SS).
                            let pos_short = blk_pos.split('.').next().unwrap_or(&blk_pos);
                            let info = format!(
                                "frame={} fps={} speed={} pos={}",
                                blk_frame, blk_fps, blk_speed, pos_short,
                            );
                            let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                id: task_id,
                                progress,
                                info,
                            });
                        }
                        // Reset for next block.
                        blk_frame.clear(); blk_fps.clear();
                        blk_speed.clear(); blk_pos.clear(); blk_us = None;
                    }
                    _ => {}
                }
            }
        }
    }

    let status = child.wait().await?;
    let stderr_lines = stderr_task.await.unwrap_or_default();

    if status.success() {
        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);
        // Grab the last few non-empty lines of stderr — ffmpeg prints the
        // relevant error at the end (e.g. "Invalid data found when processing input").
        let tail: String = stderr_lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(3)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        if tail.is_empty() {
            anyhow::bail!("ffmpeg remux failed (exit {})", code)
        } else {
            anyhow::bail!("ffmpeg remux failed (exit {}): {}", code, tail)
        }
    }
}

async fn file_len(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0)
}

/// How long a download's output (log + capture files) may stay completely
/// unchanged before the stall watchdog kills its process tree. Generous on
/// purpose: a live capture writes continuously, reconnect attempts produce log
/// lines, and post-download merges grow stem-sibling temp files — 15 minutes of
/// total silence means a wedged tool, not a slow one.
const STALL_KILL_SECS: u64 = 15 * 60;

/// Handle-based file size (0 if missing/unopenable). `fs::metadata` reads the
/// directory entry, which NTFS updates lazily while a writer holds the file
/// open — opening the file queries the handle, which is always current.
async fn open_len(p: &Path) -> u64 {
    match tokio::fs::File::open(p).await {
        Ok(f) => f.metadata().await.map(|m| m.len()).unwrap_or(0),
        Err(_) => 0,
    }
}

/// Activity signature for the stall watchdog: the log's size plus the sizes
/// (and names, to catch file-set changes) of every file in the capture dir
/// sharing the capture stem — SABR per-format `.part` files, merge temps, the
/// bare capture itself. Any write anywhere changes the sum.
async fn stall_signature(log_path: &Path, capture_path: &Path) -> u64 {
    let mut sig = open_len(log_path).await;
    if let (Some(dir), Some(stem)) = (capture_path.parent(), capture_path.file_stem()) {
        let stem = stem.to_string_lossy().into_owned();
        if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
            while let Ok(Some(e)) = rd.next_entry().await {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(&stem) {
                    sig = sig
                        .wrapping_add(open_len(&e.path()).await)
                        .wrapping_add(name.len() as u64);
                }
            }
        }
    }
    sig
}

/// Media duration of `path` in whole seconds via `ffprobe`, or `None` if it
/// can't be determined (file missing/unreadable, ffprobe absent, or a container
/// — e.g. a still-growing `.ts` — that doesn't report a duration).
async fn media_duration_secs(path: &Path) -> Option<i64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error", "-hide_banner", "-show_entries", "format=duration",
              "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if secs.is_finite() && secs >= 0.0 {
        Some(secs as i64)
    } else {
        None
    }
}

/// True if the MKV at `path` already has at least one attachment stream (cover art).
/// Runs `ffprobe` synchronously — only call from a blocking context or `spawn_blocking`.
pub fn mkv_has_thumbnail(path: &Path) -> bool {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-select_streams", "t",
               "-show_entries", "stream=index", "-of", "csv=p=0"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// Embed `thumb` as a cover-art attachment into an existing MKV file in-place
/// (remux to a temp file, then atomically replace the original).
pub async fn embed_thumbnail_into_mkv(mkv: &Path, thumb: &Path) -> anyhow::Result<()> {
    let tmp = mkv.with_extension("tmp.mkv");
    let ext = thumb.extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png"  => "image/png",
        "webp" => "image/webp",
        _      => "image/jpeg",
    };
    let cover_name = format!("cover.{ext}");
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i").arg(mkv)
        .arg("-i").arg(thumb)
        .arg("-map").arg("0")
        .arg("-c").arg("copy")
        .arg("-attach").arg(thumb)
        .arg("-metadata:s:t").arg(format!("mimetype={mime}"))
        .arg("-metadata:s:t").arg(format!("filename={cover_name}"))
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = cmd.output().await?;
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        let tail = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ffmpeg embed-thumbnail failed: {}", tail.trim().lines().last().unwrap_or(""));
    }
    tokio::fs::rename(&tmp, mkv).await?;
    Ok(())
}

/// Reorganize the files for one recording according to `cfg`. When `reverse` is
/// true, moves files from subdirs back to the base output dir.
/// Returns the new `output_path` for the video file, or the original if unchanged.
pub async fn reorganize_recording_files(
    rec_id: i64,
    store: &std::sync::Arc<crate::store::Store>,
    cfg: &crate::models::SubdirConfig,
    reverse: bool,
) -> anyhow::Result<Option<String>> {
    let (mid, output_path) = match store.get_recording_paths(rec_id)? {
        Some(v) => v,
        None => return Ok(None),
    };
    if output_path.is_empty() {
        return Ok(None);
    }
    let current = PathBuf::from(&output_path);

    // Fetch base_dir unconditionally — we need it even when the video is gone,
    // to sort any companion files (chat logs, thumbnails) stranded in the root.
    let (output_dir, _) = match store.get_monitor_output_dir(mid)? {
        Some(v) => v,
        None => return Ok(None),
    };
    let base_dir = PathBuf::from(&output_dir);

    if !current.exists() {
        // Video file is gone (failed recording, external move, etc.).
        // Still try to sort companion files in the directory the video was supposed to land in.
        if cfg.enabled && !reverse {
            if let Some(s) = current.file_stem().and_then(|s| s.to_str()) {
                let from_dir = current.parent().map(PathBuf::from).unwrap_or_else(|| base_dir.clone());
                move_companions_to_subdirs(&from_dir, &base_dir, s, cfg).await;
            }
        }
        return Ok(None);
    }

    let ext = current.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let is_mkv = ext == "mkv" || ext == "mp4" || ext == "ts";

    // Extract stem early so both the "already in place" path and the move path can use it.
    let stem = match current.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(None),
    };

    let target_dir = if reverse {
        base_dir.clone()
    } else if is_mkv && cfg.enabled {
        base_dir.join(&cfg.videos)
    } else {
        return Ok(None); // nothing to move
    };

    if target_dir == current.parent().unwrap_or(&base_dir) {
        // Video is already in the right place. Still scan base_dir for any companion
        // files that were left behind by a previous run (e.g. because the extension
        // wasn't handled at the time).
        if cfg.enabled && !reverse {
            move_companions_to_subdirs(&base_dir, &base_dir, &stem, cfg).await;
        }
        return Ok(None);
    }

    if let Err(e) = tokio::fs::create_dir_all(&target_dir).await {
        anyhow::bail!("create_dir_all {:?}: {e:#}", target_dir);
    }
    let file_name = match current.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return Ok(None),
    };
    let new_video_path = target_dir.join(&file_name);
    tokio::fs::rename(&current, &new_video_path).await?;

    // Move companion files (subs, thumbnail, chat) to their own dirs.
    if cfg.enabled && !reverse {
        let from_dir = current.parent().unwrap_or(&base_dir);
        move_companions_to_subdirs(from_dir, &base_dir, &stem, cfg).await;
    } else if reverse {
        // Collapse all companions from sub-dirs back to base_dir.
        let dirs = [&cfg.videos, &cfg.subs, &cfg.chat, &cfg.thumbs, &cfg.logs];
        for sub in &dirs {
            let sub_dir = base_dir.join(sub);
            move_companions(&sub_dir, &base_dir, &stem).await;
            // Try to remove the empty sub-dir (best effort; only our dirs).
            let _ = tokio::fs::remove_dir(&sub_dir).await;
        }
    } else {
        // Normal companion move: keep everything together with the video.
        if let Some(from_dir) = current.parent() {
            move_companions(from_dir, &target_dir, &stem).await;
        }
    }

    let new_path_str = new_video_path.to_string_lossy().into_owned();
    store.update_recording_output_path(rec_id, &new_path_str)?;
    Ok(Some(new_path_str))
}

/// Move companion files (subs, thumbnail, chat log) to the appropriate sub-dirs
/// based on `cfg`. Best-effort — skips files that can't be moved.
async fn move_companions_to_subdirs(from_dir: &Path, base_dir: &Path, stem: &str, cfg: &crate::models::SubdirConfig) {
    let prefix = format!("{stem}.");
    let mut rd = match tokio::fs::read_dir(from_dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        let rest = &name[prefix.len()..];
        let target_sub = if rest.ends_with("chat.jsonl") || rest.ends_with("live_chat.json") || rest.ends_with("chat.log") {
            Some(&cfg.chat)
        } else if rest.ends_with("thumbnail.jpg") || rest.ends_with("thumbnail.webp") {
            Some(&cfg.thumbs)
        } else {
            let lower = rest.to_ascii_lowercase();
            let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or(&lower);
            if SUBTITLE_EXTS.contains(&ext) {
                Some(&cfg.subs)
            } else if THUMBNAIL_EXTS.contains(&ext) {
                Some(&cfg.thumbs)
            } else {
                None
            }
        };
        if let Some(sub) = target_sub {
            let target_dir = base_dir.join(sub);
            let _ = tokio::fs::create_dir_all(&target_dir).await;
            let dst = target_dir.join(&name);
            let _ = tokio::fs::rename(entry.path(), dst).await;
        }
    }
}

/// Sweep every file directly in `dir` (non-recursive) and move companion files
/// (chat logs, thumbnails, subtitles) into their configured subdirectories.
/// This catches files that aren't linked to any recording in the database
/// (e.g. chat logs from recordings that ended with no output_path).
/// Video/part files are ignored — only known companion extensions are moved.
pub(crate) async fn sweep_companion_files(dir: &Path, cfg: &crate::models::SubdirConfig) {
    if !cfg.enabled {
        return;
    }
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(true) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        let target_sub = if lower.ends_with(".chat.log")
            || lower.ends_with(".chat.jsonl")
            || lower.ends_with(".live_chat.json")
        {
            Some(&cfg.chat)
        } else if lower.ends_with(".thumbnail.jpg") || lower.ends_with(".thumbnail.webp") {
            Some(&cfg.thumbs)
        } else {
            let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or("");
            if SUBTITLE_EXTS.contains(&ext) {
                Some(&cfg.subs)
            } else if THUMBNAIL_EXTS.contains(&ext) {
                Some(&cfg.thumbs)
            } else {
                None
            }
        };
        if let Some(sub) = target_sub {
            let target_dir = dir.join(sub);
            let _ = tokio::fs::create_dir_all(&target_dir).await;
            let dst = target_dir.join(&name);
            let _ = tokio::fs::rename(entry.path(), dst).await;
        }
    }
}

/// Rename a recording's output file (and its companions) to a new stem.
/// Updates `recording.output_path` in the database.
/// Returns the new path string, or None if the recording has no output path.
pub async fn rename_recording_files(
    rec_id: i64,
    store: &std::sync::Arc<crate::store::Store>,
    new_stem: &str,
) -> anyhow::Result<Option<String>> {
    let (_, output_path) = match store.get_recording_paths(rec_id)? {
        Some(v) => v,
        None => return Ok(None),
    };
    if output_path.is_empty() {
        return Ok(None);
    }
    let current = PathBuf::from(&output_path);
    if !current.exists() {
        anyhow::bail!("output file not found: {}", current.display());
    }
    let dir = match current.parent() {
        Some(d) => d.to_path_buf(),
        None => return Ok(None),
    };
    let ext = current.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
    let old_stem = match current.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(None),
    };

    // Sanitize the new stem (same as capture filenames).
    let new_stem_clean = sanitize_filename(new_stem);
    if new_stem_clean.is_empty() || new_stem_clean == old_stem {
        return Ok(None);
    }

    let new_file = dir.join(format!("{new_stem_clean}.{ext}"));
    tokio::fs::rename(&current, &new_file).await?;

    // Rename companion files.
    let prefix_old = format!("{old_stem}.");
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(_) => {
            let new_path = new_file.to_string_lossy().into_owned();
            store.update_recording_output_path(rec_id, &new_path)?;
            return Ok(Some(new_path));
        }
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == new_file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() {
            continue;
        }
        if let Some(rest) = name.strip_prefix(&prefix_old) {
            if is_companion_suffix(rest) {
                let new_name = format!("{new_stem_clean}.{rest}");
                let dst = dir.join(&new_name);
                let _ = tokio::fs::rename(entry.path(), dst).await;
            }
        }
    }

    let new_path = new_file.to_string_lossy().into_owned();
    store.update_recording_output_path(rec_id, &new_path)?;
    Ok(Some(new_path))
}

/// Actual media properties of a capture, for the filename `{resolution}`/
/// `{height}`/`{width}`/`{fps}`/`{vcodec}` variables. Empty fields render empty.
#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub resolution: String, // "1920x1080"
    pub width: String,
    pub height: String,
    pub fps: String,    // rounded whole number, e.g. "60"
    pub vcodec: String, // e.g. "h264"
    pub acodec: String, // e.g. "aac", "opus"
}

/// True if `template` uses any media-info variable (so we only probe when needed).
fn template_wants_media(template: &str) -> bool {
    ["{resolution}", "{height}", "{width}", "{fps}", "{vcodec}", "{acodec}"]
        .iter()
        .any(|k| template.contains(k))
}

/// True if `template` uses `{games}` (only known after the stream ends, so it
/// triggers a post-capture rename even when media probing is off).
fn template_wants_games(template: &str) -> bool {
    template.contains("{games}")
}

/// True if `template` uses `{title}` (may not be known at recording start for
/// all platforms, so it also triggers a post-capture rename to fill the real value).
fn template_wants_title(template: &str) -> bool {
    template.contains("{title}")
}

/// True if `template` uses `{went_live_date}` or `{went_live_time}` (only known
/// at the end of the recording, so it triggers a post-capture rename).
fn template_wants_went_live(template: &str) -> bool {
    template.contains("{went_live_date}") || template.contains("{went_live_time}")
}

/// The stream title for a finished recording: the first `title` change logged
/// by the meta-watcher (which is the baseline/initial value, i.e. the title at
/// recording start). Returns empty when no title was polled (generic URLs, etc.).
fn title_for_recording(store: &Store, rec_id: i64) -> String {
    store
        .meta_changes_for_recording(rec_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.kind == "title")
        .map(|c| c.new_value.clone())
        .next()
        .unwrap_or_default()
}

/// Max length of the expanded `{games}` value, to keep paths sane.
const GAMES_MAX_LEN: usize = 100;

/// NTFS enforces a 255 **UTF-16 code-unit** limit per path component — Windows
/// counts a surrogate pair (any character outside the Basic Multilingual
/// Plane, e.g. most emoji) as 2 units, not 1. Exceeding it fails file
/// operations with `ERROR_INVALID_NAME` ("The filename, directory name, or
/// volume label syntax is incorrect", os error 123) rather than a
/// length-specific error. See [`MAX_STEM_UTF16_LEN`].
const NTFS_MAX_COMPONENT_UTF16: usize = 255;

/// The shared filename stem is combined with several different suffixes: the
/// main recording (`.ts`/`.mkv`) and, later, companion sidecars
/// (`rename_companion_sidecars`/`move_companions`) — subtitle `.<lang>.vtt`,
/// thumbnail `.thumbnail.jpg`, chat `.chat.jsonl`/`.live_chat.json`.
/// `.live_chat.json` is the longest, so it sets the reservable budget.
const LONGEST_COMPANION_SUFFIX_LEN: usize = ".live_chat.json".len(); // 16, ASCII

/// Reserve for `unique_stem`'s collision suffix (` (2)`, ` (3)`, …).
const COLLISION_SUFFIX_RESERVE: usize = 10;

/// Hard cap on an expanded filename stem, in UTF-16 code units, so the stem
/// plus ANY companion suffix plus a collision suffix always stays under
/// [`NTFS_MAX_COMPONENT_UTF16`]. A long stream title combined with several
/// logged categories (`{games}`) is the realistic way to hit this: a
/// 2026-07-04 incident had a 251-unit stem that fit fine with `.mkv` (4 units)
/// but its `.chat.jsonl` sidecar (11 units) pushed it to 258 and the
/// post-capture rename silently failed, orphaning the sidecar under the old
/// `title-tba`/`games-tba` name.
const MAX_STEM_UTF16_LEN: usize =
    NTFS_MAX_COMPONENT_UTF16 - LONGEST_COMPANION_SUFFIX_LEN - COLLISION_SUFFIX_RESERVE; // 229

/// Build the `{games}` value from the categories played: distinct names in order
/// of first appearance (case-insensitive dedup), joined with `, ` and capped to
/// [`GAMES_MAX_LEN`] characters. Illegal filename characters are handled later by
/// `sanitize_filename` in `expand_template`.
fn format_games(categories: &[String]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for c in categories {
        let c = c.trim();
        if !c.is_empty() && !seen.iter().any(|s| s.eq_ignore_ascii_case(c)) {
            seen.push(c);
        }
    }
    let joined = seen.join(", ");
    if joined.chars().count() <= GAMES_MAX_LEN {
        joined
    } else {
        joined.chars().take(GAMES_MAX_LEN).collect()
    }
}

/// The `{games}` value for a finished recording: every distinct category logged
/// to `stream_meta_change` for it (empty when none was logged — e.g. a generic
/// URL, which has no metadata source).
fn games_for_recording(store: &Store, rec_id: i64) -> String {
    let cats: Vec<String> = store
        .meta_changes_for_recording(rec_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.kind == "category")
        .map(|c| c.new_value)
        .collect();
    format_games(&cats)
}

/// Round an ffprobe `r_frame_rate` ("60/1", "30000/1001") to a whole-number fps
/// string; empty on parse failure.
fn fmt_fps(rate: &str) -> String {
    let (n, d) = match rate.split_once('/') {
        Some((n, d)) => (n.trim().parse::<f64>().ok(), d.trim().parse::<f64>().ok()),
        None => (rate.trim().parse::<f64>().ok(), Some(1.0)),
    };
    match (n, d) {
        (Some(n), Some(d)) if d > 0.0 && n > 0.0 => (n / d).round().to_string(),
        _ => String::new(),
    }
}

/// ffprobe all streams of a file path or stream URL into [`MediaInfo`].
/// Extracts video codec, dimensions, fps and audio codec.
/// `None` if ffprobe fails / there's no readable video stream.
async fn probe_media(target: &str) -> Option<MediaInfo> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v", "error",
        "-show_entries", "stream=codec_type,codec_name,width,height,r_frame_rate",
        "-of", "json",
    ])
    .arg(target)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let streams = json["streams"].as_array()?;
    let mut info = MediaInfo::default();
    for stream in streams {
        let codec_type = stream["codec_type"].as_str().unwrap_or("");
        let codec_name = stream["codec_name"].as_str().unwrap_or("").to_string();
        match codec_type {
            "video" if info.vcodec.is_empty() => {
                info.vcodec = codec_name;
                if let (Some(w), Some(h)) = (
                    stream["width"].as_u64(),
                    stream["height"].as_u64(),
                ) {
                    info.width = w.to_string();
                    info.height = h.to_string();
                }
                if let Some(rate) = stream["r_frame_rate"].as_str() {
                    info.fps = fmt_fps(rate);
                }
            }
            "audio" if info.acodec.is_empty() => {
                info.acodec = codec_name;
            }
            _ => {}
        }
    }
    // Require real pixel dimensions (ffprobe can report "N/A" for odd inputs).
    if info.width.parse::<u32>().is_err() || info.height.parse::<u32>().is_err() {
        return None;
    }
    info.resolution = format!("{}x{}", info.width, info.height);
    Some(info)
}

/// Resolve a playable media URL for a stream (for pre-probe), then ffprobe it.
/// Best-effort; `None` on any failure (caller then leaves the media vars empty).
async fn preprobe_media(
    tool: Tool,
    url: &str,
    quality: &str,
    auth: &AuthSource,
) -> Option<MediaInfo> {
    let target = resolve_play_url(tool, url, quality, auth).await?;
    probe_media(&target).await
}

/// Resolve a stream's direct media URL via the capture tool (so ffprobe can read
/// it before recording). `None` on failure.
async fn resolve_play_url(
    tool: Tool,
    url: &str,
    quality: &str,
    auth: &AuthSource,
) -> Option<String> {
    let quality = resolved_quality(quality);
    let (program, args): (&str, Vec<String>) = match tool {
        // ffmpeg reads the source URL directly.
        Tool::Ffmpeg => return Some(url.to_string()),
        Tool::Streamlink => {
            let mut a = Vec::new();
            if Platform::detect(url) == Platform::Twitch {
                a.push("--twitch-supported-codecs=h264,h265,av1".to_string());
                if let AuthSource::Token(t) = auth {
                    a.push(format!("--twitch-api-header=Authorization=OAuth {t}"));
                }
            }
            a.push("--stream-url".into());
            a.push(url.to_string());
            a.push(quality);
            ("streamlink", a)
        }
        Tool::YtDlp => {
            let mut a = vec!["-g".to_string(), "--no-warnings".into(), "--no-playlist".into()];
            if quality != "best" {
                a.push("-f".into());
                a.push(quality);
            }
            match auth {
                AuthSource::CookiesBrowser(b) => {
                    a.push("--cookies-from-browser".into());
                    a.push(b.clone());
                }
                AuthSource::CookiesFile(p) => {
                    a.push("--cookies".into());
                    a.push(p.clone());
                }
                _ => {}
            }
            a.push(url.to_string());
            ("yt-dlp", a)
        }
    };
    let mut cmd = Command::new(program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // yt-dlp may print separate video+audio URLs; the first is the video stream.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Rename a finished capture to `new_stem` (keeping its extension), avoiding
/// collisions. Returns the resulting path (unchanged on no-op or failure).
async fn rename_for_media(final_path: PathBuf, new_stem: &str) -> PathBuf {
    let Some(dir) = final_path.parent().map(Path::to_path_buf) else {
        return final_path;
    };
    let ext = final_path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mkv".into());
    // Ignore the file we're renaming when checking collisions, so a "Both"-mode
    // capture whose pre-probe name already matches (incl. a build-time collision
    // suffix) resolves back to its own name and we no-op below.
    let unique = unique_stem(&dir, new_stem, &ext, Some(&final_path));
    let new_path = dir.join(format!("{unique}.{ext}"));
    if new_path == final_path {
        return final_path; // already correctly named (e.g. no media, or "Both")
    }
    let old_stem = final_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned());
    match rename_or_shorten(&final_path, &dir, &unique, &ext).await {
        Ok(actual) => {
            // Move subtitle / chat sidecars (e.g. `{stem}.en.vtt`,
            // `{stem}.chat.jsonl`, `{stem}.live_chat.json`) so they stay matched
            // to the renamed video instead of orphaning under the old stem.
            // Follow with the video's ACTUAL stem (rename_or_shorten may have
            // had to shorten it further) so sidecars never mismatch it.
            if let Some(old) = old_stem {
                let actual_stem = actual
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| unique.clone());
                rename_companion_sidecars(&dir, &old, &actual_stem).await;
            }
            actual
        }
        Err(e) => {
            warn!("media rename failed, keeping {}: {e:#}", final_path.display());
            final_path
        }
    }
}

/// Find a thumbnail sidecar that lives alongside `src` in the same directory.
///
/// Checked in priority order:
/// 1. `{stem}.thumbnail.jpg` — written by our HTTP fetch
/// 2. `{stem}.webp` / `{stem}.jpg` / `{stem}.png` — written by yt-dlp `--write-thumbnail`
///
/// Returns the first match found, or `None` if no thumbnail exists yet.
fn find_thumbnail_for(src: &Path) -> Option<PathBuf> {
    let dir = src.parent()?;
    let stem = src.file_stem()?.to_string_lossy();
    for suffix in &["thumbnail.jpg", "webp", "jpg", "png", "jpeg"] {
        let candidate = dir.join(format!("{stem}.{suffix}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Collect subtitle sidecar files adjacent to `src` (e.g. `{stem}.en.srt`,
/// `{stem}.vtt`, `{stem}.ass`). Used by `remux_ts_to_mkv` when `embed_subs` is on.
fn collect_subtitle_sidecars(src: &Path) -> Vec<PathBuf> {
    let dir = match src.parent() { Some(d) => d, None => return Vec::new() };
    let stem = match src.file_stem() { Some(s) => s.to_string_lossy().into_owned(), None => return Vec::new() };
    let prefix = format!("{stem}.");
    let mut subs = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) {
                continue;
            }
            let rest = &name[prefix.len()..];
            let lower = rest.to_ascii_lowercase();
            let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or(&lower);
            if SUBTITLE_EXTS.contains(&ext) {
                subs.push(entry.path());
            }
        }
    }
    subs.sort();
    subs
}

/// Subtitle-sidecar extensions, for companion-file moves when the video is
/// renamed (so external subs stay associated with their recording).
const SUBTITLE_EXTS: [&str; 6] = ["vtt", "srt", "ass", "ssa", "sub", "lrc"];

/// Thumbnail extensions, so a `{stem}.thumbnail.jpg` (our HTTP fetch) or a
/// `{stem}.webp`/`.jpg`/`.png` (yt-dlp `--write-thumbnail`) is promoted/renamed
/// alongside the recording instead of being orphaned.
const THUMBNAIL_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// True if `rest` (the part of a sibling filename after `{old_stem}.`) is a
/// recognized companion: a subtitle sidecar, a thumbnail, or a chat log
/// (`.chat.jsonl` from the Twitch logger, `.live_chat.json` from yt-dlp).
fn is_companion_suffix(rest: &str) -> bool {
    if rest.ends_with("chat.jsonl") || rest.ends_with("live_chat.json") {
        return true;
    }
    let lower = rest.to_ascii_lowercase();
    // `rest` may be a bare extension (yt-dlp's `{stem}.webp`) or a multi-part suffix
    // (`en.vtt`, `thumbnail.jpg`) — accept either: the final extension, else `rest`.
    let ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| lower.clone());
    SUBTITLE_EXTS.contains(&ext.as_str()) || THUMBNAIL_EXTS.contains(&ext.as_str())
}

/// Promote a finished capture from the `.cache\` working dir up to its final path
/// in the output dir: remux (TS→MKV) or move (already-final container), deleting the
/// cache source on success. A 0-byte/failed capture is left in `.cache\` (returns
/// the capture path so the caller can tell promotion didn't happen).
async fn promote_capture(plan: &DownloadPlan) -> PathBuf {
    // yt-dlp SABR dev-build sometimes appends a container extension to the
    // output path even when the template already specifies one — e.g. writing
    // `stem.mkv.mp4` instead of `stem.mkv` because the merged container is MP4.
    // Detect this and use the `.mp4` variant as the effective capture file.
    let effective = if !plan.remux_to_mkv && file_len(&plan.capture_path).await == 0 {
        let mut os = plan.capture_path.as_os_str().to_owned();
        os.push(".mp4");
        let mp4 = PathBuf::from(os);
        if file_len(&mp4).await > 0 { mp4 } else { plan.capture_path.clone() }
    } else {
        plan.capture_path.clone()
    };

    if file_len(&effective).await == 0 {
        return plan.capture_path.clone(); // failed: leave the partial for the sweep
    }
    if plan.remux_to_mkv {
        // ffmpeg writes the destination directly — there's no OS rename to
        // react to on a name-too-long failure, so shorten proactively before
        // the write instead of reactively after it (see path_with_safe_stem).
        let dest = path_with_safe_stem(&plan.final_path);
        match remux_ts_to_mkv(&effective, &dest, None, &Default::default()).await {
            Ok(()) => {
                let _ = tokio::fs::remove_file(&effective).await;
                dest
            }
            Err(e) => {
                warn!("remux failed, keeping {}: {e:#}", effective.display());
                effective
            }
        }
    } else {
        // Already the final container — move it up to the output dir. The
        // finished recording landing here matters more than a fully-
        // descriptive name: on a too-long name, rename_or_shorten falls back
        // to a shortened one rather than leaving a completed capture stuck
        // (and, after 24h, swept as stale) in the hidden `.cache\`.
        if let Some(parent) = plan.final_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let dir = plan.final_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = plan
            .final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = plan
            .final_path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        match rename_or_shorten(&effective, dir, &stem, &ext).await {
            Ok(actual) => actual,
            Err(e) => {
                warn!("promote move failed, keeping {}: {e:#}", effective.display());
                effective
            }
        }
    }
}

/// Move every recognized companion (`{stem}.*` matched by [`is_companion_suffix`] —
/// subtitles, thumbnail, in-process chat) from `from_dir` up to `to_dir`.
/// Best-effort; never clobbers an existing target.
async fn move_companions(from_dir: &Path, to_dir: &Path, stem: &str) {
    let prefix = format!("{stem}.");
    let mut rd = match tokio::fs::read_dir(from_dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        if !is_companion_suffix(rest) {
            continue;
        }
        let to = to_dir.join(&name);
        if to.exists() {
            continue;
        }
        match tokio::fs::rename(entry.path(), &to).await {
            Ok(()) => {}
            Err(e) if is_name_too_long(&e) => {
                if let Err(e) = rename_or_shorten(&entry.path(), to_dir, stem, rest).await {
                    warn!("companion promote failed for {name}: {e:#}");
                }
            }
            Err(e) => warn!("companion promote failed for {name}: {e:#}"),
        }
    }
}

/// After promotion, delete the recording's remaining working files (`{stem}.*`) from
/// `cache` (SABR `.sq0.part`/`.state`, leftover `.ts`, chat fragments), then remove
/// the cache dir if it is now empty. Best-effort.
async fn purge_cache(cache: &Path, stem: &str) {
    let prefix = format!("{stem}.");
    if let Ok(mut rd) = tokio::fs::read_dir(cache).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
    let _ = tokio::fs::remove_dir(cache).await; // only if empty
}

/// When the main recording file is renamed, move its companion sidecars
/// (`{old_stem}.<lang>.vtt` subtitles, `{old_stem}.chat.jsonl` /
/// `{old_stem}.live_chat.json` chat logs) to follow `new_stem`, so they don't
/// become orphaned next to a renamed video. Best-effort: per-file failures are
/// logged, not fatal; existing targets are never clobbered.
async fn rename_companion_sidecars(dir: &Path, old_stem: &str, new_stem: &str) {
    if old_stem == new_stem || old_stem.is_empty() {
        return;
    }
    let prefix = format!("{old_stem}.");
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        if !is_companion_suffix(rest) {
            continue;
        }
        let to = dir.join(format!("{new_stem}.{rest}"));
        if to.exists() {
            continue; // don't clobber an unrelated existing file
        }
        // Retry with exponential backoff: on Windows the chat downloader
        // (yt-dlp) may still have live_chat.json open when finalization runs,
        // producing os error 32 (ERROR_SHARING_VIOLATION). Give the process
        // time to flush and release the handle before giving up.
        let src = entry.path();
        let mut delay_ms = 500u64;
        let mut last_err: Option<std::io::Error> = None;
        let mut renamed = false;
        for attempt in 0u32..5 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms *= 2; // 500 → 1000 → 2000 → 4000 ms
            }
            match tokio::fs::rename(&src, &to).await {
                Ok(()) => { last_err = None; renamed = true; break; }
                Err(e) if e.raw_os_error() == Some(32)  // Windows: SHARING_VIOLATION
                       || e.raw_os_error() == Some(16)  // Unix: EBUSY
                => { last_err = Some(e); }
                Err(e) => { last_err = Some(e); break; } // non-retryable error
            }
        }
        if renamed {
            continue;
        }
        // The name itself may be the problem (most commonly NTFS's
        // 255-UTF-16-unit-per-component limit — see `MAX_STEM_UTF16_LEN`).
        // Following the video's rename matters more than a fully-descriptive
        // sidecar name, so shorten `new_stem` (never touching `rest`, which
        // identifies the sidecar's role) and retry, rather than leaving this
        // companion permanently orphaned under its old name.
        if last_err.as_ref().is_some_and(is_name_too_long) {
            match rename_or_shorten(&src, dir, new_stem, rest).await {
                Ok(_) => continue,
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(e) = last_err {
            warn!("companion sidecar rename failed for {}: {e:#}", name);
        }
    }
}

/// The configured quality selector with the `best` default applied.
pub(crate) fn resolved_quality(q: &str) -> String {
    if q.trim().is_empty() {
        "best".to_string()
    } else {
        q.trim().to_string()
    }
}

/// Read the global filename media-probe mode from settings.
fn media_info_mode(store: &Store) -> MediaInfoMode {
    MediaInfoMode::parse(
        &store
            .get_setting(K_FILENAME_MEDIA)
            .ok()
            .flatten()
            .unwrap_or_default(),
    )
}

/// Build a monitor recording's filename stem (no extension, no collision suffix).
/// Shared by [`build_plan`] and the post-capture rename so they agree.
#[allow(clippy::too_many_arguments)]
fn monitor_stem(
    m: &Monitor,
    ch_name: &str,
    started_at: i64,
    stream_id: Option<&str>,
    stream_title: &str,
    recording_count: i64,
    quality: &str,
    media: Option<&MediaInfo>,
    games: &str,
    tool: &str,
    mode: &str,
    platform: &str,
    went_live: i64,
) -> String {
    let take = (recording_count + 1).to_string();
    let mi = media.cloned().unwrap_or_default();
    // Use token-labelled placeholders for title/games not yet known at recording
    // start, filled in at the post-recording rename.
    let title_val = if stream_title.is_empty() && template_wants_title(&m.filename_template) {
        "title-tba"
    } else {
        stream_title
    };
    let games_val = if games.is_empty() && template_wants_games(&m.filename_template) {
        "games-tba"
    } else {
        games
    };
    expand_template(
        &m.filename_template,
        &TemplateVars {
            name: ch_name,
            title: title_val,
            video_id: stream_id.unwrap_or(""),
            quality,
            take: &take,
            games: games_val,
            resolution: &mi.resolution,
            height: &mi.height,
            width: &mi.width,
            fps: &mi.fps,
            vcodec: &mi.vcodec,
            acodec: &mi.acodec,
            tool,
            mode,
            platform,
            secs: started_at,
            went_live,
            ..Default::default()
        },
    )
}

/// The `{name}` value for an on-demand video: the user's Name, else the resolved
/// title, else a generic fallback.
fn video_name<'a>(v: &'a Video, resolved_title: &'a str) -> &'a str {
    let name_field = v.title.trim();
    if !name_field.is_empty() {
        name_field
    } else if !resolved_title.is_empty() {
        resolved_title
    } else {
        "video"
    }
}

/// Build an on-demand video's filename stem (no extension, no collision suffix).
/// Shared by [`build_video_plan`] and the post-capture rename.
#[allow(clippy::too_many_arguments)]
fn video_stem(
    v: &Video,
    started_at: i64,
    title: &str,
    channel: &str,
    video_id: &str,
    quality: &str,
    media: Option<&MediaInfo>,
    tool: &str,
    platform: &str,
) -> String {
    let resolved = title.trim();
    let mi = media.cloned().unwrap_or_default();
    expand_template(
        &v.filename_template,
        &TemplateVars {
            name: video_name(v, resolved),
            title: resolved,
            channel: channel.trim(),
            video_id: video_id.trim(),
            quality,
            resolution: &mi.resolution,
            height: &mi.height,
            width: &mi.width,
            fps: &mi.fps,
            vcodec: &mi.vcodec,
            acodec: &mi.acodec,
            tool,
            mode: "vod",
            platform,
            secs: started_at,
            ..Default::default()
        },
    )
}

/// Watch a from-start recording's growing capture and zero its "lost time" once
/// the captured media catches up to the live edge. Exits early when `done` is set
/// (recording ended) so finalize can compute the exact residual without a race.
#[allow(clippy::too_many_arguments)]
async fn catch_up_watcher(
    store: Arc<Store>,
    events: EventTx,
    monitor_id: i64,
    rec_id: i64,
    capture_path: PathBuf,
    went_live: i64,
    done: Arc<AtomicBool>,
) {
    loop {
        // Interruptible wait between probes (checks `done` every 250ms).
        for _ in 0..(CATCHUP_PROBE_INTERVAL_SECS * 4) {
            if done.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if done.load(Ordering::SeqCst) {
            return;
        }
        if let Some(captured) = media_duration_secs(&capture_path).await {
            let elapsed = now_unix() - went_live;
            if captured + CATCHUP_TOLERANCE_SECS >= elapsed {
                let _ = store.set_recording_lost_secs(rec_id, 0);
                info!(rec_id, "from-start capture caught up with live (lost time = 0)");
                // Wake the UI so an already-expanded history tree refreshes the
                // Lost-time column from the new value.
                let _ = events.send(AppEvent::MonitorState {
                    monitor_id,
                    state: "recording".into(),
                });
                return;
            }
        }
    }
}

/// How often to poll a stream's title/category for changes during a recording
/// (Twitch Helix, Kick v2 JSON, or the YouTube `/live` page). Changes are
/// infrequent and not time-critical for an archive log, so a coarse interval
/// keeps the cost low — one request per active recording (the YouTube path
/// fetches the full watch page; the others a small JSON response).
const META_POLL_INTERVAL_SECS: i64 = 60;

/// Poll a live channel's title + game/category for the duration of a recording,
/// logging each change to `stream_meta_change`. The metadata source is chosen by
/// `platform`: Twitch via Helix, Kick via its v2 channel JSON, YouTube by
/// scraping the `/live` page (its `game` is the broad content category, as
/// YouTube has no per-stream game field). The first observed value of each field
/// is logged as the baseline (empty `old_value`); later transitions record
/// `old -> new` (including a change to empty, e.g. a cleared category). Stops
/// when `done` (recording ended) or `shutdown` is set. No-ops gracefully when
/// the source is unavailable (creds unset / offline / blocked -> `None`).
#[allow(clippy::too_many_arguments)]
async fn meta_watcher(
    ctx: Arc<DetectContext>,
    store: Arc<Store>,
    events: EventTx,
    monitor_id: i64,
    rec_id: i64,
    started_at: i64,
    url: String,
    platform: Platform,
    done: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    let mut last_title: Option<String> = None;
    let mut last_game: Option<String> = None;
    loop {
        if done.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
            return;
        }
        let fetched = match platform {
            Platform::Twitch => ctx.twitch_stream_meta(&url).await,
            Platform::Kick => ctx.kick_stream_meta(&url).await,
            Platform::YouTube => ctx.youtube_stream_meta(&url).await,
            Platform::Generic => None,
        };
        if let Some(meta) = fetched {
            let at = (now_unix() - started_at).max(0);
            let mut changed = false;
            // Title: log the initial non-empty value, then every transition.
            if last_title.as_deref() != Some(meta.title.as_str()) {
                let baseline = last_title.is_none();
                let old = last_title.take().unwrap_or_default();
                last_title = Some(meta.title.clone());
                if !(baseline && meta.title.is_empty()) {
                    match store.insert_meta_change(rec_id, at, "title", &old, &meta.title) {
                        Ok(_) => changed = true,
                        Err(e) => warn!("insert title change failed: {e:#}"),
                    }
                }
            }
            // Category/game: same rule.
            if last_game.as_deref() != Some(meta.game.as_str()) {
                let baseline = last_game.is_none();
                let old = last_game.take().unwrap_or_default();
                last_game = Some(meta.game.clone());
                if !(baseline && meta.game.is_empty()) {
                    match store.insert_meta_change(rec_id, at, "category", &old, &meta.game) {
                        Ok(_) => changed = true,
                        Err(e) => warn!("insert category change failed: {e:#}"),
                    }
                }
            }
            if changed {
                // Wake the UI so the Changes column / popup refreshes live.
                let _ = events.send(AppEvent::MonitorState {
                    monitor_id,
                    state: "recording".into(),
                });
            }
        }

        // Interruptible wait until the next poll (checks the flags every 250ms).
        for _ in 0..(META_POLL_INTERVAL_SECS * 4) {
            if done.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Find the actual output when the predicted path is missing: the largest file
/// in `predicted`'s directory whose name shares its stem (e.g. yt-dlp wrote
/// `<stem>.webm` instead of the predicted `<stem>.mkv`).
async fn newest_with_stem(predicted: &Path) -> Option<PathBuf> {
    let dir = predicted.parent()?;
    let stem = predicted.file_stem()?.to_string_lossy().into_owned();
    let mut best: Option<(u64, PathBuf)> = None;
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&stem) {
            let len = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
            if len > 0 && best.as_ref().map(|(b, _)| len > *b).unwrap_or(true) {
                best = Some((len, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// True when a capture produced no footage because the stream wasn't actually
/// capturable — it had already ended, hadn't started yet, or exposed no live
/// video formats — rather than because of a real error. Detected from the tool's
/// stderr tail, so a concluded YouTube live (yt-dlp prints "Only images are
/// available …" once the live formats are gone) or an offline/ended Twitch
/// channel (streamlink: "No playable streams found") is classified as `ended`,
/// not `failed`.
fn stream_ended_or_unavailable(log: &str) -> bool {
    const PATTERNS: [&str; 5] = [
        // yt-dlp: a live that has ended/not-started has no video formats, only
        // thumbnail/storyboard images.
        "Only images are available",
        "This live event has ended",
        "This live event will begin",
        "Premieres in",
        // streamlink: channel offline or stream ended.
        "No playable streams found",
    ];
    PATTERNS.iter().any(|p| log.contains(p))
}

/// True when SABR live-from-start failed because YouTube's DVR window (~4h)
/// no longer covers the beginning of a long-running stream. The tool downloads
/// a few dozen initial segments then the server goes silent and raises
/// `StreamStallError: … not near live head`. Retrying from-start will always
/// hit the same wall; the next attempt should capture the live edge instead.
fn sabr_dvr_window_exceeded(log: &str) -> bool {
    log.contains("not near live head")
}

/// Minimal whitespace arg splitter (double-quoted segments kept together).
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Inputs to [`expand_template`]. Each field maps to a `{…}` variable; empty
/// fields render empty. `title`/`channel`/`video_id` are resolved metadata (empty
/// for live recordings or id-less methods); `resolution`/`height`/`width`/`fps`/
/// `vcodec` are actual media info (filled only when probing is enabled).
#[derive(Default)]
pub struct TemplateVars<'a> {
    pub name: &'a str,
    pub title: &'a str,
    pub channel: &'a str,
    pub video_id: &'a str,
    /// Configured quality selector (e.g. `1080p60`, `best`).
    pub quality: &'a str,
    pub resolution: &'a str,
    pub height: &'a str,
    pub width: &'a str,
    pub fps: &'a str,
    pub vcodec: &'a str,
    /// Audio codec, e.g. "aac", "opus" (empty when not probed or unknown).
    pub acodec: &'a str,
    /// Attempt number (per-monitor take count); empty for on-demand videos.
    pub take: &'a str,
    /// Distinct game/category names played during the recording, joined + length-
    /// capped. Only known after the stream ends, so it's filled at the post-rename
    /// (empty for the initial capture name and for on-demand videos).
    pub games: &'a str,
    /// Capture tool: "streamlink", "yt-dlp", "ffmpeg" (empty renders empty).
    pub tool: &'a str,
    /// Download mode: "live", "sabr", "dash", "hybrid", "hybrid-dash", "direct", "vod", "chat".
    pub mode: &'a str,
    /// Stream platform: "twitch", "youtube", "kick", "generic" (empty renders empty).
    pub platform: &'a str,
    /// Capture-start time (unix secs) for `{date}`/`{time}`/`{timestamp}`.
    pub secs: i64,
    /// When the broadcast went live (unix secs); 0 means unknown → {went_live_date}/{went_live_time} render empty.
    pub went_live: i64,
}

/// Preview a filename template with the given variable set. Extension not included.
/// Sanitizes and guarantees a non-empty result (falls back to `{name}_{date}_{time}`).
pub fn preview_filename(template: &str, vars: &TemplateVars<'_>) -> String {
    expand_template(template, vars)
}

/// Expand a filename template using our own (tool-agnostic) variables so the
/// output path is known in advance: `{name} {title} {channel} {video_id}
/// {quality} {resolution} {height} {width} {fps} {vcodec} {take} {games} {date}
/// {time} {timestamp}`.
fn expand_template(template: &str, v: &TemplateVars) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix_utc(v.secs);
    let date = format!("{y:04}{mo:02}{d:02}");
    let time = format!("{h:02}{mi:02}{s:02}");
    let (wl_date, wl_time) = if v.went_live > 0 {
        let (wy, wmo, wd, wh, wmi, ws) = civil_from_unix_utc(v.went_live);
        (format!("{wy:04}{wmo:02}{wd:02}"), format!("{wh:02}{wmi:02}{ws:02}"))
    } else {
        (String::new(), String::new())
    };
    let tmpl = if template.trim().is_empty() {
        "{name}_{date}_{time}"
    } else {
        template
    };
    let expanded = tmpl
        .replace("{name}", v.name)
        .replace("{title}", v.title)
        .replace("{channel}", v.channel)
        .replace("{video_id}", v.video_id)
        .replace("{quality}", v.quality)
        .replace("{resolution}", v.resolution)
        .replace("{height}", v.height)
        .replace("{width}", v.width)
        .replace("{fps}", v.fps)
        .replace("{vcodec}", v.vcodec)
        .replace("{acodec}", v.acodec)
        .replace("{tool}", v.tool)
        .replace("{mode}", v.mode)
        .replace("{platform}", v.platform)
        .replace("{take}", v.take)
        .replace("{games}", v.games)
        .replace("{date}", &date)
        .replace("{time}", &time)
        .replace("{timestamp}", &v.secs.to_string())
        .replace("{year}", &format!("{y:04}"))
        .replace("{month}", &format!("{mo:02}"))
        .replace("{day}", &format!("{d:02}"))
        .replace("{hour}", &format!("{h:02}"))
        .replace("{minute}", &format!("{mi:02}"))
        .replace("{second}", &format!("{s:02}"))
        .replace("{went_live_date}", &wl_date)
        .replace("{went_live_time}", &wl_time)
        // backward-compat aliases for tokens listed in the old tooltip
        .replace("{id}", v.video_id)
        .replace("{ts}", &v.secs.to_string())
        .replace("{category}", v.games);
    let cleaned = sanitize_filename(&expanded);
    // Bound the stem so it plus any companion suffix (the longest is
    // `.live_chat.json`) plus a collision suffix never exceeds NTFS's
    // per-component limit — see `MAX_STEM_UTF16_LEN`. Re-sanitize the trailing
    // edge: a cut can land on a space/period, which `sanitize_filename`'s trim
    // already handled for the untruncated end but not for a newly-created one.
    let cleaned = if cleaned.encode_utf16().count() > MAX_STEM_UTF16_LEN {
        truncate_utf16(&cleaned, MAX_STEM_UTF16_LEN)
            .trim_end_matches([' ', '.'])
            .to_string()
    } else {
        cleaned
    };
    if cleaned.is_empty() {
        format!("{}_{date}_{time}", sanitize_filename(v.name))
    } else {
        cleaned
    }
}

/// A stem (filename without extension) that doesn't collide with an existing
/// `<stem>.<ext>` in `dir`: returns `stem`, else `stem (2)`, `stem (3)`, … —
/// matching the file-manager convention. A missing `dir` can't collide. `ignore`
/// (the file being renamed, if any) is treated as free so a post-rename to the
/// same/own name isn't pushed to a new suffix.
pub(crate) fn unique_stem(dir: &Path, stem: &str, ext: &str, ignore: Option<&Path>) -> String {
    let taken = |s: &str| {
        let p = dir.join(format!("{s}.{ext}"));
        Some(p.as_path()) != ignore && p.exists()
    };
    if !taken(stem) {
        return stem.to_string();
    }
    for n in 2..10_000 {
        let cand = format!("{stem} ({n})");
        if !taken(&cand) {
            return cand;
        }
    }
    // Pathological fallback (10k same-named files): stamp it so we never clobber.
    format!("{stem} ({})", now_unix())
}

pub(crate) fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Windows also forbids a path component ending in a space or a period
    // (e.g. a stream title like "Chatting late tonight." would otherwise
    // produce a filename Windows refuses with the same ERROR_INVALID_NAME as
    // the length overflow above) — trim repeatedly, since removing one can
    // reveal another underneath.
    cleaned.trim().trim_end_matches([' ', '.']).to_string()
}

/// Truncate `s` to at most `max_units` **UTF-16 code units** — what NTFS and
/// Win32 file APIs actually count for a path component's length (a character
/// outside the Basic Multilingual Plane, e.g. most emoji, is a surrogate pair
/// = 2 units) — without splitting a character. A plain byte- or
/// char-count-based truncation would systematically undercount any title/
/// category containing emoji and could still overflow the true NTFS limit.
fn truncate_utf16(s: &str, max_units: usize) -> &str {
    let mut acc = 0usize;
    for (i, ch) in s.char_indices() {
        acc += ch.len_utf16();
        if acc > max_units {
            return &s[..i];
        }
    }
    s
}

/// OS-level "the filename itself is invalid/too long" errors — the reactive
/// backstop behind the proactive [`MAX_STEM_UTF16_LEN`] cap, for anything that
/// slips past it regardless (a filesystem with a tighter limit, an unusually
/// long companion suffix, `unique_stem`'s collision suffix tipping it over).
/// Distinguished from a transient problem (sharing violation, missing
/// directory, permissions) that retrying with a *different name* wouldn't fix:
/// - Windows: `ERROR_INVALID_NAME` (123 — NTFS's 255-unit-per-component limit,
///   or an illegal trailing `.`/` ` that slipped past sanitization) and
///   `ERROR_FILENAME_EXCED_RANGE` (206).
/// - Unix: `ENAMETOOLONG` (36 on Linux, 63 on macOS/BSD).
fn is_name_too_long(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(123 | 206 | 36 | 63))
}

/// Replace the trailing `remove_units` **UTF-16 code units** of `s` with
/// `"..."` — a visible marker that the OS rejected the full name and it was
/// shortened to fit — without splitting a character (e.g. an emoji surrogate
/// pair). `remove_units` is how much to strip *before* the 3-unit marker is
/// appended, not a target length — see [`stem_fitting_budget`], which does
/// that arithmetic.
fn ellipsize_utf16(s: &str, remove_units: usize) -> String {
    let total = s.encode_utf16().count();
    if remove_units >= total {
        return "...".to_string();
    }
    format!("{}...", truncate_utf16(s, total - remove_units))
}

/// Shorten `stem` (marking the cut with `"..."`, see [`ellipsize_utf16`]) so
/// its UTF-16 length is at most `budget` — a no-op if it already fits.
/// Deterministic: the same `(stem, budget)` always produces the same result.
fn stem_fitting_budget(stem: &str, budget: usize) -> String {
    let stem_units = stem.encode_utf16().count();
    if stem_units <= budget {
        return stem.to_string();
    }
    // ellipsize_utf16 removes `remove_units` THEN appends "..." (3 units), so
    // ask for exactly enough removal that the total after appending lands at
    // `budget`, not `budget - 3`.
    ellipsize_utf16(stem, stem_units + 3 - budget)
}

/// Stem-length budgets tried when a name overflows, deterministic and
/// **independent of which specific suffix** (`.mkv`, `.chat.jsonl`,
/// `.en.vtt`, …) triggered the retry. That independence is the whole point:
/// a take's video and every one of its companions each call
/// [`rename_or_shorten`] starting from the SAME original stem, so whichever
/// budget rung first makes it fit is the SAME rung for all of them — they
/// converge on one identical shortened stem instead of each laddering to a
/// length sized only for its own suffix (which would desync sibling files
/// that must share a prefix — the original design's mistake). The first
/// budget is [`MAX_STEM_UTF16_LEN`], the proactive cap that already leaves
/// room for the longest companion suffix, so it succeeds on the first try in
/// the overwhelming majority of real overflows; the smaller ones are a
/// backstop for a total-path-length problem the per-component math alone
/// can't see (e.g. a deeply nested output directory).
const STEM_SHORTEN_BUDGETS: [usize; 3] = [MAX_STEM_UTF16_LEN, 120, 40];

/// Attempt `tokio::fs::rename(from, dir.join(format!("{stem}.{suffix}")))`.
/// If the OS rejects the destination name (see [`is_name_too_long`]) —
/// overwhelmingly NTFS's 255-UTF-16-unit-per-component limit — deterministically
/// shorten `stem` (see [`stem_fitting_budget`]/[`STEM_SHORTEN_BUDGETS`]) and
/// retry. `suffix` (e.g. `"mkv"`, `"chat.jsonl"`, `"en.vtt"`) is never
/// touched — it's what identifies the file's role, and companion-matching
/// logic elsewhere depends on it staying intact.
///
/// A recording actually landing on disk matters more than a fully-descriptive
/// name, so this is the *last-resort* backstop behind the proactive stem cap
/// in `expand_template` (`MAX_STEM_UTF16_LEN`), which should make this rare in
/// practice — it exists for whatever that cap didn't anticipate.
///
/// Returns the path the file actually ended up at (`dir/{stem}.{suffix}`, or
/// a shortened sibling), or the original error if even the shortest attempt
/// still fails for an unrelated reason (e.g. a missing directory), or if a
/// shortened candidate would collide with an unrelated existing file (rather
/// than risk clobbering it, or a numeric disambiguator that would desync this
/// file from siblings independently computing the same candidate).
async fn rename_or_shorten(
    from: &Path,
    dir: &Path,
    stem: &str,
    suffix: &str,
) -> std::io::Result<PathBuf> {
    let to = dir.join(format!("{stem}.{suffix}"));
    match tokio::fs::rename(from, &to).await {
        Ok(()) => Ok(to),
        Err(e) if !is_name_too_long(&e) => Err(e),
        Err(first_err) => {
            for budget in STEM_SHORTEN_BUDGETS {
                let short_stem = stem_fitting_budget(stem, budget);
                if short_stem == stem {
                    continue; // this budget doesn't shorten it any further
                }
                let candidate = dir.join(format!("{short_stem}.{suffix}"));
                if candidate.exists() {
                    // Don't clobber it, and don't disambiguate with a numeric
                    // suffix here — that would itself desync this file from
                    // siblings that independently compute the same candidate.
                    return Err(first_err);
                }
                match tokio::fs::rename(from, &candidate).await {
                    Ok(()) => {
                        warn!(
                            "shortened an over-long filename to fit the filesystem: {} -> {}",
                            to.display(),
                            candidate.display()
                        );
                        return Ok(candidate);
                    }
                    Err(e) if is_name_too_long(&e) => continue, // still too long, shrink further
                    Err(e) => return Err(e),
                }
            }
            Err(first_err)
        }
    }
}

/// Proactively shorten `path`'s file stem (see [`stem_fitting_budget`]) if
/// it's long enough to risk NTFS's per-component limit, returning the
/// original path unchanged if it's already safe. Unlike [`rename_or_shorten`],
/// this never touches the filesystem — it's for callers that WRITE to their
/// destination directly (ffmpeg-based remux) rather than renaming an existing
/// file, so there's no OS error to react to; the only option is to make sure
/// the destination is safe *before* the write is attempted.
fn path_with_safe_stem(path: &Path) -> PathBuf {
    let (Some(dir), Some(stem)) = (
        path.parent(),
        path.file_stem().map(|s| s.to_string_lossy().into_owned()),
    ) else {
        return path.to_path_buf();
    };
    let safe_stem = stem_fitting_budget(&stem, MAX_STEM_UTF16_LEN);
    if safe_stem == stem {
        return path.to_path_buf();
    }
    match path.extension().map(|e| e.to_string_lossy().into_owned()) {
        Some(ext) if !ext.is_empty() => dir.join(format!("{safe_stem}.{ext}")),
        _ => dir.join(safe_stem),
    }
}

/// Convert a unix timestamp to a UTC civil date/time (Howard Hinnant's algorithm).
fn civil_from_unix_utc(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hh, mm, ss)
}

// ---------- Twitch VOD background checker ----------

/// How long to wait between polling attempts.
const VOD_POLL_INTERVAL_SECS: u64 = 5 * 60;
/// Maximum number of polls before giving up (5 min × 12 = 60 min total).
const VOD_MAX_POLLS: u32 = 12;
/// Maximum delta between a VOD's `created_at` and the broadcast's `went_live_at`
/// for them to be considered the same stream (2 hours).
const VOD_MATCH_WINDOW_SECS: i64 = 2 * 3600;

/// Background task that polls Helix `/videos` for the Twitch VOD produced by a
/// just-finished recording. Polls every [`VOD_POLL_INTERVAL_SECS`] for up to
/// [`VOD_MAX_POLLS`] attempts, then marks the recording `not_published` if
/// no matching VOD appears.
async fn check_twitch_vod(
    ctx: Arc<DetectContext>,
    store: Arc<Store>,
    events: EventTx,
    manual_tx: mpsc::UnboundedSender<ManualCommand>,
    rec_id: i64,
    login: String,
    went_live_at: Option<i64>,
) {
    // Wait before the first check — VODs take several minutes to appear.
    tokio::time::sleep(Duration::from_secs(VOD_POLL_INTERVAL_SECS)).await;

    let (client_id, token) = match ctx.twitch_helix_auth().await {
        Ok(t) => t,
        Err(e) => {
            warn!(rec_id, "VOD check: Twitch auth unavailable: {e:#}");
            return;
        }
    };
    let user_id = match ctx.twitch_user_id(&client_id, &token, &login).await {
        Some(id) => id,
        None => {
            warn!(rec_id, login, "VOD check: could not resolve user_id");
            return;
        }
    };

    for poll in 0..VOD_MAX_POLLS {
        if poll > 0 {
            tokio::time::sleep(Duration::from_secs(VOD_POLL_INTERVAL_SECS)).await;
        }
        match poll_twitch_vod(&ctx.http_client(), &client_id, &token, &user_id, went_live_at).await {
            Ok(Some((vod_id, muted_secs))) => {
                let _ = store.set_recording_vod_found(rec_id, &vod_id, muted_secs);
                let _ = events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                info!(rec_id, vod_id, muted_secs, "Twitch VOD found");
                let archive_on = archive_download_enabled(&store, rec_id);
                if muted_secs > 0 {
                    // A muted VOD is silenced — never a plain download. Un-mute via
                    // the CDN recovery, flag it, and (archive case) raise an issue.
                    if archive_on {
                        let _ = store.set_recording_vod_dl(rec_id, "muted", None);
                        let channel = archive_channel_name(&store, rec_id).unwrap_or_else(|| login.clone());
                        let _ = events.send(AppEvent::VodMuted {
                            recording_id: rec_id,
                            channel,
                            muted_secs,
                        });
                        spawn_auto_recovery(&ctx, &store, &events, rec_id);
                    } else if setting_true(&store, crate::recovery::K_AUTO_RECOVER_MUTED) {
                        spawn_auto_recovery(&ctx, &store, &events, rec_id);
                    }
                } else if archive_on {
                    // Clean published VOD — download it (alongside / replace).
                    enqueue_vod_archive(&store, &manual_tx, rec_id, &crate::vod_archive::twitch_vod_url(&vod_id));
                }
                return;
            }
            Ok(None) => {} // not yet available
            Err(e) => warn!(rec_id, "VOD poll error: {e:#}"),
        }
    }

    let _ = store.set_recording_vod_not_published(rec_id);
    let _ = events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
    info!(rec_id, login, "Twitch VOD not published after polling timeout");
    // Auto-recover a deleted VOD (no published archive) when enabled.
    if setting_true(&store, crate::recovery::K_AUTO_RECOVER_DELETED) {
        spawn_auto_recovery(&ctx, &store, &events, rec_id);
    }
}

/// Read a boolean setting (`"true"`/`"1"` = on), defaulting to `false`.
fn setting_true(store: &Store, key: &str) -> bool {
    matches!(
        store.get_setting(key).ok().flatten().as_deref(),
        Some("true") | Some("1")
    )
}

/// Whether the post-stream VOD-download feature resolves ON for a recording
/// (global < channel < instance).
fn archive_download_enabled(store: &Store, rec_id: i64) -> bool {
    store
        .recording_replace_info(rec_id)
        .ok()
        .flatten()
        .map(|(channel_id, monitor_id, _, _)| {
            crate::vod_archive::effective_vod_download(store, channel_id, monitor_id)
        })
        .unwrap_or(false)
}

/// The display channel name for a recording (for the muted-VOD notification).
fn archive_channel_name(store: &Store, rec_id: i64) -> Option<String> {
    let (monitor_id, _) = store.get_recording_paths(rec_id).ok().flatten()?;
    store
        .get_monitor_with_channel(monitor_id)
        .ok()
        .flatten()
        .map(|mw| mw.channel.name)
}

/// Enqueue a detached yt-dlp download of a recording's published VOD (yt-dlp →
/// MKV) as `{live_stem}.vod.mkv` in the recording's output dir, link it to the
/// recording (`vod_dl_video_id`), and start it via the command bus. The completion
/// hook ([`Supervisor::finalize_vod_archive`]) handles alongside/replace.
fn enqueue_vod_archive(
    store: &Store,
    manual_tx: &mpsc::UnboundedSender<ManualCommand>,
    rec_id: i64,
    vod_url: &str,
) -> bool {
    let Ok(Some((monitor_id, output_path))) = store.get_recording_paths(rec_id) else {
        return false;
    };
    let Ok(Some(mw)) = store.get_monitor_with_channel(monitor_id) else {
        return false;
    };
    let out = Path::new(&output_path);
    let Some(dir) = out.parent().map(|p| p.to_string_lossy().into_owned()).filter(|d| !d.is_empty())
    else {
        warn!(rec_id, "vod archive: recording has no output dir");
        return false;
    };
    let live_stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("rec_{rec_id}"));
    let m = &mw.monitor;
    let video = crate::models::Video {
        id: 0,
        url: vod_url.to_string(),
        title: format!("VOD · {} · rec #{rec_id}", mw.channel.name),
        channel: mw.channel.name.clone(),
        platform: m.platform(),
        tool: crate::models::Tool::YtDlp,
        quality: if m.quality.trim().is_empty() { "best".into() } else { m.quality.clone() },
        output_dir: dir,
        // Literal stem (no tokens) → the file is `{live_stem}.vod.mkv`, distinct
        // from the live `{live_stem}.mkv`.
        filename_template: format!("{live_stem}.vod"),
        auth_kind: m.auth_kind,
        auth_value: m.auth_value.clone(),
        audio_tracks: String::new(),
        subtitle_tracks: String::new(),
        chat_log: false,
        extra_args: String::new(),
        auto_title: false,
        status: "queued".into(),
        output_path: String::new(),
        bytes: 0,
        created_at: now_unix(),
        exit_code: None,
        log_excerpt: String::new(),
        started_at: None,
        ended_at: None,
    };
    match store.insert_video(&video) {
        Ok(id) => {
            let _ = store.set_recording_vod_dl(rec_id, "downloading", Some(id));
            let _ = manual_tx.send(ManualCommand::StartVideo(id));
            true
        }
        Err(e) => {
            warn!(rec_id, "vod archive: insert_video failed: {e:#}");
            false
        }
    }
}

/// Spawn a recovery for a tracked recording, seeded from its stored broadcast id +
/// go-live time. No-op when the recording lacks a stream id (can't derive the URL)
/// or is past the ~60-day CDN window.
fn spawn_auto_recovery(ctx: &Arc<DetectContext>, store: &Arc<Store>, events: &EventTx, rec_id: i64) {
    let Ok(Some(seed)) = store.recording_recovery_seed(rec_id) else {
        return;
    };
    if seed.stream_id.is_empty() {
        return;
    }
    if now_unix() - seed.start_epoch > 60 * 86_400 {
        return; // past the CDN retention window
    }
    let Some(login) = crate::detectors::twitch_login(&seed.monitor_url) else {
        return;
    };
    let inputs = crate::recovery::RecoveryInputs {
        login,
        broadcast_id: seed.stream_id,
        start_epoch: seed.start_epoch,
        went_live_approx: seed.went_live_approx,
        vod_id: seed.vod_id,
    };
    let quality = store
        .get_setting(crate::recovery::K_RECOVERY_QUALITY)
        .ok()
        .flatten()
        .unwrap_or_default();
    let (client, store, events) = (ctx.http_client(), store.clone(), events.clone());
    tokio::spawn(async move {
        let task_id = crate::events::next_task_id();
        crate::recovery::run_recovery(
            client,
            store,
            events,
            inputs,
            quality,
            crate::recovery::RecoverySink::Recording(rec_id),
            seed.deleted,
            task_id,
        )
        .await;
    });
}

/// Query Helix `/helix/videos` for the streamer's most recent archive VODs and
/// find one whose `created_at` is within [`VOD_MATCH_WINDOW_SECS`] of
/// `went_live_at`. Returns `Some((vod_id, muted_secs))` on match, `None` if
/// no matching VOD exists yet, or an error on a transient API failure.
async fn poll_twitch_vod(
    client: &reqwest::Client,
    client_id: &str,
    token: &str,
    user_id: &str,
    went_live_at: Option<i64>,
) -> anyhow::Result<Option<(String, i64)>> {
    use anyhow::bail;
    let resp = client
        .get("https://api.twitch.tv/helix/videos")
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .query(&[("user_id", user_id), ("type", "archive"), ("first", "20")])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Helix /videos: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let Some(data) = v["data"].as_array() else {
        return Ok(None);
    };
    for item in data {
        let Some(vod_id) = item["id"].as_str() else {
            continue;
        };
        let Some(created_at_str) = item["created_at"].as_str() else {
            continue;
        };
        let Some(created_ts) = crate::detectors::parse_rfc3339(created_at_str) else {
            continue;
        };
        let matches = match went_live_at {
            Some(wl) => (created_ts - wl).abs() <= VOD_MATCH_WINDOW_SECS,
            None => true, // no anchor — accept the most recent archive
        };
        if !matches {
            continue;
        }
        let muted_secs: i64 = item["muted_segments"]
            .as_array()
            .map(|segs| segs.iter().filter_map(|s| s["duration"].as_i64()).sum())
            .unwrap_or(0);
        return Ok(Some((vod_id.to_string(), muted_secs)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Channel, Container, DetectionMethod, Monitor, Tool};

    fn row(tool: Tool, container: Container, platform: Platform) -> MonitorWithChannel {
        // The instance URL now drives the platform-specific plan, so give it one
        // that matches `platform`.
        let url = match platform {
            Platform::Twitch => "https://twitch.tv/cool",
            Platform::YouTube => "https://youtube.com/@cool",
            Platform::Kick => "https://kick.com/cool",
            Platform::Generic => "https://example.com/cool",
        };
        MonitorWithChannel {
            channel: Channel {
                id: 1,
                name: "Cool Streamer".into(),
                url: url.into(),
                platform,
                created_at: 0,
                color: String::new(),
                preferred_platform: None,
                enabled: true,
            },
            monitor: Monitor {
                id: 7,
                channel_id: 1,
                url: url.into(),
                enabled: true,
                tool,
                detection_method: DetectionMethod::TwitchApi,
                poll_interval_secs: 60,
                quality: "best".into(),
                output_dir: "C:/rec".into(),
                filename_template: "{name}_{date}_{time}".into(),
                container,
                capture_from_start: true,
                dual_capture: false,
                ad_free: false,
                auth_kind: AuthKind::Inherit,
                auth_value: String::new(),
                audio_tracks: String::new(),
                subtitle_tracks: String::new(),
                chat_log: false,
                fetch_thumbnail: false,
                thumbnail_in_toast: false,
                fetch_chat_assets: false,
                extra_args: String::new(),
                max_concurrent: 1,
                last_checked_at: None,
                last_state: "idle".into(),
                sabr_codec_pref: SabrCodecPref::Inherit,
                sabr_codec_custom: String::new(),
            },
            last_recording_started: None,
            last_recording_ended: None,
            last_recording_status: None,
            last_recording_went_live: None,
            last_recording_went_live_approx: false,
            last_recording_lost_secs: None,
            last_recording_ad_count: 0,
            last_recording_ad_secs: 0,
            last_recording_meta_changes: 0,
            last_recording_title: String::new(),
            last_recording_category: String::new(),
            last_recording_log: String::new(),
            ad_free_sub: None,
            recording_count: 0,
            next_stream_at: None,
            next_stream_title: String::new(),
        }
    }

    #[test]
    fn unique_stem_avoids_existing_files() {
        let dir = std::env::temp_dir()
            .join(format!("sa_unique_{}_{}", std::process::id(), now_unix()));
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing there yet -> stem unchanged.
        assert_eq!(unique_stem(&dir, "Layna", "mkv", None), "Layna");
        std::fs::write(dir.join("Layna.mkv"), b"x").unwrap();
        assert_eq!(unique_stem(&dir, "Layna", "mkv", None), "Layna (2)");
        std::fs::write(dir.join("Layna (2).mkv"), b"x").unwrap();
        assert_eq!(unique_stem(&dir, "Layna", "mkv", None), "Layna (3)");
        // A different extension doesn't collide.
        assert_eq!(unique_stem(&dir, "Layna", "ts", None), "Layna");
        // A missing directory can't collide.
        assert_eq!(unique_stem(&dir.join("nope"), "Layna", "mkv", None), "Layna");
        // The file being renamed is ignored, so its own name is treated as free
        // (the post-rename no-op case): "Layna (2).mkv" exists but is `ignore`.
        let own = dir.join("Layna (2).mkv");
        assert_eq!(unique_stem(&dir, "Layna", "mkv", Some(&own)), "Layna (2)");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn companion_sidecars_follow_rename() {
        let dir = std::env::temp_dir()
            .join(format!("sa_subs_{}_{}", std::process::id(), now_unix()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let old = "cap_20260620";
        let new = "Show_1080p";
        // The video has already been renamed; its companions have not.
        tokio::fs::write(dir.join(format!("{new}.mkv")), b"v").await.unwrap();
        tokio::fs::write(dir.join(format!("{old}.en.vtt")), b"s").await.unwrap();
        tokio::fs::write(dir.join(format!("{old}.chat.jsonl")), b"c").await.unwrap();
        tokio::fs::write(dir.join(format!("{old}.live_chat.json")), b"c").await.unwrap();
        // A same-stem non-companion file must be left alone.
        tokio::fs::write(dir.join(format!("{old}.notes.txt")), b"x").await.unwrap();

        rename_companion_sidecars(&dir, old, new).await;

        assert!(dir.join(format!("{new}.en.vtt")).exists());
        assert!(dir.join(format!("{new}.chat.jsonl")).exists());
        assert!(dir.join(format!("{new}.live_chat.json")).exists());
        assert!(!dir.join(format!("{old}.en.vtt")).exists());
        assert!(!dir.join(format!("{old}.chat.jsonl")).exists());
        assert!(dir.join(format!("{old}.notes.txt")).exists());
        assert!(!dir.join(format!("{new}.notes.txt")).exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn is_name_too_long_matches_known_codes_only() {
        let too_long = |code: i32| std::io::Error::from_raw_os_error(code);
        assert!(is_name_too_long(&too_long(123))); // Windows ERROR_INVALID_NAME
        assert!(is_name_too_long(&too_long(206))); // Windows ERROR_FILENAME_EXCED_RANGE
        assert!(is_name_too_long(&too_long(36))); // Linux ENAMETOOLONG
        assert!(is_name_too_long(&too_long(63))); // macOS/BSD ENAMETOOLONG
        // A transient/unrelated error must NOT trigger the shortening path —
        // retrying with a different name wouldn't fix a sharing violation,
        // a missing directory, or a permissions problem.
        assert!(!is_name_too_long(&too_long(32))); // ERROR_SHARING_VIOLATION
        assert!(!is_name_too_long(&too_long(5))); // ERROR_ACCESS_DENIED
        assert!(!is_name_too_long(&std::io::Error::new(std::io::ErrorKind::Other, "x")));
    }

    #[test]
    fn ellipsize_utf16_marks_truncation_without_splitting_chars() {
        assert_eq!(ellipsize_utf16("abcdef", 3), "abc...");
        // Removing more than the whole string still yields a valid marker.
        assert_eq!(ellipsize_utf16("ab", 50), "...");
        // A surrogate pair ('🥹', 2 units) must never be split: removing
        // exactly 2 units lands right at its boundary (kept whole); removing
        // 3 would split it mid-pair, so the whole emoji is dropped instead of
        // producing a corrupt lone surrogate (constructing the `String` at
        // all is proof the result stayed valid UTF-8).
        let s = "ab🥹cd"; // a(1) b(1) 🥹(2) c(1) d(1) = 6 units total
        assert_eq!(ellipsize_utf16(s, 2), "ab🥹...");
        assert_eq!(ellipsize_utf16(s, 3), "ab...");
    }

    #[tokio::test]
    async fn rename_or_shorten_falls_back_on_overflow_and_preserves_content() {
        let dir = std::env::temp_dir()
            .join(format!("sa_shorten_{}_{}", std::process::id(), now_unix()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let src = dir.join("source.tmp");
        tokio::fs::write(&src, b"payload").await.unwrap();

        // Deliberately overflow NTFS's 255-UTF-16-unit-per-component limit:
        // 300 'x's + ".mkv" (4) = 304 units.
        let huge_stem = "x".repeat(300);
        let result = rename_or_shorten(&src, &dir, &huge_stem, "mkv").await;

        let actual = result.expect("must fall back to a shortened name, not fail outright");
        assert!(actual.is_file(), "the file must actually exist at the returned path");
        assert_eq!(tokio::fs::read(&actual).await.unwrap(), b"payload");
        assert!(!src.exists(), "the source must be gone (this was a rename, not a copy)");

        let name = actual.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.contains("..."), "shortened name must visibly mark the cut: {name}");
        assert!(name.ends_with(".mkv"), "the suffix must never be touched: {name}");
        assert!(
            name.encode_utf16().count() <= NTFS_MAX_COMPONENT_UTF16,
            "shortened name must actually fit: {} units",
            name.encode_utf16().count()
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn rename_or_shorten_passes_through_unrelated_errors() {
        // A source that doesn't exist fails with NotFound, not a
        // name-too-long condition — must propagate as-is, no retry loop.
        let dir = std::env::temp_dir()
            .join(format!("sa_shorten_missing_{}_{}", std::process::id(), now_unix()));
        let missing_src = dir.join("does_not_exist.tmp");
        let err = rename_or_shorten(&missing_src, &dir, "stem", "mkv")
            .await
            .expect_err("a missing source must fail, not silently succeed");
        assert!(!is_name_too_long(&err));
    }

    #[test]
    fn path_with_safe_stem_is_a_noop_when_already_safe() {
        let short = std::path::Path::new(r"C:\rec\short.mkv");
        assert_eq!(path_with_safe_stem(short), short);
    }

    #[test]
    fn path_with_safe_stem_shortens_an_overlong_stem_proactively() {
        // Used to pre-shorten a ffmpeg REMUX destination before the write is
        // even attempted (ffmpeg writes directly — there's no OS rename error
        // to react to afterward the way rename_or_shorten reacts to one).
        let long_stem = "z".repeat(260);
        let path = std::path::PathBuf::from(format!(r"C:\rec\{long_stem}.mkv"));
        let safe = path_with_safe_stem(&path);
        assert_ne!(safe, path);
        assert_eq!(safe.parent(), path.parent());
        assert_eq!(safe.extension().unwrap(), "mkv");
        let name = safe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.contains("..."), "must mark the cut: {name}");
        assert!(name.encode_utf16().count() <= NTFS_MAX_COMPONENT_UTF16);
    }

    #[tokio::test]
    async fn companion_sidecar_rename_shortens_instead_of_orphaning() {
        // Reproduces the exact CottontailVA-class failure: the main video's
        // stem fits under its own extension but overflows once combined with
        // a companion's longer suffix. The sidecar must still follow the
        // rename (under a shortened, "..."-marked name) instead of being
        // silently left behind under `old_stem`.
        let dir = std::env::temp_dir()
            .join(format!("sa_shorten_sidecar_{}_{}", std::process::id(), now_unix()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let old_stem = "old";
        // 250 'x's: old="old" -> new_stem+".mkv" is fine (254 units), but
        // new_stem+".chat.jsonl" (261 units) overflows 255.
        let new_stem = "x".repeat(250);
        tokio::fs::write(dir.join(format!("{old_stem}.chat.jsonl")), b"chat-data").await.unwrap();

        rename_companion_sidecars(&dir, old_stem, &new_stem).await;

        assert!(
            !dir.join(format!("{old_stem}.chat.jsonl")).exists(),
            "must not still be sitting under the old placeholder name"
        );
        let mut rd = tokio::fs::read_dir(&dir).await.unwrap();
        let mut found = None;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".chat.jsonl") {
                found = Some(name);
            }
        }
        let name = found.expect("the sidecar must have followed the rename under some name");
        assert!(name.contains("..."), "the shortened name must mark the cut: {name}");
        assert!(name.encode_utf16().count() <= NTFS_MAX_COMPONENT_UTF16);
        assert_eq!(
            tokio::fs::read(dir.join(&name)).await.unwrap(),
            b"chat-data",
            "content must be preserved, not just the name"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn shortened_stem_is_deterministic_across_different_suffix_lengths() {
        // The exact regression an adversarial review caught in an earlier
        // version of this fix: shortening must be a PURE function of the
        // stem alone, independent of which suffix triggered it — otherwise
        // the video (short ".mkv" suffix) and its companions (longer
        // ".chat.jsonl"/".live_chat.json" suffixes) can each independently
        // choose a DIFFERENT shortened stem and end up mismatched, which is
        // exactly the "sidecar orphaned under a name that no longer matches
        // the video" failure this whole feature exists to prevent.
        let dir = std::env::temp_dir()
            .join(format!("sa_shorten_converge_{}_{}", std::process::id(), now_unix()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Long enough that EVERY suffix below needs shortening (so we're
        // actually exercising the fallback for all of them, not just the
        // longest), independent of what triggered it first — even combined
        // with the shortest suffix here ("mkv", 4 units incl. the dot),
        // 260 + 4 = 264 > 255.
        let stem = "y".repeat(260);

        let mut shortened_stems = Vec::new();
        for suffix in ["mkv", "en.vtt", "chat.jsonl", "live_chat.json"] {
            let src = dir.join(format!("src_{suffix}.tmp"));
            tokio::fs::write(&src, b"x").await.unwrap();
            let actual = rename_or_shorten(&src, &dir, &stem, suffix)
                .await
                .unwrap_or_else(|e| panic!("rename for suffix {suffix} must succeed: {e:#}"));
            let actual_stem = actual.file_stem().unwrap().to_string_lossy().into_owned();
            // .en.vtt / .chat.jsonl further split on their own internal dots
            // via Path::file_stem() (it only strips the LAST component), so
            // recover the true shared stem by stripping the known suffix
            // from the full file name instead.
            let full_name = actual.file_name().unwrap().to_string_lossy().into_owned();
            let true_stem = full_name.strip_suffix(&format!(".{suffix}")).unwrap_or(&actual_stem).to_string();
            shortened_stems.push((suffix, true_stem));
        }

        let first = &shortened_stems[0].1;
        for (suffix, s) in &shortened_stems {
            assert_eq!(
                s, first,
                "suffix {suffix} converged on a DIFFERENT stem than {}: {s} vs {first}",
                shortened_stems[0].0
            );
        }
        assert!(first.contains("..."), "convergence check is only meaningful if shortening actually happened");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn fmt_fps_rounds() {
        assert_eq!(fmt_fps("60/1"), "60");
        assert_eq!(fmt_fps("30000/1001"), "30"); // 29.97 -> 30
        assert_eq!(fmt_fps("60000/1001"), "60"); // 59.94 -> 60
        assert_eq!(fmt_fps("50"), "50");
        assert_eq!(fmt_fps("0/0"), "");
        assert_eq!(fmt_fps("N/A"), "");
    }

    #[test]
    fn template_wants_media_detects_vars() {
        assert!(template_wants_media("{name}_{resolution}"));
        assert!(template_wants_media("{fps}"));
        assert!(template_wants_media("{vcodec}-{height}"));
        assert!(!template_wants_media("{name}_{date}_{quality}"));
        assert!(!template_wants_media("{name}_{video_id}"));
    }

    #[test]
    fn template_expands_games() {
        let out = expand_template(
            "{name}_{games}",
            &TemplateVars {
                name: "Layna",
                games: "Just Chatting, Valorant",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out, "Layna_Just Chatting, Valorant");
        assert!(template_wants_games("{name}_{games}"));
        assert!(!template_wants_games("{name}_{date}"));
    }

    #[test]
    fn format_games_dedups_orders_and_truncates() {
        // Case-insensitive dedup, blanks skipped, order of first appearance kept.
        let cats = vec![
            "Just Chatting".to_string(),
            "Valorant".to_string(),
            "just chatting".to_string(),
            String::new(),
            " Valorant ".to_string(),
        ];
        assert_eq!(format_games(&cats), "Just Chatting, Valorant");
        // Capped to GAMES_MAX_LEN characters.
        let many: Vec<String> = (0..50).map(|i| format!("Game{i}")).collect();
        assert!(format_games(&many).chars().count() <= GAMES_MAX_LEN);
        assert_eq!(format_games(&[]), "");
    }

    #[test]
    fn civil_date_known_value() {
        // 1700000000 = 2023-11-14 22:13:20 UTC
        assert_eq!(
            civil_from_unix_utc(1_700_000_000),
            (2023, 11, 14, 22, 13, 20)
        );
    }

    #[test]
    fn template_expands_and_sanitizes() {
        let name = expand_template(
            "{name}_{date}",
            &TemplateVars {
                name: "Bad/Name?",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(name, "Bad_Name__20231114");
    }

    #[test]
    fn sanitize_filename_strips_trailing_dots_and_spaces() {
        // Windows forbids a path component ending in '.' or ' ' — a stream
        // title ending in a period (very plausible) must not silently produce
        // an unrenameable/uncreatable filename.
        assert_eq!(sanitize_filename("Chatting late tonight."), "Chatting late tonight");
        assert_eq!(sanitize_filename("Trailing space "), "Trailing space");
        // Repeated trailing dots/spaces are all stripped, not just one layer.
        assert_eq!(sanitize_filename("Ellipsis... "), "Ellipsis");
        // Illegal chars still map to '_' as before.
        assert_eq!(sanitize_filename("a:b"), "a_b");
    }

    #[test]
    fn truncate_utf16_counts_surrogate_pairs_not_chars() {
        // 'a' (1 unit) + '🥹' (U+1F979, outside the BMP -> 2 units).
        let s = "a🥹b";
        assert_eq!(truncate_utf16(s, 1), "a");
        // Cutting after the emoji's first unit must not split the surrogate
        // pair (and thus must not panic / produce invalid UTF-8) — the whole
        // emoji is dropped instead.
        assert_eq!(truncate_utf16(s, 2), "a");
        assert_eq!(truncate_utf16(s, 3), "a🥹");
        assert_eq!(truncate_utf16(s, 100), "a🥹b");
    }

    #[test]
    fn expand_template_caps_stem_for_the_longest_companion_suffix() {
        // Reproduces the 2026-07-04 CottontailVA incident: a long, emoji-laden
        // title plus several logged categories produced a stem that fit under
        // `.mkv` but overflowed NTFS's 255-UTF-16-unit limit once combined
        // with `.chat.jsonl` — the companion sidecar rename then failed with
        // os error 123 and was silently left behind under the old name.
        let categories: Vec<String> =
            ["Just Chatting", "Golf With Your Friends", "Super Battle Golf", "Left 4 Dead 2", "Overwatch"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let games = format_games(&categories);
        let title = "🥹 MOMMAS BEEN DEPRESSED BUT SHES BACK WITH MILKIES !!!💋_ !spotify !gg !soap";
        let out = expand_template(
            "{name} - {date} {time} - {title} [{games}] ({quality} {mode} {vcodec} {acodec}) - [{platform} {video_id}]",
            &TemplateVars {
                name: "CottontailVA",
                title,
                games: &games,
                quality: "1080p60",
                mode: "live",
                vcodec: "h264",
                acodec: "aac",
                platform: "twitch",
                video_id: "318342459223",
                secs: 1_751_663_194,
                ..Default::default()
            },
        );
        let out_units = out.encode_utf16().count();
        assert!(out_units <= MAX_STEM_UTF16_LEN, "stem itself must respect the cap: {out_units}");
        // The property that actually matters: EVERY companion suffix, applied
        // on top of this stem, must still fit under NTFS's per-component limit.
        for suffix in [".ts", ".mkv", ".chat.jsonl", ".live_chat.json", ".en.vtt", ".thumbnail.jpg"] {
            let full = format!("{out}{suffix}");
            let units = full.encode_utf16().count();
            assert!(
                units <= NTFS_MAX_COMPONENT_UTF16,
                "{suffix} pushes the filename to {units} UTF-16 units (limit {NTFS_MAX_COMPONENT_UTF16})"
            );
        }
        // Must not have been chopped mid-surrogate-pair (would be invalid
        // UTF-8 / panic on construction) and must not end in a bare '.'/' '.
        assert!(!out.ends_with('.') && !out.ends_with(' '));
    }

    #[test]
    fn expand_template_leaves_short_stems_untouched() {
        // Non-regression: ordinary templates/values must not be truncated at all.
        let out = expand_template(
            "{name}_{date}_{time}",
            &TemplateVars { name: "Layna", secs: 1_700_000_000, ..Default::default() },
        );
        assert_eq!(out, "Layna_20231114_221320");
    }

    #[test]
    fn template_expands_video_id_quality_take() {
        let out = expand_template(
            "{name}_{video_id}_{quality}_take{take}",
            &TemplateVars {
                name: "Stream",
                video_id: "abc123",
                quality: "1080p60",
                take: "3",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out, "Stream_abc123_1080p60_take3");
        // Empty id (id-less detection) leaves the slot blank.
        let out = expand_template(
            "{name}-{video_id}",
            &TemplateVars {
                name: "Stream",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out, "Stream-");
    }

    #[test]
    fn parses_ytdlp_progress() {
        assert_eq!(parse_progress("DLPCT= 50.0%"), Some(0.5));
        assert_eq!(parse_progress("DLPCT=100.0%"), Some(1.0));
        assert_eq!(parse_progress("DLPCT=0.0%"), Some(0.0));
        // Non-marker lines and unknown values yield nothing.
        assert_eq!(parse_progress("[download]  45% of 100MiB"), None);
        assert_eq!(parse_progress("DLPCT=NA%"), None);
        assert_eq!(parse_progress("some other log line"), None);
    }

    #[test]
    fn parses_ytdlp_progress_and_speed() {
        // The combined template emits percent + speed (bytes/sec) per line.
        let (p, s) = parse_progress_fields("DLPCT= 42.0%;;SPEED=1258291.2");
        assert_eq!(p, Some(0.42));
        assert_eq!(s, Some(1_258_291.2));
        // Unknown speed ("NA") -> no speed, but the percent still parses.
        let (p, s) = parse_progress_fields("DLPCT= 5.0%;;SPEED=NA");
        assert_eq!(p, Some(0.05));
        assert_eq!(s, None);
        // Zero/negative speeds are ignored.
        assert_eq!(parse_progress_fields("DLPCT=10.0%;;SPEED=0").1, None);
        // A bare percent line (old format) still yields the fraction, no speed.
        assert_eq!(parse_progress_fields("DLPCT= 50.0%"), (Some(0.5), None));
        // Non-marker lines yield nothing.
        assert_eq!(parse_progress_fields("some other log line"), (None, None));
    }

    #[test]
    fn parses_streamlink_ad_break() {
        assert_eq!(
            parse_ad_break_secs(
                "[plugins.twitch][info] Detected advertisement break of 30 seconds"
            ),
            Some(30)
        );
        // Singular form ("1 second") and no log prefix.
        assert_eq!(
            parse_ad_break_secs("Detected advertisement break of 1 second"),
            Some(1)
        );
        // Other streamlink ad lines and unrelated lines don't match.
        assert_eq!(parse_ad_break_secs("Will skip ad segments"), None);
        assert_eq!(
            parse_ad_break_secs("Waiting for pre-roll ads to finish, be patient"),
            None
        );
        assert_eq!(parse_ad_break_secs("some other log line"), None);
    }

    #[test]
    fn template_expands_title_and_channel() {
        let out = expand_template(
            "{title}_{date}",
            &TemplateVars {
                name: "ignored",
                title: "My Stream!",
                channel: "Streamer",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out, "My Stream!_20231114");
        // name / title / channel stay distinct in expand_template itself.
        let out2 = expand_template(
            "{channel}-{name}-{title}",
            &TemplateVars {
                name: "Nm",
                title: "Ttl",
                channel: "Chan",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out2, "Chan-Nm-Ttl");
    }

    #[test]
    fn streamlink_mkv_records_ts_then_remuxes() {
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert_eq!(plan.program, "streamlink");
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.remux_to_mkv);
        assert!(
            plan.args
                .iter()
                .any(|a| a.contains("twitch-supported-codecs"))
        );
        assert!(plan.args.iter().any(|a| a == "best"));
    }

    #[test]
    fn ytdlp_mkv_records_ts_then_remuxes() {
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert_eq!(plan.program, "yt-dlp");
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.remux_to_mkv);
        assert!(plan.args.iter().any(|a| a == "--live-from-start"));
        assert!(plan.args.iter().any(|a| a == "--hls-use-mpegts"));
    }

    #[test]
    fn streamlink_token_adds_twitch_auth_header() {
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::Token("abc123".into()),
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(
            plan.args
                .iter()
                .any(|a| a == "--twitch-api-header=Authorization=OAuth abc123")
        );
    }

    #[test]
    fn ytdlp_cookies_added() {
        let browser = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::CookiesBrowser("firefox".into()),
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        let joined = browser.args.join(" ");
        assert!(joined.contains("--cookies-from-browser firefox"));

        let file = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::CookiesFile("C:/c.txt".into()),
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(file.args.join(" ").contains("--cookies C:/c.txt"));
    }

    #[test]
    fn audio_subtitle_track_selection() {
        // streamlink: "all"/"*" -> --hls-audio-select=*; a list passes through.
        let mut r = row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        r.monitor.audio_tracks = "all".into();
        r.monitor.subtitle_tracks = "all".into(); // ignored by streamlink
        let plan = build_plan(&r, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(plan.args.iter().any(|a| a == "--hls-audio-select=*"));
        assert!(!plan.args.iter().any(|a| a == "--sub-langs"));

        let mut r2 = row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        r2.monitor.audio_tracks = "en,de".into();
        let plan2 = build_plan(&r2, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(plan2.args.iter().any(|a| a == "--hls-audio-select=en,de"));

        // yt-dlp: "all" subs -> --sub-langs=all --write-subs; audio ignored. The
        // `--flag=value` form keeps a value from being mis-parsed as an option.
        let mut y = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y.monitor.subtitle_tracks = "all".into();
        y.monitor.audio_tracks = "all".into(); // ignored by yt-dlp
        let yplan = build_plan(&y, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        // live_chat is excluded from the main process via negation so "all"
        // doesn't pull in the chat stream and block the video download.
        assert!(yplan.args.iter().any(|a| a == "--sub-langs=all,-live_chat"));
        assert!(yplan.args.iter().any(|a| a == "--write-subs"));
        assert!(!yplan.args.iter().any(|a| a == "--hls-audio-select=*"));

        // "*" is normalized to "all,-live_chat" for subtitles too.
        let mut y2 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y2.monitor.subtitle_tracks = "*".into();
        let yplan2 = build_plan(&y2, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(yplan2.args.iter().any(|a| a == "--sub-langs=all,-live_chat"));

        // A language list passes through verbatim (joined form).
        let mut y3 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y3.monitor.subtitle_tracks = "en,de".into();
        let yplan3 = build_plan(&y3, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(yplan3.args.iter().any(|a| a == "--sub-langs=en,de"));

        // Empty (existing-monitor default) adds no track flags at all.
        let plain = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(
            !plain
                .args
                .iter()
                .any(|a| a.starts_with("--hls-audio-select"))
        );
    }

    #[test]
    fn chat_logging_ytdlp_live_chat() {
        // live_chat is NEVER requested by build_plan: the yt-dlp live_chat downloader
        // runs until the stream ends and blocks video download in the same process.
        // Chat replay is downloaded by build_video_plan (VOD) after the stream ends.
        let mut y = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y.monitor.chat_log = true;
        y.monitor.subtitle_tracks = String::new();
        let plan = build_plan(&y, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(!plan.args.iter().any(|a| a.contains("live_chat")));
        assert!(!plan.args.iter().any(|a| a == "--write-subs"));

        // Explicit subtitle selection still works (just no live_chat folded in).
        let mut y2 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y2.monitor.chat_log = true;
        y2.monitor.subtitle_tracks = "en".into();
        let plan2 = build_plan(&y2, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(!plan2.args.iter().any(|a| a.contains("live_chat")));
        assert!(plan2.args.iter().any(|a| a == "--sub-langs=en"));

        // Twitch + yt-dlp + chat_log -> NO yt-dlp live_chat (the native Twitch
        // chat logger handles it instead).
        let mut t = row(Tool::YtDlp, Container::Mkv, Platform::Twitch);
        t.monitor.chat_log = true;
        t.monitor.subtitle_tracks = String::new();
        let plant = build_plan(&t, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(!plant.args.iter().any(|a| a.contains("live_chat")));
    }

    #[test]
    fn resolve_auth_precedence() {
        // Inherit + global cookies -> browser cookies.
        let mut r = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        assert_eq!(
            resolve_auth(&r, "cookies", "chrome"),
            AuthSource::CookiesBrowser("chrome".into())
        );
        // Per-channel override wins over global.
        r.monitor.auth_kind = AuthKind::Token;
        r.monitor.auth_value = "tok".into();
        assert_eq!(
            resolve_auth(&r, "cookies", "chrome"),
            AuthSource::Token("tok".into())
        );
        // Disabled forces none even if a global default exists.
        r.monitor.auth_kind = AuthKind::Disabled;
        assert_eq!(resolve_auth(&r, "cookies", "chrome"), AuthSource::None);
    }

    #[test]
    fn streamlink_ts_keeps_ts() {
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Ts, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(plan.final_path.to_string_lossy().ends_with(".ts"));
        assert!(!plan.remux_to_mkv);
    }

    fn sabr_bins() -> YtDlpBins {
        YtDlpBins {
            system: String::new(),
            sabr: SabrConfig {
                enabled: true,
                binary: "C:/git/yt-dlp-dev/dist/yt-dlp.exe".into(),
                format: SABR_DEFAULT_FORMAT.into(),
                extractor_args: SABR_DEFAULT_EXTRACTOR_ARGS.into(),
                raw_args: String::new(),
                pot_args: SABR_DEFAULT_POT_ARGS.into(),
                codec_pref: SabrCodecPref::Auto,
                codec_custom: String::new(),
            },
        }
    }

    #[test]
    fn sabr_preview_args_join_at_live_edge() {
        // The live-edge preview ("Play new instance") must be the capture
        // command with from-start swapped for live-edge and an fMP4-first
        // format selector (HLS-playlist playback needs ISOBMFF files) —
        // nothing else.
        let bins = sabr_bins();
        let out = PathBuf::from(r"C:\tmp\.cache\preview.mkv");
        let preview = sabr_preview_args(
            &out, &AuthSource::None, &[], &bins.sabr, &[], "https://www.youtube.com/@chan",
        );
        assert!(preview.iter().any(|a| a == "--no-live-from-start"));
        assert!(!preview.iter().any(|a| a == "--live-from-start"));
        let fpos = preview.iter().position(|a| a == "-f").unwrap();
        assert_eq!(
            preview[fpos + 1],
            format!("bv[protocol=sabr][ext=mp4]+ba[protocol=sabr][ext=m4a]/{SABR_DEFAULT_FORMAT}")
        );
        let capture = sabr_capture_args(
            &out, &AuthSource::None, &[], &bins.sabr, &[], "https://www.youtube.com/@chan", true, "",
        );
        let normalize = |v: &[String]| {
            v.iter()
                .filter(|a| !a.contains("live-from-start") && !a.contains("[protocol=sabr]"))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(normalize(&preview), normalize(&capture));
    }

    #[test]
    fn deep_rewind_toggle_appends_extractor_arg() {
        let store = Store::open_in_memory().unwrap();
        // Off by default: the extractor-args are the plain preset.
        let off = load_ytdlp_bins(&store).sabr.extractor_args;
        assert_eq!(off, SABR_DEFAULT_EXTRACTOR_ARGS);
        assert!(!off.contains("enable_live_deep_rewind"));

        // Enabled: the deep-rewind key is appended under the youtube: namespace.
        store.set_setting("ytdlp_sabr_deep_rewind", "1").unwrap();
        let on = load_ytdlp_bins(&store).sabr.extractor_args;
        assert_eq!(
            on,
            format!("{SABR_DEFAULT_EXTRACTOR_ARGS};enable_live_deep_rewind=true")
        );

        // Explicit "0" is off again.
        store.set_setting("ytdlp_sabr_deep_rewind", "0").unwrap();
        assert!(
            !load_ytdlp_bins(&store)
                .sabr
                .extractor_args
                .contains("enable_live_deep_rewind")
        );

        // No double-append when the user already put it in the args field by hand.
        store.set_setting("ytdlp_sabr_deep_rewind", "1").unwrap();
        store
            .set_setting(
                "ytdlp_sabr_extractor_args",
                "youtube:formats=duplicate;enable_live_deep_rewind=true",
            )
            .unwrap();
        let manual = load_ytdlp_bins(&store).sabr.extractor_args;
        assert_eq!(manual.matches("enable_live_deep_rewind").count(), 1);
    }

    #[test]
    fn youtube_capture_from_start_uses_sabr_binary_and_mkv() {
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &sabr_bins(),
        );
        assert_eq!(plan.program, "C:/git/yt-dlp-dev/dist/yt-dlp.exe");
        // SABR writes the final MKV directly (no .ts intermediate, no remux), but
        // into the hidden .cache\ working dir; finalize promotes it to the output dir.
        assert!(plan.capture_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.capture_path.parent().unwrap().ends_with(".cache"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert_eq!(plan.final_path.parent().unwrap(), std::path::Path::new("C:/rec"));
        assert_ne!(plan.capture_path, plan.final_path);
        assert!(!plan.remux_to_mkv);
        assert!(!plan.writes_own_thumbnail);
        assert!(plan.args.iter().any(|a| a == "--live-from-start"));
        assert!(!plan.args.iter().any(|a| a == "--hls-use-mpegts"));
        assert!(plan.args.iter().any(|a| a == "-f"));
        assert!(plan.args.iter().any(|a| a == SABR_DEFAULT_FORMAT));
        assert!(plan.args.iter().any(|a| a == "--extractor-args"));
        assert!(plan.args.iter().any(|a| a == SABR_DEFAULT_EXTRACTOR_ARGS));
    }

    #[test]
    fn sabr_pot_args_added_as_separate_extractor_args() {
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &sabr_bins(),
        );
        // The PO-token provider args ride on their own --extractor-args entry,
        // distinct from the youtube: SABR args — so there are two of them.
        let xargs = plan.args.iter().filter(|a| *a == "--extractor-args").count();
        assert_eq!(xargs, 2);
        assert!(plan.args.iter().any(|a| a == SABR_DEFAULT_POT_ARGS));
        assert!(plan.args.iter().any(|a| a == SABR_DEFAULT_EXTRACTOR_ARGS));

        // Empty pot args ⇒ only the youtube: --extractor-args entry.
        let mut bins = sabr_bins();
        bins.sabr.pot_args = String::new();
        let plan2 = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert_eq!(plan2.args.iter().filter(|a| *a == "--extractor-args").count(), 1);
    }

    #[test]
    fn sabr_raw_args_override_replaces_preset() {
        let mut bins = sabr_bins();
        bins.sabr.raw_args = "-f custom+best --extractor-args youtube:foo".into();
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert!(plan.args.iter().any(|a| a == "custom+best"));
        assert!(plan.args.iter().any(|a| a == "youtube:foo"));
        // The preset format/extractor-args are NOT injected when raw args are set.
        assert!(!plan.args.iter().any(|a| a == SABR_DEFAULT_FORMAT));
        assert!(!plan.args.iter().any(|a| a == SABR_DEFAULT_EXTRACTOR_ARGS));
    }

    #[test]
    fn sabr_used_for_live_edge_when_enabled() {
        // Disabled SABR → normal system yt-dlp path (.ts + mpegts). This is the
        // only case that still uses the system build (no dev build configured);
        // YouTube live is unrecordable via the default clients, but that's the
        // pre-existing "SABR not set up" limitation.
        let mut bins = sabr_bins();
        bins.sabr.enabled = false;
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert_eq!(plan.program, "yt-dlp");
        assert!(plan.args.iter().any(|a| a == "--hls-use-mpegts"));

        // Enabled SABR + capture_from_start = false → SABR at the LIVE EDGE.
        // (YouTube live is SABR-only now; the old default-client "dash" path
        // returned "No video formats found" and crash-looped.)
        let mut r = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        r.monitor.capture_from_start = false;
        let plan2 = build_plan(
            &r, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &sabr_bins(),
        );
        assert_eq!(plan2.program, "C:/git/yt-dlp-dev/dist/yt-dlp.exe");
        assert!(plan2.args.iter().any(|a| a == "--no-live-from-start"));
        assert!(!plan2.args.iter().any(|a| a == "--live-from-start"));
        assert!(!plan2.args.iter().any(|a| a == "--hls-use-mpegts"));
        assert!(plan2.args.iter().any(|a| a == "-f"));
        assert!(plan2.args.iter().any(|a| a == SABR_DEFAULT_FORMAT));
        assert!(plan2.args.iter().any(|a| a == SABR_DEFAULT_EXTRACTOR_ARGS));
        // Direct-MKV (no .ts remux) at the edge, same as from-start SABR.
        assert!(plan2.capture_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan2.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(!plan2.remux_to_mkv);
    }

    #[test]
    fn explicit_system_binary_path_is_used() {
        let bins = YtDlpBins {
            system: "C:/tools/yt-dlp.exe".into(),
            ..Default::default()
        };
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert_eq!(plan.program, "C:/tools/yt-dlp.exe");
    }

    #[test]
    fn companion_suffix_recognizes_thumbnails_subs_and_chat() {
        assert!(is_companion_suffix("thumbnail.jpg"));
        assert!(is_companion_suffix("webp")); // yt-dlp --write-thumbnail
        assert!(is_companion_suffix("png"));
        assert!(is_companion_suffix("en.vtt"));
        assert!(is_companion_suffix("chat.jsonl"));
        assert!(is_companion_suffix("live_chat.json"));
        // The video itself and SABR working files are NOT companions.
        assert!(!is_companion_suffix("ts"));
        assert!(!is_companion_suffix("mkv"));
        assert!(!is_companion_suffix("f140.mkv.sq0.part"));
        assert!(!is_companion_suffix("f303.mkv.state"));
    }

    #[test]
    fn captures_route_into_hidden_cache_subdir() {
        // streamlink (MKV container): capture .ts under .cache\, final .mkv in dir.
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(plan.capture_path.parent().unwrap().ends_with(".cache"));
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert_eq!(plan.final_path.parent().unwrap(), std::path::Path::new("C:/rec"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));

        // Video (yt-dlp) also captures into .cache\, final .mkv in the output dir.
        let vplan = build_video_plan(
            &video(Tool::YtDlp, "https://youtu.be/abc"),
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::None,
            &[],
            None,
            &YtDlpBins::default(),
        );
        assert!(vplan.capture_path.parent().unwrap().ends_with(".cache"));
        assert_eq!(vplan.final_path.parent().unwrap(), std::path::Path::new("C:/vids"));
    }

    #[test]
    fn sabr_args_match_between_build_and_resume() {
        // A resume rebuilds the args from the same capture path; they must be
        // byte-identical so yt-dlp continues from the surviving `.state`.
        let bins = sabr_bins();
        let r = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        let plan = build_plan(&r, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins);
        let resume_args = sabr_capture_args(
            &plan.capture_path,
            &AuthSource::None,
            &[],
            &bins.sabr,
            &[],
            &r.monitor.url,
            r.monitor.capture_from_start,
            &resolve_sabr_sort(&r.monitor, &bins.sabr),
        );
        assert_eq!(plan.args, resume_args);
    }

    /// The value of a `-S` arg (the token after `-S`), or `None` if absent.
    fn sort_of(args: &[String]) -> Option<String> {
        args.iter().position(|a| a == "-S").map(|i| args[i + 1].clone())
    }

    #[test]
    fn sabr_codec_pref_injects_format_sort() {
        let bins = sabr_bins();

        // Default (Inherit → global Auto) emits no -S, so existing captures are
        // byte-identical.
        let auto = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        let plan = build_plan(&auto, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins);
        assert_eq!(sort_of(&plan.args), None);

        // A per-instance H.264 preference injects `-S res,fps,vcodec:h264`.
        let mut h264 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        h264.monitor.sabr_codec_pref = SabrCodecPref::H264;
        let plan = build_plan(&h264, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins);
        assert_eq!(sort_of(&plan.args).as_deref(), Some("res,fps,vcodec:h264"));

        // Inherit falls through to the global default — here Best quality → `br`.
        let mut inh = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        inh.monitor.sabr_codec_pref = SabrCodecPref::Inherit;
        let mut bins_best = sabr_bins();
        bins_best.sabr.codec_pref = SabrCodecPref::BestQuality;
        let plan = build_plan(&inh, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins_best);
        assert_eq!(sort_of(&plan.args).as_deref(), Some("res,fps,br"));

        // A per-instance override wins over the global default.
        let mut over = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        over.monitor.sabr_codec_pref = SabrCodecPref::Vp9;
        let plan = build_plan(&over, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins_best);
        assert_eq!(sort_of(&plan.args).as_deref(), Some("res,fps,vcodec:vp9"));
    }

    fn video(tool: Tool, url: &str) -> Video {
        Video {
            id: 1,
            url: url.into(),
            title: "Clip".into(),
            channel: String::new(),
            platform: Platform::detect(url),
            tool,
            quality: "best".into(),
            output_dir: "C:/vids".into(),
            filename_template: "{name}_{date}".into(),
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            audio_tracks: String::new(),
            subtitle_tracks: String::new(),
            chat_log: false,
            extra_args: String::new(),
            auto_title: false,
            status: "queued".into(),
            output_path: String::new(),
            bytes: 0,
            exit_code: None,
            log_excerpt: String::new(),
            created_at: 0,
            started_at: None,
            ended_at: None,
        }
    }

    #[test]
    fn ytdlp_video_outputs_mkv_directly() {
        let plan = build_video_plan(
            &video(Tool::YtDlp, "https://youtube.com/watch?v=abc"),
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::None,
            &[],
            None,
            &YtDlpBins::default(),
        );
        assert_eq!(plan.program, "yt-dlp");
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(!plan.remux_to_mkv); // yt-dlp produces the MKV itself
        // Not a live capture: no live-stream flags.
        assert!(!plan.args.iter().any(|a| a == "--live-from-start"));
        assert!(plan.args.iter().any(|a| a == "--remux-video"));
    }

    #[test]
    fn ytdlp_video_quality_and_cookies() {
        let mut v = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v.quality = "bv*+ba".into();
        let plan = build_video_plan(
            &v,
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::CookiesBrowser("edge".into()),
            &[],
            None,
            &sabr_bins(),
        );
        let joined = plan.args.join(" ");
        assert!(joined.contains("-f bv*+ba"));
        assert!(joined.contains("--cookies-from-browser edge"));
        // YouTube VOD media URLs 403 without a PO token — the provider args
        // must be present just like on live captures — and need a client mix
        // that still serves downloadable formats (mweb).
        assert!(joined.contains(SABR_DEFAULT_POT_ARGS));
        assert!(joined.contains("youtube:player_client=mweb"));
    }

    #[test]
    fn streamlink_vod_remuxes_to_mkv() {
        let plan = build_video_plan(
            &video(Tool::Streamlink, "https://twitch.tv/videos/123"),
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::None,
            &[],
            None,
            &YtDlpBins::default(),
        );
        assert_eq!(plan.program, "streamlink");
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.remux_to_mkv);
        // No live-only retry flags for a VOD.
        assert!(!plan.args.iter().any(|a| a == "--retry-streams"));
    }

    #[test]
    fn video_plan_track_and_chat_args() {
        // yt-dlp: subtitle + chat selection -> --sub-langs (incl. live_chat) + --write-subs.
        let mut v = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v.subtitle_tracks = "all".into();
        v.chat_log = true;
        let plan = build_video_plan(&v, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &YtDlpBins::default());
        let joined = plan.args.join(" ");
        assert!(joined.contains("--sub-langs=all,live_chat"), "{joined}");
        assert!(plan.args.iter().any(|a| a == "--write-subs"), "{joined}");
        // The URL stays last (track args were inserted before it, not after).
        assert_eq!(plan.args.last().map(String::as_str), Some(v.url.as_str()));

        // streamlink: audio-track selection -> --hls-audio-select (subtitles n/a).
        let mut s = video(Tool::Streamlink, "https://twitch.tv/videos/123");
        s.audio_tracks = "en,de".into();
        let plan = build_video_plan(&s, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &YtDlpBins::default());
        let joined = plan.args.join(" ");
        assert!(joined.contains("--hls-audio-select=en,de"), "{joined}");
    }

    #[test]
    fn ended_stream_is_not_a_failure() {
        // A concluded/upcoming YouTube live: yt-dlp has only thumbnail images left
        // (the exact stderr from the Layna YouTube take 3 bug).
        assert!(stream_ended_or_unavailable(
            "ERROR: [youtube] aEhxflmEYGA: Requested format is not available\n\
             WARNING: Only images are available for download. use --list-formats to see them"
        ));
        assert!(stream_ended_or_unavailable("This live event has ended."));
        assert!(stream_ended_or_unavailable("This live event will begin in 2 hours."));
        // streamlink: offline / ended channel.
        assert!(stream_ended_or_unavailable(
            "error: No playable streams found on this URL: https://twitch.tv/x"
        ));
        // A *real* failure must stay a failure (e.g. the Layna take 1: bad cookies),
        // and a generic error too.
        assert!(!stream_ended_or_unavailable(
            "WARNING: [youtube] The provided YouTube account cookies are no longer valid."
        ));
        assert!(!stream_ended_or_unavailable(
            "ERROR: unable to download video data: HTTP Error 403: Forbidden"
        ));
        assert!(!stream_ended_or_unavailable(""));
    }

    #[test]
    fn youtube_live_url_appends_live_to_channel_urls() {
        // Channel forms — must get /live appended.
        assert_eq!(youtube_live_url("https://www.youtube.com/@YUY_IX"), "https://www.youtube.com/@YUY_IX/live");
        assert_eq!(youtube_live_url("https://www.youtube.com/@YUY_IX/"), "https://www.youtube.com/@YUY_IX/live");
        assert_eq!(youtube_live_url("https://youtube.com/channel/UCabc123"), "https://youtube.com/channel/UCabc123/live");
        assert_eq!(youtube_live_url("https://youtube.com/c/SomeName"), "https://youtube.com/c/SomeName/live");
        assert_eq!(youtube_live_url("https://youtube.com/user/SomeName"), "https://youtube.com/user/SomeName/live");
        // Already has /live — unchanged.
        assert_eq!(youtube_live_url("https://www.youtube.com/@YUY_IX/live"), "https://www.youtube.com/@YUY_IX/live");
        // Specific video URLs — unchanged.
        assert_eq!(youtube_live_url("https://www.youtube.com/watch?v=abc123"), "https://www.youtube.com/watch?v=abc123");
        assert_eq!(youtube_live_url("https://youtu.be/abc123"), "https://youtu.be/abc123");
        assert_eq!(youtube_live_url("https://www.youtube.com/live/abc123"), "https://www.youtube.com/live/abc123");
    }
}
