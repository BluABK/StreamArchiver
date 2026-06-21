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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Semaphore, mpsc};
use tracing::{info, warn};

use crate::detectors::{DetectContext, DetectItem, DetectOutcome};
use crate::events::{AppEvent, EventTx, LiveSignal, ManualCommand};
use crate::models::{
    AuthKind, Container, DetectionMethod, K_FILENAME_MEDIA, MediaInfoMode, Monitor,
    MonitorWithChannel, Platform, Tool, Video, now_unix,
};
use crate::platform::ProcessJob;
use crate::store::Store;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

const RING_MAX_LINES: usize = 80;
/// A recording that fails faster than this is treated as a transient failure
/// and subject to backoff.
const SHORT_RUN_SECS: i64 = 15;
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
fn push_track_args(args: &mut Vec<String>, tool: Tool, audio: &str, subs: &str, chat: bool) {
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
            }
            if !langs.is_empty() {
                args.push(format!("--sub-langs={}", langs.join(",")));
                args.push("--write-subs".into());
            }
        }
        Tool::Ffmpeg => {}
    }
}

pub fn build_plan(
    row: &MonitorWithChannel,
    started_at: i64,
    auth: &AuthSource,
    stream_id: Option<&str>,
    media: Option<&MediaInfo>,
) -> DownloadPlan {
    let m = &row.monitor;
    let ch = &row.channel;
    let dir = PathBuf::from(&m.output_dir);
    let quality = resolved_quality(&m.quality);
    // `{video_id}` (platform id when known), `{take}` (attempt number), and the
    // media vars (filled only when `media` is provided, i.e. pre-probe). Then
    // avoid clobbering an existing finished file of the same name.
    // `{games}` isn't known until the stream ends; it's filled at the post-rename.
    let stem = monitor_stem(
        m, &ch.name, started_at, stream_id, row.recording_count, &quality, media, "",
    );
    let extra = split_args(&m.extra_args);
    let final_ext = match m.container {
        Container::Mkv => "mkv",
        Container::Ts => "ts",
    };
    let stem = unique_stem(&dir, &stem, final_ext, None);

    let ts_path = dir.join(format!("{stem}.ts"));
    let ts_str = ts_path.to_string_lossy().into_owned();
    let (final_path, remux_to_mkv) = match m.container {
        Container::Mkv => (dir.join(format!("{stem}.mkv")), true),
        Container::Ts => (ts_path.clone(), false),
    };

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
            args.push(ts_str);
            args.push(m.url.clone());
            args.push(quality);
            ("streamlink".to_string(), args)
        }
        Tool::YtDlp => {
            let mut args = vec![
                "--no-part".to_string(),
                "--hls-use-mpegts".into(), // progressive .ts output
                "-o".into(),
                ts_str,
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
            // YouTube chat goes through yt-dlp's live_chat; Twitch chat is logged
            // by the native WS logger instead, so don't ask yt-dlp for it there.
            let chat_subs = m.chat_log && m.platform() != Platform::Twitch;
            push_track_args(&mut args, Tool::YtDlp, &m.audio_tracks, &m.subtitle_tracks, chat_subs);
            args.extend(extra);
            args.push(m.url.clone());
            ("yt-dlp".to_string(), args)
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
            args.push(ts_str);
            ("ffmpeg".to_string(), args)
        }
    };

    DownloadPlan {
        program,
        args,
        capture_path: ts_path,
        final_path,
        remux_to_mkv,
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
    media: Option<&MediaInfo>,
) -> DownloadPlan {
    let dir = PathBuf::from(&v.output_dir);
    let quality = resolved_quality(&v.quality);
    let stem = video_stem(v, started_at, title, channel, video_id, &quality, media);
    let extra = split_args(&v.extra_args);
    let platform = Platform::detect(&v.url);
    // Don't clobber an existing finished file (all video tools end at .mkv).
    let stem = unique_stem(&dir, &stem, "mkv", None);
    let final_path = dir.join(format!("{stem}.mkv"));

    match v.tool {
        Tool::YtDlp => {
            // yt-dlp downloads the complete video and remuxes to MKV. `%(ext)s`
            // becomes `mkv` after the remux, so the final path is predictable.
            let out_tmpl = dir
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
                program: "yt-dlp".to_string(),
                args,
                // yt-dlp writes the final MKV directly; no separate capture/remux.
                capture_path: final_path.clone(),
                final_path,
                remux_to_mkv: false,
            }
        }
        Tool::Streamlink => {
            let ts_path = dir.join(format!("{stem}.ts"));
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
            }
        }
        Tool::Ffmpeg => {
            let ts_path = dir.join(format!("{stem}.ts"));
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
            }
        }
    }
}

