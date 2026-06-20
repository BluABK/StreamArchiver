//! SQLite-backed persistence (rusqlite, WAL) — the source of truth for
//! channels, monitors, recordings, and key/value settings.
//!
//! rusqlite is synchronous; the connection is wrapped in a `Mutex`. Config CRUD
//! happens on the UI thread (low volume); background tasks will access the same
//! `Arc<Store>` via `spawn_blocking`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::models::{
    AuthKind, Channel, Container, DetectionMethod, Monitor, MonitorWithChannel, Platform, Tool,
    Video, now_unix,
};

/// Latest schema version understood by this build.
const SCHEMA_VERSION: i64 = 9;

pub struct Store {
    conn: Mutex<Connection>,
}

/// Row summary for the `--recordings` diagnostic.
pub struct RecInfo {
    pub id: i64,
    pub monitor_id: i64,
    pub status: String,
    pub bytes: i64,
    pub started_at: i64,
    pub went_live_at: Option<i64>,
    pub went_live_approx: bool,
    pub output_path: String,
}

impl Store {
    /// Open (or create) the database at `path`, set pragmas, and migrate.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;
        Self::configure(&conn)?;
        let store = Store {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// In-memory store, used by tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        let store = Store {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(
                r#"
                CREATE TABLE channel (
                    id          INTEGER PRIMARY KEY,
                    name        TEXT NOT NULL,
                    url         TEXT NOT NULL,
                    platform    TEXT NOT NULL,
                    created_at  INTEGER NOT NULL
                );

                CREATE TABLE monitor (
                    id                INTEGER PRIMARY KEY,
                    channel_id        INTEGER NOT NULL REFERENCES channel(id) ON DELETE CASCADE,
                    enabled           INTEGER NOT NULL DEFAULT 1,
                    tool              TEXT NOT NULL,
                    detection_method  TEXT NOT NULL,
                    poll_interval_secs INTEGER NOT NULL DEFAULT 60,
                    quality           TEXT NOT NULL DEFAULT 'best',
                    output_dir        TEXT NOT NULL,
                    filename_template TEXT NOT NULL DEFAULT '',
                    container         TEXT NOT NULL DEFAULT 'mkv',
                    extra_args        TEXT NOT NULL DEFAULT '',
                    max_concurrent    INTEGER NOT NULL DEFAULT 1,
                    last_checked_at   INTEGER,
                    last_state        TEXT NOT NULL DEFAULT 'idle'
                );

                CREATE TABLE recording (
                    id           INTEGER PRIMARY KEY,
                    monitor_id   INTEGER NOT NULL REFERENCES monitor(id) ON DELETE CASCADE,
                    started_at   INTEGER NOT NULL,
                    ended_at     INTEGER,
                    output_path  TEXT,
                    bytes        INTEGER NOT NULL DEFAULT 0,
                    exit_code    INTEGER,
                    status       TEXT NOT NULL DEFAULT 'recording',
                    log_excerpt  TEXT NOT NULL DEFAULT ''
                );

                CREATE TABLE app_settings (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                CREATE INDEX idx_monitor_channel ON monitor(channel_id);
                CREATE INDEX idx_recording_monitor ON recording(monitor_id);
                "#,
            )?;
            conn.pragma_update(None, "user_version", 1)?;
        }
        if version < 2 {
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN capture_from_start INTEGER NOT NULL DEFAULT 1;",
            )?;
            conn.pragma_update(None, "user_version", 2)?;
        }
        if version < 3 {
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN went_live_at INTEGER;
                 ALTER TABLE recording ADD COLUMN went_live_approx INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 3)?;
        }
        if version < 4 {
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN auth_kind TEXT NOT NULL DEFAULT 'inherit';
                 ALTER TABLE monitor ADD COLUMN auth_value TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 4)?;
        }
        if version < 5 {
            conn.execute_batch(
                r#"
                CREATE TABLE video (
                    id                INTEGER PRIMARY KEY,
                    url               TEXT NOT NULL,
                    title             TEXT NOT NULL DEFAULT '',
                    platform          TEXT NOT NULL,
                    tool              TEXT NOT NULL,
                    quality           TEXT NOT NULL DEFAULT 'best',
                    output_dir        TEXT NOT NULL,
                    filename_template TEXT NOT NULL DEFAULT '',
                    auth_kind         TEXT NOT NULL DEFAULT 'inherit',
                    auth_value        TEXT NOT NULL DEFAULT '',
                    extra_args        TEXT NOT NULL DEFAULT '',
                    status            TEXT NOT NULL DEFAULT 'queued',
                    output_path       TEXT NOT NULL DEFAULT '',
                    bytes             INTEGER NOT NULL DEFAULT 0,
                    exit_code         INTEGER,
                    log_excerpt       TEXT NOT NULL DEFAULT '',
                    created_at        INTEGER NOT NULL,
                    started_at        INTEGER,
                    ended_at          INTEGER
                );
                "#,
            )?;
            conn.pragma_update(None, "user_version", 5)?;
        }
        if version < 6 {
            conn.execute_batch(
                "ALTER TABLE video ADD COLUMN auto_title INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 6)?;
        }
        if version < 7 {
            conn.execute_batch("ALTER TABLE video ADD COLUMN channel TEXT NOT NULL DEFAULT '';")?;
            conn.pragma_update(None, "user_version", 7)?;
        }
        if version < 8 {
            // Resolved "missed beginning" for a recording (NULL until confirmed);
            // 0 once a from-start capture has caught up to live (full coverage).
            conn.execute_batch("ALTER TABLE recording ADD COLUMN lost_secs INTEGER;")?;
            conn.pragma_update(None, "user_version", 8)?;
        }
        if version < 9 {
            // Platform stream/video id (Twitch stream id, YouTube video id, Kick
            // livestream id) when detection knows it — used to group recording
            // takes of the same broadcast. NULL for id-less methods (scrape etc.).
            conn.execute_batch("ALTER TABLE recording ADD COLUMN stream_id TEXT;")?;
            conn.pragma_update(None, "user_version", 9)?;
        }
        debug_assert_eq!(SCHEMA_VERSION, 9);
        Ok(())
    }

    // ----- settings (key/value, also used for credentials) -----

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let value = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ----- channels -----

    pub fn find_channel_by_url(&self, url: &str) -> Result<Option<Channel>> {
        let conn = self.conn.lock().unwrap();
        let ch = conn
            .query_row(
                "SELECT id, name, url, platform, created_at FROM channel WHERE url = ?1",
                params![url],
                Self::map_channel,
            )
            .optional()?;
        Ok(ch)
    }

    pub fn insert_channel(&self, name: &str, url: &str, platform: Platform) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO channel(name, url, platform, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![name, url, platform.as_str(), now_unix()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Get an existing channel by URL or create it.
    pub fn upsert_channel(&self, name: &str, url: &str, platform: Platform) -> Result<i64> {
        if let Some(existing) = self.find_channel_by_url(url)? {
            return Ok(existing.id);
        }
        self.insert_channel(name, url, platform)
    }

    pub fn delete_channel(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM channel WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ----- monitors -----

    #[allow(clippy::too_many_arguments)]
    pub fn insert_monitor(&self, m: &Monitor) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO monitor(channel_id, enabled, tool, detection_method, poll_interval_secs,
                quality, output_dir, filename_template, container, capture_from_start, auth_kind,
                auth_value, extra_args, max_concurrent, last_state)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                m.channel_id,
                m.enabled as i64,
                m.tool.as_str(),
                m.detection_method.as_str(),
                m.poll_interval_secs,
                m.quality,
                m.output_dir,
                m.filename_template,
                m.container.as_str(),
                m.capture_from_start as i64,
                m.auth_kind.as_str(),
                m.auth_value,
                m.extra_args,
                m.max_concurrent,
                m.last_state,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_monitor(&self, m: &Monitor) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE monitor SET enabled=?2, tool=?3, detection_method=?4, poll_interval_secs=?5,
                quality=?6, output_dir=?7, filename_template=?8, container=?9, capture_from_start=?10,
                auth_kind=?11, auth_value=?12, extra_args=?13, max_concurrent=?14 WHERE id=?1",
            params![
                m.id,
                m.enabled as i64,
                m.tool.as_str(),
                m.detection_method.as_str(),
                m.poll_interval_secs,
                m.quality,
                m.output_dir,
                m.filename_template,
                m.container.as_str(),
                m.capture_from_start as i64,
                m.auth_kind.as_str(),
                m.auth_value,
                m.extra_args,
                m.max_concurrent,
            ],
        )?;
        Ok(())
    }

    /// Persist a detection result: last observed state + check timestamp.
    pub fn set_monitor_check_result(&self, id: i64, state: &str, checked_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE monitor SET last_state = ?2, last_checked_at = ?3 WHERE id = ?1",
            params![id, state, checked_at],
        )?;
        Ok(())
    }

    pub fn set_monitor_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE monitor SET enabled=?2 WHERE id=?1",
            params![id, enabled as i64],
        )?;
        Ok(())
    }

    pub fn delete_monitor(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM monitor WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Fetch a single monitor (joined with its channel) by monitor id.
    pub fn get_monitor_with_channel(&self, id: i64) -> Result<Option<MonitorWithChannel>> {
        Ok(self
            .list_monitors_with_channels()?
            .into_iter()
            .find(|r| r.monitor.id == id))
    }

    // ----- recordings -----

    pub fn insert_recording(
        &self,
        monitor_id: i64,
        started_at: i64,
        output_path: &str,
        went_live_at: Option<i64>,
        went_live_approx: bool,
        stream_id: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO recording(monitor_id, started_at, output_path, status, went_live_at, went_live_approx, stream_id)
             VALUES(?1, ?2, ?3, 'recording', ?4, ?5, ?6)",
            params![monitor_id, started_at, output_path, went_live_at, went_live_approx as i64, stream_id],
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE recording SET ended_at=?2, bytes=?3, exit_code=?4, status=?5, output_path=?6, log_excerpt=?7 WHERE id=?1",
            params![id, ended_at, bytes, exit_code, status, output_path, log_excerpt],
        )?;
        Ok(())
    }

    /// Remove a recording (take) row from the history. The captured file on disk
    /// is left untouched. Refuses an in-progress ('recording') take so we never
    /// orphan a running capture from its history row; returns the rows removed.
    pub fn delete_recording(&self, id: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM recording WHERE id = ?1 AND status <> 'recording'",
            params![id],
        )?;
        Ok(n)
    }

    /// Set the resolved "missed footage" (seconds) for a recording. Used by the
    /// from-start catch-up watcher (0 on catch-up) and finalize (the residual).
    pub fn set_recording_lost_secs(&self, id: i64, lost_secs: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE recording SET lost_secs=?2 WHERE id=?1",
            params![id, lost_secs],
        )?;
        Ok(())
    }

    /// Mark any recordings still flagged 'recording' (i.e. left over from a
    /// crash) as 'orphaned'. Returns the number updated. Called on startup.
    pub fn mark_orphaned_recordings(&self, ended_at: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE recording SET status='orphaned', ended_at=?1 WHERE status='recording'",
            params![ended_at],
        )?;
        Ok(n)
    }

    /// All monitors joined with their channel, ordered by channel name.
    pub fn list_monitors_with_channels(&self) -> Result<Vec<MonitorWithChannel>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT
                c.id, c.name, c.url, c.platform, c.created_at,
                m.id, m.channel_id, m.enabled, m.tool, m.detection_method, m.poll_interval_secs,
                m.quality, m.output_dir, m.filename_template, m.container, m.capture_from_start,
                m.auth_kind, m.auth_value, m.extra_args, m.max_concurrent, m.last_checked_at,
                m.last_state,
                r.started_at, r.ended_at, r.status, r.went_live_at, r.went_live_approx, r.lost_secs,
                (SELECT COUNT(*) FROM recording rc WHERE rc.monitor_id = m.id)
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
                };
                let monitor = Monitor {
                    id: r.get(5)?,
                    channel_id: r.get(6)?,
                    enabled: r.get::<_, i64>(7)? != 0,
                    tool: Tool::parse(&r.get::<_, String>(8)?),
                    detection_method: DetectionMethod::parse(&r.get::<_, String>(9)?),
                    poll_interval_secs: r.get(10)?,
                    quality: r.get(11)?,
                    output_dir: r.get(12)?,
                    filename_template: r.get(13)?,
                    container: Container::parse(&r.get::<_, String>(14)?),
                    capture_from_start: r.get::<_, i64>(15)? != 0,
                    auth_kind: AuthKind::parse(&r.get::<_, String>(16)?),
                    auth_value: r.get(17)?,
                    extra_args: r.get(18)?,
                    max_concurrent: r.get(19)?,
                    last_checked_at: r.get(20)?,
                    last_state: r.get(21)?,
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
                    recording_count: r.get(28)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All recording takes for a monitor (oldest first), for the history tree.
    pub fn recordings_for_monitor(&self, monitor_id: i64) -> Result<Vec<crate::models::Recording>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status, bytes, exit_code,
                    COALESCE(output_path, ''), went_live_at, went_live_approx, lost_secs, stream_id
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Recent recordings, newest first.
    pub fn recent_recordings(&self, limit: i64) -> Result<Vec<RecInfo>> {
        let conn = self.conn.lock().unwrap();
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

    // ----- videos (on-demand downloads) -----

    /// Insert a new video download request (status starts as `queued`).
    pub fn insert_video(&self, v: &Video) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO video(url, title, channel, platform, tool, quality, output_dir,
                filename_template, auth_kind, auth_value, extra_args, auto_title, status, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'queued', ?13)",
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
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Persist a resolved/display title for a video (used by auto-detect title).
    pub fn set_video_title(&self, id: i64, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE video SET title=?2 WHERE id=?1", params![id, title])?;
        Ok(())
    }

    /// Persist a detected channel/uploader name for a video.
    pub fn set_video_channel(&self, id: i64, channel: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE video SET channel=?2 WHERE id=?1",
            params![id, channel],
        )?;
        Ok(())
    }

    /// Mark a video as started (status `downloading` + start time).
    pub fn set_video_started(&self, id: i64, started_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE video SET status='downloading', started_at=?2 WHERE id=?1",
            params![id, started_at],
        )?;
        Ok(())
    }

    /// Update only a video's status string (e.g. to `stopped`).
    pub fn set_video_status(&self, id: i64, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE video SET status='queued', started_at=NULL, ended_at=NULL, bytes=0,
                exit_code=NULL, log_excerpt='' WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn delete_video(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM video WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Mark videos still flagged `downloading` (crash leftovers) as `orphaned`.
    pub fn mark_orphaned_videos(&self, ended_at: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE video SET status='orphaned', ended_at=?1 WHERE status IN ('downloading','queued')",
            params![ended_at],
        )?;
        Ok(n)
    }

    pub fn get_video(&self, id: i64) -> Result<Option<Video>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT id, url, title, platform, tool, quality, output_dir, filename_template,
                    auth_kind, auth_value, extra_args, status, output_path, bytes, exit_code,
                    created_at, started_at, ended_at, auto_title, channel
                 FROM video WHERE id=?1",
                params![id],
                Self::map_video,
            )
            .optional()?;
        Ok(v)
    }

    /// All videos, newest first.
    pub fn list_videos(&self) -> Result<Vec<Video>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, url, title, platform, tool, quality, output_dir, filename_template,
                auth_kind, auth_value, extra_args, status, output_path, bytes, exit_code,
                created_at, started_at, ended_at, auto_title, channel
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
        })
    }

    fn map_channel(r: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
        Ok(Channel {
            id: r.get(0)?,
            name: r.get(1)?,
            url: r.get(2)?,
            platform: Platform::parse(&r.get::<_, String>(3)?),
            created_at: r.get(4)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_monitor(channel_id: i64) -> Monitor {
        Monitor {
            id: 0,
            channel_id,
            enabled: true,
            tool: Tool::Streamlink,
            detection_method: DetectionMethod::TwitchApi,
            poll_interval_secs: 60,
            quality: "best".into(),
            output_dir: "C:/tmp".into(),
            filename_template: "{name}_{date}_{time}".into(),
            container: Container::Mkv,
            capture_from_start: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            extra_args: String::new(),
            max_concurrent: 1,
            last_checked_at: None,
            last_state: "idle".into(),
        }
    }

    #[test]
    fn migrate_and_crud_roundtrip() {
        let store = Store::open_in_memory().unwrap();

        // upsert_channel is idempotent on URL.
        let c1 = store
            .upsert_channel("Alice", "https://twitch.tv/alice", Platform::Twitch)
            .unwrap();
        let c2 = store
            .upsert_channel("Alice", "https://twitch.tv/alice", Platform::Twitch)
            .unwrap();
        assert_eq!(c1, c2);

        // two monitor instances for the same channel (streamlink + yt-dlp).
        let mut m = sample_monitor(c1);
        let m1 = store.insert_monitor(&m).unwrap();
        m.tool = Tool::YtDlp;
        m.container = Container::Ts;
        let m2 = store.insert_monitor(&m).unwrap();
        assert_ne!(m1, m2);

        let rows = store.list_monitors_with_channels().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.channel.id == c1));
        assert!(rows.iter().any(|r| r.monitor.container == Container::Ts));

        store.set_monitor_enabled(m1, false).unwrap();
        let rows = store.list_monitors_with_channels().unwrap();
        assert!(
            !rows
                .iter()
                .find(|r| r.monitor.id == m1)
                .unwrap()
                .monitor
                .enabled
        );

        store.delete_monitor(m2).unwrap();
        assert_eq!(store.list_monitors_with_channels().unwrap().len(), 1);

        // deleting the channel cascades to monitors.
        store.delete_channel(c1).unwrap();
        assert_eq!(store.list_monitors_with_channels().unwrap().len(), 0);
    }

    fn sample_video() -> Video {
        Video {
            id: 0,
            url: "https://youtube.com/watch?v=abc".into(),
            title: "My VOD".into(),
            channel: String::new(),
            platform: Platform::YouTube,
            tool: Tool::YtDlp,
            quality: "best".into(),
            output_dir: "C:/vids".into(),
            filename_template: "{name}_{date}".into(),
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            extra_args: String::new(),
            auto_title: false,
            status: "queued".into(),
            output_path: String::new(),
            bytes: 0,
            exit_code: None,
            created_at: 0,
            started_at: None,
            ended_at: None,
        }
    }

    #[test]
    fn video_crud_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let mut sample = sample_video();
        sample.auto_title = true;
        let id = store.insert_video(&sample).unwrap();

        let v = store.get_video(id).unwrap().unwrap();
        assert_eq!(v.status, "queued");
        assert_eq!(v.tool, Tool::YtDlp);
        // auto_title must survive the round-trip (guards the insert/select/map
        // param + column-index wiring for the v6 column).
        assert!(v.auto_title);
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

    #[test]
    fn settings_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.get_setting("twitch_client_id").unwrap(), None);
        store.set_setting("twitch_client_id", "abc123").unwrap();
        store.set_setting("twitch_client_id", "xyz789").unwrap();
        assert_eq!(
            store.get_setting("twitch_client_id").unwrap().as_deref(),
            Some("xyz789")
        );
    }
}
