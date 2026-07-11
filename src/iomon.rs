//! Process-wide I/O accounting: every filesystem operation the app performs is
//! recorded here, categorized by purpose ([`Cat`]) and storage region
//! ([`Region`]), so the I/O tab can show live rates, session totals, and a
//! recent-operations log — and so new disk-load problems on the recordings
//! drive can be *discovered* instead of audited after the fact.
//!
//! Three layers feed the numbers:
//!
//! 1. The [`fs`] facade — thin wrappers over `std::fs`/`tokio::fs` that time
//!    the call, count ops/bytes, and delegate. All raw call sites are migrated
//!    onto it, and `clippy.toml` (`disallowed-methods`) keeps future code from
//!    bypassing it.
//! 2. Streaming call sites (chat sidecar flushes, log-tail reads, fs probes)
//!    that read/write through already-open handles call [`record`] directly
//!    with true byte counts — the facade can't see trait-level `Read`/`Write`.
//! 3. A sampler thread (see `start_sampler`, added with the child-process
//!    registry) polls `GetProcessIoCounters` for the app itself and every
//!    spawned tool, since streamlink/yt-dlp/ffmpeg do the bulk media bytes.
//!
//! Overhead is deliberately negligible: per op it's two `Instant::now()`
//! calls, a handful of `Relaxed` atomic adds, and a `try_lock` ring push that
//! never blocks a hot path (a contended push is dropped; the atomic counters
//! never miss).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

/// How long a single filesystem op may take before it counts as *slow*:
/// bumps the `slow_ops` counter, always enters the ops ring, and emits a
/// rate-limited warning. 100ms is glacial for one syscall — on the recordings
/// drive it means the disk is saturated (or the USB enclosure is wobbling).
pub const SLOW_OP_MS: u64 = 100;

/// Ops-ring capacity (recent-operations log in the I/O tab).
const OPS_RING_CAP: usize = 512;

/// Minimum gap between slow-op `warn!`s per category (the ring + counters
/// still record every one; the log just shouldn't scroll).
const SLOW_WARN_GAP_MS: i64 = 5_000;

// ===== Categories & regions =====

/// What an I/O operation was *for*. One cell of counters exists per
/// `(Cat, Region)` pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(usize)]
pub enum Cat {
    /// Capture tool stdout/stderr log lifecycle (creation, prune).
    ToolLog,
    /// Tail reads of tool logs (progress polling, finalize tail).
    LogRead,
    /// Chat sidecar appends + chat-replay reads.
    ChatSidecar,
    /// `.cache` → output renames/moves at finalize.
    Promote,
    /// Stale-file/leftover sweeps in `.cache` and output dirs.
    CacheSweep,
    /// Output/`.cache` directory creation before a capture.
    DirSetup,
    /// ffmpeg concat-list files.
    ConcatList,
    /// Thumbnail/subtitle sidecar handling in the downloader.
    Thumbnail,
    /// VOD-recovery playlist temp files.
    Recovery,
    /// HLS preview temp files.
    Preview,
    /// Channel asset cache (emotes, badges, icons, posts, about).
    AssetCache,
    /// UI-side existence/size probes (the `FsProbes` worker).
    FsProbe,
    /// User-initiated recording/file deletion from the UI.
    RecordingDelete,
    /// SQLite access (op counts + hold time via the store's guard; byte
    /// growth is sampled from the db/WAL file sizes).
    Db,
    /// The app's own rolling log (+ startup prune).
    AppLog,
    /// Detection metadata/thumbnail cache writes.
    Detector,
    /// One-off startup/etc. I/O (dir setup, fonts, crash details).
    Startup,
    /// The I/O monitor's own sample log (self-accounting).
    IoMonLog,
    /// Anything that doesn't fit the above.
    Other,
}

pub const CAT_COUNT: usize = 19;

impl Cat {
    pub const ALL: [Cat; CAT_COUNT] = [
        Cat::ToolLog,
        Cat::LogRead,
        Cat::ChatSidecar,
        Cat::Promote,
        Cat::CacheSweep,
        Cat::DirSetup,
        Cat::ConcatList,
        Cat::Thumbnail,
        Cat::Recovery,
        Cat::Preview,
        Cat::AssetCache,
        Cat::FsProbe,
        Cat::RecordingDelete,
        Cat::Db,
        Cat::AppLog,
        Cat::Detector,
        Cat::Startup,
        Cat::IoMonLog,
        Cat::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Cat::ToolLog => "tool logs",
            Cat::LogRead => "log tails",
            Cat::ChatSidecar => "chat sidecars",
            Cat::Promote => "promote/rename",
            Cat::CacheSweep => "cache sweeps",
            Cat::DirSetup => "dir setup",
            Cat::ConcatList => "concat lists",
            Cat::Thumbnail => "thumbnails/subs",
            Cat::Recovery => "recovery playlists",
            Cat::Preview => "previews",
            Cat::AssetCache => "asset cache",
            Cat::FsProbe => "fs probes",
            Cat::RecordingDelete => "deletions",
            Cat::Db => "database",
            Cat::AppLog => "app log",
            Cat::Detector => "detectors",
            Cat::Startup => "startup/misc",
            Cat::IoMonLog => "io monitor log",
            Cat::Other => "other",
        }
    }
}

/// Which storage region a path belongs to. The whole point of the monitor is
/// separating load on the (fragile, USB-attached) recordings drive from the
/// (fast, local) appdata SSD.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(usize)]
pub enum Region {
    /// Under a configured recordings root (or on the same drive as one).
    Recordings,
    /// Under the app data dir (DB, logs, asset cache).
    AppData,
    /// Under the OS temp dir.
    Temp,
    Other,
}

pub const REGION_COUNT: usize = 4;

impl Region {
    pub const ALL: [Region; REGION_COUNT] =
        [Region::Recordings, Region::AppData, Region::Temp, Region::Other];

    pub fn label(self) -> &'static str {
        match self {
            Region::Recordings => "recordings drive",
            Region::AppData => "appdata",
            Region::Temp => "temp",
            Region::Other => "other",
        }
    }
}

