//! SQLite-backed persistence (rusqlite, WAL) — the source of truth for
//! channels, monitors, recordings, and key/value settings.
//!
//! rusqlite is synchronous; the connection is wrapped in a `Mutex`. Config CRUD
//! happens on the UI thread (low volume); background tasks will access the same
//! `Arc<Store>` via `spawn_blocking`.

use std::path::Path;
use parking_lot::FairMutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use chrono::TimeZone;

/// Current date in US Pacific Time (UTC-8), which is when Google's API quotas
/// reset. Using PST (UTC-8) exactly matches the reset in winter; in summer PDT
/// (UTC-7) the local Pacific day starts 1h earlier than our boundary, so we
/// carry at most 1 hour of extra headroom — the safe direction vs. resetting
/// 9+ hours early when using the user's local timezone.
fn quota_date_today() -> String {
    let utc_secs = chrono::Utc::now().timestamp();
    let pst = chrono::FixedOffset::west_opt(8 * 3600).unwrap();
    pst.timestamp_opt(utc_secs, 0)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string()
}

use crate::models::{
    AdBreak, AuthKind, Channel, Container, DetachedKind, DetachedRow, DetectionMethod, GlobalStats,
    Monitor, MonitorWithChannel, Platform, RecurrenceKind, SabrCodecPref, ScheduleSegment,
    ScheduledRecording, ScheduledRecordingWithNames, StreamMetaChange, Tool, UpcomingStream, Video,
    now_unix,
};

/// Latest schema version understood by this build.
const SCHEMA_VERSION: i64 = 54;