#[derive(Clone)]
pub struct Supervisor {
    store: Arc<Store>,
    events: EventTx,
    active: ActiveSet,
    /// video_id -> child PID of in-flight on-demand video downloads.
    active_videos: ActiveSet,
    /// video_id -> live download progress fraction, for the UI bar.
    video_progress: VideoProgress,
    /// video_id -> live download speed (bytes/sec), for the UI Speed column.
    video_speed: VideoSpeed,
    /// video_ids whose download was asked to stop (so it finalizes as `stopped`).
    stopping_videos: Arc<Mutex<HashSet<i64>>>,
    shutdown: Arc<AtomicBool>,
    /// Shared detection context for on-demand (manual Start) liveness checks.
    ctx: Arc<DetectContext>,
    /// monitor_id -> unix time the current ad break ends (for the UI row tint).
    ad_active: AdActive,
    sem: Arc<Semaphore>,
    backoff: Arc<Mutex<HashMap<i64, BackoffEntry>>>,
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
        shutdown: Arc<AtomicBool>,
        ctx: Arc<DetectContext>,
        ad_active: AdActive,
        max_concurrent: usize,
    ) -> Supervisor {
        Supervisor {
            store,
            events,
            active,
            active_videos,
            video_progress,
            video_speed,
            stopping_videos: Arc::new(Mutex::new(HashSet::new())),
            shutdown,
            ctx,
            ad_active,
            sem: Arc::new(Semaphore::new(max_concurrent.max(1))),
            backoff: Arc::new(Mutex::new(HashMap::new())),
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
                    self.try_begin(signal.monitor_id, signal.went_live_at, signal.approximate, signal.stream_id, false);
                }
                Some(cmd) = manual_rx.recv() => match cmd {
                    ManualCommand::Start(id) => {
                        let this = self.clone();
                        tokio::spawn(async move { this.manual_start(id).await });
                    }
                    ManualCommand::Stop(id) => self.manual_stop(id),
                    ManualCommand::StartVideo(id) => {
                        if !self.shutdown.load(Ordering::SeqCst) {
                            let this = self.clone();
                            tokio::spawn(async move { this.start_video(id).await });
                        }
                    }
                    ManualCommand::StopVideo(id) => self.stop_video(id),
                },
                else => break,
            }
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
        let this = self.clone();
        tokio::spawn(async move {
            this.record(row, went_live_at, approximate, stream_id).await;
        });
        true
    }

    /// Manual "Start": check the channel now and record if live.
    async fn manual_start(&self, monitor_id: i64) {
        if self.active.lock().unwrap().contains_key(&monitor_id) {
            return; // already recording
        }
        let row = match self.store.get_monitor_with_channel(monitor_id) {
            Ok(Some(r)) => r,
            _ => return,
        };
        let name = row.channel.name.clone();
        let outcome = self.check_one(&row).await;
        if outcome.live {
            let (went, approx) = match outcome.went_live_at {
                Some(t) => (Some(t), false),
                None => (Some(now_unix()), true),
            };
            self.try_begin(monitor_id, went, approx, outcome.stream_id, true);
        } else {
            let message = if outcome.error && !outcome.detail.is_empty() {
                format!("{name}: {}", outcome.detail)
            } else {
                format!("{name} is not live")
            };
            let _ = self.events.send(AppEvent::Error {
                context: "Start".into(),
                message,
            });
        }
    }

    /// Manual "Stop": abort the active recording and apply a short cooldown so it
    /// doesn't immediately restart on the next poll.
    fn manual_stop(&self, monitor_id: i64) {
        let pid = self.active.lock().unwrap().get(&monitor_id).copied();
        if let Some(pid) = pid {
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
        let (title, channel, video_id) = if video.auto_title {
            resolve_meta(&video, &auth).await
        } else {
            (String::new(), String::new(), String::new())
        };
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
        let plan =
            build_video_plan(&video, started_at, &title, &channel, &video_id, &auth, pre_media.as_ref());
        if let Some(parent) = plan.capture_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
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
            )
            .await;

        // Remux TS -> MKV (streamlink/ffmpeg); yt-dlp already produced the MKV.
        let mut final_path = plan.final_path.clone();
        if plan.remux_to_mkv {
            if file_len(&plan.capture_path).await > 0 {
                match remux_ts_to_mkv(&plan.capture_path, &plan.final_path).await {
                    Ok(()) => {
                        let _ = tokio::fs::remove_file(&plan.capture_path).await;
                    }
                    Err(e) => {
                        warn!(video = id, "remux failed, keeping .ts: {e:#}");
                        final_path = plan.capture_path.clone();
                    }
                }
            } else {
                final_path = plan.capture_path.clone();
            }
        } else if file_len(&final_path).await == 0 {
            // yt-dlp may have produced a different extension than predicted.
            if let Some(found) = newest_with_stem(&final_path).await {
                final_path = found;
            }
        }

        // Post-capture: probe the finished file for actual media info and rename.
        if want_media && media_mode.post() && file_len(&final_path).await > 0 {
            if let Some(mi) = probe_media(&final_path.to_string_lossy()).await {
                let quality = resolved_quality(&video.quality);
                let stem =
                    video_stem(&video, started_at, &title, &channel, &video_id, &quality, Some(&mi));
                final_path = rename_for_media(final_path, &stem).await;
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
        if ok || duration_secs >= SHORT_RUN_SECS {
            map.remove(&monitor_id);
        } else {
            let entry = map.entry(monitor_id).or_insert(BackoffEntry {
                fails: 0,
                until: Instant::now(),
            });
            entry.fails = entry.fails.saturating_add(1);
            let wait = (30u64 * entry.fails as u64).min(600);
            entry.until = Instant::now() + Duration::from_secs(wait);
            warn!(
                monitor_id,
                fails = entry.fails,
                wait,
                "recording failed quickly; backing off"
            );
        }
    }

    async fn record(
        &self,
        row: MonitorWithChannel,
        went_live_at: Option<i64>,
        approximate: bool,
        stream_id: Option<String>,
    ) {
        let monitor_id = row.monitor.id;
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
        let started_at = now_unix();
        let plan = build_plan(&row, started_at, &auth, stream_id.as_deref(), pre_media.as_ref());
        if let Some(parent) = plan.capture_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let rec_id = self
            .store
            .insert_recording(
                monitor_id,
                started_at,
                &plan.final_path.to_string_lossy(),
                went_live_at,
                approximate,
                stream_id.as_deref(),
            )
            .unwrap_or(0);
        let _ = self
            .store
            .set_monitor_check_result(monitor_id, "recording", started_at);
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "recording".into(),
        });
        let _ = self.events.send(AppEvent::RecordingStarted {
            monitor_id,
            recording_id: rec_id,
            channel: row.channel.name.clone(),
        });
        info!(monitor_id, program = %plan.program, "starting recording -> {}", plan.final_path.display());

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
        // a `.chat.jsonl` sidecar next to the capture (so it follows the file's
        // stem, incl. the media rename). Twitch only; YouTube chat is captured by
        // yt-dlp (live_chat) via build_plan.
        let chat_done = Arc::new(AtomicBool::new(false));
        let chat_task = (row.monitor.chat_log && row.monitor.platform() == Platform::Twitch)
            .then(|| {
                let chat_path = plan.capture_path.with_extension("chat.jsonl");
                tokio::spawn(crate::chat::log_twitch_chat(
                    row.monitor.url.clone(),
                    chat_path,
                    chat_done.clone(),
                    self.shutdown.clone(),
                ))
            });

        let outcome = self
            .run_process(&self.active, monitor_id, &plan, None, None, ad_sink)
            .await;

        // Stop the catch-up watcher before we touch the capture file (so it can't
        // race finalize's authoritative lost-time write).
        watcher_done.store(true, Ordering::SeqCst);
        if let Some(w) = watcher {
            let _ = w.await;
        }
        // Stop the metadata watcher too (it only writes its own table, but join it
        // so a final in-flight poll can't insert after the recording is finalized).
        meta_done.store(true, Ordering::SeqCst);
        if let Some(t) = meta_task {
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

        // Remux TS -> MKV if requested and we captured something.
        let mut final_path = plan.final_path.clone();
        if plan.remux_to_mkv {
            if file_len(&plan.capture_path).await > 0 {
                match remux_ts_to_mkv(&plan.capture_path, &plan.final_path).await {
                    Ok(()) => {
                        let _ = tokio::fs::remove_file(&plan.capture_path).await;
                    }
                    Err(e) => {
                        warn!(monitor_id, "remux failed, keeping .ts: {e:#}");
                        final_path = plan.capture_path.clone();
                    }
                }
            } else {
                final_path = plan.capture_path.clone();
            }
        }

        // Post-capture: fill in the filename bits that are only known now and
        // rename. `{resolution}/{fps}/…` come from probing the finished file (the
        // post-rename media modes); `{games}` is the list of categories played,
        // and triggers a rename even when media probing is off.
        let want_games = template_wants_games(&row.monitor.filename_template);
        let do_post_media = want_media && media_mode.post() && file_len(&final_path).await > 0;
        if do_post_media || want_games {
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
            // Only rename when we actually resolved something to substitute in.
            if mi.is_some() || !games.is_empty() {
                let quality = resolved_quality(&row.monitor.quality);
                // Prefer the post-probe; fall back to the pre-probe so a {games}
                // rename in pre-probe mode doesn't drop already-resolved media vars.
                let stem = monitor_stem(
                    &row.monitor,
                    &row.channel.name,
                    started_at,
                    stream_id.as_deref(),
                    row.recording_count,
                    &quality,
                    mi.as_ref().or(pre_media.as_ref()),
                    &games,
                );
                final_path = rename_for_media(final_path, &stem).await;
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
        // A 0-byte capture isn't always a failure: a livestream that had already
        // ended (or hadn't started, or exposed no live video formats) leaves
        // nothing to capture but isn't an error. Classify those as `ended` so they
        // don't show as red failures. (`ok` still drives backoff, so we don't
        // hammer an ended broadcast.)
        let status = if ok {
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
        info!(monitor_id, bytes, status, "recording finished");

        self.note_result(monitor_id, duration, ok);
        self.active.lock().unwrap().remove(&monitor_id);
        self.ad_active.lock().unwrap().remove(&monitor_id);
    }

    async fn run_process(
        &self,
        active: &ActiveSet,
        id: i64,
        plan: &DownloadPlan,
        progress: Option<VideoProgress>,
        speed: Option<VideoSpeed>,
        ads: Option<AdSink>,
    ) -> ProcessOutcome {
        let job = match ProcessJob::new() {
            Ok(j) => Some(j),
            Err(e) => {
                warn!("job object create failed: {e:#}");
                None
            }
        };
        let mut cmd = Command::new(&plan.program);
        cmd.args(&plan.args)
            .stdin(Stdio::null())
            // Capture stdout only when we want to parse progress/speed (yt-dlp
            // prints the progress line there); otherwise discard it.
            .stdout(if progress.is_some() || speed.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .kill_on_drop(true);
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
        if let Some(pid) = child.id() {
            active.lock().unwrap().insert(id, pid);
        }

        // Parse progress + speed lines from stdout (yt-dlp `--progress-template`).
        if let Some(stdout) = child.stdout.take() {
            let prog = progress.clone();
            let spd = speed.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let (f, s) = parse_progress_fields(&line);
                    if let (Some(f), Some(m)) = (f, prog.as_ref()) {
                        m.lock().unwrap().insert(id, f);
                    }
                    if let (Some(s), Some(m)) = (s, spd.as_ref()) {
                        m.lock().unwrap().insert(id, s);
                    }
                }
            });
        }

        // Ad breaks are handled off the stderr-drain path: the drain loop just
        // forwards `(detected_at, duration)` over this channel, and a dedicated
        // task does the (potentially slow) ffprobe + DB insert. This keeps stderr
        // consumption from stalling — a blocked drain can backpressure the capture
        // process's own stderr writes.
        let (ad_tx, ad_rx) = if ads.is_some() {
            let (tx, rx) = mpsc::unbounded_channel::<(i64, i64)>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let ring: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_task = if let Some(stderr) = child.stderr.take() {
            let ring = ring.clone();
            let program = plan.program.clone();
            let prog = progress.clone();
            let spd = speed.clone();
            // Move the sole ad-break sender in; the channel closes when the drain
            // loop ends (EOF on child exit), ending the processor task.
            let ad_tx = ad_tx;
            Some(tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    // Some tools emit progress on stderr; catch it there too.
                    if prog.is_some() || spd.is_some() {
                        let (f, s) = parse_progress_fields(&line);
                        if let (Some(f), Some(m)) = (f, prog.as_ref()) {
                            m.lock().unwrap().insert(id, f);
                        }
                        if let (Some(s), Some(m)) = (s, spd.as_ref()) {
                            m.lock().unwrap().insert(id, s);
                        }
                    }
                    // Forward streamlink ad breaks without blocking the drain
                    // (streamlink de-dups, so each matching line is one break).
                    if let Some(tx) = &ad_tx {
                        if let Some(dur) = parse_ad_break_secs(&line) {
                            let _ = tx.send((now_unix(), dur));
                        }
                    }
                    tracing::trace!(target: "streamarchiver::recproc", "[{program}] {line}");
                    let mut r = ring.lock().unwrap();
                    if r.len() >= RING_MAX_LINES {
                        r.pop_front();
                    }
                    r.push_back(line);
                }
            }))
        } else {
            drop(ad_tx); // no stderr piped: close the channel so the processor ends
            None
        };

        // Ad-break processor: for each break, the cut lands at the content captured
        // so far. Ad segments are filtered out, so the capture's media duration is
        // that position — correct for both live-edge and capture-from-start/DVR.
        // Falls back to wall clock minus already-skipped ad time if ffprobe can't
        // read the still-growing file yet.
        let ad_task = match (ads, ad_rx) {
            (Some(sink), Some(mut rx)) => Some(tokio::spawn(async move {
                let mut prior_ad_secs: i64 = 0;
                let mut last_at: i64 = 0;
                while let Some((detected_at, dur)) = rx.recv().await {
                    // Prefer the actual captured-content position (one quick retry,
                    // since a just-written .ts can momentarily lack a readable
                    // duration). Fall back to wall clock minus already-skipped ad
                    // time, anchored at go-live for DVR takes (whose file timeline
                    // starts at go-live) and at recording start at the live edge.
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
            })),
            _ => None,
        };

        let status = child.wait().await;
        // Drain both reader tasks fully before returning, so every log line and ad
        // break is recorded before the caller remuxes/removes the capture file.
        if let Some(t) = stderr_task {
            let _ = t.await;
        }
        if let Some(t) = ad_task {
            let _ = t.await;
        }
        // Closing the job here terminates any stragglers (e.g. yt-dlp's ffmpeg).
        if let Some(j) = &job {
            j.kill();
        }
        drop(job);

        let exit_code = status.ok().and_then(|s| s.code()).map(|c| c as i64);
        let log = {
            let r = ring.lock().unwrap();
            r.iter().cloned().collect::<Vec<_>>().join("\n")
        };
        ProcessOutcome { exit_code, log }
    }
}

