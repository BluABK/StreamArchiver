//! When clip discovery runs.
//!
//! Two drivers, and the split between them is the whole point:
//!
//! - **Post-broadcast** (`maybe_sweep_post_broadcast`) — at `ended_at + 2h` and
//!   `+ 24h`. This is the *only* window in which Twitch still reports
//!   `video_id`/`vod_offset`, so it is the only chance to capture the keys that
//!   make a clip recoverable after it is deleted. Driven off the persisted
//!   `recording.clip_sweep_stage` so a restart inside the 24 h window does not
//!   lose it.
//! - **Daily** (`run_clip_sweep`) — one channel per wake, ~24 h per rotation.
//!   Catches clips cut from *old* VODs today, because Helix's date filter is on
//!   the clip's own creation time, not the broadcast's. These arrive without
//!   recovery keys and that is simply the truth about them.
//!
//! Both idle to one settings read per tick while clips are off.

use super::*;
use crate::models::MonitorWithChannel;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, warn};

/// How often each channel is revisited by the daily sweep.
const CYCLE_SECS: u64 = 24 * 3600;
/// Never sweep two channels closer together than this, however few there are.
const MIN_GAP_SECS: u64 = 60;
/// Re-check cadence while the feature is off or there is nothing to do.
const IDLE_POLL_SECS: u64 = 300;
/// Overlap on the incremental window, absorbing clock skew and clips created
/// while the previous sweep was running.
const OVERLAP_SECS: i64 = 6 * 3600;
/// Post-broadcast sweeps are checked at most this often (a single indexed query
/// when nothing is due).
const POST_THROTTLE_SECS: i64 = 300;
/// Settings key holding the last post-broadcast check, so the throttle survives
/// a restart rather than firing a burst on every launch.
const K_POST_LAST_CHECK: &str = "clips_post_sweep_last_check";
/// Takes handled per post-broadcast pass — bounded so a backlog drains steadily
/// instead of firing hundreds of Helix queries in one tick.
const POST_BATCH: usize = 4;
/// Known clips re-checked per sweep. 100 is Helix's per-call maximum, so this is
/// exactly one extra request.
const LIVENESS_BATCH: i64 = 100;
/// Rebuilds attempted automatically per sweep. A channel purge can take out
/// hundreds at once; recovering them all in one pass would hammer the CDN and
/// the disk together.
const AUTO_RECOVER_BATCH: usize = 3;

/// Is clip indexing on at all?
pub fn clips_enabled(store: &Store) -> bool {
    store
        .get_setting(K_CLIPS_ENABLED)
        .ok()
        .flatten()
        .as_deref()
        .is_none_or(|v| v != "0")
}

/// Resolve the sweep target for a monitor, or `None` when it is not a Twitch
/// channel we can enumerate clips for.
async fn target_for(ctx: &Arc<DetectContext>, row: &MonitorWithChannel) -> Option<SweepTarget> {
    let login = crate::detectors::twitch_login(&row.monitor.url)?;
    let (client_id, token) = ctx.twitch_helix_auth().await.ok()?;
    let user_id = ctx.twitch_user_id(&client_id, &token, &login).await?;
    Some(SweepTarget {
        channel_id: row.channel.id,
        monitor_id: row.monitor.id,
        login,
        user_id,
    })
}

/// Sweep one monitor over `window`, then link and snapshot.
///
/// Returns the number of clips seen. On failure the sweep cursor is **not**
/// advanced — per the house contract an `Err` means "we weren't watching", and
/// moving the high-water mark past a window we failed to read would leave a
/// permanent hole that no later sweep would ever revisit.
pub async fn sweep_monitor(
    ctx: &Arc<DetectContext>,
    row: &MonitorWithChannel,
    window: Window,
    now: i64,
) -> anyhow::Result<usize> {
    let Some(t) = target_for(ctx, row).await else {
        return Ok(0);
    };
    let store = &ctx.store;
    let res = match sweep_window_deep(ctx, store, &t, window, now).await {
        Ok(r) => r,
        Err(e) => {
            let _ = store.set_clip_sweep_error(t.monitor_id, &format!("{e:#}"));
            return Err(e);
        }
    };
    if res.seen > 0 {
        let _ = store.link_clips_to_recordings();
        snapshot_vod_folders(ctx, t.monitor_id).await;
    }
    // Only now, with every window drained, is it safe to move the mark.
    let _ = store.set_clip_swept(t.monitor_id, window.end);
    check_liveness(ctx, t.channel_id, now).await;
    Ok(res.seen)
}

