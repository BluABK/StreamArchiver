//! Shared poll scheduler.
//!
//! A single task loops over all enabled monitors, batches the due ones by
//! detection method, runs detection (Twitch Helix in one batched call;
//! scrape/generic probes concurrently with a cap), writes results back to the
//! store, and emits an [`AppEvent::MonitorState`] on any state change. This is
//! the low-idle-footprint design: one timer, batched work, no thread/process
//! per channel.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::warn;

use crate::detectors::{DetectContext, DetectItem, DetectOutcome};
use crate::events::{AppEvent, EventTx};
use crate::models::{DetectionMethod, now_unix};

/// Max concurrent scrape/probe checks per tick.
const MAX_CONCURRENCY: usize = 8;
/// Upper bound on idle sleep so config changes from the UI are picked up.
const MAX_SLEEP_SECS: i64 = 30;

#[derive(Clone, Copy)]
enum PerItemMode {
    Scrape,
    Generic,
}

/// Run the scheduler forever.
pub async fn run(ctx: Arc<DetectContext>, events: EventTx) {
    loop {
        let wait = tick(&ctx, &events).await;
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

async fn tick(ctx: &Arc<DetectContext>, events: &EventTx) -> u64 {
    let rows = match ctx.store.list_monitors_with_channels() {
        Ok(rows) => rows,
        Err(e) => {
            warn!("scheduler: failed to load monitors: {e:#}");
            return 5;
        }
    };

    let now = now_unix();
    let mut min_wait = MAX_SLEEP_SECS;
    let mut twitch_items: Vec<DetectItem> = Vec::new();
    let mut scrape_items: Vec<DetectItem> = Vec::new();
    let mut generic_items: Vec<DetectItem> = Vec::new();
    let mut prev_state: HashMap<i64, String> = HashMap::new();

    for row in &rows {
        let m = &row.monitor;
        prev_state.insert(m.id, m.last_state.clone());
        if !m.enabled {
            continue;
        }
        // Methods handled by the scheduler today; others are driven elsewhere
        // (CLI self-poll/EventSub in later phases).
        let handled = matches!(
            m.detection_method,
            DetectionMethod::TwitchApi | DetectionMethod::Scrape | DetectionMethod::GenericProbe
        );
        if !handled {
            continue;
        }

        let interval = m.poll_interval_secs.max(5);
        let due_at = m.last_checked_at.unwrap_or(0) + interval;
        if now >= due_at {
            let item = DetectItem {
                monitor_id: m.id,
                url: row.channel.url.clone(),
                platform: row.channel.platform,
            };
            match m.detection_method {
                DetectionMethod::TwitchApi => twitch_items.push(item),
                DetectionMethod::Scrape => scrape_items.push(item),
                DetectionMethod::GenericProbe => generic_items.push(item),
                _ => {}
            }
            min_wait = min_wait.min(interval);
        } else {
            min_wait = min_wait.min(due_at - now);
        }
    }

    let mut outcomes: Vec<DetectOutcome> = Vec::new();
    if !twitch_items.is_empty() {
        outcomes.extend(ctx.detect_twitch(&twitch_items).await);
    }
    if !scrape_items.is_empty() {
        outcomes.extend(run_per_item(ctx, scrape_items, PerItemMode::Scrape).await);
    }
    if !generic_items.is_empty() {
        outcomes.extend(run_per_item(ctx, generic_items, PerItemMode::Generic).await);
    }

    let checked_at = now_unix();
    for o in &outcomes {
        let new_state = if o.error {
            "error"
        } else if o.live {
            "live"
        } else {
            "offline"
        };
        if let Err(e) = ctx
            .store
            .set_monitor_check_result(o.monitor_id, new_state, checked_at)
        {
            warn!("scheduler: failed to persist state for {}: {e:#}", o.monitor_id);
        }
        let changed = prev_state.get(&o.monitor_id).map(String::as_str) != Some(new_state);
        if changed {
            let _ = events.send(AppEvent::MonitorState {
                monitor_id: o.monitor_id,
                state: new_state.to_string(),
            });
        }
    }

    min_wait.clamp(1, MAX_SLEEP_SECS) as u64
}

async fn run_per_item(
    ctx: &Arc<DetectContext>,
    items: Vec<DetectItem>,
    mode: PerItemMode,
) -> Vec<DetectOutcome> {
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENCY));
    let mut set: JoinSet<DetectOutcome> = JoinSet::new();
    for item in items {
        let ctx = ctx.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore");
            match mode {
                PerItemMode::Scrape => ctx.detect_scrape(&item).await,
                PerItemMode::Generic => ctx.detect_generic(&item).await,
            }
        });
    }
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(o) = res {
            out.push(o);
        }
    }
    out
}
