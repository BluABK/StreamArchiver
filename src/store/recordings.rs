//! Recording rows: CRUD, head-backfill/concat state, orphan repair, path
//! relocation, and the listing/issue queries.

use super::*;

/// One of a take's *companion* media pointers — the files that live
/// alongside the main capture.
///
/// An enum rather than a column name in a string so the reconciler cannot
/// be handed one that does not exist, and so every consumer is forced
/// through the same fixed set of SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompanionPath {
    /// The joined head+live file (`full_path`).
    Full,
    /// The CDN head backfill (`backfill_path`).
    Backfill,
    /// A gap/VOD recovery result (`recovered_path`).
    Recovered,
    /// The downloaded published VOD (`vod_dl_path`).
    VodDl,
}

/// What a take's recorded `bytes` should become when its `output_path` moves.
///
/// Re-pointing is two different operations wearing one name:
///
/// * a **relocation** — the same file at a new place (a drive move, a rename
///   once the real title arrives). The size on disk did not change.
/// * a **substitution** — a *different* file now backs the take (a head+live
///   join, a gap splice, a `.ts` remuxed to `.mkv`, a published VOD replacing
///   the live capture). The old size is simply wrong.
///
/// Making the caller say which is not ceremony. Head-backfill left `bytes` at
/// the live capture's size while `output_path` named the joined `full.mkv`, and
/// four other substitution sites did the same — 412 GB under-reported across 66
/// takes when this was measured. The error is worst on the biggest takes,
/// because the under-report is exactly the size of the head that was missing:
/// one take recorded 0.04 GB for a 16.41 GB file. Those are precisely the rows
/// the storage stats exist to surface, so the stats were blindest where they
/// mattered most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepointBytes {
    /// Same file, new path — keep the recorded size.
    Unchanged,
    /// A different file backs the take now; this is its measured size.
    Measured(i64),
}

/// One row the startup media sweep checks against the filesystem — a take or a
/// video download. See [`Store::takes_with_media_on_disk`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRow {
    pub id: i64,
    pub path: String,
    /// What the row claims the file's size is.
    pub bytes: i64,
    /// When the media was last found to be gone; `0` while it is present.
    pub missing_at: i64,
}

/// `(id, started_at, ended_at, status)` — see `Store::earlier_takes_for_stream`.
pub type EarlierTakeRow = (i64, i64, Option<i64>, String);

/// One rolling take due for disposal — everything
/// [`crate::disposal::dispose_media`] needs, without a second lookup. See
/// [`Store::expired_rolling_recordings`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredRolling {
    pub rec_id: i64,
    pub monitor_id: i64,
    pub channel_id: i64,
    pub output_path: String,
}

/// One finished take whose chat sidecar hasn't been mined for moderation
/// actions yet — see [`Store::recordings_needing_chat_scan`] and
/// [`crate::chat_scan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatScanTarget {
    pub rec_id: i64,
    pub monitor_id: i64,
    /// Broadcast id (`''` when unknown) — copied onto every event row the scan
    /// writes, so they group with the rest of that broadcast's history.
    pub stream_id: String,
    /// Capture start, the anchor for the sidecar's relative (VOD-replay)
    /// timestamps.
    pub started_at: i64,
    pub chat_path: String,
}

/// Enough of a take to name it in a list — see [`Store::take_labels`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakeLabel {
    pub channel: String,
    pub platform: String,
    pub started_at: i64,
    pub title: String,
    pub monitor_id: i64,
}