struct ProcessOutcome {
    exit_code: Option<i64>,
    log: String,
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

pub async fn remux_ts_to_mkv(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i")
        .arg(src)
        // Keep EVERY video/audio/subtitle stream, not just ffmpeg's default
        // one-per-type — otherwise the extra audio tracks captured via
        // `--hls-audio-select=*` would be dropped here. Map by type (each `?`
        // optional) rather than `-map 0` so TS data streams (e.g. timed-ID3),
        // which MKV can't hold, don't fail the remux.
        .arg("-map")
        .arg("0:v?")
        .arg("-map")
        .arg("0:a?")
        .arg("-map")
        .arg("0:s?")
        .arg("-c")
        .arg("copy")
        .arg(dst)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let status = cmd.status().await?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("ffmpeg remux exited with {:?}", status.code())
    }
}

async fn file_len(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0)
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

/// Actual media properties of a capture, for the filename `{resolution}`/
/// `{height}`/`{width}`/`{fps}`/`{vcodec}` variables. Empty fields render empty.
#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub resolution: String, // "1920x1080"
    pub width: String,
    pub height: String,
    pub fps: String,    // rounded whole number, e.g. "60"
    pub vcodec: String, // e.g. "h264"
}

/// True if `template` uses any media-info variable (so we only probe when needed).
fn template_wants_media(template: &str) -> bool {
    ["{resolution}", "{height}", "{width}", "{fps}", "{vcodec}"]
        .iter()
        .any(|k| template.contains(k))
}

