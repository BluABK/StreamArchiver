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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    #[serde(default = "d_local_permits")]
    pub local_permits: u32,
    /// Concurrent CDN-fed muxes (head backfill, VOD recovery).
    #[serde(default = "d_cdn_permits")]
    pub cdn_permits: u32,
    /// ffmpeg `-readrate` multiplier for local passes; 0 = unthrottled.
    #[serde(default = "d_readrate")]
    pub readrate: f64,
    /// yt-dlp `--limit-rate` for non-live downloads landing on this disk
    /// (e.g. `4M`, `500K`); empty = unlimited.
    #[serde(default)]
    pub rate_limit: String,
}

impl Default for DiskLimits {
    fn default() -> Self {
        DiskLimits {
            local_permits: d_local_permits(),
            cdn_permits: d_cdn_permits(),
            readrate: d_readrate(),
            rate_limit: String::new(),
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
/// save). Gate permit counts adjust lazily on each gate's next acquisition.
pub fn set_disk_limits(cfg: DiskLimitsConfig) {
    *DISK_CFG.write() = Some(cfg);
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
    let cfg = DISK_CFG.read();
    let Some(cfg) = cfg.as_ref() else { return DiskLimits::default() };
    cfg.drives.get(&drive_key(path)).cloned().unwrap_or_else(|| cfg.default.clone())
}

// ===== Gates (per drive) =====

struct GateEntry {
    sem: Arc<Semaphore>,
    permits: usize,
}

type GateMap = parking_lot::Mutex<std::collections::HashMap<String, GateEntry>>;
static LOCAL_GATES: OnceLock<GateMap> = OnceLock::new();
static CDN_GATES: OnceLock<GateMap> = OnceLock::new();

/// Get (or create) the semaphore for `key`, adjusting its permit count to the
/// configured value: growth is immediate (`add_permits`); shrink acquires the
/// excess permits in the background and forgets them, so it takes effect as
/// running passes finish. Called on every acquisition — config changes apply
/// lazily without any runtime plumbing.
fn gate_sem(map: &'static OnceLock<GateMap>, key: &str, want: usize) -> Arc<Semaphore> {
    let want = want.max(1);
    let m = map.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut g = m.lock();
    let e = g.entry(key.to_string()).or_insert_with(|| GateEntry {
        sem: Arc::new(Semaphore::new(want)),
        permits: want,
    });
    match want.cmp(&e.permits) {
        std::cmp::Ordering::Greater => {
            e.sem.add_permits(want - e.permits);
            e.permits = want;
        }
        std::cmp::Ordering::Less => {
            let take = (e.permits - want) as u32;
            let sem = e.sem.clone();
            e.permits = want;
            tokio::spawn(async move {
                if let Ok(p) = sem.acquire_many_owned(take).await {
                    p.forget();
                }
            });
        }
        std::cmp::Ordering::Equal => {}
    }
    e.sem.clone()
}

/// Live status of the local gates, for progress messages: every pass holding
/// a permit right now (label + seconds held, longest first) and how many are
/// queued across all drives.
static LOCAL_HOLDERS: parking_lot::Mutex<Vec<(u64, String, Instant)>> =
    parking_lot::Mutex::new(Vec::new());
static NEXT_HOLDER_TOKEN: AtomicU64 = AtomicU64::new(1);
static LOCAL_WAITING: AtomicUsize = AtomicUsize::new(0);

/// `(holders (label, seconds held) longest-first, queue length)`.
pub fn local_gate_status() -> (Vec<(String, u64)>, usize) {
    let mut holders: Vec<(String, u64)> = LOCAL_HOLDERS
        .lock()
        .iter()
        .map(|(_, l, t)| (l.clone(), t.elapsed().as_secs()))
        .collect();
    holders.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
    (holders, LOCAL_WAITING.load(Ordering::Relaxed))
}

/// Held permit of a local gate; removes its holder entry on drop.
pub struct LocalPass {
    token: u64,
    _permit: OwnedSemaphorePermit,
}

impl Drop for LocalPass {
    fn drop(&mut self) {
        LOCAL_HOLDERS.lock().retain(|(t, ..)| *t != self.token);
    }
}

/// Decrements the waiting counter even when the acquiring future is dropped
/// mid-await (task aborted at shutdown).
struct WaitingGuard;
impl Drop for WaitingGuard {
    fn drop(&mut self) {
        LOCAL_WAITING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Acquire the local-passes gate of the disk `path` lives on (permit count
/// per Settings → per-disk I/O limits; default 1). Hold the returned guard
/// for the duration of the ffmpeg run; drop to release.
pub async fn local_pass(label: &str, path: &std::path::Path) -> LocalPass {
    let key = drive_key(path);
    let want = limits_for(path).local_permits as usize;
    let sem = gate_sem(&LOCAL_GATES, &key, want);
    LOCAL_WAITING.fetch_add(1, Ordering::Relaxed);
    let _wg = WaitingGuard;
    let start = Instant::now();
    let permit = sem.acquire_owned().await.expect("io gate semaphore closed");
    let waited = start.elapsed();
    if waited.as_secs() >= 5 {
        info!(
            "disk gate [{key}]: {label} waited {}s for its turn (other bulk passes running)",
            waited.as_secs()
        );
    }
    let token = NEXT_HOLDER_TOKEN.fetch_add(1, Ordering::Relaxed);
    LOCAL_HOLDERS.lock().push((token, label.to_string(), Instant::now()));
    LocalPass { token, _permit: permit }
}

/// [`local_pass`], but invokes `on_wait(waited_secs, holders, queue_len)`
/// every 5 s while queued — callers surface it as task progress so a queued
/// pass is visibly waiting (and on what) instead of looking stale.
pub async fn local_pass_with_progress(
    label: &str,
    path: &std::path::Path,
    mut on_wait: impl FnMut(u64, Vec<(String, u64)>, usize),
) -> LocalPass {
    let fut = local_pass(label, path);
    tokio::pin!(fut);
    let started = Instant::now();
    loop {
        tokio::select! {
            p = &mut fut => return p,
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                let (holders, waiting) = local_gate_status();
                on_wait(started.elapsed().as_secs(), holders, waiting);
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

/// Acquire the CDN-mux gate of the disk `path` lives on (permit count per
/// Settings → per-disk I/O limits; default 2). Head backfills and VOD
/// recoveries write at network speed — gentler than a local pass, but
/// unbounded stacking still saturates a drive.
pub async fn cdn_mux(label: &str, path: &std::path::Path) -> OwnedSemaphorePermit {
    let key = drive_key(path);
    let want = limits_for(path).cdn_permits as usize;
    let sem = gate_sem(&CDN_GATES, &key, want);
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

    #[tokio::test]
    async fn local_pass_serializes_per_drive() {
        // Distinct test drive letters so other tests' config doesn't interfere.
        let qa = Path::new(r"Q:\x\a.mkv");
        let a = local_pass("a", qa).await;
        // Same drive: second acquisition must not be immediately available…
        let sem = gate_sem(&LOCAL_GATES, "Q", 1);
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
        let mut cfg = DiskLimitsConfig::default();
        cfg.default.readrate = 30.0;
        cfg.drives.insert(
            "S".into(),
            DiskLimits { local_permits: 2, cdn_permits: 1, readrate: 0.0, rate_limit: "4M".into() },
        );
        set_disk_limits(cfg);
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
        let sem = gate_sem(&LOCAL_GATES, "S", 2);
        assert!(sem.try_acquire_owned().is_err());
        drop(p1);
        drop(p2);
        // Shrink to 1: gate_sem spawns the reducer; next config read wants 1.
        let mut cfg = disk_limits_config();
        cfg.drives.get_mut("S").unwrap().local_permits = 1;
        set_disk_limits(cfg);
        let _p = local_pass("s3", s).await;
        tokio::task::yield_now().await; // let the reducer task grab the excess
        let sem = gate_sem(&LOCAL_GATES, "S", 1);
        assert!(sem.try_acquire_owned().is_err());
        // Restore defaults so other tests see a clean config.
        set_disk_limits(DiskLimitsConfig::default());
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
    }
}
