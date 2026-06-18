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

pub struct AppCore {
    pub store: Arc<Store>,
    pub events: EventTx,
    /// Background async runtime. Tuned for low idle footprint (2 worker threads).
    pub rt: Runtime,
    /// monitor_id -> child PID of in-flight recordings.
    pub active: ActiveSet,
    /// video_id -> child PID of in-flight on-demand video downloads.
    pub active_videos: ActiveSet,
    /// video_id -> live download progress fraction (for the UI progress bar).
    pub video_progress: crate::downloader::VideoProgress,
    /// Set during shutdown so the scheduler/supervisor stop starting new work.
    pub shutdown: Arc<AtomicBool>,
    /// Sends on-demand Start/Stop commands to the supervisor (set in `start`).
    manual_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::events::ManualCommand>>>,
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
            video_progress: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            manual_tx: Mutex::new(None),
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
        *self.manual_tx.lock().unwrap() = Some(manual_tx);

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
            self.shutdown.clone(),
            ctx,
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

    /// Gracefully stop all recordings and on-demand video downloads: signal
    /// shutdown, kill the tool process trees (so each task's child exits), then
    /// wait for those tasks to remux `.ts` -> `.mkv` and finalize before returning.
    pub fn stop_all_recordings(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let initial = self.active.lock().unwrap().len() + self.active_videos.lock().unwrap().len();
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
                .copied()
                .filter(|p| *p > 0)
                .collect();
            for pid in pids {
                crate::platform::kill_process_tree(pid);
            }
            if self.active.lock().unwrap().is_empty()
                && self.active_videos.lock().unwrap().is_empty()
            {
                info!("all downloads finalized");
                break;
            }
            if start.elapsed() > SHUTDOWN_DRAIN_TIMEOUT {
                let n =
                    self.active.lock().unwrap().len() + self.active_videos.lock().unwrap().len();
                warn!("timed out waiting for {n} download(s) to finalize");
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
