//! Global gates bounding how much bulk disk I/O the app's own post-processing
//! generates at once.
//!
//! All recordings live on one drive (typically a USB-enclosure HDD — see the
//! `FsProbes` doc in `ui.rs` for the observed 60s stat stalls). Live captures
//! already write to it continuously; without these gates a stream-end burst
//! (N takes finishing together, or N leftover finalizes at app launch) piles
//! N concurrent full-file ffmpeg passes — remuxes, head concats, embeds, CDN
//! recovery muxes — on top of the captures. That interleaved multi-stream
//! read+write load is the access pattern that has knocked the user's USB
//! enclosure off the bus entirely.
//!
//! Two gates, acquired inside the leaf mux functions themselves (so no
//! spawn/call site can forget them):
//!
//! - [`local_pass`] (1 permit): local full-file passes over multi-GB files —
//!   TS→MKV promote remuxes, head+live concats, thumbnail/subtitle embeds.
//!   One at a time; queued passes just finish later (a finished take sitting
//!   as `.ts` in `.cache\` a few minutes longer is harmless).
//! - [`cdn_mux`] (2 permits): CDN-fed muxes (head backfills, VOD recoveries).
//!   These write at network speed — individually gentler than a local pass,
//!   but unbounded stacking (N mute detections minutes after a shared stream
//!   end) still saturates the drive.
//!
//! The module also owns the post-processing read-rate throttle: ffmpeg ≥ 5.0
//! accepts `-readrate N` (read input at N× media rate), which caps a `-c copy`
//! pass's disk rate end-to-end. [`readrate`] returns the configured multiplier
//! (persisted as [`K_POSTPROC_READRATE`]); when an older ffmpeg rejects the
//! flag the leaf functions call [`mark_readrate_unsupported`] and retry
//! without it, once, process-wide.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::info;

/// Settings key for the post-processing disk throttle (multiplier of media
/// rate, stored as a decimal string; `0` = unthrottled).
pub const K_POSTPROC_READRATE: &str = "postproc_readrate";

/// Default `-readrate` multiplier: 30× media rate. A 6 Mbps stream remuxes at
/// ~22 MB/s read + ~22 MB/s write — a 13 GB take finishes in ~10 minutes while
/// leaving most of the drive's bandwidth to live captures.
pub const DEFAULT_READRATE: f64 = 30.0;

// ===== Per-disk limits =====

fn d_local_permits() -> u32 {
    1
}
fn d_cdn_permits() -> u32 {
    2
}
fn d_readrate() -> f64 {
    DEFAULT_READRATE
}

/// The tunable I/O limits for one disk (or the default for all disks).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskLimits {
    /// Concurrent local full-file ffmpeg passes (remux/concat/embed/merge).
    /// The CEILING the adjuster grows toward when `dynamic` is on; the fixed
    /// value used directly otherwise.
    #[serde(default = "d_local_permits")]
    pub local_permits: u32,
    /// Concurrent CDN-fed muxes (head backfill, VOD recovery). Same ceiling
    /// semantics as `local_permits` when `dynamic` is on.
    #[serde(default = "d_cdn_permits")]
    pub cdn_permits: u32,
    /// ffmpeg `-readrate` multiplier for local passes; 0 = unthrottled.
    /// Unaffected by `dynamic` — permit counts only.
    #[serde(default = "d_readrate")]
    pub readrate: f64,
    /// yt-dlp `--limit-rate` for non-live downloads landing on this disk
    /// (e.g. `4M`, `500K`); empty = unlimited.
    #[serde(default)]
    pub rate_limit: String,
    /// When true, `local_permits`/`cdn_permits` above become ceilings and the
    /// background adjuster (see `start_dynamic_adjuster`) grows/shrinks the
    /// LIVE permit count toward them based on the disk's actual queue depth
    /// — idle disk grows slowly toward the ceiling, real contention backs
    /// off immediately. Default false preserves every existing config.
    #[serde(default)]
    pub dynamic: bool,
    /// Emergency measure: block new `local_pass` admissions on this drive
    /// (concat/remux/embeds — all deferrable, a finished take sitting a few
    /// minutes longer as `.ts` is harmless) so a disk crisis leaves every
    /// byte of I/O to the URGENT `cdn_mux` gate (gap recovery, head-backfill
    /// fetch, VOD recovery — all racing a CDN window or DMCA mute) and to
    /// live captures themselves. Never touches `cdn_mux`, and never touches
    /// a `local_pass` job already running — see `local_pass`'s doc comment
    /// for why this can only be admission control, not preemption. Default
    /// false preserves every existing config.
    #[serde(default)]
    pub paused: bool,
}

impl Default for DiskLimits {
    fn default() -> Self {
        DiskLimits {
            local_permits: d_local_permits(),
            cdn_permits: d_cdn_permits(),
            readrate: d_readrate(),
            rate_limit: String::new(),
            dynamic: false,
            paused: false,
        }
    }
}

/// The whole per-disk configuration: a default + per-drive-letter overrides.
/// Persisted as JSON under [`K_DISK_LIMITS`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskLimitsConfig {
    #[serde(default)]
    pub default: DiskLimits,
    /// Uppercase drive letter (e.g. `"A"`) → overrides for that disk.
    #[serde(default)]
    pub drives: std::collections::HashMap<String, DiskLimits>,
}

/// Settings key for [`DiskLimitsConfig`] (JSON).
pub const K_DISK_LIMITS: &str = "disk_io_limits";

static DISK_CFG: parking_lot::RwLock<Option<DiskLimitsConfig>> = parking_lot::RwLock::new(None);

/// Install the per-disk limits (startup from the persisted setting + settings
/// save), and immediately resize every gate that already exists to match.
///
/// The permit-count sync itself lives inside [`gate_sem`], which normally
/// only runs when a NEW pass starts — an already-queued pass just sits
/// parked on `Semaphore::acquire`, so it never re-triggers that check.
/// During exactly the scenario this setting is meant for (draining a large
/// stuck backlog with nothing new queuing), that meant a raised limit could
/// sit installed in [`DISK_CFG`] but never actually reach the semaphore the
/// backlog is blocked on — the change looked like it did nothing. Walking
/// the existing gate map here and re-running `gate_sem` for each key closes
/// that gap: `add_permits` (the growth path) wakes already-queued waiters
/// immediately, in FIFO order, same as any other permit release.
pub fn set_disk_limits(cfg: DiskLimitsConfig) {
    modify_disk_limits(|c| *c = cfg);
}

