//! Recording rows: CRUD, head-backfill/concat state, orphan repair, path
//! relocation, and the listing/issue queries.

use super::*;

impl Store {
    // ----- recordings -----

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn insert_recording(
        &self,
        monitor_id: i64,
        started_at: i64,
        output_path: &str,
        went_live_at: Option<i64>,
        went_live_approx: bool,
        stream_id: Option<&str>,
        take_group: Option<&str>,
        trigger_info: &str,
        trigger_rule_json: &str,
    ) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO recording(monitor_id, started_at, output_path, status, went_live_at, went_live_approx, stream_id, take_group, trigger_info, trigger_rule_json)
             VALUES(?1, ?2, ?3, 'recording', ?4, ?5, ?6, ?7, ?8, ?9)",
            params![monitor_id, started_at, output_path, went_live_at, went_live_approx as i64, stream_id, take_group, trigger_info, trigger_rule_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_recording(
        &self,
        id: i64,
        ended_at: i64,
        bytes: i64,
        exit_code: Option<i64>,
        status: &str,
        output_path: &str,
        log_excerpt: &str,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET ended_at=?2, bytes=?3, exit_code=?4, status=?5, output_path=?6, log_excerpt=?7 WHERE id=?1",
            params![id, ended_at, bytes, exit_code, status, output_path, log_excerpt],
        )?;
        Ok(())
    }

