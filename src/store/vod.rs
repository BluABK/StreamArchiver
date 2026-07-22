//! Post-stream VOD archive/recovery state, ad breaks, the detached-process
//! registry, and stream-meta change history.

use super::*;

impl Store {
    // ----- post-stream published-VOD download ("archive the VOD after end") -----

    /// Set the archive-download state + link the download job (`video.id`).
    pub fn set_recording_vod_dl(&self, id: i64, state: &str, video_id: Option<i64>) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_dl_state=?2, vod_dl_video_id=?3 WHERE id=?1",
            params![id, state, video_id],
        )?;
        Ok(())
    }

    /// Record a downloaded published VOD file with a terminal archive state
    /// (`archived`/`replaced`).
    pub fn set_recording_vod_archived(&self, id: i64, path: &str, state: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_dl_path=?2, vod_dl_state=?3 WHERE id=?1",
            params![id, path, state],
        )?;
        Ok(())
    }

    /// Which recording (if any) an archive-download job belongs to.
    pub fn recording_for_vod_video(&self, video_id: i64) -> Result<Option<i64>> {
        let conn = self.db();
        let id = conn
            .query_row(
                "SELECT id FROM recording WHERE vod_dl_video_id = ?1",
                params![video_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Resolve a muted-VOD issue (Issues panel "Dismiss"/"Keep live recording").
    pub fn recording_vod_dl_acknowledge(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_dl_state='acknowledged' WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    /// `(monitor_url, vod_id, stream_id, went_live_at)` for a manual "download VOD
    /// now" — enough to resolve the published-VOD URL per platform.
    pub fn recording_archive_now(&self, rec_id: i64) -> Result<Option<VodArchiveNowInfo>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT m.url, r.vod_id, r.stream_id, r.went_live_at
                 FROM recording r JOIN monitor m ON m.id = r.monitor_id
                 WHERE r.id = ?1",
                params![rec_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// `(channel_id, monitor_id, live output_path, muted_secs)` for a recording —
    /// what the archive-completion hook needs to decide/perform a replace.
    pub fn recording_replace_info(&self, rec_id: i64) -> Result<Option<VodReplaceInfo>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT m.channel_id, r.monitor_id, COALESCE(r.output_path, ''), r.vod_muted_secs
                 FROM recording r JOIN monitor m ON m.id = r.monitor_id
                 WHERE r.id = ?1",
                params![rec_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Completed archive downloads whose recording-side finalize never ran —
    /// `vod_dl_state` still `'downloading'` (or reset to `'failed'` by the
    /// startup sweep) while the linked video row says `'completed'`. Rows whose
    /// video is still in the detached registry are excluded: those are being
    /// adopted by `reconcile_detached` and will finalize through that path.
    /// Returns `(rec_id, video_id, video output_path)`.
    pub fn vod_archive_replay_candidates(&self) -> Result<Vec<(i64, i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id, v.id, COALESCE(v.output_path, '')
             FROM recording r JOIN video v ON v.id = r.vod_dl_video_id
             WHERE r.vod_dl_state IN ('downloading', 'failed') AND v.status = 'completed'
               AND v.id NOT IN (SELECT ref_id FROM detached_process WHERE kind = 'video')",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every recording currently claiming a terminal `'archived'` state —
    /// audited at startup for bogus archives (e.g. a promoted log file).
    /// Returns `(rec_id, vod_dl_video_id, vod_dl_path, vod_muted_secs, vod_id)`.
    #[allow(clippy::type_complexity)]
    pub fn vod_archived_rows(
        &self,
    ) -> Result<Vec<(i64, Option<i64>, String, i64, Option<String>)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, vod_dl_video_id, COALESCE(vod_dl_path, ''),
                    COALESCE(vod_muted_secs, 0), vod_id
             FROM recording WHERE vod_dl_state = 'archived'",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(vod_dl_state, vod_dl_video_id)` for one recording — what the post-find
    /// mute watcher needs to decide whether the archive predates the mute.
    pub fn recording_vod_dl(&self, id: i64) -> Result<Option<(Option<String>, Option<i64>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT vod_dl_state, vod_dl_video_id FROM recording WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Update only the muted-seconds count (post-find mute watcher) — must not
    /// clobber `vod_state`/`vod_id` the way `set_recording_vod_found` would.
    /// Latest published-VOD view count (narrow setter — the checker and the
    /// mute watch refresh it without touching vod_state/vod_id).
    pub fn set_recording_vod_views(&self, id: i64, views: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE recording SET vod_views=?2 WHERE id=?1", params![id, views])?;
        Ok(())
    }

    pub fn set_recording_vod_muted_secs(&self, id: i64, muted_secs: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_muted_secs=?2 WHERE id=?1",
            params![id, muted_secs],
        )?;
        Ok(())
    }

    /// `(live output_path, went_live_at, ended_at)` — what the archive sanity
    /// check needs to derive the expected VOD duration (probe the live file,
    /// else the recorded wall-clock span).
    #[allow(clippy::type_complexity)]
    pub fn recording_duration_hint(
        &self,
        rec_id: i64,
    ) -> Result<Option<(String, Option<i64>, Option<i64>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT COALESCE(output_path, ''), went_live_at, ended_at
                 FROM recording WHERE id = ?1",
                params![rec_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Recordings whose published VOD was DMCA-muted and not yet acknowledged —
    /// the muted-VOD category of the Issues panel.
    pub fn recordings_muted_vod_unresolved(&self) -> Result<Vec<crate::models::MutedVodIssue>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id, c.name, COALESCE(r.output_path, ''), r.recovered_path, r.recovery_state,
                    COALESCE(r.vod_muted_secs, 0)
             FROM recording r
             JOIN monitor m ON m.id = r.monitor_id
             JOIN channel c ON c.id = m.channel_id
             WHERE r.vod_dl_state = 'muted'
             ORDER BY COALESCE(r.went_live_at, r.started_at) DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::MutedVodIssue {
                    rec_id: r.get(0)?,
                    channel: r.get(1)?,
                    output_path: r.get(2)?,
                    recovered_path: r.get(3)?,
                    recovery_state: r.get(4)?,
                    muted_secs: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Fields needed to build a recovery for one recording (login derived from the
    /// monitor URL). `None` if the row is missing. Callers gate on a non-empty
    /// `stream_id` (id-less detection can't derive the CDN URL).
    pub fn recording_recovery_seed(
        &self,
        id: i64,
    ) -> Result<Option<crate::models::RecoverableTake>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT r.id, m.url, COALESCE(r.stream_id, ''),
                        COALESCE(r.went_live_at, r.started_at),
                        COALESCE(r.went_live_approx, 0),
                        COALESCE(r.vod_state = 'not_published', 0), r.vod_id
                 FROM recording r JOIN monitor m ON m.id = r.monitor_id
                 WHERE r.id = ?1",
                params![id],
                |r| {
                    Ok(crate::models::RecoverableTake {
                        rec_id: r.get(0)?,
                        monitor_url: r.get(1)?,
                        stream_id: r.get(2)?,
                        start_epoch: r.get(3)?,
                        went_live_approx: r.get::<_, i64>(4)? != 0,
                        deleted: r.get::<_, i64>(5)? != 0,
                        vod_id: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Current CDN-recovery state of one recording (`None` = never attempted).
    pub fn recording_recovery_state(&self, id: i64) -> Result<Option<String>> {
        let conn = self.db();
        let v = conn
            .query_row(
                "SELECT recovery_state FROM recording WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    /// Reset takes stuck in `recovery_state = 'recovering'` to `'failed'`.
    /// Recoveries are in-process tasks — none survive a restart — so any row
    /// still marked 'recovering' at startup crashed mid-run and would
    /// otherwise show the "recovering…" badge forever and be excluded from
    /// bulk scans (which only retry NULL/'failed').
    pub fn reset_stale_recovering(&self) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE recording SET recovery_state = 'failed' WHERE recovery_state = 'recovering'",
            [],
        )?;
        Ok(n)
    }

    /// Reset takes stuck in `vod_dl_state = 'downloading'` to `'failed'`.
    /// The pre-download wait (YouTube ~5 min, Kick up to ~60 min of polls) is a
    /// plain in-process task — a quit/crash in that window leaves the state
    /// stranded with no video row to adopt. A detached download that DID
    /// survive re-archives on its adopted completion, overwriting the 'failed'.
    pub fn reset_stale_vod_downloading(&self) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE recording SET vod_dl_state = 'failed' WHERE vod_dl_state = 'downloading'",
            [],
        )?;
        Ok(n)
    }

    /// Deleted/muted Twitch takes that still have a stream id, fall inside the CDN
    /// retention window, and haven't already been recovered — the bulk-scan set.
    pub fn recordings_recoverable(
        &self,
        within_secs: i64,
        now: i64,
    ) -> Result<Vec<crate::models::RecoverableTake>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id, m.url, r.stream_id,
                    COALESCE(r.went_live_at, r.started_at),
                    COALESCE(r.went_live_approx, 0),
                    COALESCE(r.vod_state = 'not_published', 0), r.vod_id
             FROM recording r JOIN monitor m ON m.id = r.monitor_id
             WHERE r.stream_id IS NOT NULL AND r.stream_id != ''
               AND (r.vod_state = 'not_published'
                    OR (r.vod_state = 'found' AND COALESCE(r.vod_muted_secs, 0) > 0))
               AND COALESCE(r.went_live_at, r.started_at) >= ?1
               AND (r.recovery_state IS NULL OR r.recovery_state = 'failed')
               AND r.status != 'recording'
             ORDER BY COALESCE(r.went_live_at, r.started_at) DESC",
        )?;
        let cutoff = now - within_secs;
        let rows = stmt
            .query_map(params![cutoff], |r| {
                Ok(crate::models::RecoverableTake {
                    rec_id: r.get(0)?,
                    monitor_url: r.get(1)?,
                    stream_id: r.get(2)?,
                    start_epoch: r.get(3)?,
                    went_live_approx: r.get::<_, i64>(4)? != 0,
                    deleted: r.get::<_, i64>(5)? != 0,
                    vod_id: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct published Twitch VOD ids (most recent first), for harvesting the
    /// current CDN host set via GQL.
    pub fn published_vod_ids(&self, limit: i64) -> Result<Vec<String>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT vod_id FROM recording
             WHERE vod_id IS NOT NULL AND vod_id != '' AND vod_state = 'found'
             ORDER BY COALESCE(went_live_at, started_at) DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Return whether a recording's go-live time is approximate (detection-clock,
    /// not platform-reported). `false` on any error or missing row.
    pub fn recording_went_live_approx(&self, id: i64) -> bool {
        let conn = self.db();
        conn.query_row(
            "SELECT went_live_approx FROM recording WHERE id=?1",
            params![id],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
    }

    /// Record an advertisement break detected during a recording take. `at_secs`
    /// is the offset from the take's start; `duration_secs` the reported ad length.
    pub fn insert_ad_break(
        &self,
        recording_id: i64,
        at_secs: i64,
        duration_secs: i64,
    ) -> Result<i64> {
        let conn = self.db();
        // OR IGNORE + the UNIQUE(recording_id, at_secs) index makes this a no-op
        // when re-attach re-observes a break already persisted before a restart.
        conn.execute(
            "INSERT OR IGNORE INTO ad_break(recording_id, at_secs, duration_secs) VALUES(?1, ?2, ?3)",
            params![recording_id, at_secs, duration_secs],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All ad breaks for a recording take, ordered by offset (for the cut-list
    /// tooltip/popup).
    pub fn ad_breaks_for_recording(&self, recording_id: i64) -> Result<Vec<AdBreak>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, recording_id, at_secs, duration_secs FROM ad_break
             WHERE recording_id = ?1 ORDER BY at_secs, id",
        )?;
        let rows = stmt
            .query_map(params![recording_id], |r| {
                Ok(AdBreak {
                    id: r.get(0)?,
                    recording_id: r.get(1)?,
                    at_secs: r.get(2)?,
                    duration_secs: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ----- detached download registry (re-attach across app restarts) -----

    /// Register a freshly-spawned tool process so a later launch can re-attach to
    /// it if it outlives the app. Replaces any prior row for the same
    /// (kind, ref_id) so a restarted take doesn't leave a stale entry.
    #[allow(clippy::too_many_arguments)]
    pub fn register_detached(&self, row: &DetachedRow) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM detached_process WHERE kind=?1 AND ref_id=?2",
            params![row.kind.as_str(), row.ref_id],
        )?;
        conn.execute(
            "INSERT INTO detached_process(
                 kind, ref_id, monitor_id, pid, proc_start, job_name, log_path,
                 capture_path, final_path, remux_to_mkv, take_group, spawn_build, started_at,
                 secondary, stream_id, went_live_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                row.kind.as_str(),
                row.ref_id,
                row.monitor_id,
                row.pid as i64,
                row.proc_start as i64,
                row.job_name,
                row.log_path,
                row.capture_path,
                row.final_path,
                row.remux_to_mkv as i64,
                row.take_group,
                row.spawn_build,
                row.started_at,
                row.secondary as i64,
                row.stream_id,
                row.went_live_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Drop a registry row once its download has been finalized or stopped.
    pub fn clear_detached(&self, kind: DetachedKind, ref_id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM detached_process WHERE kind=?1 AND ref_id=?2",
            params![kind.as_str(), ref_id],
        )?;
        Ok(())
    }

    /// All registry rows — the startup reconcile reads this to decide what to
    /// re-attach, finalize, or orphan.
    pub fn list_detached(&self) -> Result<Vec<DetachedRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT kind, ref_id, monitor_id, pid, proc_start, job_name, log_path,
                    capture_path, final_path, remux_to_mkv, take_group, spawn_build, started_at,
                    secondary, stream_id, went_live_at
             FROM detached_process ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let kind: String = r.get(0)?;
                Ok(DetachedRow {
                    kind: DetachedKind::from_str(&kind).unwrap_or(DetachedKind::Recording),
                    ref_id: r.get(1)?,
                    monitor_id: r.get(2)?,
                    pid: r.get::<_, i64>(3)? as u32,
                    proc_start: r.get::<_, i64>(4)? as u64,
                    job_name: r.get(5)?,
                    log_path: r.get(6)?,
                    capture_path: r.get(7)?,
                    final_path: r.get(8)?,
                    remux_to_mkv: r.get::<_, i64>(9)? != 0,
                    take_group: r.get(10)?,
                    spawn_build: r.get(11)?,
                    started_at: r.get(12)?,
                    secondary: r.get::<_, i64>(13)? != 0,
                    stream_id: r.get(14)?,
                    went_live_at: r.get(15)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record a title or game/category change observed during a recording take.
    pub fn insert_meta_change(
        &self,
        recording_id: i64,
        at_secs: i64,
        kind: &str,
        old_value: &str,
        new_value: &str,
    ) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO stream_meta_change(recording_id, at_secs, kind, old_value, new_value)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![recording_id, at_secs, kind, old_value, new_value],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All metadata changes for a recording take, in chronological order.
    pub fn meta_changes_for_recording(&self, recording_id: i64) -> Result<Vec<StreamMetaChange>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, recording_id, at_secs, kind, old_value, new_value FROM stream_meta_change
             WHERE recording_id = ?1 ORDER BY at_secs, id",
        )?;
        let rows = stmt
            .query_map(params![recording_id], |r| {
                Ok(StreamMetaChange {
                    id: r.get(0)?,
                    recording_id: r.get(1)?,
                    at_secs: r.get(2)?,
                    kind: r.get(3)?,
                    old_value: r.get(4)?,
                    new_value: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record a title or game/category change for a monitor, independent of
    /// any recording — see [`MonitorStreamChange`].
    pub fn insert_monitor_stream_change(
        &self,
        monitor_id: i64,
        at_unix: i64,
        kind: &str,
        old_value: &str,
        new_value: &str,
    ) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO monitor_stream_change(monitor_id, at_unix, kind, old_value, new_value)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![monitor_id, at_unix, kind, old_value, new_value],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All title/category history for a monitor, newest first.
    pub fn monitor_stream_changes(&self, monitor_id: i64) -> Result<Vec<MonitorStreamChange>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, at_unix, kind, old_value, new_value FROM monitor_stream_change
             WHERE monitor_id = ?1 ORDER BY at_unix DESC, id DESC",
        )?;
        let rows = stmt
            .query_map(params![monitor_id], |r| {
                Ok(MonitorStreamChange {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    at_unix: r.get(2)?,
                    kind: r.get(3)?,
                    old_value: r.get(4)?,
                    new_value: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::*;

    #[test]
    fn ad_break_roundtrip_and_rollups() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let rid = store
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();

        // Insert out of order; the query must return them ordered by offset.
        store.insert_ad_break(rid, 600, 30).unwrap();
        store.insert_ad_break(rid, 120, 15).unwrap();

        let breaks = store.ad_breaks_for_recording(rid).unwrap();
        assert_eq!(breaks.len(), 2);
        assert_eq!(breaks[0].at_secs, 120);
        assert_eq!(breaks[0].duration_secs, 15);
        assert_eq!(breaks[1].at_secs, 600);

        // recordings_for_monitor rolls up count + total seconds for the take.
        let recs = store.recordings_for_monitor(mid).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].ad_count, 2);
        assert_eq!(recs[0].ad_secs, 45);

        // The latest-recording join exposes the same rollup on the monitor row.
        let row = store.get_monitor_with_channel(mid).unwrap().unwrap();
        assert_eq!(row.last_recording_ad_count, 2);
        assert_eq!(row.last_recording_ad_secs, 45);

        // Deleting the recording cascades to its ad breaks.
        store.finish_recording(rid, 2_000, 1, Some(0), "completed", "C:/rec/out.mkv", "").unwrap();
        store.delete_recording(rid).unwrap();
        assert!(store.ad_breaks_for_recording(rid).unwrap().is_empty());
    }
    #[test]
    fn ad_break_insert_is_idempotent() {
        // A re-attach after a restart may re-observe a break already persisted; the
        // UNIQUE(recording_id, at_secs) index + INSERT OR IGNORE makes that a no-op.
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let rid = store
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();

        store.insert_ad_break(rid, 120, 15).unwrap();
        store.insert_ad_break(rid, 120, 15).unwrap(); // duplicate — ignored
        store.insert_ad_break(rid, 600, 30).unwrap(); // distinct offset — kept

        let breaks = store.ad_breaks_for_recording(rid).unwrap();
        assert_eq!(breaks.len(), 2);
    }
    #[test]
    fn monitor_stream_change_roundtrip_and_ordering() {
        // No recording involved at all — this ledger is independent of any take.
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        store.insert_monitor_stream_change(mid, 1_000, "title", "", "First title").unwrap();
        store.insert_monitor_stream_change(mid, 2_000, "title", "First title", "Second title").unwrap();
        store.insert_monitor_stream_change(mid, 1_500, "category", "", "Just Chatting").unwrap();

        let changes = store.monitor_stream_changes(mid).unwrap();
        assert_eq!(changes.len(), 3);
        // Newest first.
        assert_eq!(changes[0].at_unix, 2_000);
        assert_eq!(changes[0].new_value, "Second title");
        assert_eq!(changes[1].at_unix, 1_500);
        assert_eq!(changes[2].at_unix, 1_000);
        // A different monitor's history stays separate.
        let mut m2 = sample_monitor(cid);
        m2.channel_id = cid;
        let mid2 = store.insert_monitor(&m2).unwrap();
        assert!(store.monitor_stream_changes(mid2).unwrap().is_empty());
    }
    #[test]
    fn detached_registry_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let rec = DetachedRow {
            kind: DetachedKind::Recording,
            ref_id: 7,
            monitor_id: Some(3),
            pid: 4321,
            proc_start: 999,
            job_name: "Local\\SA_rec_7".into(),
            log_path: "C:/c/.cache/x.log".into(),
            capture_path: "C:/c/.cache/x.ts".into(),
            final_path: "C:/c/x.mkv".into(),
            remux_to_mkv: true,
            take_group: Some("3:1000".into()),
            spawn_build: "0.1.1/abc-dirty".into(),
            started_at: 1_000,
            secondary: false,
            stream_id: Some("s1".into()),
            went_live_at: Some(990),
        };
        store.register_detached(&rec).unwrap();
        // Re-registering the same (kind, ref_id) replaces rather than duplicates.
        let mut rec2 = rec.clone();
        rec2.pid = 8888;
        store.register_detached(&rec2).unwrap();

        let video = DetachedRow {
            kind: DetachedKind::Video,
            ref_id: 42,
            monitor_id: None,
            pid: 11,
            proc_start: 0,
            job_name: "Local\\SA_video_42".into(),
            log_path: "l".into(),
            capture_path: "c".into(),
            final_path: "f".into(),
            remux_to_mkv: false,
            take_group: None,
            spawn_build: "0.1.1/abc-dirty".into(),
            started_at: 5,
            secondary: false,
            stream_id: None,
            went_live_at: None,
        };
        store.register_detached(&video).unwrap();

        let rows = store.list_detached().unwrap();
        assert_eq!(rows.len(), 2);
        let got = rows
            .iter()
            .find(|r| r.kind == DetachedKind::Recording)
            .unwrap();
        assert_eq!(got.ref_id, 7);
        assert_eq!(got.pid, 8888); // the replacement won
        assert_eq!(got.monitor_id, Some(3));
        assert!(got.remux_to_mkv);
        assert_eq!(got.proc_start, 999);
        assert_eq!(got.take_group.as_deref(), Some("3:1000"));
        assert!(!got.secondary);
        assert_eq!(got.stream_id.as_deref(), Some("s1"));
        assert_eq!(got.went_live_at, Some(990));

        store.clear_detached(DetachedKind::Recording, 7).unwrap();
        let rows = store.list_detached().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, DetachedKind::Video);
    }
    #[test]
    fn meta_change_roundtrip_and_rollups() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let rid = store
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();

        // Insert out of order; the query returns them ordered by offset.
        store.insert_meta_change(rid, 300, "category", "Just Chatting", "Valorant").unwrap();
        store.insert_meta_change(rid, 0, "title", "", "starting soon").unwrap();
        store.insert_meta_change(rid, 0, "category", "", "Just Chatting").unwrap();

        // The raw log keeps all 3 rows (incl. the two initial values), so the
        // current-value lookups and {games} still see them.
        let changes = store.meta_changes_for_recording(rid).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].at_secs, 0);
        // The change log is ordered chronologically (at_secs, id); its last entry
        // is the latest category transition.
        assert_eq!(changes[2].kind, "category");
        assert_eq!(changes[2].new_value, "Valorant");

        // The COUNT, however, is only *actual* changes (a non-empty old_value):
        // here just the "Just Chatting" -> "Valorant" category transition. The two
        // initial values (empty old_value) are the starting state, not changes.
        let recs = store.recordings_for_monitor(mid).unwrap();
        assert_eq!(recs[0].meta_change_count, 1);
        let row = store.get_monitor_with_channel(mid).unwrap().unwrap();
        assert_eq!(row.last_recording_meta_changes, 1);

        // Current title/category = the chronologically-latest value of each kind
        // (at_secs DESC, id DESC) — so they agree with the LAST entry shown in the
        // change log above, even though the rows were inserted out of at_secs order:
        // the lone title, and the at_secs=300 "Valorant" category (not the id-newer
        // at_secs=0 baseline).
        assert_eq!(recs[0].title, "starting soon");
        assert_eq!(recs[0].category, "Valorant");
        assert_eq!(recs[0].category, changes[2].new_value); // matches the log's last entry
        assert_eq!(row.last_recording_title, "starting soon");
        assert_eq!(row.last_recording_category, "Valorant");

        // Deleting the recording cascades to its metadata changes.
        store.finish_recording(rid, 2_000, 1, Some(0), "completed", "C:/rec/out.mkv", "").unwrap();
        store.delete_recording(rid).unwrap();
        assert!(store.meta_changes_for_recording(rid).unwrap().is_empty());
    }
    #[test]
    fn vod_muted_secs_setter_preserves_vod_state_and_id() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_vod_found(rid, "999", 0).unwrap();
        store.set_recording_vod_muted_secs(rid, 360).unwrap();
        let rec = &store.recordings_for_monitor(mid).unwrap()[0];
        assert_eq!(rec.vod_id.as_deref(), Some("999"), "vod_id untouched");
        assert_eq!(rec.vod_state.as_deref(), Some("found"), "vod_state untouched");
        assert_eq!(rec.vod_muted_secs, Some(360));
    }
    #[test]
    fn vod_archive_replay_candidates_excludes_detached_and_terminal() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();

        // rec1: 'downloading' with a completed video → replay candidate.
        let r1 = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        let v1 = store.insert_video(&sample_video()).unwrap();
        store.finish_video(v1, 200, 10, Some(0), "completed", "C:/tmp/a.vod.mkv", "").unwrap();
        store.set_recording_vod_dl(r1, "downloading", Some(v1)).unwrap();

        // rec2: video completed but the recording already archived → not a candidate.
        let r2 = store
            .insert_recording(mid, 300, "C:/tmp/b.mkv", Some(250), false, Some("s2"), None, "", "")
            .unwrap();
        let v2 = store.insert_video(&sample_video()).unwrap();
        store.finish_video(v2, 400, 10, Some(0), "completed", "C:/tmp/b.vod.mkv", "").unwrap();
        store.set_recording_vod_dl(r2, "downloading", Some(v2)).unwrap();
        store.set_recording_vod_archived(r2, "C:/tmp/b.vod.mkv", "archived").unwrap();

        let cands = store.vod_archive_replay_candidates().unwrap();
        assert_eq!(
            cands,
            vec![(r1, v1, "C:/tmp/a.vod.mkv".to_string())],
            "only the stuck-downloading row replays"
        );

        // A video still in the detached registry is being adopted — excluded.
        store
            .register_detached(&crate::models::DetachedRow {
                kind: DetachedKind::Video,
                ref_id: v1,
                monitor_id: None,
                pid: 1,
                proc_start: 0,
                job_name: String::new(),
                log_path: String::new(),
                capture_path: String::new(),
                final_path: String::new(),
                remux_to_mkv: false,
                take_group: None,
                spawn_build: String::new(),
                started_at: 0,
                secondary: false,
                stream_id: None,
                went_live_at: None,
            })
            .unwrap();
        assert!(store.vod_archive_replay_candidates().unwrap().is_empty());
    }
}
