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
    AdBreak, AuthKind, Channel, Container, DetachedKind, DetachedRow, DetectionMethod, GlobalStats,
    Monitor, MonitorWithChannel, Platform, ScheduleSegment, StreamMetaChange, Tool, UpcomingStream,
    Video, now_unix,
};

/// Latest schema version understood by this build.
const SCHEMA_VERSION: i64 = 30;

pub struct Store {
    conn: Mutex<Connection>,
}

/// One archived community-post image (schema v28 `community_post_archive`), as
/// returned by [`Store::community_post_get`]. `decoded_json` is the cached
/// `Vec<ScheduleSegment>` when `ocr_attempted` is set (empty string before the
/// first OCR).
pub struct ArchivedPost {
    // Retained for the (future) "view archived posts" UI even though the OCR walk
    // already knows the hash and reads the file via its own path.
    #[allow(dead_code)]
    pub content_hash: String,
    #[allow(dead_code)]
    pub local_path: String,
    pub ocr_attempted: bool,
    #[allow(dead_code)]
    pub decoded_events: i64,
    pub decoded_json: String,
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
    /// Acquire the DB connection, logging a warning when contention caused a
    /// wait longer than 50 ms. `#[track_caller]` embeds the caller's source
    /// location in the log line so the slow call-site is immediately visible.
    #[track_caller]
    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        let t = std::time::Instant::now();
        let g = self.conn.lock().unwrap();
        let ms = t.elapsed().as_millis();
        if ms >= 50 {
            let loc = std::panic::Location::caller();
            tracing::warn!(
                wait_ms = ms,
                file = loc.file(),
                line = loc.line(),
                "store: slow DB lock – another thread held the connection"
            );
        } else if ms >= 5 {
            let loc = std::panic::Location::caller();
            tracing::debug!(
                wait_ms = ms,
                file = loc.file(),
                line = loc.line(),
                "store: DB lock wait"
            );
        }
        g
    }

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
        let conn = self.db();
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
        if version < 19 {
            // Schedule segments can now come from more than one source per monitor
            // (the platform's published schedule, or matched Discord events), so each
            // row records its `source` and is replaced per-source. Existing rows came
            // from the platform fetchers, so they default to 'platform'.
            conn.execute_batch(
                "ALTER TABLE schedule_segment ADD COLUMN source TEXT NOT NULL DEFAULT 'platform';",
            )?;
            conn.pragma_update(None, "user_version", 19)?;
        }
        if version < 20 {
            // Optional custom hex color for a channel container (e.g. "#ff9800").
            // Empty string = use the auto-assigned palette color.
            conn.execute_batch(
                "ALTER TABLE channel ADD COLUMN color TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 20)?;
        }
        if version < 21 {
            // YouTube video ID for each scheduled segment (e.g. "dQw4w9WgXcQ").
            // Populated by the lockupViewModel scraper; used to batch videos.list
            // API calls for exact scheduledStartTime. NULL for Twitch/Discord rows
            // and pre-21 YouTube rows.
            conn.execute_batch(
                "ALTER TABLE schedule_segment ADD COLUMN video_id TEXT;",
            )?;
            conn.pragma_update(None, "user_version", 21)?;
        }
        if version < 22 {
            // Per-monitor asset archival: download stream thumbnail and
            // channel/chat assets (icon, banner, badges, emotes) alongside recordings.
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN fetch_thumbnail   INTEGER NOT NULL DEFAULT 0;\
                 ALTER TABLE monitor ADD COLUMN fetch_chat_assets INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 22)?;
        }
        if version < 23 {
            // SABR dual capture: a per-monitor toggle to also run a DASH companion
            // capture, plus a take_group key that links the two recordings (SABR
            // primary + DASH companion) produced by one capture attempt.
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN dual_capture INTEGER NOT NULL DEFAULT 0;\
                 ALTER TABLE recording ADD COLUMN take_group TEXT;",
            )?;
            conn.pragma_update(None, "user_version", 23)?;
        }
        if version < 24 {
            // Detached downloads: a persistent registry of running tool processes
            // (recordings / on-demand videos / chat sidecars) so a relaunch can
            // re-attach to ones that outlived the app instead of orphaning them.
            // Written right after a tool spawns (so a hard crash is recoverable too)
            // and deleted at finalize/stop. `proc_start` + `job_name` make the PID
            // re-use-safe; `spawn_build` records which app build started it so a
            // newer build can apply per-build compat fixups on re-attach.
            //
            // Also make ad_break re-scan idempotent: dedupe any existing
            // (recording_id, at_secs) pairs, then enforce uniqueness so a
            // re-attach can't double-insert a break it already persisted.
            conn.execute_batch(
                r#"
                CREATE TABLE detached_process (
                    id            INTEGER PRIMARY KEY,
                    kind          TEXT NOT NULL,
                    ref_id        INTEGER NOT NULL,
                    monitor_id    INTEGER,
                    pid           INTEGER NOT NULL,
                    proc_start    INTEGER NOT NULL,
                    job_name      TEXT NOT NULL DEFAULT '',
                    log_path      TEXT NOT NULL DEFAULT '',
                    capture_path  TEXT NOT NULL DEFAULT '',
                    final_path    TEXT NOT NULL DEFAULT '',
                    remux_to_mkv  INTEGER NOT NULL DEFAULT 0,
                    take_group    TEXT,
                    spawn_build   TEXT NOT NULL DEFAULT '',
                    started_at    INTEGER NOT NULL,
                    -- 1 for the DASH companion leg of a dual capture (occupies the
                    -- secondary active map); 0 for the primary / videos / chat.
                    secondary     INTEGER NOT NULL DEFAULT 0,
                    -- Carried so a re-attach can finalize exactly like the in-session
                    -- path: stream_id for the {video_id} filename var, went_live_at
                    -- for the ad-cut anchor and lost-time accounting.
                    stream_id     TEXT,
                    went_live_at  INTEGER
                );
                CREATE INDEX idx_detached_kind_ref ON detached_process(kind, ref_id);

                DELETE FROM ad_break WHERE id NOT IN (
                    SELECT MIN(id) FROM ad_break GROUP BY recording_id, at_secs
                );
                CREATE UNIQUE INDEX idx_ad_break_unique ON ad_break(recording_id, at_secs);
                "#,
            )?;
            conn.pragma_update(None, "user_version", 24)?;
        }
        if version < 25 {
            // Per-channel "preferred asset platform": which platform's profile
            // pic / banner represents the container (it can hold the same creator
            // on Twitch + YouTube + Kick, each with its own assets, now stored in
            // per-platform asset subdirs). Empty = auto (first available).
            conn.execute_batch(
                "ALTER TABLE channel ADD COLUMN preferred_platform TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 25)?;
        }
        if version < 26 {
            // Per-monitor option to prefer the stream thumbnail (fetched at
            // recording start) over the channel's static banner in the
            // recording-started desktop notification. Off by default; most useful
            // for YouTube where each stream has a unique, informative thumbnail.
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN thumbnail_in_toast INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 26)?;
        }
        if version < 27 {
            // Independent channel-level enabled flag: the channel checkbox now
            // reads/writes channel.enabled rather than cascading to all instances.
            // Existing channels default to enabled so nothing changes on upgrade.
            conn.execute_batch(
                "ALTER TABLE channel ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;",
            )?;
            conn.pragma_update(None, "user_version", 27)?;
        }
        if version < 28 {
            // Archive of every YouTube community-post image we download while
            // scanning for a schedule. Two jobs: (1) a durable record of what was
            // pulled (url/path/when), queryable later; (2) a per-image OCR cache —
            // `content_hash` keys an unchanged image to its already-decoded events
            // (`decoded_json`), so a new post pushing old ones down the feed no
            // longer forces a full re-OCR of the unchanged images. `content_hash`
            // is a decimal string of an fnv64 (u64) — TEXT, because SQLite INTEGER
            // is i64 and would overflow the high-bit hashes.
            conn.execute_batch(
                "CREATE TABLE community_post_archive (
                    id             INTEGER PRIMARY KEY,
                    monitor_id     INTEGER NOT NULL,
                    source         TEXT NOT NULL,
                    image_url      TEXT NOT NULL,
                    content_hash   TEXT NOT NULL,
                    local_path     TEXT NOT NULL,
                    fetched_at     INTEGER NOT NULL,
                    ocr_attempted  INTEGER NOT NULL DEFAULT 0,
                    decoded_events INTEGER NOT NULL DEFAULT 0,
                    decoded_json   TEXT NOT NULL DEFAULT '',
                    FOREIGN KEY(monitor_id) REFERENCES monitor(id) ON DELETE CASCADE
                );
                CREATE UNIQUE INDEX idx_community_post_archive_uniq
                    ON community_post_archive(monitor_id, content_hash);",
            )?;
            conn.pragma_update(None, "user_version", 28)?;
        }
        if version < 29 {
            // Per-take free-text notes, editable in the recording properties dialog.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN notes TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 29)?;
        }
        if version < 30 {
            // VOD tracking: Twitch VOD id, availability state, and muted-segment
            // seconds for each recording take. The background checker populates
            // these after the stream ends; NULL columns mean "not applicable"
            // (non-Twitch) or "legacy row created before this migration".
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN vod_id TEXT;
                 ALTER TABLE recording ADD COLUMN vod_state TEXT;
                 ALTER TABLE recording ADD COLUMN vod_muted_secs INTEGER;",
            )?;
            conn.pragma_update(None, "user_version", 30)?;
        }
        debug_assert_eq!(SCHEMA_VERSION, 30);
        Ok(())
    }

    // ----- settings (key/value, also used for credentials) -----

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.db();
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
        let conn = self.db();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

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

    /// Whether a periodic background job (see `events::TOGGLEABLE_JOBS`) is enabled.
    /// Default `true`; only an explicit `"0"` disables it.
    pub fn job_enabled(&self, key: &str) -> bool {
        self.get_setting(key)
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true)
    }

    // ----- channels -----

    pub fn find_channel_by_url(&self, url: &str) -> Result<Option<Channel>> {
        let conn = self.db();
        let ch = conn
            .query_row(
                "SELECT id, name, url, platform, created_at, color, preferred_platform, enabled \
                 FROM channel WHERE url = ?1",
                params![url],
                Self::map_channel,
            )
            .optional()?;
        Ok(ch)
    }

    pub fn insert_channel(&self, name: &str, url: &str, platform: Platform) -> Result<i64> {
        let conn = self.db();
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
        let conn = self.db();
        conn.execute("DELETE FROM channel WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// All channel containers (including ones with no instances yet), ordered to
    /// match the monitor list (name, then id).
    pub fn list_channels(&self) -> Result<Vec<Channel>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, name, url, platform, created_at, color, preferred_platform, enabled FROM channel
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
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Set (or clear) the custom hex color for a channel container.
    /// Pass an empty string to revert to the automatic palette color.
    pub fn set_channel_color(&self, id: i64, color: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET color = ?2 WHERE id = ?1",
            params![id, color],
        )?;
        Ok(())
    }

    /// Set (or clear) the preferred asset platform for a channel container — the
    /// platform whose profile pic / banner represents it. `None` reverts to auto
    /// (the first instance-platform that has a fetched icon).
    pub fn set_channel_preferred_platform(
        &self,
        id: i64,
        platform: Option<Platform>,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET preferred_platform = ?2 WHERE id = ?1",
            params![id, platform.map(|p| p.as_str()).unwrap_or("")],
        )?;
        Ok(())
    }

    /// Enable/disable a channel container's own flag. Does NOT touch individual
    /// instance (monitor) enabled states — those are independent.
    pub fn set_channel_enabled(&self, channel_id: i64, enabled: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET enabled = ?2 WHERE id = ?1",
            params![channel_id, enabled as i64],
        )?;
        Ok(())
    }

    // ----- monitors -----

    #[allow(clippy::too_many_arguments)]
    pub fn insert_monitor(&self, m: &Monitor) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO monitor(channel_id, url, enabled, tool, detection_method, poll_interval_secs,
                quality, output_dir, filename_template, container, capture_from_start, auth_kind,
                auth_value, extra_args, max_concurrent, last_state, ad_free, audio_tracks, subtitle_tracks,
                chat_log, fetch_thumbnail, fetch_chat_assets, dual_capture, thumbnail_in_toast)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
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
                m.fetch_thumbnail as i64,
                m.fetch_chat_assets as i64,
                m.dual_capture as i64,
                m.thumbnail_in_toast as i64,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_monitor(&self, m: &Monitor) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET url=?2, enabled=?3, tool=?4, detection_method=?5, poll_interval_secs=?6,
                quality=?7, output_dir=?8, filename_template=?9, container=?10, capture_from_start=?11,
                auth_kind=?12, auth_value=?13, extra_args=?14, max_concurrent=?15, ad_free=?16,
                audio_tracks=?17, subtitle_tracks=?18, chat_log=?19,
                fetch_thumbnail=?20, fetch_chat_assets=?21, dual_capture=?22,
                thumbnail_in_toast=?23 WHERE id=?1",
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
                m.fetch_thumbnail as i64,
                m.fetch_chat_assets as i64,
                m.dual_capture as i64,
                m.thumbnail_in_toast as i64,
            ],
        )?;
        Ok(())
    }

    /// Persist a detection result: last observed state + check timestamp.
    pub fn set_monitor_check_result(&self, id: i64, state: &str, checked_at: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_state = ?2, last_checked_at = ?3 WHERE id = ?1",
            params![id, state, checked_at],
        )?;
        Ok(())
    }

    pub fn clear_channel_errors(&self, channel_id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_state = 'idle' WHERE channel_id = ?1 AND last_state IN ('error', 'failed')",
            params![channel_id],
        )?;
        Ok(())
    }

    pub fn set_monitor_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET enabled=?2 WHERE id=?1",
            params![id, enabled as i64],
        )?;
        Ok(())
    }

    pub fn delete_monitor(&self, id: i64) -> Result<()> {
        let conn = self.db();
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
        let conn = self.db();
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
        let conn = self.db();
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
        let conn = self.db();
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

    /// The monitor and stream/video id a recording belongs to (so a
    /// `RecordingFinished` event can resolve the channel for a rich toast and
    /// build a platform-specific VOD URL when a video id is known).
    pub fn monitor_id_for_recording(
        &self,
        recording_id: i64,
    ) -> Result<Option<(i64, Option<String>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT monitor_id, stream_id FROM recording WHERE id = ?1",
                params![recording_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    // ----- recordings -----

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
    ) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO recording(monitor_id, started_at, output_path, status, went_live_at, went_live_approx, stream_id, take_group)
             VALUES(?1, ?2, ?3, 'recording', ?4, ?5, ?6, ?7)",
            params![monitor_id, started_at, output_path, went_live_at, went_live_approx as i64, stream_id, take_group],
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

    /// Return whether a recording's go-live time is approximate (detection-clock,
    /// not platform-reported). `false` on any error or missing row.
    pub fn recording_went_live_approx(&self, id: i64) -> bool {
        let conn = self.db();
        conn.query_row(
            "SELECT went_live_approx FROM recording WHERE id=?1",
            params![id],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
    }

    /// Record an advertisement break detected during a recording take. `at_secs`
    /// is the offset from the take's start; `duration_secs` the reported ad length.
    pub fn insert_ad_break(
        &self,
        recording_id: i64,
        at_secs: i64,
        duration_secs: i64,
    ) -> Result<i64> {
        let conn = self.db();
        // OR IGNORE + the UNIQUE(recording_id, at_secs) index makes this a no-op
        // when re-attach re-observes a break already persisted before a restart.
        conn.execute(
            "INSERT OR IGNORE INTO ad_break(recording_id, at_secs, duration_secs) VALUES(?1, ?2, ?3)",
            params![recording_id, at_secs, duration_secs],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All ad breaks for a recording take, ordered by offset (for the cut-list
    /// tooltip/popup).
    pub fn ad_breaks_for_recording(&self, recording_id: i64) -> Result<Vec<AdBreak>> {
        let conn = self.db();
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

    // ----- detached download registry (re-attach across app restarts) -----

    /// Register a freshly-spawned tool process so a later launch can re-attach to
    /// it if it outlives the app. Replaces any prior row for the same
    /// (kind, ref_id) so a restarted take doesn't leave a stale entry.
    #[allow(clippy::too_many_arguments)]
    pub fn register_detached(&self, row: &DetachedRow) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM detached_process WHERE kind=?1 AND ref_id=?2",
            params![row.kind.as_str(), row.ref_id],
        )?;
        conn.execute(
            "INSERT INTO detached_process(
                 kind, ref_id, monitor_id, pid, proc_start, job_name, log_path,
                 capture_path, final_path, remux_to_mkv, take_group, spawn_build, started_at,
                 secondary, stream_id, went_live_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                row.kind.as_str(),
                row.ref_id,
                row.monitor_id,
                row.pid as i64,
                row.proc_start as i64,
                row.job_name,
                row.log_path,
                row.capture_path,
                row.final_path,
                row.remux_to_mkv as i64,
                row.take_group,
                row.spawn_build,
                row.started_at,
                row.secondary as i64,
                row.stream_id,
                row.went_live_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Drop a registry row once its download has been finalized or stopped.
    pub fn clear_detached(&self, kind: DetachedKind, ref_id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "DELETE FROM detached_process WHERE kind=?1 AND ref_id=?2",
            params![kind.as_str(), ref_id],
        )?;
        Ok(())
    }

    /// All registry rows — the startup reconcile reads this to decide what to
    /// re-attach, finalize, or orphan.
    pub fn list_detached(&self) -> Result<Vec<DetachedRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT kind, ref_id, monitor_id, pid, proc_start, job_name, log_path,
                    capture_path, final_path, remux_to_mkv, take_group, spawn_build, started_at,
                    secondary, stream_id, went_live_at
             FROM detached_process ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let kind: String = r.get(0)?;
                Ok(DetachedRow {
                    kind: DetachedKind::from_str(&kind).unwrap_or(DetachedKind::Recording),
                    ref_id: r.get(1)?,
                    monitor_id: r.get(2)?,
                    pid: r.get::<_, i64>(3)? as u32,
                    proc_start: r.get::<_, i64>(4)? as u64,
                    job_name: r.get(5)?,
                    log_path: r.get(6)?,
                    capture_path: r.get(7)?,
                    final_path: r.get(8)?,
                    remux_to_mkv: r.get::<_, i64>(9)? != 0,
                    take_group: r.get(10)?,
                    spawn_build: r.get(11)?,
                    started_at: r.get(12)?,
                    secondary: r.get::<_, i64>(13)? != 0,
                    stream_id: r.get(14)?,
                    went_live_at: r.get(15)?,
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
        let conn = self.db();
        conn.execute(
            "INSERT INTO stream_meta_change(recording_id, at_secs, kind, old_value, new_value)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![recording_id, at_secs, kind, old_value, new_value],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All metadata changes for a recording take, in chronological order.
    pub fn meta_changes_for_recording(&self, recording_id: i64) -> Result<Vec<StreamMetaChange>> {
        let conn = self.db();
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
            params![monitor_id, now_unix() - 7 * 86_400],
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
            params![monitor_id, source, now_unix()],
        )?;
        // Start instants an automatic source must NOT re-create for this monitor:
        //  • those claimed by a protected manual row (a user's correction — don't
        //    duplicate it at the same instant), and
        //  • those a user explicitly removed or moved away from (tombstones,
        //    canceled = 1 — re-inserting would resurrect a deleted occurrence, or
        //    re-add the pre-correction copy of a rescheduled one).
        // Storing the manual source itself doesn't self-suppress on manual rows,
        // but still honours tombstones.
        let suppressed_starts: std::collections::HashSet<i64> = {
            let mut stmt = tx.prepare(
                "SELECT start_time FROM schedule_segment
                 WHERE monitor_id = ?1
                   AND ((source = 'manual' AND ?2 <> 'manual') OR canceled = 1)",
            )?;
            stmt.query_map(params![monitor_id, source], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?
        };
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
            for s in segs {
                if !suppressed_starts.contains(&s.start_time) {
                    evict.execute(params![monitor_id, source, s.start_time])?;
                }
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO schedule_segment(monitor_id, start_time, end_time, title, category, canceled, source, video_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for s in segs {
                if suppressed_starts.contains(&s.start_time) {
                    continue;
                }
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
    /// by start time. Used by the OCR hash cache to reconstruct in-memory segment
    /// lists from the DB after an app restart, so OCR is not re-run on unchanged
    /// images.
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
             ORDER BY start_time",
        )?;
        let rows = stmt
            .query_map(params![monitor_id, source], |r| {
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
                    c.color
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
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
                c.enabled
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
                    preferred_platform: Platform::parse_opt(&r.get::<_, String>(45)?),
                    enabled: r.get::<_, i64>(47)? != 0,
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
                    dual_capture: r.get::<_, i64>(44)? != 0,
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
                    vod_id, vod_state, vod_muted_secs
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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

    // ----- videos (on-demand downloads) -----

    /// Insert a new video download request (status starts as `queued`).
    pub fn insert_video(&self, v: &Video) -> Result<i64> {
        let conn = self.db();
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
        let conn = self.db();
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
            color: r.get(5)?,
            preferred_platform: Platform::parse_opt(&r.get::<_, String>(6)?),
            enabled: r.get::<_, i64>(7)? != 0,
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
            dual_capture: false,
            ad_free: false,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            audio_tracks: String::new(),
            subtitle_tracks: String::new(),
            chat_log: false,
            fetch_thumbnail: false,
            thumbnail_in_toast: false,
            fetch_chat_assets: false,
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
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None)
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
    fn ad_break_insert_is_idempotent() {
        // A re-attach after a restart may re-observe a break already persisted; the
        // UNIQUE(recording_id, at_secs) index + INSERT OR IGNORE makes that a no-op.
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let rid = store
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None)
            .unwrap();

        store.insert_ad_break(rid, 120, 15).unwrap();
        store.insert_ad_break(rid, 120, 15).unwrap(); // duplicate — ignored
        store.insert_ad_break(rid, 600, 30).unwrap(); // distinct offset — kept

        let breaks = store.ad_breaks_for_recording(rid).unwrap();
        assert_eq!(breaks.len(), 2);
    }

    #[test]
    fn detached_registry_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let rec = DetachedRow {
            kind: DetachedKind::Recording,
            ref_id: 7,
            monitor_id: Some(3),
            pid: 4321,
            proc_start: 999,
            job_name: "Local\\SA_rec_7".into(),
            log_path: "C:/c/.cache/x.log".into(),
            capture_path: "C:/c/.cache/x.ts".into(),
            final_path: "C:/c/x.mkv".into(),
            remux_to_mkv: true,
            take_group: Some("3:1000".into()),
            spawn_build: "0.1.1/abc-dirty".into(),
            started_at: 1_000,
            secondary: false,
            stream_id: Some("s1".into()),
            went_live_at: Some(990),
        };
        store.register_detached(&rec).unwrap();
        // Re-registering the same (kind, ref_id) replaces rather than duplicates.
        let mut rec2 = rec.clone();
        rec2.pid = 8888;
        store.register_detached(&rec2).unwrap();

        let video = DetachedRow {
            kind: DetachedKind::Video,
            ref_id: 42,
            monitor_id: None,
            pid: 11,
            proc_start: 0,
            job_name: "Local\\SA_video_42".into(),
            log_path: "l".into(),
            capture_path: "c".into(),
            final_path: "f".into(),
            remux_to_mkv: false,
            take_group: None,
            spawn_build: "0.1.1/abc-dirty".into(),
            started_at: 5,
            secondary: false,
            stream_id: None,
            went_live_at: None,
        };
        store.register_detached(&video).unwrap();

        let rows = store.list_detached().unwrap();
        assert_eq!(rows.len(), 2);
        let got = rows
            .iter()
            .find(|r| r.kind == DetachedKind::Recording)
            .unwrap();
        assert_eq!(got.ref_id, 7);
        assert_eq!(got.pid, 8888); // the replacement won
        assert_eq!(got.monitor_id, Some(3));
        assert!(got.remux_to_mkv);
        assert_eq!(got.proc_start, 999);
        assert_eq!(got.take_group.as_deref(), Some("3:1000"));
        assert!(!got.secondary);
        assert_eq!(got.stream_id.as_deref(), Some("s1"));
        assert_eq!(got.went_live_at, Some(990));

        store.clear_detached(DetachedKind::Recording, 7).unwrap();
        let rows = store.list_detached().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, DetachedKind::Video);
    }

    #[test]
    fn meta_change_roundtrip_and_rollups() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let rid = store
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None)
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
}
