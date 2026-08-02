//! Retroactive discovery for [`crate::downloader::vod::K_AUTO_BACKFILL_MISSED`]:
//! find VODs a platform still knows about that don't correlate to anything
//! this app has a `Recording` row for — the "app wasn't even running"
//! case, as opposed to a `not_recorded` row (which already exists because
//! the app *was* running, just not recording; see
//! `Store::insert_not_recorded_session`).
//!
//! Discovery's only job is to materialize a matching gap as an ordinary
//! `not_recorded` row (`Store::insert_discovered_not_recorded`) and then
//! kick off the exact same [`attempt_missed_stream_backfill`] the
//! session-close trigger uses — from that point on it's indistinguishable
//! from a live-witnessed one: same grid rendering, same context-menu
//! actions. `Store::insert_discovered_not_recorded` itself is the dedup
//! guard against repeated scans (exact `stream_id` match); the time-window
//! checks below exist only for platforms (Kick) whose listing id isn't the
//! same id space as `Recording.stream_id`, to avoid filing a broadcast we
//! already have a real take for as "missed" too.

use super::*;

/// Hard cap on pages fetched from a paginated listing per scan, so a
/// channel with an enormous archive can't loop unboundedly. Mirrors
/// `imports.rs::MAX_PAGES`'s reasoning at a smaller scale — discovery runs
/// unattended, so it errs conservative.
const MAX_LISTING_PAGES: usize = 20;

/// Periodic per-channel discovery sweep — the automatic half of
/// `K_AUTO_BACKFILL_MISSED` (the other half is the not_recorded
/// session-close trigger in `vod.rs`/`scheduler.rs`). One full rotation
/// through every enabled monitor takes roughly `CYCLE_SECS`; idles cheaply
/// (one settings read per tick) while the setting is off. Spawned once at
/// startup alongside the other periodic refreshers (`detectors::
/// refresh_community_posts` etc. — same shape).
pub async fn run_missed_stream_backfill_sweep(
    ctx: Arc<DetectContext>,
    events: EventTx,
    manual_tx: mpsc::UnboundedSender<ManualCommand>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    jobs: crate::events::JobRegistry,
) {
    use std::sync::atomic::Ordering;
    const CYCLE_SECS: u64 = 24 * 3600; // ~how often each channel is revisited
    const MIN_GAP_SECS: u64 = 30; // never scan two channels closer than this
    const IDLE_POLL_SECS: u64 = 300; // re-check when the setting is off / no channels

    crate::app_core::sleep_cancellable(std::time::Duration::from_secs(60), &shutdown).await;

    let mut queue: Vec<MonitorWithChannel> = Vec::new();
    let mut total: u64 = 1;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        if !setting_true(&ctx.store, K_AUTO_BACKFILL_MISSED) {
            queue.clear();
            crate::app_core::sleep_cancellable(std::time::Duration::from_secs(IDLE_POLL_SECS), &shutdown).await;
            continue;
        }
        if queue.is_empty() {
            queue = ctx.store.list_monitors_with_channels().unwrap_or_default().into_iter().filter(|r| r.automation_on()).collect();
            total = (queue.len() as u64).max(1);
        }
        let Some(row) = queue.pop() else {
            crate::app_core::sleep_cancellable(std::time::Duration::from_secs(IDLE_POLL_SECS), &shutdown).await;
            continue;
        };
        let found = discover_missed_streams_for_monitor(ctx.clone(), ctx.store.clone(), events.clone(), manual_tx.clone(), &row).await;
        if found > 0 {
            tracing::info!(
                monitor_id = row.monitor.id,
                channel = %row.channel.name,
                found,
                "missed-stream backfill: discovered previously-untracked broadcast(s)"
            );
        }
        let gap = (CYCLE_SECS / total).max(MIN_GAP_SECS);
        crate::events::mark_job(&jobs, "Missed-stream backfill scan", gap as i64);
        crate::app_core::sleep_cancellable(std::time::Duration::from_secs(gap), &shutdown).await;
    }
}

/// Run one discovery pass for a single channel/instance, dispatching by
/// platform, and immediately attempt-backfill anything newly found. Returns
/// the number of newly-discovered (previously untracked) broadcasts, for a
/// log line / manual-action toast. Used by both the periodic sweep and the
/// manual "🔎 Scan for missed streams" action — there is no separate code
/// path for "on demand" vs. "automatic", only a different caller.
pub(crate) async fn discover_missed_streams_for_monitor(
    ctx: Arc<DetectContext>,
    store: Arc<Store>,
    events: EventTx,
    manual_tx: mpsc::UnboundedSender<ManualCommand>,
    row: &MonitorWithChannel,
) -> usize {
    let monitor_id = row.monitor.id;
    let found: Vec<i64> = match row.monitor.platform() {
        Platform::Twitch => discover_twitch(&ctx, &store, monitor_id, &row.monitor.url).await,
        Platform::Kick => discover_kick(&ctx, &store, monitor_id, &row.monitor.url).await,
        Platform::YouTube => discover_via_ytdlp(&store, monitor_id, &row.monitor.url, "/streams", true).await,
        // Best-effort only (no reliable listing/matching for these): reuse
        // the same yt-dlp flat-playlist path against the bare channel URL,
        // time-window matched like Kick since there's no id correlation to
        // lean on either.
        Platform::Nrk | Platform::Nebula | Platform::Generic => {
            discover_via_ytdlp(&store, monitor_id, &row.monitor.url, "", false).await
        }
    };
    for rec_id in &found {
        attempt_missed_stream_backfill(ctx.clone(), store.clone(), events.clone(), manual_tx.clone(), *rec_id);
    }
    found.len()
}

