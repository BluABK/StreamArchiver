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
    Channel, Container, DetectionMethod, Monitor, MonitorWithChannel, Platform, Tool, now_unix,
};

/// Latest schema version understood by this build.
const SCHEMA_VERSION: i64 = 3;

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
        debug_assert_eq!(SCHEMA_VERSION, 3);
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
                quality, output_dir, filename_template, container, capture_from_start, extra_args, max_concurrent, last_state)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                extra_args=?11, max_concurrent=?12 WHERE id=?1",
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
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO recording(monitor_id, started_at, output_path, status, went_live_at, went_live_approx)
             VALUES(?1, ?2, ?3, 'recording', ?4, ?5)",
            params![monitor_id, started_at, output_path, went_live_at, went_live_approx as i64],
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
                m.extra_args, m.max_concurrent, m.last_checked_at, m.last_state,
                r.started_at, r.ended_at, r.status, r.went_live_at, r.went_live_approx
             FROM monitor m
             JOIN channel c ON c.id = m.channel_id
             LEFT JOIN recording r
                ON r.id = (SELECT id FROM recording r2 WHERE r2.monitor_id = m.id ORDER BY r2.id DESC LIMIT 1)
             ORDER BY c.name COLLATE NOCASE, m.id",
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
                    extra_args: r.get(16)?,
                    max_concurrent: r.get(17)?,
                    last_checked_at: r.get(18)?,
                    last_state: r.get(19)?,
                };
                Ok(MonitorWithChannel {
                    channel,
                    monitor,
                    last_recording_started: r.get(20)?,
                    last_recording_ended: r.get(21)?,
                    last_recording_status: r.get(22)?,
                    last_recording_went_live: r.get(23)?,
                    last_recording_went_live_approx: r.get::<_, Option<i64>>(24)?.unwrap_or(0) != 0,
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
