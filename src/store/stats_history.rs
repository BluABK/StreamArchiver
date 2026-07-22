//! Channel stats history (schema v59): viewer/follower time series + discrete
//! stream events (subs, bits, raids).
//!
//! `viewer_history` gets one row per monitor per minute while live; `viewers`
//! is the bucket's peak so MAX-aggregation (both query-time GROUP BYs and the
//! offline downsampler) preserves spikes. `span_secs` records each row's
//! bucket width (60 raw, 600 after downsampling) so airtime is
//! `SUM(span_secs)` and averages can weight by coverage even when resolutions
//! are mixed. Data is kept forever by default; `downsample_viewer_history`
//! rewrites rows older than a cutoff into 10-minute buckets (lossy in
//! resolution, lossless in span/peak), either manually ("Compress now") or
//! from the optional auto-downsample (`K_VH_DOWNSAMPLE_DAYS`).
//!
//! `stream_event` rows come from two independent sources: the live chat
//! parser (subs/resubs/gift subs/bits/raids-in — recording-time only, since
//! chat capture is recording-scoped) and EventSub `channel.raid`
//! (raids in/out, any time, conduit mode). Raid kinds are deduped on insert
//! because both sources can observe the same raid.

use std::collections::HashMap;

use super::*;
use crate::models::{ChannelStatsRow, StreamEventRow, StreamStatRow, ViewerBucket};

/// Raw sampling resolution (one minute; matches the poll/meta cadence).
pub const VH_RAW_BUCKET_SECS: i64 = 60;
/// Downsampled resolution for old rows (ten minutes).
pub const VH_DS_BUCKET_SECS: i64 = 600;
/// Auto-downsample age threshold in days; unset/`"0"` = off (keep raw forever).
pub const K_VH_DOWNSAMPLE_DAYS: &str = "viewer_history_downsample_days";
/// Unix time the auto-downsample last ran (it runs at most once per day).
pub const K_VH_DOWNSAMPLE_LAST: &str = "viewer_history_downsample_last";

/// Window within which a raid observed by both chat and EventSub is one event.
const RAID_DEDUP_SECS: i64 = 300;

impl Store {
    /// Fold one tick's live viewer samples into minute buckets. `samples` =
    /// `(monitor_id, viewers, followers, stream_id)`; viewers keep the bucket
    /// peak, a late-arriving follower count or stream id fills the bucket's
    /// NULL/empty slot.
    pub fn record_viewer_samples(
        &self,
        at_unix: i64,
        samples: &[(i64, i64, Option<i64>, &str)],
    ) -> Result<()> {
        let bucket_t = at_unix - at_unix.rem_euclid(VH_RAW_BUCKET_SECS);
        let conn = self.db();
        for (monitor_id, viewers, followers, stream_id) in samples {
            conn.execute(
                "INSERT INTO viewer_history(monitor_id, bucket_t, viewers, followers, stream_id)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(monitor_id, bucket_t) DO UPDATE SET
                    viewers   = MAX(viewers, excluded.viewers),
                    followers = COALESCE(excluded.followers, followers),
                    stream_id = CASE WHEN excluded.stream_id != ''
                                     THEN excluded.stream_id ELSE stream_id END",
                params![monitor_id, bucket_t, viewers, followers, stream_id],
            )?;
        }
        Ok(())
    }