/// Atomically read-modify-write the disk limits config — `f` runs with the
/// write lock held for the whole operation, unlike a [`disk_limits_config`]
/// read followed by a separate [`set_disk_limits`] write (a caller that does
/// the two as separate steps has a TOCTOU race window: a second concurrent
/// modify-write in between gets silently discarded when the first one's
/// write finally lands). Also resizes existing gates, same as
/// `set_disk_limits` — see its doc comment for why.
pub fn modify_disk_limits(f: impl FnOnce(&mut DiskLimitsConfig)) {
    let cfg = {
        let mut guard = DISK_CFG.write();
        let mut cfg = guard.take().unwrap_or_default();
        f(&mut cfg);
        *guard = Some(cfg.clone());
        cfg
    };
    // Resize using THIS call's own just-committed snapshot, not a fresh
    // re-read per key — two `modify_disk_limits` calls from different
    // threads (e.g. two tests, each touching a different drive) can each
    // trigger a resize sweep that touches an innocent-bystander key neither
    // of them actually changed (every sweep walks every gate, not just the
    // key it cares about). `DISK_CFG`'s write lock strictly orders the two
    // COMMITS, but a fresh `limits_for_key` re-read per key during the sweep
    // is a separate, unlocked step — a later commit's sweep could still read
    // a value, get preempted, and apply it AFTER an earlier commit's sweep
    // already applied a newer one, resurrecting a stale permit count. Using
    // one fixed snapshot for the whole sweep avoids that: since commits are
    // ordered, this call's snapshot is always at least as fresh as anything
    // already applied, so it can never regress a value another call set.
    resize_existing_gates(&LOCAL_GATES, &cfg, |l| l.local_permits);
    resize_existing_gates(&CDN_GATES, &cfg, |l| l.cdn_permits);
}

/// Re-run the growth/shrink logic in [`gate_sem`] for every drive key that
/// already has a gate, against the just-installed [`DISK_CFG`]. A no-op
/// until the first pass on a given map ever runs (nothing to resize yet).
///
/// Needs no special-casing for dynamic-mode drives: `gate_sem` itself is a
/// no-op (resize-wise) on an entry already flagged dynamic, so a save that
/// leaves a drive in dynamic mode can't fight the background adjuster for
/// control of its live permit count. A save that flips a drive OUT of
/// dynamic mode DOES resize it here, back to the static ceiling — handing
/// control back — and any leftover manual pin for that drive is cleared so
/// it doesn't silently reappear if dynamic mode is re-enabled later.
fn resize_existing_gates(
    map: &'static OnceLock<GateMap>,
    cfg: &DiskLimitsConfig,
    want: impl Fn(&DiskLimits) -> u32,
) {
    let Some(m) = map.get() else { return };
    let keys: Vec<String> = m.lock().keys().cloned().collect();
    for key in keys {
        let limits = cfg.drives.get(&key).cloned().unwrap_or_else(|| cfg.default.clone());
        gate_sem(map, &key, want(&limits) as usize, limits.dynamic);
        if !limits.dynamic {
            clear_dynamic_pin(&key);
        }
    }
}

/// The current config (for the Settings editor).
pub fn disk_limits_config() -> DiskLimitsConfig {
    DISK_CFG.read().clone().unwrap_or_default()
}

/// Uppercase drive-letter key for a path (`"A"`), or `"*"` for UNC/relative
/// paths (they share one bucket).
fn drive_key(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    let mut ch = s.chars();
    match (ch.next(), ch.next()) {
        (Some(l), Some(':')) if l.is_ascii_alphabetic() => l.to_ascii_uppercase().to_string(),
        _ => "*".to_string(),
    }
}

/// The effective limits for the disk `path` lives on.
pub fn limits_for(path: &std::path::Path) -> DiskLimits {
    limits_for_key(&drive_key(path))
}

/// Whether new `local_pass` admissions are currently blocked on the drive
/// `path` lives on — read fresh on every check (same as `limits_for`), so a
/// pause flip via Settings or the Background tab takes effect on the very
/// next poll, no separate push/wake path needed.
fn is_local_paused(path: &std::path::Path) -> bool {
    limits_for(path).paused
}

/// The effective limits for a drive-letter key (`"A"`, or `"*"` for
/// UNC/relative paths) directly — used by [`limits_for`] and by the
/// existing-gate resize on a settings save, which already has the key from
/// the gate map and no path to re-derive it from.
fn limits_for_key(key: &str) -> DiskLimits {
    let cfg = DISK_CFG.read();
    let Some(cfg) = cfg.as_ref() else { return DiskLimits::default() };
    cfg.drives.get(key).cloned().unwrap_or_else(|| cfg.default.clone())
}

// ===== Gates (per drive) =====

struct GateEntry {
    sem: Arc<Semaphore>,
    permits: usize,
    /// Single source of truth for who owns this gate's live permit count:
    /// the background adjuster (true) or the static config (false). Synced
    /// from `DiskLimits.dynamic` on every `gate_sem` call — re-derived, not
    /// cached stale, so it can't drift out of sync with the persisted
    /// config's own flag.
    dynamic: bool,
}

type GateMap = parking_lot::Mutex<std::collections::HashMap<String, GateEntry>>;
static LOCAL_GATES: OnceLock<GateMap> = OnceLock::new();
static CDN_GATES: OnceLock<GateMap> = OnceLock::new();

/// The app's Tokio runtime handle, registered once by `AppCore::new` right
/// after it builds the runtime. Lets the shrink path below spawn its permit-
/// reclaim task from ANY calling thread — notably the egui/UI thread, which
/// runs outside the runtime and would panic on a bare `tokio::spawn` (no
/// reactor entered there). `local_pass`/`cdn_mux` themselves always run as
/// spawned async tasks already inside the runtime, so this is only load-
/// bearing for [`set_disk_limits`]'s existing-gate resize on a settings save.
static RT_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Register the runtime handle (see [`RT_HANDLE`]). Call once, right after
/// building the runtime and before the UI can reach a settings save.
pub fn set_runtime_handle(handle: tokio::runtime::Handle) {
    let _ = RT_HANDLE.set(handle);
}

/// Growth (immediate `add_permits`, waking any pass already parked in
/// `Semaphore::acquire` in FIFO order) / shrink (background reclaim, takes
/// effect as running passes finish) to `want` permits on an existing entry —
/// the mutation core shared by the static config's resize path and the
/// dynamic adjuster's [`set_dynamic_permits`].
fn apply_target(e: &mut GateEntry, want: usize) {
    match want.cmp(&e.permits) {
        std::cmp::Ordering::Greater => {
            e.sem.add_permits(want - e.permits);
            e.permits = want;
        }
        std::cmp::Ordering::Less => {
            let take = (e.permits - want) as u32;
            let sem = e.sem.clone();
            e.permits = want;
            let reclaim = async move {
                if let Ok(p) = sem.acquire_many_owned(take).await {
                    p.forget();
                }
            };
            match RT_HANDLE.get() {
                Some(h) => {
                    h.spawn(reclaim);
                }
                None => {
                    tokio::spawn(reclaim);
                }
            }
        }
        std::cmp::Ordering::Equal => {}
    }
}

/// Get (or create) the semaphore for `key`. For a STATIC drive
/// (`dynamic=false`), also adjusts its permit count to `want` via
/// [`apply_target`] — called on every new acquisition, AND on every settings
/// save via [`set_disk_limits`]'s resize of already-existing gates, so a
/// stuck backlog doesn't have to wait for some unrelated new pass to start
/// before a raised limit reaches it. For a DYNAMIC drive, `want` is ignored
/// entirely and the live permit count is left untouched — it's owned by the
/// background adjuster (see [`start_dynamic_adjuster`]/[`set_dynamic_permits`]),
/// and letting a mere new acquisition reset it back to the static ceiling
/// would silently undo whatever the adjuster had backed it off to.
///
/// `dynamic` is resynced from the caller's current read of `DiskLimits` on
/// every call (never cached stale) — the single source of truth for which
/// mode a gate is in lives on [`GateEntry`] itself, so the two behaviors
/// above can never disagree about which one applies.
fn gate_sem(map: &'static OnceLock<GateMap>, key: &str, want: usize, dynamic: bool) -> Arc<Semaphore> {
    let want = want.max(1);
    let m = map.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut g = m.lock();
    let e = g.entry(key.to_string()).or_insert_with(|| GateEntry {
        // Dynamic gates slow-start at 1 permit and let the adjuster prove
        // it's safe to grow, rather than jumping straight to the ceiling.
        sem: Arc::new(Semaphore::new(if dynamic { 1 } else { want })),
        permits: if dynamic { 1 } else { want },
        dynamic,
    });
    e.dynamic = dynamic;
    if e.dynamic {
        return e.sem.clone();
    }
    apply_target(e, want);
    e.sem.clone()
}

