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
    AdBreak, AuthKind, Channel, Container, DetectionMethod, Monitor, MonitorWithChannel, Platform,
    ScheduleSegment, StreamMetaChange, Tool, Video, now_unix,
};

/// Latest schema version understood by this build.
const SCHEMA_VERSION: i64 = 18;

pub struct Store {
    conn: Mutex<Connection>,
}

/// Minimal monitor fields the ad-free (Twitch sub) refresher needs.
pub struct AdFreeRow {
    pub id: i64,
    pub url: String,
    pub ad_free: bool,
    pub ad_free_sub: Option<bool>,
    pub ad_free_sub_at: Option<i64>,
    pub last_state: String,
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
        if version < 10 {
            // The source URL/platform now lives on the monitor (instance), so a
            // channel is a container that can hold instances on *different*
            // platforms. Backfill each instance from its channel's URL so existing
            // single-source channels keep working unchanged.
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN url TEXT NOT NULL DEFAULT '';
                 UPDATE monitor SET url = COALESCE(
                     (SELECT c.url FROM channel c WHERE c.id = monitor.channel_id), '');",
            )?;
            conn.pragma_update(None, "user_version", 10)?;
        }
        if version < 11 {
            // Advertisement breaks detected during a recording (streamlink filters
            // Twitch ads out -> each break is a hard cut in the finished file).
            // `at_secs` is the offset from the take's start; `duration_secs` is the
            // reported ad-pod length. Cascades when the recording row is removed.
            conn.execute_batch(
                r#"
                CREATE TABLE ad_break (
                    id            INTEGER PRIMARY KEY,
                    recording_id  INTEGER NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
                    at_secs       INTEGER NOT NULL,
                    duration_secs INTEGER NOT NULL
                );
                CREATE INDEX idx_ad_break_recording ON ad_break(recording_id);
                "#,
            )?;
            conn.pragma_update(None, "user_version", 11)?;
        }
        if version < 12 {
            // Manually-marked ad-free instance (YouTube membership/Premium, Twitch
            // Turbo/sub): captures won't have ad-break hard cuts. Auto Twitch-sub
            // detection layers on top of this (a later migration).
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN ad_free INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 12)?;
        }
        if version < 13 {
            // Cached auto Twitch-sub ad-free status: ad_free_sub is NULL (unknown /
            // not checked), 0 (checked, not subscribed) or 1 (subscribed);
            // ad_free_sub_at is the last successful check time (for staleness).
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN ad_free_sub INTEGER;
                 ALTER TABLE monitor ADD COLUMN ad_free_sub_at INTEGER;",
            )?;
            conn.pragma_update(None, "user_version", 13)?;
        }
        if version < 14 {
            // Per-instance audio/subtitle track selection (max-archival). Empty
            // preserves the current single-track / no-subtitles behavior, so
            // existing monitors are unchanged until edited; the Add form defaults
            // new monitors to "all".
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN audio_tracks TEXT NOT NULL DEFAULT '';
                 ALTER TABLE monitor ADD COLUMN subtitle_tracks TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 14)?;
        }
        if version < 15 {
            // Title / game-category changes observed during a recording. Twitch
            // Helix has the metadata, but the scheduler pauses polling while a
            // monitor records, so the supervisor polls it and logs changes here.
            // `at_secs` is the offset from the take start; the first row per
            // `kind` ('title'/'category') is the initial value (empty old_value).
            // Cascades when the recording row is removed.
            conn.execute_batch(
                r#"
                CREATE TABLE stream_meta_change (
                    id            INTEGER PRIMARY KEY,
                    recording_id  INTEGER NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
                    at_secs       INTEGER NOT NULL,
                    kind          TEXT NOT NULL,
                    old_value     TEXT NOT NULL DEFAULT '',
                    new_value     TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX idx_meta_change_recording ON stream_meta_change(recording_id);
                "#,
            )?;
            conn.pragma_update(None, "user_version", 15)?;
        }
        if version < 16 {
            // Per-instance chat logging (Twitch IRC sidecar / yt-dlp live_chat).
            // Default 0 leaves existing monitors unchanged; the Add form defaults
            // new monitors on.
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN chat_log INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 16)?;
        }
        if version < 17 {
            // Bring on-demand video downloads to parity with monitors: per-download
            // audio/subtitle track selection and chat logging. Empty/0 defaults
            // leave existing rows behaving exactly as before (no track args).
            conn.execute_batch(
                "ALTER TABLE video ADD COLUMN audio_tracks    TEXT NOT NULL DEFAULT '';
                 ALTER TABLE video ADD COLUMN subtitle_tracks TEXT NOT NULL DEFAULT '';
                 ALTER TABLE video ADD COLUMN chat_log        INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 17)?;
        }
        if version < 18 {
            // Upcoming scheduled streams per monitor (Twitch Helix schedule /
            // YouTube upcoming), refreshed periodically. Replaced wholesale on each
            // refresh; cascades when the monitor is deleted.
            conn.execute_batch(
                "CREATE TABLE schedule_segment (
                    id         INTEGER PRIMARY KEY,
                    monitor_id INTEGER NOT NULL,
                    start_time INTEGER NOT NULL,
                    end_time   INTEGER,
                    title      TEXT NOT NULL DEFAULT '',
                    category   TEXT NOT NULL DEFAULT '',
                    canceled   INTEGER NOT NULL DEFAULT 0,
                    FOREIGN KEY(monitor_id) REFERENCES monitor(id) ON DELETE CASCADE
                );
                CREATE INDEX idx_schedule_monitor ON schedule_segment(monitor_id, start_time);",
            )?;
            conn.pragma_update(None, "user_version", 18)?;
        }
        debug_assert_eq!(SCHEMA_VERSION, 18);
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

    /// All channel containers (including ones with no instances yet), ordered to
    /// match the monitor list (name, then id).
    pub fn list_channels(&self) -> Result<Vec<Channel>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, url, platform, created_at FROM channel
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt
            .query_map([], Self::map_channel)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Create a new empty channel container (no URL/platform of its own; its
    /// instances carry the source URLs). Always inserts a new row.
    pub fn create_container(&self, name: &str) -> Result<i64> {
        self.insert_channel(name, "", Platform::Generic)
    }

    /// Rename a channel container.
    pub fn rename_channel(&self, id: i64, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE channel SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Enable/disable every instance of a channel at once (the channel-level On).
    pub fn set_channel_enabled(&self, channel_id: i64, enabled: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE monitor SET enabled = ?2 WHERE channel_id = ?1",
            params![channel_id, enabled as i64],
        )?;
        Ok(())
    }

    // ----- monitors -----

    #[allow(clippy::too_many_arguments)]
    pub fn insert_monitor(&self, m: &Monitor) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO monitor(channel_id, url, enabled, tool, detection_method, poll_interval_secs,
                quality, output_dir, filename_template, container, capture_from_start, auth_kind,
                auth_value, extra_args, max_concurrent, last_state, ad_free, audio_tracks, subtitle_tracks,
                chat_log)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                m.channel_id,
                m.url,
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
                m.ad_free as i64,
                m.audio_tracks,
                m.subtitle_tracks,
                m.chat_log as i64,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_monitor(&self, m: &Monitor) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE monitor SET url=?2, enabled=?3, tool=?4, detection_method=?5, poll_interval_secs=?6,
                quality=?7, output_dir=?8, filename_template=?9, container=?10, capture_from_start=?11,
                auth_kind=?12, auth_value=?13, extra_args=?14, max_concurrent=?15, ad_free=?16,
                audio_tracks=?17, subtitle_tracks=?18, chat_log=?19 WHERE id=?1",
            params![
                m.id,
                m.url,
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
                m.ad_free as i64,
                m.audio_tracks,
                m.subtitle_tracks,
                m.chat_log as i64,
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

    /// Cache the auto-detected Twitch-subscription ad-free status for a monitor,
    /// but only while a Twitch account is connected, atomically under the single
    /// connection lock: the `EXISTS` check on `connected_key` and the update can't
    /// interleave with a concurrent `disconnect` (which clears that key + the
    /// cache), so a Disconnect landing mid-refresh can't resurrect a stale result.
    /// `sub` is `Some(true)` subscribed / `Some(false)` not / `None` unknown.
    /// Returns whether a row was written.
    pub fn set_monitor_ad_free_sub_if_connected(
        &self,
        id: i64,
        sub: Option<bool>,
        checked_at: i64,
        connected_key: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE monitor SET ad_free_sub = ?2, ad_free_sub_at = ?3
             WHERE id = ?1
               AND EXISTS (SELECT 1 FROM app_settings WHERE key = ?4 AND value <> '')",
            params![id, sub.map(|b| b as i64), checked_at, connected_key],
        )?;
        Ok(n > 0)
    }

    /// Minimal Twitch-monitor rows for the ad-free refresher — just the fields it
    /// needs, avoiding the heavy channel/recording/ad-break join of
    /// [`Self::list_monitors_with_channels`] on its frequent poll tick.
    pub fn twitch_monitors_for_ad_free(&self) -> Result<Vec<AdFreeRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, url, ad_free, ad_free_sub, ad_free_sub_at, last_state
             FROM monitor WHERE url LIKE '%twitch.tv%'",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AdFreeRow {
                    id: r.get(0)?,
                    url: r.get(1)?,
                    ad_free: r.get::<_, i64>(2)? != 0,
                    ad_free_sub: r.get::<_, Option<i64>>(3)?.map(|v| v != 0),
                    ad_free_sub_at: r.get(4)?,
                    last_state: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Clear all cached auto Twitch-sub ad-free results (e.g. on disconnect).
    pub fn clear_ad_free_sub(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE monitor SET ad_free_sub = NULL, ad_free_sub_at = NULL",
            [],
        )?;
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

    /// Record an advertisement break detected during a recording take. `at_secs`
    /// is the offset from the take's start; `duration_secs` the reported ad length.
    pub fn insert_ad_break(
        &self,
        recording_id: i64,
        at_secs: i64,
        duration_secs: i64,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ad_break(recording_id, at_secs, duration_secs) VALUES(?1, ?2, ?3)",
            params![recording_id, at_secs, duration_secs],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All ad breaks for a recording take, ordered by offset (for the cut-list
    /// tooltip/popup).
    pub fn ad_breaks_for_recording(&self, recording_id: i64) -> Result<Vec<AdBreak>> {
        let conn = self.conn.lock().unwrap();
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

    /// Record a title or game/category change observed during a recording take.
    pub fn insert_meta_change(
        &self,
        recording_id: i64,
        at_secs: i64,
        kind: &str,
        old_value: &str,
        new_value: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO stream_meta_change(recording_id, at_secs, kind, old_value, new_value)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![recording_id, at_secs, kind, old_value, new_value],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All metadata changes for a recording take, in chronological order.
    pub fn meta_changes_for_recording(&self, recording_id: i64) -> Result<Vec<StreamMetaChange>> {
        let conn = self.conn.lock().unwrap();
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

    // ----- schedule (upcoming streams) -----

    /// Replace a monitor's stored schedule wholesale with a freshly-fetched set
    /// (delete-then-insert in one transaction). Called by the schedule refresher.
    pub fn replace_schedule(&self, monitor_id: i64, segs: &[ScheduleSegment]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM schedule_segment WHERE monitor_id = ?1",
            params![monitor_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO schedule_segment(monitor_id, start_time, end_time, title, category, canceled)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for s in segs {
                stmt.execute(params![
                    monitor_id,
                    s.start_time,
                    s.end_time,
                    s.title,
                    s.category,
                    s.canceled as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Upcoming (non-canceled, start ≥ `after`) schedule segments for one monitor,
    /// soonest first — for the Next stream popup.
    pub fn schedule_for_monitor(&self, monitor_id: i64, after: i64) -> Result<Vec<ScheduleSegment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, start_time, end_time, title, category, canceled
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The soonest upcoming (non-canceled, start ≥ `after`) stream per monitor:
    /// `(monitor_id, start_time, title)`. Drives the Next stream column. (SQLite
    /// returns the bare `title` from the same row as `MIN(start_time)`.)
    pub fn next_scheduled_streams(&self, after: i64) -> Result<Vec<(i64, i64, String)>> {
        let conn = self.conn.lock().unwrap();
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
                (SELECT COUNT(*) FROM recording rc WHERE rc.monitor_id = m.id),
                m.url,
                (SELECT COUNT(*) FROM ad_break ab WHERE ab.recording_id = r.id),
                COALESCE((SELECT SUM(ab.duration_secs) FROM ad_break ab WHERE ab.recording_id = r.id), 0),
                m.ad_free, m.ad_free_sub, m.audio_tracks, m.subtitle_tracks,
                (SELECT COUNT(*) FROM stream_meta_change smc
                 WHERE smc.recording_id = r.id AND smc.old_value != ''),
                m.chat_log, COALESCE(r.log_excerpt, ''),
                COALESCE((SELECT new_value FROM stream_meta_change smc
                          WHERE smc.recording_id = r.id AND smc.kind = 'title'
                          ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), ''),
                COALESCE((SELECT new_value FROM stream_meta_change smc
                          WHERE smc.recording_id = r.id AND smc.kind = 'category'
                          ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), '')
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
                    url: r.get(29)?,
                    enabled: r.get::<_, i64>(7)? != 0,
                    tool: Tool::parse(&r.get::<_, String>(8)?),
                    detection_method: DetectionMethod::parse(&r.get::<_, String>(9)?),
                    poll_interval_secs: r.get(10)?,
                    quality: r.get(11)?,
                    output_dir: r.get(12)?,
                    filename_template: r.get(13)?,
                    container: Container::parse(&r.get::<_, String>(14)?),
                    capture_from_start: r.get::<_, i64>(15)? != 0,
                    ad_free: r.get::<_, i64>(32)? != 0,
                    auth_kind: AuthKind::parse(&r.get::<_, String>(16)?),
                    auth_value: r.get(17)?,
                    audio_tracks: r.get(34)?,
                    subtitle_tracks: r.get(35)?,
                    chat_log: r.get::<_, i64>(37)? != 0,
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
                    last_recording_ad_count: r.get(30)?,
                    last_recording_ad_secs: r.get(31)?,
                    last_recording_meta_changes: r.get(36)?,
                    last_recording_log: r.get(38)?,
                    last_recording_title: r.get(39)?,
                    last_recording_category: r.get(40)?,
                    ad_free_sub: r.get::<_, Option<i64>>(33)?.map(|v| v != 0),
                    recording_count: r.get(28)?,
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
        let conn = self.conn.lock().unwrap();
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
                              ORDER BY smc.at_secs DESC, smc.id DESC LIMIT 1), '')
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
                    ad_count: r.get(12)?,
                    ad_secs: r.get(13)?,
                    meta_change_count: r.get(14)?,
                    title: r.get(16)?,
                    category: r.get(17)?,
                    log_excerpt: r.get(15)?,
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
                filename_template, auth_kind, auth_value, extra_args, auto_title, status, created_at,
                audio_tracks, subtitle_tracks, chat_log)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'queued', ?13, ?14, ?15, ?16)",
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
                    created_at, started_at, ended_at, auto_title, channel,
                    audio_tracks, subtitle_tracks, chat_log, log_excerpt
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
                created_at, started_at, ended_at, auto_title, channel,
                audio_tracks, subtitle_tracks, chat_log, log_excerpt
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
            audio_tracks: r.get(20)?,
            subtitle_tracks: r.get(21)?,
            chat_log: r.get::<_, i64>(22)? != 0,
            log_excerpt: r.get(23)?,
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
            url: "https://twitch.tv/sample".into(),
            enabled: true,
            tool: Tool::Streamlink,
            detection_method: DetectionMethod::TwitchApi,
            poll_interval_secs: 60,
            quality: "best".into(),
            output_dir: "C:/tmp".into(),
            filename_template: "{name}_{date}_{time}".into(),
            container: Container::Mkv,
            capture_from_start: true,
            ad_free: false,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            audio_tracks: String::new(),
            subtitle_tracks: String::new(),
            chat_log: false,
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
            audio_tracks: String::new(),
            subtitle_tracks: String::new(),
            chat_log: false,
            extra_args: String::new(),
            auto_title: false,
            status: "queued".into(),
            output_path: String::new(),
            bytes: 0,
            exit_code: None,
            log_excerpt: String::new(),
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

    #[test]
    fn ad_free_flag_roundtrips() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        m.ad_free = true;
        let mid = store.insert_monitor(&m).unwrap();
        let row = store.get_monitor_with_channel(mid).unwrap().unwrap();
        assert!(row.monitor.ad_free);

        // update_monitor persists a cleared flag too.
        let mut m2 = row.monitor.clone();
        m2.ad_free = false;
        store.update_monitor(&m2).unwrap();
        assert!(!store.get_monitor_with_channel(mid).unwrap().unwrap().monitor.ad_free);
    }

    #[test]
    fn ad_free_sub_write_is_gated_on_connection() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // Not connected (key absent): the guarded write is a no-op.
        let wrote = store
            .set_monitor_ad_free_sub_if_connected(mid, Some(true), 100, "twitch_user_id")
            .unwrap();
        assert!(!wrote);
        assert_eq!(
            store.get_monitor_with_channel(mid).unwrap().unwrap().ad_free_sub,
            None
        );

        // Connected: the write lands.
        store.set_setting("twitch_user_id", "12345").unwrap();
        let wrote = store
            .set_monitor_ad_free_sub_if_connected(mid, Some(true), 100, "twitch_user_id")
            .unwrap();
        assert!(wrote);
        assert_eq!(
            store.get_monitor_with_channel(mid).unwrap().unwrap().ad_free_sub,
            Some(true)
        );

        // Disconnect (key emptied): a later write can't resurrect a value.
        store.set_setting("twitch_user_id", "").unwrap();
        store.clear_ad_free_sub().unwrap();
        let wrote = store
            .set_monitor_ad_free_sub_if_connected(mid, Some(true), 200, "twitch_user_id")
            .unwrap();
        assert!(!wrote);
        assert_eq!(
            store.get_monitor_with_channel(mid).unwrap().unwrap().ad_free_sub,
            None
        );
    }

    #[test]
    fn ad_break_roundtrip_and_rollups() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let rid = store
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"))
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
    fn meta_change_roundtrip_and_rollups() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let rid = store
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"))
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
    fn schedule_replace_and_next() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        let seg = |start: i64, title: &str, canceled: bool| ScheduleSegment {
            id: 0,
            monitor_id: 0,
            start_time: start,
            end_time: Some(start + 3600),
            title: title.into(),
            category: String::new(),
            canceled,
        };
        // Out of order; a past one, a canceled one, and two future.
        store
            .replace_schedule(
                mid,
                &[
                    seg(5_000, "Later", false),
                    seg(500, "Past", false),
                    seg(3_000, "Canceled soon", true),
                    seg(2_000, "Next up", false),
                ],
            )
            .unwrap();

        // Upcoming (start >= now), non-canceled, soonest first.
        let upcoming = store.schedule_for_monitor(mid, 1_000).unwrap();
        assert_eq!(
            upcoming.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            vec!["Next up", "Later"],
        );
        // The soonest future per monitor drives the Next stream column.
        let next = store.next_scheduled_streams(1_000).unwrap();
        assert_eq!(next, vec![(mid, 2_000, "Next up".to_string())]);

        // Replace wipes the old set.
        store.replace_schedule(mid, &[seg(9_000, "Fresh", false)]).unwrap();
        let next = store.next_scheduled_streams(1_000).unwrap();
        assert_eq!(next, vec![(mid, 9_000, "Fresh".to_string())]);

        // Deleting the monitor cascades to its schedule.
        store.delete_monitor(mid).unwrap();
        assert!(store.schedule_for_monitor(mid, 0).unwrap().is_empty());
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
