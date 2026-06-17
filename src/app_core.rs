//! The always-on core: owns the tokio runtime, the persistence store, the event
//! bus, and the set of active recordings (so they can be torn down on exit).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::runtime::Runtime;

use crate::downloader::ActiveSet;
use crate::events::{EventTx, bus};
use crate::store::Store;

pub struct AppCore {
    pub store: Arc<Store>,
    pub events: EventTx,
    /// Background async runtime. Tuned for low idle footprint (2 worker threads).
    /// The GUI runs the winit/egui loop on the main thread; all async work
    /// (polling, child-process supervision) runs here.
    pub rt: Runtime,
    /// monitor_id -> child PID of in-flight recordings.
    pub active: ActiveSet,
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
        self.rt.spawn(async move {
            let ctx = Arc::new(crate::detectors::DetectContext::new(store));
            crate::scheduler::run(ctx, events, live_tx, active_sched).await;
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
            max_concurrent,
        );
        self.rt.spawn(async move {
            supervisor.run(live_rx).await;
        });
    }

    /// Kill all in-flight recording process trees. Called on shutdown so we
    /// don't leave orphaned streamlink/yt-dlp/ffmpeg processes behind.
    pub fn stop_all_recordings(&self) {
        let pids: Vec<u32> = self.active.lock().unwrap().values().copied().collect();
        for pid in pids {
            crate::platform::kill_process_tree(pid);
        }
    }
}