/// The kind of filesystem operation (for the ops ring; counters only split
/// read/write/meta).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OpKind {
    Read,
    Write,
    Create,
    Rename,
    Delete,
    /// Metadata-only: stat, exists, read_dir, open.
    Meta,
}

impl OpKind {
    pub fn label(self) -> &'static str {
        match self {
            OpKind::Read => "read",
            OpKind::Write => "write",
            OpKind::Create => "create",
            OpKind::Rename => "rename",
            OpKind::Delete => "delete",
            OpKind::Meta => "meta",
        }
    }
}

// ===== Counter cells =====

/// One `(Cat, Region)` counter cell. All `Relaxed` — these are statistics,
/// not synchronization.
struct Cell {
    read_ops: AtomicU64,
    read_bytes: AtomicU64,
    write_ops: AtomicU64,
    write_bytes: AtomicU64,
    meta_ops: AtomicU64,
    total_ns: AtomicU64,
    slow_ops: AtomicU64,
    max_op_bytes: AtomicU64,
}

impl Cell {
    const fn new() -> Cell {
        Cell {
            read_ops: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            write_ops: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            meta_ops: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            slow_ops: AtomicU64::new(0),
            max_op_bytes: AtomicU64::new(0),
        }
    }
}

// Const items so the array repeats below are allowed (Cell isn't Copy).
#[allow(clippy::declare_interior_mutable_const)]
const CELL_INIT: Cell = Cell::new();
#[allow(clippy::declare_interior_mutable_const)]
const CELL_ROW_INIT: [Cell; REGION_COUNT] = [CELL_INIT; REGION_COUNT];
static CELLS: [[Cell; REGION_COUNT]; CAT_COUNT] = [CELL_ROW_INIT; CAT_COUNT];

#[allow(clippy::declare_interior_mutable_const)]
const LAST_WARN_INIT: AtomicI64 = AtomicI64::new(i64::MIN);
/// Per-category "last slow-op warn" timestamp (ms since process start).
static LAST_SLOW_WARN: [AtomicI64; CAT_COUNT] = [LAST_WARN_INIT; CAT_COUNT];

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

// ===== Region classification =====

/// Configured recordings roots (default output dir + every monitor's output
/// dir). Set at startup and re-set on settings/monitor save.
static RECORDINGS_ROOTS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());
/// Uppercase drive letters of the recordings roots (fallback classification
/// for paths on the same physical drive but outside any configured root).
static RECORDINGS_DRIVES: RwLock<Vec<char>> = RwLock::new(Vec::new());

