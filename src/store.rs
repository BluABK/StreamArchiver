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
    Monitor, MonitorStreamChange, MonitorWithChannel, Platform, PollBucket, RecurrenceKind,
    SabrCodecPref, ScheduleSegment, ScheduledRecording, ScheduledRecordingWithNames,
    StreamMetaChange, Tool, UpcomingStream, Video, now_unix,
};

/// Latest schema version understood by this build.
const SCHEMA_VERSION: i64 = 62;

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

mod collab;
mod migrations;
mod monitors;
mod posts;
mod recordings;
mod scheduled;
mod stats_history;
pub use stats_history::K_VH_DOWNSAMPLE_DAYS;
mod videos;
mod vod;

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

/// Live + recent diagnostics for the app-wide DB connection lock — feeds the
/// "slow DB lock" warnings (naming the holder a waiter was blocked behind)
/// and the I/O tab's Database panel. Global on purpose: the app has one
/// on-disk Store. In-memory test stores report into the same slots, which
/// only matters to tests that would assert on the shared state (none do).
pub mod db_lock {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    /// One party at the lock: which thread, from which store call site, since when.
    #[derive(Clone)]
    pub(super) struct Entry {
        pub(super) thread: String,
        pub(super) file: &'static str,
        pub(super) line: u32,
        pub(super) since: Instant,
    }

    impl Entry {
        pub(super) fn call_site(&self) -> String {
            format!("{}:{}", self.file, self.line)
        }
    }

    pub(super) static HOLDER: parking_lot::Mutex<Option<Entry>> = parking_lot::Mutex::new(None);
    pub(super) static WAITERS: parking_lot::Mutex<Vec<(u64, Entry)>> =
        parking_lot::Mutex::new(Vec::new());
    pub(super) static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
    pub(super) static SLOW_WAITS: AtomicU64 = AtomicU64::new(0);
    pub(super) static LONG_HOLDS: AtomicU64 = AtomicU64::new(0);
    pub(super) static SLOW_EVENTS: parking_lot::Mutex<VecDeque<SlowEvent>> =
        parking_lot::Mutex::new(VecDeque::new());
    const SLOW_EVENTS_CAP: usize = 64;

    /// One recent contention incident (a ≥50 ms wait or a ≥200 ms hold).
    #[derive(Clone)]
    pub struct SlowEvent {
        pub at_unix: i64,
        /// `"wait"` or `"hold"`.
        pub kind: &'static str,
        pub ms: u64,
        pub thread: String,
        pub call_site: String,
        /// For waits: who held the lock when the wait started.
        pub blocked_on: Option<String>,
    }

    /// Point-in-time picture for the I/O tab's Database panel.
    #[derive(Clone, Default)]
    pub struct Snap {
        /// `(thread, call site, seconds held)` of the current holder.
        pub holder: Option<(String, String, f64)>,
        /// `(thread, call site, seconds waiting)` per waiter, queue order.
        pub waiters: Vec<(String, String, f64)>,
        pub slow_waits: u64,
        pub long_holds: u64,
        /// Recent contention incidents, newest first.
        pub recent: Vec<SlowEvent>,
    }

    pub fn snapshot() -> Snap {
        let holder = HOLDER
            .lock()
            .as_ref()
            .map(|e| (e.thread.clone(), e.call_site(), e.since.elapsed().as_secs_f64()));
        let waiters = WAITERS
            .lock()
            .iter()
            .map(|(_, e)| (e.thread.clone(), e.call_site(), e.since.elapsed().as_secs_f64()))
            .collect();
        Snap {
            holder,
            waiters,
            slow_waits: SLOW_WAITS.load(Ordering::Relaxed),
            long_holds: LONG_HOLDS.load(Ordering::Relaxed),
            recent: SLOW_EVENTS.lock().iter().rev().cloned().collect(),
        }
    }

    pub(super) fn push_event(ev: SlowEvent) {
        let mut q = SLOW_EVENTS.lock();
        if q.len() >= SLOW_EVENTS_CAP {
            q.pop_front();
        }
        q.push_back(ev);
    }