/// True if `candidate_start` falls within
/// [`crate::vod_archive::VOD_MATCH_WINDOW_SECS`] of slack around any
/// `(started_at, ended_at)` window in `existing` — the fallback correlation
/// for platforms (Kick, generic yt-dlp listings) whose listing id isn't in
/// the same space as `Recording.stream_id`. Takes plain `(started_at,
/// ended_at)` pairs rather than full `Recording`s so it stays trivially
/// testable.
fn overlaps_known_recording(existing: &[(i64, Option<i64>)], candidate_start: i64) -> bool {
    let slack = crate::vod_archive::VOD_MATCH_WINDOW_SECS;
    existing.iter().any(|&(started_at, ended_at)| {
        let end = ended_at.unwrap_or(candidate_start.max(started_at));
        candidate_start >= started_at - slack && candidate_start <= end + slack
    })
}

/// Twitch: paginate Helix `/videos?type=archive`, insert a `not_recorded`
/// row for each VOD whose own `stream_id` isn't already known for this
/// monitor. Exact-id correlation (Helix archive videos carry the
/// originating broadcast id), same field `resolve_twitch_vod_by_stream`
/// reads for the manual/automatic single-VOD lookups.
async fn discover_twitch(ctx: &Arc<DetectContext>, store: &Arc<Store>, monitor_id: i64, monitor_url: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let Some(login) = crate::detectors::twitch_login(monitor_url) else {
        return out;
    };
    let Ok((client_id, token)) = ctx.twitch_helix_auth().await else {
        return out;
    };
    let Some(user_id) = ctx.twitch_user_id(&client_id, &token, &login).await else {
        return out;
    };
    let client = ctx.http_client();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LISTING_PAGES {
        let mut query: Vec<(&str, &str)> = vec![("user_id", user_id.as_str()), ("type", "archive"), ("first", "100")];
        if let Some(c) = &cursor {
            query.push(("after", c.as_str()));
        }
        let Ok(resp) = client
            .get("https://api.twitch.tv/helix/videos")
            .header("Client-Id", &client_id)
            .bearer_auth(&token)
            .query(&query)
            .send()
            .await
        else {
            break;
        };
        if !resp.status().is_success() {
            break;
        }
        let Ok(v) = resp.json::<serde_json::Value>().await else {
            break;
        };
        let Some(data) = v["data"].as_array() else {
            break;
        };
        if data.is_empty() {
            break;
        }
        for item in data {
            let Some(stream_id) = item["stream_id"].as_str().filter(|s| !s.is_empty()) else {
                continue; // no broadcast id on this video — nothing to correlate/recover by
            };
            let Some(created_at) = item["created_at"].as_str().and_then(crate::detectors::parse_rfc3339) else {
                continue;
            };
            let duration = item["duration"].as_str().map(parse_twitch_duration).unwrap_or(0);
            let title = item["title"].as_str().unwrap_or("");
            if let Ok(Some(rec_id)) =
                store.insert_discovered_not_recorded(monitor_id, created_at, created_at + duration, stream_id, title)
            {
                out.push(rec_id);
            }
        }
        cursor = v["pagination"]["cursor"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    out
}

/// Twitch Helix's `duration` field (e.g. `"1h2m3s"`, `"45m3s"`, `"29s"`) to
/// whole seconds. Missing/unparseable pieces are just absent, not an error.
fn parse_twitch_duration(s: &str) -> i64 {
    let mut total = 0i64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let Ok(n) = num.parse::<i64>() else {
            num.clear();
            continue;
        };
        total += match c {
            'h' => n * 3600,
            'm' => n * 60,
            's' => n,
            _ => 0,
        };
        num.clear();
    }
    total
}

/// Kick: fetch the channel's full videos listing (no window filter, unlike
/// the reactive `resolve_kick_vod`) and file anything that doesn't overlap
/// an already-known recording's time window — Kick's listing uuid isn't
/// the same id space as `Recording.stream_id` (the livestream id), so
/// there's no exact-id correlation available here.
async fn discover_kick(ctx: &Arc<DetectContext>, store: &Arc<Store>, monitor_id: i64, monitor_url: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let Some(slug) = crate::vod_archive::kick_slug(monitor_url) else {
        return out;
    };
    let listing = crate::vod_archive::list_kick_vods(&ctx.http_client(), &slug).await;
    if listing.is_empty() {
        return out;
    }
    let Ok(existing) = store.recordings_for_monitor(monitor_id) else {
        return out;
    };
    let existing: Vec<(i64, Option<i64>)> = existing.iter().map(|r| (r.started_at, r.ended_at)).collect();
    // Kick's listing doesn't reliably expose a duration — a rough 2h
    // placeholder is close enough for grid display; the real content length
    // is whatever the download itself reports once fetched.
    const ROUGH_DURATION_SECS: i64 = 2 * 3600;
    for v in listing {
        if overlaps_known_recording(&existing, v.start) {
            continue;
        }
        if let Ok(Some(rec_id)) = store.insert_discovered_not_recorded(
            monitor_id,
            v.start,
            v.start + ROUGH_DURATION_SECS,
            &v.uuid,
            &v.title,
        ) {
            out.push(rec_id);
        }
    }
    out
}

/// YouTube (and best-effort Generic/Nrk/Nebula): list a channel's videos via
/// `yt-dlp --flat-playlist`, the only listing mechanism available for these
/// (no in-tree API). `id_is_stream_id`: true for YouTube, where the video id
/// IS `Recording.stream_id` (a live stream keeps the same id as its VOD) so
/// discovery can dedup exactly the same way Twitch does; false for the
/// generic path, which falls back to the same time-window overlap check
/// Kick uses. `suburl` is appended to the channel URL (YouTube's `/streams`
/// tab); empty for the generic path, which just lists the channel URL as-is.
async fn discover_via_ytdlp(
    store: &Arc<Store>,
    monitor_id: i64,
    channel_url: &str,
    suburl: &str,
    id_is_stream_id: bool,
) -> Vec<i64> {
    let mut out = Vec::new();
    let target = format!("{}{suburl}", channel_url.trim_end_matches('/'));
    let bin = setting_str(store, "ytdlp_binary_path"); // ui.rs K_YTDLP_BINARY
    let program = if bin.trim().is_empty() { "yt-dlp".to_string() } else { bin };
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg("--quiet")
        .arg("--no-warnings")
        .arg("--flat-playlist")
        .arg("--print")
        .arg("%(id)s\t%(title)s\t%(timestamp)s\t%(upload_date)s")
        .arg(&target);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return out,
    };
    let Ok(Ok(output)) = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output()).await
    else {
        return out;
    };
    if !output.status.success() {
        return out;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let existing: Vec<(i64, Option<i64>)> = if id_is_stream_id {
        Vec::new()
    } else {
        store
            .recordings_for_monitor(monitor_id)
            .ok()
            .unwrap_or_default()
            .iter()
            .map(|r| (r.started_at, r.ended_at))
            .collect()
    };
    for line in text.lines() {
        let mut cols = line.splitn(4, '\t');
        let (Some(id), Some(title), Some(ts), Some(upload_date)) = (cols.next(), cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        if id.is_empty() || id == "NA" {
            continue;
        }
        let started_at = ts
            .parse::<f64>()
            .ok()
            .map(|f| f as i64)
            .or_else(|| parse_yyyymmdd(upload_date));
        let Some(started_at) = started_at else {
            continue; // no usable date — skip rather than guess (best-effort path)
        };
        if id_is_stream_id {
            if let Ok(Some(rec_id)) =
                store.insert_discovered_not_recorded(monitor_id, started_at, started_at, id, title)
            {
                out.push(rec_id);
            }
        } else {
            if overlaps_known_recording(&existing, started_at) {
                continue;
            }
            if let Ok(Some(rec_id)) =
                store.insert_discovered_not_recorded(monitor_id, started_at, started_at, id, title)
            {
                out.push(rec_id);
            }
        }
    }
    out
}

/// `YYYYMMDD` (yt-dlp's `upload_date`) to a UTC midnight epoch, best-effort.
fn parse_yyyymmdd(s: &str) -> Option<i64> {
    if s.len() != 8 {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[4..6].parse().ok()?;
    let day: u32 = s[6..8].parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_twitch_duration_handles_all_component_shapes() {
        assert_eq!(parse_twitch_duration("1h2m3s"), 3723);
        assert_eq!(parse_twitch_duration("45m3s"), 2703);
        assert_eq!(parse_twitch_duration("29s"), 29);
        assert_eq!(parse_twitch_duration(""), 0);
        assert_eq!(parse_twitch_duration("garbage"), 0);
    }

    #[test]
    fn parse_yyyymmdd_known_value() {
        assert_eq!(parse_yyyymmdd("20231114"), Some(1_699_920_000));
        assert_eq!(parse_yyyymmdd(""), None);
        assert_eq!(parse_yyyymmdd("2023111"), None);
        assert_eq!(parse_yyyymmdd("20231399"), None); // invalid month/day
    }

    #[test]
    fn overlaps_known_recording_matches_within_slack_only() {
        let existing = vec![(1_000, Some(2_000))];
        assert!(overlaps_known_recording(&existing, 1_500)); // inside window
        assert!(overlaps_known_recording(&existing, 2_000 + crate::vod_archive::VOD_MATCH_WINDOW_SECS)); // right at slack edge
        assert!(!overlaps_known_recording(&existing, 2_000 + crate::vod_archive::VOD_MATCH_WINDOW_SECS + 1));
        assert!(!overlaps_known_recording(&existing, 500 - crate::vod_archive::VOD_MATCH_WINDOW_SECS - 1));
    }
}
