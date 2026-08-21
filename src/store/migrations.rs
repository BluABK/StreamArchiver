//! Schema migrations (`migrate`, 54 sequential version blocks) and the
//! post-parse helpers some migrations call.

use super::*;

/// Parse a YouTube relative-time string ("2 weeks ago", "Streamed 3 days ago",
/// "1 month ago (edited)") into an age in seconds. Months/years use 30/365-day
/// approximations — the source only has bucket precision anyway. `None` when no
/// `<number> <unit>` pair is found.
pub(super) fn parse_relative_age(text: &str) -> Option<i64> {
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

/// Schema v92 repair: back-fill `recording.gated` and drop the "Capture
/// failed" alerts the old behaviour filed on takes that were never ours to
/// capture. Extracted from the migration so it can be tested against a
/// hand-built damaged database.
///
/// Three steps:
/// 1. The take each existing 🔒 alert names is gated by definition.
/// 2. So is every other 0-byte failed take of that same broadcast — same
///    stream, same entitlement, and those are exactly the takes that were
///    wrongly reddened (the 🔒 alert is keyed by the broadcast and could name
///    only the first of them). `bytes = 0` keeps this off any take that
///    actually captured something.
/// 3. Their `capture_failed` alerts were filed on a false premise; drop them,
///    or the takes stay red and keep the 🚨 badge lit over a broadcast that
///    was never ours to capture.
fn repair_gated_takes(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "UPDATE recording SET gated = 1 WHERE id IN
             (SELECT recording_id FROM capture_alert
              WHERE kind = 'sub_only' AND recording_id IS NOT NULL);
         UPDATE recording SET gated = 1
          WHERE gated = 0 AND status = 'failed' AND bytes = 0
            AND stream_id IS NOT NULL AND stream_id != ''
            AND EXISTS (SELECT 1 FROM recording g
                        WHERE g.gated = 1
                          AND g.monitor_id = recording.monitor_id
                          AND g.stream_id = recording.stream_id);
         DELETE FROM capture_alert
          WHERE kind = 'capture_failed'
            AND recording_id IN (SELECT id FROM recording WHERE gated = 1);",
    )?;
    Ok(())
}

