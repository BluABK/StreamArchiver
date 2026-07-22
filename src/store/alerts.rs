//! Capture alerts (schema v64): problems scraped from the capture tools' own
//! log files — streamlink sequence gaps (lost data), failed segment fetches,
//! yt-dlp ERROR/WARNING lines — surfaced by the 🚨 Warnings window, plus the
//! `gap_range` work queue the Twitch lost-segment recovery job drains.
//!
//! Alerts aggregate: one row per `(take_key, kind)` whose `count`/
//! `lost_segments`/`last_line` grow as more matching lines appear. Growth
//! clears `acked`, so fresh data loss re-lights the badge even on an alert
//! the user already acknowledged.

use super::*;

/// One alert to upsert, built by the log scanner each watchdog cycle.
#[derive(Clone, Debug, Default)]
pub struct NewCaptureAlert {
    /// `sequence_gap` | `fetch_failed` | `tool_error` | `tool_warning`.
    pub kind: String,
    /// `error` | `warning` (drives the row tint + badge bucket).
    pub severity: String,
    /// Tool program name (`streamlink`, `yt-dlp-dev`, …).
    pub source: String,
    /// The take's tool-log path — unique per take, survives restarts.
    pub take_key: String,
    pub monitor_id: Option<i64>,
    pub recording_id: Option<i64>,
    pub video_id: Option<i64>,
    /// Channel display name (or the video job name) snapshot.
    pub channel: String,
    /// Matching lines seen THIS cycle (added to the stored count).
    pub count: i64,
    /// Segments lost THIS cycle (added; sequence_gap/fetch_failed only).
    pub lost_segments: i64,
    /// The newest matching raw log line (stored verbatim for the hover).
    pub last_line: String,
}

/// A persisted alert, as listed in the Warnings window.
#[derive(Clone, Debug)]
pub struct CaptureAlertRow {
    pub id: i64,
    pub first_at: i64,
    pub last_at: i64,
    pub kind: String,
    pub severity: String,
    pub source: String,
    pub take_key: String,
    // Click-through metadata not yet read by the Warnings UI (mirrors
    // `NotificationRow`'s convention).
    #[allow(dead_code)]
    pub monitor_id: Option<i64>,
    /// Drives the 🩹 Patches row action (open the recovered files' folder).
    pub recording_id: Option<i64>,
    #[allow(dead_code)]
    pub video_id: Option<i64>,
    pub channel: String,
    pub count: i64,
    pub lost_segments: i64,
    pub ranges_total: i64,
    pub recovered: i64,
    /// Segments in the recovered patches that had to fall back to DMCA-muted
    /// copies (a muted patch beats no patch, but the user must know).
    pub recovered_muted: i64,
    pub last_line: String,
    pub acked: bool,
}

/// One lost time range queued for VOD recovery (broadcast-start offsets,
/// already padded + coalesced by the scanner).
#[derive(Clone, Debug)]
pub struct GapRangeRow {
    pub id: i64,
    #[allow(dead_code)]
    pub recording_id: i64,
    pub start_secs: f64,
    pub end_secs: f64,
    /// Row state at query time (callers filter by state, so reads are rare).
    #[allow(dead_code)]
    pub state: String,
    pub attempts: i64,
    /// Recovered patch file once `done` (the 🩹 Patches action opens its
    /// folder).
    pub out_path: String,
    /// Muted-fallback segments inside the recovered patch (0 = clean audio).
    #[allow(dead_code)]
    pub muted_segs: i64,
}

const ALERT_COLS: &str = "id, first_at, last_at, kind, severity, source, take_key, monitor_id, \
     recording_id, video_id, channel, count, lost_segments, ranges_total, recovered, \
     recovered_muted, last_line, acked";

fn alert_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CaptureAlertRow> {
    Ok(CaptureAlertRow {
        id: r.get(0)?,
        first_at: r.get(1)?,
        last_at: r.get(2)?,
        kind: r.get(3)?,
        severity: r.get(4)?,
        source: r.get(5)?,
        take_key: r.get(6)?,
        monitor_id: r.get(7)?,
        recording_id: r.get(8)?,
        video_id: r.get(9)?,
        channel: r.get(10)?,
        count: r.get(11)?,
        lost_segments: r.get(12)?,
        ranges_total: r.get(13)?,
        recovered: r.get(14)?,
        recovered_muted: r.get(15)?,
        last_line: r.get(16)?,
        acked: r.get::<_, i64>(17)? != 0,
    })
}