/// Directly set a DYNAMIC drive's live permit count to `target` — the
/// adjuster's own entry point, bypassing `DISK_CFG` entirely (the computed
/// value is live-only; the user's configured ceiling must stay untouched so
/// Settings always shows the real configured max, not the adjuster's current
/// guess). A no-op if the gate doesn't exist yet or isn't in dynamic mode
/// (e.g. a settings save flipped it back to static in the same instant).
fn set_dynamic_permits(map: &'static OnceLock<GateMap>, key: &str, target: usize) {
    let Some(m) = map.get() else { return };
    if let Some(e) = m.lock().get_mut(key)
        && e.dynamic
    {
        apply_target(e, target.max(1));
    }
}

/// Live picture of one drive's dynamic-mode gate, for the Settings UI's
/// "actual vs configured" display.
pub struct DynGateStatus {
    /// Permits currently installed on the live semaphore — the adjuster's
    /// (or, briefly during a mode transition, the static config's) current
    /// target. This is the "actual" number.
    pub current: u32,
    /// Of those, how many are checked out (a pass is actively running) right now.
    pub in_use: u32,
    /// The user's configured ceiling (`local_permits`/`cdn_permits`) the
    /// adjuster grows toward.
    pub ceiling: u32,
}

fn dyn_status(
    map: &'static OnceLock<GateMap>,
    letter: char,
    ceiling_of: impl Fn(&DiskLimits) -> u32,
) -> Option<DynGateStatus> {
    let m = map.get()?;
    let key = letter.to_ascii_uppercase().to_string();
    let (current, in_use) = {
        let g = m.lock();
        let e = g.get(&key)?;
        let in_use = e.permits.saturating_sub(e.sem.available_permits());
        (e.permits as u32, in_use as u32)
    };
    let ceiling = ceiling_of(&limits_for_key(&key));
    Some(DynGateStatus { current, in_use, ceiling })
}

/// Live status of drive `letter`'s local-passes gate, or `None` if nothing
/// has run there yet (no gate created — the UI should show "not active yet",
/// not "0/0").
pub fn local_dyn_status(letter: char) -> Option<DynGateStatus> {
    dyn_status(&LOCAL_GATES, letter, |l| l.local_permits)
}

/// Drive letters that currently have a gate of either kind — i.e. drives the
/// app has actually done bulk I/O on since launch. Lets the Settings UI's
/// Default row (which covers every drive without an override row) list a live
/// readout per real disk instead of showing nothing.
pub fn active_gate_letters() -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for map in [&LOCAL_GATES, &CDN_GATES] {
        if let Some(m) = map.get() {
            set.extend(m.lock().keys().cloned());
        }
    }
    set.into_iter().collect()
}

/// Live status of drive `letter`'s CDN-mux gate — see [`local_dyn_status`].
pub fn cdn_dyn_status(letter: char) -> Option<DynGateStatus> {
    dyn_status(&CDN_GATES, letter, |l| l.cdn_permits)
}

/// Session-only manual override of a drive's dynamic-mode permit counts —
/// "in-flight" per the feature's own name, never persisted (a restart always
/// resumes under the adjuster's own judgment). Keyed by uppercase drive
/// letter; `(local pin, cdn pin)` — `None` in either slot means "let the
/// adjuster decide" for that gate kind.
type DynamicPinMap = parking_lot::Mutex<std::collections::HashMap<String, (Option<u32>, Option<u32>)>>;
static DYNAMIC_PIN: OnceLock<DynamicPinMap> = OnceLock::new();
fn dynamic_pin_map() -> &'static DynamicPinMap {
    DYNAMIC_PIN.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Pin a manual override for one drive's dynamic permit counts (or clear one
/// by passing `None`). Takes effect on the adjuster's next tick (within
/// [`DYNAMIC_TICK_SECS`]). Passing `None` for both clears the whole entry.
pub fn pin_dynamic_permits(letter: &str, local: Option<u32>, cdn: Option<u32>) {
    let key = letter.trim().to_uppercase();
    if key.is_empty() {
        return;
    }
    if local.is_none() && cdn.is_none() {
        dynamic_pin_map().lock().remove(&key);
    } else {
        dynamic_pin_map().lock().insert(key, (local, cdn));
    }
}

/// The current manual pin for a drive, if any — `(local pin, cdn pin)`.
pub fn dynamic_pin_for(letter: &str) -> (Option<u32>, Option<u32>) {
    dynamic_pin_map().lock().get(&letter.trim().to_uppercase()).copied().unwrap_or((None, None))
}

/// Drop any manual pin for `key` (a drive-letter key, not a raw user string
/// — see [`resize_existing_gates`], which calls this when a drive exits
/// dynamic mode so a stale pin can't silently reappear if it's re-enabled).
fn clear_dynamic_pin(key: &str) {
    dynamic_pin_map().lock().remove(key);
}

// ===== Dynamic-mode background adjuster =====

/// How often the dynamic adjuster re-evaluates each dynamic-mode drive.
const DYNAMIC_TICK_SECS: u64 = 5;
/// Consecutive low-queue ticks required before growing by 1 permit — slow
/// enough that a single quiet second doesn't ramp concurrency straight back
/// up on a drive that's only momentarily caught its breath.
const DYNAMIC_GROW_STREAK: u32 = 2;
/// `queue_depth` at/below this counts as "idle enough to grow".
const DYNAMIC_LOW_QUEUE: u32 = 1;
/// `queue_depth` at/above this triggers an immediate backoff. Between this
/// and `DYNAMIC_LOW_QUEUE` is a dead zone: hold steady, don't grow off a
/// borderline reading.
const DYNAMIC_HIGH_QUEUE: u32 = 3;

/// One adjustment step for a dynamic-mode gate. Additive growth (slow-start,
/// +1 after `DYNAMIC_GROW_STREAK` consecutive idle ticks, mirrors `gate_sem`'s
/// own grow semantics) toward `ceiling`; MULTIPLICATIVE backoff (halve
/// toward the floor of 1) the instant the disk shows real contention —
/// fast enough to matter for a drive that's already been knocked off the bus
/// once (see the module doc's USB-enclosure incident); an additive-by-1
/// backoff would take too long to help at a ceiling of 6-8. `queue_depth` is
/// whole-spindle (any process, not just this app) and instantaneous — the
/// same "is this disk actually busy" signal Task Manager's per-disk active
/// time reflects.
fn dynamic_step(current: u32, queue_depth: u32, low_streak: &mut u32, ceiling: u32) -> u32 {
    let ceiling = ceiling.max(1);
    if queue_depth <= DYNAMIC_LOW_QUEUE {
        *low_streak += 1;
        if *low_streak >= DYNAMIC_GROW_STREAK {
            (current + 1).min(ceiling)
        } else {
            current
        }
    } else if queue_depth >= DYNAMIC_HIGH_QUEUE {
        *low_streak = 0;
        (current / 2).max(1)
    } else {
        *low_streak = 0; // dead zone
        current
    }
}

