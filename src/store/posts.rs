//! Community-post archive, notifications, and about-page snapshots.

use super::*;
use super::migrations::parse_relative_age;

impl Store {
    // ----- community-post archive (schema v28) -----

    /// Look up an archived community-post image by its content hash for a monitor.
    /// `Some` means we've already downloaded this exact image; if `ocr_attempted`
    /// is set, `decoded_json` holds the events we got (possibly empty) so the
    /// caller can skip re-OCR. The hash is the fnv64 of the image bytes, rendered
    /// as a decimal string (see the v28 migration note on the TEXT column).
    pub fn community_post_get(
        &self,
        monitor_id: i64,
        content_hash: &str,
    ) -> Result<Option<ArchivedPost>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT content_hash, local_path, ocr_attempted, decoded_events, decoded_json
                 FROM community_post_archive
                 WHERE monitor_id = ?1 AND content_hash = ?2",
                params![monitor_id, content_hash],
                |r| {
                    Ok(ArchivedPost {
                        content_hash: r.get(0)?,
                        local_path: r.get(1)?,
                        ocr_attempted: r.get::<_, i64>(2)? != 0,
                        decoded_events: r.get(3)?,
                        decoded_json: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Record (or refresh) a downloaded community-post image. Idempotent on
    /// `(monitor_id, content_hash)`: re-downloading the same image just updates the
    /// URL/path it was last seen at (the feed URL can change), leaving any prior
    /// OCR result intact. Returns without touching `ocr_*`/`decoded_*`.
    pub fn community_post_upsert(
        &self,
        monitor_id: i64,
        source: &str,
        image_url: &str,
        content_hash: &str,
        local_path: &str,
        fetched_at: i64,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO community_post_archive
                 (monitor_id, source, image_url, content_hash, local_path, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(monitor_id, content_hash) DO UPDATE SET
                 image_url  = excluded.image_url,
                 local_path = excluded.local_path",
            params![monitor_id, source, image_url, content_hash, local_path, fetched_at],
        )?;
        Ok(())
    }

    /// Mark an archived image as OCR'd and cache the decoded events. `events` is
    /// the count (for cheap querying); `json` is the serialized `Vec<ScheduleSegment>`
    /// (may be `[]` — an authoritative "this image has no schedule", which still
    /// suppresses re-OCR).
    pub fn community_post_set_decoded(
        &self,
        monitor_id: i64,
        content_hash: &str,
        events: i64,
        json: &str,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE community_post_archive
             SET ocr_attempted = 1, decoded_events = ?3, decoded_json = ?4
             WHERE monitor_id = ?1 AND content_hash = ?2",
            params![monitor_id, content_hash, events, json],
        )?;
        Ok(())
    }

    /// Number of archived community-post images for a monitor (test/diagnostic helper).
    #[allow(dead_code)]
    pub fn community_post_count(&self, monitor_id: i64) -> Result<i64> {
        let conn = self.db();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM community_post_archive WHERE monitor_id = ?1",
            params![monitor_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    // ----- notifications feed (schema v37) -----

    /// Insert a notification. Idempotent when `ref_key` is non-empty (the
    /// partial-unique index makes a re-emit a silent no-op via `INSERT OR
    /// IGNORE`); `ref_key == ""` always inserts. Returns the new row id, or
    /// `None` when a dedup conflict swallowed it.
    pub fn insert_notification(&self, n: &NewNotification) -> Result<Option<i64>> {
        let conn = self.db();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO notification
                 (created_at, kind, severity, title, body, monitor_id, channel,
                  recording_id, action_label, action_url, image_path, ref_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                now_unix(),
                n.kind,
                n.severity,
                n.title,
                n.body,
                n.monitor_id,
                n.channel,
                n.recording_id,
                n.action_label,
                n.action_url,
                n.image_path,
                n.ref_key,
            ],
        )?;
        Ok((changed == 1).then(|| conn.last_insert_rowid()))
    }

    /// The most recent `limit` notifications, newest first. Kind/text filtering
    /// is done in-memory by the UI over this list.
    pub fn list_notifications(&self, limit: i64) -> Result<Vec<NotificationRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, created_at, kind, severity, title, body, monitor_id, channel,
                    recording_id, action_label, action_url, image_path, ref_key, read
             FROM notification
             ORDER BY created_at DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok(NotificationRow {
                    id: r.get(0)?,
                    created_at: r.get(1)?,
                    kind: r.get(2)?,
                    severity: r.get(3)?,
                    title: r.get(4)?,
                    body: r.get(5)?,
                    monitor_id: r.get(6)?,
                    channel: r.get(7)?,
                    recording_id: r.get(8)?,
                    action_label: r.get(9)?,
                    action_url: r.get(10)?,
                    image_path: r.get(11)?,
                    ref_key: r.get(12)?,
                    read: r.get::<_, i64>(13)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Number of unread notifications (the header badge count).
    pub fn unread_notification_count(&self) -> Result<i64> {
        let conn = self.db();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM notification WHERE read = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Mark every notification created at or before `created_at` as read (the
    /// "mark all read on window open" action — items arriving after open stay
    /// unread). Returns the number of rows updated.
    pub fn mark_notifications_read_before(&self, created_at: i64) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE notification SET read = 1 WHERE read = 0 AND created_at <= ?1",
            params![created_at],
        )?;
        Ok(n)
    }

    /// Delete notifications older than `keep_days` days (startup retention).
    /// Returns the number of rows pruned.
    pub fn prune_notifications(&self, keep_days: i64) -> Result<usize> {
        let conn = self.db();
        let cutoff = now_unix() - keep_days.max(0) * 86_400;
        let n = conn.execute(
            "DELETE FROM notification WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }

    // ----- YouTube community posts feed (schema v38/v39) -----

    /// Upsert a full community post, keyed on `(monitor_id, post_id)`. Preserves
    /// `first_seen` on an existing row (refreshing the other fields + `last_seen`).
    /// Returns `(post_pk, is_new)`; `is_new == true` drives the `youtube_post`
    /// notification.
    ///
    /// `published_at` is estimated from the relative `published_text` at INSERT
    /// and deliberately never updated: the relative buckets only get coarser
    /// with age ("2 weeks ago" → "1 month ago"), so the first estimate is the
    /// most precise one this source can give.
    pub fn community_post_upsert_full(&self, p: &NewCommunityPost) -> Result<(i64, bool)> {
        let conn = self.db();
        let now = now_unix();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM community_post WHERE monitor_id = ?1 AND post_id = ?2",
                params![p.monitor_id, p.post_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(pk) = existing {
            conn.execute(
                "UPDATE community_post SET
                     author = ?2, author_icon = ?3, published_text = ?4, body_text = ?5,
                     links_json = ?6, poll_json = ?7, vote_count = ?8, shared_json = ?9,
                     raw_json = ?10, last_seen = ?11, author_kind = ?12,
                     author_channel_id = ?13
                 WHERE id = ?1",
                params![
                    pk, p.author, p.author_icon, p.published_text, p.body_text, p.links_json,
                    p.poll_json, p.vote_count, p.shared_json, p.raw_json, now, p.author_kind,
                    p.author_channel_id,
                ],
            )?;
            Ok((pk, false))
        } else {
            let published_at = parse_relative_age(&p.published_text)
                .map(|age| now - age)
                .unwrap_or(now);
            conn.execute(
                "INSERT INTO community_post
                     (monitor_id, channel_id, post_id, author, author_icon, published_text,
                      body_text, links_json, poll_json, vote_count, shared_json, raw_json,
                      first_seen, last_seen, published_at, author_kind, author_channel_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13,?14,?15,?16)",
                params![
                    p.monitor_id, p.channel_id, p.post_id, p.author, p.author_icon,
                    p.published_text, p.body_text, p.links_json, p.poll_json, p.vote_count,
                    p.shared_json, p.raw_json, now, published_at, p.author_kind,
                    p.author_channel_id,
                ],
            )?;
            Ok((conn.last_insert_rowid(), true))
        }
    }

    /// True when the full-history posts backfill has completed for this monitor.
    pub fn posts_backfill_done(&self, monitor_id: i64) -> Result<bool> {
        let conn = self.db();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM community_post_backfill
             WHERE monitor_id = ?1 AND completed_at > 0",
            params![monitor_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Record one backfill session: accumulates pages/posts walked and stamps
    /// `completed_at` once a walk reached the end of the channel's feed. A later
    /// interrupted session never clears an earlier completion.
    pub fn posts_backfill_record(
        &self,
        monitor_id: i64,
        completed: bool,
        pages: i64,
        posts_seen: i64,
    ) -> Result<()> {
        let conn = self.db();
        let now = now_unix();
        conn.execute(
            "INSERT INTO community_post_backfill
                 (monitor_id, completed_at, last_attempt_at, pages, posts_seen)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(monitor_id) DO UPDATE SET
                 last_attempt_at = excluded.last_attempt_at,
                 pages = pages + excluded.pages,
                 posts_seen = posts_seen + excluded.posts_seen,
                 completed_at = CASE WHEN excluded.completed_at > 0
                                     THEN excluded.completed_at ELSE completed_at END",
            params![monitor_id, if completed { now } else { 0 }, now, pages, posts_seen],
        )?;
        Ok(())
    }

    /// Upsert one attachment of a post (idempotent on `(post_pk, ordinal)`).
    pub fn community_post_media_upsert(
        &self,
        post_pk: i64,
        ordinal: i64,
        kind: &str,
        image_url: &str,
        content_hash: &str,
        local_path: &str,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO community_post_media
                 (post_pk, ordinal, kind, image_url, content_hash, local_path)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(post_pk, ordinal) DO UPDATE SET
                 kind = excluded.kind, image_url = excluded.image_url,
                 content_hash = excluded.content_hash, local_path = excluded.local_path",
            params![post_pk, ordinal, kind, image_url, content_hash, local_path],
        )?;
        Ok(())
    }

    /// Number of recorded attachments for a post — lets the fetch pass skip
    /// re-downloading images for a post it has already fully cached.
    pub fn community_post_media_count(&self, post_pk: i64) -> Result<i64> {
        let conn = self.db();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM community_post_media WHERE post_pk = ?1",
            params![post_pk],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    // ----- about-page archive (schema v45) -----

    /// Record an about-page capture: insert a new version when `content_hash`
    /// differs from the latest row for `(channel_id, platform, account)`,
    /// otherwise just bump that row's `last_checked_at`.
    pub fn about_snapshot_record(&self, s: &NewAboutSnapshot) -> Result<AboutRecordOutcome> {
        let conn = self.db();
        let now = now_unix();
        let latest: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, content_hash FROM about_snapshot
                 WHERE channel_id = ?1 AND platform = ?2 AND account = ?3
                 ORDER BY fetched_at DESC, id DESC LIMIT 1",
                params![s.channel_id, s.platform, s.account],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((id, hash)) = &latest
            && *hash == s.content_hash
        {
            conn.execute(
                "UPDATE about_snapshot SET last_checked_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
            return Ok(AboutRecordOutcome {
                id: *id,
                inserted: false,
                prev_hash: Some(hash.clone()),
            });
        }
        conn.execute(
            "INSERT INTO about_snapshot
                 (channel_id, platform, account, fetched_at, last_checked_at,
                  content_hash, description, panels_json, links_json, raw_json)
             VALUES (?1,?2,?3,?4,?4,?5,?6,?7,?8,?9)",
            params![
                s.channel_id, s.platform, s.account, now, s.content_hash, s.description,
                s.panels_json, s.links_json, s.raw_json,
            ],
        )?;
        Ok(AboutRecordOutcome {
            id: conn.last_insert_rowid(),
            inserted: true,
            prev_hash: latest.map(|(_, h)| h),
        })
    }

    /// True when at least one about snapshot exists for the key. Drives the
    /// degraded-fetch gate (a partial capture may only ever be the baseline).
    pub fn about_snapshot_exists(
        &self,
        channel_id: i64,
        platform: &str,
        account: &str,
    ) -> Result<bool> {
        let conn = self.db();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM about_snapshot
             WHERE channel_id = ?1 AND platform = ?2 AND account = ?3",
            params![channel_id, platform, account],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn about_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AboutSnapshotRow> {
        Ok(AboutSnapshotRow {
            id: r.get(0)?,
            channel_id: r.get(1)?,
            platform: r.get(2)?,
            account: r.get(3)?,
            fetched_at: r.get(4)?,
            last_checked_at: r.get(5)?,
            content_hash: r.get(6)?,
            description: r.get(7)?,
            panels_json: r.get(8)?,
            links_json: r.get(9)?,
        })
    }

    /// All archived versions of one account's about page, newest first (the
    /// viewer's version picker).
    pub fn about_snapshots_for_account(
        &self,
        channel_id: i64,
        platform: &str,
        account: &str,
    ) -> Result<Vec<AboutSnapshotRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, platform, account, fetched_at, last_checked_at,
                    content_hash, description, panels_json, links_json
             FROM about_snapshot
             WHERE channel_id = ?1 AND platform = ?2 AND account = ?3
             ORDER BY fetched_at DESC, id DESC",
        )?;
        let rows = stmt
            .query_map(params![channel_id, platform, account], Self::about_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Latest snapshot per (platform, account) of a channel, with each
    /// account's total version count — the channel-Properties "About pages"
    /// list.
    pub fn about_latest_per_account(
        &self,
        channel_id: i64,
    ) -> Result<Vec<(AboutSnapshotRow, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, platform, account, fetched_at, last_checked_at,
                    content_hash, description, panels_json, links_json,
                    (SELECT COUNT(*) FROM about_snapshot i
                      WHERE i.channel_id = o.channel_id AND i.platform = o.platform
                        AND i.account = o.account) AS versions
             FROM about_snapshot o
             WHERE channel_id = ?1
               AND fetched_at = (SELECT MAX(fetched_at) FROM about_snapshot i
                                  WHERE i.channel_id = o.channel_id
                                    AND i.platform = o.platform AND i.account = o.account)
             ORDER BY platform, account",
        )?;
        let rows = stmt
            .query_map(params![channel_id], |r| {
                Ok((Self::about_row(r)?, r.get::<_, i64>(10)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// List community posts (newest publish time first — the approximate
    /// `published_at`, with `first_seen`/insertion order as tiebreakers within
    /// one relative-time bucket) with their ordered media. Global across all
    /// channels when `monitor_id` is `None`; per-channel when `Some`. Each row
    /// carries the denormalized channel name for the feed's display + filter.
    pub fn list_community_posts(
        &self,
        monitor_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<CommunityPostRow>> {
        let conn = self.db();
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<CommunityPostRow> {
            Ok(CommunityPostRow {
                id: r.get(0)?,
                monitor_id: r.get(1)?,
                channel_id: r.get(2)?,
                post_id: r.get(3)?,
                author: r.get(4)?,
                author_icon: r.get(5)?,
                published_text: r.get(6)?,
                body_text: r.get(7)?,
                links_json: r.get(8)?,
                poll_json: r.get(9)?,
                vote_count: r.get(10)?,
                shared_json: r.get(11)?,
                first_seen: r.get(12)?,
                published_at: r.get(13)?,
                author_kind: r.get(14)?,
                channel: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
                media: Vec::new(),
            })
        };
        // Order: publish-time buckets, then discovery time, then `id ASC` —
        // within one scan batch posts are inserted in page order (newest
        // first), so ascending ids keep that order for same-bucket ties.
        const COLS: &str = "cp.id, cp.monitor_id, cp.channel_id, cp.post_id, cp.author, \
             cp.author_icon, cp.published_text, cp.body_text, cp.links_json, cp.poll_json, \
             cp.vote_count, cp.shared_json, cp.first_seen, cp.published_at, \
             cp.author_kind, ch.name";
        const ORDER: &str = "cp.published_at DESC, cp.first_seen DESC, cp.id ASC";
        let mut posts: Vec<CommunityPostRow> = match monitor_id {
            Some(mid) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {COLS} FROM community_post cp
                     LEFT JOIN channel ch ON ch.id = cp.channel_id
                     WHERE cp.monitor_id = ?1
                     ORDER BY {ORDER} LIMIT ?2"
                ))?;
                stmt.query_map(params![mid, limit], |r| map_row(r))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {COLS} FROM community_post cp
                     LEFT JOIN channel ch ON ch.id = cp.channel_id
                     ORDER BY {ORDER} LIMIT ?1"
                ))?;
                stmt.query_map(params![limit], |r| map_row(r))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        // Attach ordered attachments per post (bounded by `limit`, so N+1 is fine).
        let mut mstmt = conn.prepare(
            "SELECT ordinal, kind, image_url, content_hash, local_path
             FROM community_post_media WHERE post_pk = ?1 ORDER BY ordinal",
        )?;
        for p in &mut posts {
            p.media = mstmt
                .query_map(params![p.id], |r| {
                    Ok(PostMediaRow {
                        ordinal: r.get(0)?,
                        kind: r.get(1)?,
                        image_url: r.get(2)?,
                        content_hash: r.get(3)?,
                        local_path: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(posts)
    }

    /// Aggregate counts and byte totals across the whole database for the Stats view.
    pub fn global_stats(&self) -> Result<GlobalStats> {
        let conn = self.db();
        let now = now_unix();
        let r = conn.query_row(
            "SELECT
               (SELECT COUNT(*) FROM recording)                                            AS total_recordings,
               (SELECT COALESCE(SUM(bytes), 0) FROM recording)                            AS total_bytes,
               (SELECT COUNT(*) FROM monitor)                                              AS total_monitors,
               (SELECT COUNT(*) FROM monitor WHERE active = 1)                             AS active_monitors,
               (SELECT COUNT(*) FROM channel)                                              AS total_channels,
               (SELECT COUNT(*) FROM schedule_segment WHERE canceled = 0 AND start_time > ?1) AS upcoming_segments",
            params![now],
            |r| {
                Ok(GlobalStats {
                    total_recordings: r.get(0)?,
                    total_bytes:      r.get(1)?,
                    total_monitors:   r.get(2)?,
                    active_monitors:  r.get(3)?,
                    total_channels:   r.get(4)?,
                    upcoming_segments: r.get(5)?,
                })
            },
        )?;
        Ok(r)
    }

    // ----- poll/detect request history (schema v56) -----

    /// Fold one scheduler tick's poll/detect outcomes into the minute-bucket
    /// `poll_history` table: one upsert-increment per `(platform, method)`
    /// pair that saw traffic this tick (`counts` = platform key, method
    /// short-label, polls, errors), then prune rows past the retention
    /// window. Feeds the Stats view's graphs via [`Store::poll_history`].
    pub fn record_poll_history(&self, at_unix: i64, counts: &[(&str, &str, u64, u64)]) -> Result<()> {
        let bucket_t = at_unix - at_unix.rem_euclid(POLL_HISTORY_RAW_BUCKET_SECS);
        let conn = self.db();
        for (platform, method, polls, errors) in counts {
            conn.execute(
                "INSERT INTO poll_history(bucket_t, platform, method, polls, errors)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(bucket_t, platform, method) DO UPDATE SET
                    polls = polls + excluded.polls,
                    errors = errors + excluded.errors",
                params![bucket_t, platform, method, *polls as i64, *errors as i64],
            )?;
        }
        // Range delete on the PK's leading column — cheap even when nothing
        // is old enough yet, so just run it every fold.
        conn.execute(
            "DELETE FROM poll_history WHERE bucket_t < ?1",
            params![bucket_t - POLL_HISTORY_RETENTION_SECS],
        )?;
        Ok(())
    }

    /// Poll/detect history since `since_unix`, re-aggregated into
    /// `bucket_secs`-wide buckets per `(platform, method)`, oldest first.
    /// Coarser views (hour/day/week graphs) are pure query-time GROUP BYs
    /// over the minute-resolution rows — there are no extra storage tiers.
    pub fn poll_history(&self, since_unix: i64, bucket_secs: i64) -> Result<Vec<PollBucket>> {
        let bucket_secs = bucket_secs.max(POLL_HISTORY_RAW_BUCKET_SECS);
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT (bucket_t / ?1) * ?1 AS tb, platform, method,
                    SUM(polls), SUM(errors)
             FROM poll_history
             WHERE bucket_t >= ?2
             GROUP BY tb, platform, method
             ORDER BY tb",
        )?;
        let rows = stmt
            .query_map(params![bucket_secs, since_unix], |r| {
                Ok(PollBucket {
                    t: r.get(0)?,
                    platform: r.get(1)?,
                    method: r.get(2)?,
                    polls: r.get::<_, i64>(3)?.max(0) as u64,
                    errors: r.get::<_, i64>(4)?.max(0) as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Drop all poll/detect history (the Stats view's Reset button clears
    /// the graphs together with the cumulative counters).
    pub fn clear_poll_history(&self) -> Result<()> {
        self.db().execute("DELETE FROM poll_history", [])?;
        Ok(())
    }
}

/// Raw storage resolution of the `poll_history` table (one minute).
const POLL_HISTORY_RAW_BUCKET_SECS: i64 = 60;
/// How much minute-resolution history is kept before pruning (60 days —
/// roughly 430k rows worst-case with every platform and method active, a
/// trivial table for SQLite).
const POLL_HISTORY_RETENTION_SECS: i64 = 60 * 86_400;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::*;

    #[test]
    fn poll_history_upserts_aggregates_and_prunes() {
        let store = Store::open_in_memory().unwrap();
        let t0: i64 = 1_000_000 - 1_000_000_i64.rem_euclid(3600); // hour-aligned

        // Two folds into the same minute bucket upsert-increment one row.
        store.record_poll_history(t0 + 5, &[("twitch", "Helix API", 3, 1)]).unwrap();
        store.record_poll_history(t0 + 30, &[("twitch", "Helix API", 2, 0)]).unwrap();
        // Same minute, different pair -> its own row.
        store.record_poll_history(t0 + 40, &[("youtube", "Scrape", 4, 4)]).unwrap();
        // A later minute -> new bucket for the same pair.
        store.record_poll_history(t0 + 90, &[("twitch", "Helix API", 1, 0)]).unwrap();

        // Minute-resolution query sees the raw buckets.
        let raw = store.poll_history(t0, 60).unwrap();
        assert_eq!(raw.len(), 3);
        let first = raw.iter().find(|b| b.t == t0 && b.platform == "twitch").unwrap();
        assert_eq!((first.polls, first.errors), (5, 1), "same-minute folds accumulated");

        // Query-time aggregation: an hour-wide bucket folds the minutes.
        let hourly = store.poll_history(t0, 3600).unwrap();
        assert_eq!(hourly.len(), 2, "one bucket per (platform, method)");
        let tw = hourly.iter().find(|b| b.platform == "twitch").unwrap();
        assert_eq!((tw.polls, tw.errors), (6, 1));
        assert_eq!(tw.t, t0, "bucket start aligned down to the bucket width");

        // `since` filters, and a fold ~61 days later prunes the old rows.
        assert!(store.poll_history(t0 + 3600, 60).unwrap().is_empty());
        store
            .record_poll_history(t0 + 61 * 86_400, &[("twitch", "Helix API", 1, 0)])
            .unwrap();
        let all = store.poll_history(0, 60).unwrap();
        assert_eq!(all.len(), 1, "everything from t0 aged out");

        store.clear_poll_history().unwrap();
        assert!(store.poll_history(0, 60).unwrap().is_empty());
    }

    #[test]
    fn community_post_archive_dedupes_and_preserves_ocr() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // First download of an image.
        store
            .community_post_upsert(mid, "youtube_community_ocr", "https://x/a.jpg", "1111", "C:/c/1111", 100)
            .unwrap();
        assert_eq!(store.community_post_count(mid).unwrap(), 1);

        // Cache it as OCR'd with one decoded event.
        store
            .community_post_set_decoded(mid, "1111", 1, r#"[{"t":"x"}]"#)
            .unwrap();
        let got = store.community_post_get(mid, "1111").unwrap().unwrap();
        assert!(got.ocr_attempted);
        assert_eq!(got.decoded_events, 1);
        assert_eq!(got.decoded_json, r#"[{"t":"x"}]"#);

        // Re-upserting the SAME (monitor, hash) with a new URL/path must not add a
        // row and must NOT wipe the cached OCR result (idempotent on the unique key).
        store
            .community_post_upsert(mid, "youtube_community_ocr", "https://x/b.jpg", "1111", "C:/c/1111b", 200)
            .unwrap();
        assert_eq!(store.community_post_count(mid).unwrap(), 1);
        let got = store.community_post_get(mid, "1111").unwrap().unwrap();
        assert!(got.ocr_attempted, "re-download must not clear prior OCR");
        assert_eq!(got.decoded_events, 1);
        assert_eq!(got.local_path, "C:/c/1111b", "path updated to the latest download");

        // A different content hash is a distinct archived image.
        store
            .community_post_upsert(mid, "youtube_community_ocr", "https://x/c.jpg", "2222", "C:/c/2222", 300)
            .unwrap();
        assert_eq!(store.community_post_count(mid).unwrap(), 2);
        // The new image hasn't been OCR'd yet.
        assert!(!store.community_post_get(mid, "2222").unwrap().unwrap().ocr_attempted);

        // The same content hash under a DIFFERENT monitor is independent (the unique
        // index is on the pair, and each monitor archives its own copy).
        let mid2 = store.insert_monitor(&m).unwrap();
        store
            .community_post_upsert(mid2, "youtube_community_ocr", "https://x/a.jpg", "1111", "C:/c/1111", 100)
            .unwrap();
        assert_eq!(store.community_post_count(mid2).unwrap(), 1);
        // mid2's copy is its own un-OCR'd row; mid's stays OCR'd.
        assert!(!store.community_post_get(mid2, "1111").unwrap().unwrap().ocr_attempted);
        assert!(store.community_post_get(mid, "1111").unwrap().unwrap().ocr_attempted);

        // Deleting a monitor cascades to its archived posts (FK ON DELETE CASCADE).
        store.delete_monitor(mid).unwrap();
        assert_eq!(store.community_post_count(mid).unwrap(), 0);
        assert!(store.community_post_get(mid, "1111").unwrap().is_none());
        // The other monitor's archive is untouched.
        assert_eq!(store.community_post_count(mid2).unwrap(), 1);
    }
    #[test]
    fn notification_insert_dedup_read_and_prune() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let mk = |kind: &str, ref_key: &str| NewNotification {
            kind: kind.into(),
            severity: "info".into(),
            title: "T".into(),
            body: "B".into(),
            monitor_id: Some(mid),
            channel: "Streamer".into(),
            ref_key: ref_key.into(),
            ..Default::default()
        };

        // First insert of a keyed notification returns an id and is unread.
        let id = store.insert_notification(&mk("went_live", "wentlive:1:s")).unwrap();
        assert!(id.is_some());
        assert_eq!(store.unread_notification_count().unwrap(), 1);

        // Re-inserting the SAME ref_key is a silent no-op (dedup).
        assert!(store.insert_notification(&mk("went_live", "wentlive:1:s")).unwrap().is_none());
        assert_eq!(store.list_notifications(100).unwrap().len(), 1);

        // Empty ref_key never dedups — two distinct rows.
        assert!(store.insert_notification(&mk("error", "")).unwrap().is_some());
        assert!(store.insert_notification(&mk("error", "")).unwrap().is_some());
        assert_eq!(store.list_notifications(100).unwrap().len(), 3);
        assert_eq!(store.unread_notification_count().unwrap(), 3);

        // Round-trip fields + newest-first order.
        let rows = store.list_notifications(100).unwrap();
        assert_eq!(rows[0].kind, "error"); // most recent inserts
        assert_eq!(rows[2].kind, "went_live");
        assert_eq!(rows[2].channel, "Streamer");
        assert_eq!(rows[2].monitor_id, Some(mid));

        // Mark-all-read-before(now) zeroes the badge.
        let updated = store.mark_notifications_read_before(now_unix()).unwrap();
        assert_eq!(updated, 3);
        assert_eq!(store.unread_notification_count().unwrap(), 0);

        // Retention: fresh rows (created_at == now) survive a 90-day prune…
        assert_eq!(store.prune_notifications(90).unwrap(), 0);
        assert_eq!(store.list_notifications(100).unwrap().len(), 3);
        // …but a row backdated past the window is deleted (only the old one).
        let old = now_unix() - 100 * 86_400;
        store
            .db()
            .execute("UPDATE notification SET created_at = ?1 WHERE kind = 'went_live'", params![old])
            .unwrap();
        assert_eq!(store.prune_notifications(90).unwrap(), 1);
        assert_eq!(store.list_notifications(100).unwrap().len(), 2);

        // Deleting a monitor keeps its notifications but nulls the FK (SET NULL),
        // and the denormalized channel string stays meaningful.
        let nid = store.insert_notification(&mk("youtube_post", "post:2:s")).unwrap();
        assert!(nid.is_some());
        let before = store.list_notifications(100).unwrap().len();
        store.delete_monitor(mid).unwrap();
        let rows = store.list_notifications(100).unwrap();
        assert_eq!(rows.len(), before, "SET NULL keeps rows (not CASCADE)");
        assert!(rows.iter().all(|r| r.monitor_id.is_none()), "FK nulled");
        assert!(rows.iter().all(|r| r.channel == "Streamer"), "channel preserved");
    }
    #[test]
    fn community_post_full_upsert_media_and_list() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let post = |id: &str, body: &str, published: &str| NewCommunityPost {
            monitor_id: mid,
            channel_id: cid,
            post_id: id.into(),
            author: "Streamer".into(),
            published_text: published.into(),
            body_text: body.into(),
            ..Default::default()
        };

        // First upsert → new; attach two ordered images.
        let (pk, is_new) = store
            .community_post_upsert_full(&post("p1", "hello", "2 days ago"))
            .unwrap();
        assert!(is_new);
        store.community_post_media_upsert(pk, 0, "image", "u0", "h0", "C:/0").unwrap();
        store.community_post_media_upsert(pk, 1, "image", "u1", "h1", "C:/1").unwrap();
        assert_eq!(store.community_post_media_count(pk).unwrap(), 2);
        // Re-upserting the same ordinal is idempotent (no new row).
        store.community_post_media_upsert(pk, 1, "image", "u1b", "h1", "C:/1b").unwrap();
        assert_eq!(store.community_post_media_count(pk).unwrap(), 2);

        // Re-upsert same post_id → NOT new (updates body, preserves first_seen).
        let (pk2, is_new2) = store
            .community_post_upsert_full(&post("p1", "edited", "2 days ago"))
            .unwrap();
        assert_eq!(pk2, pk);
        assert!(!is_new2);

        // A different post_id → new. Fresher relative time → sorts first.
        let (_, is_new3) = store
            .community_post_upsert_full(&post("p2", "second", "1 day ago"))
            .unwrap();
        assert!(is_new3);

        // Global list (newest publish time first): p2 then p1; p1 carries its
        // 2 ordered images.
        let rows = store.list_community_posts(None, 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].post_id, "p2");
        assert_eq!(rows[1].post_id, "p1");
        assert_eq!(rows[1].body_text, "edited", "body refreshed on re-upsert");
        assert_eq!(rows[1].channel, "Streamer", "denormalized channel name via join");
        assert_eq!(rows[1].media.len(), 2);
        assert_eq!(rows[1].media[0].local_path, "C:/0");
        // Ordinal 1 was re-upserted in place (update-on-conflict), not duplicated.
        assert_eq!(rows[1].media[1].local_path, "C:/1b");

        // Per-channel list filters to that monitor.
        assert_eq!(store.list_community_posts(Some(mid), 100).unwrap().len(), 2);

        // Deleting the monitor cascades to posts + media.
        store.delete_monitor(mid).unwrap();
        assert!(store.list_community_posts(None, 100).unwrap().is_empty());
    }
    #[test]
    fn community_post_publish_ordering() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let post = |id: &str, published: &str| NewCommunityPost {
            monitor_id: mid,
            channel_id: cid,
            post_id: id.into(),
            published_text: published.into(),
            ..Default::default()
        };

        // Inserted in scrambled discovery order (as a backfill walk would) —
        // the list must still come back in publish order, newest first.
        store.community_post_upsert_full(&post("mid", "3 weeks ago")).unwrap();
        store.community_post_upsert_full(&post("old", "1 year ago")).unwrap();
        store.community_post_upsert_full(&post("new", "2 hours ago")).unwrap();
        let rows = store.list_community_posts(None, 100).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.post_id.as_str()).collect();
        assert_eq!(ids, ["new", "mid", "old"]);

        // A re-scan coarsens the relative text ("1 year ago" stays the drifting
        // display string) but must NOT move the stored publish estimate.
        let before = rows.iter().find(|r| r.post_id == "old").unwrap().published_at;
        store.community_post_upsert_full(&post("old", "2 years ago")).unwrap();
        let rows = store.list_community_posts(None, 100).unwrap();
        let after = rows.iter().find(|r| r.post_id == "old").unwrap();
        assert_eq!(after.published_at, before, "estimate pinned at first sight");
        assert_eq!(after.published_text, "2 years ago", "display text refreshed");

        // Unparseable text falls back to discovery time (never 0).
        store.community_post_upsert_full(&post("odd", "yesterday")).unwrap();
        let rows = store.list_community_posts(None, 100).unwrap();
        let odd = rows.iter().find(|r| r.post_id == "odd").unwrap();
        assert_eq!(odd.published_at, odd.first_seen);
    }
    #[test]
    fn posts_backfill_state_accumulates() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        assert!(!store.posts_backfill_done(mid).unwrap());

        // An interrupted session records progress but not completion.
        store.posts_backfill_record(mid, false, 3, 30).unwrap();
        assert!(!store.posts_backfill_done(mid).unwrap());

        // The finishing session flips completion; counters accumulate.
        store.posts_backfill_record(mid, true, 2, 20).unwrap();
        assert!(store.posts_backfill_done(mid).unwrap());
        let (pages, posts): (i64, i64) = store
            .db()
            .query_row(
                "SELECT pages, posts_seen FROM community_post_backfill WHERE monitor_id = ?1",
                params![mid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((pages, posts), (5, 50));

        // A later partial session (e.g. a gap-fill bookkeeping call) never
        // clears an earlier completion.
        store.posts_backfill_record(mid, false, 1, 10).unwrap();
        assert!(store.posts_backfill_done(mid).unwrap());

        // Unknown monitor → not done.
        assert!(!store.posts_backfill_done(mid + 999).unwrap());
    }
    #[test]
    fn community_post_author_kind_roundtrips() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        store
            .community_post_upsert_full(&NewCommunityPost {
                monitor_id: mid,
                channel_id: cid,
                post_id: "v1".into(),
                author: "Fan".into(),
                author_kind: "viewer".into(),
                author_channel_id: "UCfan0000000000000000000".into(),
                ..Default::default()
            })
            .unwrap();
        let rows = store.list_community_posts(None, 100).unwrap();
        assert_eq!(rows[0].author_kind, "viewer");

        // A later round can correct a conservative first guess (UPDATE path).
        store
            .community_post_upsert_full(&NewCommunityPost {
                monitor_id: mid,
                channel_id: cid,
                post_id: "v1".into(),
                author: "Fan".into(),
                author_kind: "channel".into(),
                ..Default::default()
            })
            .unwrap();
        let rows = store.list_community_posts(None, 100).unwrap();
        assert_eq!(rows[0].author_kind, "channel");
    }
    #[test]
    fn about_record_inserts_baseline() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        assert!(!store.about_snapshot_exists(cid, "twitch", "a").unwrap());
        let o = store.about_record_test(&about(cid, "twitch", "a", "h1", "bio v1"));
        assert!(o.inserted);
        assert!(o.prev_hash.is_none(), "first capture ever has no prev hash");
        assert!(store.about_snapshot_exists(cid, "twitch", "a").unwrap());
        let rows = store.about_snapshots_for_account(cid, "twitch", "a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].description, "bio v1");
        assert_eq!(rows[0].fetched_at, rows[0].last_checked_at);
    }
    #[test]
    fn about_record_dedups_same_hash() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        store.about_record_test(&about(cid, "twitch", "a", "h1", "bio"));
        // Force a visibly newer last_checked_at.
        {
            let conn = store.db();
            conn.execute("UPDATE about_snapshot SET fetched_at = 100, last_checked_at = 100", [])
                .unwrap();
        }
        let o = store.about_record_test(&about(cid, "twitch", "a", "h1", "bio"));
        assert!(!o.inserted, "same hash must not create a version");
        assert_eq!(o.prev_hash.as_deref(), Some("h1"));
        let rows = store.about_snapshots_for_account(cid, "twitch", "a").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].last_checked_at > 100, "check timestamp bumped");
        assert_eq!(rows[0].fetched_at, 100, "capture timestamp untouched");
    }
    #[test]
    fn about_record_new_version_on_change() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        store.about_record_test(&about(cid, "twitch", "a", "h1", "old"));
        // Backdate the first row so newest-first ordering is observable.
        {
            let conn = store.db();
            conn.execute("UPDATE about_snapshot SET fetched_at = 100", []).unwrap();
        }
        let o = store.about_record_test(&about(cid, "twitch", "a", "h2", "new"));
        assert!(o.inserted);
        assert_eq!(o.prev_hash.as_deref(), Some("h1"), "prev hash drives the change log");
        let rows = store.about_snapshots_for_account(cid, "twitch", "a").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].description, "new", "newest first");
        assert_eq!(rows[1].description, "old");
    }
    #[test]
    fn about_latest_per_account_two_accounts() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        store.about_record_test(&about(cid, "twitch", "main", "h1", "main v1"));
        {
            let conn = store.db();
            conn.execute("UPDATE about_snapshot SET fetched_at = 100", []).unwrap();
        }
        store.about_record_test(&about(cid, "twitch", "main", "h2", "main v2"));
        store.about_record_test(&about(cid, "twitch", "alt", "h3", "alt v1"));
        let latest = store.about_latest_per_account(cid).unwrap();
        assert_eq!(latest.len(), 2);
        let main = latest.iter().find(|(s, _)| s.account == "main").unwrap();
        assert_eq!(main.0.description, "main v2");
        assert_eq!(main.1, 2, "version count");
        let alt = latest.iter().find(|(s, _)| s.account == "alt").unwrap();
        assert_eq!(alt.0.description, "alt v1");
        assert_eq!(alt.1, 1);
    }
    #[test]
    fn about_keys_isolated_per_platform() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        store.about_record_test(&about(cid, "twitch", "same", "h1", "twitch bio"));
        let o = store.about_record_test(&about(cid, "kick", "same", "h1", "kick bio"));
        assert!(o.inserted, "same account slug on another platform is a distinct key");
        assert!(o.prev_hash.is_none());
        assert_eq!(store.about_snapshots_for_account(cid, "twitch", "same").unwrap().len(), 1);
        assert_eq!(store.about_snapshots_for_account(cid, "kick", "same").unwrap().len(), 1);
    }

    impl Store {
        /// Test shim: unwraps the Result to keep the assertions terse.
        fn about_record_test(&self, s: &NewAboutSnapshot) -> AboutRecordOutcome {
            self.about_snapshot_record(s).unwrap()
        }
    }
}