/// True if `template` uses `{games}` (only known after the stream ends, so it
/// triggers a post-capture rename even when media probing is off).
fn template_wants_games(template: &str) -> bool {
    template.contains("{games}")
}

/// Max length of the expanded `{games}` value, to keep paths sane.
const GAMES_MAX_LEN: usize = 100;

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

/// ffprobe the first video stream of a file path or stream URL into [`MediaInfo`].
/// `None` if ffprobe fails / there's no readable video stream.
async fn probe_media(target: &str) -> Option<MediaInfo> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v", "error",
        "-select_streams", "v:0",
        "-show_entries", "stream=width,height,r_frame_rate,codec_name",
        "-of", "default=noprint_wrappers=1:nokey=0",
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
    let mut info = MediaInfo::default();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            match k.trim() {
                "width" => info.width = v.to_string(),
                "height" => info.height = v.to_string(),
                "codec_name" => info.vcodec = v.to_string(),
                "r_frame_rate" => info.fps = fmt_fps(v),
                _ => {}
            }
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
    match tokio::fs::rename(&final_path, &new_path).await {
        Ok(()) => {
            // Move subtitle / chat sidecars (e.g. `{stem}.en.vtt`,
            // `{stem}.chat.jsonl`, `{stem}.live_chat.json`) so they stay matched
            // to the renamed video instead of orphaning under the old stem.
            if let Some(old) = old_stem {
                rename_companion_sidecars(&dir, &old, &unique).await;
            }
            new_path
        }
        Err(e) => {
            warn!("media rename failed, keeping {}: {e:#}", final_path.display());
            final_path
        }
    }
}

