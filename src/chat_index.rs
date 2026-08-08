//! The chat index: who was in which stream, and what they said.
//!
//! We already capture every chat message — 2.68 GB of sidecars at the time this
//! shipped — but nothing could be asked of it *by person*. Answering "which
//! streams was this chatter in" meant reading the whole corpus. This module is
//! the index that makes the question cheap, and [`crate::ui::users`] is the view
//! that asks it.
//!
//! # Why a second database file
//!
//! This lives in `chat_index.sqlite3`, beside the operational store but not
//! inside it, for three reasons that all point the same way:
//!
//! * **Backups.** [`crate::db_backup`] takes rolling `VACUUM INTO` copies of the
//!   whole database and keeps several. Folding ~800 MB of index into the main
//!   file would make every backup a multi-second vacuum of a gigabyte, and the
//!   rotation ~10 GB — for data that is 100% rebuildable from the sidecars and
//!   therefore not worth backing up at all.
//! * **Locking.** A separate connection means a separate lock. A full-text write
//!   that takes hundreds of milliseconds can never block a UI query on the
//!   store. The index is allowed to be slow; the app is not.
//! * **Disposability.** "Delete and rebuild" is `remove_file`.
//!
//! The cost is no cross-file joins: this file answers *which recordings* and
//! *which messages*, and the caller resolves those ids against [`crate::store`].
//! Two cheap queries beat one lock shared between a hot path and a cold one.
//!
//! # Identity
//!
//! Chatters are keyed on the platform's stable id wherever we have one — Twitch
//! `user-id`, YouTube `authorExternalChannelId` — because logins and display
//! names are both freely renameable. See [`UserKey`] for the one era where we
//! don't have one, and what that costs.
//!
//! # Who writes it
//!
//! Only the sweep in [`crate::chat_scan`], reading finished sidecars. Nothing is
//! written during live capture: the recording path is the last place that should
//! be paying for an index, and YouTube chat (captured by yt-dlp) could only ever
//! be read back after the fact anyway. One producer also means there is no
//! live-vs-scan double-counting problem to get wrong.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::FairMutex;
use rusqlite::{Connection, OptionalExtension, params};
use tracing::{debug, info, warn};

use crate::store::db_lock;

/// Latest index-schema version understood by this build. Independent of the
/// store's `SCHEMA_VERSION` — the two files migrate separately.
const INDEX_SCHEMA_VERSION: i64 = 2;

/// How long [`ChatIndex::health`] reuses its last answer.
///
/// Every caller is a UI panel that repaints continuously, so without this the
/// Users tab would run the aggregates every frame. The counters only move when
/// a sweep lands (once a minute at best, minutes apart while a big backlog is
/// draining), so a window this wide still reads as live — and it matters,
/// because the one remaining unavoidable cost is `COUNT(*)` over `chat_user`
/// (57 ms at a quarter-million chatters, measured) and that lands on the UI
/// thread. Once every 15 s is invisible; once every 5 s is a rhythm you can
/// feel.
const HEALTH_TTL: std::time::Duration = std::time::Duration::from_secs(15);

/// A query slower than this is logged at `warn!` with its shape and row count.
/// A degrading index should show up in the log before it shows up as a stalled
/// frame (the lesson of the unindexed `raid_out` query that froze the UI).
const SLOW_QUERY_WARN_MS: u128 = 200;
/// …and slower than this at `debug!`, so the trend is visible before the cliff.
const SLOW_QUERY_DEBUG_MS: u128 = 50;

/// How stable a chatter's identity is.
///
/// Twitch only started putting `user-id` in our sidecars on 2026-08-05; roughly
/// two thirds of the Twitch archive predates it and carries nothing but a login.
/// Those entries are keyed by login and marked [`UserKey::name_matched`], which
/// the UI states plainly rather than pretending to an identity it doesn't have:
/// a login points at whoever holds that name *today*, so an account that has
/// since renamed will be attributed wrongly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserKey {
    /// `"twitch"` / `"youtube"`.
    pub platform: String,
    /// The platform's stable id, or `login:<lowercased login>` when the log
    /// predates id capture.
    pub key: String,
}

impl UserKey {
    /// Key a chatter by their platform id when the log carried one, else by
    /// login. `id` empty and `login` empty yields `None` — a message we can't
    /// attribute to anyone is not worth a row.
    pub fn new(platform: &str, id: &str, login: &str) -> Option<UserKey> {
        let id = id.trim();
        let login = login.trim();
        let key = if !id.is_empty() {
            id.to_string()
        } else if !login.is_empty() {
            format!("login:{}", login.to_lowercase())
        } else {
            return None;
        };
        Some(UserKey { platform: platform.to_string(), key })
    }

}

/// Does this `user_key` name a chatter rather than identify one?
///
/// One definition rather than a `starts_with("login:")` at each use: the prefix
/// is the whole distinction between "this is definitely them" and "this is
/// whoever holds that name", and it must never drift between the writer and
/// the reader.
pub fn key_is_name_matched(key: &str) -> bool {
    key.starts_with("login:")
}

/// One chat message, as the index stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedMessage {
    pub key: UserKey,
    /// Login (Twitch) or empty (YouTube, which has no login concept).
    pub login: String,
    /// Display name as seen on this message.
    pub display: String,
    /// Unix seconds.
    pub at: i64,
    pub text: String,
}

/// One chatter's presence in one recording — the rollup that answers "which
/// streams was this person in" without touching the message table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    pub first_at: i64,
    pub last_at: i64,
    pub msgs: i64,
}

/// Which take a parsed sidecar belongs to — the ids [`ChatIndex::write_take`]
/// files its rows under. Grouped rather than passed loose because they always
/// travel together and are trivially swappable at a call site.
#[derive(Clone, Debug)]
pub struct TakeRef<'a> {
    pub rec_id: i64,
    pub monitor_id: i64,
    pub channel_id: i64,
    pub chat_path: &'a str,
}

/// Everything one sidecar yielded, ready to write in a single transaction.
#[derive(Debug, Default)]
pub struct ParsedSidecar {
    pub messages: Vec<IndexedMessage>,
    /// Bytes read, for the throughput line in the log.
    pub bytes: u64,
}

/// A chatter as the Users view lists them.
#[derive(Clone, Debug)]
pub struct UserRow {
    pub id: i64,
    pub platform: String,
    pub key: String,
    pub login: String,
    pub display: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub msgs_total: i64,
    pub streams_total: i64,
    /// True when this identity is keyed by name rather than a platform id.
    pub name_matched: bool,
}

