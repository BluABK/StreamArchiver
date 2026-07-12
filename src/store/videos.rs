//! On-demand video download rows (Videos tab).

use super::*;

impl Store {
    // ----- videos (on-demand downloads) -----

    /// Insert a new video download request (status starts as `queued`).
    pub fn insert_video(&self, v: &Video) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO video(url, title, channel, platform, tool, quality, output_dir,
                filename_template, auth_kind, auth_value, extra_args, auto_title, status, created_at,
                audio_tracks, subtitle_tracks, chat_log, tool_binary)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'queued', ?13, ?14, ?15, ?16, ?17)",
            params![
                v.url,
                v.title,
                v.channel,
                v.platform.as_str(),
                v.tool.as_str(),
                v.quality,
                v.output_dir,
                v.filename_template,
                v.auth_kind.as_str(),
                v.auth_value,
                v.extra_args,
                v.auto_title as i64,
                now_unix(),
                v.audio_tracks,
                v.subtitle_tracks,
                v.chat_log as i64,
                v.tool_binary,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Persist a resolved/display title for a video (used by auto-detect title).
    pub fn set_video_title(&self, id: i64, title: &str) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE video SET title=?2 WHERE id=?1", params![id, title])?;
        Ok(())
    }

    /// Persist a detected channel/uploader name for a video.
    pub fn set_video_channel(&self, id: i64, channel: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE video SET channel=?2 WHERE id=?1",
            params![id, channel],
        )?;
        Ok(())
    }

    /// Mark a video as started (status `downloading` + start time).
    pub fn set_video_started(&self, id: i64, started_at: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE video SET status='downloading', started_at=?2 WHERE id=?1",
            params![id, started_at],
        )?;
        Ok(())
    }

    /// Update only a video's status string (e.g. to `stopped`).
    pub fn set_video_status(&self, id: i64, status: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE video SET status=?2 WHERE id=?1",
            params![id, status],
        )?;
        Ok(())
    }

    /// Finalize a video download with its outcome.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_video(
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
            "UPDATE video SET ended_at=?2, bytes=?3, exit_code=?4, status=?5, output_path=?6,
                log_excerpt=?7 WHERE id=?1",
            params![
                id,
                ended_at,
                bytes,
                exit_code,
                status,
                output_path,
                log_excerpt
            ],
        )?;
        Ok(())
    }

    /// Reset a finished video back to `queued` so it can be retried.
    pub fn reset_video_for_retry(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE video SET status='queued', started_at=NULL, ended_at=NULL, bytes=0,
                exit_code=NULL, log_excerpt='' WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn delete_video(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("DELETE FROM video WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Mark videos still flagged `downloading` (crash leftovers) as `orphaned`.
    /// Excludes videos with a `detached_process` registry entry — those outlived
    /// the app and are reconciled (re-attached or finalized) by the detach path.
    pub fn mark_orphaned_videos(&self, ended_at: i64) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE video SET status='orphaned', ended_at=?1
             WHERE status IN ('downloading','queued')
               AND id NOT IN (SELECT ref_id FROM detached_process WHERE kind = 'video')",
            params![ended_at],
        )?;
        Ok(n)
    }

    pub fn get_video(&self, id: i64) -> Result<Option<Video>> {
        let conn = self.db();
        let v = conn
            .query_row(
                "SELECT id, url, title, platform, tool, quality, output_dir, filename_template,
                    auth_kind, auth_value, extra_args, status, output_path, bytes, exit_code,
                    created_at, started_at, ended_at, auto_title, channel,
                    audio_tracks, subtitle_tracks, chat_log, log_excerpt, tool_binary
                 FROM video WHERE id=?1",
                params![id],
                Self::map_video,
            )
            .optional()?;
        Ok(v)
    }

    /// All videos, newest first.
    pub fn list_videos(&self) -> Result<Vec<Video>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, url, title, platform, tool, quality, output_dir, filename_template,
                auth_kind, auth_value, extra_args, status, output_path, bytes, exit_code,
                created_at, started_at, ended_at, auto_title, channel,
                audio_tracks, subtitle_tracks, chat_log, log_excerpt, tool_binary
             FROM video ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map([], Self::map_video)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn map_video(r: &rusqlite::Row<'_>) -> rusqlite::Result<Video> {
        Ok(Video {
            id: r.get(0)?,
            url: r.get(1)?,
            title: r.get(2)?,
            channel: r.get(19)?,
            platform: Platform::parse(&r.get::<_, String>(3)?),
            tool: Tool::parse(&r.get::<_, String>(4)?),
            tool_binary: r.get(24)?,
            quality: r.get(5)?,
            output_dir: r.get(6)?,
            filename_template: r.get(7)?,
            auth_kind: AuthKind::parse(&r.get::<_, String>(8)?),
            auth_value: r.get(9)?,
            extra_args: r.get(10)?,
            status: r.get(11)?,
            output_path: r.get(12)?,
            bytes: r.get(13)?,
            exit_code: r.get(14)?,
            created_at: r.get(15)?,
            started_at: r.get(16)?,
            ended_at: r.get(17)?,
            auto_title: r.get::<_, i64>(18)? != 0,
            audio_tracks: r.get(20)?,
            subtitle_tracks: r.get(21)?,
            chat_log: r.get::<_, i64>(22)? != 0,
            log_excerpt: r.get(23)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::*;

    #[test]
    fn video_crud_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let mut sample = sample_video();
        sample.auto_title = true;
        sample.audio_tracks = "all".into();
        sample.subtitle_tracks = "en,no".into();
        sample.chat_log = true;
        let id = store.insert_video(&sample).unwrap();

        let v = store.get_video(id).unwrap().unwrap();
        assert_eq!(v.status, "queued");
        assert_eq!(v.tool, Tool::YtDlp);
        // auto_title must survive the round-trip (guards the insert/select/map
        // param + column-index wiring for the v6 column).
        assert!(v.auto_title);
        // Track/chat selection must survive too (the v17 columns).
        assert_eq!(v.audio_tracks, "all");
        assert_eq!(v.subtitle_tracks, "en,no");
        assert!(v.chat_log);
        store.set_video_title(id, "Resolved Title").unwrap();
        assert_eq!(
            store.get_video(id).unwrap().unwrap().title,
            "Resolved Title"
        );
        store.set_video_channel(id, "Some Channel").unwrap();
        assert_eq!(
            store.get_video(id).unwrap().unwrap().channel,
            "Some Channel"
        );

        store.set_video_started(id, 123).unwrap();
        assert_eq!(store.get_video(id).unwrap().unwrap().status, "downloading");

        store
            .finish_video(
                id,
                456,
                1024,
                Some(0),
                "completed",
                "C:/vids/out.mkv",
                "log",
            )
            .unwrap();
        let v = store.get_video(id).unwrap().unwrap();
        assert_eq!(v.status, "completed");
        assert_eq!(v.bytes, 1024);
        assert_eq!(v.output_path, "C:/vids/out.mkv");

        // Orphan recovery only touches in-flight rows, not completed ones.
        let id2 = store.insert_video(&sample_video()).unwrap();
        store.set_video_started(id2, 1).unwrap();
        let n = store.mark_orphaned_videos(999).unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.get_video(id2).unwrap().unwrap().status, "orphaned");
        assert_eq!(store.get_video(id).unwrap().unwrap().status, "completed");

        store.reset_video_for_retry(id2).unwrap();
        assert_eq!(store.get_video(id2).unwrap().unwrap().status, "queued");

        assert_eq!(store.list_videos().unwrap().len(), 2);
        store.delete_video(id2).unwrap();
        assert_eq!(store.list_videos().unwrap().len(), 1);
    }
}