/// Per-drive-key consecutive-low-queue streak, for `dynamic_step`'s slow-
/// start growth condition. One shared map across both gate kinds (keyed by
/// `"{L or C}:{drive}"`) since local and CDN ramp independently.
static LOW_STREAK: OnceLock<parking_lot::Mutex<std::collections::HashMap<String, u32>>> = OnceLock::new();
fn low_streak_map() -> &'static parking_lot::Mutex<std::collections::HashMap<String, u32>> {
    LOW_STREAK.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Start the dynamic-limits background adjuster (idempotent — safe to call
/// more than once, only the first call spawns anything). A dedicated
/// `std::thread`, not a Tokio task: `platform::disk_performance` is a cheap
/// synchronous IOCTL, and this mirrors `iomon::start_sampler`'s own
/// precedent for exactly this kind of periodic-disk-poll work — no tokio
/// worker or `spawn_blocking` overhead for a couple of syscalls every few
/// seconds. Deliberately independent of `iomon.rs`: polls the same cheap
/// primitive directly, scoped only to drives that already have a gate (i.e.
/// drives the app is actually doing bulk I/O on), so it never needs to know
/// what iomon happens to be sampling. Fire-and-forget for the process
/// lifetime, same as the sampler.
pub fn start_dynamic_adjuster() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new().name("io-gate-dynamic".into()).spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(DYNAMIC_TICK_SECS));
        dynamic_tick_for(&LOCAL_GATES, "L", |l| l.local_permits, |p| p.0);
        dynamic_tick_for(&CDN_GATES, "C", |l| l.cdn_permits, |p| p.1);
    });
}

/// One adjuster pass over every dynamic-mode drive currently gated on `map`.
fn dynamic_tick_for(
    map: &'static OnceLock<GateMap>,
    streak_prefix: &str,
    ceiling_of: impl Fn(&DiskLimits) -> u32,
    pin_of: impl Fn(&(Option<u32>, Option<u32>)) -> Option<u32>,
) {
    let Some(m) = map.get() else { return };
    let keys: Vec<String> = m.lock().keys().cloned().collect();
    for key in keys {
        if key == "*" {
            continue; // UNC/relative bucket — no physical disk to poll
        }
        let limits = limits_for_key(&key);
        if !limits.dynamic {
            continue;
        }
        let target = match pin_of(&dynamic_pin_for(&key)) {
            Some(pinned) => pinned,
            None => {
                let queue_depth = key
                    .chars()
                    .next()
                    .and_then(crate::platform::disk_performance)
                    .map(|d| d.queue_depth)
                    .unwrap_or(0);
                let current = m.lock().get(&key).map(|e| e.permits as u32).unwrap_or(1);
                let streak_key = format!("{streak_prefix}:{key}");
                let mut streaks = low_streak_map().lock();
                let streak = streaks.entry(streak_key).or_insert(0);
                dynamic_step(current, queue_depth, streak, ceiling_of(&limits))
            }
        };
        set_dynamic_permits(map, &key, target as usize);
    }
}

/// `(token, label, drive key, started, PID once known)` — one entry per
/// local-gate permit currently held.
type HolderEntry = (u64, String, String, Instant, Option<u32>);

/// Live status of the local gates, for progress messages: every pass holding
/// a permit right now (label + drive key + seconds held + PID once its
/// caller has spawned a child, longest-held first) and every pass queued
/// (label + drive + seconds waiting, in arrival order). The PID is `None`
/// until the caller (which acquires the gate BEFORE spawning its ffmpeg
/// child) calls back in via [`LocalPass::set_pid`] — see [`kill_local_holder`].
static LOCAL_HOLDERS: parking_lot::Mutex<Vec<HolderEntry>> = parking_lot::Mutex::new(Vec::new());
static LOCAL_WAITERS: parking_lot::Mutex<Vec<(u64, String, String, Instant)>> =
    parking_lot::Mutex::new(Vec::new());
static NEXT_GATE_TOKEN: AtomicU64 = AtomicU64::new(1);

/// `(holders (label, seconds held) longest-first, queue length)` — global
/// across every drive, for the general-purpose progress message
/// ([`wait_info`]). See [`local_gate_status_by_drive`] for a per-drive
/// breakdown (what the Background tab's emergency controls need).
pub fn local_gate_status() -> (Vec<(String, u64)>, usize) {
    let mut holders: Vec<(String, u64)> = LOCAL_HOLDERS
        .lock()
        .iter()
        .map(|(_, l, _, t, _)| (l.clone(), t.elapsed().as_secs()))
        .collect();
    holders.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
    (holders, LOCAL_WAITERS.lock().len())
}

/// `(drive key, holders (label, seconds held) longest-held first, queue
/// length for that drive)`.
pub type DriveGateStatus = (String, Vec<(String, u64)>, usize);

/// Every drive that currently has local-gate activity (a holder or a
/// waiter), each with its own holders/queue-length breakdown — drives
/// sorted alphabetically. Powers the Background tab's per-drive emergency
/// pause/kill controls: a global-only view can't tell you which drive to
/// pause.
pub fn local_gate_status_by_drive() -> Vec<DriveGateStatus> {
    let mut by_drive: std::collections::BTreeMap<String, (Vec<(String, u64)>, usize)> =
        Default::default();
    for (_, label, drive, t, _) in LOCAL_HOLDERS.lock().iter() {
        by_drive.entry(drive.clone()).or_default().0.push((label.clone(), t.elapsed().as_secs()));
    }
    for (_, _, drive, _) in LOCAL_WAITERS.lock().iter() {
        by_drive.entry(drive.clone()).or_default().1 += 1;
    }
    by_drive
        .into_iter()
        .map(|(drive, (mut holders, queued))| {
            holders.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
            (drive, holders, queued)
        })
        .collect()
}

/// Every pass currently queued on a local gate — `(label, drive key,
/// seconds waiting)`, longest-waiting (= next in line per drive) first.
pub fn local_gate_queue() -> Vec<(String, String, u64)> {
    let mut v: Vec<(String, String, u64)> = LOCAL_WAITERS
        .lock()
        .iter()
        .map(|(_, l, d, t)| (l.clone(), d.clone(), t.elapsed().as_secs()))
        .collect();
    v.sort_by_key(|(.., s)| std::cmp::Reverse(*s));
    v
}

/// Held permit of a local gate; removes its holder entry on drop.
pub struct LocalPass {
    token: u64,
    _permit: OwnedSemaphorePermit,
}

impl LocalPass {
    /// Record the PID of the child process this pass is guarding, once the
    /// caller has spawned it (the gate is always acquired first) — lets
    /// [`kill_local_holder`] find and terminate it. A no-op if the holder
    /// entry is somehow already gone (dropped before the caller got around
    /// to calling this — shouldn't happen in practice, but this must never
    /// panic on a UI-adjacent path).
    pub fn set_pid(&self, pid: u32) {
        if let Some(entry) = LOCAL_HOLDERS.lock().iter_mut().find(|(t, ..)| *t == self.token) {
            entry.4 = Some(pid);
        }
    }
}

