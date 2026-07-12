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
        debug_assert_eq!(SCHEMA_VERSION, 54);
        Ok(())
    }
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