impl Store {
    pub(super) fn migrate(&self) -> Result<()> {
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
        if version < 55 {
            // Title/category history for a MONITOR (not a recording): unlike
            // `stream_meta_change` (v15, relative-second offsets scoped to one
            // take, cleared/rebuilt per recording), this is a single continuous,
            // wall-clock-timestamped ledger that keeps growing whether or not
            // anything is being recorded — fed by the scheduler's own poll (live
            // but not recording) and by `meta_watcher` (while recording), so a
            // channel's title/game history is complete regardless of Auto/Enabled
            // state. Cascades when the monitor is removed.
            conn.execute_batch(
                r#"
                CREATE TABLE monitor_stream_change (
                    id            INTEGER PRIMARY KEY,
                    monitor_id    INTEGER NOT NULL REFERENCES monitor(id) ON DELETE CASCADE,
                    at_unix       INTEGER NOT NULL,
                    kind          TEXT NOT NULL,
                    old_value     TEXT NOT NULL DEFAULT '',
                    new_value     TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX idx_monitor_stream_change_monitor ON monitor_stream_change(monitor_id, at_unix);
                "#,
            )?;
            conn.pragma_update(None, "user_version", 55)?;
        }
        if version < 56 {
            // Minute-resolution poll/detect request history behind the Stats
            // view's error-rate/request-volume graphs. One row per
            // (minute-bucket, platform, detection-method); the scheduler
            // upserts counter increments once per tick and prunes rows past
            // the retention window (see `Store::record_poll_history`).
            // Coarser views (hourly, daily, …) are SQL GROUP BY aggregations
            // at query time, not extra tiers of storage. Deliberately no
            // monitor FK: rows aggregate across monitors and must survive
            // monitor deletion — this is request health, not channel history.
            conn.execute_batch(
                r#"
                CREATE TABLE poll_history (
                    bucket_t  INTEGER NOT NULL,
                    platform  TEXT NOT NULL,
                    method    TEXT NOT NULL,
                    polls     INTEGER NOT NULL DEFAULT 0,
                    errors    INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (bucket_t, platform, method)
                ) WITHOUT ROWID;
                "#,
            )?;
            conn.pragma_update(None, "user_version", 56)?;
        }
        if version < 57 {
            // The live capture's first MPEG-TS PTS (ffprobe `format=start_time`,
            // seconds), probed off the raw growing `.ts`. Twitch HLS segments
            // keep the broadcast's own PTS timeline, so this minus the DVR
            // playlist's segment-0 PTS is the *exact* stream position where the
            // capture joined — the head backfill cuts there instead of trusting
            // wall-clock arithmetic (which overshoots by the broadcast latency,
            // duplicating ~6s at the head/live splice). Persisted because the
            // promote remux resets timestamps: once the take is an MKV the raw
            // signal is gone, and a later manual "Backfill head" needs it.
            conn.execute_batch("ALTER TABLE recording ADD COLUMN capture_start_pts REAL;")?;
            conn.pragma_update(None, "user_version", 57)?;
        }
        if version < 58 {
            // Twitch "Stream Together" collab history. One row per observed
            // collab session per monitor: `shared_chat` rows mirror Helix
            // GET /shared_chat/session (session_id is Twitch's, participants
            // are resolved to logins/display names at observation time and
            // stored denormalized as JSON — names at the time of the collab
            // are themselves history); `title` rows are @mention-derived
            // (one per broadcast, keyed by stream_id, heuristic source).
            // `ended_at` is stamped when the session disappears from a poll
            // or the channel goes offline; NULL = still active. Set changes
            // additionally feed monitor_stream_change (kind='collab') so the
            // existing 📝 history popup shows them. `monitor.last_collab` is
            // the live-display JSON (like last_title); `schedule_segment
            // .collab` carries upcoming-collab names (OCR field / title
            // mentions) for the calendar.
            conn.execute_batch(
                r#"
                CREATE TABLE collab_session (
                    id            INTEGER PRIMARY KEY,
                    monitor_id    INTEGER NOT NULL REFERENCES monitor(id) ON DELETE CASCADE,
                    source        TEXT NOT NULL,
                    session_id    TEXT NOT NULL DEFAULT '',
                    stream_id     TEXT NOT NULL DEFAULT '',
                    host_id       TEXT NOT NULL DEFAULT '',
                    participants  TEXT NOT NULL DEFAULT '[]',
                    first_seen_at INTEGER NOT NULL,
                    last_seen_at  INTEGER NOT NULL,
                    ended_at      INTEGER
                );
                CREATE INDEX idx_collab_session_monitor ON collab_session(monitor_id, first_seen_at);
                ALTER TABLE monitor ADD COLUMN last_collab TEXT NOT NULL DEFAULT '';
                ALTER TABLE schedule_segment ADD COLUMN collab TEXT NOT NULL DEFAULT '';
                "#,
            )?;
            conn.pragma_update(None, "user_version", 58)?;
        }
        if version < 59 {
            // Channel stats history. `viewer_history`: one row per monitor
            // per minute while live — viewers is the bucket peak (peak-
            // preserving, so downsampling to coarser buckets via MAX keeps
            // spikes), followers is the platform-reported total when the
            // detection path carries one (Kick channel JSON; Twitch/YouTube
            // expose none without owner credentials), stream_id ties samples
            // to a broadcast where the platform provides ids. Kept forever
            // by default; the optional auto-downsample rewrites rows older
            // than a configurable age into 10-minute buckets (see
            // `downsample_viewer_history`) instead of deleting them.
            // `stream_event`: discrete channel events — subs/resubs/gift
            // subs/bits parsed live from the recorded Twitch chat (IRC
            // USERNOTICE / bits tags, so recording-time only), raids from
            // chat and/or EventSub `channel.raid` (deduped on insert).
            conn.execute_batch(
                r#"
                CREATE TABLE viewer_history (
                    monitor_id INTEGER NOT NULL REFERENCES monitor(id) ON DELETE CASCADE,
                    bucket_t   INTEGER NOT NULL,
                    viewers    INTEGER NOT NULL,
                    followers  INTEGER,
                    stream_id  TEXT NOT NULL DEFAULT '',
                    span_secs  INTEGER NOT NULL DEFAULT 60,
                    PRIMARY KEY (monitor_id, bucket_t)
                ) WITHOUT ROWID;
                CREATE TABLE stream_event (
                    id         INTEGER PRIMARY KEY,
                    monitor_id INTEGER NOT NULL REFERENCES monitor(id) ON DELETE CASCADE,
                    at         INTEGER NOT NULL,
                    stream_id  TEXT NOT NULL DEFAULT '',
                    kind       TEXT NOT NULL,
                    actor      TEXT NOT NULL DEFAULT '',
                    target     TEXT NOT NULL DEFAULT '',
                    amount     INTEGER NOT NULL DEFAULT 0,
                    tier       TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX idx_stream_event_monitor ON stream_event(monitor_id, at);
                "#,
            )?;
            conn.pragma_update(None, "user_version", 59)?;
        }
        if version < 60 {
            // Free-text payload for stream events: the deleted message's text
            // excerpt (`msg_deleted`), the chat-mode change description
            // (`chat_mode`, e.g. "Slow mode on (30s)"), or the role change
            // (`role_change`, e.g. "gained the moderator badge"). Added for
            // the chat-moderation event kinds; the v59 kinds leave it ''.
            conn.execute_batch("ALTER TABLE stream_event ADD COLUMN detail TEXT NOT NULL DEFAULT '';")?;
            conn.pragma_update(None, "user_version", 60)?;
        }
        if version < 61 {
            // "Stream ended, capture still finishing" flag: set by the
            // in-recording meta watcher once the platform authoritatively
            // reports the channel offline while the capture tool is still
            // running (live-from-start backlog drain, tail download, or the
            // final mux of a huge file). The grid shows a distinct ⏬ state
            // instead of the live-looking "recording" (the Layna incident:
            // a SABR DVR drain kept "recording" up for hours post-stream).
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN capture_offline INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 61)?;
        }
        if version < 62 {
            // Live stream tags (", "-joined; Twitch Helix / Kick best-effort),
            // tracked exactly like last_title/last_game: the current value
            // lives here for the grid's Tags column, and every change lands
            // in the continuous `monitor_stream_change` ledger (kind='tags')
            // plus the per-take `stream_meta_change` log — so tag history
            // rides the existing 📝 popups with no extra tables.
            conn.execute_batch("ALTER TABLE monitor ADD COLUMN last_tags TEXT NOT NULL DEFAULT '';")?;
            conn.pragma_update(None, "user_version", 62)?;
        }
        if version < 63 {
            // More free-ride Helix/scrape data (the "what are we discarding"
            // audit): the stream's language + game id from every Twitch poll
            // (game id enables box-art lookups later; both persist through
            // offline as "the channel's usual values"), and the published
            // VOD's view count from the VOD checker's Get Videos calls
            // (NULL = never seen).
            conn.execute_batch(
                r#"
                ALTER TABLE monitor ADD COLUMN last_language TEXT NOT NULL DEFAULT '';
                ALTER TABLE monitor ADD COLUMN last_game_id TEXT NOT NULL DEFAULT '';
                ALTER TABLE recording ADD COLUMN vod_views INTEGER;
                "#,
            )?;
            conn.pragma_update(None, "user_version", 63)?;
        }
        if version < 64 {
            // Capture alerts (the 🚨 Warnings window): problems scraped from
            // the capture tools' own log files (streamlink sequence gaps =
            // lost data, failed segment fetches, yt-dlp ERROR/WARNING lines).
            // One aggregated row per (take, kind) — a take that logs 74 gap
            // warnings is ONE alert whose count/lost_segments grow (growth
            // also un-acks, so fresh data loss re-lights the badge).
            // `gap_range` holds the derived lost time ranges (broadcast-start
            // offsets, padded/coalesced) that the Twitch gap-recovery job
            // fetches back from the VOD CDN; `out_path` is the recovered
            // patch file once `state`='done'.
            conn.execute_batch(
                r#"
                CREATE TABLE capture_alert (
                    id            INTEGER PRIMARY KEY,
                    first_at      INTEGER NOT NULL,
                    last_at       INTEGER NOT NULL,
                    kind          TEXT NOT NULL,
                    severity      TEXT NOT NULL,
                    source        TEXT NOT NULL DEFAULT '',
                    take_key      TEXT NOT NULL,
                    monitor_id    INTEGER,
                    recording_id  INTEGER,
                    video_id      INTEGER,
                    channel       TEXT NOT NULL DEFAULT '',
                    count         INTEGER NOT NULL DEFAULT 1,
                    lost_segments INTEGER NOT NULL DEFAULT 0,
                    ranges_total  INTEGER NOT NULL DEFAULT 0,
                    recovered     INTEGER NOT NULL DEFAULT 0,
                    last_line     TEXT NOT NULL DEFAULT '',
                    acked         INTEGER NOT NULL DEFAULT 0,
                    UNIQUE(take_key, kind)
                );
                CREATE INDEX idx_capture_alert_last ON capture_alert(last_at DESC);
                CREATE TABLE gap_range (
                    id           INTEGER PRIMARY KEY,
                    recording_id INTEGER NOT NULL,
                    start_secs   REAL NOT NULL,
                    end_secs     REAL NOT NULL,
                    state        TEXT NOT NULL DEFAULT 'pending',
                    attempts     INTEGER NOT NULL DEFAULT 0,
                    out_path     TEXT NOT NULL DEFAULT '',
                    UNIQUE(recording_id, start_secs)
                );
                "#,
            )?;
            conn.pragma_update(None, "user_version", 64)?;
        }
        if version < 65 {
            // Muted-audio bookkeeping for gap recovery: `build_playlist` falls
            // back to DMCA-muted segment copies when the clean ones are gone
            // (a muted patch beats no patch) — record how many per recovered
            // range, and the rollup on the alert row, so the Warnings window
            // can say "recovered, but N segments are muted".
            conn.execute_batch(
                r#"
                ALTER TABLE gap_range ADD COLUMN muted_segs INTEGER NOT NULL DEFAULT 0;
                ALTER TABLE capture_alert ADD COLUMN recovered_muted INTEGER NOT NULL DEFAULT 0;
                "#,
            )?;
            conn.pragma_update(None, "user_version", 65)?;
        }
        if version < 66 {
            // Gap-splice state — mirrors `head_backfill_state` (v53) exactly:
            // "" = not attempted (also the required precondition for the
            // splice trigger), "queued", "done" (terminal — never
            // re-attempted even if a new gap range is discovered later),
            // "mismatch"/"anchor_failed"/"verify_failed" (which safety check
            // blocked it — distinct values so the Issues hover text can say
            // which one), "*_ack" for user-dismissed variants.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN gap_splice_state TEXT NOT NULL DEFAULT '';
                 CREATE INDEX IF NOT EXISTS idx_recording_gap_splice_issue
                     ON recording(gap_splice_state)
                     WHERE gap_splice_state NOT IN ('', 'done', 'queued');",
            )?;
            conn.pragma_update(None, "user_version", 66)?;
        }
        if version < 67 {
            // Acknowledge a failed/aborted/orphaned take: stops it bubbling
            // its ⚠ up to the instance/channel row rollup (which otherwise
            // shows the LATEST take's status forever, even if it was 10
            // takes ago and every subsequent one succeeded) and drops it out
            // of the Issues panel's error list — but the take row itself
            // keeps its ⚠, just tinted muted instead of red, so the failure
            // history stays visible at its own row. A plain bool, not a
            // "*_ack" status-suffix (unlike head_backfill_state/
            // gap_splice_state) since `recording.status` itself drives too
            // much other logic (is_active(), queries, …) to overload.
            conn.execute_batch("ALTER TABLE recording ADD COLUMN err_ack INTEGER NOT NULL DEFAULT 0;")?;
            conn.pragma_update(None, "user_version", 67)?;
        }
        if version < 68 {
            // Stamped when a from-start-configured YouTube SABR take got
            // silently downgraded to live-edge-only for this one attempt
            // (see `Supervisor::sabr_dvr_exceeded`) — the broadcast is
            // already older than SABR's DVR rewind window (~4h), so every
            // from-start fetch stalls immediately with "not near live
            // head"; capturing from the live edge is strictly better than
            // retrying a doomed fetch forever, but without this flag the
            // take just silently has no head/missed-intro with no visible
            // reason why. Set once at insert time, never cleared — it's a
            // fact about how THIS take was captured, not a live state.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN sabr_live_edge_fallback INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 68)?;
        }
        if version < 69 {
            // Chapters state — same shape as `gap_splice_state`/
            // `head_backfill_state`: "" = not attempted (also the required
            // precondition for the chapters trigger), "queued", "done"
            // (terminal), "skipped" (feature off or take excluded, e.g.
            // multi-part merged), "failed" (embed pass itself errored, file
            // untouched). Unlike gap_splice_state/head_backfill_state this
            // is purely additive metadata, not a media-integrity concern —
            // no Issues-panel section needed, so no partial-index either.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN chapters_state TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 69)?;
        }
        if version < 70 {
            // The actual chapter list (JSON `[{"at_secs":f64,"title":str}, …]`)
            // from the most recent successful embed — set alongside
            // `chapters_state = "done"`, never otherwise. Backs the
            // Background view's chapters detail popup ("which stream, which
            // file, which chapters at which timestamp"); a DB copy rather
            // than re-probing the MKV on demand, since it's a handful of
            // small strings and stays correct even if the file later moves.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN chapters_json TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 70)?;
        }
        if version < 71 {
            // Watch-state for the Backlog/Stream History views (unwatched /
            // started / skipped / watched). Keyed by the same stable
            // broadcast key `models::stream_key()` computes for
            // `StreamGroup::key` — state belongs to the broadcast, not any
            // one take/file, so it survives reconnects without needing to
            // pick a "representative" take. A broadcast with no row here is
            // `'unwatched'` by convention (see `Store::stream_watch_states`)
            // — cheaper than backfilling one row per pre-existing broadcast.
            conn.execute_batch(
                "CREATE TABLE stream_watch (
                    stream_key TEXT PRIMARY KEY,
                    monitor_id INTEGER NOT NULL,
                    watch_state TEXT NOT NULL DEFAULT 'unwatched',
                    watch_state_at INTEGER
                );",
            )?;
            conn.pragma_update(None, "user_version", 71)?;
        }
        if version < 72 {
            // Registry for still-running ffmpeg `-c copy` post-processing passes
            // (chapters/thumbnail embed, remux, gap-splice/head-backfill concat,
            // split-part merge) — parallel to `detached_process` but for these
            // jobs instead of capture/download/chat tools. Written right after
            // spawn, deleted at finalize. On the next launch the supervisor
            // reconciles every row: re-attach to still-alive ones (tail the
            // progress file), finalize ones whose `.tmp` output finished while
            // the app was down, or clean up a genuinely-interrupted one and let
            // the normal sweep re-queue it from scratch. See
            // `src/downloader/ffmpeg_job.rs`. Added after the Nihmune chapters
            // task lost 12+ hours of throttled `-c copy` progress to a restart.
            conn.execute_batch(
                "CREATE TABLE ffmpeg_job (
                    id           INTEGER PRIMARY KEY,
                    kind         TEXT NOT NULL,
                    ref_id       INTEGER NOT NULL,
                    pid          INTEGER NOT NULL,
                    proc_start   INTEGER NOT NULL,
                    job_name     TEXT NOT NULL DEFAULT '',
                    tmp_path     TEXT NOT NULL DEFAULT '',
                    final_path   TEXT NOT NULL DEFAULT '',
                    progress_log TEXT NOT NULL DEFAULT '',
                    total_secs   INTEGER,
                    started_at   INTEGER NOT NULL,
                    spawn_build  TEXT NOT NULL DEFAULT ''
                );
                CREATE UNIQUE INDEX idx_ffmpeg_job_kind_ref ON ffmpeg_job(kind, ref_id);",
            )?;
            conn.pragma_update(None, "user_version", 72)?;
        }
        if version < 73 {
            // History of automatic media disposals (trash/Recycle Bin/permanent)
            // for the Trash view — every `disposal::dispose_media` call inserts
            // one row here. `state` distinguishes a Trash-method disposal still
            // sitting in its trash folder ("soft_deleted", user-actionable:
            // restore or permanently delete) from one that's already terminal
            // ("permanent" — Recycle/Delete method, or a soft-deleted row the
            // user permanently deleted) or reversed ("restored"). Recycle Bin
            // rows are informational only — Windows owns that recovery path.
            conn.execute_batch(
                "CREATE TABLE disposal_record (
                    id            INTEGER PRIMARY KEY,
                    rec_id        INTEGER NOT NULL,
                    reason        TEXT NOT NULL,
                    method        TEXT NOT NULL,
                    original_path TEXT NOT NULL,
                    trash_path    TEXT NOT NULL DEFAULT '',
                    state         TEXT NOT NULL,
                    disposed_at   INTEGER NOT NULL,
                    updated_at    INTEGER NOT NULL
                );
                CREATE INDEX idx_disposal_record_rec ON disposal_record(rec_id);",
            )?;
            conn.pragma_update(None, "user_version", 73)?;
        }
        if version < 74 {
            // Distinguishes a disposal logged live (`disposal::log_disposal`,
            // at the exact moment it happened) from one reconstructed after
            // the fact by the one-time historical-import scan
            // (`disposal_backfill::run_historical_backfill`) for disposals
            // that predate the Trash view — where the method and exact
            // timestamp are unknowable and the path is either read back
            // verbatim from a DB column that survived ("historical_exact")
            // or inferred from a filename naming convention
            // ("historical_guess"). Default 'live' backfills every existing
            // row correctly, since the Trash view didn't exist before v73.
            conn.execute_batch(
                "ALTER TABLE disposal_record ADD COLUMN confidence TEXT NOT NULL DEFAULT 'live';",
            )?;
            conn.pragma_update(None, "user_version", 74)?;
        }
        if version < 75 {
            // How many automatic chapters-embed attempts have failed in a row
            // since the last reset — gates the automatic retry sweep
            // (`downloader::chapters::retry_queued_chapters_loop`): a
            // transient failure (e.g. the disk was momentarily unreachable)
            // requeues (`chapters_state = "queued"`) for a later retry, but
            // once `MAX_CHAPTERS_ATTEMPTS` is reached it gives up for good
            // (`chapters_state = "failed"`, same as before this existed)
            // instead of retrying a permanently-broken source forever. Added
            // after a USB enclosure overload left ~15 recordings stuck at
            // `chapters_state = 'failed'` with no automatic way back.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN chapters_attempts INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 75)?;
        }
        if version < 76 {
            // One-time repair for recordings already stuck at
            // `chapters_state = 'failed'` from before the v75 retry system
            // existed (the 2026-07-26 USB overload incident left ~15 like
            // this) — requeue them into the SAME targeted self-heal path a
            // fresh transient failure now uses, rather than needing the
            // "Re-embed chapters" bulk button, which would also needlessly
            // re-copy every already-`'done'` recording in the library.
            // `chapters_attempts = 0` is the discriminator: the new system
            // always bumps attempts to at least 1 before ever landing on
            // `'failed'`, so a `'failed'` row still at 0 can only be a
            // legacy one that never got a fair automatic retry — a row
            // that's genuinely exhausted 5 real retries (`attempts = 5`) is
            // deliberately left alone.
            let n = conn.execute(
                "UPDATE recording SET chapters_state='queued'
                 WHERE chapters_state='failed' AND chapters_attempts=0",
                [],
            )?;
            if n > 0 {
                tracing::info!(count = n, "migration: requeued pre-existing failed chapters embeds for automatic retry");
            }
            conn.pragma_update(None, "user_version", 76)?;
        }
        if version < 77 {
            // Channel groups: a channel can belong to any number of groups
            // (`channel_group_member`, many-to-many), but has at most one
            // *primary* group (`channel.primary_group_id`) — the one it
            // clusters under in the Streams grid's default view. No FK
            // constraint on `primary_group_id` itself (every other column
            // added post-v1 to `channel` follows the same plain-ALTER
            // convention — this codebase's FKs are only ever declared at a
            // table's original CREATE TABLE); a deleted group clears any
            // channel's `primary_group_id` that pointed at it explicitly, in
            // application code (`delete_channel_group`).
            conn.execute_batch(
                "CREATE TABLE channel_group (
                    id          INTEGER PRIMARY KEY,
                    name        TEXT NOT NULL,
                    created_at  INTEGER NOT NULL
                );
                CREATE TABLE channel_group_member (
                    channel_id  INTEGER NOT NULL REFERENCES channel(id) ON DELETE CASCADE,
                    group_id    INTEGER NOT NULL REFERENCES channel_group(id) ON DELETE CASCADE,
                    PRIMARY KEY (channel_id, group_id)
                );
                ALTER TABLE channel ADD COLUMN primary_group_id INTEGER;",
            )?;
            conn.pragma_update(None, "user_version", 77)?;
        }
        if version < 78 {
            // Recording groups: a free-form label spanning any number of
            // *takes* (`recording_group_member`, many-to-many on
            // `recording.id`), e.g. "Numi Subathon 2025" tying together every
            // take of every broadcast across a week. No "primary" concept
            // (unlike channel groups) — a take's home in the tree is always
            // its channel/instance, unaffected by this; a recording group is
            // a pure cross-cutting tag, surfaced only via the Streams grid's
            // group filter. Adding a *Stream* (a broadcast, possibly several
            // takes) to a group inserts membership rows for every one of its
            // takes at once (`Store::add_recordings_to_group`), so "is this
            // stream in group G" is answerable by checking any one take.
            conn.execute_batch(
                "CREATE TABLE recording_group (
                    id          INTEGER PRIMARY KEY,
                    name        TEXT NOT NULL,
                    created_at  INTEGER NOT NULL
                );
                CREATE TABLE recording_group_member (
                    recording_id  INTEGER NOT NULL REFERENCES recording(id) ON DELETE CASCADE,
                    group_id      INTEGER NOT NULL REFERENCES recording_group(id) ON DELETE CASCADE,
                    PRIMARY KEY (recording_id, group_id)
                );",
            )?;
            conn.pragma_update(None, "user_version", 78)?;
        }
        if version < 79 {
            // Where this take's chat log lives, when it ISN'T derivable from
            // `output_path` — i.e. a chat-only session (`status =
            // 'not_recorded'` with chat capture on; see
            // `downloader::chat_only`), which has no video file at all and so
            // no stem for `ui::chat::chat_file_candidates` to swap an
            // extension on. Empty for every ordinary take, whose sidecar is
            // still found from `output_path` exactly as before — this column
            // is an override, not a replacement.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN chat_path TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 79)?;
        }
        if version < 80 {
            // Minute-resolution download history behind the Stats view's
            // Network/downloads graphs. One row per (minute-bucket, traffic
            // class) — `kind` is `iomon::NetKind::key`, `bytes` is the sum of
            // the class's tools' read-side transfer in that minute (the I/O
            // sampler buckets it; the scheduler drains it, see
            // `Store::record_net_history`).
            //
            // Same shape and reasoning as `poll_history` (v56): coarser views
            // are query-time GROUP BYs, not extra storage tiers, and there is
            // deliberately no monitor/recording FK — this is app-wide network
            // health that must outlive the rows it was generated by.
            conn.execute_batch(
                r#"
                CREATE TABLE net_history (
                    bucket_t  INTEGER NOT NULL,
                    kind      TEXT NOT NULL,
                    bytes     INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (bucket_t, kind)
                ) WITHOUT ROWID;
                "#,
            )?;
            conn.pragma_update(None, "user_version", 80)?;
        }
        if version < 81 {
            // 🎫 PO-token rejections were filed as warnings until 2026-07-31
            // (6d842b4): the rejection kills the take and the footage until
            // the next attempt is genuinely missing, which is this app's
            // definition of an error. New rows file as errors; this upgrades
            // the persisted history so the Warnings window doesn't show the
            // same failure in two colours depending on its date.
            conn.execute_batch(
                "UPDATE capture_alert SET severity = 'error' WHERE kind = 'po_token_rejected';",
            )?;
            conn.pragma_update(None, "user_version", 81)?;
        }
        if version < 82 {
            // The disposed file's size, captured at the moment of disposal
            // (there's no later point it could be read back from — a Trash
            // move relocates it, Recycle/Delete makes it gone). Backs the
            // Trash view's per-row size column and per-channel size total.
            // NULL (no DEFAULT) for every pre-existing row: their files were
            // already gone by the time this column exists, so their size is
            // unknowable — same reasoning as `disposal_backfill`-imported
            // rows, which insert NULL going forward too.
            conn.execute_batch("ALTER TABLE disposal_record ADD COLUMN bytes INTEGER;")?;
            conn.pragma_update(None, "user_version", 82)?;
        }
        if version < 83 {
            // Per-channel "hide from the 📣 Posts feed" flag. The channel is
            // still fetched/archived exactly as before — this only affects
            // whether `render_posts_feed` shows its posts, so muting a noisy
            // channel there never loses history.
            conn.execute_batch("ALTER TABLE channel ADD COLUMN posts_hidden INTEGER NOT NULL DEFAULT 0;")?;
            conn.pragma_update(None, "user_version", 83)?;
        }
        if version < 84 {
            // Per-event OCR attribution: which model produced/accepted the
            // scanned title, its self-reported confidence, and the on-disk
            // source image it was read from (so a later per-event "rescan" can
            // re-target the exact same image). Empty for non-OCR sources
            // (Twitch/YouTube API, Discord, manual) and for pre-existing rows.
            conn.execute_batch(
                "ALTER TABLE schedule_segment ADD COLUMN ocr_model TEXT NOT NULL DEFAULT '';
                 ALTER TABLE schedule_segment ADD COLUMN ocr_confidence TEXT NOT NULL DEFAULT '';
                 ALTER TABLE schedule_segment ADD COLUMN ocr_image_path TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 84)?;
        }
        if version < 85 {
            // Every YouTube `chat_path` written before 2026-08-04 predicted
            // yt-dlp *appending* `.live_chat.json` to the `-o` value (keeping
            // its `.mkv` extension). Verified live that yt-dlp's `--write-subs`
            // actually REPLACES the extension — the real file is
            // `{stem}.live_chat.json`, never `{stem}.mkv.live_chat.json` — so
            // every persisted path was wrong and chat replay / finalize
            // companion-tracking silently found nothing (`chat_path` is the
            // SOLE lookup candidate once set, see `chat::chat_file_candidates`).
            // Mechanically correct: this suffix is unambiguous — only ever
            // produced by the old (wrong) prediction code, never a real
            // filename a producer intentionally wrote.
            conn.execute_batch(
                "UPDATE recording SET chat_path =
                     substr(chat_path, 1, length(chat_path) - length('.mkv.live_chat.json'))
                     || '.live_chat.json'
                 WHERE chat_path LIKE '%.mkv.live_chat.json';",
            )?;
            conn.pragma_update(None, "user_version", 85)?;
        }
        if version < 86 {
            // Hype Train percent-to-next-level + countdown, so the chat
            // replay can draw an actual progress bar instead of a static
            // "level N · X pts" line — `hype_train` rows only, `0`
            // elsewhere/unknown (pre-existing rows never had this data).
            conn.execute_batch(
                "ALTER TABLE stream_event ADD COLUMN goal INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE stream_event ADD COLUMN expires_at INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE stream_event ADD COLUMN level INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 86)?;
        }
        if version < 87 {
            // Two indexes for queries the Streams grid runs on the UI thread,
            // both of which were full table scans and measured (on a real
            // 183 MB library) as the app's two worst main-thread DB stalls:
            //
            // * `latest_raid_outs_all` ("Follow raid" button state) —
            //   `WHERE kind = 'raid_out'` over 113k `stream_event` rows, twice
            //   (once for the row fetch, once for the `MAX(id) GROUP BY` list
            //   subquery): ~90 ms cold, 100 ms average *with the lock held*
            //   across 35k calls in one day. With `(kind, monitor_id, id)` the
            //   subquery becomes a covering index scan of just the 562
            //   `raid_out` rows: 87 ms → 0.3 ms.
            // * `ALERT_SUPERSEDED_SQL` (the take/stream alert badges) — its
            //   `EXISTS(... WHERE r2.monitor_id = ? AND r2.stream_id = ?)`
            //   re-scanned the whole `recording` table per alert row:
            //   193 ms → 3.7 ms with `(monitor_id, stream_id, status)`.
            //
            // Both are pure read-path indexes: no column/table changes, so an
            // older build opening this DB keeps working unchanged.
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_stream_event_kind
                     ON stream_event(kind, monitor_id, id);
                 CREATE INDEX IF NOT EXISTS idx_recording_stream
                     ON recording(monitor_id, stream_id, status);",
            )?;
            conn.pragma_update(None, "user_version", 87)?;
        }
        if version < 88 {
            // Rolling recordings (see `crate::rolling` and
            // `crate::models::Rolling`): a take captured while its instance was
            // in rolling mode carries the resolved TTL frozen onto it, and its
            // file is disposed of once that elapses unless the user kept it.
            //
            // All four default to 0, i.e. "not a rolling recording", so every
            // existing take is untouched and nothing starts expiring because of
            // this migration. Turning the feature on later only affects takes
            // captured after that — the TTL is stamped at capture start, never
            // re-resolved.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN rolling_ttl_secs INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE recording ADD COLUMN rolling_from INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE recording ADD COLUMN rolling_kept_at INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE recording ADD COLUMN rolling_expired_at INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX IF NOT EXISTS idx_recording_rolling
                     ON recording(rolling_ttl_secs)
                     WHERE rolling_ttl_secs > 0;",
            )?;
            conn.pragma_update(None, "user_version", 88)?;
        }
        if version < 89 {
            // YouTube chat moderation (see `crate::chat_scan`). Twitch's own
            // logger records deletions/timeouts/bans as `stream_event` rows the
            // moment they happen; YouTube chat arrives as a yt-dlp sidecar with
            // no live hook, so its moderation actions are harvested by a sweep
            // that reads the finished `.live_chat.json` once. This column is
            // that sweep's "already read this one" stamp (0 = never scanned),
            // which also makes a rescan a one-line UPDATE.
            //
            // The partial index is what the sweep's own predicate walks: only
            // unscanned takes with a sidecar, which drains to (almost) empty in
            // normal operation.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN chat_scanned_at INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX IF NOT EXISTS idx_recording_chat_unscanned
                     ON recording(chat_scanned_at)
                     WHERE chat_scanned_at = 0 AND chat_path <> '';",
            )?;
            conn.pragma_update(None, "user_version", 89)?;
        }
        if version < 90 {
            // Why a "seen live, not recorded" row exists (see
            // `insert_not_recorded_session`). Until now there was exactly one
            // reason — Auto-record was off — and the Streams grid hardcoded
            // that sentence. Simulcast dedup (`crate::simulcast`) adds a
            // second: another instance of the same channel is recording this
            // broadcast.
            //
            // It is not decoration. Two VOD-backfill paths turn a closed
            // not-recorded row into "we missed this broadcast, download it",
            // which for a deliberately-skipped simulcast would fetch the exact
            // duplicate the feature exists to prevent — they read this column
            // to tell the two cases apart.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN not_recorded_reason TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 90)?;
        }
        if version < 91 {
            // "The live broadcast we can see is members-only." Detection can
            // tell (the /streams tab badges it), but the capture tool cannot:
            // to an unauthenticated yt-dlp a members-only stream simply isn't
            // there, and it reports the channel as not live at all. Without
            // somewhere to keep what detection knew, a failed capture looked
            // like any other transient error and was retried on the short
            // backoff — forever, every few minutes, for the whole broadcast.
            conn.execute_batch(
                "ALTER TABLE monitor ADD COLUMN last_members_only INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 91)?;
        }
        if version < 92 {
            // "This take captured nothing because the broadcast wasn't ours to
            // capture." Per TAKE, deliberately: the 🔒 alert it files is keyed
            // by the *broadcast* so a gated stream produces one Warnings row
            // instead of one per doomed attempt, and that row can only carry a
            // single `recording_id` — the first take's. Every take after it
            // therefore looked uncovered, got a red `capture_failed` filed
            // beside the lock, and rendered "⛔ capture error" (Mori Calliope's
            // members-only stream, 2026-08-08: one 🔒 take and seven red ones,
            // rolling the whole stream row up as a fault).
            //
            // The partial index keeps the badge lookup off a full scan of a
            // table that only grows.
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN gated INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX IF NOT EXISTS idx_recording_gated
                     ON recording(id) WHERE gated = 1;",
            )?;
            repair_gated_takes(&conn)?;
            conn.pragma_update(None, "user_version", 92)?;
        }
        if version < 93 {
            repair_literal_channel_token(&conn)?;
            // The Issues sweep's two "most recent 500" scans both sorted the
            // whole recording table through a temp B-tree (`SCAN recording;
            // USE TEMP B-TREE FOR ORDER BY`) on every refresh, with the store
            // lock held. Same shape as migration 87's `raid_out` index.
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_recording_started
                     ON recording(started_at DESC);",
            )?;
            conn.pragma_update(None, "user_version", 93)?;
        }
        if version < 94 {
            // User-defined quality presets for the video downloader's Quality
            // dropdown (raw yt-dlp selectors saved under a name), the same
            // shape as `filename_preset` — a separate table so quality
            // selectors never show up in the filename-template dropdowns.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS quality_preset (
                     id INTEGER PRIMARY KEY,
                     name TEXT NOT NULL,
                     selector TEXT NOT NULL
                 );",
            )?;
            conn.pragma_update(None, "user_version", 94)?;
        }
        if version < 95 {
            // Clips: a catalogue first, media second.
            //
            // `vod_id` + `vod_offset_secs` are the *recovery keys* — with them a
            // vanished clip can be cut back out of the parent VOD (or out of our
            // own recording of it). They are PERISHABLE: measured against the
            // live Helix API on 2026-08-16, they are present on 100% of clips up
            // to 14 days old, 68% at 30 days, 19% at 90, and 5% at a year —
            // Twitch nulls them once the parent VOD expires. So the only chance
            // to capture them is to index a clip while its VOD still lives, and
            // `upsert_clip` must never blank a key it already holds.
            //
            // `vod_cdn` is the same idea one level down: `gql_vod_info` resolves
            // a VOD to its exact CDN host+folder in ONE request while the VOD is
            // alive. Cached here, a later recovery needs zero host probing —
            // which matters because the generic `find_live_playlist` fallback
            // costs |CDN_HOSTS| x (2*window+1) ~= 2,400 HEADs, fine once for a
            // VOD and utterly unacceptable per clip. Useful to the existing VOD
            // recovery feature too.
            //
            // `recording.clip_sweep_stage` drives the post-broadcast sweeps
            // (stage 0 -> +2h due, 1 -> +24h due, 2 -> done). Persisted rather
            // than an in-process timer so a restart inside the 24h window does
            // not lose the one sweep that can still capture the keys — same
            // reasoning as v89's `chat_scanned_at`.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS clip (
                     id INTEGER PRIMARY KEY,
                     platform TEXT NOT NULL,
                     slug TEXT NOT NULL,
                     channel_id INTEGER,
                     monitor_id INTEGER,
                     broadcaster_id TEXT NOT NULL DEFAULT '',
                     broadcaster_login TEXT NOT NULL DEFAULT '',
                     creator_login TEXT NOT NULL DEFAULT '',
                     title TEXT NOT NULL DEFAULT '',
                     game TEXT NOT NULL DEFAULT '',
                     language TEXT NOT NULL DEFAULT '',
                     view_count INTEGER NOT NULL DEFAULT 0,
                     duration_ms INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL DEFAULT 0,
                     url TEXT NOT NULL DEFAULT '',
                     thumbnail_url TEXT NOT NULL DEFAULT '',
                     vod_id TEXT NOT NULL DEFAULT '',
                     vod_offset_secs INTEGER,
                     keys_captured_at INTEGER NOT NULL DEFAULT 0,
                     recording_id INTEGER,
                     state TEXT NOT NULL DEFAULT 'indexed',
                     source TEXT NOT NULL DEFAULT 'helix',
                     recovery_method TEXT NOT NULL DEFAULT '',
                     offset_confidence TEXT NOT NULL DEFAULT '',
                     dl_video_id INTEGER,
                     dl_attempts INTEGER NOT NULL DEFAULT 0,
                     output_path TEXT NOT NULL DEFAULT '',
                     bytes INTEGER NOT NULL DEFAULT 0,
                     first_seen_at INTEGER NOT NULL DEFAULT 0,
                     last_seen_at INTEGER NOT NULL DEFAULT 0,
                     gone_at INTEGER NOT NULL DEFAULT 0,
                     err TEXT NOT NULL DEFAULT ''
                 );
                 CREATE UNIQUE INDEX IF NOT EXISTS idx_clip_key ON clip(platform, slug);
                 CREATE INDEX IF NOT EXISTS idx_clip_channel
                     ON clip(channel_id, created_at DESC);
                 CREATE INDEX IF NOT EXISTS idx_clip_vod
                     ON clip(platform, vod_id) WHERE vod_id <> '';
                 CREATE INDEX IF NOT EXISTS idx_clip_rec ON clip(recording_id);
                 CREATE INDEX IF NOT EXISTS idx_clip_pending
                     ON clip(state) WHERE state IN ('queued','downloading');

                 CREATE TABLE IF NOT EXISTS clip_sweep (
                     monitor_id INTEGER PRIMARY KEY,
                     last_swept_at INTEGER NOT NULL DEFAULT 0,
                     backfill_until INTEGER NOT NULL DEFAULT 0,
                     backfill_done INTEGER NOT NULL DEFAULT 0,
                     last_error TEXT NOT NULL DEFAULT ''
                 );

                 CREATE TABLE IF NOT EXISTS vod_cdn (
                     vod_id TEXT PRIMARY KEY,
                     host TEXT NOT NULL,
                     folder TEXT NOT NULL,
                     login TEXT NOT NULL,
                     broadcast_id TEXT NOT NULL,
                     start_epoch INTEGER NOT NULL,
                     learned_at INTEGER NOT NULL
                 );

                 ALTER TABLE recording ADD COLUMN clip_sweep_stage INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX IF NOT EXISTS idx_recording_clip_sweep
                     ON recording(ended_at) WHERE clip_sweep_stage < 2 AND ended_at IS NOT NULL;",
            )?;
            stamp_history_clip_swept(&conn)?;
            conn.pragma_update(None, "user_version", 95)?;
        }
        if version < 96 {
            // `link_clips_to_recordings` correlates `clip.vod_id` against
            // `recording.vod_id`, which had NO index — so every unlinked clip
            // drove a full scan of `recording`, twice (the scalar subquery and
            // a redundant EXISTS). Measured on the live DB the morning after
            // v95 shipped: 2,455 unlinked clips x 3,544 recordings x 2 ~= 17M
            // row reads, holding the WRITE lock for 13.5 s while the UI's own
            // row reload blocked behind it — the watchdog's "UI frozen" dialog.
            //
            // Worse, it never converged: those clips are of broadcasts we never
            // recorded, so the correlation finds nothing, `recording_id` stays
            // NULL, and the identical scan repeated after every sweep.
            //
            // Partial, because the overwhelming majority of takes have no VOD
            // id and indexing them would only bloat the tree.
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_recording_vod
                     ON recording(vod_id) WHERE vod_id <> '';",
            )?;
            conn.pragma_update(None, "user_version", 96)?;
        }
        if version < 97 {
            // `youtube_anonymous_public` became `youtube_anonymous_fallback`
            // when the ladder inverted (2026-08-19): anonymity went from "how
            // public YouTube is always captured" to "the last rung, after the
            // cookie path has failed repeatedly". The two settings mean
            // opposite things at the same value, so the old row is dropped
            // rather than carried over — leaving it would only mislead the
            // next person to read the table, and the new key's default (on,
            // meaning "a last-resort attempt is allowed") is what we want
            // either way.
            conn.execute_batch(
                "DELETE FROM app_settings WHERE key = 'youtube_anonymous_public';",
            )?;
            conn.pragma_update(None, "user_version", 97)?;
        }
        if version < 98 {
            // `bytes` records how big a take WAS; it says nothing about whether
            // the media is still there. Nothing clears it when a file is
            // deleted, trashed, swept as an expired rolling recording, or moved
            // outside the app, so every "space in use" total counted media that
            // no longer existed: measured on one archive, 178 takes claiming
            // 413 GB and 37 video downloads claiming 335 GB — 748 GB of
            // phantom disk usage in the very stats meant to find what is
            // filling a drive.
            //
            // Keeping `bytes` and stamping the absence separately preserves
            // both answers: how big it was, and whether it is still here. The
            // startup reconcile is already statting these files to correct
            // sizes, so the stamp costs no extra I/O — and it clears itself if
            // the file comes back (a drive remounted, an archive restored).
            conn.execute_batch(
                "ALTER TABLE recording ADD COLUMN media_missing_at INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE video ADD COLUMN media_missing_at INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.pragma_update(None, "user_version", 98)?;
        }
        if version < 99 {
            // The Streams tree's per-VOD clip rollup (`clip_counts_by_vod`)
            // groups 30k+ rows and reads `state` for every one; with the old
            // (platform, vod_id) index each row cost a table lookup — 414 ms
            // measured cold on 32.8k clips, held under the store's one
            // connection lock, i.e. ~25 dropped frames every grid rebuild.
            // Adding `state` makes the index COVERING: 10.8 ms on the same
            // data (38x). The old index is a strict prefix of the new one, so
            // everything it served is still served; keeping both would just
            // double the write cost per clip upsert.
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_clip_vod;
                 CREATE INDEX idx_clip_vod_state ON clip(platform, vod_id, state)
                     WHERE vod_id <> '';",
            )?;
            conn.pragma_update(None, "user_version", 99)?;
        }
        if version < 100 {
            // Which account's Twitch broadcaster colour paints the channel
            // NAME, independently of `preferred_platform` (the icon/banner
            // source). A container holding two personas of one streamer
            // (Nyana + Anya) kept the retired persona's colour because the
            // name colour silently followed the first Twitch instance; empty
            // keeps exactly that behaviour.
            conn.execute_batch(
                "ALTER TABLE channel ADD COLUMN color_source TEXT NOT NULL DEFAULT '';",
            )?;
            conn.pragma_update(None, "user_version", 100)?;
        }
        debug_assert_eq!(SCHEMA_VERSION, 100);
        Ok(())
    }
}