impl Drop for LocalPass {
    fn drop(&mut self) {
        LOCAL_HOLDERS.lock().retain(|(t, ..)| *t != self.token);
    }
}

/// Force-terminate every `local_pass` job currently holding a permit on
/// `drive` (uppercase drive-letter key, or `"*"`) — the "Kill current"
/// emergency action. Returns how many process trees were signalled.
/// Admission-control-only pause can't free a drive that's ALREADY busy when
/// you hit the emergency button; this is the deliberate, explicit
/// complement to that — see `DiskLimits::paused`'s doc comment. Doesn't
/// touch the holder-tracking entries itself: those clear the normal way
/// (via `LocalPass::drop`) once the killed process's `child.wait()`
/// resolves and the owning function's existing ffmpeg-failure handling
/// runs — a kill looks exactly like any other ffmpeg crash to that code, no
/// new error path needed. A holder with no known PID yet (killed in the
/// brief window between acquiring the gate and finishing its own spawn) is
/// silently skipped — nothing to kill yet, and it'll show up on a later
/// call once it has spawned.
pub fn kill_local_holder(drive: &str) -> usize {
    let pids: Vec<u32> = LOCAL_HOLDERS
        .lock()
        .iter()
        .filter(|(_, _, d, _, _)| d == drive)
        .filter_map(|(.., pid)| *pid)
        .collect();
    for &pid in &pids {
        crate::platform::kill_process_tree(pid);
    }
    pids.len()
}

/// Removes the waiter entry even when the acquiring future is dropped
/// mid-await (task aborted at shutdown).
struct WaitingGuard(u64);
impl Drop for WaitingGuard {
    fn drop(&mut self) {
        LOCAL_WAITERS.lock().retain(|(t, ..)| *t != self.0);
    }
}

/// How often a paused drive re-checks whether it's been unpaused. Short
/// enough that an emergency toggle feels immediate; a plain boolean read is
/// essentially free at this cadence.
const PAUSE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Acquire the local-passes gate of the disk `path` lives on (permit count
/// per Settings → per-disk I/O limits; default 1). Hold the returned guard
/// for the duration of the ffmpeg run; drop to release.
///
/// While the drive is paused (`DiskLimits::paused`), blocks BEFORE even
/// entering the waiter list — an emergency pause is meant to give the
/// `cdn_mux` gate (and live captures) the whole drive, so a deferred pass
/// must not sit inside the semaphore's own FIFO wait queue where a pause
/// can no longer un-queue it; it must never get that far to begin with.
/// This can only ever be admission control: a pass that already holds the
/// gate before pause was flipped on keeps running to completion (see
/// `kill_local_holder` for the deliberate, separate way to interrupt one).
pub async fn local_pass(label: &str, path: &std::path::Path) -> LocalPass {
    while is_local_paused(path) {
        tokio::time::sleep(PAUSE_POLL_INTERVAL).await;
    }
    let key = drive_key(path);
    let limits = limits_for(path);
    let sem = gate_sem(&LOCAL_GATES, &key, limits.local_permits as usize, limits.dynamic);
    let token = NEXT_GATE_TOKEN.fetch_add(1, Ordering::Relaxed);
    LOCAL_WAITERS.lock().push((token, label.to_string(), key.clone(), Instant::now()));
    let _wg = WaitingGuard(token);
    let start = Instant::now();
    let permit = sem.acquire_owned().await.expect("io gate semaphore closed");
    let waited = start.elapsed();
    if waited.as_secs() >= 5 {
        info!(
            "disk gate [{key}]: {label} waited {}s for its turn (other bulk passes running)",
            waited.as_secs()
        );
    }
    LOCAL_HOLDERS.lock().push((token, label.to_string(), key, Instant::now(), None));
    LocalPass { token, _permit: permit }
}

/// [`local_pass`], but invokes `on_wait(waited_secs, holders, queue_len,
/// paused)` every 5 s while queued — callers surface it as task progress so
/// a queued pass is visibly waiting (and on what — or that it's parked on
/// an emergency pause, not genuine contention) instead of looking stale.
pub async fn local_pass_with_progress(
    label: &str,
    path: &std::path::Path,
    mut on_wait: impl FnMut(u64, Vec<(String, u64)>, usize, bool),
) -> LocalPass {
    let fut = local_pass(label, path);
    tokio::pin!(fut);
    let started = Instant::now();
    loop {
        tokio::select! {
            p = &mut fut => return p,
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                let (holders, waiting) = local_gate_status();
                on_wait(started.elapsed().as_secs(), holders, waiting, is_local_paused(path));
            }
        }
    }
}

/// `"{kind}: {file stem}"` (stem truncated) — gate labels carry the file so
/// "running now: remux: Vienna - 2026-07-11 …" answers WHAT holds the gate.
pub fn gate_label(kind: &str, path: &std::path::Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        return kind.to_string();
    }
    let short: String = stem.chars().take(48).collect();
    let ellipsis = if short.len() < stem.len() { "…" } else { "" };
    format!("{kind}: {short}{ellipsis}")
}

/// Standard human line for a queued pass's progress info, e.g.
/// `⏳ queued for disk gate 45s — running now: remux: Vienna… (312s) · 3 in queue`.
pub fn wait_info(waited_secs: u64, holders: Vec<(String, u64)>, waiting: usize) -> String {
    let mut s = format!("⏳ queued for disk gate {waited_secs}s");
    match holders.first() {
        Some((label, held)) => {
            s.push_str(&format!(" — running now: {label} ({held}s)"));
            if holders.len() > 1 {
                s.push_str(&format!(" +{} more", holders.len() - 1));
            }
        }
        None => s.push_str(" — gate turning over"),
    }
    if waiting > 1 {
        s.push_str(&format!(" · {waiting} in queue"));
    }
    s
}

/// Progress-info line while parked on an emergency pause, not genuine
/// contention — distinct from [`wait_info`] so a paused drive doesn't read
/// as "stuck" when it's deliberately held.
pub fn paused_wait_info(waited_secs: u64) -> String {
    format!("⏸ paused on this drive ({waited_secs}s) — will resume once unpaused")
}

/// Acquire the CDN-mux gate of the disk `path` lives on (permit count per
/// Settings → per-disk I/O limits; default 2). Head backfills and VOD
/// recoveries write at network speed — gentler than a local pass, but
/// unbounded stacking still saturates a drive.
pub async fn cdn_mux(label: &str, path: &std::path::Path) -> OwnedSemaphorePermit {
    let key = drive_key(path);
    let limits = limits_for(path);
    let sem = gate_sem(&CDN_GATES, &key, limits.cdn_permits as usize, limits.dynamic);
    let start = Instant::now();
    let permit = sem.acquire_owned().await.expect("io gate semaphore closed");
    let waited = start.elapsed();
    if waited.as_secs() >= 5 {
        info!(
            "disk gate [{key}]: {label} waited {}s for its turn (other CDN muxes running)",
            waited.as_secs()
        );
    }
    permit
}

