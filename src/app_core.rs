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
    /// Set during shutdown so the scheduler/supervisor stop starting new work.
    pub shutdown: Arc<AtomicBool>,
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
            shutdown: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// Subscribe to the event bus (used by the UI).
    pub fn subscribe(&self) -> crate::events::EventRx {
        self.events.subscribe()
    }

    /// Spawn background services (poll scheduler + download supervisor).
    pub fn start(&self) {
        let (live_tx, live_rx) = tokio::sync::mpsc::unbounded_channel::<i64>();

        // Scheduler: detection -> live signals.
        let store = self.store.clone();
        let events = self.events.clone();
        let active_sched = self.active.clone();
        let shutdown_sched = self.shutdown.clone();
        self.rt.spawn(async move {
            let ctx = Arc::new(crate::detectors::DetectContext::new(store));
            crate::scheduler::run(ctx, events, live_tx, active_sched, shutdown_sched).await;
        });

        // Supervisor: live signals -> recordings.
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
            self.shutdown.clone(),
            max_concurrent,
        );
        self.rt.spawn(async move {
            supervisor.run(live_rx).await;
        });

        // Desktop notifications for recording lifecycle events.
        let notif_rx = self.events.subscribe();
        self.rt.spawn(async move {
            crate::notifications::run(notif_rx).await;
        });
    }

    /// Gracefully stop all recordings: signal shutdown, kill the tool process
    /// trees (so each record task's child exits), then wait for those tasks to
    /// remux `.ts` -> `.mkv` and finalize before returning.
    pub fn stop_all_recordings(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let initial = self.active.lock().unwrap().len();
        if initial == 0 {
            return;
        }
        info!("stopping {initial} active recording(s); waiting for finalize...");
        let start = Instant::now();
        loop {
            let pids: Vec<u32> = self
                .active
                .lock()
                .unwrap()
                .values()
                .copied()
                .filter(|p| *p > 0)
                .collect();
            for pid in pids {
                crate::platform::kill_process_tree(pid);
            }
            if self.active.lock().unwrap().is_empty() {
                info!("all recordings finalized");
                break;
            }
            if start.elapsed() > SHUTDOWN_DRAIN_TIMEOUT {
                let n = self.active.lock().unwrap().len();
                warn!("timed out waiting for {n} recording(s) to finalize");
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
