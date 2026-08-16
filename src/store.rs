//! SQLite-backed persistence (rusqlite, WAL) — the source of truth for
//! channels, monitors, recordings, and key/value settings.
//!
//! rusqlite is synchronous; the connection is wrapped in a `Mutex`. Config CRUD
//! happens on the UI thread (low volume); background tasks will access the same
//! `Arc<Store>` via `spawn_blocking`.

use std::collections::HashMap;
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
    AdBreak, AuthKind, Channel, Clip, Container, DailyRecordingStat, DetachedKind, DetachedRow,
    DetectionMethod, FfmpegJobKind, FfmpegJobRow, GlobalStats, Monitor, MonitorStreamChange,
    MonitorWithChannel, Platform, PollBucket, RecurrenceKind, SabrCodecPref, ScheduleSegment,
    ScheduledRecording, ScheduledRecordingWithNames, StreamMetaChange, Tool, UpcomingStream, Video,
    now_unix,
};

/// Latest schema version understood by this build.
const SCHEMA_VERSION: i64 = 95;

pub struct Store {
    conn: FairMutex<Connection>,
    /// Whole-table mirror of `app_settings`, `None` until first read.
    ///
    /// Settings are the app's most-read table by a wide margin — every render
    /// path asks about a toggle — and each ask took the one store-wide
    /// connection lock. Measured on a real session: **16,484 slow waits and
    /// 234 seconds of waiting** at [`Store::get_setting`] alone, 39% of every
    /// contended acquire in the app, for a table of 207 rows whose query costs
    /// 0.04 ms. The cost was never the query; it was queueing behind whatever
    /// else held the connection.
    ///
    /// Safe to cache because [`Store::set_setting`] is the only writer outside
    /// migrations, and migrations run to completion before anything can read.
    /// An `RwLock`, not the connection lock: concurrent readers is the whole
    /// point.
    settings: std::sync::RwLock<Option<HashMap<String, String>>>,
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
    pub id: i64,
    pub created_at: i64,
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub body: String,
    /// The instance the row is about, when it has one — the feed resolves it to
    /// that channel's avatar/name colour and its "Watch in player" button.
    pub monitor_id: Option<i64>,
    pub channel: String,
    #[allow(dead_code)]
    pub recording_id: Option<i64>,
    pub action_label: String,
    pub action_url: String,
    /// Resolved hero/logo image on disk. The feed draws the channel's own
    /// avatar instead (consistent with the Streams grid); this stays the toast's
    /// picture.
    #[allow(dead_code)]
    pub image_path: String,
    /// Dedup key the row was inserted with — also how a `youtube_post` row
    /// names the post its "View post" button opens (`post:{monitor}:{post_id}`).
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

mod alerts;
pub use alerts::{
    AlertDailyStat, AlertHealthTotals, CaptureAlertRow, GapRangeRow, NewCaptureAlert, RecAlertBadge,
};
mod channel_groups;
mod clips;
pub use clips::VodCdnRow;
mod collab;
pub use collab::PartnerSessionRow;
mod disposal_records;
pub use disposal_records::DisposalRecordDisplay;
mod ffmpeg_jobs;
mod migrations;
mod monitors;
mod posts;
mod recordings;
pub use recordings::{ChatIndexTarget, EarlierTakeRow, TakeLabel};
mod recording_groups;
mod scheduled;
mod stats_history;
pub use stats_history::K_VH_DOWNSAMPLE_DAYS;
mod videos;
mod vod;
mod watch;

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

/// Live + recent diagnostics for a SQLite connection lock — feeds the "slow DB
/// lock" warnings (naming the holder a waiter was blocked behind) and the I/O
/// tab's Database panel.
///
/// One [`LockDiag`] per connection, not one per process: the app now holds two
/// independent databases — the operational store ([`MAIN`]) and the rebuildable
/// chat index ([`CHAT_INDEX`], `crate::chat_index`) — and the whole point of
/// giving the index its own file is that its multi-hundred-millisecond
/// full-text writes cannot block the UI's store queries. Merging their
/// diagnostics into one set of counters would hide exactly the thing worth
/// watching, so each lane is labelled and reported separately.
///
/// In-memory test stores report into [`MAIN`]'s slots, which only matters to
/// tests that would assert on the shared state (none do).
pub mod db_lock {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    /// One party at the lock: which thread, from which store call site, since when.
    #[derive(Clone)]
    pub struct Entry {
        pub(super) thread: String,
        pub(super) file: &'static str,
        pub(super) line: u32,
        pub(super) since: Instant,
        /// Tags which acquisition wrote this entry, so the outgoing holder's
        /// `Drop` can compare-and-clear instead of blindly overwriting —
        /// see `LockDiag::holder_token` below.
        pub(super) token: u64,
    }