/// Set once if the installed ffmpeg rejects `-readrate` (pre-5.0).
static READRATE_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// The `-readrate` multiplier for a local post-processing ffmpeg pass writing
/// to `path`'s disk, or `None` when disabled for that disk or known-unsupported
/// by the installed ffmpeg.
pub fn readrate_for(path: &std::path::Path) -> Option<f64> {
    if READRATE_UNSUPPORTED.load(Ordering::Relaxed) {
        return None;
    }
    let r = limits_for(path).readrate;
    (r > 0.0).then_some(r)
}

/// Remember (process-wide) that this ffmpeg build rejects `-readrate`.
pub fn mark_readrate_unsupported() {
    if !READRATE_UNSUPPORTED.swap(true, Ordering::Relaxed) {
        info!("ffmpeg does not support -readrate (needs ffmpeg >= 5.0) — post-processing throttle disabled");
    }
}

/// Settings key for the LEGACY global download rate limit (yt-dlp
/// `--limit-rate` syntax). Still persisted; seeds [`DiskLimitsConfig`]'s
/// default when the per-disk config has never been saved.
pub const K_DOWNLOAD_RATE_LIMIT: &str = "download_rate_limit";

/// The `--limit-rate` value for a non-live yt-dlp download landing on
/// `path`'s disk (VOD-archive grabs + Videos-tab downloads), or an empty
/// string when unlimited. Live captures are never limited — a capture that
/// can't keep up with the live edge loses data. Applies to downloads
/// *started* after a settings change; in-flight ones keep their launch args.
pub fn rate_limit_for(path: &std::path::Path) -> String {
    limits_for(path).rate_limit.trim().to_string()
}

/// Settings key for yt-dlp postprocessor args (`--postprocessor-args` specs,
/// `;;`-separated; empty = none, the default). The escape hatch for
/// throttling yt-dlp's INTERNAL ffmpeg passes — e.g. the post-stream SABR
/// format merge reads+writes the whole multi-GB capture at full disk speed,
/// and none of the app-side gates can reach inside the tool. Example:
/// `Merger+ffmpeg_i:-readrate 30` caps merges at 30× realtime.
pub const K_YTDLP_PPA: &str = "ytdlp_postprocessor_args";

static YTDLP_PPA: RwLock<String> = RwLock::new(String::new());

/// Set the yt-dlp postprocessor args (startup + settings save). Applies to
/// tools *started* afterwards.
pub fn set_ytdlp_ppa(v: &str) {
    *YTDLP_PPA.write() = v.trim().to_string();
}

/// The configured `--postprocessor-args` specs (`;;`-separated), or empty.
pub fn ytdlp_ppa() -> String {
    YTDLP_PPA.read().clone()
}