/// Stamp every already-finished take as "post-broadcast clip sweep done" (v95).
///
/// `clip_sweep_stage` defaults to 0 so a *new* take sweeps for clips at
/// `ended_at + 2h` and `+ 24h`. Leaving the existing archive at 0 would make
/// every finished take instantly due on the first launch after upgrading —
/// 3,425 of them on the development machine — thousands of Helix requests that
/// could not succeed anyway: those VODs expired long ago and Twitch has already
/// nulled the `video_id`/`vod_offset` keys the post-broadcast sweep exists to
/// capture. Cataloguing history is the daily sweep's and the backfill's job.
///
/// Takes still in progress (`ended_at IS NULL`) are deliberately left at 0 so
/// they sweep normally when they finish.
fn stamp_history_clip_swept(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE recording SET clip_sweep_stage = 2 WHERE ended_at IS NOT NULL",
        [],
    )?;
    Ok(n)
}

/// Replace a literal, never-expanded `{channel}` in stored paths with the
/// channel's actual name.
///
/// `{channel}` is a token in the *filename* template but was not one in the
/// **folder** template, where only `{name}` meant the channel. An unsupported
/// token is left literal, so anyone who typed the natural word got a directory
/// called `{channel}` — and, because the template is shared, every channel
/// using it landed in that one directory together (2026-08-09: seven channels,
/// one folder, 21 takes).
///
/// The token is now an alias, so new paths are correct. This repairs the ones
/// already written: the monitor's own `output_dir`, and the recording paths
/// derived from it. Rewriting the DB does **not** move the files — a take
/// whose file is still at the old path surfaces in the Issues panel as
/// missing, which is what that panel is for, and the Files view can relocate
/// it. Better that than leaving the paths permanently wrong.
///
/// Substring replacement rather than path surgery, deliberately: the token can
/// sit anywhere in the template (`G:\{channel}\vods`, `G:\a\{channel}-live`),
/// and every affected path was built from the same monitor's `output_dir`, so
/// the same substitution is correct wherever it appears.
fn repair_literal_channel_token(conn: &rusqlite::Connection) -> Result<()> {
    let mut st = conn.prepare(
        "SELECT m.id, ch.name FROM monitor m
           JOIN channel ch ON ch.id = m.channel_id
          WHERE m.output_dir LIKE '%{channel}%'",
    )?;
    let affected: Vec<(i64, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(st);
    for (monitor_id, name) in affected {
        // A channel name can contain characters a path can't; the expander
        // sanitizes each folder segment, so match it rather than inventing a
        // second rule here.
        let dir_name = crate::downloader::sanitize_filename(&name);
        if dir_name.is_empty() {
            continue; // nothing better to put there — leave it for the user
        }
        for (table, col) in
            [("monitor", "output_dir"), ("recording", "output_path"), ("recording", "chat_path")]
        {
            let key = if table == "monitor" { "id" } else { "monitor_id" };
            conn.execute(
                &format!(
                    "UPDATE {table} SET {col} = replace({col}, '{{channel}}', ?1)
                      WHERE {key} = ?2 AND {col} LIKE '%{{channel}}%'"
                ),
                rusqlite::params![dir_name, monitor_id],
            )?;
        }
        tracing::info!(
            monitor_id,
            channel = %dir_name,
            "migration 93: expanded a literal {{channel}} left in stored paths"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::*;

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
    /// The v92 repair, against the damage the old behaviour actually left:
    /// one broadcast, several 0-byte failed takes, a single 🔒 alert naming
    /// only the first, and a red "Capture failed" on each of the rest.
    #[test]
    fn migration_92_repairs_wrongly_reddened_gated_takes() {
        let store = Store::open_in_memory().unwrap();
        let cid = store
            .upsert_channel("Mori", "https://youtube.com/@mori", Platform::YouTube)
            .unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        let take = |at: i64| {
            let id = store
                .insert_recording(mid, at, "C:/tmp/a.mkv", None, false, Some("s1"), None, "", "")
                .unwrap();
            store
                .finish_recording(id, at + 10, 0, Some(1), "failed", "C:/tmp/a.mkv", "not live")
                .unwrap();
            id
        };
        let (t1, t2, t3) = (take(100), take(200), take(300));
        // A take of a DIFFERENT broadcast that failed for its own reasons must
        // not be swept up: it is the control for step 2's stream-id match.
        let other = store
            .insert_recording(mid, 400, "C:/tmp/b.mkv", None, false, Some("s2"), None, "", "")
            .unwrap();
        store
            .finish_recording(other, 410, 0, Some(1), "failed", "C:/tmp/b.mkv", "not live")
            .unwrap();
        // The pre-v92 world: one 🔒 row for the broadcast, pinned to take one.
        store
            .upsert_capture_alert(&NewCaptureAlert {
                kind: "sub_only".into(),
                severity: "warning".into(),
                source: "capture".into(),
                take_key: "sub_only:s1".into(),
                monitor_id: Some(mid),
                recording_id: Some(t1),
                channel: "Mori".into(),
                count: 1,
                ..Default::default()
            })
            .unwrap();
        // finish_recording already filed a capture_failed for every take.
        let reds = |store: &Store| {
            store
                .list_capture_alerts(50)
                .unwrap()
                .into_iter()
                .filter(|a| a.kind == "capture_failed")
                .filter_map(|a| a.recording_id)
                .collect::<Vec<_>>()
        };
        let mut before = reds(&store);
        before.sort();
        assert_eq!(before, vec![t1, t2, t3, other], "every take got a red row");

        // Re-running the repair on the migrated DB is what an upgrade does.
        repair_gated_takes(&store.db()).unwrap();

        for id in [t1, t2, t3] {
            assert!(store.get_recording(id).unwrap().unwrap().gated, "take {id} is gated");
        }
        assert!(
            !store.get_recording(other).unwrap().unwrap().gated,
            "an unrelated failed take must not be marked gated"
        );
        assert_eq!(reds(&store), vec![other], "only the false reds were dropped");
    }

    /// A literal `{channel}` in stored paths is expanded per monitor, not
    /// globally — two channels sharing the same broken template must come out
    /// in two different folders, which is the whole point.
    #[test]
    fn migration_95_stamps_finished_takes_done_but_leaves_live_ones_due() {
        // Guards the "an upgrade must not queue 3,425 doomed sweeps" property.
        let store = Store::open_in_memory().unwrap();
        let cid = store
            .upsert_channel("Layna", "https://twitch.tv/laynalazar", Platform::Twitch)
            .unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();

        let ended = store
            .insert_recording(mid, 100, "C:/tmp/a.mkv", Some(100), false, None, None, "", "")
            .unwrap();
        store
            .finish_recording(ended, 200, 1, Some(0), "completed", "C:/tmp/a.mkv", "")
            .unwrap();
        let live = store
            .insert_recording(mid, 300, "C:/tmp/b.mkv", Some(300), false, None, None, "", "")
            .unwrap();

        // Re-running the stamp is what the migration does on an existing DB.
        let conn = store.db();
        let n = stamp_history_clip_swept(&conn).unwrap();
        assert_eq!(n, 1, "only the finished take is stamped");
        let stage = |id: i64| -> i64 {
            conn.query_row(
                "SELECT clip_sweep_stage FROM recording WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(stage(ended), 2, "history is done, not due");
        assert_eq!(stage(live), 0, "an in-progress take still sweeps when it ends");
    }

    #[test]
    fn migration_93_expands_a_literal_channel_token_per_channel() {
        let store = Store::open_in_memory().unwrap();
        let c1 = store.upsert_channel("Blu", "https://twitch.tv/bluabk", Platform::Twitch).unwrap();
        let c2 = store
            .upsert_channel("Nyana Banyana", "https://twitch.tv/nyana", Platform::Twitch)
            .unwrap();
        let mut m1 = sample_monitor(c1);
        m1.output_dir = r"G:\streams\{channel}".into();
        let mid1 = store.insert_monitor(&m1).unwrap();
        let mut m2 = sample_monitor(c2);
        m2.output_dir = r"G:\streams\{channel}".into();
        let mid2 = store.insert_monitor(&m2).unwrap();
        // An unaffected monitor must be left completely alone.
        let mut m3 = sample_monitor(c1);
        m3.output_dir = r"G:\streams\Blu".into();
        let mid3 = store.insert_monitor(&m3).unwrap();

        let r1 = store
            .insert_recording(mid1, 100, r"G:\streams\{channel}\a.mkv", None, false, Some("s1"), None, "", "")
            .unwrap();
        let r2 = store
            .insert_recording(mid2, 100, r"G:\streams\{channel}\b.mkv", None, false, Some("s2"), None, "", "")
            .unwrap();
        store.set_recording_chat_path(r2, r"C:\chat\G\streams\{channel}\b.chat.jsonl").unwrap();
        let r3 = store
            .insert_recording(mid3, 100, r"G:\streams\Blu\c.mkv", None, false, Some("s3"), None, "", "")
            .unwrap();

        repair_literal_channel_token(&store.db()).unwrap();

        let dir = |id| store.get_monitor_output_dir(id).unwrap().unwrap_or_default().0;
        assert_eq!(dir(mid1), r"G:\streams\Blu");
        assert_eq!(dir(mid2), r"G:\streams\Nyana Banyana", "each channel gets its OWN folder");
        assert_eq!(dir(mid3), r"G:\streams\Blu", "an unaffected monitor is untouched");

        let rec = |id| store.get_recording(id).unwrap().unwrap();
        assert_eq!(rec(r1).output_path, r"G:\streams\Blu\a.mkv");
        assert_eq!(rec(r2).output_path, r"G:\streams\Nyana Banyana\b.mkv");
        // Companion paths derived from the same folder are repaired too.
        assert_eq!(rec(r2).chat_path, r"C:\chat\G\streams\Nyana Banyana\b.chat.jsonl");
        assert_eq!(rec(r3).output_path, r"G:\streams\Blu\c.mkv");

        // Idempotent: an upgrade that re-runs it changes nothing further.
        repair_literal_channel_token(&store.db()).unwrap();
        assert_eq!(dir(mid2), r"G:\streams\Nyana Banyana");
        assert_eq!(rec(r2).output_path, r"G:\streams\Nyana Banyana\b.mkv");
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
}