    /// Update the output path of a finished recording — used after a manual
    /// re-remux succeeds to replace the `.ts` capture path with the final `.mkv`.
    pub fn update_recording_output_path(&self, id: i64, path: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET output_path = ?2 WHERE id = ?1",
            params![id, path],
        )?;
        Ok(())
    }

    /// Remove a recording (take) row from the history. The captured file on disk
    /// is left untouched. Refuses an in-progress ('recording') take so we never
    /// orphan a running capture from its history row; returns the rows removed.
    pub fn delete_recording(&self, id: i64) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "DELETE FROM recording WHERE id = ?1 AND status <> 'recording'",
            params![id],
        )?;
        Ok(n)
    }

    /// Set the resolved "missed footage" (seconds) for a recording. Used by the
    /// from-start catch-up watcher (0 on catch-up) and finalize (the residual).
    pub fn set_recording_lost_secs(&self, id: i64, lost_secs: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET lost_secs=?2 WHERE id=?1",
            params![id, lost_secs],
        )?;
        Ok(())
    }

    /// Update the user-authored notes for a recording take.
    pub fn set_recording_notes(&self, id: i64, notes: &str) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE recording SET notes=?2 WHERE id=?1", params![id, notes])?;
        Ok(())
    }

    /// Mark a recording as awaiting Twitch VOD resolution.
    pub fn set_recording_vod_pending(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_state='pending' WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    /// Record a confirmed Twitch VOD: the VOD id and total muted seconds (0 = clean).
    pub fn set_recording_vod_found(&self, id: i64, vod_id: &str, muted_secs: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_id=?2, vod_state='found', vod_muted_secs=?3 WHERE id=?1",
            params![id, vod_id, muted_secs],
        )?;
        Ok(())
    }

    /// Record that no Twitch VOD was published for this take (VOD-less stream —
    /// the local recording may be the only surviving copy).
    pub fn set_recording_vod_not_published(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_state='not_published' WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    /// Set a recording's CDN VOD-recovery status (`recovering`/`failed`/`unavailable`).
    pub fn set_recording_recovery_state(&self, id: i64, state: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET recovery_state=?2 WHERE id=?1",
            params![id, state],
        )?;
        Ok(())
    }

    /// Attach a recovered MKV to a recording with a terminal recovery status
    /// (`recovered` for a complete timeline, `partial` when segments were gone).
    pub fn set_recording_recovered(&self, id: i64, path: &str, state: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET recovered_path=?2, recovery_state=?3 WHERE id=?1",
            params![id, path, state],
        )?;
        Ok(())
    }

    // ----- live DVR head backfill (capture-from-start for Twitch) -----

    /// A recording's current lost-time value (`None` = not yet resolved).
    pub fn recording_lost_secs(&self, id: i64) -> Result<Option<i64>> {
        let conn = self.db();
        let v = conn
            .query_row(
                "SELECT lost_secs FROM recording WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    /// Record the live capture's first MPEG-TS PTS (ffprobe `format=start_time`,
    /// seconds) — the exact-splice anchor for head backfills (see the v57
    /// migration comment). First writer wins: the head-backfill job and the
    /// finalize path both probe the same growing `.ts`, so whichever lands
    /// first is the authoritative (identical) value, and a later re-probe of
    /// an already-remuxed file (PTS reset to ~0) can never clobber it.
    pub fn set_recording_capture_start_pts(&self, id: i64, pts_secs: f64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET capture_start_pts=?2 WHERE id=?1 AND capture_start_pts IS NULL",
            params![id, pts_secs],
        )?;
        Ok(())
    }

    /// The persisted capture-start PTS, if one was ever probed (`None` = never
    /// probed, e.g. a take finished before this feature or a non-TS capture).
    pub fn recording_capture_start_pts(&self, id: i64) -> Result<Option<f64>> {
        let conn = self.db();
        let v = conn
            .query_row(
                "SELECT capture_start_pts FROM recording WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<f64>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    /// Set/clear a recording's pending-head-backfill marker. `"queued"` while
    /// `head_backfill_job` hasn't yet decided whether there's anything to
    /// fetch; `""` once it has (started fetching, or determined nothing was
    /// needed) — see [`crate::downloader::HEAD_BACKFILL_SETTLE_SECS`].
    pub fn set_head_backfill_state(&self, id: i64, state: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET head_backfill_state=?2 WHERE id=?1",
            params![id, state],
        )?;
        Ok(())
    }

    /// Set/clear a recording's gap-splice state — see
    /// [`crate::models::Recording::gap_splice_state`].
    pub fn set_gap_splice_state(&self, id: i64, state: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET gap_splice_state=?2 WHERE id=?1",
            params![id, state],
        )?;
        Ok(())
    }

    /// Set/clear a failed/aborted/orphaned take's acknowledged flag — see
    /// [`crate::models::Recording::err_ack`].
    pub fn set_recording_err_ack(&self, id: i64, ack: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET err_ack=?2 WHERE id=?1",
            params![id, ack as i64],
        )?;
        Ok(())
    }

    /// Stamp a take as having been silently downgraded to live-edge-only
    /// (SABR DVR-window exceeded) — see
    /// [`crate::models::Recording::sabr_live_edge_fallback`]. Set once,
    /// right after the row is inserted; never cleared.
    pub fn set_sabr_live_edge_fallback(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET sabr_live_edge_fallback=1 WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    /// How many recording rows share `id`'s `take_group` (including itself)
    /// — `1` for a solo take or one with no take_group at all. Gap-splice's
    /// split-part exclusion: a take stitched from more than one leg
    /// (crash/reconnect) has no guarantee `capture_start_pts` (anchored to
    /// leg 1 only) still describes the file gap-splice would operate on.
    pub fn recording_take_group_size(&self, id: i64) -> Result<i64> {
        let conn = self.db();
        let take_group: Option<String> = conn
            .query_row("SELECT take_group FROM recording WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?
            .flatten();
        let Some(tg) = take_group.filter(|s| !s.is_empty()) else {
            return Ok(1);
        };
        conn.query_row(
            "SELECT COUNT(*) FROM recording WHERE take_group = ?1",
            params![tg],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    /// Takes currently awaiting a head-backfill decision (still inside
    /// `head_backfill_job`'s settle wait / probing), oldest first — feeds the
    /// Background view's "Planned" section.
    pub fn queued_head_backfills(&self) -> Result<Vec<crate::models::QueuedHeadBackfill>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT c.name, r.started_at
             FROM recording r
             JOIN monitor m ON m.id = r.monitor_id
             JOIN channel c ON c.id = m.channel_id
             WHERE r.head_backfill_state = 'queued'
             ORDER BY r.started_at",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::QueuedHeadBackfill {
                    channel: r.get(0)?,
                    started_at: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Attach the backfilled head file (`{stem}.head.mkv`) to a recording.
    pub fn set_recording_backfill_path(&self, id: i64, path: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET backfill_path=?2 WHERE id=?1",
            params![id, path],
        )?;
        Ok(())
    }

    /// Attach the concatenated full file (`{stem}.full.mkv`) to a recording.
    pub fn set_recording_full_path(&self, id: i64, path: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET full_path=?2 WHERE id=?1",
            params![id, path],
        )?;
        Ok(())
    }

    /// `(status, output_path, backfill_path, full_path)` — what the head-concat
    /// step needs to decide whether both parts are ready to join.
    #[allow(clippy::type_complexity)]
    pub fn backfill_concat_info(
        &self,
        id: i64,
    ) -> Result<Option<(String, String, Option<String>, Option<String>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT status, COALESCE(output_path, ''), backfill_path, full_path
                 FROM recording WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// True when no earlier *viable* take exists for the same monitor +
    /// platform stream — only the earliest take of a stream owns the missed
    /// HEAD; later takes' gaps are mid-stream and not this feature's job.
    ///
    /// An earlier take that captured nothing at all (`status='failed'` with
    /// `bytes=0` — it never even started writing) doesn't count as "earlier":
    /// its own head-backfill job computed a bogus near-zero "missed" gap from
    /// its own stale `started_at` (which is ~equal to `went_live_at` for an
    /// instant failure) and quietly skipped with "gap too small". Without
    /// this exclusion, a stream whose first recording attempt dies instantly
    /// (e.g. a transient tool crash, or a too-long filename before the
    /// MAX_PATH fix) permanently loses head-backfill for the whole stream —
    /// the take that actually captures it is never considered "first" and
    /// never gets its own job.
    pub fn is_first_take_for_stream(
        &self,
        monitor_id: i64,
        stream_id: &str,
        started_at: i64,
    ) -> Result<bool> {
        let conn = self.db();
        let earlier: i64 = conn.query_row(
            "SELECT COUNT(*) FROM recording
             WHERE monitor_id = ?1 AND stream_id = ?2 AND started_at < ?3
               AND NOT (status = 'failed' AND bytes = 0)",
            params![monitor_id, stream_id, started_at],
            |r| r.get(0),
        )?;
        Ok(earlier == 0)
    }

    /// Recordings with a backfilled head that still lacks the final concat
    /// (crash healing — the join is idempotent and re-runnable).
    pub fn recordings_pending_head_concat(&self) -> Result<Vec<i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id FROM recording
             WHERE backfill_path IS NOT NULL AND full_path IS NULL
               AND status != 'recording'",
        )?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(rows)
    }

    /// Other recordings of the same broadcast that still carry a standalone
    /// head file — candidates a fresh, verified-good head backfill can
    /// supersede (see `Supervisor::supersede_old_heads`).
    pub fn recordings_with_backfill_for_stream(
        &self,
        monitor_id: i64,
        stream_id: &str,
        exclude_id: i64,
    ) -> Result<Vec<(i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, backfill_path FROM recording
             WHERE monitor_id = ?1 AND stream_id = ?2 AND id != ?3
               AND backfill_path IS NOT NULL",
        )?;
        let rows = stmt
            .query_map(params![monitor_id, stream_id, exclude_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<(i64, String)>>>()?;
        Ok(rows)
    }

    /// Clear a recording's head-backfill reference — the file itself is
    /// deleted by the caller first. Used when a later take's fresh head
    /// supersedes an older take's now-redundant head file.
    pub fn clear_recording_backfill_path(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET backfill_path=NULL WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }
    /// Promote orphaned recordings that have a non-TS final output file to
    /// 'completed'. These are captures where the app crashed after the file was
    /// fully written but before the status column was updated — the content is
    /// intact, so 'orphaned' is a misnomer. Returns the count updated.
    /// Candidates for the disk-aware startup repair pass
    /// ([`crate::downloader::Supervisor::reconcile_orphan_outputs`]): rows whose
    /// `output_path` claims a promoted final file but whose on-disk truth is
    /// unverified — fresh crash orphans, plus rows an older (DB-only) promotion
    /// already flipped to 'completed' with `bytes = 0` and no file behind them.
    /// Returns `(id, status, output_path)`.
    pub fn orphan_repair_candidates(&self) -> Result<Vec<(i64, String, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, status, output_path FROM recording
             WHERE output_path != ''
               AND output_path NOT LIKE '%.ts'
               AND output_path NOT LIKE '%.cache%'
               AND output_path NOT LIKE '%.sa-cache%'
               AND (status = 'orphaned'
                    OR (status IN ('completed', 'ended', 'stopped', 'failed') AND bytes = 0))",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Promote a repair candidate to 'completed' with its verified on-disk size
    /// (the disk-aware replacement for the old blind orphan promotion).
    pub fn promote_orphan_completed(&self, id: i64, bytes: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET status='completed', bytes=?2 WHERE id=?1",
            params![id, bytes],
        )?;
        Ok(())
    }

    /// Every recording's stored output path with its DB size — the raw feed
    /// for anything that must see PAST recording locations too (an instance
    /// moved from A: to D: leaves its old takes on A:): the I/O monitor's
    /// drive set, the startup cache sweep, and the Files view's per-location
    /// stats.
    pub fn recording_paths_with_bytes(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT output_path, bytes FROM recording WHERE output_path != ''",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per-monitor recording stats: `(monitor_id, count, total bytes)`.
    pub fn recording_stats_by_monitor(&self) -> Result<Vec<(i64, i64, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT monitor_id, COUNT(*), COALESCE(SUM(bytes), 0)
             FROM recording GROUP BY monitor_id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Change one monitor's output folder (Files view inline edit). Future
    /// recordings land there; existing rows keep their absolute paths.
    pub fn set_monitor_output_dir(&self, id: i64, dir: &str) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE monitor SET output_dir=?2 WHERE id=?1", params![id, dir])?;
        Ok(())
    }

    /// How many recording rows / video rows / monitors have a stored path
    /// starting with `from` — the preview for [`Self::replace_path_prefix`].
    pub fn count_path_prefix_matches(&self, from: &str) -> Result<(i64, i64, i64)> {
        let conn = self.db();
        let m = |col: &str| format!("substr(COALESCE({col}, ''), 1, length(?1)) = ?1");
        let recs: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM recording WHERE {} OR {} OR {} OR {} OR {}",
                m("output_path"),
                m("backfill_path"),
                m("full_path"),
                m("recovered_path"),
                m("vod_dl_path"),
            ),
            params![from],
            |r| r.get(0),
        )?;
        let vids: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM video WHERE {} OR {}", m("output_path"), m("output_dir")),
            params![from],
            |r| r.get(0),
        )?;
        let mons: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM monitor WHERE {}", m("output_dir")),
            params![from],
            |r| r.get(0),
        )?;
        Ok((recs, vids, mons))
    }

    /// Rewrite the leading `from` prefix to `to` in every stored path column
    /// (recordings: output/backfill/full/recovered/vod-download paths; videos:
    /// output path + dir; optionally monitor output dirs). This is a
    /// DB-side remap for files the user physically moved (e.g. a drive
    /// migration A:\ → D:\) — no files are touched. Returns
    /// `(recording cols updated, video cols updated, monitors updated)`.
    pub fn replace_path_prefix(
        &self,
        from: &str,
        to: &str,
        include_monitor_dirs: bool,
    ) -> Result<(usize, usize, usize)> {
        let conn = self.db();
        let mut recs = 0usize;
        for col in ["output_path", "backfill_path", "full_path", "recovered_path", "vod_dl_path"] {
            recs += conn.execute(
                &format!(
                    "UPDATE recording SET {col} = ?2 || substr({col}, length(?1) + 1)
                     WHERE substr(COALESCE({col}, ''), 1, length(?1)) = ?1"
                ),
                params![from, to],
            )?;
        }
        let mut vids = 0usize;
        for col in ["output_path", "output_dir"] {
            vids += conn.execute(
                &format!(
                    "UPDATE video SET {col} = ?2 || substr({col}, length(?1) + 1)
                     WHERE substr(COALESCE({col}, ''), 1, length(?1)) = ?1"
                ),
                params![from, to],
            )?;
        }
        let mons = if include_monitor_dirs {
            conn.execute(
                "UPDATE monitor SET output_dir = ?2 || substr(output_dir, length(?1) + 1)
                 WHERE substr(output_dir, 1, length(?1)) = ?1",
                params![from, to],
            )?
        } else {
            0
        };
        Ok((recs, vids, mons))
    }

    /// Recordings whose head backfill was marked planned (`head_backfill_state
    /// = 'queued'`) but whose in-memory job died with a previous session — the
    /// startup requeue re-drives (or clears) these so "Planned" can't persist
    /// across restarts forever.
    pub fn recordings_head_backfill_queued(&self) -> Result<Vec<i64>> {
        let conn = self.db();
        let mut stmt =
            conn.prepare("SELECT id FROM recording WHERE head_backfill_state = 'queued'")?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark any recordings still flagged 'recording' (i.e. left over from a
    /// crash) as 'orphaned'. Returns the number updated. Called on startup.
    pub fn mark_orphaned_recordings(&self, ended_at: i64) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE recording SET status='orphaned', ended_at=?1 WHERE status='recording'",
            params![ended_at],
        )?;
        Ok(n)
    }

    /// Mark a single in-flight recording 'orphaned' (used at startup for crash
    /// leftovers that aren't being resumed). No-op if it's no longer 'recording'.
    pub fn mark_recording_orphaned(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET status='orphaned', ended_at=?2 WHERE id=?1 AND status='recording'",
            params![id, now_unix()],
        )?;
        Ok(())
    }

    /// All monitors joined with their channel, ordered by channel name.
    pub fn list_monitors_with_channels(&self) -> Result<Vec<MonitorWithChannel>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT
                c.id, c.name, c.url, c.platform, c.created_at,
                m.id, m.channel_id, m.enabled, m.tool, m.detection_method, m.poll_interval_secs,
                m.quality, m.output_dir, m.filename_template, m.container, m.capture_from_start,
                m.auth_kind, m.auth_value, m.extra_args, m.max_concurrent, m.last_checked_at,
                m.last_state,
                r.started_at, r.ended_at, r.status, r.went_live_at, r.went_live_approx, r.lost_secs,
                (SELECT COUNT(*) FROM recording rc WHERE rc.monitor_id = m.id),
                m.url,
                (SELECT COUNT(*) FROM ad_break ab WHERE ab.recording_id = r.id),
                COALESCE((SELECT SUM(ab.duration_secs) FROM ad_break ab WHERE ab.recording_id = r.id), 0),
                m.ad_free, m.ad_free_sub, m.audio_tracks, m.subtitle_tracks,
                (SELECT COUNT(*) FROM stream_meta_change smc
                 WHERE smc.recording_id = r.id AND smc.old_value != ''),
                m.chat_log, COALESCE(r.log_excerpt, ''),
                m.fetch_thumbnail, m.fetch_chat_assets,
                COALESCE((SELECT new_value FROM stream_meta_change smc
                          WHERE smc.recording_id = r.id AND smc.kind = 'title'
                          ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), ''),
                COALESCE((SELECT new_value FROM stream_meta_change smc
                          WHERE smc.recording_id = r.id AND smc.kind = 'category'
                          ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), ''),
                c.color,
                m.dual_capture,
                c.preferred_platform,
                m.thumbnail_in_toast,
                c.enabled,
                m.sabr_codec_pref, m.sabr_codec_custom,
                COALESCE(r.trigger_info, ''),
                m.automation_enabled, c.automation_enabled,
                m.last_title, m.last_game, m.last_thumbnail_url, m.last_viewers,
                m.last_live_since, m.last_live_since_approx, m.last_collab,
                m.capture_offline, m.last_tags, m.last_language, r.err_ack
             FROM monitor m
             JOIN channel c ON c.id = m.channel_id
             LEFT JOIN recording r
                ON r.id = (SELECT id FROM recording r2 WHERE r2.monitor_id = m.id ORDER BY r2.id DESC LIMIT 1)
             ORDER BY c.name COLLATE NOCASE, c.id, m.id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let channel = Channel {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    url: r.get(2)?,
                    platform: Platform::parse(&r.get::<_, String>(3)?),
                    created_at: r.get(4)?,
                    color: r.get(43)?,
                    preferred_asset: crate::models::PreferredAssetSource::parse(
                        &r.get::<_, String>(45)?,
                    ),
                    enabled: r.get::<_, i64>(47)? != 0,
                    automation_enabled: r.get::<_, i64>(52)? != 0,
                };
                let monitor = Monitor {
                    id: r.get(5)?,
                    channel_id: r.get(6)?,
                    url: r.get(29)?,
                    enabled: r.get::<_, i64>(7)? != 0,
                    automation_enabled: r.get::<_, i64>(51)? != 0,
                    tool: Tool::parse(&r.get::<_, String>(8)?),
                    detection_method: DetectionMethod::parse(&r.get::<_, String>(9)?),
                    poll_interval_secs: r.get(10)?,
                    quality: r.get(11)?,
                    output_dir: r.get(12)?,
                    filename_template: r.get(13)?,
                    container: Container::parse(&r.get::<_, String>(14)?),
                    capture_from_start: r.get::<_, i64>(15)? != 0,
                    dual_capture: r.get::<_, i64>(44)? != 0,
                    sabr_codec_pref: SabrCodecPref::parse(&r.get::<_, String>(48)?),
                    sabr_codec_custom: r.get(49)?,
                    ad_free: r.get::<_, i64>(32)? != 0,
                    auth_kind: AuthKind::parse(&r.get::<_, String>(16)?),
                    auth_value: r.get(17)?,
                    audio_tracks: r.get(34)?,
                    subtitle_tracks: r.get(35)?,
                    chat_log: r.get::<_, i64>(37)? != 0,
                    fetch_thumbnail: r.get::<_, i64>(39)? != 0,
                    thumbnail_in_toast: r.get::<_, i64>(46)? != 0,
                    fetch_chat_assets: r.get::<_, i64>(40)? != 0,
                    extra_args: r.get(18)?,
                    max_concurrent: r.get(19)?,
                    last_checked_at: r.get(20)?,
                    last_state: r.get(21)?,
                    last_live_since: r.get(57)?,
                    last_live_since_approx: r.get::<_, Option<i64>>(58)?.unwrap_or(0) != 0,
                };
                Ok(MonitorWithChannel {
                    channel,
                    monitor,
                    last_recording_started: r.get(22)?,
                    last_recording_ended: r.get(23)?,
                    last_recording_status: r.get(24)?,
                    last_recording_went_live: r.get(25)?,
                    last_recording_went_live_approx: r.get::<_, Option<i64>>(26)?.unwrap_or(0) != 0,
                    last_recording_lost_secs: r.get(27)?,
                    last_recording_ad_count: r.get(30)?,
                    last_recording_ad_secs: r.get(31)?,
                    last_recording_meta_changes: r.get(36)?,
                    last_recording_log: r.get(38)?,
                    last_recording_title: r.get(41)?,
                    last_recording_category: r.get(42)?,
                    ad_free_sub: r.get::<_, Option<i64>>(33)?.map(|v| v != 0),
                    recording_count: r.get(28)?,
                    last_recording_trigger: r.get(50)?,
                    last_title: r.get(53)?,
                    last_game: r.get(54)?,
                    last_thumbnail_url: r.get(55)?,
                    last_viewers: r.get(56)?,
                    live_collab: crate::models::CollabLive::parse(&r.get::<_, String>(59)?),
                    capture_offline: r.get::<_, i64>(60)? != 0,
                    last_tags: r.get(61)?,
                    last_language: r.get(62)?,
                    // NULL when the monitor has no recordings yet (the LEFT JOIN).
                    last_recording_err_ack: r.get::<_, Option<i64>>(63)?.unwrap_or(0) != 0,
                    // Filled by the UI from next_scheduled_streams(), not this query.
                    next_stream_at: None,
                    next_stream_title: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All recording takes for a monitor (oldest first), for the history tree.
    pub fn recordings_for_monitor(&self, monitor_id: i64) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status, bytes, exit_code,
                    COALESCE(output_path, ''), went_live_at, went_live_approx, lost_secs, stream_id,
                    (SELECT COUNT(*) FROM ad_break ab WHERE ab.recording_id = recording.id),
                    COALESCE((SELECT SUM(ab.duration_secs) FROM ad_break ab WHERE ab.recording_id = recording.id), 0),
                    (SELECT COUNT(*) FROM stream_meta_change smc
                     WHERE smc.recording_id = recording.id AND smc.old_value != ''),
                    COALESCE(log_excerpt, ''),
                    COALESCE((SELECT new_value FROM stream_meta_change smc
                              WHERE smc.recording_id = recording.id AND smc.kind = 'title'
                              ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), ''),
                    COALESCE((SELECT new_value FROM stream_meta_change smc
                              WHERE smc.recording_id = recording.id AND smc.kind = 'category'
                              ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), ''),
                    take_group, COALESCE(notes, ''),
                    vod_id, vod_state, vod_muted_secs,
                    recovery_state, recovered_path,
                    vod_dl_state, vod_dl_path, vod_dl_video_id,
                    backfill_path, full_path, COALESCE(trigger_info, ''),
                    head_backfill_state, COALESCE(trigger_rule_json, ''), vod_views,
                    gap_splice_state, err_ack, sabr_live_edge_fallback
             FROM recording WHERE monitor_id = ?1 ORDER BY started_at, id",
        )?;
        let rows = stmt
            .query_map(params![monitor_id], |r| {
                Ok(crate::models::Recording {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    status: r.get(4)?,
                    bytes: r.get(5)?,
                    exit_code: r.get(6)?,
                    output_path: r.get(7)?,
                    went_live_at: r.get(8)?,
                    went_live_approx: r.get::<_, Option<i64>>(9)?.unwrap_or(0) != 0,
                    lost_secs: r.get(10)?,
                    stream_id: r.get(11)?,
                    take_group: r.get(18)?,
                    ad_count: r.get(12)?,
                    ad_secs: r.get(13)?,
                    meta_change_count: r.get(14)?,
                    title: r.get(16)?,
                    category: r.get(17)?,
                    log_excerpt: r.get(15)?,
                    notes: r.get(19)?,
                    vod_id: r.get(20)?,
                    vod_state: r.get(21)?,
                    vod_muted_secs: r.get(22)?,
                    recovery_state: r.get(23)?,
                    recovered_path: r.get(24)?,
                    vod_dl_state: r.get(25)?,
                    vod_dl_path: r.get(26)?,
                    vod_dl_video_id: r.get(27)?,
                    backfill_path: r.get(28)?,
                    full_path: r.get(29)?,
                    trigger_info: r.get(30)?,
                    head_backfill_state: r.get(31)?,
                    trigger_rule_json: r.get(32)?,
                    vod_views: r.get(33)?,
                    gap_splice_state: r.get(34)?,
                    err_ack: r.get::<_, i64>(35)? != 0,
                    sabr_live_edge_fallback: r.get::<_, i64>(36)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Same column list + row shape as `recordings_for_monitor`, factored out
    /// for the single-row lookups below.
    const RECORDING_FULL_COLUMNS: &str = "id, monitor_id, started_at, ended_at, status, bytes, exit_code,
            COALESCE(output_path, ''), went_live_at, went_live_approx, lost_secs, stream_id,
            (SELECT COUNT(*) FROM ad_break ab WHERE ab.recording_id = recording.id),
            COALESCE((SELECT SUM(ab.duration_secs) FROM ad_break ab WHERE ab.recording_id = recording.id), 0),
            (SELECT COUNT(*) FROM stream_meta_change smc
             WHERE smc.recording_id = recording.id AND smc.old_value != ''),
            COALESCE(log_excerpt, ''),
            COALESCE((SELECT new_value FROM stream_meta_change smc
                      WHERE smc.recording_id = recording.id AND smc.kind = 'title'
                      ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), ''),
            COALESCE((SELECT new_value FROM stream_meta_change smc
                      WHERE smc.recording_id = recording.id AND smc.kind = 'category'
                      ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), ''),
            take_group, COALESCE(notes, ''),
            vod_id, vod_state, vod_muted_secs,
            recovery_state, recovered_path,
            vod_dl_state, vod_dl_path, vod_dl_video_id,
            backfill_path, full_path, COALESCE(trigger_info, ''),
            head_backfill_state, COALESCE(trigger_rule_json, ''), vod_views,
            gap_splice_state, err_ack, sabr_live_edge_fallback";

    fn map_recording_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::models::Recording> {
        Ok(crate::models::Recording {
            id: r.get(0)?,
            monitor_id: r.get(1)?,
            started_at: r.get(2)?,
            ended_at: r.get(3)?,
            status: r.get(4)?,
            bytes: r.get(5)?,
            exit_code: r.get(6)?,
            output_path: r.get(7)?,
            went_live_at: r.get(8)?,
            went_live_approx: r.get::<_, Option<i64>>(9)?.unwrap_or(0) != 0,
            lost_secs: r.get(10)?,
            stream_id: r.get(11)?,
            take_group: r.get(18)?,
            ad_count: r.get(12)?,
            ad_secs: r.get(13)?,
            meta_change_count: r.get(14)?,
            title: r.get(16)?,
            category: r.get(17)?,
            log_excerpt: r.get(15)?,
            notes: r.get(19)?,
            vod_id: r.get(20)?,
            vod_state: r.get(21)?,
            vod_muted_secs: r.get(22)?,
            recovery_state: r.get(23)?,
            recovered_path: r.get(24)?,
            vod_dl_state: r.get(25)?,
            vod_dl_path: r.get(26)?,
            vod_dl_video_id: r.get(27)?,
            backfill_path: r.get(28)?,
            full_path: r.get(29)?,
            trigger_info: r.get(30)?,
            head_backfill_state: r.get(31)?,
            trigger_rule_json: r.get(32)?,
            vod_views: r.get(33)?,
            gap_splice_state: r.get(34)?,
            err_ack: r.get::<_, i64>(35)? != 0,
            sabr_live_edge_fallback: r.get::<_, i64>(36)? != 0,
        })
    }

    /// A single recording by id (full row) — used by manual per-take actions
    /// (e.g. the "Backfill head" context-menu action).
    pub fn get_recording(&self, id: i64) -> Result<Option<crate::models::Recording>> {
        let conn = self.db();
        let row = conn
            .query_row(
                &format!("SELECT {} FROM recording WHERE id = ?1", Self::RECORDING_FULL_COLUMNS),
                params![id],
                Self::map_recording_row,
            )
            .optional()?;
        Ok(row)
    }

    /// The newest recording's `(stream_id, went_live_at)` for a monitor — the
    /// broadcast identity a manual stop-hold is anchored to ("don't restart
    /// until a NEW stream" = a different id / newer go-live than this).
    pub fn latest_stream_identity(
        &self,
        monitor_id: i64,
    ) -> Result<Option<(Option<String>, Option<i64>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT stream_id, went_live_at FROM recording
                 WHERE monitor_id = ?1 ORDER BY started_at DESC LIMIT 1",
                params![monitor_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// One recording's platform stream id ('' when unknown/id-less) — links a
    /// take to its broadcast, e.g. for keying the collab-session refresh.
    pub fn recording_stream_id(&self, rec_id: i64) -> Result<String> {
        let conn = self.db();
        Ok(conn
            .query_row(
                "SELECT stream_id FROM recording WHERE id = ?1",
                params![rec_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or_default())
    }

    /// Recordings whose head backfill exists but can't be losslessly joined
    /// with the live capture (differing codec parameters — typically the live
    /// take joined before Twitch listed the source rendition, so it captured
    /// a transcode while the head fetched at source). Surfaced in Issues with
    /// the fixes. Listed newest-first.
    pub fn recordings_with_head_mismatch(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM recording WHERE head_backfill_state = 'mismatch'
             ORDER BY started_at DESC",
            Self::RECORDING_FULL_COLUMNS
        ))?;
        let rows = stmt
            .query_map([], Self::map_recording_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Recordings whose gap-splice attempt was blocked by a safety check
    /// (codec mismatch, an untrustworthy PTS anchor, or a failed post-splice
    /// verification) — never a state a user needs to act on urgently (the
    /// recording is intact either way), but surfaced in Issues so a
    /// permanently-unspliced gap patch isn't a silent dead end. Listed
    /// newest-first.
    pub fn recordings_with_gap_splice_issue(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM recording
             WHERE gap_splice_state IN ('mismatch', 'anchor_failed', 'verify_failed')
             ORDER BY started_at DESC",
            Self::RECORDING_FULL_COLUMNS
        ))?;
        let rows = stmt
            .query_map([], Self::map_recording_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Recordings whose output path is still a `.ts` file inside a `.cache`
    /// directory — these finished capturing but were never successfully remuxed
    /// to the final MKV container. Listed newest-first.
    pub fn recordings_needing_remux(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status,
                    COALESCE(output_path, ''), went_live_at, went_live_approx,
                    take_group, COALESCE(log_excerpt, '')
             FROM recording
             WHERE output_path LIKE '%.ts'
               AND (output_path LIKE '%.cache%' OR output_path LIKE '%.sa-cache%')
             ORDER BY started_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::Recording {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    status: r.get(4)?,
                    output_path: r.get(5)?,
                    went_live_at: r.get(6)?,
                    went_live_approx: r.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                    take_group: r.get(8)?,
                    log_excerpt: r.get(9)?,
                    bytes: 0,
                    exit_code: None,
                    lost_secs: None,
                    stream_id: None,
                    ad_count: 0,
                    ad_secs: 0,
                    meta_change_count: 0,
                    title: String::new(),
                    category: String::new(),
                    notes: String::new(),
                    vod_id: None,
                    vod_state: None,
                    vod_muted_secs: None,
                    vod_views: None,
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
                    gap_splice_state: String::new(),
                    err_ack: false,
                    sabr_live_edge_fallback: false,
                    trigger_rule_json: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Recordings whose capture fully succeeded but whose promote-to-output-dir
    /// move never completed — the file is a non-`.ts` container (`.mkv`, e.g. a
    /// SABR/DASH direct-write) still sitting in the source `.cache\`. Distinct
    /// from [`Self::recordings_needing_remux`] (a `.ts` awaiting a remux to
    /// MKV) — this is a plain move that failed, most commonly because the
    /// filename overflowed the filesystem's length limit (see
    /// `downloader::rename_or_shorten`).
    pub fn recordings_stuck_in_cache(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status,
                    COALESCE(output_path, ''), went_live_at, went_live_approx,
                    take_group, COALESCE(log_excerpt, '')
             FROM recording
             WHERE status = 'completed'
               AND (output_path LIKE '%.cache%' OR output_path LIKE '%.sa-cache%')
               AND output_path NOT LIKE '%.ts'
             ORDER BY started_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::Recording {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    status: r.get(4)?,
                    output_path: r.get(5)?,
                    went_live_at: r.get(6)?,
                    went_live_approx: r.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                    take_group: r.get(8)?,
                    log_excerpt: r.get(9)?,
                    bytes: 0,
                    exit_code: None,
                    lost_secs: None,
                    stream_id: None,
                    ad_count: 0,
                    ad_secs: 0,
                    meta_change_count: 0,
                    title: String::new(),
                    category: String::new(),
                    notes: String::new(),
                    vod_id: None,
                    vod_state: None,
                    vod_muted_secs: None,
                    vod_views: None,
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
                    gap_splice_state: String::new(),
                    err_ack: false,
                    sabr_live_edge_fallback: false,
                    trigger_rule_json: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// File stems of every recording whose CURRENT `output_path` still points
    /// into a `.cache\` working dir, regardless of status or extension — used
    /// to protect them from [`crate::downloader::Supervisor::sweep_caches`]'s
    /// age-based cleanup. That sweep can't distinguish genuine leftover
    /// garbage from a fully-valid, successfully-captured recording that's
    /// merely stuck there because its promote-to-output-dir move failed (see
    /// [`Self::recordings_stuck_in_cache`]) — without this exclusion, such a
    /// recording would be silently deleted after 24 hours.
    pub fn stems_in_cache(&self) -> Result<Vec<String>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT output_path FROM recording
             WHERE output_path LIKE '%.cache%' OR output_path LIKE '%.sa-cache%'",
        )?;
        let stems = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|p| p.ok())
            .filter_map(|p| {
                std::path::Path::new(&p)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .collect();
        Ok(stems)
    }

    /// Recordings that have a non-TS final output path but whose file no longer
    /// exists on disk — e.g. the user manually deleted the file. Returns the most
    /// recent 500 candidates; caller filters with `path.exists()`.
    pub fn recordings_with_final_path(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status,
                    COALESCE(output_path, ''), went_live_at, went_live_approx,
                    take_group, COALESCE(log_excerpt, '')
             FROM recording
             WHERE output_path != ''
               AND output_path NOT LIKE '%.ts'
               AND status NOT IN ('recording')
             ORDER BY started_at DESC
             LIMIT 500",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::Recording {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    status: r.get(4)?,
                    output_path: r.get(5)?,
                    went_live_at: r.get(6)?,
                    went_live_approx: r.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                    take_group: r.get(8)?,
                    log_excerpt: r.get(9)?,
                    bytes: 0,
                    exit_code: None,
                    lost_secs: None,
                    stream_id: None,
                    ad_count: 0,
                    ad_secs: 0,
                    meta_change_count: 0,
                    title: String::new(),
                    category: String::new(),
                    notes: String::new(),
                    vod_id: None,
                    vod_state: None,
                    vod_muted_secs: None,
                    vod_views: None,
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
                    gap_splice_state: String::new(),
                    err_ack: false,
                    sabr_live_edge_fallback: false,
                    trigger_rule_json: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Clear a stuck capture from the Issues panel: wipe `output_path` so the
    /// recording no longer matches the `recordings_needing_remux` query. Status is
    /// left as-is (already 'failed'/'completed'); the file itself must be deleted
    /// by the caller before this is called.
    pub fn clear_recording_capture(&self, rec_id: i64) -> rusqlite::Result<()> {
        self.db()
            .execute("UPDATE recording SET output_path = '' WHERE id = ?", [rec_id])?;
        Ok(())
    }

    /// Failed, aborted, or orphaned recordings that are not already caught by
    /// [`Self::recordings_needing_remux`] (ts-in-cache). Returns newest-first, up to 200.
    ///
    /// Excludes orphaned recordings that have a non-TS final output path — those
    /// are intact files where the app crashed after the capture finished but before
    /// `status` was updated; they should be shown as completed, not errors.
    pub fn recordings_with_errors(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status,
                    COALESCE(output_path, ''), went_live_at, went_live_approx,
                    take_group, COALESCE(log_excerpt, ''), exit_code
             FROM recording
             WHERE status IN ('failed', 'aborted', 'orphaned')
               AND err_ack = 0
               AND NOT (output_path LIKE '%.ts'
                        AND (output_path LIKE '%.cache%' OR output_path LIKE '%.sa-cache%'))
               AND NOT (status = 'orphaned'
                        AND output_path != ''
                        AND output_path NOT LIKE '%.ts'
                        AND output_path NOT LIKE '%.cache%'
                        AND output_path NOT LIKE '%.sa-cache%')
             ORDER BY started_at DESC
             LIMIT 200",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::Recording {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    status: r.get(4)?,
                    output_path: r.get(5)?,
                    went_live_at: r.get(6)?,
                    went_live_approx: r.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                    take_group: r.get(8)?,
                    log_excerpt: r.get(9)?,
                    exit_code: r.get(10)?,
                    bytes: 0,
                    lost_secs: None,
                    stream_id: None,
                    ad_count: 0,
                    ad_secs: 0,
                    meta_change_count: 0,
                    title: String::new(),
                    category: String::new(),
                    notes: String::new(),
                    vod_id: None,
                    vod_state: None,
                    vod_muted_secs: None,
                    vod_views: None,
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
                    gap_splice_state: String::new(),
                    err_ack: false,
                    sabr_live_edge_fallback: false,
                    trigger_rule_json: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }


    /// Every row still marked `recording`. The Issues scan pairs each with an
    /// on-disk activity probe to spot rows whose capture died (or whose
    /// finalize is pending) without the status ever settling.
    pub fn recordings_marked_recording(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status,
                    COALESCE(output_path, ''), went_live_at, went_live_approx,
                    take_group, COALESCE(log_excerpt, ''), exit_code
             FROM recording
             WHERE status = 'recording'
             ORDER BY started_at DESC
             LIMIT 200",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::Recording {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    status: r.get(4)?,
                    output_path: r.get(5)?,
                    went_live_at: r.get(6)?,
                    went_live_approx: r.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                    take_group: r.get(8)?,
                    log_excerpt: r.get(9)?,
                    exit_code: r.get(10)?,
                    bytes: 0,
                    lost_secs: None,
                    stream_id: None,
                    ad_count: 0,
                    ad_secs: 0,
                    meta_change_count: 0,
                    title: String::new(),
                    category: String::new(),
                    notes: String::new(),
                    vod_id: None,
                    vod_state: None,
                    vod_muted_secs: None,
                    vod_views: None,
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
                    gap_splice_state: String::new(),
                    err_ack: false,
                    sabr_live_edge_fallback: false,
                    trigger_rule_json: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All completed recordings whose `output_path` is an MKV (non-TS, non-empty).
    /// Used by batch maintenance jobs (embed thumbnails, re-organize, etc.).
    pub fn list_recordings_with_mkv(&self) -> rusqlite::Result<Vec<(i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, output_path FROM recording
             WHERE output_path != ''
               AND output_path NOT LIKE '%.ts'
               AND status NOT IN ('recording')
             ORDER BY id",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    /// Completed recordings that have a non-empty `output_path` (any extension)
    /// and a non-null `stream_id` (the YouTube/Twitch/Kick platform id).
    /// Used to find recordings we might be able to download a thumbnail for.
    /// Returns `(recording_id, output_path, stream_id)`.
    pub fn list_recordings_with_stream_id(&self) -> rusqlite::Result<Vec<(i64, String, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, output_path, stream_id FROM recording
             WHERE output_path != ''
               AND stream_id IS NOT NULL
               AND stream_id != ''
               AND status NOT IN ('recording')
             ORDER BY id",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    }

    /// All recording ids for a given monitor.
    pub fn list_recording_ids_for_monitor(&self, mid: i64) -> rusqlite::Result<Vec<i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id FROM recording WHERE monitor_id = ? AND status NOT IN ('recording') ORDER BY id",
        )?;
        stmt.query_map([mid], |r| r.get(0))?.collect()
    }

    /// All recording ids for all monitors belonging to a channel.
    pub fn list_recording_ids_for_channel(&self, channel_id: i64) -> rusqlite::Result<Vec<i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id FROM recording r
             JOIN monitor m ON r.monitor_id = m.id
             WHERE m.channel_id = ? AND r.status NOT IN ('recording')
             ORDER BY r.id",
        )?;
        stmt.query_map([channel_id], |r| r.get(0))?.collect()
    }

    /// All recording ids, regardless of monitor or channel. Used by "re-organize all".
    pub fn list_all_recording_ids(&self) -> rusqlite::Result<Vec<i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id FROM recording WHERE status NOT IN ('recording') ORDER BY id",
        )?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    /// All distinct output directories currently configured on monitors.
    /// Used by "re-organize all" to sweep companion files in directories that
    /// aren't linked to any specific recording (e.g. failed recordings with no output_path).
    pub fn list_monitor_output_dirs(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT output_dir FROM monitor WHERE output_dir != '' ORDER BY output_dir",
        )?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    /// Fetch the core fields needed for a reorganize/rename operation on one recording.
    /// Returns `(monitor_id, output_path)`.
    pub fn get_recording_paths(&self, rec_id: i64) -> rusqlite::Result<Option<(i64, String)>> {
        let conn = self.db();
        conn.query_row(
            "SELECT monitor_id, COALESCE(output_path, '') FROM recording WHERE id = ?",
            [rec_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
    }

    /// Fetch the monitor's `output_dir` and `channel_id` for context during batch ops.
    pub fn get_monitor_output_dir(&self, mid: i64) -> rusqlite::Result<Option<(String, i64)>> {
        let conn = self.db();
        conn.query_row(
            "SELECT output_dir, channel_id FROM monitor WHERE id = ?",
            [mid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
    }

    /// Read the current [`crate::models::SubdirConfig`] from app settings.
    pub fn subdir_config(&self) -> crate::models::SubdirConfig {
        let enabled = self.get_setting(crate::models::K_FILE_SPLIT_ENABLED)
            .ok().flatten().map_or(false, |v| v == "1");
        let str_or = |key: &str, default: &str| {
            self.get_setting(key).ok().flatten()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        crate::models::SubdirConfig {
            enabled,
            videos: str_or(crate::models::K_FILE_SPLIT_VIDEOS, "videos"),
            subs:   str_or(crate::models::K_FILE_SPLIT_SUBS,   "subs"),
            chat:   str_or(crate::models::K_FILE_SPLIT_CHAT,   "chat"),
            thumbs: str_or(crate::models::K_FILE_SPLIT_THUMBS, "thumbs"),
            logs:   str_or(crate::models::K_FILE_SPLIT_LOGS,   "logs"),
        }
    }

    /// Read the current [`crate::models::RemuxOpts`] from app settings.
    pub fn remux_opts(&self) -> crate::models::RemuxOpts {
        let bool_setting = |key: &str| {
            self.get_setting(key).ok().flatten().map_or(false, |v| v == "1")
        };
        let embed_thumbnail = self.get_setting(crate::models::K_REMUX_EMBED_THUMBNAIL)
            .ok().flatten()
            .map_or(true, |v| v != "0"); // default on
        let embed_title = bool_setting(crate::models::K_REMUX_EMBED_TITLE);
        let title_template = self.get_setting(crate::models::K_REMUX_TITLE_TEMPLATE)
            .ok().flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "{title}".to_string());
        let embed_subs = bool_setting(crate::models::K_REMUX_EMBED_SUBS);
        crate::models::RemuxOpts {
            embed_thumbnail,
            embed_title,
            title_template,
            embed_subs,
            title_vars: None,
        }
    }

    /// In-flight recordings (status `recording`) — crash/quit leftovers seen at
    /// startup. Excludes rows with a `detached_process` registry entry: those are
    /// owned by the detach reconcile (`reconcile_detached`), not the legacy
    /// resume/orphan path. Only the core fields needed for handling are populated.
    pub fn inflight_recordings(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, COALESCE(output_path, ''),
                    went_live_at, went_live_approx, stream_id, take_group
             FROM recording
             WHERE status = 'recording'
               AND id NOT IN (SELECT ref_id FROM detached_process WHERE kind = 'recording')
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::Recording {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    status: "recording".into(),
                    bytes: 0,
                    exit_code: None,
                    output_path: r.get(4)?,
                    went_live_at: r.get(5)?,
                    went_live_approx: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
                    lost_secs: None,
                    stream_id: r.get(7)?,
                    take_group: r.get(8)?,
                    ad_count: 0,
                    ad_secs: 0,
                    meta_change_count: 0,
                    title: String::new(),
                    category: String::new(),
                    log_excerpt: String::new(),
                    notes: String::new(),
                    vod_id: None,
                    vod_state: None,
                    vod_muted_secs: None,
                    vod_views: None,
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
                    gap_splice_state: String::new(),
                    err_ack: false,
                    sabr_live_edge_fallback: false,
                    trigger_rule_json: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct output directories across all monitors and videos — used to locate
    /// `.cache\` working dirs for the startup sweep.
    pub fn all_output_dirs(&self) -> Result<Vec<String>> {
        let conn = self.db();
        let mut stmt = conn
            .prepare("SELECT output_dir FROM monitor UNION SELECT output_dir FROM video")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().filter(|s| !s.trim().is_empty()).collect())
    }

    /// Recent recordings, newest first.
    pub fn recent_recordings(&self, limit: i64) -> Result<Vec<RecInfo>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, status, bytes, started_at, went_live_at, went_live_approx,
                    COALESCE(output_path, '')
             FROM recording ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok(RecInfo {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    status: r.get(2)?,
                    bytes: r.get(3)?,
                    started_at: r.get(4)?,
                    went_live_at: r.get(5)?,
                    went_live_approx: r.get::<_, i64>(6)? != 0,
                    output_path: r.get(7)?,
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
    fn err_ack_excludes_from_issues_but_survives_in_db() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let failed = store
            .insert_recording(mid, 1_000, "C:/rec/dead.mkv", Some(1_000), false, None, None, "", "")
            .unwrap();
        store
            .finish_recording(failed, 2_000, 0, Some(1), "failed", "C:/rec/dead.mkv", "boom")
            .unwrap();
        assert_eq!(store.recordings_with_errors().unwrap().len(), 1);

        // Acking pulls it out of the Issues list...
        store.set_recording_err_ack(failed, true).unwrap();
        assert!(store.recordings_with_errors().unwrap().is_empty());
        // ...and the instance/channel rollup, but the row (and its ack flag)
        // still exists for the take's own row to show.
        let rows = store.list_monitors_with_channels().unwrap();
        let row = rows.iter().find(|r| r.monitor.id == mid).unwrap();
        assert_eq!(row.last_recording_status.as_deref(), Some("failed"));
        assert!(row.last_recording_err_ack);

        // Un-acking restores both.
        store.set_recording_err_ack(failed, false).unwrap();
        assert_eq!(store.recordings_with_errors().unwrap().len(), 1);
        let rows = store.list_monitors_with_channels().unwrap();
        assert!(!rows.iter().find(|r| r.monitor.id == mid).unwrap().last_recording_err_ack);
    }

    #[test]
    fn sabr_live_edge_fallback_defaults_off_and_sticks_once_set() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let rec = store
            .insert_recording(mid, 1_000, "C:/rec/live-edge.mkv", Some(1_000), false, Some("v1"), None, "", "")
            .unwrap();
        // Never set for a normal, from-start-successful take.
        assert!(!store.get_recording(rec).unwrap().unwrap().sabr_live_edge_fallback);

        store.set_sabr_live_edge_fallback(rec).unwrap();
        assert!(store.get_recording(rec).unwrap().unwrap().sabr_live_edge_fallback);
        // Both other listing paths (recordings_for_monitor's own duplicated
        // column list, and the row still being "the latest" for the monitor)
        // must agree with get_recording's RECORDING_FULL_COLUMNS path.
        assert!(store.recordings_for_monitor(mid).unwrap()[0].sabr_live_edge_fallback);

        // Finishing the take (the real lifecycle) doesn't clear it — it's a
        // fact about how the take was captured, not a live/transient state.
        store.finish_recording(rec, 2_000, 500, Some(0), "completed", "C:/rec/live-edge.mkv", "").unwrap();
        assert!(store.get_recording(rec).unwrap().unwrap().sabr_live_edge_fallback);
    }

    #[test]
    fn stuck_in_cache_detection_and_sweep_protection() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // A completed SABR/DASH capture whose promote move failed: a non-.ts
        // file still sitting in .cache\.
        let stuck = store
            .insert_recording(mid, 1_000, "C:/rec/.cache/stuck.mkv", Some(1_000), false, None, None, "", "")
            .unwrap();
        store.finish_recording(stuck, 2_000, 500, Some(0), "completed", "C:/rec/.cache/stuck.mkv", "").unwrap();

        // A .ts-in-cache failure is a DIFFERENT, pre-existing category
        // (needs re-remux) and must NOT double-count here.
        let ts_stuck = store
            .insert_recording(mid, 1_000, "C:/rec/.cache/tsstuck.ts", Some(1_000), false, None, None, "", "")
            .unwrap();
        store.finish_recording(ts_stuck, 2_000, 500, Some(0), "completed", "C:/rec/.cache/tsstuck.ts", "").unwrap();

        // A normal, successfully-promoted recording (not in .cache at all).
        let ok = store
            .insert_recording(mid, 3_000, "C:/rec/fine.mkv", Some(3_000), false, None, None, "", "")
            .unwrap();
        store.finish_recording(ok, 4_000, 500, Some(0), "completed", "C:/rec/fine.mkv", "").unwrap();

        // A capture that's in .cache because it's still ACTIVELY recording —
        // must not be treated as "stuck" (status isn't 'completed').
        let active = store
            .insert_recording(mid, 5_000, "C:/rec/.cache/active.mkv", Some(5_000), false, None, None, "", "")
            .unwrap();
        let _ = active;

        let stuck_recs = store.recordings_stuck_in_cache().unwrap();
        assert_eq!(stuck_recs.len(), 1, "only the non-.ts completed .cache recording counts");
        assert_eq!(stuck_recs[0].id, stuck);

        // stems_in_cache protects EVERY .cache-pointing recording from the
        // sweep, regardless of status — the .ts-in-cache and still-recording
        // rows must be covered too, since deleting either would also be a
        // real loss (the ts awaits a manual re-remux; the active one is mid-capture).
        let stems = store.stems_in_cache().unwrap();
        assert!(stems.contains(&"stuck".to_string()));
        assert!(stems.contains(&"tsstuck".to_string()));
        assert!(stems.contains(&"active".to_string()));
        assert!(!stems.contains(&"fine".to_string()));
    }
    #[test]
    fn orphan_repair_candidates_and_promotion() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let rec = |path: &str, start: i64| {
            store
                .insert_recording(mid, start, path, Some(start), false, Some("s1"), None, "", "")
                .unwrap()
        };

        // Fresh crash orphan pointing at a final-shaped path → candidate.
        let orphan = rec("C:/rec/orphan.mkv", 1_000);
        store.mark_recording_orphaned(orphan).unwrap();
        // A row a blind promotion already flipped: completed, bytes=0 → candidate.
        let damaged = rec("C:/rec/damaged.mkv", 2_000);
        store.finish_recording(damaged, 3_000, 0, None, "completed", "C:/rec/damaged.mkv", "").unwrap();
        // Healthy completed row (bytes > 0) → not a candidate.
        let healthy = rec("C:/rec/healthy.mkv", 4_000);
        store.finish_recording(healthy, 5_000, 500, Some(0), "completed", "C:/rec/healthy.mkv", "").unwrap();
        // Already retargeted into .cache (ts awaiting re-remux) → not a candidate.
        let ts = rec("C:/rec/.cache/kept.ts", 6_000);
        store.mark_recording_orphaned(ts).unwrap();
        // Still recording → never touched.
        let _active = rec("C:/rec/active.mkv", 7_000);

        let ids: Vec<i64> = store
            .orphan_repair_candidates()
            .unwrap()
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert!(ids.contains(&orphan));
        assert!(ids.contains(&damaged));
        assert!(!ids.contains(&healthy));
        assert!(!ids.contains(&ts));
        assert_eq!(ids.len(), 2);

        // Promotion records the verified size and removes the row from the pool.
        store.promote_orphan_completed(orphan, 12_345).unwrap();
        let r = store.get_recording(orphan).unwrap().unwrap();
        assert_eq!(r.status, "completed");
        assert_eq!(r.bytes, 12_345);
        let ids: Vec<i64> = store
            .orphan_repair_candidates()
            .unwrap()
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(ids, vec![damaged]);
    }
    #[test]
    fn path_prefix_relocation_counts_and_rewrites() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        store.set_monitor_output_dir(mid, r"A:\streams\Chan").unwrap();

        let moved = store
            .insert_recording(mid, 1_000, r"A:\streams\Chan\a.mkv", Some(1_000), false, None, None, "", "")
            .unwrap();
        store
            .finish_recording(moved, 2_000, 5, Some(0), "completed", r"A:\streams\Chan\a.mkv", "")
            .unwrap();
        let stays = store
            .insert_recording(mid, 3_000, r"G:\other\Chan\b.mkv", Some(3_000), false, None, None, "", "")
            .unwrap();
        store
            .finish_recording(stays, 4_000, 5, Some(0), "completed", r"G:\other\Chan\b.mkv", "")
            .unwrap();

        let (r, v, mons) = store.count_path_prefix_matches(r"A:\streams").unwrap();
        assert_eq!((r, v, mons), (1, 0, 1));

        let (r, v, mons) = store.replace_path_prefix(r"A:\streams", r"D:\streams", true).unwrap();
        assert_eq!((r, v, mons), (1, 0, 1));
        assert_eq!(
            store.get_recording(moved).unwrap().unwrap().output_path,
            r"D:\streams\Chan\a.mkv"
        );
        assert_eq!(
            store.get_recording(stays).unwrap().unwrap().output_path,
            r"G:\other\Chan\b.mkv"
        );
        assert_eq!(
            store.get_monitor_with_channel(mid).unwrap().unwrap().monitor.output_dir,
            r"D:\streams\Chan"
        );
        // Nothing left matching the old prefix.
        assert_eq!(store.count_path_prefix_matches(r"A:\streams").unwrap(), (0, 0, 0));
    }
    #[test]
    fn head_backfill_queued_listing() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let r1 = store
            .insert_recording(mid, 1_000, "C:/rec/a.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        let r2 = store
            .insert_recording(mid, 2_000, "C:/rec/b.mkv", Some(2_000), false, Some("s2"), None, "", "")
            .unwrap();

        store.set_head_backfill_state(r1, "queued").unwrap();
        store.set_head_backfill_state(r2, "mismatch").unwrap();
        assert_eq!(store.recordings_head_backfill_queued().unwrap(), vec![r1]);

        // Clearing the flag (what the job does at every exit) empties the pool.
        store.set_head_backfill_state(r1, "").unwrap();
        assert!(store.recordings_head_backfill_queued().unwrap().is_empty());
    }
    #[test]
    fn pending_head_concat_needs_ended_take_without_full() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        // Still recording → not pending even with a head present.
        store.set_recording_backfill_path(rid, "C:/tmp/a.head.mkv").unwrap();
        assert!(store.recordings_pending_head_concat().unwrap().is_empty());
        // Finished → pending.
        store.finish_recording(rid, 200, 1, Some(0), "completed", "C:/tmp/a.mkv", "").unwrap();
        assert_eq!(store.recordings_pending_head_concat().unwrap(), vec![rid]);
        // Joined → no longer pending.
        store.set_recording_full_path(rid, "C:/tmp/a.full.mkv").unwrap();
        assert!(store.recordings_pending_head_concat().unwrap().is_empty());
    }
    #[test]
    fn first_take_for_stream_ignores_other_streams_and_later_takes() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        // The first take of s1 owns the head; a retake (later start) does not.
        assert!(store.is_first_take_for_stream(mid, "s1", 100).unwrap());
        assert!(!store.is_first_take_for_stream(mid, "s1", 200).unwrap());
        // A different stream id is unaffected by s1's takes.
        assert!(store.is_first_take_for_stream(mid, "s2", 200).unwrap());
    }
    #[test]
    fn first_take_for_stream_skips_prior_instant_failures() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let dead = store
            .insert_recording(mid, 100, "C:/tmp/a.ts", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        // Died instantly with nothing captured (e.g. the MAX_PATH bug) — a
        // later retake should still be able to own the missed HEAD.
        store.finish_recording(dead, 105, 0, Some(1), "failed", "C:/tmp/a.ts", "boom").unwrap();
        assert!(
            store.is_first_take_for_stream(mid, "s1", 400).unwrap(),
            "a retake after an instant 0-byte failure should own the head backfill"
        );

        // But a prior take that actually captured something (even if it later
        // failed) still owns it — there's real head footage to not duplicate.
        let partial = store
            .insert_recording(mid, 500, "C:/tmp/b.ts", Some(50), false, Some("s2"), None, "", "")
            .unwrap();
        store.finish_recording(partial, 600, 12345, Some(1), "failed", "C:/tmp/b.ts", "boom").unwrap();
        assert!(!store.is_first_take_for_stream(mid, "s2", 900).unwrap());

        // And a prior take still actively recording (no bytes yet, but not
        // finished) still blocks — it may yet succeed.
        store
            .insert_recording(mid, 1000, "C:/tmp/c.ts", Some(50), false, Some("s3"), None, "", "")
            .unwrap();
        assert!(!store.is_first_take_for_stream(mid, "s3", 1100).unwrap());
    }
    #[test]
    fn recordings_with_backfill_for_stream_finds_and_clears_old_heads() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();

        let take1 = store
            .insert_recording(mid, 100, "C:/tmp/take1.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_backfill_path(take1, "C:/tmp/take1.head.mkv").unwrap();
        store.finish_recording(take1, 200, 500, Some(0), "completed", "C:/tmp/take1.mkv", "").unwrap();

        // A take with no backfill_path at all shouldn't show up.
        let take2 = store
            .insert_recording(mid, 300, "C:/tmp/take2.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();

        // A take of a DIFFERENT stream shouldn't show up either.
        let other_stream = store
            .insert_recording(mid, 50, "C:/tmp/other.mkv", Some(10), false, Some("s2"), None, "", "")
            .unwrap();
        store.set_recording_backfill_path(other_stream, "C:/tmp/other.head.mkv").unwrap();

        let take3 = store
            .insert_recording(mid, 600, "C:/tmp/take3.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_backfill_path(take3, "C:/tmp/take3.head.mkv").unwrap();

        // From take3's perspective, only take1's head is an "other" backfill
        // for the same stream (take2 has none, other_stream is a different
        // stream, take3 excludes itself).
        let others = store.recordings_with_backfill_for_stream(mid, "s1", take3).unwrap();
        assert_eq!(others, vec![(take1, "C:/tmp/take1.head.mkv".to_string())]);
        assert!(!others.iter().any(|(id, _)| *id == take2));

        // Clearing take1's backfill_path drops it out of both queries.
        store.clear_recording_backfill_path(take1).unwrap();
        assert!(store.recordings_with_backfill_for_stream(mid, "s1", take3).unwrap().is_empty());
        assert!(!store.recordings_pending_head_concat().unwrap().contains(&take1));
    }
    #[test]
    fn get_recording_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();

        assert!(store.get_recording(999).unwrap().is_none());

        let take1 = store
            .insert_recording(mid, 100, "C:/tmp/take1.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();

        let r1 = store.get_recording(take1).unwrap().unwrap();
        assert_eq!(r1.id, take1);
        assert_eq!(r1.output_path, "C:/tmp/take1.mkv");
        assert_eq!(r1.stream_id.as_deref(), Some("s1"));
    }
    #[test]
    fn head_backfill_state_roundtrip_and_queued_query() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();

        // Fresh recording: not queued.
        assert!(store.queued_head_backfills().unwrap().is_empty());

        store.set_head_backfill_state(rid, "queued").unwrap();
        let rec = store.get_recording(rid).unwrap().unwrap();
        assert_eq!(rec.head_backfill_state, "queued");
        let planned = store.queued_head_backfills().unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].channel, "A");
        assert_eq!(planned[0].started_at, 100);

        // Clearing drops it out of the queued query but leaves everything else.
        store.set_head_backfill_state(rid, "").unwrap();
        assert!(store.queued_head_backfills().unwrap().is_empty());
        assert_eq!(store.get_recording(rid).unwrap().unwrap().head_backfill_state, "");
    }

    #[test]
    fn capture_start_pts_first_writer_wins() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();

        // Never probed → None.
        assert_eq!(store.recording_capture_start_pts(rid).unwrap(), None);

        // First probe sticks…
        store.set_recording_capture_start_pts(rid, 371.433).unwrap();
        assert_eq!(store.recording_capture_start_pts(rid).unwrap(), Some(371.433));

        // …and a later (post-remux, PTS-reset) write can't clobber it.
        store.set_recording_capture_start_pts(rid, 0.0).unwrap();
        assert_eq!(store.recording_capture_start_pts(rid).unwrap(), Some(371.433));
    }
}