/// Does this ffmpeg stderr indicate the `-readrate` flag itself was rejected
/// (as opposed to the pass genuinely failing)?
pub fn is_readrate_error(stderr: &str) -> bool {
    stderr.contains("readrate")
        && (stderr.contains("Unrecognized option") || stderr.contains("Option not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Tests below share process-global state (`DISK_CFG`, `LOCAL_GATES`/
    /// `CDN_GATES`, `DYNAMIC_PIN`). `resize_existing_gates` deliberately
    /// sweeps EVERY existing gate on every `modify_disk_limits` call — that
    /// IS the point (a raised limit must reach an already-stuck backlog
    /// immediately, not just brand-new work) — which means one test's config
    /// change can transiently touch another test's drive letter too, even
    /// though each test uses its own. In production this never matters:
    /// `modify_disk_limits` only ever has ONE caller at a time (the single-
    /// threaded Settings-save handler, or the dynamic adjuster's own
    /// narrower `set_dynamic_permits`, which bypasses `DISK_CFG` entirely).
    /// But `cargo test` runs these on independent OS threads truly in
    /// parallel, so distinct drive letters alone aren't enough to prevent
    /// interleaving. Serialize just the tests that mutate this shared state
    /// (acquire as the first line) — they still run in parallel with every
    /// other file's tests, just not with each other. Safe to hold across
    /// `.await`: `#[tokio::test]` defaults to a current-thread runtime, so
    /// one test never migrates across OS threads mid-await.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn lock_shared_state() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    async fn local_pass_serializes_per_drive() {
        let _guard = lock_shared_state();
        // Distinct test drive letters so other tests' config doesn't interfere.
        let qa = Path::new(r"Q:\x\a.mkv");
        let a = local_pass("a", qa).await;
        // Same drive: second acquisition must not be immediately available…
        let sem = gate_sem(&LOCAL_GATES, "Q", 1, false);
        assert!(sem.clone().try_acquire_owned().is_err());
        // …but ANOTHER drive's gate is independent.
        let b = local_pass("b", Path::new(r"R:\y\b.mkv")).await;
        // Both show as holders. (No assertion on the waiting count or on
        // total holders — the sibling tests share the global registries.)
        let (holders, _waiting) = local_gate_status();
        assert!(holders.iter().any(|(l, _)| l == "a"));
        assert!(holders.iter().any(|(l, _)| l == "b"));
        drop(a);
        drop(b);
        assert!(sem.try_acquire_owned().is_ok());
        let (holders, _) = local_gate_status();
        assert!(!holders.iter().any(|(l, _)| l == "a" || l == "b"));
    }

    #[tokio::test]
    async fn per_drive_limits_and_permit_adjustment() {
        let _guard = lock_shared_state();
        // `modify_disk_limits`, not a `disk_limits_config()` read followed by
        // a separate `set_disk_limits()` write — `DISK_CFG` is one global
        // shared with every other test in this file, running on independent
        // OS threads truly in parallel, and that two-step pattern has a
        // TOCTOU race: another concurrently-running test's write landing in
        // between gets silently discarded once this one's write completes.
        // Every test here uses its own drive letter, so an ATOMIC
        // modify never conflicts with another test's.
        modify_disk_limits(|cfg| {
            cfg.default.readrate = 30.0;
            cfg.drives.insert(
                "S".into(),
                DiskLimits {
                    local_permits: 2,
                    cdn_permits: 1,
                    readrate: 0.0,
                    rate_limit: "4M".into(),
                    dynamic: false,
                    paused: false,
                },
            );
        });
        let s = Path::new(r"S:\rec\file.ts");
        let t = Path::new(r"T:\rec\file.ts");
        // Per-drive resolution: S is unthrottled with a rate limit, T inherits.
        // (Asserted via limits_for, not readrate_for — the sibling test latches
        // the process-wide readrate-unsupported flag and tests run in parallel.)
        assert_eq!(limits_for(s).readrate, 0.0);
        assert_eq!(rate_limit_for(s), "4M");
        assert_eq!(limits_for(t).readrate, 30.0);
        assert_eq!(rate_limit_for(t), "");
        // S's local gate has 2 permits: two concurrent passes, not three.
        let p1 = local_pass("s1", s).await;
        let p2 = local_pass("s2", s).await;
        let sem = gate_sem(&LOCAL_GATES, "S", 2, false);
        assert!(sem.try_acquire_owned().is_err());
        drop(p1);
        drop(p2);
        // Shrink to 1: gate_sem spawns the reducer; next config read wants 1.
        modify_disk_limits(|cfg| cfg.drives.get_mut("S").unwrap().local_permits = 1);
        let _p = local_pass("s3", s).await;
        tokio::task::yield_now().await; // let the reducer task grab the excess
        let sem = gate_sem(&LOCAL_GATES, "S", 1, false);
        assert!(sem.try_acquire_owned().is_err());
    }

    /// The bug this session fixed: a pass already parked in `Semaphore::acquire`
    /// (queued before a settings save, exactly what a stuck backlog looks like)
    /// must be woken by `set_disk_limits` itself — NOT only by some unrelated
    /// NEW pass starting afterward. Before the fix, raising the limit updated
    /// `DISK_CFG` but never touched the live semaphore the backlog was blocked
    /// on, so "I upped all the values but nothing changed" was literally true.
    #[tokio::test]
    async fn raising_limit_wakes_an_already_queued_pass_with_no_new_caller() {
        let _guard = lock_shared_state();
        let v = Path::new(r"V:\rec\file.ts");
        // `modify_disk_limits` — see the comment in
        // `per_drive_limits_and_permit_adjustment` above (same shared global).
        modify_disk_limits(|cfg| {
            cfg.drives.insert(
                "V".into(),
                DiskLimits {
                    local_permits: 1,
                    cdn_permits: 1,
                    readrate: 0.0,
                    rate_limit: String::new(),
                    dynamic: false,
                    paused: false,
                },
            );
        });

        // Hold the only permit — mirrors the long-running ffmpeg pass at the
        // head of a stuck backlog.
        let held = local_pass("holder", v).await;
        // A second pass enters the queue under the OLD (1-permit) limit and
        // parks on acquire — mirrors the rest of the backlog.
        let queued = tokio::spawn(async move { local_pass("queued", v).await });
        // Give it a few scheduler turns to actually reach `.acquire().await`
        // (not just be spawned-but-unpolled).
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        // Raise the limit — the fix under test: this alone must resize the
        // already-created semaphore, no new `local_pass` call involved.
        modify_disk_limits(|cfg| cfg.drives.get_mut("V").unwrap().local_permits = 2);

        // The queued pass must complete promptly now, without anything else
        // ever calling local_pass("V", ...) again.
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), queued)
            .await
            .expect("queued pass must be woken by the settings save, not time out")
            .expect("task panicked");
        drop(got);
        drop(held);
    }

    #[test]
    fn readrate_unsupported_latch() {
        let p = Path::new(r"U:\x.ts");
        assert!(is_readrate_error("Unrecognized option 'readrate'."));
        assert!(!is_readrate_error("Invalid data found when processing input"));
        mark_readrate_unsupported();
        assert_eq!(readrate_for(p), None);
    }

    #[test]
    fn drive_keys_and_serde() {
        assert_eq!(drive_key(Path::new(r"A:\streams\x.ts")), "A");
        assert_eq!(drive_key(Path::new(r"c:\x.ts")), "C");
        assert_eq!(drive_key(Path::new(r"\\nas\share\x.ts")), "*");
        assert_eq!(drive_key(Path::new("relative/x.ts")), "*");
        // Minimal/old JSON deserializes with defaults filled in.
        let cfg: DiskLimitsConfig =
            serde_json::from_str(r#"{"drives":{"A":{"local_permits":2}}}"#).unwrap();
        assert_eq!(cfg.default, DiskLimits::default());
        let a = &cfg.drives["A"];
        assert_eq!(a.local_permits, 2);
        assert_eq!(a.cdn_permits, 2);
        assert_eq!(a.readrate, DEFAULT_READRATE);
        assert!(!a.dynamic);
    }

    // ── Dynamic mode ──────────────────────────────────────────────────

    #[test]
    fn dynamic_step_grows_slowly_and_backs_off_fast() {
        let mut streak = 0u32;
        let mut cur = 1u32;
        // Tick 1, low queue: streak builds, not yet enough to grow (GROW_STREAK = 2).
        cur = dynamic_step(cur, 0, &mut streak, 4);
        assert_eq!((cur, streak), (1, 1));
        // Tick 2, still low: streak hits the threshold, grows by 1.
        cur = dynamic_step(cur, 0, &mut streak, 4);
        assert_eq!((cur, streak), (2, 2));
        // Tick 3, still low (queue_depth == LOW_QUEUE counts as low too): grows again.
        cur = dynamic_step(cur, 1, &mut streak, 4);
        assert_eq!((cur, streak), (3, 3));
        // Tick 4: dead zone (queue_depth == 2) holds steady AND resets the streak.
        cur = dynamic_step(cur, 2, &mut streak, 4);
        assert_eq!((cur, streak), (3, 0));
        // Tick 5: real contention backs off MULTIPLICATIVELY (halve), not by 1.
        cur = dynamic_step(cur, 3, &mut streak, 4);
        assert_eq!((cur, streak), (1, 0)); // 3 / 2 = 1
        // Never below the floor of 1, however busy.
        assert_eq!(dynamic_step(1, 9, &mut 0, 4), 1);
        // Growth never exceeds the ceiling, however long the idle streak.
        let mut maxed_streak = DYNAMIC_GROW_STREAK;
        assert_eq!(dynamic_step(4, 0, &mut maxed_streak, 4), 4);
    }

    /// The core fix this feature depends on: a NEW acquisition-time call
    /// must never undo the adjuster's current throttling on a dynamic gate
    /// (unlike a static gate, which DOES resize on every acquisition).
    ///
    /// Registers "W" in `DISK_CFG` up front (not just a hardcoded
    /// `dynamic=true` literal passed straight to `gate_sem`) — `resize_
    /// existing_gates` derives `dynamic` from `DISK_CFG`, not from whatever
    /// a caller happens to pass, so if this test's literal disagreed with
    /// the (never-updated) config, a concurrently-running sibling test's own
    /// `modify_disk_limits` call would sweep every key including this one,
    /// see `dynamic=false` in the config, and fight this test for control of
    /// "W" — exactly the split-brain the single-`GateEntry.dynamic`-flag
    /// design is meant to prevent, except the discrepancy would be coming
    /// from this test's setup, not production code (which always derives
    /// `dynamic` from `limits_for`/`limits_for_key`, never a literal).
    /// `#[tokio::test]`: `modify_disk_limits` can need a runtime to spawn a
    /// shrink-reclaim task on this thread.
    #[tokio::test]
    async fn dynamic_gate_ignores_want_on_repeated_acquisition() {
        let _guard = lock_shared_state();
        modify_disk_limits(|cfg| {
            cfg.drives.insert(
                "W".into(),
                DiskLimits {
                    local_permits: 5,
                    cdn_permits: 2,
                    readrate: 0.0,
                    rate_limit: String::new(),
                    dynamic: true,
                    paused: false,
                },
            );
        });
        let sem1 = gate_sem(&LOCAL_GATES, "W", 4, true);
        assert_eq!(sem1.available_permits(), 1, "dynamic gates slow-start at 1, not at `want`");
        // A second "new pass" acquisition with a DIFFERENT `want` (as if the
        // ceiling changed) must not resize an already-created dynamic gate.
        let sem2 = gate_sem(&LOCAL_GATES, "W", 8, true);
        assert_eq!(sem2.available_permits(), 1);
        assert!(Arc::ptr_eq(&sem1, &sem2));
        // Only the adjuster's own entry point may move it.
        set_dynamic_permits(&LOCAL_GATES, "W", 3);
        assert_eq!(sem1.available_permits(), 3);
        // Flip to static via the config (the same path production uses) —
        // local_permits stayed 5 throughout, so this is always a GROWTH
        // (3 -> 5, synchronous `add_permits`, no spawned reclaim task to
        // race against) regardless of whether this test's own resize sweep
        // or a concurrent sibling's gets there first — both converge on 5.
        modify_disk_limits(|cfg| cfg.drives.get_mut("W").unwrap().dynamic = false);
        assert_eq!(local_dyn_status('W').map(|s| s.current), Some(5));
    }

    // `#[tokio::test]`, not plain `#[test]`: `modify_disk_limits` resizes
    // EVERY key in the shared gate maps, not just this test's own — if a
    // concurrently-running test's drive needs a shrink at that moment, the
    // reclaim task's spawn needs a runtime on this thread (no RT_HANDLE is
    // registered in test context, only in AppCore::new for the real app).
    #[tokio::test]
    async fn resize_existing_gates_skips_dynamic_and_clears_pin_on_exit() {
        let _guard = lock_shared_state();
        modify_disk_limits(|cfg| {
            cfg.drives.insert(
                "X".into(),
                DiskLimits {
                    local_permits: 4,
                    cdn_permits: 2,
                    readrate: 0.0,
                    rate_limit: String::new(),
                    dynamic: true,
                    paused: false,
                },
            );
        });
        let sem = gate_sem(&LOCAL_GATES, "X", 4, true);
        assert_eq!(sem.available_permits(), 1);
        pin_dynamic_permits("X", Some(3), None);
        assert_eq!(dynamic_pin_for("X"), (Some(3), None));
        // Simulate the adjuster having moved it away from the ceiling.
        set_dynamic_permits(&LOCAL_GATES, "X", 2);
        assert_eq!(sem.available_permits(), 2);
        // A save that changes an unrelated field but LEAVES dynamic=true must
        // not fight the adjuster — resize_existing_gates should no-op here.
        modify_disk_limits(|cfg| cfg.drives.get_mut("X").unwrap().cdn_permits = 3);
        assert_eq!(sem.available_permits(), 2, "still-dynamic drive must not be resized on save");
        assert_eq!(dynamic_pin_for("X"), (Some(3), None), "pin survives while still dynamic");
        // Flipping dynamic OFF: resize_existing_gates must resize back to the
        // static ceiling (4) AND clear the now-stale pin.
        modify_disk_limits(|cfg| cfg.drives.get_mut("X").unwrap().dynamic = false);
        assert_eq!(sem.available_permits(), 4, "resized back to the static ceiling on exit");
        assert_eq!(dynamic_pin_for("X"), (None, None), "pin cleared on exiting dynamic mode");
    }

    // `#[tokio::test]` — see the comment on the previous test for why.
    #[tokio::test]
    async fn dyn_status_none_before_gate_then_tracks_current_in_use_ceiling() {
        let _guard = lock_shared_state();
        assert!(local_dyn_status('Y').is_none());
        modify_disk_limits(|cfg| {
            cfg.drives.insert(
                "Y".into(),
                DiskLimits {
                    local_permits: 5,
                    cdn_permits: 2,
                    readrate: 0.0,
                    rate_limit: String::new(),
                    dynamic: true,
                    paused: false,
                },
            );
        });
        assert!(local_dyn_status('Y').is_none(), "still None until a gate actually exists");
        let sem = gate_sem(&LOCAL_GATES, "Y", 5, true);
        let status = local_dyn_status('Y').unwrap();
        assert_eq!((status.current, status.in_use, status.ceiling), (1, 0, 5));
        let _permit = sem.clone().try_acquire_owned().unwrap();
        let status = local_dyn_status('Y').unwrap();
        assert_eq!((status.current, status.in_use, status.ceiling), (1, 1, 5));
    }

    #[test]
    fn pin_dynamic_permits_round_trip() {
        assert_eq!(dynamic_pin_for("Z"), (None, None));
        pin_dynamic_permits("z", Some(2), Some(1)); // lowercase input normalizes
        assert_eq!(dynamic_pin_for("Z"), (Some(2), Some(1)));
        pin_dynamic_permits("Z", None, Some(3));
        assert_eq!(dynamic_pin_for("Z"), (None, Some(3)));
        // Clearing both removes the entry entirely, not just zeroes it.
        pin_dynamic_permits("Z", None, None);
        assert_eq!(dynamic_pin_for("Z"), (None, None));
    }

    // ── Emergency pause / kill (2026-07-23) ──────────────────────────────

    #[test]
    fn disk_limits_paused_field_defaults_and_round_trips() {
        // Old JSON predating this field deserializes to false, same pattern
        // as every other bool field here (`drive_keys_and_serde` above).
        let old: DiskLimits = serde_json::from_str(r#"{"local_permits":2}"#).unwrap();
        assert!(!old.paused);
        let cfg = DiskLimitsConfig {
            default: DiskLimits::default(),
            drives: std::collections::HashMap::from([(
                "M".into(),
                DiskLimits { paused: true, ..Default::default() },
            )]),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: DiskLimitsConfig = serde_json::from_str(&json).unwrap();
        assert!(back.drives["M"].paused);
        assert!(!back.default.paused);
    }

    #[tokio::test]
    async fn pause_blocks_new_local_pass_until_unpaused() {
        let _guard = lock_shared_state();
        let n = Path::new(r"N:\rec\file.ts");
        modify_disk_limits(|cfg| {
            cfg.drives.insert("N".into(), DiskLimits { paused: true, ..Default::default() });
        });
        let waiting = tokio::spawn(async move { local_pass("paused-job", n).await });
        // Give it several scheduler turns to reach (and park inside) the
        // pause poll loop, then confirm it's genuinely still blocked — not
        // just not-yet-polled.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(!waiting.is_finished(), "must stay blocked while the drive is paused");
        // A paused-but-blocked pass must not even be visible as a waiter —
        // it hasn't reached the semaphore yet (distinct from genuine
        // contention, which the Background tab needs to tell apart).
        assert!(local_gate_queue().iter().all(|(_, d, _)| d != "N"));

        modify_disk_limits(|cfg| cfg.drives.get_mut("N").unwrap().paused = false);
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("must resolve promptly once unpaused")
            .expect("task panicked");
        drop(got);
    }

    #[tokio::test]
    async fn local_gate_status_by_drive_groups_by_drive() {
        let _guard = lock_shared_state();
        let o = local_pass("o-job", Path::new(r"O:\x.mkv")).await;
        let p1 = local_pass("p-job-1", Path::new(r"P:\x.mkv")).await;
        // P allows only 1 permit by default, so a second acquisition queues.
        let p_waiting = tokio::spawn(async { local_pass("p-job-2", Path::new(r"P:\y.mkv")).await });
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let by_drive = local_gate_status_by_drive();
        let o_entry = by_drive.iter().find(|(d, ..)| d == "O").expect("O must be listed");
        assert_eq!(o_entry.1.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), vec!["o-job"]);
        assert_eq!(o_entry.2, 0, "O has no queue");
        let p_entry = by_drive.iter().find(|(d, ..)| d == "P").expect("P must be listed");
        assert_eq!(p_entry.1.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), vec!["p-job-1"]);
        assert_eq!(p_entry.2, 1, "P's second acquisition is queued");
        drop(o);
        drop(p1);
        let _ = p_waiting.await;
    }

    #[test]
    fn kill_local_holder_on_idle_drive_kills_nothing() {
        assert_eq!(kill_local_holder("nonexistent-drive-letter"), 0);
    }
}