/// Re-check clips we already know about, and act on the ones that have gone.
///
/// **A failed hydrate marks nothing.** Absence is the deletion signal here, so
/// treating a 500 as "they're all gone" would both lie and fire a recovery
/// attempt at a clip that is perfectly fine — the `Err` = "we weren't watching"
/// contract matters more here than anywhere else in the module.
async fn check_liveness(ctx: &Arc<DetectContext>, channel_id: i64, now: i64) {
    let store = &ctx.store;
    let Ok(known) = store.clips_to_recheck(channel_id, LIVENESS_BATCH) else {
        return;
    };
    if known.is_empty() {
        return;
    }
    let ids: Vec<String> = known.iter().map(|c| c.slug.clone()).collect();
    let alive = match hydrate_clip_ids(ctx, &ids).await {
        Ok(v) => v,
        Err(e) => {
            debug!("clips: liveness check skipped (not authoritative): {e:#}");
            return;
        }
    };
    let alive: std::collections::HashSet<String> = alive
        .iter()
        .filter_map(|v| v["id"].as_str().map(str::to_string))
        .collect();
    let gone: Vec<String> = ids.into_iter().filter(|s| !alive.contains(s)).collect();
    if gone.is_empty() {
        return;
    }
    let n = store
        .mark_clips_gone(crate::models::Platform::Twitch, &gone, now)
        .unwrap_or(0);
    if n == 0 {
        return;
    }
    warn!(channel_id, gone = n, "clips: vanished upstream since the last sweep");

    // Auto-attempt once, then leave it to the manual action. Only for clips we
    // never archived — one that is already on disk has nothing to recover.
    if !auto_recover_on(store) {
        return;
    }
    let client = ctx.http_client();
    for slug in gone.iter().take(AUTO_RECOVER_BATCH) {
        let Ok(Some(c)) = store.clip_by_slug(crate::models::Platform::Twitch, slug) else {
            continue;
        };
        if c.is_archived() || c.dl_attempts > 0 {
            continue;
        }
        let ok = super::recover::recover_clip(store, &client, c.id, 4).await;
        debug!(slug = %slug, ok, "clips: auto-recovery attempt");
    }
}

/// Attempt one automatic rebuild when a clip is first found missing. On by
/// default — the window in which a rebuild can still succeed closes with the
/// parent VOD, so waiting for a human is often waiting too long.
fn auto_recover_on(store: &Store) -> bool {
    store
        .get_setting("clips_auto_recover")
        .ok()
        .flatten()
        .as_deref()
        .is_none_or(|v| v != "0")
}

