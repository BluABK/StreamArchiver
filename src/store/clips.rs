//! Clip catalogue rows (Clips view), plus the per-instance sweep cursor and the
//! VOD→CDN folder cache that makes recovering a vanished clip cheap.
//!
//! The store API is written complete here, ahead of the discovery/download/
//! recovery phases that call it — same "kept API-complete even where no call
//! site exists yet" allowance `iomon::fs` uses. Drop this `allow` once the
//! Clips view and sweeps have landed; anything still unused then really is dead.
#![allow(dead_code)]

use super::*;

/// Per-instance clip-sweep bookkeeping (schema v95 `clip_sweep`): the
/// incremental high-water mark and the resumable newest-first backfill cursor.
#[derive(Clone, Debug, Default)]
pub struct ClipSweepState {
    pub monitor_id: i64,
    /// Newest `created_at` we have swept up to. Only advanced when a window set
    /// drained cleanly — a partial sweep must never move it, or the gap it
    /// leaves is permanent.
    pub last_swept_at: i64,
    /// History walked back to this `created_at` (0 = backfill never started).
    pub backfill_until: i64,
    pub backfill_done: bool,
    pub last_error: String,
}

/// A VOD's resolved CDN location (schema v95 `vod_cdn`), captured while the VOD
/// is still alive so a later recovery needs no host probing at all.
#[derive(Clone, Debug, Default)]
pub struct VodCdnRow {
    pub vod_id: String,
    pub host: String,
    pub folder: String,
    pub login: String,
    pub broadcast_id: String,
    pub start_epoch: i64,
    pub learned_at: i64,
}

impl Store {
    // ----- clips -----

