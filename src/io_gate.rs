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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::info;

/// Settings key for the post-processing disk throttle (multiplier of media
/// rate, stored as a decimal string; `0` = unthrottled).
pub const K_POSTPROC_READRATE: &str = "postproc_readrate";

/// Default `-readrate` multiplier: 30× media rate. A 6 Mbps stream remuxes at
/// ~22 MB/s read + ~22 MB/s write — a 13 GB take finishes in ~10 minutes while
/// leaving most of the drive's bandwidth to live captures.
pub const DEFAULT_READRATE: f64 = 30.0;

static LOCAL_PASS: OnceLock<Arc<Semaphore>> = OnceLock::new();
static CDN_MUX: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Configured readrate ×10 (fixed-point so it fits an atomic). 0 = off.
static READRATE_X10: AtomicU32 = AtomicU32::new((DEFAULT_READRATE * 10.0) as u32);
/// Set once if the installed ffmpeg rejects `-readrate` (pre-5.0).
static READRATE_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

async fn acquire(gate: &'static OnceLock<Arc<Semaphore>>, permits: usize, label: &str) -> OwnedSemaphorePermit {
    let sem = gate.get_or_init(|| Arc::new(Semaphore::new(permits))).clone();
    let start = Instant::now();
    let permit = sem.acquire_owned().await.expect("io gate semaphore closed");
    let waited = start.elapsed();
    if waited.as_secs() >= 5 {
        info!(
            "disk gate: {label} waited {}s for its turn (other bulk passes running)",
            waited.as_secs()
        );
    }
    permit
}

/// Acquire the single-permit gate for local full-file passes (remux, concat,
/// embed). Hold the permit for the duration of the ffmpeg run; drop to release.
pub async fn local_pass(label: &str) -> OwnedSemaphorePermit {
    acquire(&LOCAL_PASS, 1, label).await
}

/// Acquire the 2-permit gate for CDN-fed muxes (head backfill, VOD recovery).
pub async fn cdn_mux(label: &str) -> OwnedSemaphorePermit {
    acquire(&CDN_MUX, 2, label).await
}

/// Set the post-processing readrate multiplier (0 disables the throttle).
/// Called at startup from the persisted setting and again on settings save.
pub fn set_readrate(mult: f64) {
    READRATE_X10.store((mult.clamp(0.0, 1000.0) * 10.0).round() as u32, Ordering::Relaxed);
}

/// The `-readrate` multiplier to pass to local post-processing ffmpeg runs,
/// or `None` when disabled or known-unsupported by the installed ffmpeg.
pub fn readrate() -> Option<f64> {
    if READRATE_UNSUPPORTED.load(Ordering::Relaxed) {
        return None;
    }
    let x10 = READRATE_X10.load(Ordering::Relaxed);
    (x10 > 0).then(|| f64::from(x10) / 10.0)
}

/// Remember (process-wide) that this ffmpeg build rejects `-readrate`.
pub fn mark_readrate_unsupported() {
    if !READRATE_UNSUPPORTED.swap(true, Ordering::Relaxed) {
        info!("ffmpeg does not support -readrate (needs ffmpeg >= 5.0) — post-processing throttle disabled");
    }
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

    #[tokio::test]
    async fn local_pass_serializes() {
        let a = local_pass("a").await;
        // Second acquisition must not be immediately available.
        let sem = LOCAL_PASS.get().unwrap().clone();
        assert!(sem.clone().try_acquire_owned().is_err());
        drop(a);
        assert!(sem.try_acquire_owned().is_ok());
    }

    #[test]
    fn readrate_roundtrip_and_unsupported_latch() {
        set_readrate(12.5);
        assert_eq!(readrate(), Some(12.5));
        set_readrate(0.0);
        assert_eq!(readrate(), None);
        set_readrate(30.0);
        assert!(is_readrate_error("Unrecognized option 'readrate'."));
        assert!(!is_readrate_error("Invalid data found when processing input"));
        mark_readrate_unsupported();
        assert_eq!(readrate(), None);
    }
}