    impl Entry {
        pub(super) fn call_site(&self) -> String {
            format!("{}:{}", self.file, self.line)
        }
    }

    const SLOW_EVENTS_CAP: usize = 64;
    /// A waiter blocked this long (ms) is worth a warning and a counter.
    const SLOW_WAIT_MS: u128 = 50;
    /// A holder keeping the lock this long (ms) is worth a warning — long
    /// enough that a UI frame would notice.
    const LONG_HOLD_MS: u128 = 200;
    /// Floor for the per-acquire DEBUG lines below [`SLOW_WAIT_MS`].
    ///
    /// Was 5 ms, which on a busy library produced **42,000 log lines in one
    /// session** — enough that reading the log for anything else meant
    /// filtering them out first, and the diagnostic drowned the diagnosis.
    /// A few milliseconds of queueing is ordinary and says nothing; the
    /// counters and the Stats tab's Database panel already carry the totals
    /// for anyone who wants the distribution.
    const CHATTY_MS: u128 = 25;

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
        /// Which connection this describes ("database" / "chat index").
        pub label: &'static str,
        /// `(thread, call site, seconds held)` of the current holder.
        pub holder: Option<(String, String, f64)>,
        /// `(thread, call site, seconds waiting)` per waiter, queue order.
        pub waiters: Vec<(String, String, f64)>,
        pub slow_waits: u64,
        pub long_holds: u64,
        /// Recent contention incidents, newest first.
        pub recent: Vec<SlowEvent>,
    }

    /// The diagnostics for one connection lock.
    pub struct LockDiag {
        /// Human name of this connection, used in log lines and the I/O tab.
        pub label: &'static str,
        /// Log-line prefix, so `store:` and `chat index:` warnings are
        /// greppable apart.
        pub log_prefix: &'static str,
        /// Which I/O-monitor category this lock's hold time is charged to.
        pub cat: crate::iomon::Cat,
        holder: parking_lot::Mutex<Option<Entry>>,
        waiters: parking_lot::Mutex<Vec<(u64, Entry)>>,
        /// Orders the `waiters` queue.
        next_token: AtomicU64,
        /// Separate id space from `next_token` — mints a unique id per
        /// successful acquisition so the outgoing holder can tell "is the
        /// holder slot still mine?" before clearing it.
        holder_token: AtomicU64,
        slow_waits: AtomicU64,
        long_holds: AtomicU64,
        slow_events: parking_lot::Mutex<VecDeque<SlowEvent>>,
    }

    impl LockDiag {
        pub const fn new(
            label: &'static str,
            log_prefix: &'static str,
            cat: crate::iomon::Cat,
        ) -> LockDiag {
            LockDiag {
                label,
                log_prefix,
                cat,
                holder: parking_lot::Mutex::new(None),
                waiters: parking_lot::Mutex::new(Vec::new()),
                next_token: AtomicU64::new(1),
                holder_token: AtomicU64::new(1),
                slow_waits: AtomicU64::new(0),
                long_holds: AtomicU64::new(0),
                slow_events: parking_lot::Mutex::new(VecDeque::new()),
            }
        }

        pub fn snapshot(&self) -> Snap {
            let holder = self
                .holder
                .lock()
                .as_ref()
                .map(|e| (e.thread.clone(), e.call_site(), e.since.elapsed().as_secs_f64()));
            let waiters = self
                .waiters
                .lock()
                .iter()
                .map(|(_, e)| (e.thread.clone(), e.call_site(), e.since.elapsed().as_secs_f64()))
                .collect();
            Snap {
                label: self.label,
                holder,
                waiters,
                slow_waits: self.slow_waits.load(Ordering::Relaxed),
                long_holds: self.long_holds.load(Ordering::Relaxed),
                recent: self.slow_events.lock().iter().rev().cloned().collect(),
            }
        }

        fn push_event(&self, ev: SlowEvent) {
            let mut q = self.slow_events.lock();
            if q.len() >= SLOW_EVENTS_CAP {
                q.pop_front();
            }
            q.push_back(ev);
        }
    }

    /// The operational database (`streamarchiver.sqlite3`) — [`super::Store`].
    pub static MAIN: LockDiag = LockDiag::new("database", "store", crate::iomon::Cat::Db);
    /// The rebuildable chat index (`chat_index.sqlite3`) —
    /// [`crate::chat_index::ChatIndex`]. Separate file, separate lock, so a
    /// long full-text write never shows up as store contention.
    pub static CHAT_INDEX: LockDiag =
        LockDiag::new("chat index", "chat index", crate::iomon::Cat::ChatIndexDb);

