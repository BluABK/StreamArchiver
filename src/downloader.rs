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
    AuthKind, Container, DetectionMethod, MonitorWithChannel, Platform, Tool, Video, now_unix,
};
use crate::platform::ProcessJob;
use crate::store::Store;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Monitors currently being recorded, mapped to their child PID (0 until the
/// process has spawned). Shared with the scheduler (so it doesn't re-trigger an
/// active recording) and used at shutdown to kill the process trees.
pub type ActiveSet = Arc<Mutex<HashMap<i64, u32>>>;

const RING_MAX_LINES: usize = 80;
/// A recording that fails faster than this is treated as a transient failure
/// and subject to backoff.
const SHORT_RUN_SECS: i64 = 15;

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
pub fn build_plan(row: &MonitorWithChannel, started_at: i64, auth: &AuthSource) -> DownloadPlan {
    let m = &row.monitor;
    let ch = &row.channel;
    let dir = PathBuf::from(&m.output_dir);
    let stem = expand_template(&m.filename_template, &ch.name, "", started_at);
    let quality = if m.quality.trim().is_empty() {
        "best".to_string()
    } else {
        m.quality.clone()
    };
    let extra = split_args(&m.extra_args);

    let ts_path = dir.join(format!("{stem}.ts"));
    let ts_str = ts_path.to_string_lossy().into_owned();
    let (final_path, remux_to_mkv) = match m.container {
        Container::Mkv => (dir.join(format!("{stem}.mkv")), true),
        Container::Ts => (ts_path.clone(), false),
    };

    let (program, args) = match m.tool {
        Tool::Streamlink => {
            let mut args = Vec::new();
            if ch.platform == Platform::Twitch {
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
            args.extend(extra);
            args.push("-o".into());
            args.push(ts_str);
            args.push(ch.url.clone());
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
            args.extend(extra);
            args.push(ch.url.clone());
            ("yt-dlp".to_string(), args)
        }
        Tool::Ffmpeg => {
            let mut args = vec![
                "-y".to_string(),
                "-i".into(),
                ch.url.clone(),
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
    auth: &AuthSource,
) -> DownloadPlan {
    let dir = PathBuf::from(&v.output_dir);
    // `{name}` prefers the user's Name field, then the resolved title, then a
    // generic fallback. `{title}` is the resolved title (may be empty).
    let name_field = v.title.trim();
    let resolved = title.trim();
    let name = if !name_field.is_empty() {
        name_field
    } else if !resolved.is_empty() {
        resolved
    } else {
        "video"
    };
    let stem = expand_template(&v.filename_template, name, resolved, started_at);
    let quality = if v.quality.trim().is_empty() {
        "best".to_string()
    } else {
        v.quality.trim().to_string()
    };
    let extra = split_args(&v.extra_args);
    let platform = Platform::detect(&v.url);
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
    /// video_ids whose download was asked to stop (so it finalizes as `stopped`).
    stopping_videos: Arc<Mutex<HashSet<i64>>>,
    shutdown: Arc<AtomicBool>,
    /// Shared detection context for on-demand (manual Start) liveness checks.
    ctx: Arc<DetectContext>,
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
        shutdown: Arc<AtomicBool>,
        ctx: Arc<DetectContext>,
        max_concurrent: usize,
    ) -> Supervisor {
        Supervisor {
            store,
            events,
            active,
            active_videos,
            stopping_videos: Arc::new(Mutex::new(HashSet::new())),
            shutdown,
            ctx,
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
                    self.try_begin(signal.monitor_id, signal.went_live_at, signal.approximate, false);
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
            this.record(row, went_live_at, approximate).await;
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
            self.try_begin(monitor_id, went, approx, true);
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
        // Optionally resolve the real stream/video title (for {title}/{name} and
        // the list display).
        let title = if video.auto_title {
            resolve_title(&video, &auth).await
        } else {
            String::new()
        };
        if !title.is_empty() && video.title.trim().is_empty() {
            let _ = self.store.set_video_title(id, &title);
        }
        let plan = build_video_plan(&video, started_at, &title, &auth);
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

        let outcome = self.run_process(&self.active_videos, id, &plan).await;

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
            url: row.channel.url.clone(),
            platform: row.channel.platform,
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

    async fn record(&self, row: MonitorWithChannel, went_live_at: Option<i64>, approximate: bool) {
        let monitor_id = row.monitor.id;
        let _permit = self.sem.acquire().await.expect("semaphore");

        let started_at = now_unix();
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
        let plan = build_plan(&row, started_at, &auth);
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

        let outcome = self.run_process(&self.active, monitor_id, &plan).await;

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

        let bytes = file_len(&final_path).await as i64;
        let duration = now_unix() - started_at;
        let ok = bytes > 0;
        let status = if ok { "completed" } else { "failed" };
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
    }

    async fn run_process(
        &self,
        active: &ActiveSet,
        id: i64,
        plan: &DownloadPlan,
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
            .stdout(Stdio::null())
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

        let ring: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr.take() {
            let ring = ring.clone();
            let program = plan.program.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::trace!(target: "streamarchiver::recproc", "[{program}] {line}");
                    let mut r = ring.lock().unwrap();
                    if r.len() >= RING_MAX_LINES {
                        r.pop_front();
                    }
                    r.push_back(line);
                }
            });
        }

        let status = child.wait().await;
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

/// Resolve a video/stream's real title via yt-dlp (no download). Works for
/// YouTube, Twitch VODs, Kick, and many sites; returns an empty string on any
/// failure (caller falls back to the Name field or "video"). Truncated to keep
/// filenames sane.
async fn resolve_title(video: &Video, auth: &AuthSource) -> String {
    let mut args: Vec<String> = vec![
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--skip-download".into(),
        "--print".into(),
        "%(title)s".into(),
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
        _ => return String::new(),
    };
    let raw = String::from_utf8_lossy(&out.stdout);
    let title = raw.lines().next().unwrap_or("").trim();
    if title.is_empty() || title == "NA" {
        String::new()
    } else {
        title.chars().take(120).collect()
    }
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

/// Expand a filename template using our own (tool-agnostic) variables so the
/// output path is known in advance: `{name} {title} {date} {time} {timestamp}`.
/// `title` is the resolved stream/video title (empty when not auto-detected).
fn expand_template(template: &str, name: &str, title: &str, secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix_utc(secs);
    let date = format!("{y:04}{mo:02}{d:02}");
    let time = format!("{h:02}{mi:02}{s:02}");
    let tmpl = if template.trim().is_empty() {
        "{name}_{date}_{time}"
    } else {
        template
    };
    let expanded = tmpl
        .replace("{name}", name)
        .replace("{title}", title)
        .replace("{date}", &date)
        .replace("{time}", &time)
        .replace("{timestamp}", &secs.to_string());
    let cleaned = sanitize_filename(&expanded);
    if cleaned.is_empty() {
        format!("{}_{date}_{time}", sanitize_filename(name))
    } else {
        cleaned
    }
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
        MonitorWithChannel {
            channel: Channel {
                id: 1,
                name: "Cool Streamer".into(),
                url: "https://twitch.tv/cool".into(),
                platform,
                created_at: 0,
            },
            monitor: Monitor {
                id: 7,
                channel_id: 1,
                enabled: true,
                tool,
                detection_method: DetectionMethod::TwitchApi,
                poll_interval_secs: 60,
                quality: "best".into(),
                output_dir: "C:/rec".into(),
                filename_template: "{name}_{date}_{time}".into(),
                container,
                capture_from_start: true,
                auth_kind: AuthKind::Inherit,
                auth_value: String::new(),
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
        }
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
        let name = expand_template("{name}_{date}", "Bad/Name?", "", 1_700_000_000);
        assert_eq!(name, "Bad_Name__20231114");
    }

    #[test]
    fn template_expands_title() {
        let out = expand_template("{title}_{date}", "ignored", "My Stream!", 1_700_000_000);
        assert_eq!(out, "My Stream!_20231114");
        // {name} falls back to the resolved title via build_video_plan, but
        // expand_template itself keeps name and title distinct.
        let out2 = expand_template("{name}-{title}", "Nm", "Ttl", 1_700_000_000);
        assert_eq!(out2, "Nm-Ttl");
    }

    #[test]
    fn streamlink_mkv_records_ts_then_remuxes() {
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
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
        );
        let joined = browser.args.join(" ");
        assert!(joined.contains("--cookies-from-browser firefox"));

        let file = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::CookiesFile("C:/c.txt".into()),
        );
        assert!(file.args.join(" ").contains("--cookies C:/c.txt"));
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
        );
        assert!(plan.final_path.to_string_lossy().ends_with(".ts"));
        assert!(!plan.remux_to_mkv);
    }

    fn video(tool: Tool, url: &str) -> Video {
        Video {
            id: 1,
            url: url.into(),
            title: "Clip".into(),
            platform: Platform::detect(url),
            tool,
            quality: "best".into(),
            output_dir: "C:/vids".into(),
            filename_template: "{name}_{date}".into(),
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            extra_args: String::new(),
            auto_title: false,
            status: "queued".into(),
            output_path: String::new(),
            bytes: 0,
            exit_code: None,
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
            &AuthSource::None,
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
            &AuthSource::CookiesBrowser("edge".into()),
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
            &AuthSource::None,
        );
        assert_eq!(plan.program, "streamlink");
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.remux_to_mkv);
        // No live-only retry flags for a VOD.
        assert!(!plan.args.iter().any(|a| a == "--retry-streams"));
    }
}