/// One take the chat index has yet to read — see
/// [`Store::chat_index_candidates`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatIndexTarget {
    pub rec_id: i64,
    pub monitor_id: i64,
    /// Denormalised into `chat_presence` so "which channels was this person in"
    /// never has to reach back into the main database.
    pub channel_id: i64,
    /// Capture start, the anchor for a YouTube sidecar's relative timestamps.
    pub started_at: i64,
    pub chat_path: String,
    /// The instance's source URL — `Platform::detect` turns it into a platform.
    /// There is no `monitor.platform` column: an instance's platform has always
    /// been derived from its URL.
    pub url: String,
}

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

    /// Stamp a freshly-inserted take with the rolling TTL resolved for its
    /// instance (see [`crate::disposal::effective_rolling`]).
    ///
    /// Deliberately a second statement rather than another parameter on
    /// [`Self::insert_recording`], whose argument list is already at the
    /// `too_many_arguments` limit and which a dozen tests call positionally.
    /// The gap between the two writes is harmless: the sweep only ever looks
    /// at takes that have already ended.
    pub fn set_recording_rolling_ttl(&self, rec_id: i64, ttl_secs: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET rolling_ttl_secs = ?2 WHERE id = ?1",
            params![rec_id, ttl_secs],
        )?;
        Ok(())
    }

    /// **Keep** a rolling take: it stops counting down and becomes an ordinary
    /// archived stream that happens to have come from a rolling recording.
    pub fn keep_rolling_recording(&self, rec_id: i64, now: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET rolling_kept_at = ?2 WHERE id = ?1 AND rolling_expired_at = 0",
            params![rec_id, now],
        )?;
        Ok(())
    }

    /// **Unkeep**: put a kept take back in the rolling set, with the clock
    /// **restarted** from `now` rather than resumed from when it ended —
    /// otherwise un-keeping anything older than its TTL would delete it on the
    /// next sweep, seconds later. See [`crate::models::Rolling::from`].
    pub fn unkeep_rolling_recording(&self, rec_id: i64, now: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET rolling_kept_at = 0, rolling_from = ?2
             WHERE id = ?1 AND rolling_expired_at = 0",
            params![rec_id, now],
        )?;
        Ok(())
    }

    /// Stamp a take as expired once the sweep has disposed of its file.
    ///
    /// Test-only: production always goes through
    /// [`Self::clear_recording_media`], because a take's countdown ending and
    /// its file going away are the same event and writing one without the
    /// other is exactly the bug that produced permanent `🕰 N (due)` badges.
    /// Kept here so a test can set up an already-swept row without also
    /// clearing its path.
    #[cfg(test)]
    pub fn mark_rolling_expired(&self, rec_id: i64, now: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET rolling_expired_at = ?2 WHERE id = ?1",
            params![rec_id, now],
        )?;
        Ok(())
    }

    /// Rolling takes whose TTL has elapsed and which the user never kept —
    /// what [`crate::rolling`]'s sweep disposes of.
    ///
    /// Excludes, in order: non-rolling takes, already-kept ones, already-swept
    /// ones, and takes still recording (`ended_at IS NULL` — the clock only
    /// starts when the capture finishes). The deadline mirrors
    /// `Rolling::deadline`: `rolling_from` when the user un-kept it, otherwise
    /// `ended_at`.
    ///
    /// It deliberately does **not** exclude takes with an empty `output_path`.
    /// It used to — "no file left to delete" reads like an obvious skip — and
    /// that made every such take a permanent ghost: the countdown query
    /// ([`Self::rolling_rollup_by_monitor`]) still counted it, so its channel
    /// read `🕰 38 (due)` forever, while the sweep that would have stamped it
    /// could not see it. A take whose media went elsewhere (manual delete, a
    /// relocation that cleared the path) has *already* reached the end state
    /// this countdown exists to produce; the sweep stamps it and moves on.
    /// Anything the two queries disagree about is a bug by construction —
    /// `every_counting_take_is_reachable_by_the_sweep` pins that.
    pub fn expired_rolling_recordings(&self, now: i64) -> Result<Vec<ExpiredRolling>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id, r.monitor_id, m.channel_id, COALESCE(r.output_path, '')
             FROM recording r JOIN monitor m ON m.id = r.monitor_id
             WHERE r.rolling_ttl_secs > 0
               AND r.rolling_kept_at = 0
               AND r.rolling_expired_at = 0
               AND r.ended_at IS NOT NULL
               AND (CASE WHEN r.rolling_from > 0 THEN r.rolling_from ELSE r.ended_at END)
                   + r.rolling_ttl_secs <= ?1
             ORDER BY r.id",
        )?;
        let rows = stmt
            .query_map(params![now], |r| {
                Ok(ExpiredRolling {
                    rec_id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    channel_id: r.get(2)?,
                    output_path: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Finished takes whose chat sidecar still has to be mined for moderation
    /// actions ([`crate::chat_scan`]), oldest first.
    ///
    /// Only takes that have **ended** qualify: a live YouTube sidecar is still
    /// being appended to by yt-dlp, and re-reading a growing file every sweep
    /// would be both wasteful and duplicate-prone. The chat replay strikes
    /// deleted messages the moment it parses them either way, so nothing the
    /// user can see waits on this — only the recorded statistics do.
    ///
    /// `chat_scanned_at = 0` is the "never scanned" sentinel (schema v89), and
    /// `idx_recording_chat_unscanned` covers exactly this predicate, so the
    /// query stays cheap once the backlog has drained.
    pub fn recordings_needing_chat_scan(&self, limit: i64) -> Result<Vec<ChatScanTarget>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, COALESCE(stream_id, ''), started_at, chat_path
             FROM recording
             WHERE chat_scanned_at = 0
               AND COALESCE(chat_path, '') != ''
               AND ended_at IS NOT NULL
             ORDER BY id
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok(ChatScanTarget {
                    rec_id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    stream_id: r.get(2)?,
                    started_at: r.get(3)?,
                    chat_path: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every finished take that has a chat sidecar, with the ids the chat index
    /// needs to file it under — newest first, so a fresh install indexes the
    /// streams the user is most likely to look up before the 2024 backlog.
    ///
    /// The "already indexed" stamp lives in the *other* database file
    /// (`indexed_take`, see [`crate::chat_index`]), so it can't be joined here;
    /// the caller filters this list against
    /// [`ChatIndex::indexed_rec_ids`](crate::chat_index::ChatIndex::indexed_rec_ids).
    /// Keeping the stamp in one place is worth the full list: a mirrored column
    /// here could drift out of step with a deleted or rebuilt index file, and a
    /// take that silently believes it was indexed is invisible — nothing would
    /// ever look for it again.
    pub fn chat_index_candidates(&self) -> Result<Vec<ChatIndexTarget>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id, r.monitor_id, m.channel_id, r.started_at, r.chat_path, m.url
             FROM recording r
             JOIN monitor m ON m.id = r.monitor_id
             WHERE COALESCE(r.chat_path, '') != ''
               AND r.ended_at IS NOT NULL
             ORDER BY r.id DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ChatIndexTarget {
                    rec_id: r.get(0)?,
                    monitor_id: r.get(1)?,
                    channel_id: r.get(2)?,
                    started_at: r.get(3)?,
                    chat_path: r.get(4)?,
                    url: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Name, platform, start time and title for a set of takes, in one query.
    ///
    /// The chat index returns recording ids and nothing else (it is a separate
    /// database file and cannot join against this one), so the Users view needs
    /// to turn a page of ids into something readable. One statement rather than
    /// a `get_recording` per row: a busy chatter's page is fifty streams, and
    /// fifty lock acquisitions on the render path is exactly the sort of thing
    /// that shows up as a stutter.
    pub fn take_labels(&self, rec_ids: &[i64]) -> Result<std::collections::HashMap<i64, TakeLabel>> {
        if rec_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let conn = self.db();
        let placeholders = vec!["?"; rec_ids.len()].join(",");
        let sql = format!(
            // `title` is derived from `stream_meta_change`, not a stored column
            // — the same latest-title subquery `RECORDING_FULL_COLUMNS` uses.
            "SELECT r.id, c.name, m.url, r.started_at,
                    COALESCE((SELECT new_value FROM stream_meta_change smc
                              WHERE smc.recording_id = r.id AND smc.kind = 'title'
                              ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), ''),
                    m.id
             FROM recording r
             JOIN monitor m ON m.id = r.monitor_id
             JOIN channel c ON c.id = m.channel_id
             WHERE r.id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(rec_ids.iter());
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                TakeLabel {
                    channel: r.get(1)?,
                    platform: crate::models::Platform::detect(&r.get::<_, String>(2)?)
                        .as_str()
                        .to_string(),
                    started_at: r.get(3)?,
                    title: r.get(4)?,
                    monitor_id: r.get(5)?,
                },
            ))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (id, label) = row?;
            out.insert(id, label);
        }
        Ok(out)
    }

    /// Stamp a take's chat sidecar as mined. Also used to force a **rescan**
    /// (stamp 0), which is why it takes the timestamp rather than reading the
    /// clock itself.
    pub fn set_recording_chat_scanned(&self, rec_id: i64, at: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE recording SET chat_scanned_at = ?2 WHERE id = ?1", params![rec_id, at])?;
        Ok(())
    }

    /// What each monitor's rolling takes add up to — how many are counting
    /// down towards auto-deletion, and when the first of them is due. Backs
    /// the 🕰 rollup badge and the sortable "Rolling" column on the Streams
    /// grid's instance and channel rows.
    ///
    /// Monitors with none are absent from the map. Served by
    /// `idx_recording_rolling` (a partial index over `rolling_ttl_secs > 0`),
    /// so this stays cheap however large the recording table gets — but it is
    /// still a DB read, so cache it against `streams_cache_rev` rather than
    /// calling it from a render path.
    /// Per-monitor sum of finished-take bytes — the Streams grid's channel/
    /// instance "disk use" rollup (`take_size_bytes` covers the period/
    /// stream/take rows below them, confirming each file against disk; this
    /// coarser SQL-only sum is what makes a COLLAPSED channel/instance row
    /// affordable, since it never needs `recordings_for_monitor`'s full
    /// per-take history — see `[[stream-take-filesize]]`/the `groups` map's
    /// own doc comment on why that stays expansion-gated).
    ///
    /// Excludes `output_path = ''` (a take whose only output was a failed/
    /// never-attempted VOD backfill, never a real file) but does NOT confirm
    /// the file still exists — a take whose file vanished after finalize
    /// keeps counting here until its row is disposed of or its `bytes` is
    /// otherwise cleared. Cache against `streams_cache_rev`, same as
    /// `rolling_rollup_by_monitor`, never call from a render path.
    pub fn monitor_disk_usage(&self) -> Result<std::collections::HashMap<i64, i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            // `media_missing_at`: a deleted file is not disk usage. Without
            // this the badge kept charging a channel for media that is gone.
            "SELECT monitor_id, SUM(bytes) FROM recording
             WHERE bytes > 0 AND output_path != '' AND media_missing_at = 0
             GROUP BY monitor_id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<std::collections::HashMap<i64, i64>>>()?;
        Ok(rows)
    }

    /// Which drive letters each monitor's takes are stored on — the Streams
    /// grid's 🖴 column on channel and instance rows, where the per-take paths
    /// aren't loaded (a collapsed row has no history in memory; see `groups`).
    ///
    /// Read from the stored `output_path`s, **not** confirmed against disk:
    /// a take whose file has since gone missing keeps its drive listed until
    /// the row is disposed of (which clears `output_path`) — the same caveat
    /// as `monitor_disk_usage`, and for the same reason. Takes with no path
    /// (never captured, VOD-backfill placeholders, already disposed of) and
    /// non-drive paths (UNC, relative) contribute nothing.
    ///
    /// The `GROUP BY` collapses this to at most a handful of rows per
    /// monitor rather than one per take, but it is still a DB read — cache it
    /// against `streams_cache_rev` like its neighbours, never call it from a
    /// render path.
    pub fn drive_letters_by_monitor(&self) -> Result<std::collections::HashMap<i64, Vec<char>>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT monitor_id, UPPER(SUBSTR(output_path, 1, 1))
             FROM recording
             WHERE output_path != '' AND SUBSTR(output_path, 2, 1) = ':'
             GROUP BY monitor_id, UPPER(SUBSTR(output_path, 1, 1))
             ORDER BY monitor_id, 2",
        )?;
        let mut out: std::collections::HashMap<i64, Vec<char>> = std::collections::HashMap::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (mid, letter) = row?;
            // SUBSTR can't tell "C:/rec" from "1:2" — only an ASCII letter is
            // a drive.
            if let Some(c) = letter.chars().next().filter(char::is_ascii_alphabetic) {
                let e = out.entry(mid).or_default();
                if !e.contains(&c) {
                    e.push(c);
                }
            }
        }
        Ok(out)
    }

    pub fn rolling_rollup_by_monitor(
        &self,
    ) -> Result<std::collections::HashMap<i64, crate::rolling::RollingRollup>> {
        let conn = self.db();
        // The deadline expression mirrors `Rolling::deadline` exactly (see
        // `expired_rolling_recordings`, which sweeps on the same arithmetic):
        // count from `rolling_from` when an Unkeep restarted the clock, else
        // from `ended_at`. A still-recording take has no deadline at all —
        // MIN() skips it while COUNT(*) still counts it, which is what
        // "3 rolling, next in 2d" has to mean while one of them is being
        // captured right now. `ended_at IS NULL` is tested FIRST rather than
        // relying on the ELSE branch to be NULL: an Unkeep on a live take sets
        // `rolling_from`, which would otherwise hand it a deadline the sweep
        // (gated on `ended_at IS NOT NULL`) could never act on.
        //
        // `rolling_ttl_secs` is a bare column beside a single MIN(), so SQLite
        // takes it from the row that MIN() selected — the TTL of the soonest
        // take, which is exactly the denominator its countdown colour needs.
        let mut stmt = conn.prepare(
            "SELECT monitor_id,
                    COUNT(*),
                    MIN(CASE WHEN ended_at IS NULL THEN NULL
                             WHEN rolling_from > 0 THEN rolling_from + rolling_ttl_secs
                             ELSE ended_at + rolling_ttl_secs END),
                    rolling_ttl_secs
             FROM recording
             WHERE rolling_ttl_secs > 0 AND rolling_kept_at = 0 AND rolling_expired_at = 0
             GROUP BY monitor_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let soonest: Option<i64> = r.get(2)?;
                Ok((
                    r.get::<_, i64>(0)?,
                    crate::rolling::RollingRollup {
                        count: r.get(1)?,
                        soonest,
                        // Meaningless without a deadline to pair it with, and
                        // arbitrary in SQL when every row's deadline is NULL.
                        ttl_secs: if soonest.is_some() { r.get(3)? } else { 0 },
                    },
                ))
            })?
            .collect::<rusqlite::Result<
                std::collections::HashMap<i64, crate::rolling::RollingRollup>,
            >>()?;
        Ok(rows)
    }

    /// Currently-open "seen live but not recorded" session for this monitor
    /// (`status = 'not_recorded'`, `ended_at IS NULL`), if any — `(id,
    /// started_at)`. Used both to avoid opening a second session while one is
    /// already tracking the same broadcast, and to compute a take-relative
    /// `at_secs` offset when mirroring a title/category change into it.
    pub fn open_not_recorded_session(&self, monitor_id: i64) -> Result<Option<(i64, i64)>> {
        let conn = self.db();
        conn.query_row(
            "SELECT id, started_at FROM recording
             WHERE monitor_id = ?1 AND status = 'not_recorded' AND ended_at IS NULL
             ORDER BY id DESC LIMIT 1",
            params![monitor_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Record that a broadcast was seen live while Auto-record was off — a
    /// take-shaped row (so it slots into the Streams grid's normal
    /// take-grouping/numbering) with no capture behind it: `output_path`
    /// stays empty, `bytes` stays 0, nothing here ever spawns a process or
    /// touches disk. Closed by [`Self::close_open_not_recorded_sessions`]
    /// once the broadcast ends (or a real recording starts and supersedes it).
    pub fn insert_not_recorded_session(
        &self,
        monitor_id: i64,
        started_at: i64,
        went_live_at: Option<i64>,
        went_live_approx: bool,
        stream_id: Option<&str>,
    ) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO recording(monitor_id, started_at, output_path, status, went_live_at, went_live_approx, stream_id)
             VALUES(?1, ?2, '', 'not_recorded', ?3, ?4, ?5)",
            params![monitor_id, started_at, went_live_at, went_live_approx as i64, stream_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Retroactively record a broadcast the app has no other trace of — found
    /// by a platform-side discovery scan (`crate::downloader::backfill_discover`)
    /// diffing the platform's own VOD/video listing against known
    /// `stream_id`s. Materializes it as an ordinary `not_recorded` row (see
    /// [`Self::insert_not_recorded_session`]) with an explicit `ended_at` (the
    /// session is already over, unlike a live-witnessed one) so it renders
    /// exactly like a "seen live, Auto was off" row — same grid, same
    /// context-menu actions, same `attempt_missed_stream_backfill` trigger.
    /// Skips (returns `Ok(None)`) if a recording for this monitor already
    /// carries this `stream_id`, so repeated scans never create duplicates.
    pub fn insert_discovered_not_recorded(
        &self,
        monitor_id: i64,
        started_at: i64,
        ended_at: i64,
        stream_id: &str,
        title: &str,
    ) -> Result<Option<i64>> {
        let rec_id = {
            let conn = self.db();
            let n = conn.execute(
                "INSERT INTO recording(monitor_id, started_at, ended_at, output_path, status, stream_id)
                 SELECT ?1, ?2, ?3, '', 'not_recorded', ?4
                 WHERE NOT EXISTS (SELECT 1 FROM recording WHERE monitor_id = ?1 AND stream_id = ?4)",
                params![monitor_id, started_at, ended_at, stream_id],
            )?;
            if n == 0 {
                return Ok(None);
            }
            conn.last_insert_rowid()
        };
        // `title`/`category` are derived from `stream_meta_change`, not a
        // stored column (see `RECORDING_FULL_COLUMNS`) — give the row its
        // discovered title the same way a live poll would have logged one.
        if !title.is_empty() {
            let _ = self.insert_meta_change(rec_id, 0, "title", "", title);
        }
        Ok(Some(rec_id))
    }

    /// Whether a **different** instance of the same channel actually captured
    /// a broadcast covering `at` (± `slack` seconds).
    ///
    /// The missed-stream discovery scan only ever sees one instance's own VOD
    /// listing, so a simulcast broadcast that was deliberately recorded on a
    /// sibling instead looks exactly like a gap. This is how it tells the two
    /// apart. Only real captures count — a `not_recorded` row on the sibling
    /// means nobody has the broadcast.
    pub fn sibling_take_covers(&self, monitor_id: i64, at: i64, slack: i64) -> Result<bool> {
        let conn = self.db();
        let covered: bool = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM recording r
                JOIN monitor m  ON m.id  = r.monitor_id
                JOIN monitor me ON me.id = ?1
                WHERE m.channel_id = me.channel_id
                  AND r.monitor_id != ?1
                  AND r.status != 'not_recorded'
                  AND COALESCE(r.output_path, '') != ''
                  AND ?2 >= r.started_at - ?3
                  AND ?2 <= COALESCE(r.ended_at, r.started_at) + ?3)",
            params![monitor_id, at, slack],
            |r| r.get(0),
        )?;
        Ok(covered)
    }

    /// Record why a `not_recorded` take wasn't captured (see
    /// [`crate::models::Recording::not_recorded_reason`]). Only set on insert —
    /// a session reused across polls keeps the reason it opened with, so the
    /// row can't flip its story mid-broadcast.
    pub fn set_not_recorded_reason(&self, rec_id: i64, reason: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET not_recorded_reason = ?2 WHERE id = ?1",
            params![rec_id, reason],
        )?;
        Ok(())
    }

    /// Mark a take as refused for lack of entitlement (see
    /// [`crate::models::Recording::gated`]). Set by the supervisor before it
    /// finalizes the take, so `finish_recording` can see it and skip the
    /// `capture_failed` error the 🔒 alert already explains.
    pub fn set_recording_gated(&self, rec_id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE recording SET gated = 1 WHERE id = ?1", params![rec_id])?;
        Ok(())
    }

    /// Point a take at the chat sidecar being written for it (see
    /// [`crate::models::Recording::chat_path`]). Persisted at spawn for EVERY
    /// chat producer (recorded takes and chat-only sessions alike) since the
    /// dedicated chat-root option landed — the sidecar may live on another
    /// drive, so the reader's `output_path`-derived fallbacks can't find it.
    pub fn set_recording_chat_path(&self, rec_id: i64, chat_path: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET chat_path = ?2 WHERE id = ?1",
            params![rec_id, chat_path],
        )?;
        Ok(())
    }

    /// Work list for the one-shot chat-log migration sweep: every recording's
    /// `(id, output_path, chat_path, still_open)`. `still_open` (no `ended_at`)
    /// flags takes whose chat may still be written — the sweep must not move
    /// a file out from under a live logger.
    pub fn list_recordings_for_chat_migration(&self) -> Result<Vec<(i64, String, String, bool)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, COALESCE(output_path, ''), chat_path, ended_at IS NULL FROM recording
             WHERE chat_path != '' OR COALESCE(output_path, '') != ''",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Re-point `chat_path` after a chat sidecar was physically renamed or
    /// moved, keyed by the OLD path — every move/rename site calls this with
    /// the (from, to) pair it just executed, so no caller needs to know which
    /// recording owns the file. A no-op (0 rows) when no take points there
    /// (e.g. legacy takes recorded before `chat_path` was persisted at spawn).
    pub fn update_chat_path_by_path(&self, old_path: &str, new_path: &str) -> Result<usize> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET chat_path = ?2 WHERE chat_path = ?1",
            params![old_path, new_path],
        )
        .map_err(Into::into)
    }

    /// Close any open not-recorded session for this monitor (see
    /// [`Self::open_not_recorded_session`]) — the broadcast ended, or a real
    /// recording just started and supersedes it. A no-op (empty `Vec`) when
    /// none is open, so callers can call this unconditionally on every
    /// relevant transition without checking first. Returns the closed rows'
    /// ids (at most one in practice — one open session per monitor — but a
    /// `Vec` in case that invariant is ever relaxed) so a genuine
    /// broadcast-ended close can kick off a missed-stream backfill attempt;
    /// see `crate::downloader::vod::attempt_missed_stream_backfill`.
    pub fn close_open_not_recorded_sessions(&self, monitor_id: i64, ended_at: i64) -> Result<Vec<i64>> {
        let conn = self.db();
        let ids: Vec<i64> = conn
            .prepare(
                "SELECT id FROM recording
                 WHERE monitor_id = ?1 AND status = 'not_recorded' AND ended_at IS NULL",
            )?
            .query_map(params![monitor_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        conn.execute(
            "UPDATE recording SET ended_at = ?2
             WHERE monitor_id = ?1 AND status = 'not_recorded' AND ended_at IS NULL",
            params![monitor_id, ended_at],
        )?;
        Ok(ids)
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
        {
            let conn = self.db();
            conn.execute(
                "UPDATE recording SET ended_at=?2, bytes=?3, exit_code=?4, status=?5, output_path=?6, log_excerpt=?7 WHERE id=?1",
                params![id, ended_at, bytes, exit_code, status, output_path, log_excerpt],
            )?;
        }
        // Every FAILED take must be visible in the 🚨 Warnings window, not
        // only in the 🔔 feed's "— failed" rows: takes that die without a
        // scanner-matched log line (killed process, unrecognised wording)
        // previously left no alert at all. Filed here — the one choke point
        // every finalize path (supervisor + detached/adopted re-attach) goes
        // through — and only when no error-severity alert already covers the
        // take, so a 🎫 PO-token or tool-error row isn't duplicated.
        // Deliberately only 'failed': user-initiated aborts are not damage.
        if status == "failed" {
            self.maybe_file_capture_failed_alert(id, exit_code, log_excerpt)?;
        }
        Ok(())
    }

    /// See [`Store::finish_recording`]: one `capture_failed` error alert per
    /// failed take with no other error alert to its name. `last_line` = the
    /// log tail's last non-empty line (the actual reason, when the tool said
    /// one), else the exit code.
    fn maybe_file_capture_failed_alert(
        &self,
        rec_id: i64,
        exit_code: Option<i64>,
        log_excerpt: &str,
    ) -> Result<()> {
        // A gated take is covered by definition: its 🔒 alert IS the whole
        // explanation for the failure — the broadcast wasn't ours to capture —
        // and adding "Capture failed" beside it says the opposite of the truth,
        // turning a known, expected state into a red fault.
        //
        // Read off the take's own `gated` flag, not off the 🔒 alert's
        // `recording_id`: that alert is keyed by the broadcast, so it names
        // only the FIRST doomed take, and every attempt after it would look
        // uncovered here and file the error anyway.
        let covered: bool = self.db().query_row(
            "SELECT EXISTS(SELECT 1 FROM capture_alert
                           WHERE recording_id = ?1 AND severity = 'error')
                 OR EXISTS(SELECT 1 FROM recording WHERE id = ?1 AND gated = 1)",
            params![rec_id],
            |r| r.get(0),
        )?;
        if covered {
            return Ok(());
        }
        let (monitor_id, channel): (i64, String) = self.db().query_row(
            "SELECT r.monitor_id, COALESCE(c.name, '')
             FROM recording r
             JOIN monitor m ON m.id = r.monitor_id
             JOIN channel c ON c.id = m.channel_id
             WHERE r.id = ?1",
            params![rec_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let last_line = log_excerpt
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| match exit_code {
                Some(c) => format!("capture process exited with code {c} (no log output)"),
                None => "capture process died without log output".to_string(),
            });
        // A failed take whose log shows a GVS PO-token rejection files as the
        // same 🎫 kind the supervisor pre-files for zero-byte rejections —
        // one cause, one title, whether or not the take captured bytes first.
        let (kind, take_key) = if crate::models::po_token_rejected(log_excerpt) {
            ("po_token_rejected", format!("po_token:rec{rec_id}"))
        } else {
            ("capture_failed", format!("capture_failed:rec{rec_id}"))
        };
        self.upsert_capture_alert(&crate::store::NewCaptureAlert {
            kind: kind.to_string(),
            severity: "error".to_string(),
            source: "capture".to_string(),
            take_key,
            monitor_id: Some(monitor_id),
            recording_id: Some(rec_id),
            video_id: None,
            channel,
            count: 1,
            lost_segments: 0,
            last_line,
        })?;
        Ok(())
    }

    /// 1-based position of this take among its broadcast's takes (ordered by
    /// start, id-tiebroken) — "take 3" in notification headings. `None` when
    /// the recording has no platform stream id to group by (id-less takes
    /// group fuzzily by time; a wrong number is worse than none) or the row
    /// is gone.
    pub fn take_number(&self, rec_id: i64) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        let conn = self.db();
        let Some((monitor_id, stream_id, started_at)) = conn
            .query_row(
                "SELECT monitor_id, stream_id, started_at FROM recording WHERE id = ?1",
                params![rec_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let Some(sid) = stream_id.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM recording
             WHERE monitor_id = ?1 AND stream_id = ?2
               AND (started_at < ?3 OR (started_at = ?3 AND id <= ?4))",
            params![monitor_id, sid, started_at, rec_id],
            |r| r.get(0),
        )?;
        Ok(Some(n))
    }

    /// Update the output path of a finished recording — used after a manual
    /// re-remux succeeds to replace the `.ts` capture path with the final `.mkv`.
    /// A take's media is gone: clear the path and end any countdown attached
    /// to it, in one step.
    ///
    /// Every caller that disposes of a take's file wants both halves, and the
    /// two used to be written separately at each site — so manual delete wrote
    /// one and forgot the other. That left a rolling take with no file still
    /// counting down, unreachable by the sweep (which needs something to
    /// delete) and still counted by the badge, which pinned a whole channel at
    /// `🕰 N (due)` for ever. See `crate::rolling`.
    ///
    /// A **kept** take is deliberately left alone: it has no countdown to end,
    /// so it was never part of that failure, and stamping it would relabel a
    /// deliberate Keep as an expiry — reporting something the user did not do.
    pub fn clear_recording_media(&self, id: i64, now: i64) -> Result<()> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        tx.execute("UPDATE recording SET output_path = '' WHERE id = ?1", params![id])?;
        tx.execute(
            "UPDATE recording SET rolling_expired_at = ?2
             WHERE id = ?1 AND rolling_ttl_secs > 0
               AND rolling_kept_at = 0 AND rolling_expired_at = 0",
            params![id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Re-point a take at a different `output_path`, stating what its recorded
    /// `bytes` should become. See [`RepointBytes`] for why that is not optional.
    pub fn update_recording_output_path(&self, id: i64, path: &str, bytes: RepointBytes) -> Result<()> {
        let conn = self.db();
        match bytes {
            RepointBytes::Unchanged => conn.execute(
                "UPDATE recording SET output_path = ?2 WHERE id = ?1",
                params![id, path],
            )?,
            RepointBytes::Measured(n) => conn.execute(
                "UPDATE recording SET output_path = ?2, bytes = ?3 WHERE id = ?1",
                params![id, path, n],
            )?,
        };
        Ok(())
    }

    /// Every take whose `output_path` names a file materially larger than its
    /// recorded `bytes` — the drift left behind by every join/splice/remux that
    /// re-pointed a row before [`RepointBytes`] existed. Returns
    /// `(id, output_path, recorded_bytes)`; the caller stats each file (this is
    /// the sync store; disk I/O belongs in the async reconcile) and corrects
    /// the ones that really are wrong.
    ///
    /// Deliberately *not* limited to `.full.mkv`/`.gapless.mkv`: the same drift
    /// reaches any take a substitution touched, and the caller's own stat is
    /// the authority on whether a given row needs correcting.
    pub fn takes_with_media_on_disk(&self) -> Result<Vec<MediaRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, output_path, bytes, media_missing_at FROM recording
             WHERE output_path <> '' AND status IN ('completed', 'ended', 'stopped')",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(MediaRow { id: r.get(0)?, path: r.get(1)?, bytes: r.get(2)?, missing_at: r.get(3)? })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Correct a take's recorded size to what is actually on disk.
    pub fn set_recording_bytes(&self, id: i64, bytes: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE recording SET bytes = ?2 WHERE id = ?1", params![id, bytes])?;
        Ok(())
    }

    /// Record that a take's media is (or is no longer) missing from disk.
    /// `now` stamps the absence; `0` clears it because the file came back.
    ///
    /// Separate from `bytes` on purpose: `bytes` answers "how big was this
    /// take", which stays true forever, while this answers "is it still
    /// here", which every space-in-use total needs and none of them could ask.
    /// Writes only on a change, so a startup sweep over a healthy archive
    /// touches nothing.
    pub fn set_recording_media_missing(&self, id: i64, now: i64) -> Result<bool> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE recording SET media_missing_at = ?2
             WHERE id = ?1 AND (media_missing_at = 0) != (?2 = 0)",
            params![id, now],
        )?;
        Ok(n > 0)
    }

    /// Remove a recording (take) row from the history. The captured file on disk
    /// is left untouched. Refuses an in-progress ('recording') take so we never
    /// orphan a running capture from its history row; returns the rows removed.
    ///
    /// Also drops the take's chat-index rows. That lives here, at the single
    /// choke point every deletion path goes through, rather than at the half
    /// dozen call sites — a missed one would leave the index claiming a chatter
    /// was in a stream that no longer exists, and "seen in N streams" would
    /// slowly drift upward forever. Best-effort by design: a broken index must
    /// never block deleting a row from the real database.
    pub fn delete_recording(&self, id: i64) -> Result<usize> {
        let n = {
            let conn = self.db();
            conn.execute(
                "DELETE FROM recording WHERE id = ?1 AND status <> 'recording'",
                params![id],
            )?
        };
        if n > 0
            && let Some(idx) = crate::chat_index::shared()
            && let Err(e) = idx.forget_take(id)
        {
            tracing::warn!(rec_id = id, "chat index: could not drop rows for a deleted take: {e:#}");
        }
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

    /// Set/clear a recording's chapters-embed state — see
    /// [`crate::models::Recording::chapters_state`].
    pub fn set_chapters_state(&self, id: i64, state: &str) -> Result<()> {
        let conn = self.db();
        // A reset to "" (manual retrigger, bulk re-embed) always means "start
        // fresh" — zero the automatic-retry counter along with it, so a prior
        // run of transient failures doesn't count against the new attempt.
        conn.execute(
            "UPDATE recording SET chapters_state=?2,
                 chapters_attempts = CASE WHEN ?2 = '' THEN 0 ELSE chapters_attempts END
             WHERE id=?1",
            params![id, state],
        )?;
        Ok(())
    }

    /// Record a failed chapters-embed attempt: bump `chapters_attempts` and
    /// requeue (`chapters_state = "queued"`) for the automatic retry sweep to
    /// pick up, unless `next_attempts` has reached `MAX_CHAPTERS_ATTEMPTS` —
    /// then give up for good (`chapters_state = "failed"`, needing the manual
    /// "Re-embed chapters" button). Same attempt-count-gated shape as
    /// gap-recovery's `gap_range.attempts`.
    pub fn record_chapters_failure(&self, id: i64, next_attempts: i64, exhausted: bool) -> Result<()> {
        let conn = self.db();
        let state = if exhausted { "failed" } else { "queued" };
        conn.execute(
            "UPDATE recording SET chapters_state=?2, chapters_attempts=?3 WHERE id=?1",
            params![id, state, next_attempts],
        )?;
        Ok(())
    }

    /// Persist the actual embedded chapter list — see
    /// [`crate::models::Recording::chapters_json`]. Set alongside
    /// `chapters_state = "done"` on a successful embed.
    pub fn set_chapters_json(&self, id: i64, json: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET chapters_json=?2 WHERE id=?1",
            params![id, json],
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

    /// Earlier takes of the same broadcast — `(id, started_at, ended_at,
    /// status)`, oldest first — for the head-backfill "don't re-fetch what's
    /// already covered" guard: a non-first take shouldn't re-download the
    /// whole missed span from the live VOD if an earlier take is still
    /// recording it, or already captured it live.
    pub fn earlier_takes_for_stream(
        &self,
        monitor_id: i64,
        stream_id: &str,
        before_started_at: i64,
    ) -> Result<Vec<EarlierTakeRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, started_at, ended_at, status FROM recording
             WHERE monitor_id = ?1 AND stream_id = ?2 AND started_at < ?3
             ORDER BY started_at",
        )?;
        let rows = stmt
            .query_map(params![monitor_id, stream_id, before_started_at], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The first take of this broadcast that was refused for lack of
    /// entitlement (subscriber-only / members-only): `(id, output_path)`.
    ///
    /// Being refused is a fact about the BROADCAST, not about the attempt, so
    /// once one take of a stream carries it every further automatic attempt at
    /// the same stream is refused identically. Two callers need that: the retry
    /// cadence, to stop spawning captures (each of which queues a full head
    /// backfill) for a broadcast the CDN path already owns, and the CDN session
    /// itself, which adopts the FIRST such take as its anchor so a broadcast
    /// keeps one row in the archive across restarts instead of one per attempt.
    pub fn gated_take_for_stream(
        &self,
        monitor_id: i64,
        stream_id: &str,
    ) -> Result<Option<(i64, String)>> {
        if stream_id.is_empty() {
            return Ok(None);
        }
        Ok(self
            .db()
            .query_row(
                "SELECT id, output_path FROM recording
                  WHERE monitor_id = ?1 AND stream_id = ?2 AND gated = 1
                  ORDER BY started_at LIMIT 1",
                params![monitor_id, stream_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
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
    /// Takes the Issues panel is currently reporting **because of a file**:
    /// the "needs remux" list (a `.ts` still in the capture cache) and the
    /// gap-splice failures. Returned as `(id, path)` so the caller can check
    /// each against disk.
    ///
    /// Issues is built entirely from database state — no section asks whether
    /// the file is still there — so an entry can never resolve itself once its
    /// media is swept, disposed of, or deleted by hand. On a real archive that
    /// left **177 of 465** path-bearing entries pointing at files that had not
    /// existed for weeks, burying the 210 that were genuine work.
    ///
    /// Deliberately scoped to the Issues predicates rather than "every take
    /// whose file is missing": clearing paths archive-wide on startup would
    /// rewrite thousands of rows unattended, and the question asked here is
    /// only whether what the panel *reports* is true.
    pub fn issue_paths_to_verify(&self) -> Result<Vec<(i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, output_path FROM recording
             WHERE output_path != ''
               AND ( (output_path LIKE '%.ts'
                      AND (output_path LIKE '%.cache%' OR output_path LIKE '%.sa-cache%')
                      AND status != 'ended')
                  OR gap_splice_state IN ('mismatch', 'anchor_failed', 'verify_failed') )",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Forget a gap-splice failure whose media is gone.
    ///
    /// `mismatch` / `anchor_failed` / `verify_failed` describe a splice that
    /// went wrong on a specific file. Once that file no longer exists the
    /// state is a fact about nothing — there is no retry, no repair, and no
    /// way for the row to leave the Issues panel on its own.
    pub fn clear_stale_gap_splice(&self, id: i64) -> Result<usize> {
        let conn = self.db();
        Ok(conn.execute(
            "UPDATE recording SET gap_splice_state = '' WHERE id = ?1
               AND gap_splice_state IN ('mismatch', 'anchor_failed', 'verify_failed')",
            params![id],
        )?)
    }

    /// Gap-splice failures with no `output_path` at all — the media was
    /// disposed of, so the state can only ever be historical.
    pub fn pathless_gap_splice_failures(&self) -> Result<Vec<i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id FROM recording
             WHERE COALESCE(output_path, '') = ''
               AND gap_splice_state IN ('mismatch', 'anchor_failed', 'verify_failed')",
        )?;
        let rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every companion pointer currently set, as `(recording id, which, path)`.
    ///
    /// Small by construction — 136 rows on a real 3.8k-take archive — because
    /// only takes that actually produced a head/full/recovery/VOD file have
    /// one. Cheap enough for the startup reconciler to stat every one.
    pub fn companion_media_paths(&self) -> Result<Vec<(i64, CompanionPath, String)>> {
        let conn = self.db();
        let mut out = Vec::new();
        for (which, col) in [
            (CompanionPath::Full, "full_path"),
            (CompanionPath::Backfill, "backfill_path"),
            (CompanionPath::Recovered, "recovered_path"),
            (CompanionPath::VodDl, "vod_dl_path"),
        ] {
            let sql =
                format!("SELECT id, {col} FROM recording WHERE COALESCE({col}, '') != ''");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, which, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.extend(rows);
        }
        Ok(out)
    }

    /// Forget a companion pointer whose file is gone. The history row and
    /// every other pointer are untouched — this only stops the take claiming
    /// a file that is not there.
    pub fn clear_recording_companion(&self, id: i64, which: CompanionPath) -> Result<()> {
        let conn = self.db();
        let sql = match which {
            CompanionPath::Full => "UPDATE recording SET full_path = '' WHERE id = ?1",
            CompanionPath::Backfill => "UPDATE recording SET backfill_path = '' WHERE id = ?1",
            CompanionPath::Recovered => "UPDATE recording SET recovered_path = '' WHERE id = ?1",
            CompanionPath::VodDl => "UPDATE recording SET vod_dl_path = '' WHERE id = ?1",
        };
        conn.execute(sql, params![id])?;
        Ok(())
    }

    pub fn clear_recording_backfill_path(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET backfill_path=NULL WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    // ----- historical-disposal candidate scans (see `disposal_backfill`) -----

    /// Every completed gap-splice patch that still has a path on record —
    /// `gap_range.out_path` is never cleared when a patch is later disposed
    /// (unlike `recording.backfill_path`), so this is an exact trace, not a
    /// guess. Paired with a disposal-timestamp proxy (the take's `ended_at`,
    /// falling back to `started_at` for a take that never got one) since the
    /// actual disposal time isn't recorded anywhere pre-dating the Trash view.
    pub fn gap_splice_patch_candidates(&self) -> Result<Vec<(i64, GapRangeRow, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT g.id, g.recording_id, g.start_secs, g.end_secs, g.state,
                    g.attempts, g.out_path, g.muted_segs,
                    COALESCE(r.ended_at, r.started_at)
             FROM gap_range g
             JOIN recording r ON r.id = g.recording_id
             WHERE g.state = 'done' AND g.out_path != ''",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let row = GapRangeRow {
                    id: r.get(0)?,
                    recording_id: r.get(1)?,
                    start_secs: r.get(2)?,
                    end_secs: r.get(3)?,
                    state: r.get(4)?,
                    attempts: r.get(5)?,
                    out_path: r.get(6)?,
                    muted_segs: r.get(7)?,
                };
                Ok((row.recording_id, row, r.get(8)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Recordings whose live capture was displaced by *Replace with VOD* —
    /// `(rec_id, current output_path, disposal-timestamp proxy)`. The
    /// displaced file's exact path is a deterministic transform of
    /// `output_path` (see `disposal_backfill::vod_backup_path`), following
    /// the same `.pre-vod.bak` rule `vod::` itself uses, so this is exact,
    /// not a guess — just not literally read back from a column.
    pub fn vod_replace_candidates(&self) -> Result<Vec<(i64, String, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, COALESCE(output_path, ''), COALESCE(ended_at, started_at)
             FROM recording
             WHERE vod_dl_state = 'replaced' AND output_path IS NOT NULL AND output_path != ''",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Recordings whose post-join cleanup disposed the head (and, when
    /// `output_path == full_path`, the live capture too) —
    /// `(rec_id, full_path, output_path, disposal-timestamp proxy)`.
    /// `full_path IS NOT NULL AND backfill_path IS NULL` is a clean signal
    /// that a join happened AND its head was disposed (the only two call
    /// sites that null `backfill_path` are this cleanup and "superseded old
    /// head" — see `clear_recording_backfill_path`'s callers — and a
    /// superseded recording never reaches `full_path IS NOT NULL` through
    /// its OWN join by construction). The exact disposed path(s) are only a
    /// naming-convention guess from `full_path`, though — see
    /// `disposal_backfill::head_guess_path`/`live_capture_guess_path`.
    pub fn post_join_head_disposal_candidates(&self) -> Result<Vec<(i64, String, String, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, full_path, COALESCE(output_path, ''), COALESCE(ended_at, started_at)
             FROM recording
             WHERE full_path IS NOT NULL AND full_path != '' AND backfill_path IS NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(rec_id, output_path, full_path)` for every take that has FINISHED and
    /// claims a final file — the lookup table the capture-cache sweep uses to
    /// map a leftover working-dir capture back to the archive copy that
    /// supersedes it (`Supervisor::sweep_redundant_captures`).
    ///
    /// In-flight takes are excluded by `ended_at IS NOT NULL`: their cache file
    /// IS the recording, and nothing may touch it.
    pub fn finished_takes_final_paths(&self) -> Result<Vec<(i64, String, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, COALESCE(output_path, ''), COALESCE(full_path, '')
             FROM recording
             WHERE ended_at IS NOT NULL AND COALESCE(output_path, '') != ''",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Takes that have a joined `full.mkv` but may still be carrying the parts
    /// it was built from — the input to the "Re-run join cleanup" maintenance
    /// action (`Supervisor::cmd_rerun_join_cleanup`).
    ///
    /// Returns `(rec_id, full_path, output_path, backfill_path)`. Deliberately
    /// broader than "definitely uncleaned": a take whose `output_path` already
    /// IS the full has had `Both` applied, but it may still hold a head, and a
    /// take with neither part left simply produces no work. The caller decides,
    /// because only it knows the effective per-scope cleanup setting and what
    /// is actually on disk.
    ///
    /// Exists because `join_cleanup` is applied **at join time only** — flipping
    /// it from "Keep" later does nothing for takes already joined, which is how
    /// 199 GB of redundant parts accumulated on one drive (2026-07-31).
    pub fn joined_takes_with_parts(&self) -> Result<Vec<(i64, String, String, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, full_path, COALESCE(output_path, ''), COALESCE(backfill_path, '')
             FROM recording
             WHERE full_path IS NOT NULL AND full_path != ''
               AND (COALESCE(output_path, '') != full_path OR COALESCE(backfill_path, '') != '')
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
    ///
    /// Counts only takes whose media is still on disk — the Files view renders
    /// this as "N GB in M recording(s)" beside an output directory, which is a
    /// claim about what is in that directory right now, not about how much was
    /// ever recorded into it.
    pub fn recording_stats_by_monitor(&self) -> Result<Vec<(i64, i64, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT monitor_id, COUNT(*), COALESCE(SUM(bytes), 0)
             FROM recording WHERE media_missing_at = 0 AND output_path <> ''
             GROUP BY monitor_id",
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
                "SELECT COUNT(*) FROM recording WHERE {} OR {} OR {} OR {} OR {} OR {}",
                m("output_path"),
                m("backfill_path"),
                m("full_path"),
                m("recovered_path"),
                m("vod_dl_path"),
                m("chat_path"),
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
    /// (recordings: output/backfill/full/recovered/vod-download/chat paths;
    /// videos: output path + dir; optionally monitor output dirs). This is a
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
        for col in [
            "output_path",
            "backfill_path",
            "full_path",
            "recovered_path",
            "vod_dl_path",
            "chat_path",
        ] {
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
                m.capture_offline, m.last_tags, m.last_language, r.err_ack, c.primary_group_id,
                c.posts_hidden, c.color_source
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
                    primary_group_id: r.get(64)?,
                    posts_hidden: r.get::<_, i64>(65)? != 0,
                    color_source: crate::models::PreferredAssetSource::parse(
                        &r.get::<_, String>(66)?,
                    ),
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

    /// Every distinct stream title and category ever logged against each
    /// monitor's recordings, as per-monitor lowercase `(titles, categories)`
    /// haystacks (newline-joined). Backs the Streams grid's deep filter: the
    /// values a collapsed instance's stream/take rows *would* show must still
    /// be matchable, and `rec_cache` only ever loads recordings for expanded
    /// instances. Sourced from `stream_meta_change` because that's where a
    /// recording's title/category actually live (the `Recording.title`/
    /// `.category` fields are just the latest change) — so mid-stream retitles
    /// are searchable too, same as the 📝 history that displays them.
    pub fn monitor_meta_filter_texts(
        &self,
    ) -> Result<std::collections::HashMap<i64, (String, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.monitor_id, smc.kind, smc.new_value
             FROM stream_meta_change smc
             JOIN recording r ON r.id = smc.recording_id
             WHERE smc.kind IN ('title', 'category') AND smc.new_value != ''",
        )?;
        let mut out: std::collections::HashMap<i64, (String, String)> =
            std::collections::HashMap::new();
        // Dedupe in Rust rather than GROUP_CONCAT(DISTINCT …) — SQLite's
        // DISTINCT aggregate can't take a custom separator, and the comma
        // default would let a filter straddle two adjacent values.
        let mut seen: std::collections::HashSet<(i64, bool, String)> = std::collections::HashSet::new();
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        for row in rows {
            let (mid, kind, value) = row?;
            let value = value.to_lowercase();
            let is_title = kind == "title";
            if !seen.insert((mid, is_title, value.clone())) {
                continue;
            }
            let slot = out.entry(mid).or_default();
            let s = if is_title { &mut slot.0 } else { &mut slot.1 };
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&value);
        }
        Ok(out)
    }

    /// All recording takes for a monitor (oldest first), for the history tree.
    ///
    /// Shares [`Self::RECORDING_FULL_COLUMNS`] / [`Self::map_recording_row`]
    /// with the single-row lookups: this used to carry its own byte-identical
    /// copy of both, which promptly went stale the first time a column was
    /// added (the rolling ones) — the Streams grid silently read defaults while
    /// `get_recording` read the truth.
    pub fn recordings_for_monitor(&self, monitor_id: i64) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM recording WHERE monitor_id = ?1 ORDER BY started_at, id",
            Self::RECORDING_FULL_COLUMNS
        ))?;
        let rows = stmt
            .query_map(params![monitor_id], Self::map_recording_row)?
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
            gap_splice_state, err_ack, sabr_live_edge_fallback, chapters_state,
            COALESCE(chapters_json, ''), chapters_attempts, chat_path,
            rolling_ttl_secs, rolling_from, rolling_kept_at, rolling_expired_at,
            COALESCE(not_recorded_reason, ''), gated";

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
            chapters_state: r.get(37)?,
            chapters_json: r.get(38)?,
            chapters_attempts: r.get(39)?,
            chat_path: r.get(40)?,
            rolling: crate::models::Rolling {
                ttl_secs: r.get(41)?,
                from: r.get(42)?,
                kept_at: r.get(43)?,
                expired_at: r.get(44)?,
            },
            not_recorded_reason: r.get(45)?,
            gated: r.get::<_, i64>(46)? != 0,
        })
    }

    /// A single recording by id (full row) — used by manual per-take actions
    /// (e.g. the "Backfill head" context-menu action).
    /// Every take of one broadcast `(monitor, stream_id)`, oldest first —
    /// the YouTube auto-heal's raw material for computing uncovered spans.
    pub fn takes_for_stream(
        &self,
        monitor_id: i64,
        stream_id: &str,
    ) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut st = conn.prepare(&format!(
            "SELECT {} FROM recording
             WHERE monitor_id = ?1 AND stream_id = ?2 ORDER BY started_at, id",
            Self::RECORDING_FULL_COLUMNS
        ))?;
        let rows = st
            .query_map(params![monitor_id, stream_id], Self::map_recording_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

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

    /// This monitor's currently in-progress take, if any (at most one row
    /// has `status='recording'` per monitor). Used by the live "play new
    /// instance" watch-state hook — see `crate::models::stream_key`.
    pub fn current_recording_for_monitor(
        &self,
        monitor_id: i64,
    ) -> Result<Option<crate::models::Recording>> {
        let conn = self.db();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {} FROM recording WHERE monitor_id = ?1 AND status = 'recording' \
                     ORDER BY id DESC LIMIT 1",
                    Self::RECORDING_FULL_COLUMNS
                ),
                params![monitor_id],
                Self::map_recording_row,
            )
            .optional()?;
        Ok(row)
    }

    /// `(monitor_id, recording id, started_at)` for every currently-open take
    /// (`ended_at IS NULL`) across all monitors — the scheduler's periodic
    /// consistency check cross-references this against `self.active` to
    /// catch a recurrence of the 2026-07-24 Layna incident (the in-memory
    /// map silently losing track of a still-healthy recording) the moment it
    /// happens, rather than only via its consequences days later.
    ///
    /// Excludes `status = 'not_recorded'` rows (see
    /// [`Self::insert_not_recorded_session`]): those are deliberately open
    /// with no capture behind them and never have a `self.active` entry to
    /// begin with, so they'd permanently false-positive this check instead
    /// of ever indicating a real desync.
    pub fn open_recordings_all(&self) -> Result<Vec<(i64, i64, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT monitor_id, id, started_at FROM recording
             WHERE ended_at IS NULL AND status != 'not_recorded'",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// This monitor's most recent still-open take (`ended_at IS NULL`), if
    /// any — unlike `current_recording_for_monitor` (which trusts
    /// `status='recording'` alone), this is read by the duplicate-recording
    /// safety net in `Supervisor::record`, which additionally verifies the
    /// take's own capture file is still being written to before trusting
    /// that it's genuinely still alive (a real crash also leaves `ended_at`
    /// null until finalize runs, so this alone can't distinguish the two).
    pub fn open_recording_for_monitor(
        &self,
        monitor_id: i64,
    ) -> Result<Option<crate::models::Recording>> {
        let conn = self.db();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {} FROM recording WHERE monitor_id = ?1 AND ended_at IS NULL \
                     ORDER BY started_at DESC LIMIT 1",
                    Self::RECORDING_FULL_COLUMNS
                ),
                params![monitor_id],
                Self::map_recording_row,
            )
            .optional()?;
        Ok(row)
    }

    /// All recording takes across every monitor, newest-first, capped at
    /// `limit`. Backs the Backlog/Stream History views — checkbox filtering
    /// happens client-side over this list (same convention as
    /// `list_notifications`); callers increase `limit` for "Load more".
    /// Every take belonging to a monitor that has at least one take still
    /// counting down (or kept) — the 🕰 Rolling recordings section's own set.
    ///
    /// The section used to be built by filtering Backlog's loaded page, which
    /// made it silently incomplete: the page is `recordings_all(limit)` ordered
    /// newest-first, and on a busy archive 500 rows barely covers a day, so a
    /// week-old take counting down was simply not considered. The header then
    /// reported "next in 1d 3h" while something was four minutes from deletion.
    /// A list whose whole job is to prevent surprise deletions cannot be a view
    /// of whatever happened to be paged in.
    ///
    /// It returns *every* take of those monitors, not just the rolling ones,
    /// because `group_recordings` walks takes in order and decides broadcast
    /// boundaries from gaps between neighbours: hand it a filtered subset and
    /// id-less takes group differently, and every rolled-up cell (duration,
    /// size, take count) is drawn from the wrong set. Rolling mode is a
    /// per-channel opt-in, so this is bounded by the monitors actually using
    /// it — 281 rows of 3,709 on a real archive, 1.8 ms, both sides indexed
    /// (`idx_recording_monitor` and the partial `idx_recording_rolling`).
    pub fn recordings_for_rolling(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM recording
             WHERE monitor_id IN (SELECT monitor_id FROM recording
                                  WHERE rolling_ttl_secs > 0 AND rolling_expired_at = 0)
             ORDER BY started_at DESC",
            Self::RECORDING_FULL_COLUMNS
        ))?;
        let rows = stmt
            .query_map([], Self::map_recording_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn recordings_all(&self, limit: i64) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM recording ORDER BY started_at DESC LIMIT ?1",
            Self::RECORDING_FULL_COLUMNS
        ))?;
        let rows = stmt
            .query_map(params![limit], Self::map_recording_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
               -- 'ended' is only ever set on a take that captured nothing, so
               -- its .ts is a 0-byte husk with nothing to remux. Mirrors
               -- `Recording::needs_remux` — keep the two in step.
               AND status != 'ended'
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
                    chapters_state: String::new(),
                    chapters_json: String::new(),
                    chapters_attempts: 0,
                    chat_path: String::new(),
                    rolling: crate::models::Rolling::default(),
                    not_recorded_reason: String::new(),
                    gated: false,
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
                    chapters_state: String::new(),
                    chapters_json: String::new(),
                    chapters_attempts: 0,
                    chat_path: String::new(),
                    rolling: crate::models::Rolling::default(),
                    not_recorded_reason: String::new(),
                    gated: false,
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
    ///
    /// **`log_excerpt` is deliberately not selected**, and comes back empty.
    /// It averages 8 KB per row here and peaks at 275 KB, so including it made
    /// this 500-row query move ~4 MB and take **123 ms with the store lock
    /// held** — for a question that only needs a path. Measured on the real
    /// library: 546 runs in one session, 73 seconds of held lock, more than
    /// half of ALL lock time in the app. Dropping it takes the query to a few
    /// milliseconds; the handful of rows that turn out to be missing can be
    /// re-read individually if their log is ever wanted.
    pub fn recordings_with_final_path(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status,
                    COALESCE(output_path, ''), went_live_at, went_live_approx,
                    take_group, ''
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
                    chapters_state: String::new(),
                    chapters_json: String::new(),
                    chapters_attempts: 0,
                    chat_path: String::new(),
                    rolling: crate::models::Rolling::default(),
                    not_recorded_reason: String::new(),
                    gated: false,
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
                    chapters_state: String::new(),
                    chapters_json: String::new(),
                    chapters_attempts: 0,
                    chat_path: String::new(),
                    rolling: crate::models::Rolling::default(),
                    not_recorded_reason: String::new(),
                    gated: false,
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
                    chapters_state: String::new(),
                    chapters_json: String::new(),
                    chapters_attempts: 0,
                    chat_path: String::new(),
                    rolling: crate::models::Rolling::default(),
                    not_recorded_reason: String::new(),
                    gated: false,
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
                    chapters_state: String::new(),
                    chapters_json: String::new(),
                    chapters_attempts: 0,
                    chat_path: String::new(),
                    rolling: crate::models::Rolling::default(),
                    not_recorded_reason: String::new(),
                    gated: false,
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

    /// The 🖴 column's per-monitor drive list: one entry per distinct drive,
    /// uppercased, with non-drive and pathless rows contributing nothing.
    #[test]
    fn drive_letters_by_monitor_dedups_and_ignores_non_drive_paths() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let take = |started: i64, path: &str| {
            store
                .insert_recording(mid, started, path, Some(started), false, None, None, "", "")
                .unwrap()
        };
        take(1, r"G:\streams\a.mkv");
        take(2, r"g:\streams\b.mkv"); // same drive, lowercase
        take(3, r"A:\streams\c.mkv");
        take(4, r"\\server\share\d.mkv"); // UNC — no drive letter
        take(5, ""); // never captured / already disposed of
        take(6, r"1:\weird\e.mkv"); // not a letter, so not a drive

        let map = store.drive_letters_by_monitor().unwrap();
        assert_eq!(map.get(&mid).unwrap(), &vec!['A', 'G']);

        // A monitor with nothing stored is absent rather than empty-valued.
        let mid2 = {
            let mut m2 = sample_monitor(cid);
            m2.channel_id = cid;
            m2.url = "https://twitch.tv/other".into();
            store.insert_monitor(&m2).unwrap()
        };
        assert!(!store.drive_letters_by_monitor().unwrap().contains_key(&mid2));
    }

    /// The rolling rollup must report the SOONEST deadline together with that
    /// same take's TTL (the countdown's colour divides one by the other), count
    /// still-recording takes without letting their absent deadline win, and
    /// ignore kept/expired takes entirely.
    #[test]
    fn rolling_rollup_reports_soonest_deadline_with_its_own_ttl() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let take = |started: i64, ended: Option<i64>, ttl: i64| {
            let r = store
                .insert_recording(mid, started, "C:/rec/x.mkv", Some(started), false, None, None, "", "")
                .unwrap();
            store.set_recording_rolling_ttl(r, ttl).unwrap();
            if let Some(e) = ended {
                store.finish_recording(r, e, 1, Some(0), "completed", "C:/rec/x.mkv", "").unwrap();
            }
            r
        };

        // Ends at 1_000 with a 10 h TTL -> due 37_000; the other ends later but
        // with a much shorter TTL, so IT is the soonest (11_600) — a rollup
        // that just took the oldest take, or paired one take's deadline with
        // another's TTL, would get this wrong.
        take(500, Some(1_000), 36_000);
        take(1_000, Some(2_000), 9_600);
        // Still recording: counts, but has no deadline to contribute yet.
        take(3_000, None, 3);
        // Neither of these is counting down any more.
        let kept = take(100, Some(200), 60);
        store.keep_rolling_recording(kept, 300).unwrap();
        let expired = take(100, Some(200), 60);
        store.mark_rolling_expired(expired, 300).unwrap();

        let map = store.rolling_rollup_by_monitor().unwrap();
        let r = map.get(&mid).copied().unwrap();
        assert_eq!(r.count, 3, "kept and expired takes are not counting down");
        assert_eq!(r.soonest, Some(11_600));
        assert_eq!(r.ttl_secs, 9_600, "the TTL must belong to the soonest take");
        assert_eq!(r.remaining(10_000), Some(1_600));

        // An Unkeep restarts the clock from `rolling_from`, and the query has
        // to count from there rather than from `ended_at` — same arithmetic the
        // sweep uses.
        store.unkeep_rolling_recording(kept, 100_000).unwrap();
        let r = store.rolling_rollup_by_monitor().unwrap().get(&mid).copied().unwrap();
        assert_eq!(r.count, 4);
        assert_eq!(r.soonest, Some(11_600), "the un-kept take is due much later");

        // A monitor with nothing rolling is absent rather than zero-valued.
        let mid2 = {
            let mut m2 = sample_monitor(cid);
            m2.channel_id = cid;
            m2.url = "https://twitch.tv/other".into();
            store.insert_monitor(&m2).unwrap()
        };
        assert!(!store.rolling_rollup_by_monitor().unwrap().contains_key(&mid2));
    }

    /// Every failed take must reach the 🚨 Warnings window: finish_recording
    /// files a `capture_failed` error alert — unless an error alert (🎫 PO
    /// token, tool error) already covers the take, and never for
    /// non-"failed" outcomes.
    #[test]
    fn finish_recording_files_capture_failed_alert_once() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // A failed take with a log tail -> one capture_failed error alert
        // whose last_line is the tail's last non-empty line.
        let r1 = store
            .insert_recording(mid, 1_000, "C:/rec/a.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        store
            .finish_recording(r1, 1_100, 0, Some(1), "failed", "C:/rec/a.mkv", "line one\nreal reason\n\n")
            .unwrap();
        let alerts = store.list_capture_alerts(10).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, "capture_failed");
        assert_eq!(alerts[0].severity, "error");
        assert_eq!(alerts[0].last_line, "real reason");
        assert_eq!(alerts[0].channel, "Streamer");
        assert_eq!(alerts[0].recording_id, Some(r1));

        // Already covered by an error alert (e.g. the 🎫 PO row) -> no second row.
        let r2 = store
            .insert_recording(mid, 2_000, "C:/rec/b.mkv", Some(2_000), false, Some("s2"), None, "", "")
            .unwrap();
        store
            .upsert_capture_alert(&crate::store::NewCaptureAlert {
                kind: "po_token_rejected".into(),
                severity: "error".into(),
                source: "capture".into(),
                take_key: format!("po_token:rec{r2}"),
                monitor_id: Some(mid),
                recording_id: Some(r2),
                video_id: None,
                channel: "Streamer".into(),
                count: 1,
                lost_segments: 0,
                last_line: "rejected".into(),
            })
            .unwrap();
        store.finish_recording(r2, 2_100, 0, Some(1), "failed", "C:/rec/b.mkv", "x").unwrap();
        assert_eq!(
            store.list_capture_alerts(10).unwrap().iter().filter(|a| a.recording_id == Some(r2)).count(),
            1,
            "the PO row already covers the take"
        );

        // Completed takes file nothing; a failed take with NO log falls back
        // to the exit code.
        let r3 = store
            .insert_recording(mid, 3_000, "C:/rec/c.mkv", Some(3_000), false, Some("s3"), None, "", "")
            .unwrap();
        store.finish_recording(r3, 3_100, 9, Some(0), "completed", "C:/rec/c.mkv", "").unwrap();
        assert!(store.list_capture_alerts(10).unwrap().iter().all(|a| a.recording_id != Some(r3)));
        let r4 = store
            .insert_recording(mid, 4_000, "C:/rec/d.mkv", Some(4_000), false, Some("s4"), None, "", "")
            .unwrap();
        store.finish_recording(r4, 4_100, 0, Some(3221225477), "failed", "C:/rec/d.mkv", "").unwrap();
        let a4 = store.list_capture_alerts(10).unwrap();
        let a4 = a4.iter().find(|a| a.recording_id == Some(r4)).unwrap();
        assert!(a4.last_line.contains("3221225477"), "{}", a4.last_line);

        // A failed take whose excerpt shows a PO rejection files as the 🎫
        // kind — same title as a zero-byte rejection's pre-filed row.
        let r5 = store
            .insert_recording(mid, 5_000, "C:/rec/e.mkv", Some(5_000), false, Some("s5"), None, "", "")
            .unwrap();
        store
            .finish_recording(
                r5, 5_100, 9_999, Some(1), "failed", "C:/rec/e.mkv",
                "yt_dlp.utils.DownloadError: This stream requires a GVS PO Token to continue",
            )
            .unwrap();
        let a5 = store.list_capture_alerts(10).unwrap();
        let a5 = a5.iter().find(|a| a.recording_id == Some(r5)).unwrap();
        assert_eq!(a5.kind, "po_token_rejected");
        assert_eq!(a5.severity, "error");
    }

    /// Take numbering for notification headings: 1-based within one
    /// broadcast (same stream id), ordered by start; id-less takes get None
    /// (fuzzy time-grouping must not produce a confidently wrong number).
    #[test]
    fn take_number_counts_within_one_stream_only() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let t1 = store
            .insert_recording(mid, 1_000, "C:/rec/a.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        let t2 = store
            .insert_recording(mid, 2_000, "C:/rec/b.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        // A different broadcast and an id-less take don't count in.
        let other = store
            .insert_recording(mid, 1_500, "C:/rec/c.mkv", Some(1_500), false, Some("s2"), None, "", "")
            .unwrap();
        let idless = store
            .insert_recording(mid, 1_600, "C:/rec/d.mkv", Some(1_600), false, None, None, "", "")
            .unwrap();
        assert_eq!(store.take_number(t1).unwrap(), Some(1));
        assert_eq!(store.take_number(t2).unwrap(), Some(2));
        assert_eq!(store.take_number(other).unwrap(), Some(1));
        assert_eq!(store.take_number(idless).unwrap(), None);
        assert_eq!(store.take_number(999_999).unwrap(), None);
    }

    #[test]
    fn monitor_meta_filter_texts_groups_dedupes_and_lowercases() {
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
        store.insert_meta_change(r1, 0, "title", "", "Jesse and Doogs Play:").unwrap();
        store.insert_meta_change(r1, 0, "category", "", "Lost in Tandem").unwrap();
        // The same title on a later take dedupes; a mid-take retitle adds;
        // empty values never land in the haystack.
        store.insert_meta_change(r2, 0, "title", "", "Jesse and Doogs Play:").unwrap();
        store.insert_meta_change(r2, 10, "title", "Jesse and Doogs Play:", "Patch Day!").unwrap();
        store.insert_meta_change(r2, 0, "category", "", "").unwrap();

        let texts = store.monitor_meta_filter_texts().unwrap();
        let (titles, categories) = texts.get(&mid).expect("monitor present");
        assert_eq!(
            titles.matches("jesse and doogs play:").count(),
            1,
            "deduped across takes: {titles:?}"
        );
        assert!(titles.contains("patch day!"), "mid-take retitle included: {titles:?}");
        assert_eq!(categories, "lost in tandem", "empty category dropped");

        // A monitor with no logged metadata simply has no entry.
        let m2 = {
            let mut m = sample_monitor(cid);
            m.channel_id = cid;
            store.insert_monitor(&m).unwrap()
        };
        assert!(!store.monitor_meta_filter_texts().unwrap().contains_key(&m2));
    }

    /// `monitor_disk_usage` sums finished-take bytes per monitor, excluding
    /// zero-byte and empty-`output_path` rows (a take whose only "output" was
    /// a failed VOD backfill — see `take_size_bytes`'s doc comment) — it does
    /// NOT confirm the file still exists, unlike the per-take/stream/period
    /// figure the Streams grid computes when a channel is actually expanded.
    #[test]
    fn monitor_disk_usage_sums_bytes_and_excludes_pathless_or_empty_takes() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m1 = sample_monitor(cid);
        m1.channel_id = cid;
        let mid1 = store.insert_monitor(&m1).unwrap();
        let mut m2 = sample_monitor(cid);
        m2.channel_id = cid;
        let mid2 = store.insert_monitor(&m2).unwrap();

        // mid1: two finished takes -> summed.
        let r1 = store
            .insert_recording(mid1, 1_000, "C:/rec/a.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(r1, 1_100, 1_000, Some(0), "completed", "C:/rec/a.mkv", "").unwrap();
        let r2 = store
            .insert_recording(mid1, 2_000, "C:/rec/b.mkv", Some(2_000), false, Some("s2"), None, "", "")
            .unwrap();
        store.finish_recording(r2, 2_100, 2_000, Some(0), "completed", "C:/rec/b.mkv", "").unwrap();
        // A failed VOD backfill's own recording row: bytes carried over from
        // the live capture, but no file was ever kept at any path.
        let r3 = store
            .insert_recording(mid1, 3_000, "C:/rec/c.mkv", Some(3_000), false, Some("s3"), None, "", "")
            .unwrap();
        store.finish_recording(r3, 3_100, 5_000, Some(0), "completed", "", "").unwrap();
        // A zero-byte take (never actually captured anything).
        let r4 = store
            .insert_recording(mid1, 4_000, "C:/rec/d.mkv", Some(4_000), false, Some("s4"), None, "", "")
            .unwrap();
        store.finish_recording(r4, 4_100, 0, Some(0), "completed", "C:/rec/d.mkv", "").unwrap();

        // mid2: one finished take.
        let r5 = store
            .insert_recording(mid2, 5_000, "C:/rec/e.mkv", Some(5_000), false, Some("s5"), None, "", "")
            .unwrap();
        store.finish_recording(r5, 5_100, 500, Some(0), "completed", "C:/rec/e.mkv", "").unwrap();

        let usage = store.monitor_disk_usage().unwrap();
        assert_eq!(usage.get(&mid1), Some(&3_000), "only the two real files count");
        assert_eq!(usage.get(&mid2), Some(&500));

        // A monitor with nothing finished has no entry at all (not a zero).
        let mut m3 = sample_monitor(cid);
        m3.channel_id = cid;
        let mid3 = store.insert_monitor(&m3).unwrap();
        assert!(!store.monitor_disk_usage().unwrap().contains_key(&mid3));
    }

    /// The "🔄 Rescan disk usage" action's whole point: once a take's file is
    /// confirmed gone (deleted outside the app — nothing else notices this on
    /// its own), `clear_recording_capture` is what makes `monitor_disk_usage`
    /// stop counting it, same as it already does for a take whose path was
    /// always empty.
    #[test]
    fn clear_recording_capture_removes_it_from_disk_usage() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let r1 = store
            .insert_recording(mid, 1_000, "C:/rec/a.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(r1, 1_100, 1_000, Some(0), "completed", "C:/rec/a.mkv", "").unwrap();
        let r2 = store
            .insert_recording(mid, 2_000, "C:/rec/b.mkv", Some(2_000), false, Some("s2"), None, "", "")
            .unwrap();
        store.finish_recording(r2, 2_100, 2_000, Some(0), "completed", "C:/rec/b.mkv", "").unwrap();
        assert_eq!(store.monitor_disk_usage().unwrap().get(&mid), Some(&3_000));

        // r1's file was found gone by a rescan.
        store.clear_recording_capture(r1).unwrap();
        assert_eq!(
            store.monitor_disk_usage().unwrap().get(&mid),
            Some(&2_000),
            "cleared take drops out, the other survives"
        );
        let rec = store.recordings_for_monitor(mid).unwrap();
        assert!(rec.iter().find(|r| r.id == r1).unwrap().output_path.is_empty());
    }

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
    fn open_recording_for_monitor_finds_only_the_unfinished_row() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        assert!(store.open_recording_for_monitor(mid).unwrap().is_none());

        let closed = store
            .insert_recording(mid, 1_000, "C:/rec/take1.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(closed, 2_000, 500, Some(0), "completed", "C:/rec/take1.mkv", "").unwrap();
        // Finished takes never count as "open".
        assert!(store.open_recording_for_monitor(mid).unwrap().is_none());

        let open = store
            .insert_recording(mid, 3_000, "C:/rec/.cache/take2.ts", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        let found = store.open_recording_for_monitor(mid).unwrap().unwrap();
        assert_eq!(found.id, open);
    }

    #[test]
    fn not_recorded_session_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        assert!(store.open_not_recorded_session(mid).unwrap().is_none());

        let id = store.insert_not_recorded_session(mid, 1_000, Some(1_000), false, Some("s1")).unwrap();
        let (open_id, started_at) = store.open_not_recorded_session(mid).unwrap().unwrap();
        assert_eq!(open_id, id);
        assert_eq!(started_at, 1_000);

        // The row reads back as a real take (status/stream_id/empty output_path)
        // so it slots into the normal Streams-grid listing unmodified.
        let all = store.recordings_for_monitor(mid).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, "not_recorded");
        assert_eq!(all[0].stream_id.as_deref(), Some("s1"));
        assert!(all[0].output_path.is_empty());
        assert!(all[0].ended_at.is_none());
        assert!(all[0].chat_path.is_empty(), "no chat capture attached yet");

        // A chat-only capture (see `downloader::chat_only`) attaches its
        // sidecar here — the session has no `output_path` to derive one from,
        // so this column is the only way the chat replay can find it.
        store.set_recording_chat_path(id, "C:/rec/Streamer - live.chat.jsonl").unwrap();
        let all = store.recordings_for_monitor(mid).unwrap();
        assert_eq!(all[0].chat_path, "C:/rec/Streamer - live.chat.jsonl");
        assert!(all[0].output_path.is_empty(), "still no video — this is not a capture");
        // Single-row lookups read the same column list.
        assert_eq!(
            store.get_recording(id).unwrap().unwrap().chat_path,
            "C:/rec/Streamer - live.chat.jsonl"
        );

        // Closing clears the "open" lookup and stamps ended_at.
        let closed = store.close_open_not_recorded_sessions(mid, 2_000).unwrap();
        assert_eq!(closed, vec![id]);
        assert!(store.open_not_recorded_session(mid).unwrap().is_none());
        let all = store.recordings_for_monitor(mid).unwrap();
        assert_eq!(all[0].ended_at, Some(2_000));

        // Closing again (no open session) is a harmless no-op.
        assert!(store.close_open_not_recorded_sessions(mid, 3_000).unwrap().is_empty());
    }

    #[test]
    fn not_recorded_reason_defaults_empty_and_survives_a_close() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let id = store.insert_not_recorded_session(mid, 1_000, Some(1_000), false, Some("s1")).unwrap();
        // Empty = the historical reason (Auto-record was off).
        assert_eq!(store.get_recording(id).unwrap().unwrap().not_recorded_reason, "");
        assert!(!crate::simulcast::is_simulcast_skip(""));

        let reason = "simulcast: recording this broadcast on the YouTube instance instead";
        store.set_not_recorded_reason(id, reason).unwrap();
        assert!(crate::simulcast::is_simulcast_skip(reason));
        // Both read paths agree, and closing the session doesn't wipe it — the
        // VOD-backfill guard runs *after* the close.
        store.close_open_not_recorded_sessions(mid, 2_000).unwrap();
        assert_eq!(store.get_recording(id).unwrap().unwrap().not_recorded_reason, reason);
        assert_eq!(store.recordings_for_monitor(mid).unwrap()[0].not_recorded_reason, reason);
    }

    #[test]
    fn sibling_take_covers_only_real_captures_on_other_instances() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let tw = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let mut yt_mon = sample_monitor(cid);
        yt_mon.url = "https://youtube.com/@a".into();
        let yt = store.insert_monitor(&yt_mon).unwrap();
        // A different channel entirely — must never count.
        let other_cid = store.upsert_channel("B", "https://twitch.tv/b", Platform::Twitch).unwrap();
        let other = store.insert_monitor(&sample_monitor(other_cid)).unwrap();

        let rid = store
            .insert_recording(yt, 1_000, "C:/tmp/a.mkv", None, false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(rid, 5_000, 10, Some(0), "completed", "C:/tmp/a.mkv", "").unwrap();

        assert!(store.sibling_take_covers(tw, 3_000, 0).unwrap(), "inside the sibling's take");
        assert!(!store.sibling_take_covers(tw, 9_000, 0).unwrap(), "well after it");
        assert!(store.sibling_take_covers(tw, 5_100, 300).unwrap(), "within the slack");
        assert!(!store.sibling_take_covers(yt, 3_000, 0).unwrap(), "its own take doesn't count");
        assert!(!store.sibling_take_covers(other, 3_000, 0).unwrap(), "another channel doesn't count");

        // A sibling that also didn't record covers nothing — nobody has it.
        let ghost = store.insert_not_recorded_session(yt, 20_000, Some(20_000), false, Some("s2")).unwrap();
        store.close_open_not_recorded_sessions(yt, 21_000).unwrap();
        let _ = ghost;
        assert!(!store.sibling_take_covers(tw, 20_500, 0).unwrap());
    }

    #[test]
    fn insert_discovered_not_recorded_dedups_by_stream_id_and_sets_title() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let id = store
            .insert_discovered_not_recorded(mid, 1_000, 2_000, "s1", "Discovered title")
            .unwrap();
        assert!(id.is_some());
        let rec = store.get_recording(id.unwrap()).unwrap().unwrap();
        assert_eq!(rec.status, "not_recorded");
        assert_eq!(rec.ended_at, Some(2_000));
        assert!(rec.output_path.is_empty());
        assert_eq!(rec.title, "Discovered title");

        // A repeated scan finding the same stream_id is a no-op, not a
        // duplicate row.
        let again = store.insert_discovered_not_recorded(mid, 1_000, 2_000, "s1", "Discovered title").unwrap();
        assert!(again.is_none());
        assert_eq!(store.recordings_for_monitor(mid).unwrap().len(), 1);

        // A real recorded take already covering this stream_id also blocks
        // discovery from filing a duplicate "missed" row for it.
        store
            .insert_recording(mid, 5_000, "C:/rec/take.ts", Some(5_000), false, Some("s2"), None, "", "")
            .unwrap();
        assert!(store.insert_discovered_not_recorded(mid, 5_000, 6_000, "s2", "").unwrap().is_none());
    }

    #[test]
    fn open_recordings_all_excludes_not_recorded_sessions() {
        // A `not_recorded` session (Auto off) is deliberately open with no
        // capture behind it and never has a `self.active` entry — it must
        // never appear in the scheduler's active/DB desync check, or an
        // Auto-off broadcast whose offline transition arrived via EventSub
        // (see `Supervisor::handle_offline_signal`) would false-positive it
        // forever (found live 2026-07-28: monitor 50/GEEGA, rec_id=1098).
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let not_recorded = store.insert_not_recorded_session(mid, 1_000, Some(1_000), false, Some("s1")).unwrap();
        let real_open = store
            .insert_recording(mid, 3_000, "C:/rec/.cache/take2.ts", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();

        let open: Vec<i64> = store.open_recordings_all().unwrap().into_iter().map(|(_, id, _)| id).collect();
        assert!(open.contains(&real_open));
        assert!(!open.contains(&not_recorded));
    }

    #[test]
    fn earlier_takes_for_stream_orders_oldest_first_and_excludes_later_ones() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let take1 = store
            .insert_recording(mid, 1_000, "C:/rec/take1.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(take1, 5_000, 500, Some(0), "completed", "C:/rec/take1.mkv", "").unwrap();
        let take2 = store
            .insert_recording(mid, 5_100, "C:/rec/take2.mkv", Some(1_000), false, Some("s1"), None, "", "")
            .unwrap();

        let earlier = store.earlier_takes_for_stream(mid, "s1", 5_100).unwrap();
        assert_eq!(earlier, vec![(take1, 1_000, Some(5_000), "completed".to_string())]);
        // Nothing is "earlier" than the stream's own first take.
        assert!(store.earlier_takes_for_stream(mid, "s1", 1_000).unwrap().is_empty());
        // take2 itself isn't included when querying strictly before its own start.
        assert!(!earlier.iter().any(|(id, ..)| *id == take2));
    }

    /// The anchor a suppressed retry revives the CDN session on.
    ///
    /// It must be the FIRST refused take of the broadcast, not the newest: the
    /// whole point is that one broadcast keeps one row in the archive, and
    /// anchoring on the newest attempt would create a fresh row every time the
    /// retry cadence came round — which is exactly what happened before the
    /// suppression existed.
    #[test]
    fn gated_take_for_stream_finds_the_first_refused_take_of_that_broadcast() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // An ordinary take of the same broadcast is not an anchor.
        let ok = store
            .insert_recording(mid, 900, "C:/rec/ok.mkv", Some(900), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(ok, 950, 10, Some(0), "completed", "C:/rec/ok.mkv", "").unwrap();
        assert_eq!(store.gated_take_for_stream(mid, "s1").unwrap(), None);

        let first = store
            .insert_recording(mid, 1_000, "C:/rec/a.ts", Some(900), false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_gated(first).unwrap();
        let second = store
            .insert_recording(mid, 5_000, "C:/rec/b.ts", Some(900), false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_gated(second).unwrap();

        assert_eq!(
            store.gated_take_for_stream(mid, "s1").unwrap(),
            Some((first, "C:/rec/a.ts".to_string())),
            "the oldest refused take anchors the broadcast"
        );
        // A different broadcast is a different question — a new stream may
        // well not be subscriber-only.
        assert_eq!(store.gated_take_for_stream(mid, "s2").unwrap(), None);
        // No broadcast id: nothing to key on, so never suppress.
        assert_eq!(store.gated_take_for_stream(mid, "").unwrap(), None);
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

        // A chat-only take living entirely on the relocated prefix: chat_path
        // is its ONLY pointer, so the relocation must rewrite it too.
        let chat_only = store
            .insert_recording(mid, 5_000, "", None, false, None, None, "", "")
            .unwrap();
        store
            .set_recording_chat_path(chat_only, r"A:\streams\Chan\c.chat.jsonl")
            .unwrap();

        let (r, v, mons) = store.count_path_prefix_matches(r"A:\streams").unwrap();
        assert_eq!((r, v, mons), (2, 0, 1));

        let (r, v, mons) = store.replace_path_prefix(r"A:\streams", r"D:\streams", true).unwrap();
        assert_eq!((r, v, mons), (2, 0, 1));
        assert_eq!(
            store.get_recording(chat_only).unwrap().unwrap().chat_path,
            r"D:\streams\Chan\c.chat.jsonl"
        );
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
    fn gap_splice_patch_candidates_only_sees_done_rows_with_a_path() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rec = store
            .insert_recording(mid, 100, "C:/tmp/x.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(rec, 200, 500, Some(0), "completed", "C:/tmp/x.mkv", "").unwrap();

        store.replace_pending_gap_ranges(rec, &[(10.0, 20.0), (30.0, 40.0), (50.0, 60.0)]).unwrap();
        let pending = store.gap_ranges_in_state(rec, "pending").unwrap();
        store.set_gap_range_state(pending[0].id, "done", "C:/tmp/x.gap10.mkv", 0).unwrap();
        store.set_gap_range_state(pending[1].id, "done", "", 0).unwrap(); // done but no path
        store.set_gap_range_state(pending[2].id, "failed", "C:/tmp/x.gap50.mkv", 0).unwrap();

        let candidates = store.gap_splice_patch_candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, rec);
        assert_eq!(candidates[0].1.out_path, "C:/tmp/x.gap10.mkv");
        assert_eq!(candidates[0].2, 200); // ended_at proxy
    }

    #[test]
    fn vod_replace_candidates_requires_replaced_state_and_a_path() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let replaced = store
            .insert_recording(mid, 100, "C:/tmp/live.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(replaced, 200, 500, Some(0), "completed", "C:/tmp/live.mkv", "").unwrap();
        store.set_recording_vod_archived(replaced, "C:/tmp/live.mkv", "replaced").unwrap();

        let archived_only = store
            .insert_recording(mid, 300, "C:/tmp/other.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_vod_archived(archived_only, "C:/tmp/other.vod.mkv", "archived").unwrap();

        let candidates = store.vod_replace_candidates().unwrap();
        assert_eq!(candidates, vec![(replaced, "C:/tmp/live.mkv".to_string(), 200)]);
    }

    /// The catch-up pass's input: takes that joined but may still carry parts.
    /// A take whose `output_path` already IS the full AND has no head left is
    /// fully cleaned and must not come back every run.
    #[test]
    fn joined_takes_with_parts_lists_only_takes_with_something_left() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let new = |t: i64, out: &str| {
            store.insert_recording(mid, t, out, Some(t), false, None, None, "", "").unwrap()
        };

        // Never joined — not our business.
        new(100, "C:/rec/plain.mkv");
        // Joined, nothing cleaned yet: output still the live capture.
        let uncleaned = new(200, "C:/rec/a.mkv");
        store.set_recording_full_path(uncleaned, "C:/rec/a.full.mkv").unwrap();
        store.set_recording_backfill_path(uncleaned, "C:/rec/a.head.mkv").unwrap();
        // Joined, head disposed, live capture still there (`Head` cleanup).
        let head_only = new(300, "C:/rec/b.mkv");
        store.set_recording_full_path(head_only, "C:/rec/b.full.mkv").unwrap();
        // Fully cleaned (`Both`): re-pointed at the full, no head.
        let done = new(400, "C:/rec/c.full.mkv");
        store.set_recording_full_path(done, "C:/rec/c.full.mkv").unwrap();
        // Re-pointed at the full but a head somehow survived — still work to do.
        let stray_head = new(500, "C:/rec/d.full.mkv");
        store.set_recording_full_path(stray_head, "C:/rec/d.full.mkv").unwrap();
        store.set_recording_backfill_path(stray_head, "C:/rec/d.head.mkv").unwrap();

        let ids: Vec<i64> =
            store.joined_takes_with_parts().unwrap().into_iter().map(|(id, ..)| id).collect();
        assert_eq!(ids, vec![uncleaned, head_only, stray_head]);
        assert!(!ids.contains(&done), "an already-cleaned take must not be revisited");
    }

    /// The cache sweep's lookup only offers FINISHED takes — an in-flight
    /// take's working-dir file IS the recording and must never be a candidate.
    #[test]
    fn finished_takes_final_paths_excludes_in_flight() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let live = store
            .insert_recording(mid, 100, "C:/rec/live.mkv", Some(50), false, None, None, "", "")
            .unwrap();
        let done = store
            .insert_recording(mid, 200, "C:/rec/done.mkv", Some(150), false, None, None, "", "")
            .unwrap();
        store
            .finish_recording(done, 900, 123, Some(0), "completed", "C:/rec/done.mkv", "")
            .unwrap();

        let rows = store.finished_takes_final_paths().unwrap();
        let ids: Vec<i64> = rows.iter().map(|(id, ..)| *id).collect();
        assert_eq!(ids, vec![done]);
        assert!(!ids.contains(&live), "an open take is never superseded by anything");
    }

    #[test]
    fn post_join_head_disposal_candidates_needs_full_path_and_null_backfill() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();

        // Head disposed post-join: full_path set, backfill_path cleared.
        let disposed = store
            .insert_recording(mid, 100, "C:/tmp/x.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(disposed, 200, 500, Some(0), "completed", "C:/tmp/x.mkv", "").unwrap();
        store.set_recording_full_path(disposed, "C:/tmp/x.full.mkv").unwrap();

        // Head kept (cleanup = Keep): full_path set, backfill_path still there.
        let kept = store
            .insert_recording(mid, 300, "C:/tmp/y.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_backfill_path(kept, "C:/tmp/y.head.mkv").unwrap();
        store.set_recording_full_path(kept, "C:/tmp/y.full.mkv").unwrap();

        // Never joined at all: no signal either way.
        let _never_joined = store
            .insert_recording(mid, 500, "C:/tmp/z.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();

        let candidates = store.post_join_head_disposal_candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, disposed);
        assert_eq!(candidates[0].1, "C:/tmp/x.full.mkv");
    }

    #[test]
    fn record_chapters_failure_requeues_until_attempts_exhausted() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rec = store
            .insert_recording(mid, 100, "C:/tmp/x.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        // Finished take: the hourly retry sweeps
        // `recordings_needing_chapters_check`, whose candidates must be
        // completed — a fresh '' take is eligible from the moment it ends.
        store.finish_recording(rec, 160, 1, Some(0), "completed", "C:/tmp/x.mkv", "").unwrap();
        assert_eq!(store.recordings_needing_chapters_check().unwrap(), vec![rec]);

        // First few failures requeue (not yet at the cap) — still a sweep
        // candidate.
        store.record_chapters_failure(rec, 1, false).unwrap();
        let row = store.get_recording(rec).unwrap().unwrap();
        assert_eq!(row.chapters_state, "queued");
        assert_eq!(row.chapters_attempts, 1);
        assert_eq!(store.recordings_needing_chapters_check().unwrap(), vec![rec]);

        // The caller decides "exhausted" (mirrors gap-recovery's own
        // attempts-vs-cap check) — once true, it's terminal and the sweep
        // leaves it alone.
        store.record_chapters_failure(rec, 5, true).unwrap();
        let row = store.get_recording(rec).unwrap().unwrap();
        assert_eq!(row.chapters_state, "failed");
        assert_eq!(row.chapters_attempts, 5);
        assert!(store.recordings_needing_chapters_check().unwrap().is_empty());

        // A manual reset (retrigger / bulk re-embed) always zeroes the
        // counter along with the state, regardless of how it got here.
        store.set_chapters_state(rec, "").unwrap();
        let row = store.get_recording(rec).unwrap().unwrap();
        assert_eq!(row.chapters_state, "");
        assert_eq!(row.chapters_attempts, 0);
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
    fn chat_scan_queue_only_offers_finished_takes_with_a_sidecar() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://youtube.com/@a", Platform::YouTube).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let take = |n: i64, chat: Option<&str>, finished: bool| {
            let rid = store
                .insert_recording(mid, n, &format!("C:/tmp/{n}.mkv"), None, false, Some("s1"), None, "", "")
                .unwrap();
            if let Some(p) = chat {
                store.set_recording_chat_path(rid, p).unwrap();
            }
            if finished {
                store
                    .finish_recording(rid, n + 10, 1, Some(0), "completed", &format!("C:/tmp/{n}.mkv"), "")
                    .unwrap();
            }
            rid
        };
        let ready = take(100, Some("C:/tmp/100.live_chat.json"), true);
        let _no_sidecar = take(200, None, true);
        let _still_recording = take(300, Some("C:/tmp/300.live_chat.json"), false);

        let queue = store.recordings_needing_chat_scan(10).unwrap();
        assert_eq!(queue.iter().map(|t| t.rec_id).collect::<Vec<_>>(), vec![ready]);
        let t = &queue[0];
        assert_eq!((t.monitor_id, t.stream_id.as_str(), t.started_at), (mid, "s1", 100));
        assert_eq!(t.chat_path, "C:/tmp/100.live_chat.json");

        // Once stamped it never comes back — that's what drains the queue.
        store.set_recording_chat_scanned(ready, 9_999).unwrap();
        assert!(store.recordings_needing_chat_scan(10).unwrap().is_empty());
    }

    /// Both chat-index queries join `monitor`, and one of them wants the
    /// instance's platform — which is **not** a column (it is derived from the
    /// URL). Exercising them against a real schema is what catches that; a
    /// query referencing a column that doesn't exist compiles perfectly well.
    #[test]
    fn chat_index_candidates_and_labels_join_real_columns() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", None, false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_chat_path(rid, "C:/tmp/a.chat.jsonl").unwrap();
        // Unfinished takes are excluded: a live sidecar is still being written.
        let live = store
            .insert_recording(mid, 200, "C:/tmp/b.mkv", None, false, Some("s2"), None, "", "")
            .unwrap();
        store.set_recording_chat_path(live, "C:/tmp/b.chat.jsonl").unwrap();
        store.finish_recording(rid, 110, 1, Some(0), "completed", "C:/tmp/a.mkv", "").unwrap();

        let cands = store.chat_index_candidates().unwrap();
        assert_eq!(cands.iter().map(|c| c.rec_id).collect::<Vec<_>>(), vec![rid]);
        let c = &cands[0];
        assert_eq!((c.monitor_id, c.channel_id, c.started_at), (mid, cid, 100));
        assert_eq!(c.chat_path, "C:/tmp/a.chat.jsonl");
        assert_eq!(crate::models::Platform::detect(&c.url), Platform::Twitch);

        let labels = store.take_labels(&[rid, 9_999]).unwrap();
        assert_eq!(labels.len(), 1, "a missing id is simply absent, not an error");
        let l = &labels[&rid];
        assert_eq!((l.channel.as_str(), l.monitor_id, l.started_at), ("A", mid, 100));
        assert_eq!(l.platform, Platform::Twitch.as_str());
        // Empty input must not build a `WHERE id IN ()`, which is a syntax error.
        assert!(store.take_labels(&[]).unwrap().is_empty());
    }

    #[test]
    fn expired_rolling_recordings_excludes_everything_that_isnt_actually_due() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        const TTL: i64 = 3600;
        const ENDED: i64 = 10_000;
        let now = ENDED + TTL + 1; // one second past every deadline below

        // Helper: a finished take with a file, optionally rolling.
        let take = |n: i64, ttl: i64| {
            let rid = store
                .insert_recording(mid, n, &format!("C:/tmp/{n}.mkv"), None, false, None, None, "", "")
                .unwrap();
            store.finish_recording(rid, ENDED, 1, Some(0), "completed", &format!("C:/tmp/{n}.mkv"), "").unwrap();
            if ttl > 0 {
                store.set_recording_rolling_ttl(rid, ttl).unwrap();
            }
            rid
        };

        let due = take(1, TTL);
        let not_rolling = take(2, 0);
        let kept = take(3, TTL);
        store.keep_rolling_recording(kept, now).unwrap();
        let already_swept = take(4, TTL);
        store.mark_rolling_expired(already_swept, now).unwrap();
        // No file left — still due. It has already reached the end state the
        // countdown produces, and the sweep is the only thing that can stamp
        // it; excluding it here is what made these rows read "due" for ever.
        let no_file = take(5, TTL);
        store.update_recording_output_path(no_file, "", RepointBytes::Unchanged).unwrap();
        // Still recording: no `ended_at`, so the clock hasn't started.
        let recording = store
            .insert_recording(mid, 6, "C:/tmp/6.mkv", None, false, None, None, "", "")
            .unwrap();
        store.set_recording_rolling_ttl(recording, TTL).unwrap();

        let got: Vec<i64> =
            store.expired_rolling_recordings(now).unwrap().into_iter().map(|r| r.rec_id).collect();
        assert_eq!(got, vec![due, no_file], "the due take and the one with nothing left to delete");
        let _ = (not_rolling, kept, already_swept, recording);

        // One second before the deadline it isn't due yet.
        assert!(store.expired_rolling_recordings(ENDED + TTL - 1).unwrap().is_empty());

        // The due row carries what `dispose_media` needs, no second lookup.
        let row = &store.expired_rolling_recordings(now).unwrap()[0];
        assert_eq!((row.monitor_id, row.channel_id), (mid, cid));
        assert_eq!(row.output_path, "C:/tmp/1.mkv");
        // The sweep tells the two apart on this alone, so it has to survive.
        assert_eq!(store.expired_rolling_recordings(now).unwrap()[1].output_path, "");
    }

    /// The Issues panel reports from database state alone, so its entries
    /// have to be verifiable against disk from outside. This pins WHICH rows
    /// the verifier is handed: the ones the panel is showing because of a
    /// file, and no others — a verifier scoped to "every take with a missing
    /// file" would rewrite thousands of unrelated rows on startup.
    /// Re-pointing a take must say what its size becomes, because the two
    /// kinds of re-point disagree: a relocation keeps the size, a substitution
    /// replaces it. Conflating them under-reported 412 GB across 66 takes.
    #[test]
    fn repointing_a_take_keeps_or_replaces_its_size_as_the_caller_states() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, r"G:\streams\c\a.mkv", None, false, None, None, "", "")
            .unwrap();
        store
            .finish_recording(rid, 110, 3_000_000_000, Some(0), "completed", r"G:\streams\c\a.mkv", "")
            .unwrap();
        let size = |id: i64| store.get_recording(id).unwrap().unwrap().bytes;
        assert_eq!(size(rid), 3_000_000_000);

        // A drive move: same file, new address. The size must survive.
        store
            .update_recording_output_path(rid, r"P:\streams\c\a.mkv", RepointBytes::Unchanged)
            .unwrap();
        assert_eq!(size(rid), 3_000_000_000, "a relocation must not resize the take");

        // A head+live join: a different, larger file now backs the take.
        store
            .update_recording_output_path(
                rid,
                r"P:\streams\c\a.full.mkv",
                RepointBytes::Measured(18_000_000_000),
            )
            .unwrap();
        assert_eq!(size(rid), 18_000_000_000, "a substitution must adopt the new file's size");
    }

    /// The repair sweep's candidate list is what the reconciler stats, so it
    /// has to carry the recorded size to compare against — and it must not
    /// offer rows with no file to measure.
    #[test]
    fn the_size_sweep_lists_archived_takes_with_their_recorded_size() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let mk = |n: i64, path: &str, bytes: i64, status: &str| -> i64 {
            let rid = store
                .insert_recording(mid, n, path, None, false, None, None, "", "")
                .unwrap();
            store.finish_recording(rid, n + 10, bytes, Some(0), status, path, "").unwrap();
            rid
        };
        let joined = mk(100, r"G:\streams\c\a.full.mkv", 1_000, "completed");
        let pathless = mk(200, "", 5_000, "completed");
        store.update_recording_output_path(pathless, "", RepointBytes::Unchanged).unwrap();

        let rows = store.takes_with_media_on_disk().unwrap();
        assert!(
            rows.iter().any(|r| r.id == joined && r.path.ends_with("a.full.mkv") && r.bytes == 1_000),
            "an archived take is offered with the size to compare against"
        );
        assert!(
            !rows.iter().any(|r| r.id == pathless),
            "a take with no path has nothing to stat"
        );

        store.set_recording_bytes(joined, 18_000_000_000).unwrap();
        assert_eq!(store.get_recording(joined).unwrap().unwrap().bytes, 18_000_000_000);
    }

    /// `bytes` says how big a take was; it has never said whether the media is
    /// still there, so every space-in-use total counted files that had been
    /// deleted — 748 GB of them on one archive. The stamp is what lets SQL ask
    /// a question it cannot answer by statting.
    #[test]
    fn media_that_is_gone_stops_counting_as_disk_usage() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let mk = |n: i64, path: &str, bytes: i64| -> i64 {
            let rid = store
                .insert_recording(mid, n, path, None, false, None, None, "", "")
                .unwrap();
            store.finish_recording(rid, n + 10, bytes, Some(0), "completed", path, "").unwrap();
            rid
        };
        let here = mk(100, r"G:\streams\c\here.mkv", 3_000);
        let gone = mk(200, r"G:\streams\c\gone.mkv", 7_000);
        let usage = |store: &Store| store.monitor_disk_usage().unwrap().get(&mid).copied().unwrap_or(0);
        assert_eq!(usage(&store), 10_000, "both count while both are present");

        assert!(store.set_recording_media_missing(gone, 1_800_000_000).unwrap());
        assert_eq!(usage(&store), 3_000, "a deleted file is not disk usage");
        // `bytes` survives: how big it WAS is still a true and useful answer.
        assert_eq!(store.get_recording(gone).unwrap().unwrap().bytes, 7_000);

        // Idempotent, so a sweep over a settled archive writes nothing.
        assert!(!store.set_recording_media_missing(gone, 1_800_000_001).unwrap());

        // And it reverses: a remounted drive puts the media back in the totals.
        assert!(store.set_recording_media_missing(gone, 0).unwrap());
        assert_eq!(usage(&store), 10_000);
        assert_ne!(here, gone);

        // The OTHER way media leaves: the app disposes of it and clears the
        // path, keeping `bytes` as the historical size. The startup sweep can
        // never stamp a pathless row (there is no file to stat), so the
        // queries must exclude them on their own — 217 disposed takes were
        // still counted as 2,265 GB of disk usage before this guard.
        store.update_recording_output_path(gone, "", RepointBytes::Unchanged).unwrap();
        assert_eq!(usage(&store), 3_000, "a disposed take is not disk usage");
        assert_eq!(
            store.get_recording(gone).unwrap().unwrap().bytes,
            7_000,
            "its historical size still survives"
        );
        let g = store.global_stats().unwrap();
        assert_eq!(g.total_bytes, 3_000, "Total on disk excludes disposed takes too");
    }

    #[test]
    fn only_file_backed_issue_entries_are_offered_for_verification() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let mk = |n: i64, path: &str, status: &str| -> i64 {
            let rid = store
                .insert_recording(mid, n, path, None, false, None, None, "", "")
                .unwrap();
            store.finish_recording(rid, n + 10, 1, Some(0), status, path, "").unwrap();
            rid
        };

        let needs_remux = mk(100, r"G:\streams\.sa-cache\c\a.ts", "completed");
        // 'ended' takes captured nothing: their .ts is a husk, not remux work.
        let ended_husk = mk(200, r"G:\streams\.sa-cache\c\b.ts", "ended");
        // An ordinary archived take is not an issue and must not be offered.
        let archived = mk(300, r"G:\streams\c\c.mkv", "completed");
        let splice = mk(400, r"G:\streams\c\d.mkv", "completed");
        store.set_gap_splice_state(splice, "anchor_failed").unwrap();

        let offered: Vec<i64> =
            store.issue_paths_to_verify().unwrap().into_iter().map(|(id, _)| id).collect();
        assert!(offered.contains(&needs_remux), "a cached .ts is remux work");
        assert!(offered.contains(&splice), "a splice failure names a file");
        assert!(!offered.contains(&ended_husk), "'ended' is not remux work");
        assert!(!offered.contains(&archived), "an ordinary take is not an issue");

        // Clearing a splice verdict is scoped to failure states, so a healthy
        // or in-progress splice can never be wiped by the repair.
        assert_eq!(store.clear_stale_gap_splice(splice).unwrap(), 1);
        assert_eq!(store.clear_stale_gap_splice(splice).unwrap(), 0, "idempotent");
        store.set_gap_splice_state(archived, "done").unwrap();
        assert_eq!(store.clear_stale_gap_splice(archived).unwrap(), 0, "'done' is untouched");
    }

    /// A splice failure whose take has no media at all can only be historical,
    /// and nothing else will ever clear it.
    #[test]
    fn pathless_splice_failures_are_found_regardless_of_status() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", None, false, None, None, "", "")
            .unwrap();
        store.finish_recording(rid, 200, 1, Some(0), "completed", "", "").unwrap();
        store.set_gap_splice_state(rid, "verify_failed").unwrap();
        assert_eq!(store.pathless_gap_splice_failures().unwrap(), vec![rid]);
        // ...and it is NOT offered to the disk verifier, which needs a path.
        assert!(store.issue_paths_to_verify().unwrap().is_empty());
    }

    /// Companion pointers are enumerated and cleared independently of each
    /// other and of `output_path`, because they are separate files that can
    /// disappear separately. Clearing one must not disturb the rest — a
    /// blanket "this take's media is gone" would drop pointers to files that
    /// are still there, which is the opposite mistake and the harder one to
    /// notice.
    #[test]
    fn companion_pointers_are_listed_and_cleared_one_at_a_time() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", None, false, None, None, "", "")
            .unwrap();
        store.finish_recording(rid, 200, 1, Some(0), "completed", "C:/tmp/a.mkv", "").unwrap();
        store.set_recording_full_path(rid, "C:/tmp/a.full.mkv").unwrap();
        store.set_recording_recovered(rid, "C:/tmp/a.rec.mkv", "done").unwrap();

        let mut got = store.companion_media_paths().unwrap();
        got.sort_by_key(|(_, w, _)| format!("{w:?}"));
        assert_eq!(got.len(), 2, "only the two that are set");
        assert!(got.iter().any(|(_, w, p)| *w == CompanionPath::Full && p == "C:/tmp/a.full.mkv"));
        assert!(
            got.iter().any(|(_, w, p)| *w == CompanionPath::Recovered && p == "C:/tmp/a.rec.mkv")
        );

        store.clear_recording_companion(rid, CompanionPath::Full).unwrap();
        let left = store.companion_media_paths().unwrap();
        assert_eq!(left.len(), 1, "the other pointer survives");
        assert_eq!(left[0].1, CompanionPath::Recovered);
        // And the take's own media pointer is untouched by all of this.
        let r = store.get_recording(rid).unwrap().unwrap();
        assert_eq!(r.output_path, "C:/tmp/a.mkv");
    }

    /// Deleting a rolling take's file by hand ends its countdown there and
    /// then, instead of leaving a row the sweep can't reach and the badge
    /// still counts. This is how the real ghosts were made: three Zentreya
    /// takes manually deleted on 08-16 kept counting down afterwards.
    #[test]
    fn clearing_a_takes_media_ends_its_countdown() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let mk = |n: i64| {
            let rid = store
                .insert_recording(mid, n, "C:/tmp/a.mkv", None, false, None, None, "", "")
                .unwrap();
            store.finish_recording(rid, n + 100, 1, Some(0), "completed", "C:/tmp/a.mkv", "").unwrap();
            store.set_recording_rolling_ttl(rid, 3600).unwrap();
            rid
        };

        let counting = mk(1);
        store.clear_recording_media(counting, 555).unwrap();
        let r = store.get_recording(counting).unwrap().unwrap();
        assert_eq!(r.output_path, "");
        assert_eq!(r.rolling.expired_at, 555, "the countdown ended with the file");
        // And it is gone from both the sweep's set and the badge's.
        assert!(store.expired_rolling_recordings(999_999).unwrap().is_empty());
        assert!(store.recordings_for_rolling().unwrap().is_empty());

        // A kept take has no countdown to end; stamping it would relabel a
        // deliberate Keep as an expiry.
        let kept = mk(1_000);
        store.keep_rolling_recording(kept, 10).unwrap();
        store.clear_recording_media(kept, 555).unwrap();
        let r = store.get_recording(kept).unwrap().unwrap();
        assert_eq!(r.output_path, "");
        assert_eq!((r.rolling.kept_at, r.rolling.expired_at), (10, 0));

        // A take that was never rolling is only ever a path clear.
        let plain = store
            .insert_recording(mid, 2_000, "C:/tmp/b.mkv", None, false, None, None, "", "")
            .unwrap();
        store.clear_recording_media(plain, 555).unwrap();
        let r = store.get_recording(plain).unwrap().unwrap();
        assert_eq!((r.output_path.as_str(), r.rolling.expired_at), ("", 0));
    }

    /// The 🕰 Rolling recordings section must see a counting-down take that
    /// the Backlog page does not.
    ///
    /// This is the pagination half of the same evening's bug. `recordings_all`
    /// is newest-first and capped, so on a busy archive an older rolling take
    /// falls off the page — and the section, which was filtering that page,
    /// reported "next in 1d 3h" while a take was four minutes from deletion.
    /// The test makes the page cap deliberately too small and asserts the two
    /// sets disagree in exactly that way.
    #[test]
    fn the_rolling_section_sees_takes_the_backlog_page_does_not() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        const TTL: i64 = 3600;

        // One old rolling take, then newer ordinary ones that crowd it off a
        // small page — the shape of a week-old countdown under a day of volume.
        let old_rolling = store
            .insert_recording(mid, 100, "C:/tmp/old.mkv", None, false, None, None, "", "")
            .unwrap();
        store
            .finish_recording(old_rolling, 200, 1, Some(0), "completed", "C:/tmp/old.mkv", "")
            .unwrap();
        store.set_recording_rolling_ttl(old_rolling, TTL).unwrap();
        for n in 0..5 {
            let rid = store
                .insert_recording(mid, 1_000 + n * 10, "C:/tmp/n.mkv", None, false, None, None, "", "")
                .unwrap();
            store
                .finish_recording(rid, 1_005 + n * 10, 1, Some(0), "completed", "C:/tmp/n.mkv", "")
                .unwrap();
        }

        let page: Vec<i64> = store.recordings_all(3).unwrap().into_iter().map(|r| r.id).collect();
        assert!(!page.contains(&old_rolling), "the page must have crowded it out");

        let section: Vec<i64> =
            store.recordings_for_rolling().unwrap().into_iter().map(|r| r.id).collect();
        assert!(section.contains(&old_rolling), "the section must still see it");
        // Every take of the monitor, not just the rolling one: `group_recordings`
        // decides broadcast boundaries from the gaps between neighbouring takes,
        // so a filtered subset would group them differently.
        assert_eq!(section.len(), 6, "all takes of a rolling monitor, not only the rolling ones");
    }

    /// A monitor with nothing rolling contributes nothing — the section's query
    /// must not quietly become "the whole recording table".
    #[test]
    fn the_rolling_section_query_ignores_monitors_with_nothing_counting() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let rolling_mon = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let plain_mon = store.insert_monitor(&sample_monitor(cid)).unwrap();
        for mid in [rolling_mon, plain_mon] {
            let rid = store
                .insert_recording(mid, 100, "C:/tmp/a.mkv", None, false, None, None, "", "")
                .unwrap();
            store
                .finish_recording(rid, 200, 1, Some(0), "completed", "C:/tmp/a.mkv", "")
                .unwrap();
            if mid == rolling_mon {
                store.set_recording_rolling_ttl(rid, 3600).unwrap();
            }
        }
        let mons: Vec<i64> =
            store.recordings_for_rolling().unwrap().into_iter().map(|r| r.monitor_id).collect();
        assert_eq!(mons, vec![rolling_mon]);

        // An already-swept take is not "counting down", so its monitor drops
        // out too once nothing else there is rolling.
        let swept = store.recordings_for_rolling().unwrap()[0].id;
        store.mark_rolling_expired(swept, 999).unwrap();
        assert!(store.recordings_for_rolling().unwrap().is_empty());
    }

    /// The bug this pins, stated as an invariant: **anything the countdown
    /// badge shows as due must be something the sweep can reach.**
    ///
    /// `rolling_rollup_by_monitor` drives the `🕰 N (due)` badge and
    /// `expired_rolling_recordings` drives the deletion. They are separate SQL
    /// and they drifted: the sweep excluded takes with an empty `output_path`,
    /// the rollup did not, so one such take dragged its whole channel's MIN()
    /// into the past and pinned it at "due" with nothing able to clear it.
    /// Zentreya sat at `🕰 38 (due)` for a week on the strength of one row.
    #[test]
    fn every_counting_take_is_reachable_by_the_sweep() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        const TTL: i64 = 3600;
        const ENDED: i64 = 10_000;
        let now = ENDED + TTL + 1;

        // Every shape a rolling take can be in, including the ones that drifted.
        let mut rolling = Vec::new();
        for (n, path, end, unkeep) in [
            (1_i64, "C:/tmp/1.mkv", true, false), // ordinary, due
            (2, "", true, false),                 // media already gone
            (3, "C:/tmp/3.mkv", false, false),    // still recording
            (4, "C:/tmp/4.mkv", false, true),     // still recording, un-kept
        ] {
            let rid = store
                .insert_recording(mid, n, "C:/tmp/x.mkv", None, false, None, None, "", "")
                .unwrap();
            if end {
                store
                    .finish_recording(rid, ENDED, 1, Some(0), "completed", path, "")
                    .unwrap();
            }
            store.set_recording_rolling_ttl(rid, TTL).unwrap();
            if unkeep {
                // Unkeep stamps `rolling_from`, the one way a take can have a
                // clock start that isn't `ended_at`.
                store.unkeep_rolling_recording(rid, ENDED - TTL).unwrap();
            }
            store.update_recording_output_path(rid, path, RepointBytes::Unchanged).unwrap();
            rolling.push(rid);
        }

        let rollup = store.rolling_rollup_by_monitor().unwrap();
        let got = rollup.get(&mid).expect("the monitor has rolling takes");
        assert_eq!(got.count, rolling.len() as i64, "every rolling take is counted");

        let due: Vec<i64> =
            store.expired_rolling_recordings(now).unwrap().into_iter().map(|r| r.rec_id).collect();
        // The badge says "due" exactly when the soonest deadline has passed —
        // so when it does, the sweep must have something to work with.
        let soonest = got.soonest.expect("two of them have ended, so there is a deadline");
        assert!(soonest <= now, "the badge would read (due)");
        assert!(!due.is_empty(), "...so the sweep must be able to clear it");
        assert!(due.contains(&rolling[1]), "including the one with no file left");
        // A take that never ended has no deadline to count down to, whichever
        // way its clock was started — the sweep can't touch it either.
        assert!(!due.contains(&rolling[2]) && !due.contains(&rolling[3]));
    }

    #[test]
    fn unkeep_restarts_the_countdown_so_an_old_take_isnt_instantly_due() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        const TTL: i64 = 3600;
        let rid = store
            .insert_recording(mid, 1, "C:/tmp/a.mkv", None, false, None, None, "", "")
            .unwrap();
        store.finish_recording(rid, 100, 1, Some(0), "completed", "C:/tmp/a.mkv", "").unwrap();
        store.set_recording_rolling_ttl(rid, TTL).unwrap();

        let now = 1_000_000; // aeons past `ended_at + TTL`
        assert_eq!(store.expired_rolling_recordings(now).unwrap().len(), 1);

        // Keeping takes it out of the sweep entirely…
        store.keep_rolling_recording(rid, now).unwrap();
        assert!(store.expired_rolling_recordings(now).unwrap().is_empty());

        // …and un-keeping puts it back with a FULL fresh TTL, not instantly due.
        store.unkeep_rolling_recording(rid, now).unwrap();
        assert!(
            store.expired_rolling_recordings(now).unwrap().is_empty(),
            "unkeep must restart the clock, not resume an already-elapsed one"
        );
        assert_eq!(store.expired_rolling_recordings(now + TTL).unwrap().len(), 1);

        // An expired take can no longer be kept or un-kept back into the pool.
        store.mark_rolling_expired(rid, now + TTL).unwrap();
        store.unkeep_rolling_recording(rid, now + TTL).unwrap();
        assert!(store.expired_rolling_recordings(now + TTL * 10).unwrap().is_empty());
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