    /// Remove `*slot` iff it's still tagged with `token` — a plain
    /// unconditional clear would risk wiping a fresh entry written by
    /// another thread that raced in and acquired the lock between the real
    /// unlock and this call. A no-op when someone else already holds it.
    /// Generic over the mutex (rather than hardwired to one `LockDiag`) so it's
    /// independently unit-testable against a throwaway local instance — the
    /// process-wide statics are shared by every test in the binary (each
    /// `Store::open_in_memory()` still goes through the real `db()`), so
    /// asserting on them directly would be racy under `cargo test`'s parallel
    /// runner.
    pub(super) fn clear_holder_if_matches_in(slot: &parking_lot::Mutex<Option<Entry>>, token: u64) {
        let mut h = slot.lock();
        if h.as_ref().is_some_and(|e| e.token == token) {
            *h = None;
        }
    }

    pub(super) fn thread_name() -> String {
        std::thread::current().name().unwrap_or("?").to_string()
    }

    /// Acquire `mutex`, recording the wait/hold in `diag`.
    ///
    /// Shared by every instrumented connection in the app: adding a second
    /// database must not mean a second copy of this (subtle) bookkeeping —
    /// the holder-attribution races it guards against were found the hard way
    /// (see [[db-lock-holder-unknown]]) and are not worth re-deriving.
    #[track_caller]
    pub fn acquire<'a, T>(
        mutex: &'a parking_lot::FairMutex<T>,
        diag: &'static LockDiag,
    ) -> Guard<'a, T> {
        let caller = std::panic::Location::caller();
        let t = Instant::now();
        // Record ourselves as HOLDER right after actually acquiring the real
        // mutex, before doing anything else. Slow-wait logging below (atomic
        // counters, `tracing::warn!` formatting/dispatch, a locked VecDeque
        // push) is not instant — a waiter whose own `try_lock()` fails while
        // we're still in that logging, before the holder slot is updated,
        // would otherwise blame "<holder unknown>" despite us clearly holding
        // the lock (seen live: a wait logged unknown immediately after another
        // thread's own contended acquisition). Returns the token this entry
        // was tagged with, so `Guard::drop` can compare-and-clear.
        let set_holder = || -> u64 {
            let holder_token = diag.holder_token.fetch_add(1, Ordering::Relaxed);
            *diag.holder.lock() = Some(Entry {
                thread: thread_name(),
                file: caller.file(),
                line: caller.line(),
                since: Instant::now(),
                token: holder_token,
            });
            holder_token
        };
        // Uncontended fast path (parking_lot's fair unlock hands the mutex
        // directly to the next queued waiter, so try_lock can't barge).
        let (g, holder_token) = match mutex.try_lock() {
            Some(g) => {
                let holder_token = set_holder();
                (g, holder_token)
            }
            None => {
                // Contended: remember who we're stuck behind (the holder at
                // wait start — the one worth blaming) and join the visible
                // waiter queue for the I/O tab.
                let blocked_on = diag.holder.lock().as_ref().map(|h| {
                    format!(
                        "{} at {} (held {}ms so far)",
                        h.thread,
                        h.call_site(),
                        h.since.elapsed().as_millis()
                    )
                });
                let token = diag.next_token.fetch_add(1, Ordering::Relaxed);
                diag.waiters.lock().push((
                    token,
                    Entry {
                        thread: thread_name(),
                        file: caller.file(),
                        line: caller.line(),
                        since: t,
                        token,
                    },
                ));
                // Leaves the queue on every exit path (incl. unwinds).
                struct WaiterGuard(&'static LockDiag, u64);
                impl Drop for WaiterGuard {
                    fn drop(&mut self) {
                        self.0.waiters.lock().retain(|(t, _)| *t != self.1);
                    }
                }
                let _wg = WaiterGuard(diag, token);
                let g = mutex.lock();
                let holder_token = set_holder();
                let wait_ms = t.elapsed().as_millis();
                if wait_ms >= SLOW_WAIT_MS {
                    diag.slow_waits.fetch_add(1, Ordering::Relaxed);
                    let blame =
                        blocked_on.clone().unwrap_or_else(|| "<holder unknown>".to_string());
                    tracing::warn!(
                        wait_ms,
                        lock = diag.label,
                        file = caller.file(),
                        line = caller.line(),
                        "{}: slow DB lock – blocked behind {blame}",
                        diag.log_prefix
                    );
                    diag.push_event(SlowEvent {
                        at_unix: crate::models::now_unix(),
                        kind: "wait",
                        ms: wait_ms as u64,
                        thread: thread_name(),
                        call_site: format!("{}:{}", caller.file(), caller.line()),
                        blocked_on,
                    });
                } else if wait_ms >= CHATTY_MS {
                    tracing::debug!(
                        wait_ms,
                        lock = diag.label,
                        file = caller.file(),
                        line = caller.line(),
                        "{}: DB lock wait",
                        diag.log_prefix
                    );
                }
                (g, holder_token)
            }
        };
        Guard { inner: Some(g), acquired_at: Instant::now(), caller, token: holder_token, diag }
    }

    /// RAII guard returned by [`acquire`]. Logs a warning when the lock is held
    /// longer than 200 ms, showing the call-site that acquired it — useful for
    /// identifying which method is the bottleneck.
    ///
    /// `inner` is an `Option` solely so `Drop` can release the real mutex as its
    /// very first action (`self.inner.take()`) — before any of the bookkeeping/
    /// logging below, which is not instant (tracing dispatch, a locked VecDeque
    /// push). It is `Some` for the guard's entire externally-visible lifetime;
    /// only `Drop::drop` ever sees it `None`.
    pub struct Guard<'a, T> {
        inner: Option<parking_lot::FairMutexGuard<'a, T>>,
        acquired_at: Instant,
        caller: &'static std::panic::Location<'static>,
        /// This guard's holder token — lets `Drop` compare-and-clear.
        token: u64,
        diag: &'static LockDiag,
    }

    impl<T> std::ops::Deref for Guard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.inner.as_deref().expect("Guard.inner is only None mid-drop")
        }
    }

    impl<T> std::ops::DerefMut for Guard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            self.inner.as_deref_mut().expect("Guard.inner is only None mid-drop")
        }
    }

    impl<T> Drop for Guard<'_, T> {
        fn drop(&mut self) {
            // Release the REAL lock first, before any bookkeeping/logging below
            // (which is not instant: tracing dispatch, a locked VecDeque push for
            // a long hold). Waiters unblock immediately instead of waiting out
            // our logging too, and — the point of doing this here rather than
            // letting the field drop naturally after this function returns — it
            // closes a holder-attribution race: the old ordering cleared the
            // holder slot to None *before* actually unlocking, so a waiter whose
            // try_lock() genuinely failed (we still held the real mutex) could
            // read it as already-empty and misattribute its wait to
            // "<holder unknown>" (see [[db-lock-holder-unknown]] — this is that
            // bug's sibling on the release side rather than the acquire side).
            drop(self.inner.take());

            // Count every DB access (ops + cumulative hold time) at the single
            // chokepoint all queries pass through; byte-level growth is sampled
            // from the db/WAL file sizes by the I/O monitor instead.
            crate::iomon::record_region(
                self.diag.cat,
                crate::iomon::Region::AppData,
                crate::iomon::OpKind::Meta,
                0,
                self.acquired_at.elapsed(),
                true,
            );
            // Compare-and-clear: only remove OUR entry. Between the unlock above
            // and this line, another thread may already have acquired the real
            // mutex and written its own holder entry — a blind overwrite here
            // would wipe that fresh, correct entry back to "no one" while they
            // still hold it.
            clear_holder_if_matches_in(&self.diag.holder, self.token);
            let ms = self.acquired_at.elapsed().as_millis();
            if ms >= LONG_HOLD_MS {
                self.diag.long_holds.fetch_add(1, Ordering::Relaxed);
                let thread = thread_name();
                tracing::warn!(
                    hold_ms = ms,
                    lock = self.diag.label,
                    thread = thread.as_str(),
                    file = self.caller.file(),
                    line = self.caller.line(),
                    "{}: long DB lock hold",
                    self.diag.log_prefix
                );
                self.diag.push_event(SlowEvent {
                    at_unix: crate::models::now_unix(),
                    kind: "hold",
                    ms: ms as u64,
                    thread,
                    call_site: format!("{}:{}", self.caller.file(), self.caller.line()),
                    blocked_on: None,
                });
            } else if ms >= SLOW_WAIT_MS {
                tracing::debug!(
                    hold_ms = ms,
                    lock = self.diag.label,
                    file = self.caller.file(),
                    line = self.caller.line(),
                    "{}: DB lock hold",
                    self.diag.log_prefix
                );
            }
        }
    }
}

