//! Shared poll scheduler.
//!
//! A single task loops over all enabled monitors, batches the due ones by
//! detection method, runs detection (Twitch Helix in one batched call;
//! scrape/generic probes concurrently with a cap), writes results back to the
//! store, and emits an [`AppEvent::MonitorState`] on any state change. This is
//! the low-idle-footprint design: one timer, batched work, no thread/process
//! per channel.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
#[allow(clippy::too_many_arguments)]
pub async fn run(
    ctx: Arc<DetectContext>,
    events: EventTx,
    live_tx: mpsc::UnboundedSender<LiveSignal>,
    active: ActiveSet,
    shutdown: Arc<AtomicBool>,
    jobs: crate::events::JobRegistry,
    manual_tx: mpsc::UnboundedSender<crate::events::ManualCommand>,
) {
    // Persisted across ticks for the `self.active`/DB consistency check (see
    // `tick`'s use of it) — monitor id -> when the mismatch was first
    // observed, and which ids have already been warned about for the
    // current, still-ongoing mismatch episode.
    let mut active_desync_since: HashMap<i64, Instant> = HashMap::new();
    let mut active_desync_warned: HashSet<i64> = HashSet::new();
    while !shutdown.load(Ordering::SeqCst) {
        // Ahead of the pause check on purpose: manual video downloads keep
        // running while live polling is paused, and their traffic still has
        // to reach the Stats view's history.
        fold_net_history(&ctx.store);
        // Live poll can be disabled from the Background view (a global pause of
        // detection/recording); idle-check for re-enable without polling.
        if !ctx.store.job_enabled("job_live_poll") {
            crate::app_core::sleep_cancellable(Duration::from_secs(10), &shutdown).await;
            continue;
        }
        let wait = tick(
            &ctx,
            &events,
            &live_tx,
            &active,
            &mut active_desync_since,
            &mut active_desync_warned,
            &manual_tx,
        )
        .await;
        crate::events::mark_job(&jobs, "Live poll", wait as i64);
        crate::app_core::sleep_cancellable(Duration::from_secs(wait), &shutdown).await;
    }
}

