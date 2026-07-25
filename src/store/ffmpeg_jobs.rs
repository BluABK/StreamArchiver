//! Registry for still-running ffmpeg `-c copy` post-processing passes
//! (chapters/thumbnail embed, remux, gap-splice/head-backfill concat, split-part
//! merge) — parallel to the `detached_process` registry (`src/store/vod.rs`) but
//! for these jobs instead of capture/download/chat tools.

use super::*;

impl Store {
    /// Register a freshly-spawned ffmpeg post-processing job so a later launch
    /// can re-attach to it if it outlives the app. Replaces any prior row for
    /// the same (kind, ref_id) — needed because e.g. `remux_ts_to_mkv_gated`
    /// retries itself on readrate-pacing collapse and must replace, not
    /// duplicate, its row.
    pub fn register_ffmpeg_job(&self, row: &FfmpegJobRow) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM ffmpeg_job WHERE kind=?1 AND ref_id=?2",
            params![row.kind.as_str(), row.ref_id],
        )?;
        conn.execute(
            "INSERT INTO ffmpeg_job(
                 kind, ref_id, pid, proc_start, job_name, tmp_path, final_path,
                 progress_log, total_secs, started_at, spawn_build)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                row.kind.as_str(),
                row.ref_id,
                row.pid as i64,
                row.proc_start as i64,
                row.job_name,
                row.tmp_path,
                row.final_path,
                row.progress_log,
                row.total_secs,
                row.started_at,
                row.spawn_build,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Drop a registry row once its job has been finalized or stopped.
    pub fn clear_ffmpeg_job(&self, kind: FfmpegJobKind, ref_id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM ffmpeg_job WHERE kind=?1 AND ref_id=?2",
            params![kind.as_str(), ref_id],
        )?;
        Ok(())
    }

    /// All registry rows — the startup reconcile reads this to decide what to
    /// re-attach, finalize, or clean up.
    pub fn list_ffmpeg_jobs(&self) -> Result<Vec<FfmpegJobRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT kind, ref_id, pid, proc_start, job_name, tmp_path, final_path,
                    progress_log, total_secs, started_at, spawn_build
             FROM ffmpeg_job ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let kind: String = r.get(0)?;
                Ok(FfmpegJobRow {
                    kind: FfmpegJobKind::from_str(&kind).unwrap_or(FfmpegJobKind::Remux),
                    ref_id: r.get(1)?,
                    pid: r.get::<_, i64>(2)? as u32,
                    proc_start: r.get::<_, i64>(3)? as u64,
                    job_name: r.get(4)?,
                    tmp_path: r.get(5)?,
                    final_path: r.get(6)?,
                    progress_log: r.get(7)?,
                    total_secs: r.get(8)?,
                    started_at: r.get(9)?,
                    spawn_build: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(kind: FfmpegJobKind, ref_id: i64) -> FfmpegJobRow {
        FfmpegJobRow {
            kind,
            ref_id,
            pid: 4242,
            proc_start: 123456789,
            job_name: format!("Local\\StreamArchiver_ffmpeg_{}_{ref_id}", kind.as_str()),
            tmp_path: "C:\\out\\rec.tmp.mkv".into(),
            final_path: "C:\\out\\rec.mkv".into(),
            progress_log: "C:\\cache\\rec.progress.log".into(),
            total_secs: Some(3600),
            started_at: 1_000_000,
            spawn_build: "test-build".into(),
        }
    }

    #[test]
    fn register_list_clear_round_trip() {
        let store = Store::open_in_memory().unwrap();
        store
            .register_ffmpeg_job(&sample_row(FfmpegJobKind::ChaptersEmbed, 1))
            .unwrap();
        store
            .register_ffmpeg_job(&sample_row(FfmpegJobKind::Remux, 2))
            .unwrap();

        let rows = store.list_ffmpeg_jobs().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.kind == FfmpegJobKind::ChaptersEmbed && r.ref_id == 1));
        assert!(rows.iter().any(|r| r.kind == FfmpegJobKind::Remux && r.ref_id == 2));

        store.clear_ffmpeg_job(FfmpegJobKind::ChaptersEmbed, 1).unwrap();
        let rows = store.list_ffmpeg_jobs().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, FfmpegJobKind::Remux);
    }

    #[test]
    fn re_registering_the_same_kind_and_ref_id_replaces_not_duplicates() {
        let store = Store::open_in_memory().unwrap();
        let mut row = sample_row(FfmpegJobKind::GapSplice, 5);
        store.register_ffmpeg_job(&row).unwrap();
        row.pid = 9999;
        store.register_ffmpeg_job(&row).unwrap();

        let rows = store.list_ffmpeg_jobs().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, 9999);
    }
}