fn drive_letter(p: &Path) -> Option<char> {
    match p.components().next() {
        Some(std::path::Component::Prefix(pre)) => match pre.kind() {
            std::path::Prefix::Disk(b) | std::path::Prefix::VerbatimDisk(b) => {
                Some((b as char).to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Register the set of recordings roots used for [`Region`] classification.
pub fn set_recordings_roots(roots: Vec<PathBuf>) {
    let mut drives: Vec<char> = roots.iter().filter_map(|r| drive_letter(r)).collect();
    drives.sort_unstable();
    drives.dedup();
    *RECORDINGS_DRIVES.write() = drives;
    *RECORDINGS_ROOTS.write() = roots;
}

fn data_dir_cached() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(crate::app_paths::data_dir)
}

fn temp_dir_cached() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(std::env::temp_dir)
}

/// Classify a path into its storage [`Region`].
pub fn classify(path: &Path) -> Region {
    {
        let roots = RECORDINGS_ROOTS.read();
        if roots.iter().any(|r| path.starts_with(r)) {
            return Region::Recordings;
        }
    }
    if path.starts_with(data_dir_cached()) {
        return Region::AppData;
    }
    if path.starts_with(temp_dir_cached()) {
        return Region::Temp;
    }
    // Same drive as a recordings root still hits the same spindle.
    if let Some(letter) = drive_letter(path)
        && RECORDINGS_DRIVES.read().contains(&letter)
    {
        return Region::Recordings;
    }
    Region::Other
}

// ===== Ops ring =====

/// One entry in the recent-operations log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpRecord {
    /// Wall-clock ms (unix epoch).
    pub at_ms: i64,
    pub cat: Cat,
    pub region: Region,
    pub kind: OpKind,
    pub path: Box<str>,
    pub bytes: u64,
    pub dur_us: u32,
    /// Name of the thread that performed the op — UI-thread ("main") disk
    /// touches are exactly the thing the monitor exists to expose.
    pub thread: Box<str>,
}

static OPS_RING: Mutex<std::collections::VecDeque<OpRecord>> =
    Mutex::new(std::collections::VecDeque::new());

// ===== Recording =====

/// Record one filesystem operation, classifying `path` into a region.
pub fn record(cat: Cat, path: &Path, kind: OpKind, bytes: u64, dur: Duration) {
    record_inner(cat, classify(path), kind, bytes, dur, Some(path));
}

/// Like [`record`] for call sites that classified their path once up front
/// (e.g. the chat sink, which appends to the same file for a whole session).
pub fn record_region(cat: Cat, region: Region, kind: OpKind, bytes: u64, dur: Duration) {
    record_inner(cat, region, kind, bytes, dur, None);
}

fn record_inner(
    cat: Cat,
    region: Region,
    kind: OpKind,
    bytes: u64,
    dur: Duration,
    path: Option<&Path>,
) {
    let cell = &CELLS[cat as usize][region as usize];
    match kind {
        OpKind::Read => {
            cell.read_ops.fetch_add(1, Ordering::Relaxed);
            cell.read_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
        OpKind::Write | OpKind::Create => {
            cell.write_ops.fetch_add(1, Ordering::Relaxed);
            cell.write_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
        OpKind::Rename | OpKind::Delete | OpKind::Meta => {
            cell.meta_ops.fetch_add(1, Ordering::Relaxed);
        }
    }
    cell.total_ns
        .fetch_add(dur.as_nanos().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
    cell.max_op_bytes.fetch_max(bytes, Ordering::Relaxed);

    let slow = dur.as_millis() as u64 >= SLOW_OP_MS;
    if slow {
        cell.slow_ops.fetch_add(1, Ordering::Relaxed);
    }

    // DB guard drops fire hundreds of times a second under load and carry no
    // path — counters only. The store logs slow lock holds itself, with the
    // acquiring call site (file:line), which this pathless warn can't match.
    if matches!(cat, Cat::Db) {
        return;
    }

    if slow {
        let now_ms = epoch().elapsed().as_millis() as i64;
        let last = &LAST_SLOW_WARN[cat as usize];
        let prev = last.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) >= SLOW_WARN_GAP_MS
            && last
                .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                "slow {} op ({}ms): {} [{}] {}",
                kind.label(),
                dur.as_millis(),
                cat.label(),
                region.label(),
                path.map(|p| p.display().to_string()).unwrap_or_default()
            );
        }
    }
    // Ring push: try_lock so no hot path (or panic path) ever blocks here.
    // A contended push is dropped — the atomic counters above never miss.
    if let Some(mut ring) = OPS_RING.try_lock() {
        if ring.len() >= OPS_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(OpRecord {
            at_ms: chrono::Utc::now().timestamp_millis(),
            cat,
            region,
            kind,
            path: path
                .map(|p| p.display().to_string().into_boxed_str())
                .unwrap_or_default(),
            bytes,
            dur_us: dur.as_micros().min(u128::from(u32::MAX)) as u32,
            thread: std::thread::current().name().unwrap_or("?").into(),
        });
    }
}

// ===== Snapshots (UI-side) =====

/// Plain-number copy of one counter cell.
#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize)]
pub struct CellSnap {
    pub read_ops: u64,
    pub read_bytes: u64,
    pub write_ops: u64,
    pub write_bytes: u64,
    pub meta_ops: u64,
    pub total_ns: u64,
    pub slow_ops: u64,
    pub max_op_bytes: u64,
}

impl CellSnap {
    pub fn ops(&self) -> u64 {
        self.read_ops + self.write_ops + self.meta_ops
    }
    pub fn bytes(&self) -> u64 {
        self.read_bytes + self.write_bytes
    }
    /// Element-wise difference vs an earlier snapshot (for rate computation).
    pub fn delta(&self, earlier: &CellSnap) -> CellSnap {
        CellSnap {
            read_ops: self.read_ops.saturating_sub(earlier.read_ops),
            read_bytes: self.read_bytes.saturating_sub(earlier.read_bytes),
            write_ops: self.write_ops.saturating_sub(earlier.write_ops),
            write_bytes: self.write_bytes.saturating_sub(earlier.write_bytes),
            meta_ops: self.meta_ops.saturating_sub(earlier.meta_ops),
            total_ns: self.total_ns.saturating_sub(earlier.total_ns),
            slow_ops: self.slow_ops.saturating_sub(earlier.slow_ops),
            max_op_bytes: self.max_op_bytes,
        }
    }
    fn add(&mut self, other: &CellSnap) {
        self.read_ops += other.read_ops;
        self.read_bytes += other.read_bytes;
        self.write_ops += other.write_ops;
        self.write_bytes += other.write_bytes;
        self.meta_ops += other.meta_ops;
        self.total_ns += other.total_ns;
        self.slow_ops += other.slow_ops;
        self.max_op_bytes = self.max_op_bytes.max(other.max_op_bytes);
    }
}

/// A point-in-time copy of all counters + the ops ring.
#[derive(Clone, Default)]
pub struct CountersSnapshot {
    /// `[cat][region]` cells.
    pub cells: Vec<[CellSnap; REGION_COUNT]>,
    pub ring: Vec<OpRecord>,
}

impl CountersSnapshot {
    #[cfg_attr(not(test), allow(dead_code))] // exercised by the unit tests
    pub fn cell(&self, cat: Cat, region: Region) -> &CellSnap {
        &self.cells[cat as usize][region as usize]
    }
    /// All regions of one category summed.
    pub fn cat_total(&self, cat: Cat) -> CellSnap {
        let mut out = CellSnap::default();
        for r in &self.cells[cat as usize] {
            out.add(r);
        }
        out
    }
    /// All categories of one region summed.
    pub fn region_total(&self, region: Region) -> CellSnap {
        let mut out = CellSnap::default();
        for c in &self.cells {
            out.add(&c[region as usize]);
        }
        out
    }
}

/// Copy every counter and the ops ring. Called from the UI (a few KB clone).
pub fn snapshot() -> CountersSnapshot {
    let cells = CELLS
        .iter()
        .map(|regions| {
            let mut row = [CellSnap::default(); REGION_COUNT];
            for (i, cell) in regions.iter().enumerate() {
                row[i] = CellSnap {
                    read_ops: cell.read_ops.load(Ordering::Relaxed),
                    read_bytes: cell.read_bytes.load(Ordering::Relaxed),
                    write_ops: cell.write_ops.load(Ordering::Relaxed),
                    write_bytes: cell.write_bytes.load(Ordering::Relaxed),
                    meta_ops: cell.meta_ops.load(Ordering::Relaxed),
                    total_ns: cell.total_ns.load(Ordering::Relaxed),
                    slow_ops: cell.slow_ops.load(Ordering::Relaxed),
                    max_op_bytes: cell.max_op_bytes.load(Ordering::Relaxed),
                };
            }
            row
        })
        .collect();
    let ring = OPS_RING.lock().iter().cloned().collect();
    CountersSnapshot { cells, ring }
}

// ===== Child-process registry =====

/// What a registered child process is, for the per-process table and for
/// attributing its I/O to a storage region (approximate: a process's
/// `IO_COUNTERS` are not per-volume, so we attribute by its target path).
#[derive(Clone, Debug)]
pub struct ChildInfo {
    /// Channel / video / job name shown in the per-process table.
    pub label: String,
    /// Program: streamlink / yt-dlp / ffmpeg / ffprobe / ...
    pub tool: String,
    /// capture / chat / remux / concat / embed / recovery / probe / ...
    pub purpose: String,
    /// Region of the file the tool works against.
    pub region: Region,
    /// OS process creation time (see `platform::process_start_time`) so a
    /// recycled PID is never attributed; 0 = unknown (skip the check).
    pub proc_start: u64,
}

static CHILDREN: Mutex<Option<HashMap<u32, ChildInfo>>> = Mutex::new(None);

/// Register a spawned tool for I/O sampling. Pair with [`unregister_child`]
/// (or use [`track_child`] for RAII).
pub fn register_child(pid: u32, info: ChildInfo) {
    if pid == 0 {
        return;
    }
    CHILDREN.lock().get_or_insert_with(HashMap::new).insert(pid, info);
}

/// Remove a child from sampling. Its last-sampled totals are folded into the
/// session's finished-children counters by the sampler (so lifetime totals
/// don't shrink when a capture ends).
pub fn unregister_child(pid: u32) {
    if let Some(map) = CHILDREN.lock().as_mut() {
        map.remove(&pid);
    }
}

/// RAII registration for short-lived tools (ffprobe, embed passes).
pub struct ChildGuard(u32);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        unregister_child(self.0);
    }
}