/// Drain the I/O sampler's per-minute download totals into `net_history`
/// (Stats view → Network / downloads). The sampler stamps each bucket with the
/// minute the traffic actually happened in, so folding on the scheduler's own
/// irregular cadence never smears the series — this just has to run often
/// enough to keep the undrained accumulator small.
fn fold_net_history(store: &crate::store::Store) {
    let net = crate::iomon::take_net_buckets();
    if net.is_empty() {
        return;
    }
    let rows: Vec<(i64, &str, u64)> =
        net.iter().map(|(t, kind, bytes)| (*t, kind.key(), *bytes)).collect();
    if let Err(e) = store.record_net_history(&rows) {
        warn!("scheduler: failed to persist download history: {e:#}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn tick(
    ctx: &Arc<DetectContext>,
    events: &EventTx,
    live_tx: &mpsc::UnboundedSender<LiveSignal>,
    active: &ActiveSet,
    active_desync_since: &mut HashMap<i64, Instant>,
    active_desync_warned: &mut HashSet<i64>,
    manual_tx: &mpsc::UnboundedSender<crate::events::ManualCommand>,
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
    // monitor id -> the currently-persisted (title, game, tags), captured
    // before this tick's writes, so a change can be detected and archived to
    // `monitor_stream_change` — the continuous history that spans live-not-
    // recording time too (recording-time changes come from `meta_watcher`).
    let mut prev_meta_values: HashMap<i64, (String, String, String)> = HashMap::new();

    let recording: std::collections::HashSet<i64> =
        active.lock().unwrap().keys().copied().collect();

    // Consistency check: a monitor whose DB row is still open (no `ended_at`)
    // but missing from `self.active` is exactly the anomaly behind the
    // 2026-07-24 Layna duplicate-recording incident — `self.active` silently
    // lost track of a still-healthy recording, and (per this same tick's own
    // `recording.contains(&m.id)` skip below) that's what let a second
    // process start for the same monitor. A brief mismatch is routine:
    // `self.active` frees its slot the instant the tool process exits,
    // before the DB row's `ended_at` gets written moments later during
    // finalize — so only warn once it's persisted past
    // `ACTIVE_DESYNC_WARN_SECS`, well beyond normal finalize latency, and
    // only once per continuous episode (not every tick after that).
    const ACTIVE_DESYNC_WARN_SECS: u64 = 30;
    match ctx.store.open_recordings_all() {
        Ok(open) => {
            let still_mismatched: HashSet<i64> = open
                .iter()
                .map(|&(mid, ..)| mid)
                .filter(|mid| !recording.contains(mid))
                .collect();
            active_desync_since.retain(|mid, _| still_mismatched.contains(mid));
            active_desync_warned.retain(|mid| still_mismatched.contains(mid));
            for &(mid, rec_id, started_at) in &open {
                if !still_mismatched.contains(&mid) {
                    continue;
                }
                let since = *active_desync_since.entry(mid).or_insert_with(Instant::now);
                if since.elapsed().as_secs() >= ACTIVE_DESYNC_WARN_SECS
                    && active_desync_warned.insert(mid)
                {
                    warn!(
                        monitor_id = mid,
                        open_rec_id = rec_id,
                        rec_started_at = started_at,
                        desynced_for_secs = since.elapsed().as_secs(),
                        "scheduler: DB shows an open recording for this monitor but \
                         self.active doesn't have it — self.active has desynced from reality \
                         (see the Layna incident, 2026-07-24); a duplicate recording may start \
                         on the next live poll for this monitor"
                    );
                }
            }
        }
        Err(e) => warn!("scheduler: active/DB consistency check failed to load recordings: {e:#}"),
    }

    // Twitch monitors' logins + whether they currently show a collab — inputs
    // to the per-tick "Stream Together" refresh below.
    let mut twitch_logins: HashMap<i64, String> = HashMap::new();
    let mut prev_collab: HashMap<i64, bool> = HashMap::new();

    for row in &rows {
        let m = &row.monitor;
        prev_state.insert(m.id, m.last_state.clone());
        prev_meta_values
            .insert(m.id, (row.last_title.clone(), row.last_game.clone(), row.last_tags.clone()));
        prev_live_since.insert(m.id, (m.last_live_since, m.last_live_since_approx));
        if m.platform() == crate::models::Platform::Twitch {
            if let Some(l) = crate::detectors::twitch_login(&m.url) {
                twitch_logins.insert(m.id, l);
            }
            prev_collab.insert(m.id, row.live_collab.is_some());
        }
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
        // (platform key, method label) -> (polls, errors) for this tick's
        // fold into the minute-resolution poll_history table (Stats graphs).
        let mut hist: HashMap<(&str, &str), (u64, u64)> = HashMap::new();
        for o in &outcomes {
            let (name, method, platform) = meta
                .get(&o.monitor_id)
                .map(|(n, m, p)| (n.as_str(), *m, *p))
                .unwrap_or(("?", "?", crate::models::Platform::Generic));
            stats.record(checked_at, platform, method, name, o.error, &o.detail);
            let h = hist.entry((platform.as_str(), method)).or_default();
            h.0 += 1;
            h.1 += u64::from(o.error);
        }
        save_poll_stats(&ctx.store, &stats);
        let counts: Vec<(&str, &str, u64, u64)> =
            hist.into_iter().map(|((p, m), (polls, errors))| (p, m, polls, errors)).collect();
        if let Err(e) = ctx.store.record_poll_history(checked_at, &counts) {
            warn!("scheduler: failed to persist poll history: {e:#}");
        }
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
        // Tags read as "the channel's usual tags" rather than clearing
        // offline — same rationale/pattern as `last_language`/`last_game_id`
        // below (`set_monitor_stream_extras`): a live poll that positively
        // carries tags always wins, but going offline (or a source that
        // never reports tags at all) must not blank a previously-seen value.
        let tags = o.stream_tags.as_deref().unwrap_or("");
        // Archive a title/category change to the continuous per-monitor
        // history the moment this poll observes it — independent of whether
        // anything is being recorded. Only while genuinely live: an
        // offline/errored tick's forced-empty title/game isn't a real
        // transition worth logging (recording-time changes come from
        // `meta_watcher` instead; the scheduler never polls an active
        // recording, so there's no overlap/double-logging between the two).
        if o.live && !o.error
            && let Some((prev_title, prev_game, prev_tags)) = prev_meta_values.get(&o.monitor_id)
        {
            // Also mirror into the take-shaped "not recorded" session's own
            // per-take history (`stream_meta_change`), if one is open for
            // this monitor — same table/shape `meta_watcher` writes to for
            // an actual recording, so the Streams grid's take-title lookup
            // (which only ever reads `stream_meta_change`) works unchanged
            // for a not-recorded take too. `at_secs` is relative to the
            // session's own `started_at`, same convention as a real take.
            let open_session = ctx.store.open_not_recorded_session(o.monitor_id).ok().flatten();
            if title != prev_title {
                let _ = ctx.store.insert_monitor_stream_change(
                    o.monitor_id, checked_at, "title", prev_title, title,
                );
                if let Some((rec_id, rec_started_at)) = open_session {
                    let _ = ctx.store.insert_meta_change(
                        rec_id, checked_at - rec_started_at, "title", prev_title, title,
                    );
                }
            }
            if game != prev_game {
                let _ = ctx.store.insert_monitor_stream_change(
                    o.monitor_id, checked_at, "category", prev_game, game,
                );
                if let Some((rec_id, rec_started_at)) = open_session {
                    let _ = ctx.store.insert_meta_change(
                        rec_id, checked_at - rec_started_at, "category", prev_game, game,
                    );
                }
            }
            // Only when the source actually carries tags: a Twitch poll with
            // none genuinely means "no tags", but a source that omits them
            // entirely (YouTube scrape) must not log a fake "cleared" event.
            if o.stream_tags.is_some() && tags != prev_tags {
                let _ = ctx.store.insert_monitor_stream_change(
                    o.monitor_id, checked_at, "tags", prev_tags, tags,
                );
            }
        }
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
        // Language + game id only when the source carries them (Twitch);
        // kept through offline as the channel's usual values.
        if (o.stream_language.is_some() || o.stream_game_id.is_some())
            && let Err(e) = ctx.store.set_monitor_stream_extras(
                o.monitor_id,
                o.stream_language.as_deref().unwrap_or(""),
                o.stream_game_id.as_deref().unwrap_or(""),
            )
        {
            warn!("scheduler: failed to persist stream extras for {}: {e:#}", o.monitor_id);
        }
        if o.stream_tags.is_some()
            && let Err(e) = ctx.store.set_monitor_tags(o.monitor_id, tags)
        {
            warn!("scheduler: failed to persist tags for {}: {e:#}", o.monitor_id);
        }
        // Detection's members-only verdict, kept for the capture path (see
        // `set_monitor_members_only`). Written every poll, including offline
        // ones, so it clears itself when the broadcast ends.
        if let Err(e) = ctx.store.set_monitor_members_only(o.monitor_id, o.members_only) {
            warn!("scheduler: failed to persist members-only flag for {}: {e:#}", o.monitor_id);
        }
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
        // The broadcast just ended: close any open not-recorded session for
        // it (see `insert_not_recorded_session`). Deliberately NOT on
        // "error" — a transient poll failure shouldn't fragment one
        // continuous broadcast's history into two sessions.
        if old_state == Some("live") && new_state == "offline" {
            match ctx.store.close_open_not_recorded_sessions(o.monitor_id, checked_at) {
                Ok(closed) if crate::downloader::vod::setting_true(&ctx.store, crate::downloader::vod::K_AUTO_BACKFILL_MISSED) => {
                    for rec_id in closed {
                        crate::downloader::vod::attempt_missed_stream_backfill(
                            ctx.clone(),
                            ctx.store.clone(),
                            events.clone(),
                            manual_tx.clone(),
                            rec_id,
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("scheduler: failed to close not_recorded session for {}: {e:#}", o.monitor_id),
            }
        }

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
            .with_stream_game(o.stream_game.clone())
            .with_stream_viewers(o.stream_viewers)
            .with_stream_tags(o.stream_tags.clone());
            let _ = live_tx.send(signal);
        }
    }

    // Fold this tick's live viewer/follower counts into the minute-bucket
    // viewer_history table (Channel Stats graphs + grid sparklines). Only
    // genuinely-live outcomes with a count sample; recording monitors are
    // sampled by meta_watcher instead (the scheduler skips them entirely).
    let samples: Vec<(i64, i64, Option<i64>, &str)> = outcomes
        .iter()
        .filter(|o| o.live && !o.error && o.stream_viewers.is_some())
        .map(|o| {
            (
                o.monitor_id,
                o.stream_viewers.unwrap_or(0),
                o.stream_followers,
                o.stream_id.as_deref().unwrap_or(""),
            )
        })
        .collect();
    if !samples.is_empty()
        && let Err(e) = ctx.store.record_viewer_samples(checked_at, &samples)
    {
        warn!("scheduler: failed to persist viewer history: {e:#}");
    }
    // Optional auto-downsample of old viewer history (runs at most daily;
    // a cheap two-settings-read no-op the rest of the time).
    if let Err(e) = ctx.store.maybe_auto_downsample_viewer_history(checked_at) {
        warn!("scheduler: viewer-history downsample failed: {e:#}");
    }
    // Logs-directory retention sweep (runs at most daily; see its own doc
    // comment for why this can't just be the one-shot startup prune).
    crate::app_paths::maybe_prune_old_logs(&ctx.store, checked_at);
    // Rolling database backup sweep (runs at most every `db_backup_interval_hours`).
    crate::db_backup::maybe_run_backup(&ctx.store, checked_at);
    // Rolling-recording expiry sweep (self-throttled to once a minute; a no-op
    // single query when nothing is due).
    crate::rolling::maybe_sweep_rolling(&ctx.store, events, checked_at).await;
    // Post-broadcast clip sweeps (+2h/+24h after a take ends). Self-throttled to
    // one indexed query per 5 minutes. This runs here rather than on the daily
    // loop because it is the only window in which Twitch still reports a clip's
    // `video_id`/`vod_offset` — once the parent VOD expires those are gone for
    // good, and with them any chance of reconstructing a deleted clip.
    crate::clips::maybe_sweep_post_broadcast(ctx, checked_at).await;
    // Chat sidecar harvest (self-throttled, a few finished takes per pass):
    // YouTube moderation actions into `stream_event`, and — for both platforms
    // — messages and identities into the chat index. Twitch records its own
    // moderation as it happens; YouTube's only exist inside yt-dlp's sidecar,
    // so they're read back once the capture is over. One read serves both.
    crate::chat_scan::maybe_sweep_chat_scan(
        &ctx.store,
        crate::chat_index::shared(),
        checked_at,
    )
    .await;
    // Fold pre-2026-08-05 login-keyed Twitch chatters into their real account
    // ids, a handful of Helix lookups at a time.
    crate::chat_scan::maybe_resolve_logins(ctx, checked_at).await;

    // ── Twitch "Stream Together" collab refresh ──
    // Piggybacks each monitor's own poll cadence (only monitors polled this
    // tick appear in `outcomes`). Live → fetch the Shared Chat session +
    // title @mentions and persist (`refresh_twitch_collab` is the shared
    // routine `meta_watcher` also uses while recording). Offline → end open
    // sessions and clear the live column, once (guarded by `prev_collab` so
    // permanently-offline channels don't cost a write per tick).
    let collab_sem = Arc::new(Semaphore::new(MAX_CONCURRENCY));
    let mut collab_set: JoinSet<()> = JoinSet::new();
    for o in &outcomes {
        let Some(login) = twitch_logins.get(&o.monitor_id).cloned() else {
            continue;
        };
        if o.error {
            continue; // no answer — keep whatever collab state we had
        }
        if o.live {
            let ctx = ctx.clone();
            let sem = collab_sem.clone();
            let (mid, bid, sid, title) = (
                o.monitor_id,
                o.broadcaster_id.clone(),
                o.stream_id.clone().unwrap_or_default(),
                o.stream_title.clone().unwrap_or_default(),
            );
            collab_set.spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore");
                ctx.refresh_twitch_collab(mid, &login, bid, &sid, &title).await;
            });
        } else if prev_collab.get(&o.monitor_id).copied().unwrap_or(false) {
            match ctx.store.end_open_collab_sessions(o.monitor_id, &[], &[], checked_at) {
                Ok(closed) => {
                    for names in closed {
                        let _ = ctx.store.insert_monitor_stream_change(
                            o.monitor_id, checked_at, "collab", &names, "",
                        );
                    }
                }
                Err(e) => warn!("scheduler: ending collab sessions failed: {e:#}"),
            }
            let _ = ctx.store.set_monitor_live_collab(o.monitor_id, "");
        }
    }
    while collab_set.join_next().await.is_some() {}

    // ── Hype-train confirmation (anonymous GQL) ──
    // One aliased batch request covers every live Twitch monitor polled this
    // tick (recording monitors are meta_watcher's job). Confirmed trains
    // land/refresh their stream_event row and calibrate the chat inference.
    // No false-positive sweep here: this cadence is per-monitor poll
    // intervals, too coarse to declare an inferred burst unconfirmed.
    let hype_targets: Vec<(i64, String, String)> = outcomes
        .iter()
        .filter(|o| o.live && !o.error)
        .filter_map(|o| {
            twitch_logins.get(&o.monitor_id).map(|l| {
                (o.monitor_id, l.clone(), o.stream_id.clone().unwrap_or_default())
            })
        })
        .collect();
    ctx.refresh_hype_trains(&hype_targets, false).await;
    // Creator Goals ride the same targets and cadence — one more anonymous
    // GQL request per poll for channels we're already asking about.
    ctx.refresh_creator_goals(&hype_targets).await;

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
        stats.record(12000, Platform::Twitch, "Helix API", "somechannel", false, "");
        save_poll_stats(&store, &stats);

        // A second tick's worth of outcomes folds onto the first, not replaces it.
        let mut stats = load_poll_stats(&store);
        stats.record(
            12345,
            Platform::Twitch,
            "Helix API",
            "somechannel",
            true,
            "error sending request for url (...)",
        );
        save_poll_stats(&store, &stats);

        let stats = load_poll_stats(&store);
        let tw = &stats.by_platform[Platform::Twitch.as_str()];
        assert_eq!(tw.polls, 2, "accumulates across ticks, doesn't overwrite");
        assert_eq!(tw.errors, 1);
        assert_eq!(tw.last_error_at, Some(12345));
        assert!(tw.last_error.contains("error sending request"));
        // The individual error round-trips through the persisted JSON too.
        assert_eq!(tw.recent_errors.len(), 1);
        assert_eq!(tw.recent_errors[0].monitor, "somechannel");
        assert_eq!(tw.recent_errors[0].method, "Helix API");
        // A platform that was never polled has no entry at all (not a
        // zeroed-out one) — the Stats view's "polls == 0 -> skip" check
        // relies on this via `.get(...).unwrap_or_default()`.
        assert!(!stats.by_platform.contains_key(Platform::YouTube.as_str()));
    }
}