impl Store {
    /// Insert or grow an alert. Growth (an existing row) bumps `count`,
    /// `lost_segments`, `last_at`, `last_line` and CLEARS `acked` — new
    /// occurrences must re-light the badge. Returns `(id, was_new)`.
    pub fn upsert_capture_alert(&self, a: &NewCaptureAlert) -> Result<(i64, bool)> {
        let now = now_unix();
        let conn = self.db();
        let updated = conn.execute(
            "UPDATE capture_alert SET
                last_at = ?3, count = count + ?4, lost_segments = lost_segments + ?5,
                last_line = ?6, acked = 0
             WHERE take_key = ?1 AND kind = ?2",
            params![a.take_key, a.kind, now, a.count, a.lost_segments, a.last_line],
        )?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO capture_alert(first_at, last_at, kind, severity, source, take_key,
                    monitor_id, recording_id, video_id, channel, count, lost_segments, last_line)
                 VALUES(?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    now,
                    a.kind,
                    a.severity,
                    a.source,
                    a.take_key,
                    a.monitor_id,
                    a.recording_id,
                    a.video_id,
                    a.channel,
                    a.count.max(1),
                    a.lost_segments,
                    a.last_line
                ],
            )?;
            return Ok((conn.last_insert_rowid(), true));
        }
        let id = conn.query_row(
            "SELECT id FROM capture_alert WHERE take_key = ?1 AND kind = ?2",
            params![a.take_key, a.kind],
            |r| r.get(0),
        )?;
        Ok((id, false))
    }

    /// The most recent `limit` alerts, newest activity first.
    pub fn list_capture_alerts(&self, limit: i64) -> Result<Vec<CaptureAlertRow>> {
        let conn = self.db();
        let mut st = conn.prepare(&format!(
            "SELECT {ALERT_COLS} FROM capture_alert ORDER BY last_at DESC, id DESC LIMIT ?1"
        ))?;
        let rows = st.query_map([limit], alert_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Unacked `(errors, warnings)` — the 🚨 button badge.
    pub fn alert_badge_counts(&self) -> Result<(i64, i64)> {
        let conn = self.db();
        conn.query_row(
            "SELECT
                COALESCE(SUM(CASE WHEN severity = 'error' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN severity != 'error' THEN 1 ELSE 0 END), 0)
             FROM capture_alert WHERE acked = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(Into::into)
    }

    pub fn ack_capture_alert(&self, id: i64) -> Result<()> {
        self.db().execute("UPDATE capture_alert SET acked = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn ack_all_capture_alerts(&self) -> Result<()> {
        self.db().execute("UPDATE capture_alert SET acked = 1 WHERE acked = 0", [])?;
        Ok(())
    }

    /// Update a take's gap-recovery progress on its data-loss alert rows
    /// (does NOT touch `acked` — recovery progress isn't new damage).
    pub fn set_alert_recovery(
        &self,
        recording_id: i64,
        ranges_total: i64,
        recovered: i64,
        recovered_muted: i64,
    ) -> Result<()> {
        self.db().execute(
            "UPDATE capture_alert SET ranges_total = ?2, recovered = ?3, recovered_muted = ?4
             WHERE recording_id = ?1 AND kind IN ('sequence_gap', 'fetch_failed')",
            params![recording_id, ranges_total, recovered, recovered_muted],
        )?;
        Ok(())
    }

    /// Whether any alert already exists for a tool log — the retro log sweep
    /// skips those files (they were live-scanned, or swept before; rescanning
    /// would double the counters and un-ack the row).
    pub fn alert_exists_for_take(&self, take_key: &str) -> bool {
        self.db()
            .query_row(
                "SELECT 1 FROM capture_alert WHERE take_key = ?1 LIMIT 1",
                [take_key],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Recordings carrying this platform stream id, newest take first —
    /// `(id, monitor_id, started_at, status)`. The retro log sweep picks the
    /// take whose start time matches the log filename's timestamp.
    pub fn recordings_by_stream_id(&self, stream_id: &str) -> Result<Vec<(i64, i64, i64, String)>> {
        let conn = self.db();
        let mut st = conn.prepare(
            "SELECT id, monitor_id, started_at, status FROM recording
             WHERE stream_id = ?1 ORDER BY started_at DESC",
        )?;
        let rows = st
            .query_map([stream_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Startup repair: fetches orphaned by a shutdown go back to `pending`
    /// (they keep their attempt count — `set_gap_range_state` only bumps it
    /// on explicit retry re-queues, and this isn't the range's fault).
    pub fn requeue_stale_gap_fetches(&self) -> Result<usize> {
        Ok(self
            .db()
            .execute("UPDATE gap_range SET state = 'pending' WHERE state = 'fetching'", [])?)
    }

    /// Replace a recording's PENDING gap ranges with a fresh coalesced set.
    /// Rows already `fetching`/`done`/`failed` are kept untouched — the
    /// scanner re-derives the full range list every flush, but work that
    /// started must not be forgotten or duplicated. Incoming ranges that
    /// overlap a kept row are dropped.
    pub fn replace_pending_gap_ranges(&self, recording_id: i64, ranges: &[(f64, f64)]) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM gap_range WHERE recording_id = ?1 AND state = 'pending'",
            [recording_id],
        )?;
        let kept: Vec<(f64, f64)> = {
            let mut st = conn.prepare(
                "SELECT start_secs, end_secs FROM gap_range WHERE recording_id = ?1",
            )?;
            st.query_map([recording_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for &(start, end) in ranges {
            if kept.iter().any(|&(ks, ke)| start < ke && end > ks) {
                continue;
            }
            conn.execute(
                "INSERT OR IGNORE INTO gap_range(recording_id, start_secs, end_secs)
                 VALUES(?1, ?2, ?3)",
                params![recording_id, start, end],
            )?;
        }
        Ok(())
    }

    /// A recording's gap ranges in a given state, oldest range first.
    pub fn gap_ranges_in_state(&self, recording_id: i64, state: &str) -> Result<Vec<GapRangeRow>> {
        let conn = self.db();
        let mut st = conn.prepare(
            "SELECT id, recording_id, start_secs, end_secs, state, attempts, out_path, muted_segs
             FROM gap_range WHERE recording_id = ?1 AND state = ?2 ORDER BY start_secs",
        )?;
        let rows = st
            .query_map(params![recording_id, state], |r| {
                Ok(GapRangeRow {
                    id: r.get(0)?,
                    recording_id: r.get(1)?,
                    start_secs: r.get(2)?,
                    end_secs: r.get(3)?,
                    state: r.get(4)?,
                    attempts: r.get(5)?,
                    out_path: r.get(6)?,
                    muted_segs: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(total, done, muted_segs)` for a recording's ranges — drives the
    /// alert row's "N/M recovered (✂ muted)" readout.
    pub fn gap_range_progress(&self, recording_id: i64) -> Result<(i64, i64, i64)> {
        self.db()
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN state = 'done' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(muted_segs), 0)
                 FROM gap_range WHERE recording_id = ?1",
                [recording_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(Into::into)
    }

    /// Move a range to a new state; `attempts` bumps only on `pending` (a
    /// retry re-queue), `out_path` is written only when non-empty, and
    /// `muted_segs` records the muted-fallback count for a `done` range.
    pub fn set_gap_range_state(&self, id: i64, state: &str, out_path: &str, muted_segs: i64) -> Result<()> {
        self.db().execute(
            "UPDATE gap_range SET
                state = ?2,
                attempts = attempts + (CASE WHEN ?2 = 'pending' THEN 1 ELSE 0 END),
                out_path = CASE WHEN ?3 != '' THEN ?3 ELSE out_path END,
                muted_segs = CASE WHEN ?2 = 'done' THEN ?4 ELSE muted_segs END
             WHERE id = ?1",
            params![id, state, out_path, muted_segs],
        )?;
        Ok(())
    }

    /// Recordings that still have pending gap ranges — the finalize-time and
    /// startup sweeps use this to resume unfinished recovery.
    pub fn recordings_with_pending_gaps(&self) -> Result<Vec<i64>> {
        let conn = self.db();
        let mut st = conn.prepare(
            "SELECT DISTINCT recording_id FROM gap_range WHERE state IN ('pending', 'fetching')",
        )?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Startup retention: drop alerts idle for `keep_days` and gap ranges
    /// whose recording's alerts are gone (done/failed rows only — pending
    /// work is kept so a long-offline app can still finish recovery inside
    /// the CDN's ~60-day window).
    pub fn prune_capture_alerts(&self, keep_days: i64) -> Result<usize> {
        let cutoff = now_unix() - keep_days * 86_400;
        let conn = self.db();
        let n = conn.execute("DELETE FROM capture_alert WHERE last_at < ?1", [cutoff])?;
        conn.execute(
            "DELETE FROM gap_range WHERE state IN ('done', 'failed')
             AND recording_id NOT IN (SELECT COALESCE(recording_id, -1) FROM capture_alert)",
            [],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gap_alert(take: &str) -> NewCaptureAlert {
        NewCaptureAlert {
            kind: "sequence_gap".into(),
            severity: "error".into(),
            source: "streamlink".into(),
            take_key: take.into(),
            monitor_id: None,
            recording_id: Some(7),
            video_id: None,
            channel: "Ebiko".into(),
            count: 2,
            lost_segments: 29,
            last_line: "Sequence gap of 21 segments at position 3159.".into(),
        }
    }

    #[test]
    fn alert_upsert_growth_unacks_and_badges() {
        let store = Store::open_in_memory().unwrap();
        let (id, new) = store.upsert_capture_alert(&gap_alert("A:\\x.ts.log")).unwrap();
        assert!(new);
        assert_eq!(store.alert_badge_counts().unwrap(), (1, 0));

        // Ack clears the badge; the row stays listed.
        store.ack_capture_alert(id).unwrap();
        assert_eq!(store.alert_badge_counts().unwrap(), (0, 0));
        assert_eq!(store.list_capture_alerts(10).unwrap().len(), 1);

        // Growth: same (take, kind) → same row, counters add, acked clears.
        let (id2, new2) = store.upsert_capture_alert(&gap_alert("A:\\x.ts.log")).unwrap();
        assert_eq!(id2, id);
        assert!(!new2);
        let row = &store.list_capture_alerts(10).unwrap()[0];
        assert_eq!((row.count, row.lost_segments, row.acked), (4, 58, false));
        assert_eq!(store.alert_badge_counts().unwrap(), (1, 0));

        // A warning-severity alert lands in the other badge bucket.
        let mut w = gap_alert("A:\\x.ts.log");
        w.kind = "tool_warning".into();
        w.severity = "warning".into();
        store.upsert_capture_alert(&w).unwrap();
        assert_eq!(store.alert_badge_counts().unwrap(), (1, 1));
        store.ack_all_capture_alerts().unwrap();
        assert_eq!(store.alert_badge_counts().unwrap(), (0, 0));

        // Recovery progress lands on the sequence_gap row without un-acking.
        store.set_alert_recovery(7, 7, 5, 3).unwrap();
        let rows = store.list_capture_alerts(10).unwrap();
        let gap = rows.iter().find(|r| r.kind == "sequence_gap").unwrap();
        assert_eq!((gap.ranges_total, gap.recovered, gap.recovered_muted), (7, 5, 3));
        assert!(gap.acked);

        // Retro-sweep guard: the take is known once ANY alert row exists.
        assert!(store.alert_exists_for_take("A:\\x.ts.log"));
        assert!(!store.alert_exists_for_take("A:\\other.ts.log"));
    }

    #[test]
    fn gap_range_lifecycle_keeps_started_work() {
        let store = Store::open_in_memory().unwrap();
        store.replace_pending_gap_ranges(7, &[(100.0, 130.0), (500.0, 520.0)]).unwrap();
        let pending = store.gap_ranges_in_state(7, "pending").unwrap();
        assert_eq!(pending.len(), 2);

        // Start fetching the first, finish it (2 muted-fallback segments).
        store.set_gap_range_state(pending[0].id, "fetching", "", 0).unwrap();
        store.set_gap_range_state(pending[0].id, "done", "A:\\x.gap100.mkv", 2).unwrap();

        // Scanner re-derives the full list (incl. a range overlapping the
        // done one) → pending rows replaced, done row kept, overlap dropped.
        store
            .replace_pending_gap_ranges(7, &[(95.0, 135.0), (500.0, 520.0), (900.0, 910.0)])
            .unwrap();
        assert_eq!(store.gap_ranges_in_state(7, "done").unwrap().len(), 1);
        let pending = store.gap_ranges_in_state(7, "pending").unwrap();
        assert_eq!(
            pending.iter().map(|r| r.start_secs as i64).collect::<Vec<_>>(),
            vec![500, 900]
        );
        assert_eq!(store.gap_range_progress(7).unwrap(), (3, 1, 2));
        assert_eq!(store.recordings_with_pending_gaps().unwrap(), vec![7]);

        // Retry accounting: pending re-queue bumps attempts, out_path sticks.
        store.set_gap_range_state(pending[0].id, "pending", "", 0).unwrap();
        let re = store.gap_ranges_in_state(7, "pending").unwrap();
        assert_eq!(re.iter().find(|r| r.id == pending[0].id).unwrap().attempts, 1);
        let done = &store.gap_ranges_in_state(7, "done").unwrap()[0];
        assert_eq!(done.out_path, "A:\\x.gap100.mkv");
        assert_eq!(done.muted_segs, 2);
    }

    #[test]
    fn alert_prune_drops_idle_rows() {
        let store = Store::open_in_memory().unwrap();
        let (id, _) = store.upsert_capture_alert(&gap_alert("A:\\old.ts.log")).unwrap();
        // Age the row past the cutoff by hand.
        store
            .db()
            .execute("UPDATE capture_alert SET last_at = last_at - 100 * 86400 WHERE id = ?1", [id])
            .unwrap();
        assert_eq!(store.prune_capture_alerts(60).unwrap(), 1);
        assert!(store.list_capture_alerts(10).unwrap().is_empty());
    }
}