    pub(super) fn thread_name() -> String {
        std::thread::current().name().unwrap_or("?").to_string()
    }
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
            true,
        );
        // Clear the holder slot BEFORE the inner guard releases the mutex
        // (field drop runs after this body), so the next holder never races
        // an overwrite from us.
        *db_lock::HOLDER.lock() = None;
        let ms = self.acquired_at.elapsed().as_millis();
        if ms >= 200 {
            db_lock::LONG_HOLDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let thread = db_lock::thread_name();
            tracing::warn!(
                hold_ms = ms,
                thread = thread.as_str(),
                file = self.caller.file(),
                line = self.caller.line(),
                "store: long DB lock hold"
            );
            db_lock::push_event(db_lock::SlowEvent {
                at_unix: crate::models::now_unix(),
                kind: "hold",
                ms: ms as u64,
                thread,
                call_site: format!("{}:{}", self.caller.file(), self.caller.line()),
                blocked_on: None,
            });
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
        // Record ourselves as HOLDER right after actually acquiring the real
        // mutex, before doing anything else. Slow-wait logging below (atomic
        // counters, `tracing::warn!` formatting/dispatch, a locked VecDeque
        // push) is not instant — a waiter whose own `try_lock()` fails while
        // we're still in that logging, before HOLDER is updated, would
        // otherwise blame "<holder unknown>" despite us clearly holding the
        // lock (seen live: a wait logged unknown immediately after another
        // thread's own contended acquisition).
        let set_holder = || {
            *db_lock::HOLDER.lock() = Some(db_lock::Entry {
                thread: db_lock::thread_name(),
                file: caller.file(),
                line: caller.line(),
                since: std::time::Instant::now(),
            });
        };
        // Uncontended fast path (parking_lot's fair unlock hands the mutex
        // directly to the next queued waiter, so try_lock can't barge).
        let g = match self.conn.try_lock() {
            Some(g) => {
                set_holder();
                g
            }
            None => {
                // Contended: remember who we're stuck behind (the holder at
                // wait start — the one worth blaming) and join the visible
                // waiter queue for the I/O tab.
                let blocked_on = db_lock::HOLDER.lock().as_ref().map(|h| {
                    format!(
                        "{} at {} (held {}ms so far)",
                        h.thread,
                        h.call_site(),
                        h.since.elapsed().as_millis()
                    )
                });
                let token =
                    db_lock::NEXT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                db_lock::WAITERS.lock().push((
                    token,
                    db_lock::Entry {
                        thread: db_lock::thread_name(),
                        file: caller.file(),
                        line: caller.line(),
                        since: t,
                    },
                ));
                // Leaves the queue on every exit path (incl. unwinds).
                struct WaiterGuard(u64);
                impl Drop for WaiterGuard {
                    fn drop(&mut self) {
                        db_lock::WAITERS.lock().retain(|(t, _)| *t != self.0);
                    }
                }
                let _wg = WaiterGuard(token);
                let g = self.conn.lock();
                set_holder();
                let wait_ms = t.elapsed().as_millis();
                if wait_ms >= 50 {
                    db_lock::SLOW_WAITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let blame =
                        blocked_on.clone().unwrap_or_else(|| "<holder unknown>".to_string());
                    tracing::warn!(
                        wait_ms,
                        file = caller.file(),
                        line = caller.line(),
                        "store: slow DB lock – blocked behind {blame}"
                    );
                    db_lock::push_event(db_lock::SlowEvent {
                        at_unix: crate::models::now_unix(),
                        kind: "wait",
                        ms: wait_ms as u64,
                        thread: db_lock::thread_name(),
                        call_site: format!("{}:{}", caller.file(), caller.line()),
                        blocked_on,
                    });
                } else if wait_ms >= 5 {
                    tracing::debug!(
                        wait_ms,
                        file = caller.file(),
                        line = caller.line(),
                        "store: DB lock wait"
                    );
                }
                g
            }
        };
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
pub(crate) mod test_util {
    use super::*;

    pub fn sample_monitor(channel_id: i64) -> Monitor {
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
    pub fn sample_video() -> Video {
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
    pub fn about(cid: i64, platform: &str, account: &str, hash: &str, desc: &str) -> NewAboutSnapshot {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