/// Subtitle-sidecar extensions, for companion-file moves when the video is
/// renamed (so external subs stay associated with their recording).
const SUBTITLE_EXTS: [&str; 6] = ["vtt", "srt", "ass", "ssa", "sub", "lrc"];

/// True if `rest` (the part of a sibling filename after `{old_stem}.`) is a
/// recognized companion: a subtitle sidecar (by final extension) or a chat log
/// (`.chat.jsonl` from the Twitch logger, `.live_chat.json` from yt-dlp).
fn is_companion_suffix(rest: &str) -> bool {
    if rest.ends_with("chat.jsonl") || rest.ends_with("live_chat.json") {
        return true;
    }
    Path::new(rest)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| SUBTITLE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
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
        if let Err(e) = tokio::fs::rename(entry.path(), &to).await {
            warn!("companion sidecar rename failed for {}: {e:#}", name);
        }
    }
}

/// The configured quality selector with the `best` default applied.
fn resolved_quality(q: &str) -> String {
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
    recording_count: i64,
    quality: &str,
    media: Option<&MediaInfo>,
    games: &str,
) -> String {
    let take = (recording_count + 1).to_string();
    let mi = media.cloned().unwrap_or_default();
    expand_template(
        &m.filename_template,
        &TemplateVars {
            name: ch_name,
            video_id: stream_id.unwrap_or(""),
            quality,
            take: &take,
            games,
            resolution: &mi.resolution,
            height: &mi.height,
            width: &mi.width,
            fps: &mi.fps,
            vcodec: &mi.vcodec,
            secs: started_at,
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

/// Minimal whitespace arg splitter (double-quoted segments kept together).
fn split_args(s: &str) -> Vec<String> {
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
    /// Attempt number (per-monitor take count); empty for on-demand videos.
    pub take: &'a str,
    /// Distinct game/category names played during the recording, joined + length-
    /// capped. Only known after the stream ends, so it's filled at the post-rename
    /// (empty for the initial capture name and for on-demand videos).
    pub games: &'a str,
    /// Capture-start time (unix secs) for `{date}`/`{time}`/`{timestamp}`.
    pub secs: i64,
}

/// Expand a filename template using our own (tool-agnostic) variables so the
/// output path is known in advance: `{name} {title} {channel} {video_id}
/// {quality} {resolution} {height} {width} {fps} {vcodec} {take} {games} {date}
/// {time} {timestamp}`.
fn expand_template(template: &str, v: &TemplateVars) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix_utc(v.secs);
    let date = format!("{y:04}{mo:02}{d:02}");
    let time = format!("{h:02}{mi:02}{s:02}");
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
        .replace("{take}", v.take)
        .replace("{games}", v.games)
        .replace("{date}", &date)
        .replace("{time}", &time)
        .replace("{timestamp}", &v.secs.to_string());
    let cleaned = sanitize_filename(&expanded);
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
fn unique_stem(dir: &Path, stem: &str, ext: &str, ignore: Option<&Path>) -> String {
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

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
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
                ad_free: false,
                auth_kind: AuthKind::Inherit,
                auth_value: String::new(),
                audio_tracks: String::new(),
                subtitle_tracks: String::new(),
                chat_log: false,
                extra_args: String::new(),
                max_concurrent: 1,
                last_checked_at: None,
                last_state: "idle".into(),
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
        );
        let joined = browser.args.join(" ");
        assert!(joined.contains("--cookies-from-browser firefox"));

        let file = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::CookiesFile("C:/c.txt".into()),
            None,
            None,
        );
        assert!(file.args.join(" ").contains("--cookies C:/c.txt"));
    }

    #[test]
    fn audio_subtitle_track_selection() {
        // streamlink: "all"/"*" -> --hls-audio-select=*; a list passes through.
        let mut r = row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        r.monitor.audio_tracks = "all".into();
        r.monitor.subtitle_tracks = "all".into(); // ignored by streamlink
        let plan = build_plan(&r, 1_700_000_000, &AuthSource::None, None, None);
        assert!(plan.args.iter().any(|a| a == "--hls-audio-select=*"));
        assert!(!plan.args.iter().any(|a| a == "--sub-langs"));

        let mut r2 = row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        r2.monitor.audio_tracks = "en,de".into();
        let plan2 = build_plan(&r2, 1_700_000_000, &AuthSource::None, None, None);
        assert!(plan2.args.iter().any(|a| a == "--hls-audio-select=en,de"));

        // yt-dlp: "all" subs -> --sub-langs=all --write-subs; audio ignored. The
        // `--flag=value` form keeps a value from being mis-parsed as an option.
        let mut y = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y.monitor.subtitle_tracks = "all".into();
        y.monitor.audio_tracks = "all".into(); // ignored by yt-dlp
        let yplan = build_plan(&y, 1_700_000_000, &AuthSource::None, None, None);
        assert!(yplan.args.iter().any(|a| a == "--sub-langs=all"));
        assert!(yplan.args.iter().any(|a| a == "--write-subs"));
        assert!(!yplan.args.iter().any(|a| a == "--hls-audio-select=*"));

        // "*" is normalized to "all" for subtitles too (matches the audio path),
        // so it never reaches yt-dlp as the invalid regex `--sub-langs *`.
        let mut y2 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y2.monitor.subtitle_tracks = "*".into();
        let yplan2 = build_plan(&y2, 1_700_000_000, &AuthSource::None, None, None);
        assert!(yplan2.args.iter().any(|a| a == "--sub-langs=all"));

        // A language list passes through verbatim (joined form).
        let mut y3 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y3.monitor.subtitle_tracks = "en,de".into();
        let yplan3 = build_plan(&y3, 1_700_000_000, &AuthSource::None, None, None);
        assert!(yplan3.args.iter().any(|a| a == "--sub-langs=en,de"));

        // Empty (existing-monitor default) adds no track flags at all.
        let plain = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            None,
            None,
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
        // YouTube + yt-dlp + chat_log -> --sub-langs includes live_chat.
        let mut y = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y.monitor.chat_log = true;
        y.monitor.subtitle_tracks = String::new();
        let plan = build_plan(&y, 1_700_000_000, &AuthSource::None, None, None);
        assert!(plan.args.iter().any(|a| a == "--sub-langs=live_chat"));
        assert!(plan.args.iter().any(|a| a == "--write-subs"));

        // Folded together with an explicit subtitle selection.
        let mut y2 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y2.monitor.chat_log = true;
        y2.monitor.subtitle_tracks = "en".into();
        let plan2 = build_plan(&y2, 1_700_000_000, &AuthSource::None, None, None);
        assert!(plan2.args.iter().any(|a| a == "--sub-langs=en,live_chat"));

        // Twitch + yt-dlp + chat_log -> NO yt-dlp live_chat (the native Twitch
        // chat logger handles it instead).
        let mut t = row(Tool::YtDlp, Container::Mkv, Platform::Twitch);
        t.monitor.chat_log = true;
        t.monitor.subtitle_tracks = String::new();
        let plant = build_plan(&t, 1_700_000_000, &AuthSource::None, None, None);
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
            None,
            None,
        );
        assert!(plan.final_path.to_string_lossy().ends_with(".ts"));
        assert!(!plan.remux_to_mkv);
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
            None,
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
            None,
        );
        let joined = plan.args.join(" ");
        assert!(joined.contains("-f bv*+ba"));
        assert!(joined.contains("--cookies-from-browser edge"));
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
            None,
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
        let plan = build_video_plan(&v, 1_700_000_000, "", "", "", &AuthSource::None, None);
        let joined = plan.args.join(" ");
        assert!(joined.contains("--sub-langs=all,live_chat"), "{joined}");
        assert!(plan.args.iter().any(|a| a == "--write-subs"), "{joined}");
        // The URL stays last (track args were inserted before it, not after).
        assert_eq!(plan.args.last().map(String::as_str), Some(v.url.as_str()));

        // streamlink: audio-track selection -> --hls-audio-select (subtitles n/a).
        let mut s = video(Tool::Streamlink, "https://twitch.tv/videos/123");
        s.audio_tracks = "en,de".into();
        let plan = build_video_plan(&s, 1_700_000_000, "", "", "", &AuthSource::None, None);
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
}