/// One stream a chatter appeared in, as the Users view lists them.
#[derive(Clone, Debug)]
pub struct UserStreamRow {
    pub rec_id: i64,
    pub channel_id: i64,
    pub first_at: i64,
    pub last_at: i64,
    pub msgs: i64,
}

/// One message hit from a search.
#[derive(Clone, Debug)]
pub struct MessageHit {
    pub rec_id: i64,
    pub at: i64,
    pub text: String,
    /// Set on global searches, where the sender varies per row.
    pub user_id: i64,
    pub display: String,
}

/// Aggregate health of the index, for the App Stats panel.
#[derive(Clone, Debug, Default)]
pub struct IndexHealth {
    pub users: i64,
    pub messages: i64,
    pub takes_indexed: i64,
    pub takes_failed: i64,
    /// Size of the index file (+ its WAL) on disk, bytes.
    pub bytes_on_disk: u64,
    /// One row per (chatter, stream) — the cheap layer's size.
    pub presence_rows: i64,
    /// Slowest single take on record, and which.
    pub slowest_ms: i64,
    pub slowest_rec_id: i64,
    /// Identities still keyed by login that a Helix lookup might resolve.
    pub unresolved_logins: i64,
}

/// How a take's indexing attempt ended — the `indexed_take.status` value.
pub mod status {
    /// Read and indexed.
    pub const OK: &str = "ok";
    /// The sidecar is gone. Stamped anyway, so the queue drains.
    pub const MISSING: &str = "missing";
    /// Present but unreadable/unparseable.
    pub const FAILED: &str = "failed";
}

/// Path to the index database. Sits beside the store so both live on local
/// disk (WAL requires it), and inherits the `STREAMARCHIVER_DB` override's
/// directory so a portable/test install keeps them together.
pub fn index_path() -> PathBuf {
    let db = crate::app_paths::db_path();
    match db.parent() {
        Some(dir) => dir.join("chat_index.sqlite3"),
        None => PathBuf::from("chat_index.sqlite3"),
    }
}

/// The chat index database.
pub struct ChatIndex {
    conn: FairMutex<Connection>,
    path: PathBuf,
    /// Last [`health`](Self::health) answer and when it was taken — see
    /// [`HEALTH_TTL`]. Its own lock, so reading it never touches the
    /// connection.
    health_cache: parking_lot::Mutex<Option<(IndexHealth, std::time::Instant)>>,
}

