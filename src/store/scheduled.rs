//! Scheduled recordings and schedule sources/segments/filename presets.

use super::*;

impl Store {
    // ----- scheduled recordings (force-start at a time, schema v51) -----

    fn map_scheduled_recording(r: &rusqlite::Row) -> rusqlite::Result<ScheduledRecording> {
        Ok(ScheduledRecording {
            id: r.get(0)?,
            monitor_id: r.get(1)?,
            label: r.get(2)?,
            kind: RecurrenceKind::parse(&r.get::<_, String>(3)?),
            start_at: r.get(4)?,
            days_of_week: r.get(5)?,
            time_of_day_secs: r.get(6)?,
            until: r.get(7)?,
            duration_secs: r.get(8)?,
            enabled: r.get::<_, i64>(9)? != 0,
            next_run_at: r.get(10)?,
            last_fired_at: r.get(11)?,
            pending_stop_at: r.get(12)?,
            created_at: r.get(13)?,
        })
    }

    const SCHEDULED_RECORDING_SELECT: &str =
        "SELECT id, monitor_id, label, kind, start_at, days_of_week, time_of_day_secs, until, \
         duration_secs, enabled, next_run_at, last_fired_at, pending_stop_at, created_at \
         FROM scheduled_recording";

    #[allow(clippy::too_many_arguments)]
    pub fn insert_scheduled_recording(
        &self,
        monitor_id: i64,
        label: &str,
        kind: RecurrenceKind,
        start_at: Option<i64>,
        days_of_week: Option<i64>,
        time_of_day_secs: Option<i64>,
        until: Option<i64>,
        duration_secs: Option<i64>,
        next_run_at: Option<i64>,
    ) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO scheduled_recording(
                monitor_id, label, kind, start_at, days_of_week, time_of_day_secs, until,
                duration_secs, enabled, next_run_at, created_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10)",
            params![
                monitor_id,
                label,
                kind.as_str(),
                start_at,
                days_of_week,
                time_of_day_secs,
                until,
                duration_secs,
                next_run_at,
                now_unix(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Updates the user-editable fields only — `last_fired_at`/`pending_stop_at`
    /// are job bookkeeping and untouched by an edit.
    #[allow(clippy::too_many_arguments)]
    pub fn update_scheduled_recording(
        &self,
        id: i64,
        label: &str,
        kind: RecurrenceKind,
        start_at: Option<i64>,
        days_of_week: Option<i64>,
        time_of_day_secs: Option<i64>,
        until: Option<i64>,
        duration_secs: Option<i64>,
        enabled: bool,
        next_run_at: Option<i64>,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE scheduled_recording SET
                label=?2, kind=?3, start_at=?4, days_of_week=?5, time_of_day_secs=?6, until=?7,
                duration_secs=?8, enabled=?9, next_run_at=?10
             WHERE id=?1",
            params![
                id,
                label,
                kind.as_str(),
                start_at,
                days_of_week,
                time_of_day_secs,
                until,
                duration_secs,
                enabled as i64,
                next_run_at,
            ],
        )?;
        Ok(())
    }

    pub fn delete_scheduled_recording(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("DELETE FROM scheduled_recording WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Every scheduled recording, joined with its channel/monitor for the
    /// management window — soonest-due enabled rules first, fired/disabled
    /// ones last.
    pub fn list_scheduled_recordings(&self) -> Result<Vec<ScheduledRecordingWithNames>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT sr.id, sr.monitor_id, sr.label, sr.kind, sr.start_at, sr.days_of_week, \
             sr.time_of_day_secs, sr.until, sr.duration_secs, sr.enabled, sr.next_run_at, \
             sr.last_fired_at, sr.pending_stop_at, sr.created_at, c.name, m.url
             FROM scheduled_recording sr
             JOIN monitor m ON m.id = sr.monitor_id
             JOIN channel c ON c.id = m.channel_id
             ORDER BY sr.enabled DESC, sr.next_run_at IS NULL, sr.next_run_at ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ScheduledRecordingWithNames {
                    rec: Self::map_scheduled_recording(r)?,
                    channel_name: r.get(14)?,
                    monitor_url: r.get(15)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Rules due to force-start right now (the background job's tick query).
    pub fn due_scheduled_recordings(&self, now: i64) -> Result<Vec<ScheduledRecording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "{} WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?1",
            Self::SCHEDULED_RECORDING_SELECT
        ))?;
        let rows = stmt
            .query_map(params![now], Self::map_scheduled_recording)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(id, monitor_id)` of duration-bound occurrences whose auto-stop is due.
    pub fn due_scheduled_stops(&self, now: i64) -> Result<Vec<(i64, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id FROM scheduled_recording
             WHERE pending_stop_at IS NOT NULL AND pending_stop_at <= ?1",
        )?;
        let rows = stmt
            .query_map(params![now], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(i64, i64)>>>()?;
        Ok(rows)
    }

    /// Records that a rule just fired: stamps the occurrence, advances (or
    /// clears) `next_run_at`, and arms `pending_stop_at` when the rule has a
    /// duration. A `None` `next_run_at` (a `Once` rule, or a `Weekly` rule past
    /// its `until`) soft-disables the rule so it stops showing as upcoming but
    /// stays listed for the user to review/delete.
    pub fn mark_scheduled_recording_fired(
        &self,
        id: i64,
        occurrence_start: i64,
        next_run_at: Option<i64>,
        pending_stop_at: Option<i64>,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE scheduled_recording SET
                last_fired_at=?2, next_run_at=?3, pending_stop_at=?4,
                enabled = CASE WHEN ?3 IS NULL THEN 0 ELSE enabled END
             WHERE id=?1",
            params![id, occurrence_start, next_run_at, pending_stop_at],
        )?;
        Ok(())
    }

    pub fn clear_scheduled_recording_stop(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE scheduled_recording SET pending_stop_at=NULL WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    /// Flips `enabled` (management-window checkbox / row action) and installs
    /// the caller's freshly recomputed `next_run_at` (`None` when disabling).
    pub fn set_scheduled_recording_enabled(
        &self,
        id: i64,
        enabled: bool,
        next_run_at: Option<i64>,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE scheduled_recording SET enabled=?2, next_run_at=?3 WHERE id=?1",
            params![id, enabled as i64, next_run_at],
        )?;
        Ok(())
    }
    // ----- schedule (upcoming streams) -----

    /// Replace one monitor's schedule for a single `source` (`"platform"` or
    /// `"discord"`), leaving its other sources intact (delete-then-insert in one
    /// transaction). Lets the platform schedule and Discord events coexist without
    /// clobbering each other.
    pub fn replace_schedule_source(
        &self,
        monitor_id: i64,
        source: &str,
        segs: &[ScheduleSegment],
    ) -> Result<()> {
        let now = now_unix();
        // Phase 1 — short lock: read suppressed start instants so the lock is
        // released before the heavier write transaction below.  Other threads
        // (save, reload-rows) can acquire the DB between the two phases.
        //
        // Start instants an automatic source must NOT re-create for this monitor:
        //  • those claimed by a protected manual row (a user's correction — don't
        //    duplicate it at the same instant), and
        //  • those a user explicitly removed or moved away from (tombstones,
        //    canceled = 1 — re-inserting would resurrect a deleted occurrence, or
        //    re-add the pre-correction copy of a rescheduled one).
        // Storing the manual source itself doesn't self-suppress on manual rows,
        // but still honours tombstones.
        let suppressed_starts: std::collections::HashSet<i64> = {
            let conn = self.db();
            let mut stmt = conn.prepare(
                "SELECT start_time FROM schedule_segment
                 WHERE monitor_id = ?1
                   AND ((source = 'manual' AND ?2 <> 'manual') OR canceled = 1)",
            )?;
            stmt.query_map(params![monitor_id, source], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?
        };
        // DB lock released — other threads get a turn between phases.

        // Pre-filter in memory: only the segments we'll actually write.
        let segs_to_write: Vec<&ScheduleSegment> = segs
            .iter()
            .filter(|s| !suppressed_starts.contains(&s.start_time))
            .collect();

        // Phase 2 — write transaction: all SQL is writes-only, keeping this
        // lock hold as short as possible.
        let mut conn = self.db();
        let tx = conn.transaction()?;
        // Drop stale tombstones (canceled rows well past their instant): a
        // tombstone's job is to suppress re-insertion of a deleted event on the
        // next refresh. Because we only ever re-insert UPCOMING events (the delete
        // below is future-only), a tombstone becomes inert once its start_time is
        // comfortably in the past. Keep a 7-day grace so a deletion stays visible
        // while the user can still see that week in the calendar.
        tx.execute(
            "DELETE FROM schedule_segment
             WHERE monitor_id = ?1 AND canceled = 1 AND start_time < ?2",
            params![monitor_id, now - 7 * 86_400],
        )?;
        // Replace only this source's FUTURE non-canceled rows. Past events are
        // intentionally preserved as a schedule archive — the next banner fetch only
        // re-emits upcoming streams, so old rows accumulate as history. Tombstones
        // (canceled = 1) are also preserved so a user's "Delete" keeps suppressing
        // a recurring event on future re-fetches.
        tx.execute(
            "DELETE FROM schedule_segment
             WHERE monitor_id = ?1 AND source = ?2 AND canceled = 0
               AND start_time >= ?3",
            params![monitor_id, source, now],
        )?;
        // For each segment the winning source is about to write, evict any other
        // automatic source's live row at that SAME instant — both past and future.
        // This prevents cross-source duplicates from accumulating in the archive
        // when two sources (e.g. Twitch schedule + OCR banner) previously both
        // stored the same event (possibly with different title casing). The winning
        // source's version is the only one that survives per (monitor, instant).
        {
            let mut evict = tx.prepare(
                "DELETE FROM schedule_segment
                 WHERE monitor_id = ?1 AND source <> ?2 AND source <> 'manual'
                   AND start_time = ?3 AND canceled = 0",
            )?;
            for s in &segs_to_write {
                evict.execute(params![monitor_id, source, s.start_time])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO schedule_segment(monitor_id, start_time, end_time, title, category, canceled, source, video_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for s in &segs_to_write {
                stmt.execute(params![
                    monitor_id,
                    s.start_time,
                    s.end_time,
                    s.title,
                    s.category,
                    s.canceled as i64,
                    source,
                    s.video_id.as_deref(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Like [`Self::replace_schedule_source`], but also returns what changed among
    /// the monitor's UPCOMING (future) segments for this source, so the caller can
    /// emit `schedule_added` / `schedule_updated` notifications. Compares the
    /// future, non-canceled segments (keyed by `start_time`) before and after the
    /// replace. Pure title-fill (blank → real title, category unchanged) is
    /// suppressed as noise. Time-moves surface as an "added" at the new instant
    /// (the old instant silently drops).
    pub fn replace_schedule_source_diffed(
        &self,
        monitor_id: i64,
        source: &str,
        segs: &[ScheduleSegment],
    ) -> Result<Vec<ScheduleChange>> {
        let now = now_unix();
        let snapshot = |store: &Store| -> std::collections::HashMap<i64, (String, String)> {
            store
                .schedule_segments_for_source(monitor_id, source)
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.start_time > now) // only future occurrences are notify-worthy
                .map(|s| (s.start_time, (s.title, s.category)))
                .collect()
        };
        let before = snapshot(self);
        self.replace_schedule_source(monitor_id, source, segs)?;
        let after = snapshot(self);

        let mut changes = Vec::new();
        for (start, (title, category)) in &after {
            match before.get(start) {
                None => changes.push(ScheduleChange {
                    added: true,
                    start_time: *start,
                    title: title.clone(),
                    category: category.clone(),
                }),
                Some((old_title, old_cat)) => {
                    if old_title == title && old_cat == category {
                        continue; // unchanged
                    }
                    // Suppress pure title-fill (a blank title just got filled in,
                    // category unchanged) — not a user-meaningful "update".
                    if old_title.is_empty() && old_cat == category {
                        continue;
                    }
                    changes.push(ScheduleChange {
                        added: false,
                        start_time: *start,
                        title: title.clone(),
                        category: category.clone(),
                    });
                }
            }
        }
        Ok(changes)
    }

    /// Delete a monitor's UPCOMING (future) schedule segments from every source
    /// EXCEPT `keep` (and EXCEPT protected `"manual"` rows). Past events are
    /// intentionally left untouched as a schedule archive. The ordered-source
    /// refresh calls this after a source resolves a schedule, so exactly one
    /// automatic source's future rows remain per monitor — a lower-priority source
    /// can't leave stale upcoming rows behind while historical rows accumulate.
    /// User-edited `"manual"` rows and tombstones (`canceled = 1`) always survive.
    pub fn clear_other_schedule_sources(&self, monitor_id: i64, keep: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM schedule_segment
             WHERE monitor_id = ?1 AND source <> ?2 AND source <> 'manual'
               AND canceled = 0 AND start_time >= ?3",
            params![monitor_id, keep, now_unix()],
        )?;
        Ok(())
    }

    /// Convert one schedule segment into a protected manual entry, overwriting its
    /// time/title/category. The row's `source` becomes `"manual"` and `canceled`
    /// is cleared, so subsequent automatic refreshes of other sources leave it
    /// intact (see [`Self::clear_other_schedule_sources`]). Backs the calendar's
    /// "Edit…" dialog. Returns the number of rows updated — `0` means the segment
    /// no longer exists (e.g. a background refresh cleared it mid-edit), so the
    /// caller can report the edit didn't apply instead of falsely claiming success.
    ///
    /// When the edit MOVES an automatic event to a new instant, a tombstone
    /// (`canceled = 1`) is left at the original instant so the next refresh of
    /// that source doesn't re-create the stream at its old, uncorrected time
    /// alongside the correction (see [`Self::replace_schedule_source`]).
    pub fn update_schedule_segment_manual(
        &self,
        id: i64,
        start_time: i64,
        end_time: Option<i64>,
        title: &str,
        category: &str,
    ) -> Result<usize> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        // Capture the pre-edit state so we know whether a time-move tombstone is
        // needed and what source to attribute it to.
        let orig: Option<(i64, i64, String)> = tx
            .query_row(
                "SELECT monitor_id, start_time, source FROM schedule_segment WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()?;
        let Some((monitor_id, orig_start, orig_source)) = orig else {
            return Ok(0);
        };
        let n = tx.execute(
            "UPDATE schedule_segment
                SET start_time = ?2, end_time = ?3, title = ?4, category = ?5,
                    source = 'manual', canceled = 0
              WHERE id = ?1",
            params![id, start_time, end_time, title, category],
        )?;
        if n > 0 && orig_source != "manual" && orig_start != start_time {
            tx.execute(
                "INSERT INTO schedule_segment
                     (monitor_id, start_time, end_time, title, category, canceled, source, video_id)
                 VALUES (?1, ?2, NULL, '', '', 1, ?3, NULL)",
                params![monitor_id, orig_start, orig_source],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Remove one schedule segment from the calendar (the "Delete" action). For
    /// automatic sources a hard delete wouldn't stick — the next refresh re-emits
    /// the same occurrence — so this tombstones the row (`canceled = 1`) instead;
    /// every read filters `canceled = 0`, and [`Self::replace_schedule_source`]
    /// treats the tombstoned instant as suppressed, so the occurrence stays gone.
    /// Returns the number of rows affected (`0` if it was already gone).
    pub fn delete_schedule_segment(&self, id: i64) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE schedule_segment SET canceled = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(n)
    }

    /// Manually merge secondary segments into a primary. Sets `merged_into = primary_id`
    /// on each secondary. Does not modify the primary row itself.
    pub fn merge_segments_manual(&self, primary_id: i64, secondary_ids: &[i64]) -> Result<()> {
        let conn = self.db();
        for &sid in secondary_ids {
            conn.execute(
                "UPDATE schedule_segment SET merged_into = ?1 WHERE id = ?2",
                params![primary_id, sid],
            )?;
        }
        Ok(())
    }

    /// Undo a manual merge: clears `merged_into` from all segments merged into
    /// `primary_id`. Also clears `merged_into` on `primary_id` itself in case it
    /// was itself a secondary of a higher-level merge.
    pub fn unmerge_segment(&self, primary_id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE schedule_segment SET merged_into = NULL WHERE merged_into = ?1",
            params![primary_id],
        )?;
        conn.execute(
            "UPDATE schedule_segment SET merged_into = NULL WHERE id = ?1",
            params![primary_id],
        )?;
        Ok(())
    }

    /// Opt a segment into (`excluded = false`) or out of (`excluded = true`) automatic
    /// time-overlap merge grouping with same-channel events.
    pub fn set_auto_merge_excluded(&self, segment_id: i64, excluded: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE schedule_segment SET auto_merge_excluded = ?1 WHERE id = ?2",
            params![excluded as i64, segment_id],
        )?;
        Ok(())
    }

    // ----- user-defined filename presets -----

    pub fn get_filename_presets(&self) -> Result<Vec<(i64, String, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, name, template FROM filename_preset ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn save_filename_preset(&self, name: &str, template: &str) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO filename_preset (name, template) VALUES (?1, ?2)",
            params![name, template],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn delete_filename_preset(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("DELETE FROM filename_preset WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Bulk-set every monitor's `filename_template` column. Returns the number of rows updated.
    pub fn set_all_filename_templates(&self, template: &str) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE monitor SET filename_template = ?1",
            params![template],
        )?;
        Ok(n)
    }

    /// Delete every schedule segment from one `source` across all monitors. Used to
    /// purge imported Discord events when that import is turned off.
    pub fn clear_schedule_source(&self, source: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM schedule_segment WHERE source = ?1",
            params![source],
        )?;
        Ok(())
    }

    /// Monitor ids with at least one upcoming (non-canceled, start ≥ `after`)
    /// schedule segment from any source OTHER than `discord`. The Discord sweep
    /// uses this so it never attaches an event to a channel that already resolved a
    /// schedule from a higher-priority source (the two would otherwise duplicate) —
    /// the winning source may be `platform`, `youtube`, an OCR source, etc.
    pub fn monitors_with_upcoming_non_discord(
        &self,
        after: i64,
    ) -> Result<std::collections::HashSet<i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT monitor_id FROM schedule_segment
             WHERE source <> 'discord' AND canceled = 0 AND start_time >= ?1",
        )?;
        let ids = stmt
            .query_map(params![after], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        Ok(ids)
    }

    /// Upcoming (non-canceled, start ≥ `after`) schedule segments for one monitor,
    /// soonest first — for the Next stream popup.
    pub fn schedule_for_monitor(&self, monitor_id: i64, after: i64) -> Result<Vec<ScheduleSegment>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, start_time, end_time, title, category, canceled, video_id
             FROM schedule_segment
             WHERE monitor_id = ?1 AND canceled = 0 AND start_time >= ?2
             ORDER BY start_time",
        )?;
        let rows = stmt
            .query_map(params![monitor_id, after], |r| {
                Ok(ScheduleSegment {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    start_time: r.get(2)?,
                    end_time: r.get(3)?,
                    title: r.get(4)?,
                    category: r.get(5)?,
                    canceled: r.get::<_, i64>(6)? != 0,
                    video_id: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All non-canceled schedule segments for one `(monitor, source)` pair, sorted
    /// Non-canceled segments for one `(monitor, source)`, ordered by start
    /// time. Limited to rows within the past 24 hours and forward — past-only
    /// history rows accumulate indefinitely and make a full-table scan expensive.
    /// Used by the OCR hash cache to reconstruct in-memory segment lists from the
    /// DB after an app restart so OCR is not re-run on unchanged images.
    pub fn schedule_segments_for_source(
        &self,
        monitor_id: i64,
        source: &str,
    ) -> Result<Vec<ScheduleSegment>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, start_time, end_time, title, category, canceled, video_id
             FROM schedule_segment
             WHERE monitor_id = ?1 AND source = ?2 AND canceled = 0
               AND start_time >= ?3
             ORDER BY start_time",
        )?;
        let rows = stmt
            .query_map(params![monitor_id, source, now_unix() - 86_400], |r| {
                Ok(ScheduleSegment {
                    id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    start_time: r.get(2)?,
                    end_time: r.get(3)?,
                    title: r.get(4)?,
                    category: r.get(5)?,
                    canceled: r.get::<_, i64>(6)? != 0,
                    video_id: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// YouTube monitors that have at least one `platform` schedule segment with a
    /// NULL `video_id`. Used by the "Re-fetch missing video IDs" button to target
    /// only the channels that need a scrape pass, not every YouTube monitor.
    pub fn youtube_monitors_missing_video_ids(&self) -> Result<Vec<(i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT m.id, m.url
             FROM monitor m
             JOIN schedule_segment ss ON ss.monitor_id = m.id
             WHERE (m.url LIKE '%youtube.com%' OR m.url LIKE '%youtu.be%')
               AND ss.source = 'platform'
               AND ss.video_id IS NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The soonest upcoming (non-canceled, start ≥ `after`) stream per monitor:
    /// `(monitor_id, start_time, title)`. Drives the Next stream column. (SQLite
    /// returns the bare `title` from the same row as `MIN(start_time)`.)
    pub fn next_scheduled_streams(&self, after: i64) -> Result<Vec<(i64, i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT monitor_id, MIN(start_time), title
             FROM schedule_segment
             WHERE canceled = 0 AND start_time >= ?1
             GROUP BY monitor_id",
        )?;
        let rows = stmt
            .query_map(params![after], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every upcoming (non-canceled, start ≥ `after`) scheduled stream across all
    /// monitors, joined with channel name + the monitor's source URL, soonest
    /// first. Drives the Schedule calendar (one row per occurrence, unlike
    /// [`Self::next_scheduled_streams`] which collapses to the soonest per monitor).
    pub fn all_upcoming_schedule(&self, after: i64) -> Result<Vec<UpcomingStream>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.monitor_id, m.channel_id, c.name, m.url,
                    s.start_time, s.end_time, s.title, s.category, s.source,
                    c.color, s.merged_into, s.auto_merge_excluded
             FROM schedule_segment s
             JOIN monitor m ON m.id = s.monitor_id
             JOIN channel c ON c.id = m.channel_id
             WHERE s.canceled = 0 AND s.start_time >= ?1
             ORDER BY s.start_time, c.name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map(params![after], |r| {
                Ok(UpcomingStream {
                    segment_id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    channel_id: r.get(2)?,
                    channel_name: r.get(3)?,
                    url: r.get(4)?,
                    start_time: r.get(5)?,
                    end_time: r.get(6)?,
                    title: r.get(7)?,
                    category: r.get(8)?,
                    source: r.get(9)?,
                    channel_color: r.get(10)?,
                    merged_into: r.get(11)?,
                    auto_merge_excluded: r.get::<_, i64>(12)? != 0,
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
    fn schedule_replace_and_next() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let now = now_unix();
        let seg = |start: i64, title: &str, canceled: bool| ScheduleSegment {
            id: 0,
            monitor_id: 0,
            start_time: start,
            end_time: Some(start + 3600),
            title: title.into(),
            category: String::new(),
            canceled,
            video_id: None,
        };
        // Out of order; a past one, a canceled one, and two future.
        store
            .replace_schedule_source(
                mid,
                "platform",
                &[
                    seg(now + 5_000, "Later", false),
                    seg(500, "Past", false),
                    seg(now + 3_000, "Canceled soon", true),
                    seg(now + 2_000, "Next up", false),
                ],
            )
            .unwrap();

        // Upcoming (start >= after), non-canceled, soonest first.
        let upcoming = store.schedule_for_monitor(mid, now + 1_000).unwrap();
        assert_eq!(
            upcoming.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            vec!["Next up", "Later"],
        );
        // The soonest future per monitor drives the Next stream column.
        let next = store.next_scheduled_streams(now + 1_000).unwrap();
        assert_eq!(next, vec![(mid, now + 2_000, "Next up".to_string())]);

        // Replace wipes the old future set and leaves the past row archived.
        store
            .replace_schedule_source(mid, "platform", &[seg(now + 9_000, "Fresh", false)])
            .unwrap();
        let next = store.next_scheduled_streams(now + 1_000).unwrap();
        assert_eq!(next, vec![(mid, now + 9_000, "Fresh".to_string())]);

        // Deleting the monitor cascades to its schedule.
        store.delete_monitor(mid).unwrap();
        assert!(store.schedule_for_monitor(mid, 0).unwrap().is_empty());
    }
    #[test]
    fn all_upcoming_schedule_joins_channel() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m1 = sample_monitor(cid);
        m1.channel_id = cid;
        m1.url = "https://twitch.tv/streamer".into();
        let mid1 = store.insert_monitor(&m1).unwrap();
        let mut m2 = sample_monitor(cid);
        m2.channel_id = cid;
        m2.url = "https://youtube.com/@streamer".into();
        let mid2 = store.insert_monitor(&m2).unwrap();

        let now = now_unix();
        let seg = |start: i64, title: &str, canceled: bool| ScheduleSegment {
            id: 0,
            monitor_id: 0,
            start_time: start,
            end_time: Some(start + 3600),
            title: title.into(),
            category: String::new(),
            canceled,
            video_id: None,
        };
        store
            .replace_schedule_source(
                mid1,
                "platform",
                &[seg(500, "Past", false), seg(now + 2_000, "TW soon", false)],
            )
            .unwrap();
        store
            .replace_schedule_source(
                mid2,
                "platform",
                &[seg(now + 3_000, "Canceled", true), seg(now + 1_500, "YT soon", false)],
            )
            .unwrap();

        // Every upcoming occurrence, soonest first, canceled + past excluded.
        let all = store.all_upcoming_schedule(now + 1_000).unwrap();
        assert_eq!(
            all.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            vec!["YT soon", "TW soon"],
        );
        // The join carries the channel name + each monitor's own URL/platform.
        assert!(all.iter().all(|s| s.channel_name == "Streamer"));
        let yt = all.iter().find(|s| s.title == "YT soon").unwrap();
        assert_eq!(yt.monitor_id, mid2);
        assert_eq!(yt.platform(), Platform::YouTube);
        let tw = all.iter().find(|s| s.title == "TW soon").unwrap();
        assert_eq!(tw.platform(), Platform::Twitch);
    }
    #[test]
    fn schedule_sources_coexist_and_replace_independently() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let now = now_unix();
        let seg = |start: i64, title: &str| ScheduleSegment {
            id: 0,
            monitor_id: 0,
            start_time: start,
            end_time: Some(start + 3600),
            title: title.into(),
            category: String::new(),
            canceled: false,
            video_id: None,
        };

        // A platform segment and a Discord segment for the same monitor coexist.
        store
            .replace_schedule_source(mid, "platform", &[seg(now + 2_000, "Platform")])
            .unwrap();
        store
            .replace_schedule_source(mid, "discord", &[seg(now + 3_000, "Discord")])
            .unwrap();
        let all = store.all_upcoming_schedule(now + 1_000).unwrap();
        let mut titles: Vec<&str> = all.iter().map(|s| s.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, vec!["Discord", "Platform"]);
        assert!(all.iter().any(|s| s.title == "Discord" && s.source == "discord"));
        assert!(all.iter().any(|s| s.title == "Platform" && s.source != "discord"));

        // The monitor counts as having an upcoming non-Discord schedule (the
        // platform segment), which suppresses the Discord sweep for it.
        assert!(store.monitors_with_upcoming_non_discord(now + 1_000).unwrap().contains(&mid));

        // Replacing one source leaves the other intact.
        store.replace_schedule_source(mid, "discord", &[]).unwrap();
        let all = store.all_upcoming_schedule(now + 1_000).unwrap();
        assert_eq!(all.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), vec!["Platform"]);

        // Clearing the platform source drops it from the non-Discord presence set.
        store.replace_schedule_source(mid, "platform", &[]).unwrap();
        assert!(!store.monitors_with_upcoming_non_discord(now + 1_000).unwrap().contains(&mid));
    }
    #[test]
    fn manual_time_edit_leaves_tombstone_that_blocks_resurrection() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // Use upcoming instants so the past-tombstone prune never touches them.
        let t1 = now_unix() + 100_000;
        let t2 = t1 + 7_200; // user moves the stream +2h
        let seg = |start: i64, title: &str| ScheduleSegment {
            id: 0,
            monitor_id: 0,
            start_time: start,
            end_time: None,
            title: title.into(),
            category: String::new(),
            canceled: false,
            video_id: None,
        };

        // Auto source publishes the stream at t1; user corrects the time to t2.
        store.replace_schedule_source(mid, "platform", &[seg(t1, "Stream A")]).unwrap();
        let id = store.schedule_for_monitor(mid, 0).unwrap()[0].id;
        let n = store.update_schedule_segment_manual(id, t2, None, "Stream A", "").unwrap();
        assert_eq!(n, 1);

        // The platform source re-runs and STILL reports the stream at its old t1.
        store.replace_schedule_source(mid, "platform", &[seg(t1, "Stream A")]).unwrap();
        store.clear_other_schedule_sources(mid, "platform").unwrap();

        // Only the corrected manual entry survives — the pre-correction copy at t1
        // must not be resurrected.
        let visible = store.all_upcoming_schedule(t1 - 1).unwrap();
        let times: Vec<i64> = visible
            .iter()
            .filter(|s| s.monitor_id == mid)
            .map(|s| s.start_time)
            .collect();
        assert_eq!(times, vec![t2], "the pre-correction t1 copy must not reappear");
        assert!(visible.iter().any(|s| s.start_time == t2 && s.source == "manual"));

        // A non-existent id reports 0 rows (so the UI won't claim false success).
        assert_eq!(store.update_schedule_segment_manual(999_999, t2, None, "x", "").unwrap(), 0);
    }
    #[test]
    fn delete_schedule_segment_suppresses_refresh_resurrection() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let t1 = now_unix() + 100_000;
        let seg = |start: i64, title: &str| ScheduleSegment {
            id: 0,
            monitor_id: 0,
            start_time: start,
            end_time: None,
            title: title.into(),
            category: String::new(),
            canceled: false,
            video_id: None,
        };

        // An OCR source emits a bogus occurrence; the user deletes it.
        store.replace_schedule_source(mid, "twitch_banner_ocr", &[seg(t1, "Bogus")]).unwrap();
        let id = store.schedule_for_monitor(mid, 0).unwrap()[0].id;
        assert_eq!(store.delete_schedule_segment(id).unwrap(), 1);
        assert!(store.schedule_for_monitor(mid, 0).unwrap().is_empty(), "hidden after delete");

        // The OCR source re-runs against the unchanged image and re-emits it; the
        // tombstone must keep it gone (a bare delete would let it reappear).
        store.replace_schedule_source(mid, "twitch_banner_ocr", &[seg(t1, "Bogus")]).unwrap();
        assert!(
            store.schedule_for_monitor(mid, 0).unwrap().is_empty(),
            "delete must stick across an automatic refresh"
        );
    }
    #[test]
    fn schedule_diff_reports_added_updated_and_suppresses_title_fill() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let base = now_unix() + 100_000;
        let seg = |start: i64, title: &str, cat: &str| ScheduleSegment {
            id: 0,
            monitor_id: 0,
            start_time: start,
            end_time: None,
            title: title.into(),
            category: cat.into(),
            canceled: false,
            video_id: None,
        };

        // First run: everything is new → all "added".
        let ch = store
            .replace_schedule_source_diffed(mid, "platform", &[seg(base, "A", "Cat1"), seg(base + 60, "B", "")])
            .unwrap();
        assert_eq!(ch.len(), 2);
        assert!(ch.iter().all(|c| c.added));

        // Second run: retitle the first, leave the second, add a third.
        let ch = store
            .replace_schedule_source_diffed(
                mid,
                "platform",
                &[seg(base, "A2", "Cat1"), seg(base + 60, "B", ""), seg(base + 120, "C", "")],
            )
            .unwrap();
        // t=base retitled → updated; t=base+60 unchanged → absent; t=base+120 new → added.
        assert_eq!(ch.len(), 2);
        let updated = ch.iter().find(|c| c.start_time == base).unwrap();
        assert!(!updated.added && updated.title == "A2");
        let added = ch.iter().find(|c| c.start_time == base + 120).unwrap();
        assert!(added.added && added.title == "C");

        // Third run: only a blank→real title fill (category unchanged) → suppressed.
        store
            .replace_schedule_source_diffed(mid, "platform", &[seg(base + 180, "", "")])
            .unwrap();
        let ch = store
            .replace_schedule_source_diffed(mid, "platform", &[seg(base + 180, "Filled", "")])
            .unwrap();
        assert!(ch.is_empty(), "pure title-fill must be suppressed");

        // A category change on top of a fill IS reported (not pure title-fill).
        let ch = store
            .replace_schedule_source_diffed(mid, "platform", &[seg(base + 180, "Filled", "NewCat")])
            .unwrap();
        assert_eq!(ch.len(), 1);
        assert!(!ch[0].added && ch[0].category == "NewCat");
    }
    #[test]
    fn scheduled_recording_crud_and_due_queries() {
        let store = Store::open_in_memory().unwrap();
        let cid = store
            .upsert_channel("Sched", "https://twitch.tv/sched", Platform::Twitch)
            .unwrap();
        let m = sample_monitor(cid);
        let mid = store.insert_monitor(&m).unwrap();

        let id = store
            .insert_scheduled_recording(
                mid,
                "test rule",
                RecurrenceKind::Weekly,
                None,
                Some(crate::models::DOW_MON),
                Some(3600),
                None,
                Some(1800),
                Some(1_000_000),
            )
            .unwrap();

        let find = |s: &Store, id: i64| {
            s.list_scheduled_recordings()
                .unwrap()
                .into_iter()
                .find(|r| r.rec.id == id)
                .unwrap()
        };
        let got = find(&store, id).rec;
        assert_eq!(got.label, "test rule");
        assert_eq!(got.kind, RecurrenceKind::Weekly);
        assert_eq!(got.days_of_week, Some(crate::models::DOW_MON));
        assert_eq!(got.next_run_at, Some(1_000_000));
        assert!(got.enabled);

        // due_scheduled_recordings only surfaces enabled rows whose next_run_at
        // has arrived.
        assert!(store.due_scheduled_recordings(999_999).unwrap().is_empty());
        let due = store.due_scheduled_recordings(1_000_000).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, id);

        // Listing joins channel/monitor names.
        let listed = store.list_scheduled_recordings().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].channel_name, "Sched");

        // Firing advances next_run_at and arms the auto-stop.
        store
            .mark_scheduled_recording_fired(id, 1_000_000, Some(1_600_000), Some(1_000_000 + 1800))
            .unwrap();
        let after_fire = find(&store, id).rec;
        assert_eq!(after_fire.last_fired_at, Some(1_000_000));
        assert_eq!(after_fire.next_run_at, Some(1_600_000));
        assert_eq!(after_fire.pending_stop_at, Some(1_001_800));
        assert!(after_fire.enabled, "weekly rule stays enabled after firing");

        let stops = store.due_scheduled_stops(1_001_800).unwrap();
        assert_eq!(stops, vec![(id, mid)]);
        store.clear_scheduled_recording_stop(id).unwrap();
        assert!(store.due_scheduled_stops(1_001_800).unwrap().is_empty());

        // A fired Once rule (next_run_at = None) soft-disables instead of
        // deleting — it stays visible/auditable until the user removes it.
        let once_id = store
            .insert_scheduled_recording(
                mid,
                "once",
                RecurrenceKind::Once,
                Some(500),
                None,
                None,
                None,
                None,
                Some(500),
            )
            .unwrap();
        store
            .mark_scheduled_recording_fired(once_id, 500, None, None)
            .unwrap();
        let once_after = find(&store, once_id).rec;
        assert!(!once_after.enabled);
        assert_eq!(once_after.next_run_at, None);
        assert_eq!(store.list_scheduled_recordings().unwrap().len(), 2);

        store.delete_scheduled_recording(once_id).unwrap();
        assert_eq!(store.list_scheduled_recordings().unwrap().len(), 1);

        // Cascade delete when the owning monitor is removed.
        store.delete_monitor(mid).unwrap();
        assert!(store.list_scheduled_recordings().unwrap().is_empty());
    }
}