/// Register `pid` and get a guard that unregisters on drop.
pub fn track_child(pid: u32, info: ChildInfo) -> ChildGuard {
    register_child(pid, info);
    ChildGuard(pid)
}

/// Convenience for post-processing passes: register a just-spawned tool
/// against the file it works on. `None` pid (spawn raced its exit) → no-op.
/// Skips the PID-recycle guard — these are held for one child's lifetime.
pub fn track_tool(
    pid: Option<u32>,
    tool: &str,
    purpose: &str,
    target: &Path,
) -> Option<ChildGuard> {
    pid.map(|pid| {
        track_child(
            pid,
            ChildInfo {
                label: target
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                tool: tool.to_string(),
                purpose: purpose.to_string(),
                region: classify(target),
                proc_start: 0,
            },
        )
    })
}

// ===== Sampler (1 s cadence): self + child process I/O, history, JSONL =====

/// Seconds between samples.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// In-memory history depth: 30 min at 1 s.
const HISTORY_LEN: usize = 1800;
/// JSONL sample-log flush cadence (samples).
const SAMPLE_LOG_FLUSH_EVERY: usize = 10;
/// Days of iomon sample logs kept (pruned at startup with the other logs).
pub const SAMPLE_LOG_KEEP_DAYS: u64 = 14;

/// Settings key: write 1 s samples to a JSONL under the appdata logs dir.
pub const K_IOMON_LOG: &str = "iomon_sample_log";
/// The sample log defaults to ON: post-mortems of overnight drive stalls
/// only work if the data was already being collected.
pub const SAMPLE_LOG_DEFAULT: bool = true;

static SAMPLE_LOG_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(SAMPLE_LOG_DEFAULT);

/// Enable/disable the JSONL sample log (startup + settings save).
pub fn set_sample_logging(on: bool) {
    SAMPLE_LOG_ENABLED.store(on, Ordering::Relaxed);
}

pub fn sample_logging() -> bool {
    SAMPLE_LOG_ENABLED.load(Ordering::Relaxed)
}

/// One process's contribution within a [`Sample`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcSample {
    pub pid: u32,
    pub label: String,
    pub tool: String,
    pub purpose: String,
    pub region: Region,
    /// Bytes/sec since the previous sample. Read side of capture tools is
    /// mostly network (CDN) — the write side is the disk-relevant number.
    pub read_bps: u64,
    pub write_bps: u64,
    /// Cumulative transfer since the process started.
    pub total_read: u64,
    pub total_write: u64,
    /// Live descendant processes rolled into this row (yt-dlp → ffmpeg).
    pub descendants: u32,
}

/// A physical-disk reading (filled in by `platform::disk_performance`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskSample {
    /// Drive letter the reading was resolved from (e.g. 'A').
    pub letter: char,
    pub read_bps: u64,
    pub write_bps: u64,
    pub queue_depth: u32,
}

/// One 1 s observation of everything.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sample {
    /// Wall-clock ms (unix epoch).
    pub at_ms: i64,
    /// This process (includes SQLite, tracing, the UI — and its *network*
    /// I/O: `IO_COUNTERS` count sockets too).
    pub self_read_bps: u64,
    pub self_write_bps: u64,
    /// Sum over live child trees.
    pub child_read_bps: u64,
    pub child_write_bps: u64,
    /// Instrumented in-process (read, write) B/s per region (from the
    /// category counters — real file bytes, no network).
    pub per_region: [(u64, u64); REGION_COUNT],
    /// Instrumented (read, write) B/s per category.
    pub per_cat: Vec<(u64, u64)>,
    /// Per live child-process rates (grandchildren rolled up).
    pub procs: Vec<ProcSample>,
    /// Self-process B/s the in-process instrumentation didn't account for:
    /// SQLite, the tracing appender, egui persistence — and all of the app's
    /// own network traffic (API polls, image downloads).
    pub unattributed_bps: u64,
    /// Current size of the SQLite db + WAL (bytes).
    pub db_bytes: u64,
    /// Physical-disk readings (recordings drive first), when available.
    pub disks: Vec<DiskSample>,
}

/// Session totals for children that already exited (folded on unregister so
/// lifetime numbers don't shrink when a capture ends).
#[derive(Clone, Copy, Debug, Default)]
pub struct FinishedChildTotals {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub count: u64,
}

static HISTORY: Mutex<std::collections::VecDeque<Sample>> =
    Mutex::new(std::collections::VecDeque::new());
static FINISHED: Mutex<FinishedChildTotals> = Mutex::new(FinishedChildTotals {
    read_bytes: 0,
    write_bytes: 0,
    count: 0,
});

/// Copy of the sample history, oldest → newest. UI-side callers should cache
/// this and refresh ~1×/s rather than cloning every frame.
pub fn history() -> Vec<Sample> {
    HISTORY.lock().iter().cloned().collect()
}

pub fn finished_child_totals() -> FinishedChildTotals {
    *FINISHED.lock()
}