impl ChatIndex {
    /// Open (or create) the index at `path`.
    pub fn open(path: &Path) -> Result<ChatIndex> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening chat index at {}", path.display()))?;
        Self::configure(&conn)?;
        let idx = ChatIndex {
            conn: FairMutex::new(conn),
            path: path.to_path_buf(),
            health_cache: parking_lot::Mutex::new(None),
        };
        idx.migrate()?;
        Ok(idx)
    }

    /// In-memory index, used by tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<ChatIndex> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        let idx = ChatIndex {
            conn: FairMutex::new(conn),
            path: PathBuf::from(":memory:"),
            health_cache: parking_lot::Mutex::new(None),
        };
        idx.migrate()?;
        Ok(idx)
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    }

    /// Acquire the index connection. Reports into [`db_lock::CHAT_INDEX`], a
    /// lane of its own — index contention must never be read as store
    /// contention, since keeping the two apart is the reason for the second
    /// file.
    #[track_caller]
    fn db(&self) -> db_lock::Guard<'_, Connection> {
        db_lock::acquire(&self.conn, &db_lock::CHAT_INDEX)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.db();
        let version: i64 =
            conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if version < 1 {
            conn.execute_batch(
                r#"
                -- One row per chat identity. `user_key` is the platform's
                -- stable id where we have one, else `login:<name>` — see
                -- `UserKey`. `merged_into` points a superseded login-keyed row
                -- at the id-keyed row a Helix lookup resolved it to; such rows
                -- keep their key so searching the old name still finds them.
                CREATE TABLE chat_user (
                    id            INTEGER PRIMARY KEY,
                    platform      TEXT NOT NULL,
                    user_key      TEXT NOT NULL,
                    login         TEXT NOT NULL DEFAULT '',
                    display       TEXT NOT NULL DEFAULT '',
                    first_seen    INTEGER NOT NULL DEFAULT 0,
                    last_seen     INTEGER NOT NULL DEFAULT 0,
                    msgs_total    INTEGER NOT NULL DEFAULT 0,
                    streams_total INTEGER NOT NULL DEFAULT 0,
                    -- 0 while a login-keyed row has never been looked up, 1 once
                    -- Helix has answered (either way) so we stop asking.
                    resolved      INTEGER NOT NULL DEFAULT 0,
                    merged_into   INTEGER NOT NULL DEFAULT 0,
                    UNIQUE(platform, user_key)
                );
                CREATE INDEX idx_chat_user_login ON chat_user(login);
                CREATE INDEX idx_chat_user_display ON chat_user(display);

                -- The cheap layer: who was in which recording. `monitor_id` and
                -- `channel_id` are denormalised so "which channels has this
                -- person been in" never needs the other database file.
                CREATE TABLE chat_presence (
                    user_ref   INTEGER NOT NULL,
                    rec_id     INTEGER NOT NULL,
                    monitor_id INTEGER NOT NULL DEFAULT 0,
                    channel_id INTEGER NOT NULL DEFAULT 0,
                    first_at   INTEGER NOT NULL DEFAULT 0,
                    last_at    INTEGER NOT NULL DEFAULT 0,
                    msgs       INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (user_ref, rec_id)
                ) WITHOUT ROWID;
                CREATE INDEX idx_presence_rec ON chat_presence(rec_id);
                CREATE INDEX idx_presence_channel ON chat_presence(channel_id, user_ref);

                -- The heavy layer: every message. Separable from the presence
                -- layer by design — it can be dropped without breaking "which
                -- streams was this person in".
                CREATE TABLE chat_message (
                    id       INTEGER PRIMARY KEY,
                    user_ref INTEGER NOT NULL,
                    rec_id   INTEGER NOT NULL,
                    at       INTEGER NOT NULL,
                    text     TEXT NOT NULL
                );
                CREATE INDEX idx_message_user ON chat_message(user_ref, at);
                CREATE INDEX idx_message_rec ON chat_message(rec_id, at);

                -- External-content FTS: the text lives once, in chat_message.
                CREATE VIRTUAL TABLE chat_message_fts USING fts5(
                    text, content='chat_message', content_rowid='id'
                );

                -- The work queue's stamp. Deliberately NOT
                -- `recording.chat_scanned_at`: that column is already burned for
                -- every take the moderation sweep has seen, and it stamps Twitch
                -- takes without reading the file at all — indexing has to
                -- actually read them.
                CREATE TABLE indexed_take (
                    rec_id       INTEGER PRIMARY KEY,
                    indexed_at   INTEGER NOT NULL,
                    chat_path    TEXT NOT NULL DEFAULT '',
                    source_bytes INTEGER NOT NULL DEFAULT 0,
                    msgs         INTEGER NOT NULL DEFAULT 0,
                    users        INTEGER NOT NULL DEFAULT 0,
                    parse_ms     INTEGER NOT NULL DEFAULT 0,
                    insert_ms    INTEGER NOT NULL DEFAULT 0,
                    fts_ms       INTEGER NOT NULL DEFAULT 0,
                    status       TEXT NOT NULL DEFAULT 'ok'
                );
                "#,
            )?;
            info!(version = 1, "chat index: schema created");
        }
        if version < 2 {
            // `resolved` used to be set only by the login resolver, so it was 0
            // for every id-keyed identity too — which made "still to look up"
            // an unindexable `user_key LIKE 'login:%'` scan costing a full
            // second on a real index, on the UI thread. Give it its honest
            // meaning ("we know this identity's account id"), true by
            // construction for anything keyed by a real id, and index it.
            conn.execute_batch(
                r#"
                UPDATE chat_user SET resolved = 1
                 WHERE resolved = 0 AND user_key NOT LIKE 'login:%';
                CREATE INDEX IF NOT EXISTS idx_chat_user_unresolved
                    ON chat_user(msgs_total DESC)
                    WHERE resolved = 0 AND merged_into = 0;
                "#,
            )?;
            info!(version = 2, "chat index: indexed the unresolved-login lookup");
        }
        conn.pragma_update(None, "user_version", INDEX_SCHEMA_VERSION)?;
        Ok(())
    }

    // ----- writes -----

    /// Write one parsed sidecar: identities, presence rollup, messages, FTS
    /// rows, and the queue stamp — in one transaction, so a take is either
    /// wholly indexed or not indexed at all. Re-indexing a take replaces its
    /// previous rows rather than doubling them.
    ///
    /// Returns `(messages, distinct chatters)` written.
    pub fn write_take(
        &self,
        take: &TakeRef<'_>,
        parsed: &ParsedSidecar,
        parse_ms: u128,
        now: i64,
    ) -> Result<(i64, i64)> {
        let TakeRef { rec_id, monitor_id, channel_id, chat_path } = *take;
        let t_insert = std::time::Instant::now();
        let mut conn = self.db();
        let tx = conn.transaction()?;

        // Re-index is a replace, not an append. FTS rows are external-content,
        // so they must be deleted through the FTS table (which needs the old
        // text) *before* the base rows go.
        tx.execute(
            "INSERT INTO chat_message_fts(chat_message_fts, rowid, text)
             SELECT 'delete', id, text FROM chat_message WHERE rec_id = ?1",
            params![rec_id],
        )?;
        tx.execute("DELETE FROM chat_message WHERE rec_id = ?1", params![rec_id])?;
        tx.execute("DELETE FROM chat_presence WHERE rec_id = ?1", params![rec_id])?;

        let users;
        let mut msgs = 0i64;
        {
            // Identity upsert. `display`/`login` track the most recent sighting;
            // first/last seen widen monotonically so an out-of-order take can
            // never narrow them.
            let mut up_user = tx.prepare(
                // `resolved` is "we know this identity's account id" — true by
                // construction unless the key is a bare login.
                "INSERT INTO chat_user(platform, user_key, login, display, first_seen, last_seen, resolved)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?2 NOT LIKE 'login:%')
                 ON CONFLICT(platform, user_key) DO UPDATE SET
                     login      = CASE WHEN excluded.login != '' THEN excluded.login ELSE login END,
                     display    = CASE WHEN excluded.display != '' THEN excluded.display ELSE display END,
                     first_seen = CASE WHEN first_seen = 0 OR excluded.first_seen < first_seen
                                       THEN excluded.first_seen ELSE first_seen END,
                     last_seen  = MAX(last_seen, excluded.last_seen)",
            )?;
            let mut find_user = tx.prepare(
                "SELECT id FROM chat_user WHERE platform = ?1 AND user_key = ?2",
            )?;
            let mut ins_msg = tx.prepare(
                "INSERT INTO chat_message(user_ref, rec_id, at, text) VALUES(?1, ?2, ?3, ?4)",
            )?;
            let mut ins_pres = tx.prepare(
                "INSERT INTO chat_presence(user_ref, rec_id, monitor_id, channel_id,
                                           first_at, last_at, msgs)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;

            // user_ref -> presence rollup, accumulated as we walk the messages.
            let mut pres: std::collections::HashMap<i64, Presence> =
                std::collections::HashMap::new();
            // Resolving a key to a row id costs two statements; a chatter sends
            // many messages per stream, so cache it for the file.
            let mut ids: std::collections::HashMap<(String, String), i64> =
                std::collections::HashMap::new();

            for m in &parsed.messages {
                let cache_key = (m.key.platform.clone(), m.key.key.clone());
                let user_ref = match ids.get(&cache_key) {
                    Some(id) => *id,
                    None => {
                        up_user.execute(params![
                            m.key.platform,
                            m.key.key,
                            m.login,
                            m.display,
                            m.at,
                            m.at
                        ])?;
                        let id: i64 = find_user
                            .query_row(params![m.key.platform, m.key.key], |r| r.get(0))?;
                        ids.insert(cache_key, id);
                        id
                    }
                };
                ins_msg.execute(params![user_ref, rec_id, m.at, m.text])?;
                msgs += 1;
                pres.entry(user_ref)
                    .and_modify(|p| {
                        p.first_at = p.first_at.min(m.at);
                        p.last_at = p.last_at.max(m.at);
                        p.msgs += 1;
                    })
                    .or_insert(Presence { first_at: m.at, last_at: m.at, msgs: 1 });
            }
            users = pres.len() as i64;
            for (user_ref, p) in &pres {
                ins_pres.execute(params![
                    user_ref,
                    rec_id,
                    monitor_id,
                    channel_id,
                    p.first_at,
                    p.last_at,
                    p.msgs
                ])?;
            }
        }
        let insert_ms = t_insert.elapsed().as_millis();

        // FTS last: external-content tables want the base rows to exist.
        let t_fts = std::time::Instant::now();
        tx.execute(
            "INSERT INTO chat_message_fts(rowid, text)
             SELECT id, text FROM chat_message WHERE rec_id = ?1",
            params![rec_id],
        )?;
        let fts_ms = t_fts.elapsed().as_millis();

        // Roll the per-user totals forward from the presence table rather than
        // incrementing them — a re-index must correct the totals, not double
        // them, and this is the only place that knows both old and new.
        tx.execute(
            "UPDATE chat_user SET
                 msgs_total    = COALESCE((SELECT SUM(msgs) FROM chat_presence
                                           WHERE user_ref = chat_user.id), 0),
                 streams_total = COALESCE((SELECT COUNT(*) FROM chat_presence
                                           WHERE user_ref = chat_user.id), 0)
             WHERE id IN (SELECT user_ref FROM chat_presence WHERE rec_id = ?1)",
            params![rec_id],
        )?;

        tx.execute(
            "INSERT INTO indexed_take(rec_id, indexed_at, chat_path, source_bytes, msgs, users,
                                      parse_ms, insert_ms, fts_ms, status)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(rec_id) DO UPDATE SET
                 indexed_at = excluded.indexed_at, chat_path = excluded.chat_path,
                 source_bytes = excluded.source_bytes, msgs = excluded.msgs,
                 users = excluded.users, parse_ms = excluded.parse_ms,
                 insert_ms = excluded.insert_ms, fts_ms = excluded.fts_ms,
                 status = excluded.status",
            params![
                rec_id,
                now,
                chat_path,
                parsed.bytes as i64,
                msgs,
                users,
                parse_ms as i64,
                insert_ms as i64,
                fts_ms as i64,
                status::OK
            ],
        )?;
        tx.commit()?;
        Ok((msgs, users))
    }

    /// Stamp a take we could not index, so the queue drains. A sidecar that
    /// will never come back must not be retried forever.
    pub fn stamp_take(&self, rec_id: i64, chat_path: &str, status: &str, now: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO indexed_take(rec_id, indexed_at, chat_path, status)
             VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(rec_id) DO UPDATE SET
                 indexed_at = excluded.indexed_at, chat_path = excluded.chat_path,
                 status = excluded.status",
            params![rec_id, now, chat_path, status],
        )?;
        Ok(())
    }

    /// Recording ids already stamped, for the queue query on the store side
    /// (which lives in the other database and so can't join against this one).
    pub fn indexed_rec_ids(&self) -> Result<std::collections::HashSet<i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare("SELECT rec_id FROM indexed_take")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        Ok(ids)
    }

    /// Drop a take's rows entirely — used when its recording is deleted.
    pub fn forget_take(&self, rec_id: i64) -> Result<()> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO chat_message_fts(chat_message_fts, rowid, text)
             SELECT 'delete', id, text FROM chat_message WHERE rec_id = ?1",
            params![rec_id],
        )?;
        tx.execute("DELETE FROM chat_message WHERE rec_id = ?1", params![rec_id])?;
        tx.execute("DELETE FROM chat_presence WHERE rec_id = ?1", params![rec_id])?;
        tx.execute("DELETE FROM indexed_take WHERE rec_id = ?1", params![rec_id])?;
        tx.commit()?;
        Ok(())
    }

    // ----- identity resolution -----

    /// Login-keyed Twitch identities that have never been looked up, oldest
    /// first. These are the pre-2026-08-05 logs, where the sidecar carried no
    /// `user-id` at all.
    pub fn unresolved_logins(&self, limit: i64) -> Result<Vec<(i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, login FROM chat_user
             WHERE resolved = 0 AND merged_into = 0 AND platform = 'twitch' AND login != ''
             ORDER BY msgs_total DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record that Helix answered for a login-keyed identity.
    ///
    /// With an id, the login-keyed row is merged into the id-keyed one: its
    /// presence and message rows are repointed, its totals folded in, and
    /// `merged_into` set so the old name still resolves to the merged identity.
    /// Without one (`None` — deleted or renamed away), the row is only marked
    /// resolved, so we stop asking.
    ///
    /// Returns true when a merge happened.
    pub fn resolve_login(&self, user_id: i64, twitch_id: Option<&str>) -> Result<bool> {
        let mut conn = self.db();
        let tx = conn.transaction()?;
        let Some(twitch_id) = twitch_id.map(str::trim).filter(|s| !s.is_empty()) else {
            tx.execute("UPDATE chat_user SET resolved = 1 WHERE id = ?1", params![user_id])?;
            tx.commit()?;
            return Ok(false);
        };
        // The id-keyed row may not exist yet (this person only ever appears in
        // old logs) — in that case adopting the id in place is the whole merge.
        let target: Option<i64> = tx
            .query_row(
                "SELECT id FROM chat_user WHERE platform = 'twitch' AND user_key = ?1",
                params![twitch_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(target) = target else {
            tx.execute(
                "UPDATE chat_user SET user_key = ?2, resolved = 1 WHERE id = ?1",
                params![user_id, twitch_id],
            )?;
            tx.commit()?;
            return Ok(true);
        };
        if target == user_id {
            tx.execute("UPDATE chat_user SET resolved = 1 WHERE id = ?1", params![user_id])?;
            tx.commit()?;
            return Ok(false);
        }
        // Repoint the heavy rows. Presence is keyed (user_ref, rec_id), so a
        // stream both identities appear in would collide — sum those instead of
        // failing the merge.
        tx.execute(
            "INSERT INTO chat_presence(user_ref, rec_id, monitor_id, channel_id,
                                       first_at, last_at, msgs)
             SELECT ?2, rec_id, monitor_id, channel_id, first_at, last_at, msgs
             FROM chat_presence WHERE user_ref = ?1
             ON CONFLICT(user_ref, rec_id) DO UPDATE SET
                 first_at = MIN(first_at, excluded.first_at),
                 last_at  = MAX(last_at,  excluded.last_at),
                 msgs     = msgs + excluded.msgs",
            params![user_id, target],
        )?;
        tx.execute("DELETE FROM chat_presence WHERE user_ref = ?1", params![user_id])?;
        tx.execute(
            "UPDATE chat_message SET user_ref = ?2 WHERE user_ref = ?1",
            params![user_id, target],
        )?;
        tx.execute(
            "UPDATE chat_user SET
                 first_seen = CASE WHEN first_seen = 0 THEN (SELECT first_seen FROM chat_user WHERE id = ?1)
                                   ELSE MIN(first_seen, COALESCE((SELECT NULLIF(first_seen, 0) FROM chat_user WHERE id = ?1), first_seen)) END,
                 last_seen  = MAX(last_seen, COALESCE((SELECT last_seen FROM chat_user WHERE id = ?1), 0)),
                 msgs_total    = COALESCE((SELECT SUM(msgs) FROM chat_presence WHERE user_ref = ?2), 0),
                 streams_total = COALESCE((SELECT COUNT(*) FROM chat_presence WHERE user_ref = ?2), 0)
             WHERE id = ?2",
            params![user_id, target],
        )?;
        tx.execute(
            "UPDATE chat_user SET resolved = 1, merged_into = ?2, msgs_total = 0, streams_total = 0
             WHERE id = ?1",
            params![user_id, target],
        )?;
        tx.commit()?;
        Ok(true)
    }

    // ----- reads -----

    /// Log a query that took long enough to be worth knowing about. Every read
    /// below funnels through this: an index that is degrading should announce
    /// itself in the log, not by stalling a frame.
    fn timed<T>(shape: &'static str, rows: usize, t: std::time::Instant, out: T) -> T {
        let ms = t.elapsed().as_millis();
        if ms >= SLOW_QUERY_WARN_MS {
            warn!(ms, rows, "chat index: slow query [{shape}]");
        } else if ms >= SLOW_QUERY_DEBUG_MS {
            debug!(ms, rows, "chat index: query [{shape}]");
        }
        out
    }

    /// Search identities by display name, login, or exact platform id.
    ///
    /// Merged-away rows are folded to their target so searching an old name
    /// lands on the person, not on an empty shell.
    pub fn find_users(&self, query: &str, limit: i64) -> Result<Vec<UserRow>> {
        let t = std::time::Instant::now();
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let like = format!("%{}%", q.to_lowercase());
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT u.id, u.platform, u.user_key, u.login, u.display, u.first_seen, u.last_seen,
                    u.msgs_total, u.streams_total
             FROM chat_user u
             WHERE u.id IN (
                 SELECT CASE WHEN merged_into != 0 THEN merged_into ELSE id END
                 FROM chat_user
                 WHERE lower(display) LIKE ?1 OR lower(login) LIKE ?1 OR user_key = ?2
             )
             ORDER BY u.msgs_total DESC, u.display
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![like, q, limit], map_user_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Self::timed("find_users", rows.len(), t, rows))
    }

    /// One identity by row id.
    pub fn user(&self, id: i64) -> Result<Option<UserRow>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT id, platform, user_key, login, display, first_seen, last_seen,
                        msgs_total, streams_total
                 FROM chat_user WHERE id = ?1",
                params![id],
                map_user_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Every alias we have ever recorded for one identity's platform id —
    /// including the login-keyed rows merged into it, which is how a rename
    /// becomes visible instead of silently rewriting history.
    pub fn aliases(&self, user_id: i64) -> Result<Vec<String>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT display FROM chat_user
             WHERE (id = ?1 OR merged_into = ?1) AND display != ''",
        )?;
        let rows = stmt
            .query_map(params![user_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// How many of an identity's streams came in through a name match rather
    /// than a platform id — the caveat the Users view states out loud.
    pub fn name_matched_streams(&self, user_id: i64) -> Result<i64> {
        let conn = self.db();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM chat_presence p
             WHERE p.user_ref = ?1
               AND EXISTS (SELECT 1 FROM chat_user u
                           WHERE u.merged_into = ?1 AND u.user_key LIKE 'login:%')",
            params![user_id],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(n)
    }

    /// Streams one identity appeared in, newest first.
    pub fn user_streams(&self, user_id: i64, limit: i64) -> Result<Vec<UserStreamRow>> {
        let t = std::time::Instant::now();
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT rec_id, channel_id, first_at, last_at, msgs
             FROM chat_presence WHERE user_ref = ?1
             ORDER BY first_at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![user_id, limit], |r| {
                Ok(UserStreamRow {
                    rec_id: r.get(0)?,
                    channel_id: r.get(1)?,
                    first_at: r.get(2)?,
                    last_at: r.get(3)?,
                    msgs: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Self::timed("user_streams", rows.len(), t, rows))
    }

    /// One identity's messages, newest first, optionally full-text filtered.
    pub fn user_messages(
        &self,
        user_id: i64,
        query: &str,
        limit: i64,
    ) -> Result<Vec<MessageHit>> {
        let t = std::time::Instant::now();
        let conn = self.db();
        let q = query.trim();
        let rows = if q.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT rec_id, at, text FROM chat_message
                 WHERE user_ref = ?1 ORDER BY at DESC LIMIT ?2",
            )?;
            stmt.query_map(params![user_id, limit], |r| {
                Ok(MessageHit {
                    rec_id: r.get(0)?,
                    at: r.get(1)?,
                    text: r.get(2)?,
                    user_id,
                    display: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT m.rec_id, m.at, m.text FROM chat_message_fts f
                 JOIN chat_message m ON m.id = f.rowid
                 WHERE f.chat_message_fts MATCH ?1 AND m.user_ref = ?2
                 ORDER BY m.at DESC LIMIT ?3",
            )?;
            stmt.query_map(params![fts_query(q), user_id, limit], |r| {
                Ok(MessageHit {
                    rec_id: r.get(0)?,
                    at: r.get(1)?,
                    text: r.get(2)?,
                    user_id,
                    display: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(Self::timed("user_messages", rows.len(), t, rows))
    }

    /// Full-text search across every indexed message.
    pub fn search_messages(&self, query: &str, limit: i64) -> Result<Vec<MessageHit>> {
        let t = std::time::Instant::now();
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT m.rec_id, m.at, m.text, m.user_ref, COALESCE(u.display, '')
             FROM chat_message_fts f
             JOIN chat_message m ON m.id = f.rowid
             LEFT JOIN chat_user u ON u.id = m.user_ref
             WHERE f.chat_message_fts MATCH ?1
             ORDER BY m.at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![fts_query(q), limit], |r| {
                Ok(MessageHit {
                    rec_id: r.get(0)?,
                    at: r.get(1)?,
                    text: r.get(2)?,
                    user_id: r.get(3)?,
                    display: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Self::timed("search_messages", rows.len(), t, rows))
    }

    /// Aggregate counters for the App Stats "Index health" block.
    pub fn health(&self) -> Result<IndexHealth> {
        if let Some((cached, at)) = &*self.health_cache.lock()
            && at.elapsed() < HEALTH_TTL
        {
            return Ok(cached.clone());
        }
        let h = self.health_uncached()?;
        *self.health_cache.lock() = Some((h.clone(), std::time::Instant::now()));
        Ok(h)
    }

    /// The queries behind [`health`](Self::health).
    ///
    /// Every count here is deliberately cheap, because this is read from the
    /// UI thread. `COUNT(*)` over `chat_message` is a full scan of millions of
    /// rows — 172 ms at 8M messages — and the old unresolved-logins predicate
    /// (`user_key LIKE 'login:%'`) could not use an index at all and cost a
    /// full second. Both were measured on a real 909 MB index, on the main
    /// thread, at which point the Users tab is simply broken.
    ///
    /// So the two big totals come from `indexed_take` instead — one row per
    /// take, ~1 ms — which already records what each take contributed.
    /// `messages` is exact (it is the sum of what was written). `presence_rows`
    /// can read a hair high: merging a login-keyed identity into an id-keyed
    /// one collapses their rows in any stream they BOTH appear in, which the
    /// per-take totals can't know about. Measured drift on a real index: 70 of
    /// 610,630, or 0.01%. It is a health readout, not an accounting ledger.
    fn health_uncached(&self) -> Result<IndexHealth> {
        let t = std::time::Instant::now();
        let conn = self.db();
        let one = |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |r| r.get(0)) };
        let (messages, presence_rows, takes_indexed, takes_failed) = conn.query_row(
            "SELECT COALESCE(SUM(msgs), 0), COALESCE(SUM(users), 0),
                    COALESCE(SUM(status = 'ok'), 0), COALESCE(SUM(status != 'ok'), 0)
             FROM indexed_take",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let mut h = IndexHealth {
            users: one("SELECT COUNT(*) FROM chat_user WHERE merged_into = 0")?,
            presence_rows,
            messages,
            takes_indexed,
            takes_failed,
            // `resolved` is set at insert for anything keyed by a real platform
            // id, so this predicate is exactly "login-keyed and never looked
            // up" and rides `idx_chat_user_unresolved`.
            unresolved_logins: one(
                "SELECT COUNT(*) FROM chat_user WHERE resolved = 0 AND merged_into = 0",
            )?,
            ..Default::default()
        };
        if let Some((ms, rec)) = conn
            .query_row(
                "SELECT parse_ms + insert_ms + fts_ms AS total, rec_id FROM indexed_take
                 ORDER BY total DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
        {
            h.slowest_ms = ms;
            h.slowest_rec_id = rec;
        }
        drop(conn);
        h.bytes_on_disk = self.bytes_on_disk();
        Ok(Self::timed("health", 1, t, h))
    }

    /// Size of the index file plus its WAL, as the Settings readout shows it.
    pub fn bytes_on_disk(&self) -> u64 {
        let mut total = 0;
        for suffix in ["", "-wal", "-shm"] {
            let p = if suffix.is_empty() {
                self.path.clone()
            } else {
                PathBuf::from(format!("{}{suffix}", self.path.display()))
            };
            total += crate::iomon::fs::metadata_sync(crate::iomon::Cat::ChatIndexDb, &p)
                .map(|m| m.len())
                .unwrap_or(0);
        }
        total
    }

    /// Empty every table, keeping the file. Used by "Delete and rebuild": the
    /// sweep then re-reads every sidecar from scratch.
    pub fn clear(&self) -> Result<()> {
        let conn = self.db();
        conn.execute_batch(
            "DELETE FROM chat_message_fts;
             DELETE FROM chat_message;
             DELETE FROM chat_presence;
             DELETE FROM chat_user;
             DELETE FROM indexed_take;",
        )?;
        // A user pressing "Rebuild" expects the readout to go to zero at once,
        // not in five seconds.
        *self.health_cache.lock() = None;
        info!("chat index: cleared — every take will be re-read");
        Ok(())
    }
}

fn map_user_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    let key: String = r.get(2)?;
    Ok(UserRow {
        id: r.get(0)?,
        platform: r.get(1)?,
        name_matched: key_is_name_matched(&key),
        key,
        login: r.get(3)?,
        display: r.get(4)?,
        first_seen: r.get(5)?,
        last_seen: r.get(6)?,
        msgs_total: r.get(7)?,
        streams_total: r.get(8)?,
    })
}

/// Turn user input into an FTS5 query string.
///
/// FTS5's query syntax treats `"`, `*`, `:`, `^`, `-`, `(`, `)`, `AND`/`OR`/`NOT`
/// as operators, so raw chat text pasted into the box (`:)`, `@name`, `!drop`)
/// is a syntax error, not a search. Each whitespace-separated term is quoted
/// into a phrase instead, which is what a user typing words into a search box
/// means. A trailing `*` survives as a prefix search — the one operator worth
/// keeping.
pub fn fts_query(input: &str) -> String {
    let mut out = String::new();
    for term in input.split_whitespace() {
        let prefix = term.ends_with('*') && term.len() > 1;
        let core = if prefix { &term[..term.len() - 1] } else { term };
        let cleaned: String = core.replace('"', "");
        if cleaned.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push('"');
        out.push_str(&cleaned);
        out.push('"');
        if prefix {
            out.push('*');
        }
    }
    out
}

static SHARED: std::sync::OnceLock<Option<Arc<ChatIndex>>> = std::sync::OnceLock::new();

/// The app-wide index, opened on first use, or `None` if it can't be opened.
///
/// One file means one connection, and both the scheduler's sweep and the Users
/// view need it — threading it through `DetectContext` *and* `AppCore` would be
/// two paths to the same singleton. Rebuilding goes through
/// [`ChatIndex::clear`] rather than deleting the file, so this handle stays
/// valid for the life of the process.
///
/// A failure here is never fatal: the index is an accelerator, and everything
/// that reads it degrades to "not indexed yet" rather than breaking.
pub fn shared() -> Option<&'static Arc<ChatIndex>> {
    SHARED
        .get_or_init(|| {
            let path = index_path();
            match ChatIndex::open(&path) {
                Ok(idx) => {
                    info!(path = %path.display(), "chat index: opened");
                    Some(Arc::new(idx))
                }
                Err(e) => {
                    warn!(path = %path.display(), "chat index: unavailable: {e:#}");
                    None
                }
            }
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(key: &str, login: &str, at: i64, text: &str) -> IndexedMessage {
        IndexedMessage {
            key: UserKey::new("twitch", key, login).unwrap(),
            login: login.to_string(),
            display: login.to_string(),
            at,
            text: text.to_string(),
        }
    }

    fn parsed(messages: Vec<IndexedMessage>) -> ParsedSidecar {
        ParsedSidecar { messages, bytes: 0 }
    }

    /// One take to file rows under. Monitor 3 / channel 2 throughout, so a test
    /// only names what it actually cares about.
    fn take(rec_id: i64, chat_path: &str) -> TakeRef<'_> {
        TakeRef { rec_id, monitor_id: 3, channel_id: 2, chat_path }
    }

    #[test]
    fn user_key_prefers_the_platform_id() {
        let k = UserKey::new("twitch", "12345", "someone").unwrap();
        assert_eq!(k.key, "12345");
        assert!(!key_is_name_matched(&k.key));
    }

    #[test]
    fn user_key_falls_back_to_a_lowercased_login() {
        let k = UserKey::new("twitch", "", "SomeOne").unwrap();
        assert_eq!(k.key, "login:someone");
        assert!(key_is_name_matched(&k.key));
    }

    #[test]
    fn user_key_needs_something_to_key_on() {
        assert!(UserKey::new("twitch", "  ", "").is_none());
    }

    #[test]
    fn writing_a_take_rolls_up_presence_and_totals() {
        let idx = ChatIndex::open_in_memory().unwrap();
        let p = parsed(vec![
            msg("1", "ann", 100, "hello"),
            msg("1", "ann", 160, "still here"),
            msg("2", "bob", 120, "hi ann"),
        ]);
        let (msgs, users) = idx.write_take(&take(7, "c:/x.jsonl"), &p, 0, 1_000).unwrap();
        assert_eq!((msgs, users), (3, 2));

        let ann = idx.find_users("ann", 10).unwrap().pop().unwrap();
        assert_eq!((ann.msgs_total, ann.streams_total), (2, 1));
        let streams = idx.user_streams(ann.id, 10).unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!((streams[0].first_at, streams[0].last_at, streams[0].msgs), (100, 160, 2));
        assert_eq!(streams[0].channel_id, 2);
    }

    #[test]
    fn reindexing_a_take_replaces_rather_than_doubles() {
        let idx = ChatIndex::open_in_memory().unwrap();
        let p = parsed(vec![msg("1", "ann", 100, "hello")]);
        idx.write_take(&take(7, "c:/x.jsonl"), &p, 0, 1_000).unwrap();
        idx.write_take(&take(7, "c:/x.jsonl"), &p, 0, 2_000).unwrap();
        let ann = idx.find_users("ann", 10).unwrap().pop().unwrap();
        assert_eq!((ann.msgs_total, ann.streams_total), (1, 1));
        assert_eq!(idx.health().unwrap().messages, 1);
        // The FTS rows must be replaced too, not accumulated — a stale external
        // content row would return a hit pointing at a deleted message.
        assert_eq!(idx.search_messages("hello", 10).unwrap().len(), 1);
    }

    #[test]
    fn full_text_search_finds_messages_and_scopes_to_a_user() {
        let idx = ChatIndex::open_in_memory().unwrap();
        let p = parsed(vec![
            msg("1", "ann", 100, "the cheese is old and moldy"),
            msg("2", "bob", 120, "where is the bathroom"),
        ]);
        idx.write_take(&take(7, "c:/x.jsonl"), &p, 0, 1_000).unwrap();
        let hits = idx.search_messages("cheese", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display, "ann");
        let ann = idx.find_users("ann", 10).unwrap().pop().unwrap();
        assert_eq!(idx.user_messages(ann.id, "bathroom", 10).unwrap().len(), 0);
        assert_eq!(idx.user_messages(ann.id, "cheese", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_input_is_never_treated_as_fts_syntax() {
        // Chat is full of text that is FTS5 operator syntax; typing it into the
        // search box must search, not fail.
        assert_eq!(fts_query("hello"), "\"hello\"");
        assert_eq!(fts_query("  two  words "), "\"two\" \"words\"");
        assert_eq!(fts_query("pre*"), "\"pre\"*");
        assert_eq!(fts_query("\"quoted\""), "\"quoted\"");
        let idx = ChatIndex::open_in_memory().unwrap();
        let p = parsed(vec![msg("1", "ann", 100, "NOT really an operator")]);
        idx.write_take(&take(7, "c:/x.jsonl"), &p, 0, 1_000).unwrap();
        // Bare `NOT` would be a syntax error unquoted.
        assert_eq!(idx.search_messages("NOT", 10).unwrap().len(), 1);
        // ":)" is all punctuation — no terms, so no query and no error.
        assert!(idx.search_messages(":)", 10).unwrap().is_empty());
    }

    #[test]
    fn resolving_a_login_merges_it_into_the_id_keyed_identity() {
        let idx = ChatIndex::open_in_memory().unwrap();
        // An old log (login only) and a new one (id) for the same person.
        idx.write_take(&take(1, "old"), &parsed(vec![msg("", "ann", 100, "old message")]), 0, 1)
            .unwrap();
        idx.write_take(&take(2, "new"), &parsed(vec![msg("42", "ann", 900, "new message")]), 0, 1)
            .unwrap();
        let before = idx.find_users("ann", 10).unwrap();
        assert_eq!(before.len(), 2, "two identities before the merge");

        let legacy = idx.unresolved_logins(10).unwrap();
        assert_eq!(legacy.len(), 1);
        assert!(idx.resolve_login(legacy[0].0, Some("42")).unwrap());

        let after = idx.find_users("ann", 10).unwrap();
        assert_eq!(after.len(), 1, "one identity after the merge");
        assert_eq!((after[0].msgs_total, after[0].streams_total), (2, 2));
        assert!(!after[0].name_matched);
        assert_eq!(idx.name_matched_streams(after[0].id).unwrap(), 2);
        // Both messages now hang off the surviving identity.
        assert_eq!(idx.user_messages(after[0].id, "", 10).unwrap().len(), 2);
    }

    #[test]
    fn a_login_with_no_helix_answer_is_marked_resolved_but_never_merged() {
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.write_take(&take(1, "old"), &parsed(vec![msg("", "ann", 100, "hi")]), 0, 1).unwrap();
        let legacy = idx.unresolved_logins(10).unwrap();
        assert!(!idx.resolve_login(legacy[0].0, None).unwrap());
        // Marked resolved, so the resolver stops asking...
        assert!(idx.unresolved_logins(10).unwrap().is_empty());
        // ...but the identity is untouched and still findable.
        let rows = idx.find_users("ann", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].name_matched);
    }

    #[test]
    fn adopting_an_id_in_place_when_no_id_keyed_row_exists() {
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.write_take(&take(1, "old"), &parsed(vec![msg("", "ann", 100, "hi")]), 0, 1).unwrap();
        let legacy = idx.unresolved_logins(10).unwrap();
        assert!(idx.resolve_login(legacy[0].0, Some("42")).unwrap());
        let rows = idx.find_users("ann", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "42");
        assert!(!rows[0].name_matched);
        assert_eq!(rows[0].msgs_total, 1);
    }

    #[test]
    fn merging_sums_a_stream_both_identities_appear_in() {
        let idx = ChatIndex::open_in_memory().unwrap();
        // Same recording, same person, seen once under each key — possible when
        // a take is re-indexed across the id-capture boundary.
        idx.write_take(&take(5, "x"), &parsed(vec![msg("", "ann", 100, "a"), msg("42", "ann", 200, "b")]),
            0,
            1,
        )
        .unwrap();
        let legacy = idx.unresolved_logins(10).unwrap();
        assert!(idx.resolve_login(legacy[0].0, Some("42")).unwrap());
        let row = idx.find_users("ann", 10).unwrap().pop().unwrap();
        let streams = idx.user_streams(row.id, 10).unwrap();
        assert_eq!(streams.len(), 1, "one presence row, not two");
        assert_eq!((streams[0].first_at, streams[0].last_at, streams[0].msgs), (100, 200, 2));
    }

    #[test]
    fn searching_an_old_name_lands_on_the_merged_identity() {
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.write_take(&take(1, "old"), &parsed(vec![msg("", "oldname", 100, "hi")]), 0, 1)
            .unwrap();
        idx.write_take(&take(2, "new"), &parsed(vec![msg("42", "newname", 900, "hi")]), 0, 1)
            .unwrap();
        let legacy = idx.unresolved_logins(10).unwrap();
        idx.resolve_login(legacy[0].0, Some("42")).unwrap();
        let hit = idx.find_users("oldname", 10).unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].display, "newname", "folded to the surviving identity");
        let aliases = idx.aliases(hit[0].id).unwrap();
        assert!(aliases.contains(&"oldname".to_string()));
        assert!(aliases.contains(&"newname".to_string()));
    }

    #[test]
    fn a_stamped_take_drains_the_queue_without_rows() {
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.stamp_take(9, "c:/gone.jsonl", status::MISSING, 500).unwrap();
        assert!(idx.indexed_rec_ids().unwrap().contains(&9));
        let h = idx.health().unwrap();
        assert_eq!((h.takes_indexed, h.takes_failed), (0, 1));
    }

    #[test]
    fn forgetting_a_take_removes_its_rows_and_its_stamp() {
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.write_take(&take(7, "x"), &parsed(vec![msg("1", "ann", 100, "hello")]), 0, 1).unwrap();
        idx.forget_take(7).unwrap();
        let h = idx.health().unwrap();
        assert_eq!((h.messages, h.presence_rows, h.takes_indexed), (0, 0, 0));
        assert!(idx.indexed_rec_ids().unwrap().is_empty());
        assert!(idx.search_messages("hello", 10).unwrap().is_empty());
    }



    #[test]
    fn health_totals_come_from_the_per_take_rollup_not_a_full_scan() {
        // `COUNT(*)` over chat_message cost 172 ms at 8M rows, on the UI
        // thread. These totals must agree with the real tables while being
        // derived from `indexed_take` instead.
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.write_take(
            &take(7, "x"),
            &parsed(vec![
                msg("1", "ann", 100, "a"),
                msg("2", "bob", 110, "b"),
                msg("2", "bob", 120, "c"),
            ]),
            0,
            1,
        )
        .unwrap();
        idx.write_take(&take(8, "y"), &parsed(vec![msg("1", "ann", 200, "d")]), 0, 1).unwrap();
        let h = idx.health_uncached().unwrap();
        assert_eq!(h.messages, 4);
        assert_eq!(h.presence_rows, 3, "ann in two streams, bob in one");
        assert_eq!((h.takes_indexed, h.takes_failed), (2, 0));
        // ...and they match what a full scan would have said.
        let conn = idx.db();
        let msgs: i64 = conn.query_row("SELECT COUNT(*) FROM chat_message", [], |r| r.get(0)).unwrap();
        let pres: i64 =
            conn.query_row("SELECT COUNT(*) FROM chat_presence", [], |r| r.get(0)).unwrap();
        assert_eq!((h.messages, h.presence_rows), (msgs, pres));
    }

    #[test]
    fn only_login_keyed_identities_are_ever_unresolved() {
        // `resolved` means "we know this identity's account id" — set at insert
        // for anything keyed by a real id. That is what lets the lookup use an
        // index instead of a `LIKE 'login:%'` scan (measured: 1034 ms -> 3 ms).
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.write_take(
            &take(7, "x"),
            &parsed(vec![msg("42", "hasid", 100, "a"), msg("", "noid", 110, "b")]),
            0,
            1,
        )
        .unwrap();
        let pending = idx.unresolved_logins(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "noid");
        assert_eq!(idx.health_uncached().unwrap().unresolved_logins, 1);
    }

    #[test]
    fn health_is_cached_but_a_rebuild_clears_it_immediately() {
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.write_take(&take(7, "x"), &parsed(vec![msg("1", "ann", 100, "hi")]), 0, 1).unwrap();
        assert_eq!(idx.health().unwrap().messages, 1);
        // Within the TTL a later write is not yet reflected — the whole point.
        idx.write_take(&take(8, "y"), &parsed(vec![msg("1", "ann", 200, "again")]), 0, 1).unwrap();
        assert_eq!(idx.health().unwrap().messages, 1, "still the cached answer");
        assert_eq!(idx.health_uncached().unwrap().messages, 2, "the truth underneath");
        // Rebuild must not leave a stale readout sitting there.
        idx.clear().unwrap();
        assert_eq!(idx.health().unwrap().messages, 0);
    }

    #[test]
    fn clearing_empties_every_table() {
        let idx = ChatIndex::open_in_memory().unwrap();
        idx.write_take(&take(7, "x"), &parsed(vec![msg("1", "ann", 100, "hi")]), 0, 1).unwrap();
        idx.clear().unwrap();
        let h = idx.health().unwrap();
        assert_eq!((h.users, h.messages, h.presence_rows, h.takes_indexed), (0, 0, 0, 0));
    }
}