/// Cache the CDN folder of any VOD we just learned about but have not resolved.
///
/// This is the cheap half of the perishable-key story. `gql_vod_info` answers in
/// one request **while the VOD is alive**; once it expires the answer is gone
/// forever and recovering a clip from it would need the ~2,400-HEAD host probe
/// that `find_live_playlist` performs — acceptable once for a VOD, never per
/// clip. One request now buys a free, exact answer later.
async fn snapshot_vod_folders(ctx: &Arc<DetectContext>, monitor_id: i64) {
    let store = &ctx.store;
    let Ok(vods) = store.unsnapshotted_clip_vods(monitor_id, 8) else {
        return;
    };
    let client = ctx.http_client();
    for vod_id in vods {
        match crate::recovery::gql_vod_info(&client, &vod_id).await {
            Ok(info) => {
                let _ = store.put_vod_cdn(&crate::store::VodCdnRow {
                    vod_id: vod_id.clone(),
                    host: info.host,
                    folder: info.folder,
                    login: info.login,
                    broadcast_id: info.broadcast_id,
                    start_epoch: info.start_epoch,
                    learned_at: crate::models::now_unix(),
                });
                debug!("clips: cached CDN folder for VOD {vod_id}");
            }
            Err(e) => {
                // Deleted/private/sub-only VOD — expected, and not worth a warn
                // per clip. The clip stays recoverable only via its own object.
                debug!("clips: no CDN folder for VOD {vod_id}: {e:#}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(REQUEST_PACE_MS)).await;
    }
}

/// Post-broadcast sweeps, called from the scheduler tick.
///
/// Self-throttled to one check per [`POST_THROTTLE_SECS`], and a single indexed
/// query when nothing is due — the same shape as `rolling::maybe_sweep_rolling`.
pub async fn maybe_sweep_post_broadcast(ctx: &Arc<DetectContext>, now: i64) {
    let store = &ctx.store;
    if !clips_enabled(store) {
        return;
    }
    let last = store
        .get_setting(K_POST_LAST_CHECK)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    if now - last < POST_THROTTLE_SECS {
        return;
    }
    let _ = store.set_setting(K_POST_LAST_CHECK, &now.to_string());

    let offsets = store
        .get_setting(K_CLIPS_POST_OFFSETS)
        .ok()
        .flatten()
        .map(|v| parse_post_offsets(&v))
        .unwrap_or(DEFAULT_POST_OFFSETS);
    let Ok(due) = store.recordings_due_clip_sweep(now, offsets.0, offsets.1) else {
        return;
    };
    if due.is_empty() {
        return;
    }
    let rows = store.list_monitors_with_channels().unwrap_or_default();

    for (rec_id, monitor_id, stage) in due.into_iter().take(POST_BATCH) {
        let Some(row) = rows.iter().find(|r| r.monitor.id == monitor_id) else {
            // The instance is gone; nothing will ever sweep this take.
            let _ = store.bump_clip_sweep_stage(rec_id, 2);
            continue;
        };
        let Ok(Some(rec)) = store.get_recording(rec_id) else {
            let _ = store.bump_clip_sweep_stage(rec_id, 2);
            continue;
        };
        // Window the broadcast itself plus a day: clips are made during the
        // stream and for a while after, and Helix filters on the clip's own
        // creation time.
        let from = rec.went_live_at.unwrap_or(rec.started_at) - 3600;
        let window = Window::new(from, now);
        match sweep_monitor(ctx, row, window, now).await {
            Ok(n) => {
                let _ = store.bump_clip_sweep_stage(rec_id, stage + 1);
                if n > 0 {
                    info!(
                        recording_id = rec_id,
                        channel = %row.channel.name,
                        stage,
                        clips = n,
                        "clips: post-broadcast sweep (recovery keys are only \
                         available this close to the broadcast)"
                    );
                }
            }
            Err(e) => {
                // Leave the stage alone so the next pass retries — this is the
                // one sweep whose window closes for good.
                warn!(
                    recording_id = rec_id,
                    channel = %row.channel.name,
                    "clips: post-broadcast sweep failed, will retry: {e:#}"
                );
            }
        }
    }
}

/// The daily per-channel sweep loop. Spawned once at startup.
pub async fn run_clip_sweep(
    ctx: Arc<DetectContext>,
    manual_tx: tokio::sync::mpsc::UnboundedSender<crate::events::ManualCommand>,
    shutdown: Arc<AtomicBool>,
    jobs: crate::events::JobRegistry,
) {
    crate::app_core::sleep_cancellable(std::time::Duration::from_secs(90), &shutdown).await;

    let mut queue: Vec<MonitorWithChannel> = Vec::new();
    let mut total: u64 = 1;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        if !clips_enabled(&ctx.store) {
            queue.clear();
            crate::app_core::sleep_cancellable(
                std::time::Duration::from_secs(IDLE_POLL_SECS),
                &shutdown,
            )
            .await;
            continue;
        }
        if queue.is_empty() {
            queue = ctx
                .store
                .list_monitors_with_channels()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.automation_on() && r.monitor.url.contains("twitch.tv/"))
                .collect();
            total = (queue.len() as u64).max(1);
        }
        let Some(row) = queue.pop() else {
            crate::app_core::sleep_cancellable(
                std::time::Duration::from_secs(IDLE_POLL_SECS),
                &shutdown,
            )
            .await;
            continue;
        };

        let now = crate::models::now_unix();
        let state = ctx.store.clip_sweep_state(row.monitor.id).unwrap_or_default();
        let window = incremental_window(state.last_swept_at, now, row.channel.created_at);
        match sweep_monitor(&ctx, &row, window, now).await {
            Ok(n) if n > 0 => {
                info!(
                    monitor_id = row.monitor.id,
                    channel = %row.channel.name,
                    clips = n,
                    "clips: daily sweep"
                );
            }
            Ok(_) => {}
            Err(e) => warn!(channel = %row.channel.name, "clips: daily sweep failed: {e:#}"),
        }

        // One historical window per pass, after the incremental sweep — the
        // cheap, always-on work must never be starved by the expensive opt-in
        // one, and pacing it a window per channel-visit spreads thousands of
        // requests over days rather than minutes.
        if backfill_on(&ctx.store) {
            backfill_step(&ctx, &row, now).await;
        }

        // Top the download queue up rather than enqueueing everything at once:
        // a channel can hold ten thousand pending clips, and the gate is
        // per-channel so most candidates are skipped anyway.
        let started = super::drain_clip_queue(&ctx.store, &manual_tx);
        if started > 0 {
            debug!("clips: queued {started} download(s)");
        }

        let gap = (CYCLE_SECS / total).max(MIN_GAP_SECS);
        crate::events::mark_job(&jobs, "Clip sweep", gap as i64);
        crate::app_core::sleep_cancellable(std::time::Duration::from_secs(gap), &shutdown).await;
    }
}

/// How far back one backfill pass reaches. A month at a time keeps each pass
/// bounded while rarely needing more than a split or two.
const BACKFILL_STRIDE_SECS: i64 = 30 * 24 * 3600;
/// Nothing on Twitch predates this, so it is the hard floor for a channel with
/// no recorded creation date — an unbounded walk would never terminate.
const TWITCH_EPOCH: i64 = 1_293_840_000; // 2011-01-01

/// The next window a historical backfill should sweep, walking **newest first**.
///
/// Newest-first is deliberate: the recent windows are the only ones whose clips
/// still carry recovery keys, so an interrupted or abandoned backfill has
/// already captured everything that could still be rebuilt. `None` means the
/// walk has reached the floor and the channel's history is complete.
pub fn next_backfill_window(backfill_until: i64, floor: i64, now: i64) -> Option<Window> {
    let floor = if floor > 0 { floor } else { TWITCH_EPOCH };
    // First pass starts at "now" and works back.
    let end = if backfill_until > 0 { backfill_until } else { now };
    if end <= floor {
        return None;
    }
    let start = (end - BACKFILL_STRIDE_SECS).max(floor);
    Some(Window::new(start, end))
}

/// Is the expensive historical backfill enabled?
///
/// Off by default. The daily and post-broadcast sweeps are a handful of requests
/// each; this walks a channel's entire history a month at a time, bisecting
/// wherever the ~1000 cap truncates, and is thousands of requests per channel.
fn backfill_on(store: &Store) -> bool {
    matches!(
        store.get_setting(K_CLIPS_BACKFILL).ok().flatten().as_deref(),
        Some("1") | Some("true")
    )
}

/// Advance one channel's historical backfill by a single window.
///
/// Resumable across restarts: progress lives in `clip_sweep.backfill_until`, and
/// a window is only marked done once it has fully drained — a failure leaves the
/// cursor where it was so the same window is retried rather than skipped.
async fn backfill_step(ctx: &Arc<DetectContext>, row: &MonitorWithChannel, now: i64) -> usize {
    let store = &ctx.store;
    let state = store.clip_sweep_state(row.monitor.id).unwrap_or_default();
    if state.backfill_done {
        return 0;
    }
    let Some(window) = next_backfill_window(state.backfill_until, row.channel.created_at, now)
    else {
        let _ = store.set_clip_backfill(row.monitor.id, state.backfill_until, true);
        info!(
            channel = %row.channel.name,
            "clips: historical backfill complete"
        );
        return 0;
    };
    let Some(t) = target_for(ctx, row).await else {
        return 0;
    };
    match sweep_window_deep(ctx, store, &t, window, now).await {
        Ok(res) => {
            let _ = store.link_clips_to_recordings();
            let _ = store.set_clip_backfill(row.monitor.id, window.start, false);
            if res.seen > 0 {
                info!(
                    channel = %row.channel.name,
                    clips = res.seen,
                    from = %fmt_rfc3339(window.start),
                    "clips: backfilled a window of history"
                );
            }
            res.seen
        }
        Err(e) => {
            // Cursor untouched: retry this window next pass rather than
            // stepping over it and leaving a hole nothing revisits.
            warn!(channel = %row.channel.name, "clips: backfill window failed: {e:#}");
            let _ = store.set_clip_sweep_error(row.monitor.id, &format!("{e:#}"));
            0
        }
    }
}

/// The window an incremental sweep should cover.
///
/// Overlaps the previous high-water mark by [`OVERLAP_SECS`] so a clip created
/// while the last sweep was mid-flight is not skipped; a first-ever sweep starts
/// from when we first knew the channel (falling back to a year) rather than the
/// epoch, since an unbounded window would just cap immediately.
pub fn incremental_window(last_swept_at: i64, now: i64, channel_created_at: i64) -> Window {
    let start = if last_swept_at > 0 {
        last_swept_at - OVERLAP_SECS
    } else if channel_created_at > 0 {
        channel_created_at
    } else {
        now - 365 * 24 * 3600
    };
    Window::new(start.min(now), now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_window_overlaps_the_previous_high_water_mark() {
        // The overlap is what stops a clip created mid-sweep from falling into
        // the gap between two windows and never being seen again.
        let w = incremental_window(1_000_000, 1_100_000, 0);
        assert_eq!(w.start, 1_000_000 - OVERLAP_SECS);
        assert_eq!(w.end, 1_100_000);
        assert!(w.start < 1_000_000);
    }

    #[test]
    fn a_first_sweep_starts_at_the_channel_not_the_epoch() {
        // Starting at 0 would make the very first window span decades and cap
        // instantly, hiding everything but the 1000 most-viewed clips.
        let w = incremental_window(0, 2_000_000, 1_500_000);
        assert_eq!(w.start, 1_500_000);

        // With no creation date, fall back to a bounded year rather than 1970.
        let w = incremental_window(0, 2_000_000, 0);
        assert_eq!(w.start, 2_000_000 - 365 * 24 * 3600);
    }

    #[test]
    fn the_backfill_walks_newest_first_and_terminates_at_the_floor() {
        // Newest-first matters: those windows hold the clips that still carry
        // recovery keys, so an abandoned backfill has already got the valuable
        // part.
        let now = 1_000_000_000;
        let floor = now - 90 * 24 * 3600;

        let first = next_backfill_window(0, floor, now).unwrap();
        assert_eq!(first.end, now, "the first pass starts at now");
        assert_eq!(first.start, now - BACKFILL_STRIDE_SECS);

        let second = next_backfill_window(first.start, floor, now).unwrap();
        assert_eq!(second.end, first.start, "contiguous — no gap between passes");
        assert!(second.start < second.end);

        let third = next_backfill_window(second.start, floor, now).unwrap();
        assert_eq!(third.start, floor, "clamped to the channel's own start");

        // Reaching the floor ends the walk rather than looping forever.
        assert!(next_backfill_window(third.start, floor, now).is_none());
    }

    #[test]
    fn a_channel_with_no_creation_date_still_terminates() {
        // An unbounded walk back to 1970 would never finish; Twitch itself
        // predates nothing before 2011.
        let now = 1_400_000_000;
        let mut cursor = 0;
        for _ in 0..1000 {
            match next_backfill_window(cursor, 0, now) {
                Some(w) => cursor = w.start,
                None => break,
            }
        }
        assert!(next_backfill_window(cursor, 0, now).is_none());
        assert_eq!(cursor, TWITCH_EPOCH);
    }

    #[test]
    fn a_window_never_ends_before_it_starts() {
        // A clock that jumped backwards (or a mark written by a future build)
        // must not produce a reversed window that Helix would reject.
        let w = incremental_window(9_000_000, 1_000_000, 0);
        assert!(w.start <= w.end);
        assert_eq!(w.end, 1_000_000);
    }
}
