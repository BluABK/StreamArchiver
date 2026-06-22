//! The always-on core: owns the tokio runtime, the persistence store, the event
//! bus, and the set of active recordings (so they can be torn down on exit).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::runtime::Runtime;
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

pub struct AppCore {
    pub store: Arc<Store>,
    pub events: EventTx,
    /// Background async runtime. Tuned for low idle footprint (2 worker threads).
    pub rt: Runtime,
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
    /// Set during shutdown so the scheduler/supervisor stop starting new work.
    pub shutdown: Arc<AtomicBool>,
    /// Sends on-demand Start/Stop commands to the supervisor (set in `start`).
    manual_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::events::ManualCommand>>>,
    /// Notified to make the schedule refresher do an immediate forced pass (the
    /// UI "reload" action), instead of waiting out its periodic tick.
    schedule_refresh: Arc<tokio::sync::Notify>,
    /// Set to `true` by `request_yt_video_id_refetch()` so the next schedule pass
    /// re-scrapes only the YouTube monitors whose stored segments are missing video IDs.
    yt_video_id_refetch: Arc<AtomicBool>,
}

impl AppCore {
    pub fn new(store: Arc<Store>) -> Result<Arc<AppCore>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("streamarchiver-core")
            .build()?;
        let (events, _rx) = bus();
        Ok(Arc::new(AppCore {
            store,
            events,
            rt,
            active: Arc::new(Mutex::new(HashMap::new())),
            active_videos: Arc::new(Mutex::new(HashMap::new())),
            active_chats: Arc::new(Mutex::new(HashMap::new())),
            video_progress: Arc::new(Mutex::new(HashMap::new())),
            video_speed: Arc::new(Mutex::new(HashMap::new())),
            ad_active: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            manual_tx: Mutex::new(None),
            schedule_refresh: Arc::new(tokio::sync::Notify::new()),
            yt_video_id_refetch: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// Subscribe to the event bus (used by the UI).
    pub fn subscribe(&self) -> crate::events::EventRx {
        self.events.subscribe()
    }

    /// Spawn background services (poll scheduler + download supervisor).
    pub fn start(&self) {
        let (live_tx, live_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::events::LiveSignal>();
        let (manual_tx, manual_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::events::ManualCommand>();
        *self.manual_tx.lock().unwrap() = Some(manual_tx.clone());

        // One shared detection context (HTTP client + cached Twitch token).
        let ctx = Arc::new(crate::detectors::DetectContext::new(self.store.clone()));

        // Scheduler: detection -> live signals.
        let events = self.events.clone();
        let active_sched = self.active.clone();
        let shutdown_sched = self.shutdown.clone();
        let live_tx_sched = live_tx.clone();
        let ctx_sched = ctx.clone();
        self.rt.spawn(async move {
            crate::scheduler::run(
                ctx_sched,
                events,
                live_tx_sched,
                active_sched,
                shutdown_sched,
            )
            .await;
        });

        // Twitch EventSub real-time push -> live signals (idles if unused).
        let es_store = self.store.clone();
        let es_shutdown = self.shutdown.clone();
        self.rt.spawn(async move {
            crate::eventsub::run(es_store, live_tx, es_shutdown).await;
        });

        // YouTube WebSub push via the VPS relay -> on-demand liveness checks
        // (manual Start commands). Idles if unused / not configured.
        let ws_store = self.store.clone();
        let ws_shutdown = self.shutdown.clone();
        let ws_manual_tx = manual_tx.clone();
        self.rt.spawn(async move {
            crate::websub::run(ws_store, ws_manual_tx, ws_shutdown).await;
        });

        // Periodic ad-free (Twitch sub) status refresher. Idles when no Twitch
        // account is connected; otherwise refreshes stale entries every few hours
        // and emits a bus event (so the UI reloads) when a status changes.
        let af_ctx = ctx.clone();
        let af_events = self.events.clone();
        let af_shutdown = self.shutdown.clone();
        self.rt.spawn(async move {
            crate::detectors::refresh_ad_free(af_ctx, af_events, af_shutdown).await;
        });

        // Periodic upcoming-stream schedule refresher (Twitch Helix schedule /
        // YouTube scraped upcoming) -> the Next stream column. Cheap + idle-friendly.
        let sch_ctx = ctx.clone();
        let sch_events = self.events.clone();
        let sch_shutdown = self.shutdown.clone();
        let sch_refresh = self.schedule_refresh.clone();
        let sch_vid_refetch = self.yt_video_id_refetch.clone();
        self.rt.spawn(async move {
            crate::detectors::refresh_schedules(
                sch_ctx, sch_events, sch_shutdown, sch_refresh, sch_vid_refetch,
            )
            .await;
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
            ctx,
            self.ad_active.clone(),
            max_concurrent,
        );
        self.rt.spawn(async move {
            supervisor.run(live_rx, manual_rx).await;
        });

        // Desktop notifications for recording lifecycle events.
        let notif_rx = self.events.subscribe();
        self.rt.spawn(async move {
            crate::notifications::run(notif_rx).await;
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

    /// Gracefully stop all recordings and on-demand video downloads: signal
    /// shutdown, kill the tool process trees (so each task's child exits), then
    /// wait for those tasks to remux `.ts` -> `.mkv` and finalize before returning.
    pub fn stop_all_recordings(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let initial = self.active.lock().unwrap().len()
            + self.active_videos.lock().unwrap().len()
            + self.active_chats.lock().unwrap().len();
        if initial == 0 {
            return;
        }
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
}
