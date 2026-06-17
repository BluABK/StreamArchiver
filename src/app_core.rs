//! The always-on core: owns the tokio runtime, the persistence store, and the
//! event bus. In Phase 1 it wires these together; the scheduler and download
//! supervisor attach to it in later phases.

use std::sync::Arc;

use anyhow::Result;
use tokio::runtime::Runtime;

use crate::events::{EventTx, bus};
use crate::store::Store;

pub struct AppCore {
    pub store: Arc<Store>,
    pub events: EventTx,
    /// Background async runtime. Tuned for low idle footprint (2 worker threads).
    /// The GUI runs the winit/egui loop on the main thread; all async work
    /// (polling, child-process supervision) runs here.
    pub rt: Runtime,
}

impl AppCore {
    pub fn new(store: Arc<Store>) -> Result<Arc<AppCore>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("streamarchiver-core")
            .build()?;
        let (events, _rx) = bus();
        Ok(Arc::new(AppCore { store, events, rt }))
    }

    /// Subscribe to the event bus (used by the UI).
    pub fn subscribe(&self) -> crate::events::EventRx {
        self.events.subscribe()
    }

    /// Spawn background services (the poll scheduler) on the async runtime.
    pub fn start(&self) {
        let store = self.store.clone();
        let events = self.events.clone();
        self.rt.spawn(async move {
            let ctx = Arc::new(crate::detectors::DetectContext::new(store));
            crate::scheduler::run(ctx, events).await;
        });
    }
}