    /// Viewer history for all monitors of `channel_id` in `[since, until)`,
    /// re-bucketed to `bucket_secs` at query time (MAX = peak-preserving),
    /// per monitor, oldest first. `until = i64::MAX` for open-ended.
    pub fn viewer_history_range(
        &self,
        channel_id: i64,
        since: i64,
        until: i64,
        bucket_secs: i64,
    ) -> Result<Vec<ViewerBucket>> {
        let bucket_secs = bucket_secs.max(VH_RAW_BUCKET_SECS);
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT v.monitor_id, (v.bucket_t / ?1) * ?1 AS tb,
                    MAX(v.viewers), MAX(v.followers)
             FROM viewer_history v
             JOIN monitor m ON m.id = v.monitor_id
             WHERE m.channel_id = ?2 AND v.bucket_t >= ?3 AND v.bucket_t < ?4
             GROUP BY v.monitor_id, tb
             ORDER BY tb",
        )?;
        let rows = stmt
            .query_map(params![bucket_secs, channel_id, since, until], |r| {
                Ok(ViewerBucket {
                    monitor_id: r.get(0)?,
                    t: r.get(1)?,
                    viewers: r.get(2)?,
                    followers: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Raw (unaggregated) recent samples for every monitor, for the grid's
    /// inline sparklines: `monitor_id -> [(bucket_t, viewers)]`, oldest first.
    pub fn recent_viewer_history(&self, since: i64) -> Result<HashMap<i64, Vec<(i64, i64)>>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT monitor_id, bucket_t, viewers FROM viewer_history
             WHERE bucket_t >= ?1 ORDER BY monitor_id, bucket_t",
        )?;
        let mut out: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
        let rows = stmt.query_map(params![since], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (mid, t, v) = row?;
            out.entry(mid).or_default().push((t, v));
        }
        Ok(out)
    }

    /// Per-channel viewer aggregates since `since_unix` for the Channel Stats
    /// overview table: peak viewers, span-weighted average viewers, live
    /// airtime (sum of sample spans), and the latest known follower count.
    /// Channels with no samples in the window are omitted. Sorted by peak
    /// viewers descending.
    pub fn channel_stats_overview(&self, since_unix: i64) -> Result<Vec<ChannelStatsRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT m.channel_id, c.name,
                    MAX(v.viewers),
                    CAST(SUM(v.viewers * v.span_secs) AS REAL) / MAX(SUM(v.span_secs), 1),
                    SUM(v.span_secs),
                    (SELECT v2.followers FROM viewer_history v2
                     JOIN monitor m2 ON m2.id = v2.monitor_id
                     WHERE m2.channel_id = m.channel_id AND v2.followers IS NOT NULL
                     ORDER BY v2.bucket_t DESC LIMIT 1)
             FROM viewer_history v
             JOIN monitor m ON m.id = v.monitor_id
             JOIN channel c ON c.id = m.channel_id
             WHERE v.bucket_t >= ?1
             GROUP BY m.channel_id
             ORDER BY 3 DESC, c.name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map(params![since_unix], |r| {
                Ok(ChannelStatsRow {
                    channel_id: r.get(0)?,
                    name: r.get(1)?,
                    peak_viewers: r.get(2)?,
                    avg_viewers: r.get(3)?,
                    live_secs: r.get(4)?,
                    followers: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Insert a discrete stream event. Raid kinds (`raid_in`/`raid_out`) are
    /// deduped: chat and EventSub can both observe the same raid, so an
    /// existing same-kind row for the same monitor and actor/target within
    /// [`RAID_DEDUP_SECS`] wins and the insert is skipped. Returns whether a
    /// row was inserted.
    #[allow(clippy::too_many_arguments)]
    pub fn record_stream_event(
        &self,
        monitor_id: i64,
        at: i64,
        stream_id: &str,
        kind: &str,
        actor: &str,
        target: &str,
        amount: i64,
        tier: &str,
        detail: &str,
    ) -> Result<bool> {
        let conn = self.db();
        if kind == "raid_in" || kind == "raid_out" {
            let dup: bool = conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM stream_event
                    WHERE monitor_id = ?1 AND kind = ?2
                      AND lower(actor) = lower(?3) AND lower(target) = lower(?4)
                      AND at BETWEEN ?5 - ?6 AND ?5 + ?6)",
                params![monitor_id, kind, actor, target, at, RAID_DEDUP_SECS],
                |r| r.get(0),
            )?;
            if dup {
                return Ok(false);
            }
        }
        conn.execute(
            "INSERT INTO stream_event(monitor_id, at, stream_id, kind, actor, target, amount, tier, detail)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![monitor_id, at, stream_id, kind, actor, target, amount, tier, detail],
        )?;
        Ok(true)
    }

    /// All stream events for `channel_id`'s monitors in `[since, until)`,
    /// newest first.
    pub fn stream_events_range(
        &self,
        channel_id: i64,
        since: i64,
        until: i64,
    ) -> Result<Vec<StreamEventRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT e.monitor_id, e.at, e.stream_id, e.kind, e.actor, e.target, e.amount, e.tier,
                    e.detail
             FROM stream_event e
             JOIN monitor m ON m.id = e.monitor_id
             WHERE m.channel_id = ?1 AND e.at >= ?2 AND e.at < ?3
             ORDER BY e.at DESC",
        )?;
        let rows = stmt
            .query_map(params![channel_id, since, until], |r| {
                Ok(StreamEventRow {
                    monitor_id: r.get(0)?,
                    at: r.get(1)?,
                    stream_id: r.get(2)?,
                    kind: r.get(3)?,
                    actor: r.get(4)?,
                    target: r.get(5)?,
                    amount: r.get(6)?,
                    tier: r.get(7)?,
                    detail: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per-channel event totals since `since_unix` for the overview table:
    /// `channel_id -> [subs+resubs, gifted subs, bits, raids in, raids out,
    /// mod actions]`. Gifted subs sum `amount` (a mystery-gift row carries
    /// the batch size); bits sum `amount`; raids count rows; mod actions
    /// count deletions + timeouts + bans (chat-mode/role rows aren't
    /// totaled — they're context, not volume).
    pub fn stream_event_totals(&self, since_unix: i64) -> Result<HashMap<i64, [i64; 6]>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT m.channel_id, e.kind,
                    COUNT(*), SUM(MAX(e.amount, 1))
             FROM stream_event e
             JOIN monitor m ON m.id = e.monitor_id
             WHERE e.at >= ?1
             GROUP BY m.channel_id, e.kind",
        )?;
        let mut out: HashMap<i64, [i64; 6]> = HashMap::new();
        let rows = stmt.query_map(params![since_unix], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (cid, kind, count, amount) = row?;
            let e = out.entry(cid).or_default();
            match kind.as_str() {
                "sub" | "resub" => e[0] += count,
                "subgift" => e[1] += amount,
                "bits" => e[2] += amount,
                "raid_in" => e[3] += count,
                "raid_out" => e[4] += count,
                "msg_deleted" | "timeout" | "ban" => e[5] += count,
                _ => {}
            }
        }
        Ok(out)
    }

    /// Per-broadcast breakdown for `channel_id` since `since_unix`, newest
    /// first: one row per distinct `stream_id` seen in the viewer samples
    /// (id-less scrape-path samples can't be attributed and are skipped),
    /// with events folded in — matched by stream id where the event carries
    /// one (chat events do), else by falling inside the broadcast's sampled
    /// time range ±15 min (EventSub raids store no id).
    pub fn stream_stats_breakdown(
        &self,
        channel_id: i64,
        since_unix: i64,
    ) -> Result<Vec<StreamStatRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT v.stream_id, MIN(v.bucket_t), MAX(v.bucket_t + v.span_secs),
                    MAX(v.viewers),
                    CAST(SUM(v.viewers * v.span_secs) AS REAL) / MAX(SUM(v.span_secs), 1),
                    SUM(v.span_secs)
             FROM viewer_history v
             JOIN monitor m ON m.id = v.monitor_id
             WHERE m.channel_id = ?1 AND v.bucket_t >= ?2 AND v.stream_id != ''
             GROUP BY v.stream_id
             ORDER BY 2 DESC",
        )?;
        let mut rows = stmt
            .query_map(params![channel_id, since_unix], |r| {
                Ok(StreamStatRow {
                    stream_id: r.get(0)?,
                    started: r.get(1)?,
                    ended: r.get(2)?,
                    peak_viewers: r.get(3)?,
                    avg_viewers: r.get(4)?,
                    live_secs: r.get(5)?,
                    totals: [0; 6],
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        // Release the connection guard BEFORE the nested store call below —
        // the DB mutex is not re-entrant, so holding it here would deadlock.
        drop(conn);
        if rows.is_empty() {
            return Ok(rows);
        }
        for e in self.stream_events_range(channel_id, since_unix, i64::MAX)? {
            let hit = if e.stream_id.is_empty() {
                rows.iter_mut().find(|s| e.at >= s.started - 900 && e.at <= s.ended + 900)
            } else {
                rows.iter_mut().find(|s| s.stream_id == e.stream_id)
            };
            if let Some(s) = hit {
                match e.kind.as_str() {
                    "sub" | "resub" => s.totals[0] += 1,
                    "subgift" => s.totals[1] += e.amount.max(1),
                    "bits" => s.totals[2] += e.amount,
                    "raid_in" => s.totals[3] += 1,
                    "raid_out" => s.totals[4] += 1,
                    "msg_deleted" | "timeout" | "ban" => s.totals[5] += 1,
                    _ => {}
                }
            }
        }
        Ok(rows)
    }

    /// Title/category/collab changes for `channel_id`'s monitors in
    /// `[since, until)` — graph markers for the Channel Stats plots.
    /// Returns `(at, kind, new_value)`, oldest first.
    pub fn monitor_changes_range(
        &self,
        channel_id: i64,
        since: i64,
        until: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT h.at, h.kind, h.new_value
             FROM monitor_stream_change h
             JOIN monitor m ON m.id = h.monitor_id
             WHERE m.channel_id = ?1 AND h.at >= ?2 AND h.at < ?3
             ORDER BY h.at",
        )?;
        let rows = stmt
            .query_map(params![channel_id, since, until], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rewrite viewer-history rows older than `older_than_unix` into
    /// [`VH_DS_BUCKET_SECS`]-wide buckets (peak-preserving MAX; spans
    /// accumulate so airtime stays exact). Idempotent — already-downsampled
    /// rows regroup into themselves. Returns `(rows_before, rows_after)` for
    /// the affected range.
    pub fn downsample_viewer_history(&self, older_than_unix: i64) -> Result<(i64, i64)> {
        // Align the cutoff so a coarse bucket never straddles it.
        let cut = older_than_unix - older_than_unix.rem_euclid(VH_DS_BUCKET_SECS);
        let conn = self.db();
        let before: i64 = conn.query_row(
            "SELECT COUNT(*) FROM viewer_history WHERE bucket_t < ?1",
            params![cut],
            |r| r.get(0),
        )?;
        conn.execute_batch(&format!(
            r#"
            BEGIN;
            CREATE TEMP TABLE vh_ds AS
                SELECT monitor_id, (bucket_t / {ds}) * {ds} AS tb,
                       MAX(viewers) AS v, MAX(followers) AS f,
                       MAX(stream_id) AS sid, MIN(SUM(span_secs), {ds}) AS sp
                FROM viewer_history WHERE bucket_t < {cut}
                GROUP BY monitor_id, tb;
            DELETE FROM viewer_history WHERE bucket_t < {cut};
            INSERT OR REPLACE INTO viewer_history
                (monitor_id, bucket_t, viewers, followers, stream_id, span_secs)
                SELECT monitor_id, tb, v, f, sid, sp FROM vh_ds;
            DROP TABLE vh_ds;
            COMMIT;
            "#,
            ds = VH_DS_BUCKET_SECS,
        ))?;
        let after: i64 = conn.query_row(
            "SELECT COUNT(*) FROM viewer_history WHERE bucket_t < ?1",
            params![cut],
            |r| r.get(0),
        )?;
        Ok((before, after))
    }

    /// Size/shape of the viewer-history table for the settings readout:
    /// `(total rows, oldest bucket_t, raw-resolution rows)`.
    pub fn viewer_history_info(&self) -> Result<(i64, Option<i64>, i64)> {
        let conn = self.db();
        conn.query_row(
            "SELECT COUNT(*), MIN(bucket_t),
                    COUNT(*) FILTER (WHERE span_secs <= ?1)
             FROM viewer_history",
            params![VH_RAW_BUCKET_SECS],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(Into::into)
    }

    /// Run the auto-downsample if it's enabled ([`K_VH_DOWNSAMPLE_DAYS`] > 0)
    /// and hasn't run in the last day. Called from the scheduler tick — cheap
    /// no-op in the common case (two settings reads).
    pub fn maybe_auto_downsample_viewer_history(&self, now: i64) -> Result<()> {
        let days: i64 = self
            .get_setting(K_VH_DOWNSAMPLE_DAYS)?
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if days <= 0 {
            return Ok(());
        }
        let last: i64 = self
            .get_setting(K_VH_DOWNSAMPLE_LAST)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if now - last < 86_400 {
            return Ok(());
        }
        self.set_setting(K_VH_DOWNSAMPLE_LAST, &now.to_string())?;
        let (before, after) = self.downsample_viewer_history(now - days * 86_400)?;
        if before != after {
            tracing::info!(
                "viewer history: auto-downsampled samples older than {days}d \
                 ({before} rows -> {after})"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::*;

    fn channel_with_monitor(store: &Store) -> (i64, i64) {
        let cid = store.create_container("Streamer").unwrap();
        let m = sample_monitor(cid);
        let mid = store.insert_monitor(&m).unwrap();
        (cid, mid)
    }

    #[test]
    fn viewer_samples_bucket_peak_and_aggregate() {
        let store = Store::open_in_memory().unwrap();
        let (cid, mid) = channel_with_monitor(&store);
        let t0: i64 = 1_000_000 - 1_000_000_i64.rem_euclid(3600);

        // Two samples in the same minute keep the peak; follower fill-in.
        store.record_viewer_samples(t0 + 5, &[(mid, 100, None, "s1")]).unwrap();
        store.record_viewer_samples(t0 + 40, &[(mid, 80, Some(5000), "s1")]).unwrap();
        store.record_viewer_samples(t0 + 65, &[(mid, 150, None, "s1")]).unwrap();

        let raw = store.viewer_history_range(cid, 0, i64::MAX, 60).unwrap();
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0].viewers, 100, "same-minute peak kept");
        assert_eq!(raw[0].followers, Some(5000), "late follower count filled in");
        assert_eq!(raw[1].viewers, 150);

        // Query-time re-bucketing folds to the hour, keeping the peak.
        let hourly = store.viewer_history_range(cid, 0, i64::MAX, 3600).unwrap();
        assert_eq!(hourly.len(), 1);
        assert_eq!(hourly[0].viewers, 150);

        // Overview: weighted average over 2 minutes of span.
        let ov = store.channel_stats_overview(0).unwrap();
        assert_eq!(ov.len(), 1);
        assert_eq!(ov[0].peak_viewers, 150);
        assert_eq!(ov[0].live_secs, 120);
        assert_eq!(ov[0].followers, Some(5000));
        assert!((ov[0].avg_viewers - 125.0).abs() < 0.01);
    }

    #[test]
    fn downsample_preserves_peak_and_span() {
        let store = Store::open_in_memory().unwrap();
        let (cid, mid) = channel_with_monitor(&store);
        let t0: i64 = 1_200_000 - 1_200_000_i64.rem_euclid(VH_DS_BUCKET_SECS);
        // Ten minutes of raw samples with one spike.
        for i in 0..10 {
            let v = if i == 4 { 900 } else { 100 };
            store.record_viewer_samples(t0 + i * 60, &[(mid, v, None, "s1")]).unwrap();
        }
        let (before, after) = store.downsample_viewer_history(t0 + 86_400).unwrap();
        assert_eq!((before, after), (10, 1));
        let rows = store.viewer_history_range(cid, 0, i64::MAX, 60).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].viewers, 900, "downsampling keeps the peak");
        let ov = store.channel_stats_overview(0).unwrap();
        assert_eq!(ov[0].live_secs, 600, "span survives downsampling");
        // Idempotent: a second pass leaves the row alone.
        let (b2, a2) = store.downsample_viewer_history(t0 + 86_400).unwrap();
        assert_eq!((b2, a2), (1, 1));
        let (total, oldest, raw) = store.viewer_history_info().unwrap();
        assert_eq!((total, oldest, raw), (1, Some(t0), 0));
    }

    #[test]
    fn per_stream_breakdown_groups_samples_and_events() {
        let store = Store::open_in_memory().unwrap();
        let (cid, mid) = channel_with_monitor(&store);
        let t0: i64 = 5_000_000 - 5_000_000_i64.rem_euclid(60);
        // Broadcast A: two minutes; broadcast B an hour later: one minute.
        store.record_viewer_samples(t0, &[(mid, 100, None, "sA")]).unwrap();
        store.record_viewer_samples(t0 + 60, &[(mid, 300, None, "sA")]).unwrap();
        store.record_viewer_samples(t0 + 3600, &[(mid, 50, None, "sB")]).unwrap();
        // Chat event carries the stream id; EventSub raid doesn't (matched by
        // time containment); an id-less orphan far outside both is dropped.
        store.record_stream_event(mid, t0 + 30, "sA", "subgift", "g", "", 20, "1000", "").unwrap();
        store.record_stream_event(mid, t0 + 40, "", "raid_in", "r", "", 500, "", "").unwrap();
        store.record_stream_event(mid, t0 + 3610, "sB", "bits", "c", "", 100, "", "").unwrap();
        store.record_stream_event(mid, t0 + 7200, "", "sub", "x", "", 1, "1000", "").unwrap();

        let rows = store.stream_stats_breakdown(cid, 0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].stream_id, "sB", "newest first");
        assert_eq!(rows[0].totals, [0, 0, 100, 0, 0, 0]);
        let a = &rows[1];
        assert_eq!((a.peak_viewers, a.live_secs), (300, 120));
        assert_eq!(a.totals, [0, 20, 0, 1, 0, 0], "gift batch + time-matched raid");
        assert_eq!(a.ended - a.started, 120, "sample envelope spans both buckets");
    }

    #[test]
    fn stream_events_dedup_raids_and_total() {
        let store = Store::open_in_memory().unwrap();
        let (cid, mid) = channel_with_monitor(&store);
        let t = 2_000_000;

        assert!(store.record_stream_event(mid, t, "s1", "sub", "alice", "", 1, "1000", "").unwrap());
        assert!(store.record_stream_event(mid, t + 1, "s1", "subgift", "bob", "", 5, "1000", "").unwrap());
        assert!(store.record_stream_event(mid, t + 2, "s1", "bits", "carol", "", 500, "", "").unwrap());
        // Raid seen by chat, then again by EventSub 10s later -> one row.
        assert!(store.record_stream_event(mid, t + 10, "s1", "raid_in", "dave", "", 250, "", "").unwrap());
        assert!(!store.record_stream_event(mid, t + 20, "", "raid_in", "Dave", "", 250, "", "").unwrap());
        // Same raider well outside the window -> a genuinely new raid.
        assert!(store.record_stream_event(mid, t + 1000, "", "raid_in", "dave", "", 99, "", "").unwrap());
        // Bits are never deduped (two cheers in a minute are two events).
        assert!(store.record_stream_event(mid, t + 30, "s1", "bits", "carol", "", 500, "", "").unwrap());
        // Moderation kinds (v60): deletions/timeouts/bans fold into one
        // "mod actions" total; chat-mode changes are context, not volume.
        assert!(store
            .record_stream_event(mid, t + 40, "s1", "msg_deleted", "eve", "", 0, "", "spam link")
            .unwrap());
        assert!(store.record_stream_event(mid, t + 41, "s1", "timeout", "eve", "", 600, "", "").unwrap());
        assert!(store.record_stream_event(mid, t + 42, "s1", "ban", "mallory", "", 0, "", "").unwrap());
        assert!(store
            .record_stream_event(mid, t + 43, "s1", "chat_mode", "", "", 0, "", "Slow mode on (30s)")
            .unwrap());

        let events = store.stream_events_range(cid, 0, i64::MAX).unwrap();
        assert_eq!(events.len(), 10);
        assert_eq!(events[0].at, t + 1000, "newest first");
        let del = events.iter().find(|e| e.kind == "msg_deleted").unwrap();
        assert_eq!(del.detail, "spam link");

        let totals = store.stream_event_totals(0).unwrap();
        let e = totals.get(&cid).unwrap();
        assert_eq!(e, &[1, 5, 1000, 2, 0, 3], "subs, gifted, bits, raids in/out, mod actions");
    }
}
