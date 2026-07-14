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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::detectors::{DetectContext, DetectItem, DetectOutcome};
use crate::downloader::ActiveSet;
use crate::events::{AppEvent, EventTx, LiveSignal};
use crate::models::{DetectionMethod, now_unix};

/// Max concurrent scrape/probe checks per tick.
const MAX_CONCURRENCY: usize = 8;
/// Upper bound on idle sleep so config changes from the UI are picked up.
const MAX_SLEEP_SECS: i64 = 30;

#[derive(Clone, Copy)]
enum PerItemMode {
    Scrape,
    Generic,
    YouTubeApi,
    KickApi,
}

/// Run the scheduler until shutdown is signaled.
pub async fn run(
    ctx: Arc<DetectContext>,
    events: EventTx,
    live_tx: mpsc::UnboundedSender<LiveSignal>,
    active: ActiveSet,
    shutdown: Arc<AtomicBool>,
    jobs: crate::events::JobRegistry,
) {
    while !shutdown.load(Ordering::SeqCst) {
        // Live poll can be disabled from the Background view (a global pause of
        // detection/recording); idle-check for re-enable without polling.
        if !ctx.store.job_enabled("job_live_poll") {
            crate::app_core::sleep_cancellable(Duration::from_secs(10), &shutdown).await;
            continue;
        }
        let wait = tick(&ctx, &events, &live_tx, &active).await;
        crate::events::mark_job(&jobs, "Live poll", wait as i64);
        crate::app_core::sleep_cancellable(Duration::from_secs(wait), &shutdown).await;
    }
}