/// Where the current session's JSONL sample log lives (dir is created by the
/// sampler on first write).
pub fn sample_log_dir() -> PathBuf {
    crate::app_paths::logs_dir().join("iomon")
}

/// Per-PID state the sampler carries between ticks (last cumulative counters,
/// for delta rates and for folding into the finished totals on exit).
struct SeenProc {
    last: crate::platform::ProcIo,
}

/// Start the background sampler thread (idempotent).
pub fn start_sampler() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("io-sampler".into())
            .spawn(sampler_loop)
            .expect("spawn io-sampler thread");
    });
}

fn sampler_loop() {
    let mut prev_cells = snapshot().cells;
    let mut prev_self = crate::platform::self_io_counters().unwrap_or_default();
    let mut seen: HashMap<u32, SeenProc> = HashMap::new();
    let mut prev_disks: HashMap<char, (u64, u64)> = HashMap::new();
    let mut log_buf: Vec<String> = Vec::new();
    let session_log = sample_log_dir().join(format!(
        "session-{}.jsonl",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    loop {
        std::thread::sleep(SAMPLE_INTERVAL);
        let interval_secs = SAMPLE_INTERVAL.as_secs_f64();

        // --- in-process instrumented deltas (per region / per category) ---
        let cells = snapshot().cells;
        let mut per_region = [(0u64, 0u64); REGION_COUNT];
        let mut per_cat = vec![(0u64, 0u64); CAT_COUNT];
        let mut instrumented_bytes = 0u64;
        for (ci, row) in cells.iter().enumerate() {
            for (ri, cell) in row.iter().enumerate() {
                let d = cell.delta(&prev_cells[ci][ri]);
                per_region[ri].0 += d.read_bytes;
                per_region[ri].1 += d.write_bytes;
                per_cat[ci].0 += d.read_bytes;
                per_cat[ci].1 += d.write_bytes;
                instrumented_bytes += d.read_bytes + d.write_bytes;
            }
        }
        prev_cells = cells;

        // --- self process ---
        let self_now = crate::platform::self_io_counters().unwrap_or(prev_self);
        let self_read_bps = self_now.read_bytes.saturating_sub(prev_self.read_bytes);
        let self_write_bps = self_now.write_bytes.saturating_sub(prev_self.write_bytes);
        prev_self = self_now;
        let unattributed_bps = (self_read_bps + self_write_bps).saturating_sub(instrumented_bytes);

        // --- children (registered roots + their live descendants) ---
        let registered: Vec<(u32, ChildInfo)> = CHILDREN
            .lock()
            .as_ref()
            .map(|m| m.iter().map(|(p, i)| (*p, i.clone())).collect())
            .unwrap_or_default();
        // Fold children that disappeared since the last tick into the
        // session totals before dropping their state.
        let live_pids: std::collections::HashSet<u32> =
            registered.iter().map(|(p, _)| *p).collect();
        seen.retain(|pid, sp| {
            if live_pids.contains(pid) {
                return true;
            }
            let mut fin = FINISHED.lock();
            fin.read_bytes += sp.last.read_bytes;
            fin.write_bytes += sp.last.write_bytes;
            fin.count += 1;
            false
        });

        // One Toolhelp snapshot per tick, shared by every tree walk.
        let pairs = if registered.is_empty() {
            Vec::new()
        } else {
            crate::platform::process_children_snapshot()
        };
        let mut procs: Vec<ProcSample> = Vec::with_capacity(registered.len());
        let (mut child_read_bps, mut child_write_bps) = (0u64, 0u64);
        for (pid, info) in registered {
            // PID-recycle guard: a mismatching creation time means a new
            // process wears this number now — drop the registration.
            if info.proc_start != 0 {
                match crate::platform::process_start_time(pid) {
                    Some(st) if st == info.proc_start => {}
                    Some(_) => {
                        unregister_child(pid);
                        continue;
                    }
                    None => continue, // exited — folded next tick
                }
            }
            // BFS the live descendants (yt-dlp spawns the ffmpeg that does
            // the real writing).
            let mut tree = vec![pid];
            let mut i = 0;
            while i < tree.len() {
                let cur = tree[i];
                for &(p, parent) in &pairs {
                    if parent == cur && !tree.contains(&p) {
                        tree.push(p);
                    }
                }
                i += 1;
            }
            let mut now = crate::platform::ProcIo::default();
            let mut alive = 0u32;
            for &p in &tree {
                if let Some(io) = crate::platform::process_io_counters(p) {
                    now.read_bytes += io.read_bytes;
                    now.write_bytes += io.write_bytes;
                    now.read_ops += io.read_ops;
                    now.write_ops += io.write_ops;
                    alive += 1;
                }
            }
            if alive == 0 {
                continue; // whole tree gone — folded next tick via `seen`
            }
            // First sight of a pid seeds the baseline and reports ZERO rate —
            // its counters are cumulative since the process started, so
            // delta-from-nothing would report e.g. a re-attached capture's
            // whole 12 GB as one second's throughput (a 37 GB/s graph spike).
            let (read_bps, write_bps) = match seen.insert(pid, SeenProc { last: now }) {
                Some(prev) => (
                    now.read_bytes.saturating_sub(prev.last.read_bytes),
                    now.write_bytes.saturating_sub(prev.last.write_bytes),
                ),
                None => (0, 0),
            };
            child_read_bps += read_bps;
            child_write_bps += write_bps;
            procs.push(ProcSample {
                pid,
                label: info.label,
                tool: info.tool,
                purpose: info.purpose,
                region: info.region,
                read_bps,
                write_bps,
                total_read: now.read_bytes,
                total_write: now.write_bytes,
                descendants: alive.saturating_sub(1),
            });
        }
        procs.sort_by_key(|p| std::cmp::Reverse(p.write_bps + p.read_bps));

        // --- db + wal size (C:) ---
        let db_path = crate::app_paths::db_path();
        let wal = {
            let mut w = db_path.as_os_str().to_owned();
            w.push("-wal");
            PathBuf::from(w)
        };
        let db_bytes = fs::metadata_sync(Cat::Db, &db_path).map(|m| m.len()).unwrap_or(0)
            + fs::metadata_sync(Cat::Db, &wal).map(|m| m.len()).unwrap_or(0);

        let sample = Sample {
            at_ms: chrono::Utc::now().timestamp_millis(),
            self_read_bps: (self_read_bps as f64 / interval_secs) as u64,
            self_write_bps: (self_write_bps as f64 / interval_secs) as u64,
            child_read_bps: (child_read_bps as f64 / interval_secs) as u64,
            child_write_bps: (child_write_bps as f64 / interval_secs) as u64,
            per_region,
            per_cat,
            procs,
            unattributed_bps: (unattributed_bps as f64 / interval_secs) as u64,
            db_bytes,
            disks: sample_disks(&mut prev_disks, interval_secs),
        };

        // --- JSONL sample log (buffered; appdata drive only) ---
        if sample_logging() {
            if let Ok(line) = serde_json::to_string(&sample) {
                log_buf.push(line);
            }
            if log_buf.len() >= SAMPLE_LOG_FLUSH_EVERY {
                flush_sample_log(&session_log, &mut log_buf);
            }
        } else if !log_buf.is_empty() {
            flush_sample_log(&session_log, &mut log_buf);
        }

        {
            let mut hist = HISTORY.lock();
            if hist.len() >= HISTORY_LEN {
                hist.pop_front();
            }
            hist.push_back(sample);
        }
    }
}

/// Physical-disk readings for the recordings drive(s) + the appdata drive.
/// Cumulative counters → B/s via `prev`; drives whose disk-performance query
/// fails (no diskperf filter, USB oddities) are simply absent — the UI shows
/// "n/a". Recordings drives sort first.
fn sample_disks(prev: &mut HashMap<char, (u64, u64)>, interval_secs: f64) -> Vec<DiskSample> {
    let mut letters: Vec<char> = RECORDINGS_DRIVES.read().clone();
    if let Some(l) = drive_letter(data_dir_cached())
        && !letters.contains(&l)
    {
        letters.push(l);
    }
    let mut out = Vec::with_capacity(letters.len());
    for letter in letters {
        let Some(perf) = crate::platform::disk_performance(letter) else { continue };
        let (pr, pw) = prev
            .insert(letter, (perf.bytes_read, perf.bytes_written))
            .unwrap_or((perf.bytes_read, perf.bytes_written));
        out.push(DiskSample {
            letter,
            read_bps: (perf.bytes_read.saturating_sub(pr) as f64 / interval_secs) as u64,
            write_bps: (perf.bytes_written.saturating_sub(pw) as f64 / interval_secs) as u64,
            queue_depth: perf.queue_depth,
        });
    }
    out
}

fn flush_sample_log(path: &Path, buf: &mut Vec<String>) {
    use std::io::Write;
    if buf.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all_sync(Cat::IoMonLog, dir);
    }
    let mut text = buf.join("\n");
    text.push('\n');
    buf.clear();
    let start = Instant::now();
    if let Ok(mut f) = fs::open_with_sync(Cat::IoMonLog, path, |o| {
        o.create(true).append(true);
    }) {
        let bytes = text.len() as u64;
        let res = f.write_all(text.as_bytes());
        record_region(Cat::IoMonLog, Region::AppData, OpKind::Write, bytes, start.elapsed());
        if let Err(e) = res {
            tracing::warn!("iomon sample log write failed: {e}");
        }
    }
}

// ===== Instrumented filesystem facade =====

/// Timed, counted wrappers over `std::fs`/`tokio::fs`. Every function takes
/// the [`Cat`] first, then mirrors the wrapped function's signature. Async
/// variants keep the std names; sync variants carry a `_sync` suffix.
///
/// `clippy.toml` disallows the raw functions everywhere else, so this module
/// is the only place in the app that touches `std::fs`/`tokio::fs` directly.
/// Kept API-complete even where no call site exists yet — the clippy `reason`
/// strings point future code here (hence the dead_code allow).
#[allow(clippy::disallowed_methods, dead_code)]
pub mod fs {
    use std::io;
    use std::path::Path;
    use std::time::Instant;

    use super::{Cat, OpKind, record};

    fn done<T>(
        cat: Cat,
        path: &Path,
        kind: OpKind,
        start: Instant,
        bytes: u64,
        res: io::Result<T>,
    ) -> io::Result<T> {
        record(cat, path, kind, bytes, start.elapsed());
        res
    }

    // --- async (tokio::fs) ---

    pub async fn write(cat: Cat, path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
        let (path, contents) = (path.as_ref(), contents.as_ref());
        let start = Instant::now();
        let res = tokio::fs::write(path, contents).await;
        done(cat, path, OpKind::Write, start, contents.len() as u64, res)
    }

    pub async fn read(cat: Cat, path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::read(path).await;
        let bytes = res.as_ref().map(|b| b.len() as u64).unwrap_or(0);
        done(cat, path, OpKind::Read, start, bytes, res)
    }

    pub async fn read_to_string(cat: Cat, path: impl AsRef<Path>) -> io::Result<String> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::read_to_string(path).await;
        let bytes = res.as_ref().map(|s| s.len() as u64).unwrap_or(0);
        done(cat, path, OpKind::Read, start, bytes, res)
    }

    pub async fn copy(cat: Cat, from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
        let (from, to) = (from.as_ref(), to.as_ref());
        let start = Instant::now();
        let res = tokio::fs::copy(from, to).await;
        let bytes = *res.as_ref().unwrap_or(&0);
        done(cat, to, OpKind::Write, start, bytes, res)
    }

    pub async fn rename(cat: Cat, from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
        let (from, to) = (from.as_ref(), to.as_ref());
        let start = Instant::now();
        let res = tokio::fs::rename(from, to).await;
        done(cat, to, OpKind::Rename, start, 0, res)
    }

    pub async fn remove_file(cat: Cat, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::remove_file(path).await;
        done(cat, path, OpKind::Delete, start, 0, res)
    }

    pub async fn remove_dir(cat: Cat, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::remove_dir(path).await;
        done(cat, path, OpKind::Delete, start, 0, res)
    }

    pub async fn remove_dir_all(cat: Cat, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::remove_dir_all(path).await;
        done(cat, path, OpKind::Delete, start, 0, res)
    }

    pub async fn create_dir_all(cat: Cat, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::create_dir_all(path).await;
        done(cat, path, OpKind::Create, start, 0, res)
    }

    pub async fn metadata(cat: Cat, path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::metadata(path).await;
        done(cat, path, OpKind::Meta, start, 0, res)
    }

    pub async fn try_exists(cat: Cat, path: impl AsRef<Path>) -> io::Result<bool> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::try_exists(path).await;
        done(cat, path, OpKind::Meta, start, 0, res)
    }

    pub async fn read_dir(cat: Cat, path: impl AsRef<Path>) -> io::Result<tokio::fs::ReadDir> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::read_dir(path).await;
        done(cat, path, OpKind::Meta, start, 0, res)
    }

    pub async fn open(cat: Cat, path: impl AsRef<Path>) -> io::Result<tokio::fs::File> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::File::open(path).await;
        done(cat, path, OpKind::Meta, start, 0, res)
    }

    pub async fn create(cat: Cat, path: impl AsRef<Path>) -> io::Result<tokio::fs::File> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = tokio::fs::File::create(path).await;
        done(cat, path, OpKind::Create, start, 0, res)
    }

    /// Open with custom options (append, truncate, ...). The closure
    /// configures a fresh `OpenOptions`.
    pub async fn open_with(
        cat: Cat,
        path: impl AsRef<Path>,
        configure: impl FnOnce(&mut tokio::fs::OpenOptions),
    ) -> io::Result<tokio::fs::File> {
        let path = path.as_ref();
        let mut opts = tokio::fs::OpenOptions::new();
        configure(&mut opts);
        let start = Instant::now();
        let res = opts.open(path).await;
        done(cat, path, OpKind::Meta, start, 0, res)
    }

    // --- sync (std::fs) ---

    pub fn write_sync(cat: Cat, path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
        let (path, contents) = (path.as_ref(), contents.as_ref());
        let start = Instant::now();
        let res = std::fs::write(path, contents);
        done(cat, path, OpKind::Write, start, contents.len() as u64, res)
    }

    pub fn read_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::read(path);
        let bytes = res.as_ref().map(|b| b.len() as u64).unwrap_or(0);
        done(cat, path, OpKind::Read, start, bytes, res)
    }

    pub fn read_to_string_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<String> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::read_to_string(path);
        let bytes = res.as_ref().map(|s| s.len() as u64).unwrap_or(0);
        done(cat, path, OpKind::Read, start, bytes, res)
    }

    pub fn copy_sync(cat: Cat, from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
        let (from, to) = (from.as_ref(), to.as_ref());
        let start = Instant::now();
        let res = std::fs::copy(from, to);
        let bytes = *res.as_ref().unwrap_or(&0);
        done(cat, to, OpKind::Write, start, bytes, res)
    }

    pub fn rename_sync(cat: Cat, from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
        let (from, to) = (from.as_ref(), to.as_ref());
        let start = Instant::now();
        let res = std::fs::rename(from, to);
        done(cat, to, OpKind::Rename, start, 0, res)
    }

    pub fn remove_file_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::remove_file(path);
        done(cat, path, OpKind::Delete, start, 0, res)
    }

    pub fn remove_dir_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::remove_dir(path);
        done(cat, path, OpKind::Delete, start, 0, res)
    }

    pub fn remove_dir_all_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::remove_dir_all(path);
        done(cat, path, OpKind::Delete, start, 0, res)
    }

    pub fn create_dir_all_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::create_dir_all(path);
        done(cat, path, OpKind::Create, start, 0, res)
    }

    pub fn metadata_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::metadata(path);
        done(cat, path, OpKind::Meta, start, 0, res)
    }

    /// `Path::exists` semantics (false on any error), counted as a meta op.
    pub fn exists_sync(cat: Cat, path: impl AsRef<Path>) -> bool {
        metadata_sync(cat, path).is_ok()
    }

    /// `Path::is_file` semantics, counted as a meta op.
    pub fn is_file_sync(cat: Cat, path: impl AsRef<Path>) -> bool {
        metadata_sync(cat, path).map(|m| m.is_file()).unwrap_or(false)
    }

    /// `Path::is_dir` semantics, counted as a meta op.
    pub fn is_dir_sync(cat: Cat, path: impl AsRef<Path>) -> bool {
        metadata_sync(cat, path).map(|m| m.is_dir()).unwrap_or(false)
    }

    pub fn read_dir_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<std::fs::ReadDir> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::read_dir(path);
        done(cat, path, OpKind::Meta, start, 0, res)
    }

    pub fn open_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<std::fs::File> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::File::open(path);
        done(cat, path, OpKind::Meta, start, 0, res)
    }

    pub fn create_sync(cat: Cat, path: impl AsRef<Path>) -> io::Result<std::fs::File> {
        let path = path.as_ref();
        let start = Instant::now();
        let res = std::fs::File::create(path);
        done(cat, path, OpKind::Create, start, 0, res)
    }

    /// Open with custom options (append, truncate, ...). The closure
    /// configures a fresh `OpenOptions`.
    pub fn open_with_sync(
        cat: Cat,
        path: impl AsRef<Path>,
        configure: impl FnOnce(&mut std::fs::OpenOptions),
    ) -> io::Result<std::fs::File> {
        let path = path.as_ref();
        let mut opts = std::fs::OpenOptions::new();
        configure(&mut opts);
        let start = Instant::now();
        let res = opts.open(path);
        done(cat, path, OpKind::Meta, start, 0, res)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sa-iomon-test-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn classify_region_prefix_and_drive_letter_fallback() {
        set_recordings_roots(vec![PathBuf::from("Q:\\streams")]);
        assert_eq!(classify(Path::new("Q:\\streams\\chan\\take.mkv")), Region::Recordings);
        // Same drive, outside the root → still the same spindle.
        assert_eq!(classify(Path::new("Q:\\elsewhere\\x.txt")), Region::Recordings);
        assert_eq!(classify(&data_dir_cached().join("streamarchiver.sqlite3")), Region::AppData);
        assert_eq!(classify(&temp_dir_cached().join("preview.m3u8")), Region::Temp);
        assert_eq!(classify(Path::new("Z:\\unrelated\\file")), Region::Other);
        set_recordings_roots(Vec::new());
    }

    #[test]
    fn counters_accumulate_per_cat_region() {
        let before = *snapshot().cell(Cat::Recovery, Region::Other);
        record_region(Cat::Recovery, Region::Other, OpKind::Write, 1234, Duration::from_micros(50));
        record_region(Cat::Recovery, Region::Other, OpKind::Read, 100, Duration::from_micros(50));
        record_region(Cat::Recovery, Region::Other, OpKind::Meta, 0, Duration::from_micros(50));
        let after = *snapshot().cell(Cat::Recovery, Region::Other);
        let d = after.delta(&before);
        assert_eq!(d.write_ops, 1);
        assert_eq!(d.write_bytes, 1234);
        assert_eq!(d.read_ops, 1);
        assert_eq!(d.read_bytes, 100);
        assert_eq!(d.meta_ops, 1);
        assert!(after.max_op_bytes >= 1234);
    }

    #[test]
    fn slow_op_increments_slow_counter() {
        let before = snapshot().cell(Cat::Preview, Region::Other).slow_ops;
        record_region(
            Cat::Preview,
            Region::Other,
            OpKind::Read,
            1,
            Duration::from_millis(SLOW_OP_MS + 1),
        );
        let after = snapshot().cell(Cat::Preview, Region::Other).slow_ops;
        assert_eq!(after, before + 1);
    }

    #[test]
    fn ops_ring_is_bounded_at_cap() {
        for i in 0..(OPS_RING_CAP + 50) {
            record_region(Cat::Other, Region::Other, OpKind::Meta, i as u64, Duration::ZERO);
        }
        let ring = snapshot().ring;
        assert!(ring.len() <= OPS_RING_CAP);
        assert!(!ring.is_empty());
    }

    #[test]
    fn sample_jsonl_roundtrip() {
        let s = Sample {
            at_ms: 1_752_000_000_000,
            self_read_bps: 1,
            self_write_bps: 2,
            child_read_bps: 3,
            child_write_bps: 4,
            per_region: [(1, 2), (3, 4), (5, 6), (7, 8)],
            per_cat: vec![(9, 10); CAT_COUNT],
            procs: vec![ProcSample {
                pid: 1234,
                label: "chan".into(),
                tool: "streamlink".into(),
                purpose: "live capture".into(),
                region: Region::Recordings,
                read_bps: 11,
                write_bps: 12,
                total_read: 13,
                total_write: 14,
                descendants: 1,
            }],
            unattributed_bps: 15,
            db_bytes: 16,
            disks: vec![DiskSample { letter: 'A', read_bps: 17, write_bps: 18, queue_depth: 2 }],
        };
        let line = serde_json::to_string(&s).unwrap();
        let back: Sample = serde_json::from_str(&line).unwrap();
        assert_eq!(back.at_ms, s.at_ms);
        assert_eq!(back.per_region, s.per_region);
        assert_eq!(back.procs.len(), 1);
        assert_eq!(back.procs[0].pid, 1234);
        assert_eq!(back.disks[0].letter, 'A');
        assert_eq!(back.disks[0].queue_depth, 2);
    }

    #[test]
    fn child_registry_register_unregister() {
        let info = ChildInfo {
            label: "x".into(),
            tool: "ffmpeg".into(),
            purpose: "test".into(),
            region: Region::Other,
            proc_start: 0,
        };
        {
            let _g = track_child(99_999_999, info.clone());
            assert!(
                CHILDREN.lock().as_ref().map(|m| m.contains_key(&99_999_999)).unwrap_or(false)
            );
        }
        assert!(!CHILDREN.lock().as_ref().map(|m| m.contains_key(&99_999_999)).unwrap_or(false));
    }

    #[test]
    fn facade_sync_roundtrip_counts_bytes() {
        let dir = unique_temp_dir("sync");
        let file = dir.join("a.txt");
        let before = *snapshot().cell(Cat::Startup, Region::Temp);

        fs::write_sync(Cat::Startup, &file, b"hello world").unwrap();
        let data = fs::read_sync(Cat::Startup, &file).unwrap();
        assert_eq!(data, b"hello world");
        let meta = fs::metadata_sync(Cat::Startup, &file).unwrap();
        assert_eq!(meta.len(), 11);
        let renamed = dir.join("b.txt");
        fs::rename_sync(Cat::Startup, &file, &renamed).unwrap();
        fs::remove_file_sync(Cat::Startup, &renamed).unwrap();

        let d = snapshot().cell(Cat::Startup, Region::Temp).delta(&before);
        assert_eq!(d.write_ops, 1);
        assert_eq!(d.write_bytes, 11);
        assert_eq!(d.read_ops, 1);
        assert_eq!(d.read_bytes, 11);
        assert!(d.meta_ops >= 3); // metadata + rename + delete

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn facade_async_roundtrip_counts_bytes() {
        let dir = unique_temp_dir("async");
        let file = dir.join("a.txt");
        let before = *snapshot().cell(Cat::Detector, Region::Temp);

        fs::write(Cat::Detector, &file, b"abcdef").await.unwrap();
        let s = fs::read_to_string(Cat::Detector, &file).await.unwrap();
        assert_eq!(s, "abcdef");
        assert!(fs::try_exists(Cat::Detector, &file).await.unwrap());
        fs::remove_file(Cat::Detector, &file).await.unwrap();

        let d = snapshot().cell(Cat::Detector, Region::Temp).delta(&before);
        assert_eq!(d.write_ops, 1);
        assert_eq!(d.write_bytes, 6);
        assert_eq!(d.read_ops, 1);
        assert_eq!(d.read_bytes, 6);
        assert!(d.meta_ops >= 2); // try_exists + delete

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