/// RAII guard over the main store's connection.
type DbGuard<'a> = db_lock::Guard<'a, Connection>;

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
        db_lock::acquire(&self.conn, &db_lock::MAIN)
    }

    /// Open (or create) the database at `path`, set pragmas, and migrate.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;
        Self::configure(&conn)?;
        let store = Store {
            conn: FairMutex::new(conn),
            settings: std::sync::RwLock::new(None),
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
            settings: std::sync::RwLock::new(None),
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
        if let Some(map) = self.settings.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            return Ok(map.get(key).cloned());
        }
        // First read of the session: pull the whole (small) table in one go,
        // so this is the last time settings touch the connection lock.
        let loaded = {
            let conn = self.db();
            let mut stmt = conn.prepare("SELECT key, value FROM app_settings")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<HashMap<_, _>>>()?
        };
        let value = loaded.get(key).cloned();
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = Some(loaded);
        Ok(value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        {
            let conn = self.db();
            conn.execute(
                "INSERT INTO app_settings(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        // After the write, and only if it succeeded: a failed write must not
        // leave the mirror claiming a value the table doesn't hold.
        if let Some(map) = self.settings.write().unwrap_or_else(|e| e.into_inner()).as_mut() {
            map.insert(key.to_string(), value.to_string());
        }
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
            primary_group_id: r.get(9)?,
            posts_hidden: r.get::<_, i64>(10)? != 0,
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

    /// Settings are served from an in-memory mirror, so every way the mirror
    /// could disagree with the table has to be closed.
    ///
    /// The dangerous shapes are: a value read BEFORE the mirror is built
    /// (which is what populates it) and then written; a key that didn't exist
    /// when the mirror was built; and a value written before any read at all,
    /// so the mirror is built from a table that already has it.
    #[test]
    fn settings_cache_never_serves_a_value_the_table_doesnt_have() {
        let store = Store::open_in_memory().unwrap();

        // Read first (builds the mirror with the key absent), then write.
        assert_eq!(store.get_setting("a").unwrap(), None);
        store.set_setting("a", "1").unwrap();
        assert_eq!(store.get_setting("a").unwrap().as_deref(), Some("1"));

        // A key the mirror has never seen still resolves.
        store.set_setting("b", "2").unwrap();
        assert_eq!(store.get_setting("b").unwrap().as_deref(), Some("2"));

        // Overwrites land, including to empty (a real value, not "unset").
        store.set_setting("a", "").unwrap();
        assert_eq!(store.get_setting("a").unwrap().as_deref(), Some(""));

        // …and the mirror agrees with the table itself, not just with itself.
        let from_table: Option<String> = store
            .db()
            .query_row("SELECT value FROM app_settings WHERE key = 'a'", [], |r| r.get(0))
            .optional()
            .unwrap();
        assert_eq!(from_table.as_deref(), Some(""));

        // Write-before-any-read: the mirror is built from a table that
        // already holds the value.
        let store2 = Store::open_in_memory().unwrap();
        store2.set_setting("c", "3").unwrap();
        assert_eq!(store2.get_setting("c").unwrap().as_deref(), Some("3"));
    }

    /// A stale `DbGuard::drop` (delayed by its own slow-hold logging) must
    /// never wipe a fresh holder that already raced in and re-acquired the
    /// real mutex in the meantime — that overwrite is what previously made
    /// "who holds it" briefly report `None` even though the lock was
    /// genuinely, continuously held (see [[db-lock-holder-unknown]]). Uses a
    /// throwaway local mutex (not the process-wide `HOLDER` static) so it
    /// can't be perturbed by unrelated tests' own `Store::db()` calls.
    #[test]
    fn holder_compare_and_clear_never_wipes_a_fresher_entry() {
        let slot: parking_lot::Mutex<Option<db_lock::Entry>> = parking_lot::Mutex::new(None);
        let entry = |token| db_lock::Entry {
            thread: "t".into(),
            file: "f",
            line: 1,
            since: std::time::Instant::now(),
            token,
        };

        // Our own entry is still there → the clear takes effect.
        *slot.lock() = Some(entry(1));
        db_lock::clear_holder_if_matches_in(&slot, 1);
        assert!(slot.lock().is_none());

        // Someone else already raced in and holds it now (different token)
        // — our late clear must be a no-op, not an overwrite to None.
        *slot.lock() = Some(entry(2));
        db_lock::clear_holder_if_matches_in(&slot, 1);
        let held = slot.lock();
        assert!(held.is_some(), "a fresher holder's entry must survive a stale clear");
        assert_eq!(held.as_ref().unwrap().token, 2);
        drop(held);

        // Already empty → no-op, no panic.
        *slot.lock() = None;
        db_lock::clear_holder_if_matches_in(&slot, 1);
        assert!(slot.lock().is_none());
    }
}