pub struct Store {
    conn: FairMutex<Connection>,
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

/// A notification to insert into the feed (schema v37 `notification`), built at
/// each emit site (the toast hook, schedule diff, posts fetch, task failure). A
/// non-empty `ref_key` dedups re-emits via the partial-unique index; `""` never
/// dedups. `severity` is `"info" | "warn" | "error"` (drives the row tint).
#[derive(Clone, Debug, Default)]
pub struct NewNotification {
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub body: String,
    pub monitor_id: Option<i64>,
    pub channel: String,
    pub recording_id: Option<i64>,
    pub action_label: String,
    pub action_url: String,
    pub image_path: String,
    pub ref_key: String,
}

/// A persisted notification feed row, as returned by [`Store::list_notifications`].
/// Some fields are persistence/click-through metadata not yet read by the feed
/// UI (mirrors [`ArchivedPost`]'s `#[allow(dead_code)]` convention).
#[derive(Clone, Debug)]
pub struct NotificationRow {
    #[allow(dead_code)]
    pub id: i64,
    pub created_at: i64,
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub body: String,
    #[allow(dead_code)]
    pub monitor_id: Option<i64>,
    pub channel: String,
    #[allow(dead_code)]
    pub recording_id: Option<i64>,
    pub action_label: String,
    pub action_url: String,
    /// Resolved hero/logo image on disk (rendered inline in a later phase).
    #[allow(dead_code)]
    pub image_path: String,
    #[allow(dead_code)]
    pub ref_key: String,
    pub read: bool,
}

/// A full YouTube community post to upsert (schema v38 `community_post`), parsed
/// from the community tab. Keyed on `(monitor_id, post_id)`.
#[derive(Clone, Debug, Default)]
pub struct NewCommunityPost {
    pub monitor_id: i64,
    pub channel_id: i64,
    pub post_id: String,
    pub author: String,
    pub author_icon: String,
    pub published_text: String,
    pub body_text: String,
    pub links_json: String,
    pub poll_json: String,
    pub vote_count: String,
    pub shared_json: String,
    pub raw_json: String,
    /// `channel` (the monitored channel's own post), `viewer` (a fan posting in
    /// the channel's Community space), or `channel` for a reshare. Drives the
    /// UI's viewer-post hiding + the "only channel posts notify" rule.
    pub author_kind: String,
    /// The post author's `UC…` channel id, when extractable — lets a later
    /// round correct a conservative first classification.
    pub author_channel_id: String,
}

/// Parse a YouTube relative-time string ("2 weeks ago", "Streamed 3 days ago",
/// "1 month ago (edited)") into an age in seconds. Months/years use 30/365-day
/// approximations — the source only has bucket precision anyway. `None` when no
/// `<number> <unit>` pair is found.
fn parse_relative_age(text: &str) -> Option<i64> {
    let lower = text.trim().to_lowercase();
    if lower.starts_with("just now") || lower.starts_with("moments ago") {
        return Some(0);
    }
    let mut toks = lower.split_whitespace().peekable();
    while let Some(tok) = toks.next() {
        let Ok(n) = tok.parse::<i64>() else { continue };
        let Some(unit) = toks.peek() else { break };
        let mult: i64 = if unit.starts_with("sec") {
            1
        } else if unit.starts_with("min") {
            60
        } else if unit.starts_with("hour") {
            3600
        } else if unit.starts_with("day") {
            86_400
        } else if unit.starts_with("week") {
            604_800
        } else if unit.starts_with("month") {
            2_592_000
        } else if unit.starts_with("year") {
            31_536_000
        } else {
            continue;
        };
        return Some(n.saturating_mul(mult));
    }
    None
}

/// v46 migration: estimate `published_at` for legacy post rows from the stored
/// relative text, anchored at `last_seen` (the scan that last refreshed the
/// text — it is overwritten on every re-scan, so that is when it was true).
/// Unparseable text falls back to `first_seen`. Only touches rows still at the
/// column DEFAULT 0.
fn fill_published_at(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, published_text, first_seen, last_seen
         FROM community_post WHERE published_at = 0",
    )?;
    let rows: Vec<(i64, String, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (id, text, first_seen, last_seen) in rows {
        let at = parse_relative_age(&text)
            .map(|age| last_seen - age)
            .unwrap_or(first_seen);
        conn.execute(
            "UPDATE community_post SET published_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
    }
    Ok(())
}

/// The author's `UC…` channel id from a post renderer's `authorEndpoint`
/// (current `profileCardCommand` shape, then the legacy `browseEndpoint`).
fn post_author_channel_id(post: &serde_json::Value) -> String {
    let ep = post.get("authorEndpoint");
    ep.and_then(|e| e.get("profileCardCommand"))
        .and_then(|c| c.get("profileOwnerExternalChannelId"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            ep.and_then(|e| e.get("browseEndpoint"))
                .and_then(|b| b.get("browseId"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

/// Flatten a `{runs:[{text,navigationEndpoint…}]}` node into concatenated body
/// text plus a `[{text,url}]` links array (the same 1:1 shape the live parser
/// produces). Used by the v48 reshare repair.
fn runs_node_to_body_links(node: Option<&serde_json::Value>) -> (String, String) {
    let mut body = String::new();
    let mut runs_json: Vec<serde_json::Value> = Vec::new();
    if let Some(runs) = node.and_then(|c| c.get("runs")).and_then(|r| r.as_array()) {
        for run in runs {
            let text = run.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let url = run
                .get("navigationEndpoint")
                .and_then(|ne| {
                    ne.get("urlEndpoint")
                        .and_then(|u| u.get("url"))
                        .and_then(|u| u.as_str())
                        .or_else(|| {
                            ne.get("commandMetadata")
                                .and_then(|c| c.get("webCommandMetadata"))
                                .and_then(|w| w.get("url"))
                                .and_then(|u| u.as_str())
                        })
                })
                .unwrap_or("");
            body.push_str(text);
            runs_json.push(serde_json::json!({ "text": text, "url": url }));
        }
    }
    let links = serde_json::to_string(&runs_json).unwrap_or_else(|_| "[]".to_string());
    (body, links)
}

/// v48 repair: tag every existing community_post row as `channel` or `viewer`,
/// and rebuild reshare rows the old `sharedPostRenderer` path stored empty.
///
/// Owner id per monitor is inferred offline from the rows that carry
/// `showPostAuthorBackgroundHighlight` (the channel's own posts) — no network.
fn reclassify_posts_v48(conn: &Connection) -> Result<()> {
    use std::collections::HashMap;
    struct Row {
        id: i64,
        monitor_id: i64,
        raw: serde_json::Value,
        author_id: String,
        highlighted: bool,
    }
    let mut stmt =
        conn.prepare("SELECT id, monitor_id, raw_json FROM community_post")?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let id: i64 = r.get(0)?;
            let monitor_id: i64 = r.get(1)?;
            let raw_str: String = r.get(2)?;
            Ok((id, monitor_id, raw_str))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(id, monitor_id, raw_str)| {
            let raw: serde_json::Value =
                serde_json::from_str(&raw_str).unwrap_or(serde_json::Value::Null);
            let author_id = post_author_channel_id(&raw);
            let highlighted = raw.get("showPostAuthorBackgroundHighlight").is_some();
            Row { id, monitor_id, raw, author_id, highlighted }
        })
        .collect();
    drop(stmt);

    // Owner id per monitor = the author id of a highlighted (own) post.
    let mut owner: HashMap<i64, String> = HashMap::new();
    for row in &rows {
        if row.highlighted && !row.author_id.is_empty() {
            owner.entry(row.monitor_id).or_insert_with(|| row.author_id.clone());
        }
    }

    for row in &rows {
        // Reshare repair: the sharedPostRenderer subtree the old path mangled.
        let is_reshare = row.raw.get("originalPost").is_some()
            || row.raw.get("displayName").is_some();
        if is_reshare {
            let author = row
                .raw
                .get("displayName")
                .and_then(|d| d.get("runs"))
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|run| run.get("text").and_then(|t| t.as_str()))
                        .collect::<String>()
                })
                .unwrap_or_default();
            let (body, links) = runs_node_to_body_links(row.raw.get("content"));
            let orig = row
                .raw
                .get("originalPost")
                .and_then(|o| o.get("backstagePostRenderer"));
            let shared_json = orig
                .map(|o| {
                    let author_o =
                        first_run_text(o.get("authorText"));
                    let (obody, olinks) =
                        runs_node_to_body_links(o.get("contentText"));
                    serde_json::json!({
                        "author": author_o,
                        "author_channel_id": post_author_channel_id(o),
                        "published_text": first_run_text(o.get("publishedTimeText")),
                        "body_text": obody,
                        "links_json": olinks,
                    })
                    .to_string()
                })
                .unwrap_or_default();
            conn.execute(
                "UPDATE community_post
                    SET author = ?2, body_text = ?3, links_json = ?4,
                        shared_json = ?5, author_kind = 'channel',
                        author_channel_id = ?6
                  WHERE id = ?1",
                params![row.id, author, body, links, shared_json, row.author_id],
            )?;
            continue;
        }

        let owner_id = owner.get(&row.monitor_id).map(String::as_str).unwrap_or("");
        let kind = if !row.highlighted
            && !row.author_id.is_empty()
            && !owner_id.is_empty()
            && row.author_id != owner_id
        {
            "viewer"
        } else {
            "channel"
        };
        conn.execute(
            "UPDATE community_post SET author_kind = ?2, author_channel_id = ?3
              WHERE id = ?1",
            params![row.id, kind, row.author_id],
        )?;
    }
    Ok(())
}

/// First `runs[0].text` of a `{runs:[…]}` node, else empty — the migration twin
/// of the live parser's inline helper.
fn first_run_text(node: Option<&serde_json::Value>) -> String {
    node.and_then(|n| n.get("runs"))
        .and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .and_then(|r| r.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string()
}

/// A new about-page capture to record (schema v45 `about_snapshot`). Keyed on
/// `(channel_id, platform, account)` — one row per distinct content version;
/// identical re-captures only bump `last_checked_at`.
#[derive(Clone, Debug, Default)]
pub struct NewAboutSnapshot {
    pub channel_id: i64,
    pub platform: String, // Platform::as_str()
    pub account: String,  // assets::account_slug of the instance URL
    pub content_hash: String,
    pub description: String,
    pub panels_json: String, // JSON [assets::AboutPanel]
    pub links_json: String,  // JSON [assets::AboutLink]
    pub raw_json: String,    // platform response subtree (forward-compat)
}

/// One persisted about-page version, as returned by
/// [`Store::about_snapshots_for_account`] / [`Store::about_latest_per_account`].
#[derive(Clone, Debug)]
pub struct AboutSnapshotRow {
    pub id: i64,
    #[allow(dead_code)]
    pub channel_id: i64,
    pub platform: String,
    pub account: String,
    pub fetched_at: i64,
    pub last_checked_at: i64,
    /// Version identity — read by the dedup path, kept on the row for
    /// completeness (the viewer identifies versions by `id`/`fetched_at`).
    #[allow(dead_code)]
    pub content_hash: String,
    pub description: String,
    pub panels_json: String,
    pub links_json: String,
}

/// Outcome of [`Store::about_snapshot_record`]: `inserted` = a new version row
/// was created; `prev_hash` = the latest hash BEFORE this call (`None` = first
/// capture ever for the key — the caller keeps the change log silent).
pub struct AboutRecordOutcome {
    #[allow(dead_code)]
    pub id: i64,
    pub inserted: bool,
    pub prev_hash: Option<String>,
}

/// A persisted community post feed row with its ordered attachments, as returned
/// by [`Store::list_community_posts`].
#[derive(Clone, Debug)]
pub struct CommunityPostRow {
    pub id: i64,
    #[allow(dead_code)]
    pub monitor_id: i64,
    #[allow(dead_code)]
    pub channel_id: i64,
    #[allow(dead_code)]
    pub post_id: String,
    pub author: String,
    pub author_icon: String,
    pub published_text: String,
    pub body_text: String,
    pub links_json: String,
    /// Poll options (rendered in a later phase).
    #[allow(dead_code)]
    pub poll_json: String,
    pub vote_count: String,
    /// Reshared/quoted original as JSON `{author, author_channel_id,
    /// published_text, body_text, links_json}` — empty for a non-reshare.
    pub shared_json: String,
    /// First-seen timestamp — ordering tiebreaker for same-bucket posts.
    #[allow(dead_code)]
    pub first_seen: i64,
    /// Approximate publish time (epoch), derived from YouTube's relative
    /// "2 weeks ago" text at first sight — drives the feed order; 0 = unknown.
    pub published_at: i64,
    /// `channel` or `viewer` — the feed hides viewer posts unless toggled on.
    pub author_kind: String,
    pub channel: String,
    pub media: Vec<PostMediaRow>,
}

/// One attachment of a community post (image / poll option / shared thumbnail).
#[derive(Clone, Debug)]
pub struct PostMediaRow {
    #[allow(dead_code)]
    pub ordinal: i64,
    pub kind: String,
    #[allow(dead_code)]
    pub image_url: String,
    pub content_hash: String,
    pub local_path: String,
}

/// One upcoming schedule change detected by [`Store::replace_schedule_source_diffed`]:
/// a future occurrence that was newly added (`added = true`) or whose title/category
/// changed (`added = false`). Drives `schedule_added` / `schedule_updated` feed rows.
pub struct ScheduleChange {
    pub added: bool,
    pub start_time: i64,
    pub title: String,
    pub category: String,
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

/// RAII guard returned by [`Store::db`]. Logs a warning when the lock is held
/// longer than 200 ms, showing the call-site that acquired it — useful for
/// identifying which store method is the bottleneck.
struct DbGuard<'a> {
    inner: parking_lot::FairMutexGuard<'a, Connection>,
    acquired_at: std::time::Instant,
    caller: &'static std::panic::Location<'static>,
}

impl std::ops::Deref for DbGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection { &*self.inner }
}

impl std::ops::DerefMut for DbGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection { &mut *self.inner }
}

impl Drop for DbGuard<'_> {
    fn drop(&mut self) {
        // Count every DB access (ops + cumulative hold time) at the single
        // chokepoint all queries pass through; byte-level growth is sampled
        // from the db/WAL file sizes by the I/O monitor instead.
        crate::iomon::record_region(
            crate::iomon::Cat::Db,
            crate::iomon::Region::AppData,
            crate::iomon::OpKind::Meta,
            0,
            self.acquired_at.elapsed(),
        );
        let ms = self.acquired_at.elapsed().as_millis();
        if ms >= 200 {
            tracing::warn!(
                hold_ms = ms,
                file = self.caller.file(),
                line = self.caller.line(),
                "store: long DB lock hold"
            );
        } else if ms >= 50 {
            tracing::debug!(
                hold_ms = ms,
                file = self.caller.file(),
                line = self.caller.line(),
                "store: DB lock hold"
            );
        }
    }
}

/// `(channel_id, monitor_id, live output_path, muted_secs)` — the archive-replace
/// decision inputs for a recording.
type VodReplaceInfo = (i64, i64, String, Option<i64>);
/// `(monitor_url, vod_id, stream_id, went_live_at)` — inputs to resolve a
/// recording's published-VOD URL for a manual "download VOD now".
type VodArchiveNowInfo = (String, Option<String>, Option<String>, Option<i64>);

impl Store {
    /// Acquire the DB connection. Logs a warning when contention caused a wait
    /// longer than 50 ms (waiter side) or when the lock is held longer than
    /// 200 ms (holder side). `#[track_caller]` embeds the caller's source
    /// location in both log lines so slow call-sites are immediately visible.
    #[track_caller]
    fn db(&self) -> DbGuard<'_> {
        let caller = std::panic::Location::caller();
        let t = std::time::Instant::now();
        let g = self.conn.lock();
        let wait_ms = t.elapsed().as_millis();
        if wait_ms >= 50 {
            tracing::warn!(
                wait_ms,
                file = caller.file(),
                line = caller.line(),
                "store: slow DB lock – another thread held the connection"
            );
        } else if wait_ms >= 5 {
            tracing::debug!(
                wait_ms,
                file = caller.file(),
                line = caller.line(),
                "store: DB lock wait"
            );
        }
        DbGuard { inner: g, acquired_at: std::time::Instant::now(), caller }
    }

