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
    if let Some(letter) = drive_letter(path) {
        if RECORDINGS_DRIVES.read().contains(&letter) {
            return Region::Recordings;
        }
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

    // DB guard drops fire hundreds of times a second under load and carry no
    // path — counters only, unless slow (a slow DB hold is worth surfacing).
    if matches!(cat, Cat::Db) && !slow {
        return;
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

// ===== Instrumented filesystem facade =====

/// Timed, counted wrappers over `std::fs`/`tokio::fs`. Every function takes
/// the [`Cat`] first, then mirrors the wrapped function's signature. Async
/// variants keep the std names; sync variants carry a `_sync` suffix.
///
/// `clippy.toml` disallows the raw functions everywhere else, so this module
/// is the only place in the app that touches `std::fs`/`tokio::fs` directly.
#[allow(clippy::disallowed_methods)]
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
