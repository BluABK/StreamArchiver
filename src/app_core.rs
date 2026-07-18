//! The always-on core: owns the tokio runtime, the persistence store, the event
//! bus, and the set of active recordings (so they can be torn down on exit).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::runtime::{Handle, Runtime};
use tracing::{info, warn};

use crate::downloader::ActiveSet;
use crate::events::{EventTx, bus};
use crate::store::Store;

/// How long to wait for in-flight recordings to remux/finalize on shutdown.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

/// Sleep `dur`, returning early (within ~200ms) once `shutdown` is set. Shared by
/// the background loops (scheduler-adjacent tasks, eventsub, websub, ad-free
/// refresher) so they all stop promptly on quit.
pub async fn sleep_cancellable(dur: Duration, shutdown: &Arc<AtomicBool>) {
    let steps = (dur.as_millis() / 200).max(1);
    for _ in 0..steps {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A snapshot row for the process-manager dialog: one spawned download tool,
/// taken from the persistent detached-process registry (which holds a row for
/// every live recording / on-demand video / chat sidecar) and enriched with its
/// channel/video name and tool.
pub struct ProcInfo {
    pub kind: crate::models::DetachedKind,
    pub ref_id: i64,
    pub monitor_id: Option<i64>,
    pub pid: u32,
    pub job_name: String,
    /// Channel name (recording/chat) or video title.
    pub name: String,
    /// Tool label (streamlink / yt-dlp / ffmpeg).
    pub tool: String,
    /// True for the DASH companion leg of a dual capture.
    pub secondary: bool,
    pub started_at: i64,
    pub spawn_build: String,
    /// Started by a different build than the running one — i.e. it survived a
    /// restart/rebuild and was re-attached this session.
    pub reattached: bool,
    pub capture_path: String,
    pub log_path: String,
}

pub struct AppCore {
    pub store: Arc<Store>,
    pub events: EventTx,
    /// Handle to the async runtime — used to spawn background tasks. `rt_owned`
    /// holds the actual runtime; taking it out allows an explicit bounded shutdown.
    pub rt: Handle,
    /// The runtime itself, taken out during explicit shutdown so `stop_all_recordings`
    /// can call `shutdown_timeout` rather than blocking forever in `Drop`.
    rt_owned: Mutex<Option<Runtime>>,
    /// monitor_id -> child PID of in-flight recordings.
    pub active: ActiveSet,
    /// video_id -> child PID of in-flight on-demand video downloads.
    pub active_videos: ActiveSet,
    /// monitor_id -> child PID of in-flight live-chat sidecar downloads.
    pub active_chats: ActiveSet,
    /// video_id -> live download progress fraction (for the UI progress bar).
    pub video_progress: crate::downloader::VideoProgress,
    /// video_id -> live download speed in bytes/sec (for the UI Speed column).
    pub video_speed: crate::downloader::VideoSpeed,
    /// monitor_id -> unix time the current ad break ends (for the UI row tint).
    pub ad_active: crate::downloader::AdActive,
    /// monitor_id -> stop-hold (user Stop suppressing automatic restarts).
    /// Written by the supervisor, read by the UI for the ✋ state badge.
    pub stop_holds: crate::downloader::StopHolds,
    /// monitor_id -> rec_id of takes whose capture ended but whose finalize
    /// (remux, possibly gate-queued for hours) is still pending. Read by the
    /// Streams grid to show "finalizing" instead of a stale "recording".
    pub finalizing: crate::downloader::Finalizing,
    /// Set during shutdown so the scheduler/supervisor stop starting new work.
    pub shutdown: Arc<AtomicBool>,
    /// Set by a "Quit & stop recordings" action so the exit path kills the tool
    /// trees instead of detaching them (overrides the `stop_downloads_on_quit`
    /// setting for this one quit).
    pub force_stop_on_quit: AtomicBool,
    /// Sends on-demand Start/Stop commands to the supervisor (set in `start`).
    manual_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::events::ManualCommand>>>,
    /// Notified to make the schedule refresher do an immediate forced pass (the
    /// UI "reload" action), instead of waiting out its periodic tick.
    schedule_refresh: Arc<tokio::sync::Notify>,
    /// Set to `true` by `request_yt_video_id_refetch()` so the next schedule pass
    /// re-scrapes only the YouTube monitors whose stored segments are missing video IDs.
    yt_video_id_refetch: Arc<AtomicBool>,
    /// When `Some(channel_id)`, the next schedule pass refreshes only monitors
    /// belonging to that channel (set by "Refetch schedule" on a single channel).
    schedule_refresh_channel: Arc<Mutex<Option<i64>>>,
    /// Periodic background jobs + their next-run estimates (the Background view's
    /// "Scheduled" section); updated by each periodic loop before it sleeps.
    pub jobs: crate::events::JobRegistry,
    /// A one-shot message the UI shows once on first frame (e.g. how many detached
    /// downloads were recovered from a previous session). Taken by the UI.
    pub startup_notice: Mutex<Option<String>>,
}

impl AppCore {
    pub fn new(store: Arc<Store>) -> Result<Arc<AppCore>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            // Name the async workers apart from the on-demand blocking pool
            // (tokio names both from the same hook; the 2 workers spawn
            // eagerly inside build(), so the first 2 names are theirs). The
            // distinction makes iomon's slow-op warnings diagnosable: a slow
            // fs op on a `-blocking-` thread is tokio::fs doing its job, one
            // on a `-core-` worker stalls every async task scheduled there.
            .thread_name_fn(|| {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                if n < 2 {
                    format!("streamarchiver-core-{n}")
                } else {
                    format!("streamarchiver-blocking-{}", n - 2)
                }
            })
            .build()?;
        let rt_handle = rt.handle().clone();
        // Registered here (not the interactive-only main.rs GUI path) so
        // EVERY entry point — GUI, `--run-for` headless, `--manual-test` —
        // gets it before anything could reach a settings save or the
        // dynamic disk-gate adjuster's shrink-reclaim spawn, both of which
        // need it to spawn safely from a non-runtime thread (egui's UI
        // thread, or the adjuster's own dedicated std::thread).
        crate::io_gate::set_runtime_handle(rt_handle.clone());
        let (events, _rx) = bus();
        Ok(Arc::new(AppCore {
            store,
            events,
            rt: rt_handle,
            rt_owned: Mutex::new(Some(rt)),
            active: Arc::new(Mutex::new(HashMap::new())),
            active_videos: Arc::new(Mutex::new(HashMap::new())),
            active_chats: Arc::new(Mutex::new(HashMap::new())),
            video_progress: Arc::new(Mutex::new(HashMap::new())),
            video_speed: Arc::new(Mutex::new(HashMap::new())),
            ad_active: Arc::new(Mutex::new(HashMap::new())),
            stop_holds: Arc::new(Mutex::new(HashMap::new())),
            finalizing: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            force_stop_on_quit: AtomicBool::new(false),
            manual_tx: Mutex::new(None),
            schedule_refresh: Arc::new(tokio::sync::Notify::new()),
            yt_video_id_refetch: Arc::new(AtomicBool::new(false)),
            schedule_refresh_channel: Arc::new(Mutex::new(None)),
            jobs: crate::events::job_registry(),
            startup_notice: Mutex::new(None),
        }))
    }

    /// Subscribe to the event bus (used by the UI).
    pub fn subscribe(&self) -> crate::events::EventRx {
        self.events.subscribe()
    }

    /// Spawn background services (poll scheduler + download supervisor).
    pub fn start(&self) {
        // Central capture-cache location (empty = per-output-dir layout) —
        // applied here, before the startup reconcile/resume derive any capture
        // paths, and covering headless runs too. Live-updated on settings save.
        crate::downloader::set_cache_root(
            &self
                .store
                .get_setting(crate::downloader::K_CACHE_ROOT)
                .ok()
                .flatten()
                .unwrap_or_default(),
        );
        // I/O monitor: register the recordings roots for region classification,
        // apply the persisted sample-log toggle, and start the 1 s process/disk
        // sampler (here rather than the GUI path so headless runs sample too).
        {
            let mut roots: Vec<std::path::PathBuf> = self
                .store
                .all_output_dirs()
                .unwrap_or_default()
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect();
            roots.push(
                self.store
                    .get_setting("default_output_dir") // ui.rs K_DEFAULT_OUT
                    .ok()
                    .flatten()
                    .filter(|s| !s.trim().is_empty())
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(crate::app_paths::default_output_dir),
            );
            // …plus every dir PAST recordings live in: an instance retargeted
            // from A:\ to D:\ leaves its old takes on A:, and that drive must
            // stay classified/sampled as a recordings drive too.
            roots.extend(crate::downloader::historical_recording_dirs(&self.store));
            crate::iomon::set_recordings_roots(roots);
            crate::iomon::set_sample_logging(
                self.store
                    .get_setting(crate::iomon::K_IOMON_LOG)
                    .ok()
                    .flatten()
                    .map(|v| v == "1")
                    .unwrap_or(crate::iomon::SAMPLE_LOG_DEFAULT),
            );
            crate::iomon::start_sampler();
            // Dynamic disk-gate limits: started here too (not just the GUI
            // path) so headless/daemon runs also get unattended adaptation —
            // arguably the case that needs it most, since nobody's watching
            // to raise a static limit by hand.
            crate::io_gate::start_dynamic_adjuster();
        }

        // Managed bgutil PO token server: adopt a still-running instance from a
        // previous run, then keep it healthy for as long as the app lives.
        // Here (not the GUI path) so headless runs get tokens too — SABR
        // captures die without them.
        crate::pot_server::init(&self.store);
        crate::pot_server::start_watchdog(
            self.store.clone(),
            self.events.clone(),
            self.shutdown.clone(),
            &self.rt,
        );

        let (live_tx, live_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::events::LiveSignal>();
        let (offline_tx, offline_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::events::OfflineSignal>();
        let (manual_tx, manual_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::events::ManualCommand>();
        *self.manual_tx.lock().unwrap() = Some(manual_tx.clone());

        // One shared detection context (HTTP client + cached Twitch token).
        let ctx = Arc::new(crate::detectors::DetectContext::new(
            self.store.clone(),
            self.events.clone(),
        ));

        // Scheduler: detection -> live signals.
        let events = self.events.clone();
        let active_sched = self.active.clone();
        let shutdown_sched = self.shutdown.clone();
        let live_tx_sched = live_tx.clone();
        let ctx_sched = ctx.clone();
        let jobs_sched = self.jobs.clone();
        self.rt.spawn(async move {
            crate::scheduler::run(
                ctx_sched,
                events,
                live_tx_sched,
                active_sched,
                shutdown_sched,
                jobs_sched,
            )
            .await;
        });

        // Twitch EventSub real-time push -> live signals (idles if unused).
        let es_store = self.store.clone();
        let es_shutdown = self.shutdown.clone();
        self.rt.spawn(async move {
            crate::eventsub::run(es_store, live_tx, offline_tx, es_shutdown).await;
        });

        // YouTube WebSub push via the VPS relay -> on-demand liveness checks
        // (manual Start commands). Idles if unused / not configured.
        let ws_store = self.store.clone();
        let ws_shutdown = self.shutdown.clone();
        let ws_manual_tx = manual_tx.clone();
        let ws_jobs = self.jobs.clone();
        self.rt.spawn(async move {
            crate::websub::run(ws_store, ws_manual_tx, ws_shutdown, ws_jobs).await;
        });

        // Periodic ad-free (Twitch sub) status refresher. Idles when no Twitch
        // account is connected; otherwise refreshes stale entries every few hours
        // and emits a bus event (so the UI reloads) when a status changes.
        let af_ctx = ctx.clone();
        let af_events = self.events.clone();
        let af_shutdown = self.shutdown.clone();
        let af_jobs = self.jobs.clone();
        self.rt.spawn(async move {
            crate::detectors::refresh_ad_free(af_ctx, af_events, af_shutdown, af_jobs).await;
        });

        // Periodic upcoming-stream schedule refresher (Twitch Helix schedule /
        // YouTube scraped upcoming) -> the Next stream column. Cheap + idle-friendly.
        let sch_ctx = ctx.clone();
        let sch_events = self.events.clone();
        let sch_shutdown = self.shutdown.clone();
        let sch_refresh = self.schedule_refresh.clone();
        let sch_vid_refetch = self.yt_video_id_refetch.clone();
        let sch_channel = self.schedule_refresh_channel.clone();
        let sch_jobs = self.jobs.clone();
        self.rt.spawn(async move {
            crate::detectors::refresh_schedules(
                sch_ctx, sch_events, sch_shutdown, sch_refresh, sch_vid_refetch, sch_channel,
                sch_jobs,
            )
            .await;
        });

        // Periodic YouTube community-posts fetcher (the posts feed) — independent
        // of schedule OCR. Idle-friendly: one DB query per pass when it has no
        // YouTube channels to fetch.
        let cp_ctx = ctx.clone();
        let cp_events = self.events.clone();
        let cp_shutdown = self.shutdown.clone();
        let cp_jobs = self.jobs.clone();
        self.rt.spawn(async move {
            crate::detectors::refresh_community_posts(cp_ctx, cp_events, cp_shutdown, cp_jobs).await;
        });

        // Supervisor: live signals + manual commands -> recordings.
        let max_concurrent = self
            .store
            .get_setting("max_concurrent_downloads")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(3)
            .max(1);
        let supervisor = crate::downloader::Supervisor::new(
            self.store.clone(),
            self.events.clone(),
            self.active.clone(),
            self.active_videos.clone(),
            self.video_progress.clone(),
            self.video_speed.clone(),
            self.active_chats.clone(),
            self.shutdown.clone(),
            manual_tx.clone(),
            ctx,
            self.ad_active.clone(),
            max_concurrent,
            self.stop_holds.clone(),
            self.finalizing.clone(),
        );
        // Crash/restart recovery, in two synchronous passes so reservations land
        // before detection can fire:
        //   1) reconcile the detached-process registry — re-attach to downloads that
        //      outlived the app, finalize ones that finished while it was down, and
        //      hand SABR-resumable ones to the resume path;
        //   2) resume_inflight for any legacy in-flight recording WITHOUT a registry
        //      row (registry-backed ones are excluded so they aren't double-handled).
        // Then sweep stale `.cache\` working files, protecting every still-needed stem.
        // CDN recoveries are in-process tasks — none survive a restart. Reset
        // any row a crash left stuck in 'recovering' (permanent badge +
        // excluded from bulk scans otherwise).
        match self.store.reset_stale_recovering() {
            Ok(n) if n > 0 => warn!(n, "reset stale 'recovering' takes from a previous session"),
            _ => {}
        }
        // Same for VOD archives whose pre-download wait died with the app —
        // an adopted detached download re-archives over the 'failed' on finish.
        match self.store.reset_stale_vod_downloading() {
            Ok(n) if n > 0 => warn!(n, "reset stale 'downloading' VOD archives from a previous session"),
            _ => {}
        }
        let (reattach_items, mut skip_stems) = supervisor.reconcile_detached();
        // Surface what we recovered: name the PIDs we're re-attaching to (still
        // running from a previous session); fall back to a count for ones that only
        // need finalizing/resuming.
        let adopt_pids: Vec<u32> = reattach_items
            .iter()
            .filter(|i| i.is_adopt())
            .map(|i| i.pid())
            .collect();
        if !adopt_pids.is_empty() {
            let shown = adopt_pids
                .iter()
                .take(4)
                .map(|p| format!("pid {p}"))
                .collect::<Vec<_>>()
                .join(", ");
            let more = adopt_pids.len().saturating_sub(4);
            let suffix = if more > 0 { format!(", +{more} more") } else { String::new() };
            *self.startup_notice.lock().unwrap() = Some(format!(
                "Re-attaching to {} running download(s): {shown}{suffix}",
                adopt_pids.len()
            ));
        } else if !reattach_items.is_empty() {
            *self.startup_notice.lock().unwrap() = Some(format!(
                "Recovering {} download(s) from a previous session.",
                reattach_items.len()
            ));
        }
        for item in reattach_items {
            let s = supervisor.clone();
            self.rt.spawn(async move { s.reattach_one(item).await });
        }
        let to_resume = supervisor.resume_inflight();
        skip_stems.extend(to_resume.iter().filter_map(|(rec, _)| {
            std::path::Path::new(&rec.output_path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        }));
        for (rec, row) in to_resume {
            let s = supervisor.clone();
            self.rt.spawn(async move { s.resume_recording(rec, row).await });
        }
        // Also protect every recording whose CURRENT output_path still points
        // into a `.cache\` dir — e.g. a fully-successful capture that's merely
        // stuck there because its promote-to-output-dir move failed (surfaced
        // in the Issues panel, recoverable from there). The sweep can't tell
        // that apart from genuine leftover garbage by file age alone.
        skip_stems.extend(self.store.stems_in_cache().unwrap_or_default());
        // …and every orphan-repair candidate's stem: their `.cache\` capture may
        // be the ONLY surviving copy (final file never written), and the repair
        // pass below runs asynchronously — the sweep must not race it.
        skip_stems.extend(
            self.store
                .orphan_repair_candidates()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(_, _, out)| {
                    std::path::Path::new(&out)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                }),
        );
        let sweep_sup = supervisor.clone();
        self.rt
            .spawn(async move { sweep_sup.sweep_caches(skip_stems).await });

        // Disk-aware repair of takes whose row claims a final file that was never
        // written (crash before/during the finalize remux): promote intact ones,
        // retarget stranded `.cache\` captures into the Issues recovery sections.
        let orphan_sup = supervisor.clone();
        self.rt
            .spawn(async move { orphan_sup.reconcile_orphan_outputs().await });

        // Re-drive head backfills whose planned ('queued') state outlived the
        // in-memory job that owned it — otherwise "Planned" persists forever.
        let hb_sup = supervisor.clone();
        self.rt
            .spawn(async move { hb_sup.requeue_stale_head_backfills().await });

        // Repair pass for post-stream VOD archives: replay completed downloads
        // whose recording-side finalize never ran, and demote 'archived' rows
        // that point at a bogus file (see reconcile_vod_archives).
        let vod_sup = supervisor.clone();
        self.rt
            .spawn(async move { vod_sup.reconcile_vod_archives().await });

        // Periodic channel-asset refresh (keeps icons/badges/emotes current for
        // channels that rarely record).
        let asset_sup = supervisor.clone();
        let asset_shutdown = self.shutdown.clone();
        let asset_jobs = self.jobs.clone();
        self.rt.spawn(async move {
            asset_sup.asset_refresh_loop(asset_shutdown, asset_jobs).await;
        });

        // Scheduled recordings (schema v51): force-start/stop at a specific
        // time or on a weekly repeat, independent of Auto/live-detection.
        let sched_rec_sup = supervisor.clone();
        let sched_rec_shutdown = self.shutdown.clone();
        let sched_rec_jobs = self.jobs.clone();
        self.rt.spawn(async move {
            sched_rec_sup
                .scheduled_recordings_loop(sched_rec_shutdown, sched_rec_jobs)
                .await;
        });

        self.rt.spawn(async move {
            supervisor.run(live_rx, offline_rx, manual_rx).await;
        });

        // Desktop notifications for recording lifecycle events (gated on the
        // `notifications_enabled` setting, read live).
        let notif_rx = self.events.subscribe();
        let notif_store = self.store.clone();
        self.rt.spawn(async move {
            crate::notifications::run(notif_rx, notif_store).await;
        });
    }

    /// Send an on-demand recording command (Start/Stop) to the supervisor.
    pub fn manual(&self, cmd: crate::events::ManualCommand) {
        if let Some(tx) = self.manual_tx.lock().unwrap().as_ref() {
            let _ = tx.send(cmd);
        }
    }

    /// Ask the schedule refresher to fetch all monitors' schedules now (the UI
    /// "reload" action), rather than waiting for its periodic tick. The refresher
    /// emits a state event when done, which makes the UI reload from the store.
    pub fn request_schedule_refresh(&self) {
        self.schedule_refresh.notify_one();
    }

    /// Trigger a targeted re-scrape of YouTube monitors whose stored schedule
    /// segments are missing video IDs (so they can be refined by `videos.list`).
    /// Only those monitors are re-fetched; others keep their cached schedules.
    pub fn request_yt_video_id_refetch(&self) {
        self.yt_video_id_refetch.store(true, Ordering::SeqCst);
        self.schedule_refresh.notify_one();
    }

    /// Refresh the schedule for a single channel immediately (ignoring staleness).
    /// Only the monitors belonging to `channel_id` are re-fetched; every other
    /// channel keeps its cached schedule.
    pub fn request_schedule_refresh_for_channel(&self, channel_id: i64) {
        *self.schedule_refresh_channel.lock().unwrap() = Some(channel_id);
        self.schedule_refresh.notify_one();
    }

    /// Gracefully stop all recordings and on-demand video downloads: signal
    /// shutdown, kill the tool process trees (so each task's child exits), then
    /// wait for those tasks to remux `.ts` -> `.mkv` and finalize before returning.
    /// Finally shuts down the async runtime with a bounded timeout so the process
    /// never hangs indefinitely waiting for a stuck background task or blocking thread.
    pub fn stop_all_recordings(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let initial = self.active.lock().unwrap().len()
            + self.active_videos.lock().unwrap().len()
            + self.active_chats.lock().unwrap().len();
        if initial > 0 {
            info!("stopping {initial} active download(s); waiting for finalize...");
            let start = Instant::now();
            loop {
                let pids: Vec<u32> = self
                    .active
                    .lock()
                    .unwrap()
                    .values()
                    .chain(self.active_videos.lock().unwrap().values())
                    .chain(self.active_chats.lock().unwrap().values())
                    .copied()
                    .filter(|p| *p > 0)
                    .collect();
                for pid in pids {
                    crate::platform::kill_process_tree(pid);
                }
                if self.active.lock().unwrap().is_empty()
                    && self.active_videos.lock().unwrap().is_empty()
                    && self.active_chats.lock().unwrap().is_empty()
                {
                    info!("all downloads finalized");
                    break;
                }
                if start.elapsed() > SHUTDOWN_DRAIN_TIMEOUT {
                    let n = self.active.lock().unwrap().len()
                        + self.active_videos.lock().unwrap().len()
                        + self.active_chats.lock().unwrap().len();
                    warn!("timed out waiting for {n} download(s) to finalize");
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        // Stopping everything includes the managed PO token server (an external
        // one is untouched); the detach path deliberately leaves it running —
        // detached SABR captures still need tokens after the app exits.
        crate::pot_server::kill_managed(&self.store);
        // Belt-and-suspenders: terminate any detached job tree by name (catches a
        // grandchild that escaped the PID-tree walk) and drop its registry row so the
        // next launch doesn't try to re-attach to a download we just stopped.
        for row in self.store.list_detached().unwrap_or_default() {
            if let Some(job) = crate::platform::DetachedJob::open(&row.job_name) {
                job.kill();
            }
            let _ = self.store.clear_detached(row.kind, row.ref_id);
        }
        // Shut down the runtime explicitly with a timeout so a stuck background task
        // (e.g. an eventsub WebSocket mid-keepalive, a spawn_blocking notification)
        // never causes the process to hang indefinitely after the UI exits.
        if let Some(rt) = self.rt_owned.lock().unwrap().take() {
            info!("shutting down async runtime (timeout 30s)");
            rt.shutdown_timeout(Duration::from_secs(30));
            info!("runtime shut down");
        }
    }

    /// Quit **without** stopping downloads: signal the background loops to stop and
    /// shut the runtime down, but leave the tool process trees running. Because the
    /// tools are spawned detached (no `kill_on_drop`, into a job without
    /// kill-on-close) and each one persisted a `detached_process` registry row at
    /// spawn, dropping the supervisor tasks leaves them recording; the next launch
    /// re-attaches via [`crate::downloader::Supervisor::resume_inflight`].
    pub fn detach_all(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let n = self.active.lock().unwrap().len()
            + self.active_videos.lock().unwrap().len()
            + self.active_chats.lock().unwrap().len();
        if n > 0 {
            info!("detaching {n} running download(s) — they keep running after exit");
        }
        if let Some(rt) = self.rt_owned.lock().unwrap().take() {
            info!("shutting down async runtime (timeout 30s); downloads left running");
            rt.shutdown_timeout(Duration::from_secs(30));
            info!("runtime shut down");
        }
    }

    /// The app is exiting: stop the tool trees only when the user asked to (a
    /// "Quit & stop recordings" action, or the `stop_downloads_on_quit` setting);
    /// otherwise detach so downloads survive the restart/rebuild.
    pub fn shutdown_on_exit(&self) {
        let stop = self.force_stop_on_quit.load(Ordering::SeqCst)
            || self
                .store
                .get_setting("stop_downloads_on_quit")
                .ok()
                .flatten()
                .as_deref()
                == Some("1");
        if stop {
            info!("shutting down; stopping active recordings");
            self.stop_all_recordings();
        } else {
            info!("shutting down; detaching active recordings (they keep running)");
            self.detach_all();
        }
    }

    /// Snapshot every spawned download tool process for the process-manager dialog.
    /// Sourced from the detached-process registry (a row exists for each live
    /// download), enriched with the channel/video name + tool, and a live
    /// `pid_alive` check. Does a few synchronous DB reads — call on a refresh tick,
    /// not every frame.
    pub fn list_processes(&self) -> Vec<ProcInfo> {
        use crate::models::DetachedKind;
        let build = crate::version::build_id();
        self.store
            .list_detached()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| crate::platform::pid_alive(r.pid))
            .map(|r| {
                let (name, tool) = match r.kind {
                    DetachedKind::Recording | DetachedKind::Chat => self
                        .store
                        .get_monitor_with_channel(r.monitor_id.unwrap_or(0))
                        .ok()
                        .flatten()
                        .map(|m| (m.channel.name, m.monitor.tool.label().to_string()))
                        .unwrap_or_default(),
                    DetachedKind::Video => self
                        .store
                        .get_video(r.ref_id)
                        .ok()
                        .flatten()
                        .map(|v| {
                            let name = if v.title.trim().is_empty() { v.url } else { v.title };
                            (name, v.tool.label().to_string())
                        })
                        .unwrap_or_default(),
                };
                ProcInfo {
                    reattached: r.spawn_build != build,
                    kind: r.kind,
                    ref_id: r.ref_id,
                    monitor_id: r.monitor_id,
                    pid: r.pid,
                    job_name: r.job_name,
                    name,
                    tool,
                    secondary: r.secondary,
                    started_at: r.started_at,
                    spawn_build: r.spawn_build,
                    capture_path: r.capture_path,
                    log_path: r.log_path,
                }
            })
            .collect()
    }

    /// Force-terminate a spawned process tree (its named job + a Toolhelp tree
    /// walk by PID). A hard kill — the recording is finalized by the supervisor
    /// task's normal exit handling (or the next-launch reconcile), not here.
    pub fn force_kill(&self, pid: u32, job_name: &str) {
        if let Some(job) = crate::platform::DetachedJob::open(job_name) {
            job.kill();
        }
        crate::platform::kill_process_tree(pid);
    }

    /// Gracefully stop a spawned download through the supervisor's coordinated
    /// path so the take is finalized (remuxed, marked `stopped`). The DASH
    /// companion has no dedicated command, so it falls back to a tree kill (its
    /// own task still finalizes on the resulting process exit).
    pub fn stop_process(&self, p: &ProcInfo) {
        use crate::events::ManualCommand;
        use crate::models::DetachedKind;
        match (p.kind, p.secondary) {
            (DetachedKind::Recording, false) => {
                self.manual(ManualCommand::Stop(p.monitor_id.unwrap_or(0)))
            }
            (DetachedKind::Video, _) => self.manual(ManualCommand::StopVideo(p.ref_id)),
            (DetachedKind::Chat, _) => self.manual(ManualCommand::StopChat(p.monitor_id.unwrap_or(0))),
            (DetachedKind::Recording, true) => self.force_kill(p.pid, &p.job_name),
        }
    }
}