    /// Insert a clip, or refresh an existing one in place.
    ///
    /// Volatile fields (title, view count, game, thumbnail, url) are always
    /// refreshed. The **recovery keys are never destroyed**: `vod_id`,
    /// `vod_offset_secs` and `broadcaster_login` only move from empty to
    /// non-empty, never the other way. That asymmetry is the whole point — see
    /// the v95 migration comment: Twitch nulls `video_id`/`vod_offset` once the
    /// parent VOD expires, so a re-sight of a year-old clip carries blanks, and
    /// a naive upsert would erase the keys the first sweep captured and make the
    /// clip permanently unrecoverable.
    ///
    /// Local state (`state`, `output_path`, `bytes`, `recovery_method`,
    /// `dl_video_id`, …) is likewise left alone: a sweep reports what the
    /// platform says, never what we have done with it.
    pub fn upsert_clip(&self, c: &Clip, now: i64) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO clip(
                 platform, slug, channel_id, monitor_id, broadcaster_id, broadcaster_login,
                 creator_login, title, game, language, view_count, duration_ms, created_at,
                 url, thumbnail_url, vod_id, vod_offset_secs, keys_captured_at, recording_id,
                 state, source, first_seen_at, last_seen_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                    CASE WHEN ?16 <> '' AND ?17 IS NOT NULL THEN ?18 ELSE 0 END,
                    ?19, ?20, ?21, ?18, ?18)
             ON CONFLICT(platform, slug) DO UPDATE SET
                 -- volatile: always refreshed
                 title             = excluded.title,
                 game              = excluded.game,
                 language          = excluded.language,
                 view_count        = excluded.view_count,
                 thumbnail_url     = excluded.thumbnail_url,
                 url               = CASE WHEN excluded.url <> '' THEN excluded.url ELSE clip.url END,
                 last_seen_at      = excluded.last_seen_at,
                 -- a clip we can see again is, by definition, not gone
                 gone_at           = 0,
                 -- scoping: fill in, never blank out
                 channel_id        = COALESCE(excluded.channel_id, clip.channel_id),
                 monitor_id        = COALESCE(excluded.monitor_id, clip.monitor_id),
                 recording_id      = COALESCE(excluded.recording_id, clip.recording_id),
                 broadcaster_id    = CASE WHEN excluded.broadcaster_id <> ''
                                          THEN excluded.broadcaster_id ELSE clip.broadcaster_id END,
                 creator_login     = CASE WHEN excluded.creator_login <> ''
                                          THEN excluded.creator_login ELSE clip.creator_login END,
                 duration_ms       = CASE WHEN excluded.duration_ms > 0
                                          THEN excluded.duration_ms ELSE clip.duration_ms END,
                 created_at        = CASE WHEN excluded.created_at > 0
                                          THEN excluded.created_at ELSE clip.created_at END,
                 -- RECOVERY KEYS: one-way, empty -> non-empty only
                 broadcaster_login = CASE WHEN excluded.broadcaster_login <> ''
                                          THEN excluded.broadcaster_login
                                          ELSE clip.broadcaster_login END,
                 vod_id            = CASE WHEN excluded.vod_id <> ''
                                          THEN excluded.vod_id ELSE clip.vod_id END,
                 vod_offset_secs   = COALESCE(excluded.vod_offset_secs, clip.vod_offset_secs),
                 keys_captured_at  = CASE
                                       WHEN clip.keys_captured_at > 0 THEN clip.keys_captured_at
                                       WHEN excluded.vod_id <> '' AND excluded.vod_offset_secs IS NOT NULL
                                            THEN excluded.last_seen_at
                                       ELSE 0 END",
            params![
                c.platform.as_str(),
                c.slug,
                c.channel_id,
                c.monitor_id,
                c.broadcaster_id,
                c.broadcaster_login,
                c.creator_login,
                c.title,
                c.game,
                c.language,
                c.view_count,
                c.duration_ms,
                c.created_at,
                c.url,
                c.thumbnail_url,
                c.vod_id,
                c.vod_offset_secs,
                now,
                c.recording_id,
                c.state,
                c.source,
            ],
        )?;
        let id = conn.query_row(
            "SELECT id FROM clip WHERE platform=?1 AND slug=?2",
            params![c.platform.as_str(), c.slug],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn get_clip(&self, id: i64) -> Result<Option<Clip>> {
        let conn = self.db();
        let c = conn
            .query_row(
                &format!("SELECT {CLIP_COLS} FROM clip WHERE id=?1"),
                params![id],
                Self::map_clip,
            )
            .optional()?;
        Ok(c)
    }

    /// Look a clip up by its platform-native key (the sweep's dedup path).
    pub fn clip_by_slug(&self, platform: Platform, slug: &str) -> Result<Option<Clip>> {
        let conn = self.db();
        let c = conn
            .query_row(
                &format!("SELECT {CLIP_COLS} FROM clip WHERE platform=?1 AND slug=?2"),
                params![platform.as_str(), slug],
                Self::map_clip,
            )
            .optional()?;
        Ok(c)
    }

    /// Every clip of one channel, newest first.
    pub fn clips_for_channel(&self, channel_id: i64, limit: i64) -> Result<Vec<Clip>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CLIP_COLS} FROM clip WHERE channel_id=?1
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![channel_id, limit], Self::map_clip)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every clip cut from one VOD, in timeline order.
    ///
    /// This is the whole answer to "list a VOD's clips": Helix has no
    /// `video_id` filter, so VOD scoping is necessarily derived locally from the
    /// `vod_id` each clip already carries.
    pub fn clips_for_vod(&self, platform: Platform, vod_id: &str) -> Result<Vec<Clip>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CLIP_COLS} FROM clip WHERE platform=?1 AND vod_id=?2
             ORDER BY vod_offset_secs, created_at"
        ))?;
        let rows = stmt
            .query_map(params![platform.as_str(), vod_id], Self::map_clip)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Per-broadcast clip counts, for the Streams tree summary row.
    /// Returns `(vod_id, total, archived)`.
    pub fn clip_counts_by_vod(&self, platform: Platform) -> Result<HashMap<String, (i64, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT vod_id, COUNT(*), SUM(CASE WHEN state='archived' THEN 1 ELSE 0 END)
             FROM clip WHERE platform=?1 AND vod_id <> '' GROUP BY vod_id",
        )?;
        let rows = stmt
            .query_map(params![platform.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    }

    /// Clips waiting to be downloaded, oldest first — the queue drainer's feed.
    /// Only ever returns clips whose channel is known, since the download gate
    /// is per channel.
    pub fn pending_clip_downloads(&self, limit: i64) -> Result<Vec<Clip>> {
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CLIP_COLS} FROM clip
             WHERE state='indexed' AND channel_id IS NOT NULL AND gone_at = 0
             ORDER BY created_at, id LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map(params![limit], Self::map_clip)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// How many clip downloads are queued or running right now.
    pub fn active_clip_download_count(&self) -> Result<i64> {
        let conn = self.db();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM clip WHERE state IN ('queued','downloading')",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Attach a download job ticket and move the clip into the queue.
    pub fn set_clip_download(&self, id: i64, state: &str, video_id: Option<i64>) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE clip SET state=?2, dl_video_id=?3 WHERE id=?1",
            params![id, state, video_id],
        )?;
        Ok(())
    }

    /// Find the clip owning a download job ticket (the finalize hook's lookup).
    pub fn clip_for_video(&self, video_id: i64) -> Result<Option<i64>> {
        let conn = self.db();
        let id = conn
            .query_row(
                "SELECT id FROM clip WHERE dl_video_id=?1",
                params![video_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Record a finished archive. Clears `dl_video_id` so the caller can then
    /// delete the `video` job ticket without orphaning a reference.
    pub fn finish_clip(
        &self,
        id: i64,
        output_path: &str,
        bytes: i64,
        method: &str,
        confidence: &str,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE clip SET state='archived', output_path=?2, bytes=?3, recovery_method=?4,
                 offset_confidence=?5, dl_video_id=NULL, err='' WHERE id=?1",
            params![id, output_path, bytes, method, confidence],
        )?;
        Ok(())
    }

    /// Record a failed attempt (bumps the retry counter).
    pub fn fail_clip(&self, id: i64, err: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE clip SET state='failed', dl_attempts=dl_attempts+1, dl_video_id=NULL,
                 err=?2 WHERE id=?1",
            params![id, err],
        )?;
        Ok(())
    }

    /// Mark clips that a sweep could no longer see upstream.
    ///
    /// Callers must only pass slugs from a **successful** hydrate — the Helix
    /// error contract is that `Err` means "we weren't watching", and marking a
    /// clip gone because a request failed would be a lie that also triggers a
    /// pointless recovery attempt.
    pub fn mark_clips_gone(&self, platform: Platform, slugs: &[String], now: i64) -> Result<usize> {
        if slugs.is_empty() {
            return Ok(0);
        }
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE clip SET gone_at=?3 WHERE platform=?1 AND slug=?2 AND gone_at=0",
            )?;
            for s in slugs {
                n += stmt.execute(params![platform.as_str(), s, now])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Clear the local file reference after the media is disposed of, keeping
    /// the catalogue row — the same "row survives, file does not" split that
    /// rolling recordings use.
    pub fn clear_clip_output(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE clip SET output_path='', bytes=0, state='indexed' WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    /// Total archived bytes for one channel — the per-channel ceiling check.
    pub fn clip_bytes_for_channel(&self, channel_id: i64) -> Result<i64> {
        let conn = self.db();
        let n = conn.query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM clip WHERE channel_id=?1",
            params![channel_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    fn map_clip(r: &rusqlite::Row<'_>) -> rusqlite::Result<Clip> {
        Ok(Clip {
            id: r.get(0)?,
            platform: Platform::parse(&r.get::<_, String>(1)?),
            slug: r.get(2)?,
            channel_id: r.get(3)?,
            monitor_id: r.get(4)?,
            broadcaster_id: r.get(5)?,
            broadcaster_login: r.get(6)?,
            creator_login: r.get(7)?,
            title: r.get(8)?,
            game: r.get(9)?,
            language: r.get(10)?,
            view_count: r.get(11)?,
            duration_ms: r.get(12)?,
            created_at: r.get(13)?,
            url: r.get(14)?,
            thumbnail_url: r.get(15)?,
            vod_id: r.get(16)?,
            vod_offset_secs: r.get(17)?,
            keys_captured_at: r.get(18)?,
            recording_id: r.get(19)?,
            state: r.get(20)?,
            source: r.get(21)?,
            recovery_method: r.get(22)?,
            offset_confidence: r.get(23)?,
            dl_video_id: r.get(24)?,
            dl_attempts: r.get(25)?,
            output_path: r.get(26)?,
            bytes: r.get(27)?,
            first_seen_at: r.get(28)?,
            last_seen_at: r.get(29)?,
            gone_at: r.get(30)?,
            err: r.get(31)?,
        })
    }

    // ----- sweep cursor -----

    pub fn clip_sweep_state(&self, monitor_id: i64) -> Result<ClipSweepState> {
        let conn = self.db();
        let s = conn
            .query_row(
                "SELECT monitor_id, last_swept_at, backfill_until, backfill_done, last_error
                 FROM clip_sweep WHERE monitor_id=?1",
                params![monitor_id],
                |r| {
                    Ok(ClipSweepState {
                        monitor_id: r.get(0)?,
                        last_swept_at: r.get(1)?,
                        backfill_until: r.get(2)?,
                        backfill_done: r.get::<_, i64>(3)? != 0,
                        last_error: r.get(4)?,
                    })
                },
            )
            .optional()?
            .unwrap_or(ClipSweepState {
                monitor_id,
                ..Default::default()
            });
        Ok(s)
    }

    /// Advance the incremental high-water mark. Only call this once a window set
    /// has drained completely.
    pub fn set_clip_swept(&self, monitor_id: i64, at: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO clip_sweep(monitor_id, last_swept_at, last_error)
             VALUES(?1, ?2, '')
             ON CONFLICT(monitor_id) DO UPDATE SET last_swept_at=?2, last_error=''",
            params![monitor_id, at],
        )?;
        Ok(())
    }

    /// Record a sweep failure without touching the high-water mark.
    pub fn set_clip_sweep_error(&self, monitor_id: i64, err: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO clip_sweep(monitor_id, last_error) VALUES(?1, ?2)
             ON CONFLICT(monitor_id) DO UPDATE SET last_error=?2",
            params![monitor_id, err],
        )?;
        Ok(())
    }

    pub fn set_clip_backfill(&self, monitor_id: i64, until: i64, done: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO clip_sweep(monitor_id, backfill_until, backfill_done) VALUES(?1, ?2, ?3)
             ON CONFLICT(monitor_id) DO UPDATE SET backfill_until=?2, backfill_done=?3",
            params![monitor_id, until, done as i64],
        )?;
        Ok(())
    }

    // ----- post-broadcast trigger -----

    /// Ended takes whose next clip sweep is due.
    ///
    /// Stage 0 fires at `ended_at + offsets[0]`, stage 1 at `ended_at +
    /// offsets[1]`; stage 2 is done. Returns `(recording_id, monitor_id, stage)`.
    pub fn recordings_due_clip_sweep(
        &self,
        now: i64,
        stage0_after: i64,
        stage1_after: i64,
    ) -> Result<Vec<(i64, i64, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, clip_sweep_stage FROM recording
             WHERE ended_at IS NOT NULL AND clip_sweep_stage < 2
               AND ((clip_sweep_stage = 0 AND ended_at + ?2 <= ?1)
                 OR (clip_sweep_stage = 1 AND ended_at + ?3 <= ?1))
             ORDER BY ended_at",
        )?;
        let rows = stmt
            .query_map(params![now, stage0_after, stage1_after], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn bump_clip_sweep_stage(&self, rec_id: i64, stage: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET clip_sweep_stage=?2 WHERE id=?1",
            params![rec_id, stage],
        )?;
        Ok(())
    }

    // ----- VOD CDN cache -----

    /// Remember a VOD's resolved CDN location.
    ///
    /// Taken while the VOD is alive, this is what lets a later clip recovery
    /// skip host probing entirely — see the v95 migration comment.
    pub fn put_vod_cdn(&self, v: &VodCdnRow) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO vod_cdn(vod_id, host, folder, login, broadcast_id, start_epoch, learned_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(vod_id) DO UPDATE SET host=?2, folder=?3, login=?4,
                 broadcast_id=?5, start_epoch=?6, learned_at=?7",
            params![
                v.vod_id,
                v.host,
                v.folder,
                v.login,
                v.broadcast_id,
                v.start_epoch,
                v.learned_at
            ],
        )?;
        Ok(())
    }

    pub fn get_vod_cdn(&self, vod_id: &str) -> Result<Option<VodCdnRow>> {
        let conn = self.db();
        let v = conn
            .query_row(
                "SELECT vod_id, host, folder, login, broadcast_id, start_epoch, learned_at
                 FROM vod_cdn WHERE vod_id=?1",
                params![vod_id],
                |r| {
                    Ok(VodCdnRow {
                        vod_id: r.get(0)?,
                        host: r.get(1)?,
                        folder: r.get(2)?,
                        login: r.get(3)?,
                        broadcast_id: r.get(4)?,
                        start_epoch: r.get(5)?,
                        learned_at: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(v)
    }
}

/// Column list for every `clip` SELECT, so `map_clip`'s indices have exactly one
/// definition to drift from.
const CLIP_COLS: &str = "id, platform, slug, channel_id, monitor_id, broadcaster_id,
     broadcaster_login, creator_login, title, game, language, view_count, duration_ms,
     created_at, url, thumbnail_url, vod_id, vod_offset_secs, keys_captured_at, recording_id,
     state, source, recovery_method, offset_confidence, dl_video_id, dl_attempts,
     output_path, bytes, first_seen_at, last_seen_at, gone_at, err";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_clip(slug: &str) -> Clip {
        Clip {
            platform: Platform::Twitch,
            slug: slug.into(),
            channel_id: Some(1),
            monitor_id: Some(2),
            broadcaster_login: "laynalazar".into(),
            title: "A Clip".into(),
            duration_ms: 15_600,
            created_at: 1_786_000_000,
            url: format!("https://www.twitch.tv/laynalazar/clip/{slug}"),
            vod_id: "2840712897".into(),
            vod_offset_secs: Some(4780),
            ..Default::default()
        }
    }

    #[test]
    fn clip_upsert_roundtrips_every_recovery_key() {
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_clip(&sample_clip("Gorgeous"), 100).unwrap();

        let c = store.get_clip(id).unwrap().unwrap();
        assert_eq!(c.slug, "Gorgeous");
        assert_eq!(c.vod_id, "2840712897");
        assert_eq!(c.vod_offset_secs, Some(4780));
        assert_eq!(c.broadcaster_login, "laynalazar");
        assert_eq!(c.duration_ms, 15_600);
        assert_eq!(c.created_at, 1_786_000_000);
        assert!(c.has_recovery_keys());
        // Captured-at is stamped only because both keys were present.
        assert_eq!(c.keys_captured_at, 100);
        assert_eq!(c.first_seen_at, 100);
        assert_eq!(c.state, "indexed");
    }

    #[test]
    fn reupsert_never_blanks_a_captured_recovery_key() {
        // The invariant the whole feature rests on. Twitch nulls video_id and
        // vod_offset once the parent VOD expires, so a later sweep of the same
        // clip legitimately carries blanks — and must not erase what we hold.
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_clip(&sample_clip("Gorgeous"), 100).unwrap();

        let mut expired = sample_clip("Gorgeous");
        expired.vod_id = String::new();
        expired.vod_offset_secs = None;
        expired.broadcaster_login = String::new();
        expired.view_count = 999;
        expired.title = "A Clip (renamed)".into();
        let id2 = store.upsert_clip(&expired, 200).unwrap();
        assert_eq!(id, id2, "same (platform, slug) must not create a second row");

        let c = store.get_clip(id).unwrap().unwrap();
        assert_eq!(c.vod_id, "2840712897", "vod_id must survive a blank re-sight");
        assert_eq!(c.vod_offset_secs, Some(4780), "offset must survive too");
        assert_eq!(c.broadcaster_login, "laynalazar");
        assert_eq!(c.keys_captured_at, 100, "capture time is the FIRST capture");
        // Volatile fields do refresh.
        assert_eq!(c.view_count, 999);
        assert_eq!(c.title, "A Clip (renamed)");
        assert_eq!(c.last_seen_at, 200);
        assert_eq!(c.first_seen_at, 100, "first_seen_at is never moved");
    }

    #[test]
    fn keys_can_still_be_filled_in_later_when_first_sight_had_none() {
        // A clip harvested from chat has no keys; a later Helix hydrate may.
        let store = Store::open_in_memory().unwrap();
        let mut bare = sample_clip("Later");
        bare.vod_id = String::new();
        bare.vod_offset_secs = None;
        let id = store.upsert_clip(&bare, 100).unwrap();
        assert!(!store.get_clip(id).unwrap().unwrap().has_recovery_keys());
        assert_eq!(store.get_clip(id).unwrap().unwrap().keys_captured_at, 0);

        store.upsert_clip(&sample_clip("Later"), 300).unwrap();
        let c = store.get_clip(id).unwrap().unwrap();
        assert!(c.has_recovery_keys());
        assert_eq!(c.keys_captured_at, 300, "stamped when they first arrived");
    }

    #[test]
    fn same_slug_on_two_platforms_are_distinct_clips() {
        let store = Store::open_in_memory().unwrap();
        let a = sample_clip("Shared");
        let mut b = sample_clip("Shared");
        b.platform = Platform::YouTube;
        let ia = store.upsert_clip(&a, 1).unwrap();
        let ib = store.upsert_clip(&b, 1).unwrap();
        assert_ne!(ia, ib);
    }

    #[test]
    fn clips_for_vod_scopes_to_one_broadcast_in_timeline_order() {
        let store = Store::open_in_memory().unwrap();
        let mut early = sample_clip("Early");
        early.vod_offset_secs = Some(100);
        let mut late = sample_clip("Late");
        late.vod_offset_secs = Some(9000);
        let mut other = sample_clip("Other");
        other.vod_id = "999".into();
        store.upsert_clip(&late, 1).unwrap();
        store.upsert_clip(&early, 1).unwrap();
        store.upsert_clip(&other, 1).unwrap();

        let got = store.clips_for_vod(Platform::Twitch, "2840712897").unwrap();
        assert_eq!(got.len(), 2, "the other VOD's clip is excluded");
        assert_eq!(got[0].slug, "Early", "ordered by offset into the VOD");
        assert_eq!(got[1].slug, "Late");
    }

    #[test]
    fn marking_gone_is_idempotent_and_skips_unknown_slugs() {
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_clip(&sample_clip("Gone"), 100).unwrap();
        assert!(!store.get_clip(id).unwrap().unwrap().is_gone());

        let n = store
            .mark_clips_gone(Platform::Twitch, &["Gone".into(), "NoSuch".into()], 500)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.get_clip(id).unwrap().unwrap().gone_at, 500);

        // A second sweep must not move the first-noticed timestamp.
        store
            .mark_clips_gone(Platform::Twitch, &["Gone".into()], 900)
            .unwrap();
        assert_eq!(store.get_clip(id).unwrap().unwrap().gone_at, 500);
    }

    #[test]
    fn a_clip_seen_again_is_no_longer_gone() {
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_clip(&sample_clip("Back"), 100).unwrap();
        store
            .mark_clips_gone(Platform::Twitch, &["Back".into()], 500)
            .unwrap();
        assert!(store.get_clip(id).unwrap().unwrap().is_gone());

        store.upsert_clip(&sample_clip("Back"), 600).unwrap();
        assert!(!store.get_clip(id).unwrap().unwrap().is_gone());
    }

    #[test]
    fn pending_downloads_skip_gone_orphan_and_finished_clips() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_clip(&sample_clip("Ready"), 1).unwrap();

        let mut orphan = sample_clip("Orphan"); // chat-harvested, unmonitored channel
        orphan.channel_id = None;
        store.upsert_clip(&orphan, 1).unwrap();

        let gone = store.upsert_clip(&sample_clip("Vanished"), 1).unwrap();
        store
            .mark_clips_gone(Platform::Twitch, &["Vanished".into()], 2)
            .unwrap();

        let done = store.upsert_clip(&sample_clip("Done"), 1).unwrap();
        store.finish_clip(done, "C:/c/done.mp4", 42, "live", "").unwrap();

        let pending = store.pending_clip_downloads(50).unwrap();
        let slugs: Vec<_> = pending.iter().map(|c| c.slug.as_str()).collect();
        assert_eq!(slugs, ["Ready"]);
        assert_ne!(gone, done);
    }

    #[test]
    fn finish_clears_the_job_ticket_so_the_video_row_can_be_deleted() {
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_clip(&sample_clip("Job"), 1).unwrap();
        store.set_clip_download(id, "downloading", Some(77)).unwrap();
        assert_eq!(store.clip_for_video(77).unwrap(), Some(id));

        store
            .finish_clip(id, "C:/clips/a.mp4", 1024, "live", "")
            .unwrap();
        let c = store.get_clip(id).unwrap().unwrap();
        assert!(c.is_archived());
        assert_eq!(c.bytes, 1024);
        assert_eq!(c.dl_video_id, None);
        assert_eq!(store.clip_for_video(77).unwrap(), None);
    }

    #[test]
    fn disposing_media_keeps_the_catalogue_row() {
        // Same "row survives, file does not" split as rolling recordings: the
        // row is the archive index and lets the clip be re-fetched while alive.
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_clip(&sample_clip("Kept"), 1).unwrap();
        store.finish_clip(id, "C:/c/a.mp4", 500, "live", "").unwrap();
        store.clear_clip_output(id).unwrap();

        let c = store.get_clip(id).unwrap().unwrap();
        assert_eq!(c.output_path, "");
        assert_eq!(c.bytes, 0);
        assert_eq!(c.state, "indexed");
        assert!(c.has_recovery_keys(), "keys outlive the media");
    }

    #[test]
    fn sweep_cursor_defaults_and_errors_never_advance_the_high_water_mark() {
        let store = Store::open_in_memory().unwrap();
        let s = store.clip_sweep_state(5).unwrap();
        assert_eq!(s.monitor_id, 5);
        assert_eq!(s.last_swept_at, 0);

        store.set_clip_swept(5, 1000).unwrap();
        assert_eq!(store.clip_sweep_state(5).unwrap().last_swept_at, 1000);

        // A failure records the reason but must leave the mark where it was,
        // or the window it failed on becomes a permanent hole.
        store.set_clip_sweep_error(5, "helix 500").unwrap();
        let s = store.clip_sweep_state(5).unwrap();
        assert_eq!(s.last_swept_at, 1000);
        assert_eq!(s.last_error, "helix 500");

        // A later success clears the error.
        store.set_clip_swept(5, 2000).unwrap();
        let s = store.clip_sweep_state(5).unwrap();
        assert_eq!(s.last_swept_at, 2000);
        assert_eq!(s.last_error, "");
    }

    #[test]
    fn post_broadcast_sweep_is_due_only_after_each_stage_offset() {
        // The stages exist because the recovery keys are perishable: these two
        // sweeps run inside the parent VOD's lifetime, which is the only window
        // in which Twitch still reports video_id/vod_offset.
        use crate::store::test_util::*;
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let rec = store
            .insert_recording(mid, 1_000, "C:/rec/a.mkv", Some(1_000), false, None, None, "", "")
            .unwrap();
        store
            .finish_recording(rec, 5_000, 1, Some(0), "completed", "C:/rec/a.mkv", "")
            .unwrap();

        let (h2, h24) = (2 * 3600, 24 * 3600);
        // Still inside the +2h settle: nothing due.
        assert!(store.recordings_due_clip_sweep(5_100, h2, h24).unwrap().is_empty());

        // +2h reached -> stage 0 due.
        let due = store.recordings_due_clip_sweep(5_000 + h2, h2, h24).unwrap();
        assert_eq!(due, vec![(rec, mid, 0)]);
        store.bump_clip_sweep_stage(rec, 1).unwrap();

        // Stage 1 is not due merely because stage 0 was; it waits for +24h.
        assert!(store
            .recordings_due_clip_sweep(5_000 + h2 + 60, h2, h24)
            .unwrap()
            .is_empty());
        let due = store.recordings_due_clip_sweep(5_000 + h24, h2, h24).unwrap();
        assert_eq!(due, vec![(rec, mid, 1)]);

        // Stage 2 is terminal — never due again, however long we wait.
        store.bump_clip_sweep_stage(rec, 2).unwrap();
        assert!(store
            .recordings_due_clip_sweep(9_999_999, h2, h24)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_unfinished_take_is_never_due_for_a_post_broadcast_sweep() {
        use crate::store::test_util::*;
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        store
            .insert_recording(mid, 1_000, "C:/rec/a.mkv", Some(1_000), false, None, None, "", "")
            .unwrap();

        // ended_at is still NULL: the broadcast hasn't finished, so its clips
        // haven't been made yet.
        assert!(store
            .recordings_due_clip_sweep(9_999_999, 7200, 86_400)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn vod_cdn_cache_roundtrips_and_updates_in_place() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_vod_cdn(&VodCdnRow {
                vod_id: "123".into(),
                host: "https://d2nvs31859zcd8.cloudfront.net/".into(),
                folder: "abcdef0123456789abcd_layna_456_1786000000".into(),
                login: "layna".into(),
                broadcast_id: "456".into(),
                start_epoch: 1_786_000_000,
                learned_at: 10,
            })
            .unwrap();
        let v = store.get_vod_cdn("123").unwrap().unwrap();
        assert_eq!(v.login, "layna");
        assert_eq!(v.start_epoch, 1_786_000_000);
        assert!(store.get_vod_cdn("nope").unwrap().is_none());
    }
}