    /// Open (or create) the database at `path`, set pragmas, and migrate.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;
        Self::configure(&conn)?;
        let store = Store {
            conn: FairMutex::new(conn),
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
            conn: FairMutex::new(conn),
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
        if version < SCHEMA_VERSION {
            tracing::info!(from = version, to = SCHEMA_VERSION, "migrating database schema");
        }
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
        if version < 31 {
            // Covering index for schedule_segments_for_source: the existing
            // idx_schedule_monitor covers (monitor_id, start_time) but not
            // `source`, so queries filtering by source scanned every row for
            // that monitor. On an accumulated historical archive (past segments
            // are kept as history) this caused multi-second lock holds per call.
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_schedule_source
                 ON schedule_segment(monitor_id, source, start_time);",
            )?;
            conn.pragma_update(None, "user_version", 31)?;
        }
        if version < 32 {
            // Index for all_upcoming_schedule: the query filters
            // `canceled = 0 AND start_time >= ?` across all monitors, but the
            // existing idx_schedule_monitor leads with monitor_id, so SQLite
            // had to full-scan the entire table. With months of historical rows
            // accumulated this caused 4+ second lock holds on Schedule tab clicks.
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_schedule_canceled_start
                 ON schedule_segment(canceled, start_time);",
            )?;
            conn.pragma_update(None, "user_version", 32)?;
        }
        if version < 33 {
            // Per-provider, per-day API quota tracking. Currently used for the
            // YouTube Data API (10,000 free units/day). `provider` is a short key
            // like "youtube"; `date` is an ISO date string "YYYY-MM-DD".
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS api_quota (
                    provider TEXT NOT NULL,
                    date     TEXT NOT NULL,
                    units    INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (provider, date)
                );",
            )?;
            conn.pragma_update(None, "user_version", 33)?;
        }
        if version < 34 {
            // Schedule segment merge support: `merged_into` links a secondary
            // segment to its primary (manual merge) so the secondary is hidden
            // from the calendar in favour of the primary. `auto_merge_excluded`
            // opts a segment out of automatic time-overlap merge grouping with
            // same-channel events.
            conn.execute_batch(
                "ALTER TABLE schedule_segment ADD COLUMN merged_into       INTEGER;
                 ALTER TABLE schedule_segment ADD COLUMN auto_merge_excluded INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 34)?;
        }
        if version < 35 {
            // User-defined filename-template presets (name + template string).
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS filename_preset (
                     id       INTEGER PRIMARY KEY,
                     name     TEXT NOT NULL,
                     template TEXT NOT NULL
                 );",
            )?;
            conn.pragma_update(None, "user_version", 35)?;
        }
        if version < 36 {
            // Deduplicate schedule_segment rows that accumulated due to a bug where
            // OCR-cadence cache hits re-inserted past rows on every 60-second tick
            // (replace_schedule_source deletes only future rows, so past rows doubled
            // each tick). Keep the earliest id per (monitor, source, start_time,
            // canceled) tuple; window function avoids an expensive NOT-IN subquery.
            conn.execute_batch(
                "DELETE FROM schedule_segment WHERE id IN (
                     SELECT id FROM (
                         SELECT id,
                                ROW_NUMBER() OVER (
                                    PARTITION BY monitor_id, source, start_time, canceled
                                    ORDER BY id
                                ) AS rn
                         FROM schedule_segment
                     ) WHERE rn > 1
                 );",
            )?;
            conn.pragma_update(None, "user_version", 36)?;
        }
        if version < 37 {
            // In-app notifications feed: a persisted, filterable history of
            // toast-worthy events (recording lifecycle, errors), went-live,
            // schedule changes, background-task failures, and new YouTube posts.
            // One row fully reconstructs the item at render (no re-resolution).
            // `ref_key` (partial-unique) makes "insert if new" a single
            // ON CONFLICT DO NOTHING; rows that don't dedup use ref_key=''.
            // FK is SET NULL (not CASCADE): deleting a monitor keeps its history
            // meaningful via the denormalized `channel` string.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS notification (
                    id           INTEGER PRIMARY KEY,
                    created_at   INTEGER NOT NULL,
                    kind         TEXT NOT NULL,
                    severity     TEXT NOT NULL DEFAULT 'info',
                    title        TEXT NOT NULL DEFAULT '',
                    body         TEXT NOT NULL DEFAULT '',
                    monitor_id   INTEGER,
                    channel      TEXT NOT NULL DEFAULT '',
                    recording_id INTEGER,
                    action_label TEXT NOT NULL DEFAULT '',
                    action_url   TEXT NOT NULL DEFAULT '',
                    image_path   TEXT NOT NULL DEFAULT '',
                    ref_key      TEXT NOT NULL DEFAULT '',
                    read         INTEGER NOT NULL DEFAULT 0,
                    FOREIGN KEY(monitor_id) REFERENCES monitor(id) ON DELETE SET NULL
                );
                CREATE INDEX IF NOT EXISTS idx_notification_created
                    ON notification(created_at DESC);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_notification_refkey
                    ON notification(ref_key) WHERE ref_key <> '';",
            )?;
            conn.pragma_update(None, "user_version", 37)?;
        }
        if version < 38 {
            // Full YouTube community posts (the posts feed) — distinct from the
            // image-only `community_post_archive` (schedule-OCR). One row per
            // post, keyed by the stable backstage `post_id`. `raw_json` keeps the
            // renderer subtree for forward-compat re-parsing. `first_seen` drives
            // the feed order + the "new post" notification.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS community_post (
                    id             INTEGER PRIMARY KEY,
                    monitor_id     INTEGER NOT NULL,
                    channel_id     INTEGER NOT NULL,
                    post_id        TEXT NOT NULL,
                    author         TEXT NOT NULL DEFAULT '',
                    author_icon    TEXT NOT NULL DEFAULT '',
                    published_text TEXT NOT NULL DEFAULT '',
                    body_text      TEXT NOT NULL DEFAULT '',
                    links_json     TEXT NOT NULL DEFAULT '[]',
                    poll_json      TEXT NOT NULL DEFAULT '',
                    vote_count     TEXT NOT NULL DEFAULT '',
                    shared_json    TEXT NOT NULL DEFAULT '',
                    raw_json       TEXT NOT NULL DEFAULT '',
                    first_seen     INTEGER NOT NULL,
                    last_seen      INTEGER NOT NULL,
                    FOREIGN KEY(monitor_id) REFERENCES monitor(id) ON DELETE CASCADE
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_community_post_uniq
                    ON community_post(monitor_id, post_id);
                CREATE INDEX IF NOT EXISTS idx_community_post_seen
                    ON community_post(monitor_id, first_seen DESC);",
            )?;
            conn.pragma_update(None, "user_version", 38)?;
        }
        if version < 39 {
            // Attachments of a community post (posts are 1-to-many): images, poll
            // options, shared-video thumbnails. `ordinal` preserves display order;
            // `content_hash`/`local_path` are the content-addressed cached image.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS community_post_media (
                    id           INTEGER PRIMARY KEY,
                    post_pk      INTEGER NOT NULL,
                    ordinal      INTEGER NOT NULL DEFAULT 0,
                    kind         TEXT NOT NULL DEFAULT 'image',
                    image_url    TEXT NOT NULL DEFAULT '',
                    content_hash TEXT NOT NULL DEFAULT '',
                    local_path   TEXT NOT NULL DEFAULT '',
                    FOREIGN KEY(post_pk) REFERENCES community_post(id) ON DELETE CASCADE
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_community_post_media_uniq
                    ON community_post_media(post_pk, ordinal);",
            )?;
            conn.pragma_update(None, "user_version", 39)?;
        }
        if version < 40 {
            // Per-monitor YouTube SABR codec/quality preference (a yt-dlp `-S`
            // sort). `inherit` = use the global Settings default; `sabr_codec_custom`
            // holds the raw `-S` string when the pref is `custom`. Mirrors the
            // auth_kind/auth_value inherit pattern.
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN sabr_codec_pref   TEXT NOT NULL DEFAULT 'inherit';
                 ALTER TABLE monitor ADD COLUMN sabr_codec_custom TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 40)?;
        }
        if version < 41 {
            // Twitch VOD recovery: attach a recovered MKV + a distinct status onto
            // a recording take. NULL = never attempted (all legacy/non-Twitch rows).
            // `recovery_state` is a namespace disjoint from `status` and `vod_state`:
            // recovering | recovered | partial | failed | unavailable.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN recovery_state TEXT;
                 ALTER TABLE recording ADD COLUMN recovered_path TEXT;",
            )?;
            conn.pragma_update(None, "user_version", 41)?;
        }
        if version < 42 {
            // Post-stream published-VOD download ("archive the VOD after end").
            // Tracks the download job on the recording take, parallel to the
            // recovery columns. NULL = not attempted.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN vod_dl_state    TEXT;
                 ALTER TABLE recording ADD COLUMN vod_dl_path     TEXT;
                 ALTER TABLE recording ADD COLUMN vod_dl_video_id INTEGER;",
            )?;
            conn.pragma_update(None, "user_version", 42)?;
        }
        if version < 43 {
            // Live DVR head backfill: a late-joined capture's missed beginning,
            // downloaded from the growing published-VOD playlist while the
            // stream is still live (`{stem}.head.mkv`), and the post-stream
            // lossless concat of head + live capture (`{stem}.full.mkv`).
            // NULL = no backfill was needed/attempted.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN backfill_path TEXT;
                 ALTER TABLE recording ADD COLUMN full_path     TEXT;",
            )?;
            conn.pragma_update(None, "user_version", 43)?;
        }
        if version < 44 {
            // Trigger words: the human description of the rule match that
            // started this recording (e.g. `title ~ "karaoke"`), empty when it
            // started normally. Named trigger_info because TRIGGER is an SQL
            // keyword. Drives the ⚡ badge + notification.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN trigger_info TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 44)?;
        }
        if version < 45 {
            // About-page archive: one row per distinct content VERSION of an
            // account's about page (description, panels, links). Keyed by
            // (channel_id, platform, account) — the same identity as the asset
            // dirs, but by channel *id* so renames don't orphan history.
            // `last_checked_at` bumps when a fetch found identical content.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS about_snapshot (
                    id              INTEGER PRIMARY KEY,
                    channel_id      INTEGER NOT NULL,
                    platform        TEXT NOT NULL,
                    account         TEXT NOT NULL,
                    fetched_at      INTEGER NOT NULL,
                    last_checked_at INTEGER NOT NULL,
                    content_hash    TEXT NOT NULL,
                    description     TEXT NOT NULL DEFAULT '',
                    panels_json     TEXT NOT NULL DEFAULT '[]',
                    links_json      TEXT NOT NULL DEFAULT '[]',
                    raw_json        TEXT NOT NULL DEFAULT '',
                    FOREIGN KEY(channel_id) REFERENCES channel(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_about_snapshot_key
                    ON about_snapshot(channel_id, platform, account, fetched_at DESC);",
            )?;
            conn.pragma_update(None, "user_version", 45)?;
        }
        if version < 46 {
            // Community posts: an (approximate) publish time derived from
            // YouTube's relative "2 weeks ago" strings. Feed ordering previously
            // used `first_seen` (discovery time), which scrambles a channel's
            // backlog — every post found in one scan ties on the same second.
            // Existing rows are estimated from the stored relative text anchored
            // at `last_seen` (the scan that last refreshed the text); rows with
            // unparseable text fall back to `first_seen`. Plus per-monitor
            // bookkeeping for the full-history posts backfill walk.
            conn.execute_batch(
                "ALTER TABLE community_post
                     ADD COLUMN published_at INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX IF NOT EXISTS idx_community_post_pub
                     ON community_post(monitor_id, published_at DESC);
                 CREATE TABLE IF NOT EXISTS community_post_backfill (
                     monitor_id      INTEGER PRIMARY KEY,
                     completed_at    INTEGER NOT NULL DEFAULT 0,
                     last_attempt_at INTEGER NOT NULL DEFAULT 0,
                     pages           INTEGER NOT NULL DEFAULT 0,
                     posts_seen      INTEGER NOT NULL DEFAULT 0,
                     FOREIGN KEY(monitor_id) REFERENCES monitor(id) ON DELETE CASCADE
                 );",
            )?;
            fill_published_at(&conn)?;
            conn.pragma_update(None, "user_version", 46)?;
        }
        if version < 47 {
            // Hygiene for the short-lived v46 build: a ZERO-post first page
            // used to record a "trivially complete" posts backfill, conflating
            // channels without a community tab (or an interstitial that parsed
            // empty) with feeds that genuinely fit on one page. A completion
            // recorded without a single archived post is bogus — drop it so
            // the monitor gets a real walk. One-page feeds keep theirs (they
            // have posts).
            conn.execute_batch(
                "DELETE FROM community_post_backfill
                  WHERE pages = 0 AND posts_seen = 0
                    AND monitor_id NOT IN
                        (SELECT DISTINCT monitor_id FROM community_post);",
            )?;
            conn.pragma_update(None, "user_version", 47)?;
        }
        if version < 48 {
            // Community posts carry three item kinds at the same structural
            // position: the channel's own posts, VIEWER posts (fans posting in
            // the channel's space), and reshares. They were all archived as the
            // channel's own — viewer posts even fired misattributed "«channel»
            // posted" notifications. Tag each row so the UI can hide viewer
            // posts and the fetcher can skip notifying them; repair reshare
            // rows the old sharedPostRenderer path stored empty.
            conn.execute_batch(
                "ALTER TABLE community_post
                     ADD COLUMN author_kind       TEXT NOT NULL DEFAULT 'channel';
                 ALTER TABLE community_post
                     ADD COLUMN author_channel_id TEXT NOT NULL DEFAULT '';",
            )?;
            reclassify_posts_v48(&conn)?;
            conn.pragma_update(None, "user_version", 48)?;
        }
        if version < 49 {
            // Two separate switches. `enabled` (both tables) has always been the
            // Auto-RECORD flag (a disk-space control) — it is left untouched.
            // `automation_enabled` is a NEW master switch: off = fully dormant
            // (no detection/recording/asset/about/posts/schedule fetch; only
            // manual actions work). Plus per-monitor live-state columns so the
            // last-detected title/game/thumbnail/viewers are stored on every
            // poll (regardless of Auto) and shown in the grid without a
            // recording. `last_viewers = -1` means unknown/not-applicable.
            conn.execute_batch(
                "ALTER TABLE monitor  ADD COLUMN automation_enabled INTEGER NOT NULL DEFAULT 1;
                 ALTER TABLE channel  ADD COLUMN automation_enabled INTEGER NOT NULL DEFAULT 1;
                 ALTER TABLE monitor  ADD COLUMN last_title         TEXT    NOT NULL DEFAULT '';
                 ALTER TABLE monitor  ADD COLUMN last_game          TEXT    NOT NULL DEFAULT '';
                 ALTER TABLE monitor  ADD COLUMN last_thumbnail_url TEXT    NOT NULL DEFAULT '';
                 ALTER TABLE monitor  ADD COLUMN last_viewers       INTEGER NOT NULL DEFAULT -1;",
            )?;
            conn.pragma_update(None, "user_version", 49)?;
        }
        if version < 50 {
            // Which yt-dlp-family binary a Video download uses: empty = system
            // yt-dlp, "sabr" = the built-in SABR dev build, else a custom
            // tool's alias (see downloader::CustomTool).
            conn.execute_batch(
                "ALTER TABLE video ADD COLUMN tool_binary TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 50)?;
        }
        if version < 51 {
            // Scheduled recordings: force-start a recording at a specific time
            // (once) or on a weekly repeat, bypassing Auto the same way a
            // trigger-word match does. `next_run_at`/`last_fired_at` drive the
            // due-scan; `pending_stop_at` tracks an in-flight duration-bound
            // occurrence awaiting its auto-stop. See `scheduled_recordings.rs`.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS scheduled_recording (
                    id               INTEGER PRIMARY KEY,
                    monitor_id       INTEGER NOT NULL,
                    label            TEXT NOT NULL DEFAULT '',
                    kind             TEXT NOT NULL,
                    start_at         INTEGER,
                    days_of_week     INTEGER,
                    time_of_day_secs INTEGER,
                    until            INTEGER,
                    duration_secs    INTEGER,
                    enabled          INTEGER NOT NULL DEFAULT 1,
                    next_run_at      INTEGER,
                    last_fired_at    INTEGER,
                    pending_stop_at  INTEGER,
                    created_at       INTEGER NOT NULL,
                    FOREIGN KEY(monitor_id) REFERENCES monitor(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_scheduled_recording_due
                    ON scheduled_recording(enabled, next_run_at);
                CREATE INDEX IF NOT EXISTS idx_scheduled_recording_monitor
                    ON scheduled_recording(monitor_id);",
            )?;
            conn.pragma_update(None, "user_version", 51)?;
        }
        if version < 52 {
            // Poll-detected "currently live" go-live time, tracked independent of
            // any recording (like last_title/last_game — written on every poll
            // regardless of Auto) so the Went Live/Started On/Duration columns
            // have something to show for a live-but-not-recording (Auto off)
            // instance instead of sitting blank. Cleared to NULL on offline.
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN last_live_since        INTEGER;
                 ALTER TABLE monitor ADD COLUMN last_live_since_approx INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 52)?;
        }
        if version < 53 {
            // "queued" while a head-backfill decision is pending for a take —
            // set the instant the job is spawned, cleared once it either
            // starts fetching or determines nothing is needed. Drives the
            // Streams-grid "⏳ backfill queued" badge and the Background
            // view's "Planned" section, covering `head_backfill_job`'s ~2
            // minute settle wait, which otherwise has no visible signal at
            // all. See `downloader::HEAD_BACKFILL_SETTLE_SECS`.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN head_backfill_state TEXT NOT NULL DEFAULT '';
                 CREATE INDEX IF NOT EXISTS idx_recording_head_backfill_queued
                     ON recording(head_backfill_state) WHERE head_backfill_state = 'queued';",
            )?;
            conn.pragma_update(None, "user_version", 53)?;
        }
        if version < 54 {
            // The exact TriggerRule (serde JSON) that started this recording,
            // frozen at start time — empty = not trigger-started. Needed
            // (rather than re-resolving the live global/channel/instance rule
            // lists) because TriggerRules have no stable id and can be
            // edited/reordered mid-broadcast; a re-attach after an app
            // restart also has no other way to recover which rule (and its
            // stop_on_unmatch/lead_secs/end_delay_secs config) an
            // already-running take was started by. See `trigger_info`
            // (v44) for the human-readable sibling of this column.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN trigger_rule_json TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 54)?;
        }
        debug_assert_eq!(SCHEMA_VERSION, 54);
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

    // ----- API quota tracking (schema v33) -----

    /// Increment the quota-units counter for `provider` on today's date.
    /// Silently ignores errors (quota tracking is best-effort).
    pub fn record_quota_usage(&self, provider: &str, units: i64) -> Result<()> {
        let today = quota_date_today();
        let conn = self.db();
        conn.execute(
            "INSERT INTO api_quota(provider, date, units) VALUES(?1, ?2, ?3)
             ON CONFLICT(provider, date) DO UPDATE SET units = units + excluded.units",
            params![provider, today, units],
        )?;
        Ok(())
    }

    /// Return the total quota units consumed by `provider` today, or 0 if none.
    pub fn get_quota_today(&self, provider: &str) -> Result<i64> {
        let today = quota_date_today();
        let conn = self.db();
        let units = conn
            .query_row(
                "SELECT units FROM api_quota WHERE provider = ?1 AND date = ?2",
                params![provider, today],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(units)
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
                "SELECT id, name, url, platform, created_at, color, preferred_platform, enabled, \
                 automation_enabled FROM channel WHERE url = ?1",
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
            "SELECT id, name, url, platform, created_at, color, preferred_platform, enabled, \
             automation_enabled FROM channel
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt
            .query_map([], Self::map_channel)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Create a new empty channel container (no URL/platform of its own; its
    /// instances carry the source URLs). Always inserts a new row. `enabled`/
    /// `automation_enabled` default to the schema default (both `true`).
    pub fn create_container(&self, name: &str) -> Result<i64> {
        self.insert_channel(name, "", Platform::Generic)
    }

    /// Create a new channel container, seeding its Auto (`enabled`) and
    /// Enabled (`automation_enabled`) switches instead of leaving them at the
    /// schema default. Meant for "add stream" flows that create a channel
    /// alongside its first instance: without this, a brand-new channel always
    /// starts Auto=on/Enabled=on regardless of what the instance was
    /// configured with, leaving a channel/instance mismatch the grid ANDs
    /// together (confusing even though not functionally broken) the moment
    /// it's created.
    pub fn create_container_with_flags(
        &self,
        name: &str,
        enabled: bool,
        automation_enabled: bool,
    ) -> Result<i64> {
        let id = self.create_container(name)?;
        self.set_channel_enabled(id, enabled)?;
        self.set_channel_automation_enabled(id, automation_enabled)?;
        Ok(id)
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

    /// Set (or clear) the preferred asset source for a channel container — the
    /// platform (and optionally account) whose profile pic / banner represents
    /// it, stored as `platform[:account]` text. `None` reverts to auto (the
    /// first instance that has a fetched icon).
    pub fn set_channel_preferred_asset(
        &self,
        id: i64,
        source: Option<&crate::models::PreferredAssetSource>,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET preferred_platform = ?2 WHERE id = ?1",
            params![id, source.map(|s| s.to_db()).unwrap_or_default()],
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

    /// Master automation switch for a whole channel (all its instances). Off =
    /// fully dormant. Independent from `enabled` (the Auto-record flag).
    pub fn set_channel_automation_enabled(&self, channel_id: i64, on: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET automation_enabled = ?2 WHERE id = ?1",
            params![channel_id, on as i64],
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
                chat_log, fetch_thumbnail, fetch_chat_assets, dual_capture, thumbnail_in_toast,
                sabr_codec_pref, sabr_codec_custom, automation_enabled)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)",
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
                m.sabr_codec_pref.id(),
                m.sabr_codec_custom,
                m.automation_enabled as i64,
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
                thumbnail_in_toast=?23, sabr_codec_pref=?24, sabr_codec_custom=?25,
                automation_enabled=?26 WHERE id=?1",
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
                m.sabr_codec_pref.id(),
                m.sabr_codec_custom,
                m.automation_enabled as i64,
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

    /// Persist the last-detected live info on a monitor (title/game/thumbnail/
    /// viewers/go-live time), written on every poll regardless of the
    /// Auto-record flag so the grid can show a live channel's info — including
    /// Went Live/Started On/Duration — without a recording. Empty strings +
    /// `viewers = -1` clear stale info when a channel goes offline; `live_since
    /// = None` likewise clears the go-live time (a fresh value is stamped again
    /// the next time it's seen live).
    #[allow(clippy::too_many_arguments)]
    pub fn set_monitor_live_meta(
        &self,
        id: i64,
        title: &str,
        game: &str,
        thumbnail_url: &str,
        viewers: i64,
        live_since: Option<i64>,
        live_since_approx: bool,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_title = ?2, last_game = ?3,
                 last_thumbnail_url = ?4, last_viewers = ?5,
                 last_live_since = ?6, last_live_since_approx = ?7 WHERE id = ?1",
            params![id, title, game, thumbnail_url, viewers, live_since, live_since_approx as i64],
        )?;
        Ok(())
    }

    /// Update just the live viewer count, independent of `set_monitor_live_meta`
    /// — used by the in-recording `meta_watcher` (`downloader.rs`), which polls
    /// title/game/viewers directly while the scheduler skips an actively-
    /// recording monitor entirely. A narrow single-column setter so a viewer
    /// refresh can't clobber the thumbnail/go-live fields that only the
    /// scheduler's full poll outcome should own.
    pub fn set_monitor_viewers(&self, id: i64, viewers: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_viewers = ?2 WHERE id = ?1",
            params![id, viewers],
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

    /// Master automation switch for a single instance. Off = fully dormant.
    /// Independent from `enabled` (the Auto-record flag).
    pub fn set_monitor_automation_enabled(&self, id: i64, on: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET automation_enabled=?2 WHERE id=?1",
            params![id, on as i64],
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

    /// Update the output path of a finished recording — used after a manual
    /// re-remux succeeds to replace the `.ts` capture path with the final `.mkv`.
    pub fn update_recording_output_path(&self, id: i64, path: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET output_path = ?2 WHERE id = ?1",
            params![id, path],
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
    pub fn clear_recording_backfill_path(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET backfill_path=NULL WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

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

    // ----- post-stream published-VOD download ("archive the VOD after end") -----

    /// Set the archive-download state + link the download job (`video.id`).
    pub fn set_recording_vod_dl(&self, id: i64, state: &str, video_id: Option<i64>) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_dl_state=?2, vod_dl_video_id=?3 WHERE id=?1",
            params![id, state, video_id],
        )?;
        Ok(())
    }

    /// Record a downloaded published VOD file with a terminal archive state
    /// (`archived`/`replaced`).
    pub fn set_recording_vod_archived(&self, id: i64, path: &str, state: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_dl_path=?2, vod_dl_state=?3 WHERE id=?1",
            params![id, path, state],
        )?;
        Ok(())
    }

    /// Which recording (if any) an archive-download job belongs to.
    pub fn recording_for_vod_video(&self, video_id: i64) -> Result<Option<i64>> {
        let conn = self.db();
        let id = conn
            .query_row(
                "SELECT id FROM recording WHERE vod_dl_video_id = ?1",
                params![video_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Resolve a muted-VOD issue (Issues panel "Dismiss"/"Keep live recording").
    pub fn recording_vod_dl_acknowledge(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_dl_state='acknowledged' WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    /// `(monitor_url, vod_id, stream_id, went_live_at)` for a manual "download VOD
    /// now" — enough to resolve the published-VOD URL per platform.
    pub fn recording_archive_now(&self, rec_id: i64) -> Result<Option<VodArchiveNowInfo>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT m.url, r.vod_id, r.stream_id, r.went_live_at
                 FROM recording r JOIN monitor m ON m.id = r.monitor_id
                 WHERE r.id = ?1",
                params![rec_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// `(channel_id, monitor_id, live output_path, muted_secs)` for a recording —
    /// what the archive-completion hook needs to decide/perform a replace.
    pub fn recording_replace_info(&self, rec_id: i64) -> Result<Option<VodReplaceInfo>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT m.channel_id, r.monitor_id, COALESCE(r.output_path, ''), r.vod_muted_secs
                 FROM recording r JOIN monitor m ON m.id = r.monitor_id
                 WHERE r.id = ?1",
                params![rec_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Completed archive downloads whose recording-side finalize never ran —
    /// `vod_dl_state` still `'downloading'` (or reset to `'failed'` by the
    /// startup sweep) while the linked video row says `'completed'`. Rows whose
    /// video is still in the detached registry are excluded: those are being
    /// adopted by `reconcile_detached` and will finalize through that path.
    /// Returns `(rec_id, video_id, video output_path)`.
    pub fn vod_archive_replay_candidates(&self) -> Result<Vec<(i64, i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id, v.id, COALESCE(v.output_path, '')
             FROM recording r JOIN video v ON v.id = r.vod_dl_video_id
             WHERE r.vod_dl_state IN ('downloading', 'failed') AND v.status = 'completed'
               AND v.id NOT IN (SELECT ref_id FROM detached_process WHERE kind = 'video')",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every recording currently claiming a terminal `'archived'` state —
    /// audited at startup for bogus archives (e.g. a promoted log file).
    /// Returns `(rec_id, vod_dl_video_id, vod_dl_path, vod_muted_secs, vod_id)`.
    #[allow(clippy::type_complexity)]
    pub fn vod_archived_rows(
        &self,
    ) -> Result<Vec<(i64, Option<i64>, String, i64, Option<String>)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, vod_dl_video_id, COALESCE(vod_dl_path, ''),
                    COALESCE(vod_muted_secs, 0), vod_id
             FROM recording WHERE vod_dl_state = 'archived'",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(vod_dl_state, vod_dl_video_id)` for one recording — what the post-find
    /// mute watcher needs to decide whether the archive predates the mute.
    pub fn recording_vod_dl(&self, id: i64) -> Result<Option<(Option<String>, Option<i64>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT vod_dl_state, vod_dl_video_id FROM recording WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Update only the muted-seconds count (post-find mute watcher) — must not
    /// clobber `vod_state`/`vod_id` the way `set_recording_vod_found` would.
    pub fn set_recording_vod_muted_secs(&self, id: i64, muted_secs: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE recording SET vod_muted_secs=?2 WHERE id=?1",
            params![id, muted_secs],
        )?;
        Ok(())
    }

    /// `(live output_path, went_live_at, ended_at)` — what the archive sanity
    /// check needs to derive the expected VOD duration (probe the live file,
    /// else the recorded wall-clock span).
    #[allow(clippy::type_complexity)]
    pub fn recording_duration_hint(
        &self,
        rec_id: i64,
    ) -> Result<Option<(String, Option<i64>, Option<i64>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT COALESCE(output_path, ''), went_live_at, ended_at
                 FROM recording WHERE id = ?1",
                params![rec_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Recordings whose published VOD was DMCA-muted and not yet acknowledged —
    /// the muted-VOD category of the Issues panel.
    pub fn recordings_muted_vod_unresolved(&self) -> Result<Vec<crate::models::MutedVodIssue>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id, c.name, COALESCE(r.output_path, ''), r.recovered_path, r.recovery_state,
                    COALESCE(r.vod_muted_secs, 0)
             FROM recording r
             JOIN monitor m ON m.id = r.monitor_id
             JOIN channel c ON c.id = m.channel_id
             WHERE r.vod_dl_state = 'muted'
             ORDER BY COALESCE(r.went_live_at, r.started_at) DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::MutedVodIssue {
                    rec_id: r.get(0)?,
                    channel: r.get(1)?,
                    output_path: r.get(2)?,
                    recovered_path: r.get(3)?,
                    recovery_state: r.get(4)?,
                    muted_secs: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Fields needed to build a recovery for one recording (login derived from the
    /// monitor URL). `None` if the row is missing. Callers gate on a non-empty
    /// `stream_id` (id-less detection can't derive the CDN URL).
    pub fn recording_recovery_seed(
        &self,
        id: i64,
    ) -> Result<Option<crate::models::RecoverableTake>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT r.id, m.url, COALESCE(r.stream_id, ''),
                        COALESCE(r.went_live_at, r.started_at),
                        COALESCE(r.went_live_approx, 0),
                        COALESCE(r.vod_state = 'not_published', 0), r.vod_id
                 FROM recording r JOIN monitor m ON m.id = r.monitor_id
                 WHERE r.id = ?1",
                params![id],
                |r| {
                    Ok(crate::models::RecoverableTake {
                        rec_id: r.get(0)?,
                        monitor_url: r.get(1)?,
                        stream_id: r.get(2)?,
                        start_epoch: r.get(3)?,
                        went_live_approx: r.get::<_, i64>(4)? != 0,
                        deleted: r.get::<_, i64>(5)? != 0,
                        vod_id: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Current CDN-recovery state of one recording (`None` = never attempted).
    pub fn recording_recovery_state(&self, id: i64) -> Result<Option<String>> {
        let conn = self.db();
        let v = conn
            .query_row(
                "SELECT recovery_state FROM recording WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    /// Reset takes stuck in `recovery_state = 'recovering'` to `'failed'`.
    /// Recoveries are in-process tasks — none survive a restart — so any row
    /// still marked 'recovering' at startup crashed mid-run and would
    /// otherwise show the "recovering…" badge forever and be excluded from
    /// bulk scans (which only retry NULL/'failed').
    pub fn reset_stale_recovering(&self) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE recording SET recovery_state = 'failed' WHERE recovery_state = 'recovering'",
            [],
        )?;
        Ok(n)
    }

    /// Reset takes stuck in `vod_dl_state = 'downloading'` to `'failed'`.
    /// The pre-download wait (YouTube ~5 min, Kick up to ~60 min of polls) is a
    /// plain in-process task — a quit/crash in that window leaves the state
    /// stranded with no video row to adopt. A detached download that DID
    /// survive re-archives on its adopted completion, overwriting the 'failed'.
    pub fn reset_stale_vod_downloading(&self) -> Result<usize> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE recording SET vod_dl_state = 'failed' WHERE vod_dl_state = 'downloading'",
            [],
        )?;
        Ok(n)
    }

    /// Deleted/muted Twitch takes that still have a stream id, fall inside the CDN
    /// retention window, and haven't already been recovered — the bulk-scan set.
    pub fn recordings_recoverable(
        &self,
        within_secs: i64,
        now: i64,
    ) -> Result<Vec<crate::models::RecoverableTake>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT r.id, m.url, r.stream_id,
                    COALESCE(r.went_live_at, r.started_at),
                    COALESCE(r.went_live_approx, 0),
                    COALESCE(r.vod_state = 'not_published', 0), r.vod_id
             FROM recording r JOIN monitor m ON m.id = r.monitor_id
             WHERE r.stream_id IS NOT NULL AND r.stream_id != ''
               AND (r.vod_state = 'not_published'
                    OR (r.vod_state = 'found' AND COALESCE(r.vod_muted_secs, 0) > 0))
               AND COALESCE(r.went_live_at, r.started_at) >= ?1
               AND (r.recovery_state IS NULL OR r.recovery_state = 'failed')
               AND r.status != 'recording'
             ORDER BY COALESCE(r.went_live_at, r.started_at) DESC",
        )?;
        let cutoff = now - within_secs;
        let rows = stmt
            .query_map(params![cutoff], |r| {
                Ok(crate::models::RecoverableTake {
                    rec_id: r.get(0)?,
                    monitor_url: r.get(1)?,
                    stream_id: r.get(2)?,
                    start_epoch: r.get(3)?,
                    went_live_approx: r.get::<_, i64>(4)? != 0,
                    deleted: r.get::<_, i64>(5)? != 0,
                    vod_id: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct published Twitch VOD ids (most recent first), for harvesting the
    /// current CDN host set via GQL.
    pub fn published_vod_ids(&self, limit: i64) -> Result<Vec<String>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT vod_id FROM recording
             WHERE vod_id IS NOT NULL AND vod_id != '' AND vod_state = 'found'
             ORDER BY COALESCE(went_live_at, started_at) DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
                m.last_live_since, m.last_live_since_approx
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
                    vod_id, vod_state, vod_muted_secs,
                    recovery_state, recovered_path,
                    vod_dl_state, vod_dl_path, vod_dl_video_id,
                    backfill_path, full_path, COALESCE(trigger_info, ''),
                    head_backfill_state, COALESCE(trigger_rule_json, '')
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
                })
            })?
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
            head_backfill_state, COALESCE(trigger_rule_json, '')";

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
        })
    }

    /// A single recording by id (full row) — used by manual per-take actions
    /// (e.g. the "Backfill head" context-menu action).
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
               AND output_path LIKE '%.cache%'
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
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
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
               AND output_path LIKE '%.cache%'
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
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
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
            "SELECT output_path FROM recording WHERE output_path LIKE '%.cache%'",
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
    pub fn recordings_with_final_path(&self) -> Result<Vec<crate::models::Recording>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, monitor_id, started_at, ended_at, status,
                    COALESCE(output_path, ''), went_live_at, went_live_approx,
                    take_group, COALESCE(log_excerpt, '')
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
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
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
               AND NOT (output_path LIKE '%.ts' AND output_path LIKE '%.cache%')
               AND NOT (status = 'orphaned'
                        AND output_path != ''
                        AND output_path NOT LIKE '%.ts'
                        AND output_path NOT LIKE '%.cache%')
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
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
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
        crate::models::RemuxOpts { embed_thumbnail, embed_title, title_template, embed_subs }
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
                    recovery_state: None,
                    recovered_path: None,
                    vod_dl_state: None,
                    vod_dl_path: None,
                    vod_dl_video_id: None,
                    backfill_path: None,
                    full_path: None,
                    trigger_info: String::new(),
                    head_backfill_state: String::new(),
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

    fn map_channel(r: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
        Ok(Channel {
            id: r.get(0)?,
            name: r.get(1)?,
            url: r.get(2)?,
            platform: Platform::parse(&r.get::<_, String>(3)?),
            created_at: r.get(4)?,
            color: r.get(5)?,
            preferred_asset: crate::models::PreferredAssetSource::parse(&r.get::<_, String>(6)?),
            enabled: r.get::<_, i64>(7)? != 0,
            automation_enabled: r.get::<_, i64>(8)? != 0,
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
            automation_enabled: true,
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
            last_live_since: None,
            last_live_since_approx: false,
            sabr_codec_pref: SabrCodecPref::Inherit,
            sabr_codec_custom: String::new(),
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
        // Exercise the SABR codec-pref columns round-tripping through the
        // positional read in list_monitors_with_channels (idx 48/49).
        m.sabr_codec_pref = SabrCodecPref::Custom;
        m.sabr_codec_custom = "res,fps,br".into();
        let m2 = store.insert_monitor(&m).unwrap();
        assert_ne!(m1, m2);

        let rows = store.list_monitors_with_channels().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.channel.id == c1));
        assert!(rows.iter().any(|r| r.monitor.container == Container::Ts));
        let r2 = rows.iter().find(|r| r.monitor.id == m2).unwrap();
        assert_eq!(r2.monitor.sabr_codec_pref, SabrCodecPref::Custom);
        assert_eq!(r2.monitor.sabr_codec_custom, "res,fps,br");
        // The other instance keeps the default Inherit.
        let r1 = rows.iter().find(|r| r.monitor.id == m1).unwrap();
        assert_eq!(r1.monitor.sabr_codec_pref, SabrCodecPref::Inherit);

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
    fn automation_switch_and_live_meta() {
        let store = Store::open_in_memory().unwrap();
        let cid = store
            .upsert_channel("Live One", "https://twitch.tv/live1", Platform::Twitch)
            .unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // Both master switches default ON (new columns DEFAULT 1); automation_on
        // requires both channel + monitor.
        let row = |s: &Store| {
            s.list_monitors_with_channels()
                .unwrap()
                .into_iter()
                .find(|r| r.monitor.id == mid)
                .unwrap()
        };
        assert!(row(&store).monitor.automation_enabled);
        assert!(row(&store).channel.automation_enabled);
        assert!(row(&store).automation_on());

        // Disabling the instance's master switch turns automation off; the
        // Auto-record flag (`enabled`) is untouched.
        store.set_monitor_automation_enabled(mid, false).unwrap();
        let r = row(&store);
        assert!(!r.monitor.automation_enabled);
        assert!(!r.automation_on());
        assert!(r.monitor.enabled, "Auto-record flag independent of master");

        store.set_monitor_automation_enabled(mid, true).unwrap();
        store.set_channel_automation_enabled(cid, false).unwrap();
        assert!(!row(&store).automation_on(), "channel master gates too");
        store.set_channel_automation_enabled(cid, true).unwrap();
        assert!(row(&store).automation_on());

        // Live meta persists + round-trips; offline clears it.
        assert_eq!(row(&store).last_viewers, -1, "unknown by default");
        assert_eq!(row(&store).monitor.last_live_since, None, "unset by default");
        store
            .set_monitor_live_meta(
                mid, "Ranked grind", "VALORANT", "https://t/x.jpg", 1234, Some(1_000_000), true,
            )
            .unwrap();
        let r = row(&store);
        assert_eq!(r.last_title, "Ranked grind");
        assert_eq!(r.last_game, "VALORANT");
        assert_eq!(r.last_thumbnail_url, "https://t/x.jpg");
        assert_eq!(r.last_viewers, 1234);
        assert_eq!(r.monitor.last_live_since, Some(1_000_000));
        assert!(r.monitor.last_live_since_approx);

        store
            .set_monitor_live_meta(mid, "", "", "", -1, None, false)
            .unwrap();
        let r = row(&store);
        assert_eq!(r.last_title, "");
        assert_eq!(r.last_viewers, -1);
        assert_eq!(r.monitor.last_live_since, None, "cleared on offline");
    }

    #[test]
    fn create_container_with_flags_seeds_channel_switches() {
        let store = Store::open_in_memory().unwrap();

        // Plain create_container always defaults both switches on (schema
        // default) — the mismatch this feature avoids.
        let plain = store.create_container("Plain").unwrap();
        let ch = |s: &Store, id: i64| {
            s.list_channels().unwrap().into_iter().find(|c| c.id == id).unwrap()
        };
        assert!(ch(&store, plain).enabled);
        assert!(ch(&store, plain).automation_enabled);

        // Auto off + Enabled off on the seeding instance -> channel matches.
        let off = store.create_container_with_flags("Off", false, false).unwrap();
        assert!(!ch(&store, off).enabled);
        assert!(!ch(&store, off).automation_enabled);

        // Auto off + Enabled on -> channel matches independently per flag.
        let mixed = store.create_container_with_flags("Mixed", false, true).unwrap();
        assert!(!ch(&store, mixed).enabled);
        assert!(ch(&store, mixed).automation_enabled);
    }

    fn sample_video() -> Video {
        Video {
            id: 0,
            url: "https://youtube.com/watch?v=abc".into(),
            title: "My VOD".into(),
            channel: String::new(),
            platform: Platform::YouTube,
            tool: Tool::YtDlp,
            tool_binary: String::new(),
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
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None, "", "")
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
    fn ad_break_insert_is_idempotent() {
        // A re-attach after a restart may re-observe a break already persisted; the
        // UNIQUE(recording_id, at_secs) index + INSERT OR IGNORE makes that a no-op.
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let rid = store
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None, "", "")
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
            .insert_recording(mid, 1_000, "C:/rec/out.mkv", Some(1_000), false, Some("s1"), None, "", "")
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
    fn parse_relative_age_buckets() {
        assert_eq!(parse_relative_age("37 seconds ago"), Some(37));
        assert_eq!(parse_relative_age("5 minutes ago (edited)"), Some(300));
        assert_eq!(parse_relative_age("10 hours ago"), Some(36_000));
        assert_eq!(parse_relative_age("2 days ago"), Some(172_800));
        assert_eq!(parse_relative_age("Streamed 3 weeks ago"), Some(1_814_400));
        assert_eq!(parse_relative_age("1 month ago"), Some(2_592_000));
        assert_eq!(parse_relative_age("1 year ago"), Some(31_536_000));
        assert_eq!(parse_relative_age("just now"), Some(0));
        // No <number> <unit> pair → unknown.
        assert_eq!(parse_relative_age(""), None);
        assert_eq!(parse_relative_age("yesterday"), None);
        assert_eq!(parse_relative_age("Episode 5"), None);
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
    fn fill_published_at_estimates_legacy_rows() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // Simulate pre-v46 rows: published_at still at the column DEFAULT 0.
        // The estimate anchors at last_seen (the scan that wrote the text).
        let ins = |post_id: &str, text: &str, first: i64, last: i64| {
            store
                .db()
                .execute(
                    "INSERT INTO community_post
                         (monitor_id, channel_id, post_id, published_text,
                          first_seen, last_seen, published_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                    params![mid, cid, post_id, text, first, last],
                )
                .unwrap();
        };
        ins("a", "2 days ago", 1_000, 2_000_000);
        ins("b", "no date here", 1_234, 2_000_000);

        fill_published_at(&store.db()).unwrap();

        let rows = store.list_community_posts(None, 100).unwrap();
        let get = |id: &str| rows.iter().find(|r| r.post_id == id).unwrap().published_at;
        assert_eq!(get("a"), 2_000_000 - 172_800);
        assert_eq!(get("b"), 1_234, "unparseable → first_seen fallback");
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
    fn reclassify_v48_tags_and_repairs() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // Insert three legacy rows via raw SQL at the pre-v48 shape (the new
        // columns exist but sit at their DEFAULT 'channel'/'' — reclassify must
        // move them). raw_json mirrors the real renderer subtrees.
        let owner = "UCowner00000000000000000";
        let author_ep = |id: &str| {
            serde_json::json!({
                "profileCardCommand": { "profileOwnerExternalChannelId": id }
            })
        };
        let own_post = serde_json::json!({
            "postId": "own1",
            "authorText": { "runs": [{ "text": "Streamer" }] },
            "authorEndpoint": author_ep(owner),
            "showPostAuthorBackgroundHighlight": { "lightThemeColor": 1 },
            "contentText": { "runs": [{ "text": "my post" }] }
        });
        let viewer_post = serde_json::json!({
            "postId": "fan1",
            "authorText": { "runs": [{ "text": "A Fan" }] },
            "authorEndpoint": author_ep("UCfan0000000000000000000"),
            "contentText": { "runs": [{ "text": "hello there" }] }
        });
        // A reshare the old path stored with empty author/body.
        let reshare = serde_json::json!({
            "postId": "re1",
            "displayName": { "runs": [{ "text": "Streamer" }] },
            "endpoint": author_ep(owner),
            "content": { "runs": [{ "text": "check this out" }] },
            "originalPost": { "backstagePostRenderer": {
                "postId": "orig1",
                "authorText": { "runs": [{ "text": "Miniko Mew" }] },
                "authorEndpoint": author_ep("UCorig0000000000000000000"),
                "publishedTimeText": { "runs": [{ "text": "1 month ago" }] },
                "contentText": { "runs": [{ "text": "the original" }] }
            }}
        });
        let ins = |post_id: &str, author: &str, body: &str, raw: &serde_json::Value| {
            store
                .db()
                .execute(
                    "INSERT INTO community_post
                         (monitor_id, channel_id, post_id, author, body_text,
                          raw_json, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 100, 100)",
                    params![mid, cid, post_id, author, body, raw.to_string()],
                )
                .unwrap();
        };
        ins("own1", "Streamer", "my post", &own_post);
        ins("fan1", "A Fan", "hello there", &viewer_post);
        ins("re1", "", "", &reshare); // legacy mangled reshare

        reclassify_posts_v48(&store.db()).unwrap();

        let rows = store.list_community_posts(None, 100).unwrap();
        let get = |id: &str| rows.iter().find(|r| r.post_id == id).unwrap();
        assert_eq!(get("own1").author_kind, "channel");
        assert_eq!(get("fan1").author_kind, "viewer");

        let re = get("re1");
        assert_eq!(re.author_kind, "channel");
        assert_eq!(re.author, "Streamer", "reshare author rebuilt from displayName");
        assert_eq!(re.body_text, "check this out", "reshare body rebuilt from content");
        let shared: serde_json::Value = serde_json::from_str(&re.shared_json).unwrap();
        assert_eq!(shared["author"], "Miniko Mew");
        assert_eq!(shared["body_text"], "the original");
        assert_eq!(shared["published_text"], "1 month ago");
    }

    #[test]
    fn migration_43_backfill_columns_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();

        store.set_recording_backfill_path(rid, "C:/tmp/a.head.mkv").unwrap();
        store.set_recording_full_path(rid, "C:/tmp/a.full.mkv").unwrap();
        let recs = store.recordings_for_monitor(mid).unwrap();
        assert_eq!(recs[0].backfill_path.as_deref(), Some("C:/tmp/a.head.mkv"));
        assert_eq!(recs[0].full_path.as_deref(), Some("C:/tmp/a.full.mkv"));

        let (status, out, head, full) = store.backfill_concat_info(rid).unwrap().unwrap();
        assert_eq!(status, "recording");
        assert_eq!(out, "C:/tmp/a.mkv");
        assert_eq!(head.as_deref(), Some("C:/tmp/a.head.mkv"));
        assert_eq!(full.as_deref(), Some("C:/tmp/a.full.mkv"));
    }

    #[test]
    fn migration_44_trigger_info_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let hit = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "title ~ \"karaoke\"", "")
            .unwrap();
        let normal = store
            .insert_recording(mid, 200, "C:/tmp/b.mkv", Some(150), false, Some("s2"), None, "", "")
            .unwrap();
        let recs = store.recordings_for_monitor(mid).unwrap();
        let by_id = |id| recs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(by_id(hit).trigger_info, "title ~ \"karaoke\"");
        assert_eq!(by_id(normal).trigger_info, "");
    }

    #[test]
    fn migration_54_trigger_rule_json_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rule_json = r#"{"pattern":"gdq segment","stop_on_unmatch":true,"lead_secs":30,"end_delay_secs":15}"#;
        let hit = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "title ~ \"gdq segment\"", rule_json)
            .unwrap();
        let normal = store
            .insert_recording(mid, 200, "C:/tmp/b.mkv", Some(150), false, Some("s2"), None, "", "")
            .unwrap();
        let recs = store.recordings_for_monitor(mid).unwrap();
        let by_id = |id| recs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(by_id(hit).trigger_rule_json, rule_json);
        assert_eq!(by_id(normal).trigger_rule_json, "");
        // get_recording (RECORDING_FULL_COLUMNS path) must agree.
        assert_eq!(store.get_recording(hit).unwrap().unwrap().trigger_rule_json, rule_json);
    }

    fn about(cid: i64, platform: &str, account: &str, hash: &str, desc: &str) -> NewAboutSnapshot {
        NewAboutSnapshot {
            channel_id: cid,
            platform: platform.into(),
            account: account.into(),
            content_hash: hash.into(),
            description: desc.into(),
            panels_json: "[]".into(),
            links_json: "[]".into(),
            raw_json: String::new(),
        }
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
    fn vod_muted_secs_setter_preserves_vod_state_and_id() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let rid = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.set_recording_vod_found(rid, "999", 0).unwrap();
        store.set_recording_vod_muted_secs(rid, 360).unwrap();
        let rec = &store.recordings_for_monitor(mid).unwrap()[0];
        assert_eq!(rec.vod_id.as_deref(), Some("999"), "vod_id untouched");
        assert_eq!(rec.vod_state.as_deref(), Some("found"), "vod_state untouched");
        assert_eq!(rec.vod_muted_secs, Some(360));
    }

    #[test]
    fn vod_archive_replay_candidates_excludes_detached_and_terminal() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();

        // rec1: 'downloading' with a completed video → replay candidate.
        let r1 = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        let v1 = store.insert_video(&sample_video()).unwrap();
        store.finish_video(v1, 200, 10, Some(0), "completed", "C:/tmp/a.vod.mkv", "").unwrap();
        store.set_recording_vod_dl(r1, "downloading", Some(v1)).unwrap();

        // rec2: video completed but the recording already archived → not a candidate.
        let r2 = store
            .insert_recording(mid, 300, "C:/tmp/b.mkv", Some(250), false, Some("s2"), None, "", "")
            .unwrap();
        let v2 = store.insert_video(&sample_video()).unwrap();
        store.finish_video(v2, 400, 10, Some(0), "completed", "C:/tmp/b.vod.mkv", "").unwrap();
        store.set_recording_vod_dl(r2, "downloading", Some(v2)).unwrap();
        store.set_recording_vod_archived(r2, "C:/tmp/b.vod.mkv", "archived").unwrap();

        let cands = store.vod_archive_replay_candidates().unwrap();
        assert_eq!(
            cands,
            vec![(r1, v1, "C:/tmp/a.vod.mkv".to_string())],
            "only the stuck-downloading row replays"
        );

        // A video still in the detached registry is being adopted — excluded.
        store
            .register_detached(&crate::models::DetachedRow {
                kind: DetachedKind::Video,
                ref_id: v1,
                monitor_id: None,
                pid: 1,
                proc_start: 0,
                job_name: String::new(),
                log_path: String::new(),
                capture_path: String::new(),
                final_path: String::new(),
                remux_to_mkv: false,
                take_group: None,
                spawn_build: String::new(),
                started_at: 0,
                secondary: false,
                stream_id: None,
                went_live_at: None,
            })
            .unwrap();
        assert!(store.vod_archive_replay_candidates().unwrap().is_empty());
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