async fn tick(
    ctx: &Arc<DetectContext>,
    events: &EventTx,
    live_tx: &mpsc::UnboundedSender<LiveSignal>,
    active: &ActiveSet,
) -> u64 {
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
    let mut youtube_api_items: Vec<DetectItem> = Vec::new();
    let mut kick_api_items: Vec<DetectItem> = Vec::new();
    let mut prev_state: HashMap<i64, String> = HashMap::new();
    // monitor id -> (channel name, detection short label, platform) for
    // readable logs.
    let mut meta: HashMap<i64, (String, &'static str, crate::models::Platform)> = HashMap::new();
    // monitor id -> the currently-persisted (go-live time, is-approx) so a
    // continuing live session with no platform-reported go-live time keeps its
    // originally-stamped approximation instead of drifting forward every poll.
    let mut prev_live_since: HashMap<i64, (Option<i64>, bool)> = HashMap::new();

    let recording: std::collections::HashSet<i64> =
        active.lock().unwrap().keys().copied().collect();

    for row in &rows {
        let m = &row.monitor;
        prev_state.insert(m.id, m.last_state.clone());
        prev_live_since.insert(m.id, (m.last_live_since, m.last_live_since_approx));
        // Master "Enabled" switch off → fully dormant: no detection at all (nor
        // any recording/fetch elsewhere). The channel keeps its last state until
        // manually checked. Distinct from Auto (below), which never gates
        // detection. This is the ONLY switch that stops polling.
        if !row.automation_on() {
            continue;
        }
        // Auto-off monitors are still polled: Auto only gates the automatic
        // recording start (enforced in the supervisor's try_begin), while
        // detection keeps liveness, go-live times, and downstream metadata
        // current for every monitored channel.
        // Don't poll a monitor that's currently being recorded — the supervisor
        // owns its state until the tool exits.
        if recording.contains(&m.id) {
            continue;
        }
        // Methods handled by the scheduler today; others are driven elsewhere
        // (CLI self-poll/EventSub in later phases).
        // EventSubHelix is polled here (Helix) *and* pushed by the EventSub task;
        // whichever sees live first wins (the supervisor dedupes). WebSub is the
        // same idea for YouTube: scrape-polled here as a fallback, and pushed by
        // the websub task (which triggers an on-demand liveness check).
        // WebSubOnly is push-only — no poll fallback, so it is not in this list.
        // Disabled is intentionally never in this list either: it means "never
        // auto-check this instance at all" (see `DetectionMethod::Disabled`).
        let handled = matches!(
            m.detection_method,
            DetectionMethod::TwitchApi
                | DetectionMethod::EventSubHelix
                | DetectionMethod::Scrape
                | DetectionMethod::GenericProbe
                | DetectionMethod::YouTubeApi
                | DetectionMethod::KickApi
                | DetectionMethod::WebSub
        );
        if !handled {
            continue;
        }

        let interval = m.poll_interval_secs.max(5);
        let due_at = m.last_checked_at.unwrap_or(0) + interval;
        if now >= due_at {
            meta.insert(
                m.id,
                (row.channel.name.clone(), m.detection_method.short_label(), m.platform()),
            );
            let item = DetectItem {
                monitor_id: m.id,
                url: m.url.clone(),
                platform: m.platform(),
            };
            match m.detection_method {
                DetectionMethod::TwitchApi | DetectionMethod::EventSubHelix => {
                    twitch_items.push(item)
                }
                DetectionMethod::Scrape | DetectionMethod::WebSub => scrape_items.push(item),
                DetectionMethod::GenericProbe => generic_items.push(item),
                DetectionMethod::YouTubeApi => youtube_api_items.push(item),
                DetectionMethod::KickApi => kick_api_items.push(item),
                _ => {}
            }
            min_wait = min_wait.min(interval);
        } else {
            min_wait = min_wait.min(due_at - now);
        }
    }

    let due = twitch_items.len()
        + scrape_items.len()
        + generic_items.len()
        + youtube_api_items.len()
        + kick_api_items.len();
    if due > 0 {
        debug!(
            "scheduler: polling {due} monitor(s) due [twitch={} scrape={} generic={} yt-api={} kick={}]",
            twitch_items.len(),
            scrape_items.len(),
            generic_items.len(),
            youtube_api_items.len(),
            kick_api_items.len(),
        );
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
    if !youtube_api_items.is_empty() {
        outcomes.extend(run_per_item(ctx, youtube_api_items, PerItemMode::YouTubeApi).await);
    }
    if !kick_api_items.is_empty() {
        outcomes.extend(run_per_item(ctx, kick_api_items, PerItemMode::KickApi).await);
    }

    let checked_at = now_unix();
    // One read-modify-write per tick (not per monitor) — folds every outcome
    // from this pass into the persisted per-platform counters so the Stats
    // view can show request instability (error rates, most recent failure)
    // without needing to comb the log.
    if !outcomes.is_empty() {
        let mut stats = load_poll_stats(&ctx.store);
        for o in &outcomes {
            let platform = meta
                .get(&o.monitor_id)
                .map(|(_, _, p)| *p)
                .unwrap_or(crate::models::Platform::Generic);
            let entry = stats.by_platform.entry(platform.as_str().to_string()).or_default();
            entry.polls += 1;
            if o.error {
                entry.errors += 1;
                entry.last_error_at = Some(checked_at);
                entry.last_error = o.detail.clone();
            }
        }
        save_poll_stats(&ctx.store, &stats);
    }
    for o in &outcomes {
        // This tick's `recording` snapshot was taken before the (possibly slow,
        // batched) detection calls above ran — a recording can start for this
        // monitor in the meantime (e.g. an EventSub push winning the race
        // against a still-in-flight Helix poll). Re-check membership fresh
        // here so this write never clobbers the supervisor's own "recording"
        // state back to "live"/"offline".
        let new_state = if active.lock().unwrap().contains_key(&o.monitor_id) {
            "recording"
        } else if o.error {
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
            warn!(
                "scheduler: failed to persist state for {}: {e:#}",
                o.monitor_id
            );
        }
        // Persist the last-detected live info on EVERY poll, regardless of the
        // Auto-record flag, so the grid can show a live channel's title/game/
        // viewers without a recording. Cleared (empty + -1) when offline/errored
        // or when the platform omits a field.
        let (title, game, thumb, viewers) = if o.live && !o.error {
            (
                o.stream_title.as_deref().unwrap_or(""),
                o.stream_game.as_deref().unwrap_or(""),
                o.thumbnail_url.as_deref().unwrap_or(""),
                o.stream_viewers.unwrap_or(-1),
            )
        } else {
            ("", "", "", -1)
        };
        let old_state = prev_state.get(&o.monitor_id).map(String::as_str);
        // Go-live time for the CURRENTLY live broadcast, independent of any
        // recording (so Went Live/Started On/Duration have data with Auto off).
        // A platform-reported time (Twitch) is authoritative and always wins;
        // when the source gives none, keep the previously-stamped approximation
        // for as long as the same broadcast continues (still "live" last poll)
        // rather than re-approximating (and thus drifting) every tick.
        let (live_since, live_since_approx) = if o.live && !o.error {
            match o.went_live_at {
                Some(t) => (Some(t), false),
                None if old_state == Some("live") => prev_live_since
                    .get(&o.monitor_id)
                    .copied()
                    .unwrap_or((Some(checked_at), true)),
                None => (Some(checked_at), true),
            }
        } else {
            (None, false)
        };
        if let Err(e) = ctx.store.set_monitor_live_meta(
            o.monitor_id,
            title,
            game,
            thumb,
            viewers,
            live_since,
            live_since_approx,
        ) {
            warn!(
                "scheduler: failed to persist live meta for {}: {e:#}",
                o.monitor_id
            );
        }
        let changed = old_state != Some(new_state);

        // Readable per-poll logging: name [method] result (+ go-live / error
        // detail). A state change is INFO; a routine poll is DEBUG.
        let (name, method, plat) = meta
            .get(&o.monitor_id)
            .map(|(n, m, p)| (n.as_str(), *m, *p))
            .unwrap_or(("?", "?", crate::models::Platform::Generic));
        let tag = plat.tag();
        let extra = if o.error {
            format!(" — {}", o.detail)
        } else if o.live {
            match o.went_live_at {
                Some(t) => format!(" (live since {})", fmt_log_time(t)),
                None => String::new(),
            }
        } else {
            String::new()
        };
        if changed {
            info!(
                "poll: {tag} {name} [{method}] {} -> {new_state}{extra}",
                old_state.unwrap_or("?")
            );
            let _ = events.send(AppEvent::MonitorState {
                monitor_id: o.monitor_id,
                state: new_state.to_string(),
            });
        } else {
            debug!("poll: {tag} {name} [{method}] {new_state}{extra}");
        }
        // Signal the supervisor to (consider) starting a recording. Use the
        // platform-reported go-live time when available, else approximate it
        // with our detection time.
        if o.live && !o.error {
            let signal = match o.went_live_at {
                Some(t) => LiveSignal::new(o.monitor_id, Some(t), false),
                None => LiveSignal::new(o.monitor_id, Some(checked_at), true),
            }
            .with_stream_id(o.stream_id.clone())
            .with_thumbnail_url(o.thumbnail_url.clone())
            .with_broadcaster_id(o.broadcaster_id.clone())
            .with_stream_title(o.stream_title.clone())
            .with_stream_game(o.stream_game.clone());
            let _ = live_tx.send(signal);
        }
    }

    min_wait.clamp(1, MAX_SLEEP_SECS) as u64
}

/// Load the cumulative per-platform poll/detect stats from the settings
/// store (see [`crate::models::PollStats`]). Used by the Stats view; the
/// scheduler itself only needs the mutate-then-save half (below).
pub fn load_poll_stats(store: &crate::store::Store) -> crate::models::PollStats {
    store
        .get_setting(crate::models::K_POLL_STATS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_poll_stats(store: &crate::store::Store, stats: &crate::models::PollStats) {
    if let Ok(json) = serde_json::to_string(stats) {
        let _ = store.set_setting(crate::models::K_POLL_STATS, &json);
    }
}

/// Local `HH:MM:SS` for a unix timestamp (log-friendly).
fn fmt_log_time(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_default()
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
                PerItemMode::YouTubeApi => ctx.detect_youtube_api(&item).await,
                PerItemMode::KickApi => ctx.detect_kick_api(&item).await,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::models::Platform;
    use crate::store::Store;

    /// `load_poll_stats`/`save_poll_stats` round-trip through the settings
    /// store, and folding several outcomes for the same platform accumulates
    /// rather than overwrites (mirrors what `tick`'s per-tick block does).
    #[test]
    fn poll_stats_round_trip_and_accumulate() {
        let store = Store::open_in_memory().unwrap();

        // Empty store -> empty stats, not an error.
        let stats = load_poll_stats(&store);
        assert!(stats.by_platform.is_empty());

        let mut stats = load_poll_stats(&store);
        {
            let e = stats.by_platform.entry(Platform::Twitch.as_str().to_string()).or_default();
            e.polls += 1;
        }
        save_poll_stats(&store, &stats);

        // A second tick's worth of outcomes folds onto the first, not replaces it.
        let mut stats = load_poll_stats(&store);
        {
            let e = stats.by_platform.entry(Platform::Twitch.as_str().to_string()).or_default();
            e.polls += 1;
            e.errors += 1;
            e.last_error_at = Some(12345);
            e.last_error = "error sending request for url (...)".into();
        }
        save_poll_stats(&store, &stats);

        let stats = load_poll_stats(&store);
        let tw = &stats.by_platform[Platform::Twitch.as_str()];
        assert_eq!(tw.polls, 2, "accumulates across ticks, doesn't overwrite");
        assert_eq!(tw.errors, 1);
        assert_eq!(tw.last_error_at, Some(12345));
        assert!(tw.last_error.contains("error sending request"));
        // A platform that was never polled has no entry at all (not a
        // zeroed-out one) — the Stats view's "polls == 0 -> skip" check
        // relies on this via `.get(...).unwrap_or_default()`.
        assert!(!stats.by_platform.contains_key(Platform::YouTube.as_str()));
    }
}
