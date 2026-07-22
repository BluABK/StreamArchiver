//! Download supervisor + per-tool adapters.
//!
//! When the scheduler reports a monitor live, the supervisor (bounded by a
//! global concurrency semaphore) spawns the configured tool as a child process,
//! captures its stderr into a ring buffer, waits for exit, classifies the
//! outcome, optionally remuxes TS -> MKV, and records the run in the store. A
//! Win32 Job Object guarantees the whole process tree is killed on stop/exit.
//!
//! Default container is MKV (never MP4): streamlink records to `.ts` then remuxes
//! losslessly to `.mkv`; yt-dlp merges straight to `.mkv`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::detectors::{DetectContext, DetectItem, DetectOutcome};
use crate::events::{AppEvent, EventTx, LiveSignal, ManualCommand};
use crate::iomon::Cat;
use crate::models::{
    AuthKind, Container, DetachedKind, DetachedRow, DetectionMethod, K_FILENAME_MEDIA,
    MediaInfoMode, Monitor, MonitorWithChannel, Platform, Recording, SabrCodecPref, Tool, Video,
    now_unix,
};
use crate::platform::DetachedJob;
use crate::store::Store;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How a [`Supervisor::run_process`] call registers itself in the persistent
/// detached-process registry, so a later launch can re-attach if the tool
/// outlives the app. `ref_id == 0` (e.g. a recording row that failed to insert)
/// disables registration — there'd be nothing to reconcile against.
#[derive(Clone)]
struct DetachReg {
    kind: DetachedKind,
    ref_id: i64,
    monitor_id: Option<i64>,
    take_group: Option<String>,
    /// The take's start time (recording/video started_at), not the spawn time —
    /// the timeline anchor a re-attach finalize needs for the stem + lost-time.
    started_at: i64,
    /// True only for the DASH companion (occupies the secondary active map).
    secondary: bool,
    stream_id: Option<String>,
    went_live_at: Option<i64>,
}

/// Monitors currently being recorded, mapped to their child PID (0 until the
/// process has spawned). Shared with the scheduler (so it doesn't re-trigger an
/// active recording) and used at shutdown to kill the process trees.
pub type ActiveSet = Arc<Mutex<HashMap<i64, u32>>>;

/// video_id -> download progress fraction (0.0..=1.0), for the UI progress bar.
/// Populated by the tool's progress output while a video downloads; cleared when
/// it finishes. (Live recordings have no meaningful total, so they don't use it.)
pub type VideoProgress = Arc<Mutex<HashMap<i64, f32>>>;

/// video_id -> current download speed in bytes/sec, for the UI Speed column.
/// Populated alongside `VideoProgress` from the tool's progress output; cleared
/// when the download finishes.
pub type VideoSpeed = Arc<Mutex<HashMap<i64, f64>>>;

/// monitor_id -> unix time the current ad break ends, while one is playing
/// (Twitch+streamlink). Lets the UI tint a row "ad running"; entries expire
/// naturally (now >= value) and are removed when the recording ends.
pub type AdActive = Arc<Mutex<HashMap<i64, i64>>>;

/// monitor_id -> rec_id of a take whose capture process has ENDED but whose
/// finalize (remux/promote, possibly queued behind the disk gate for hours)
/// hasn't completed yet. The monitor stays in [`ActiveSet`] for that whole
/// span, so without this the UI would keep saying "recording" long after the
/// stream ended (the Arielle/Bao/crelly report, 2026-07-13). Written by the
/// in-session, re-attach, and SABR-resume finalize paths; read by the Streams
/// grid to show "finalizing" instead.
pub type Finalizing = Arc<Mutex<HashMap<i64, i64>>>;

/// Key for the per-stream SABR stall maps: `(monitor_id, stream_id)`. Fully
/// per-stream when a video ID is known; degrades to per-monitor otherwise.
type SabrKey = (i64, Option<String>);

const RING_MAX_LINES: usize = 80;
/// How often the from-start catch-up watcher probes the growing capture.
const CATCHUP_PROBE_INTERVAL_SECS: u64 = 20;
/// Treat a from-start capture as caught up once its media is within this many
/// seconds of the live edge (absorbs fragment lag + approximate go-live times).
const CATCHUP_TOLERANCE_SECS: i64 = 45;
/// Fixed grace period `head_backfill_job` waits before doing anything, so the
/// CDN's live-VOD folder can appear and streamlink's own `--hls-live-restart`
/// can finish its own rewind attempt — unrelated to how late the recording
/// joined relative to go-live (even an instant join needs this settle time).
/// Exposed so the UI can render "starts in ~Ns" for a queued take.
pub(crate) const HEAD_BACKFILL_SETTLE_SECS: i64 = 120;

/// Wiring for recording advertisement breaks parsed from a live capture's log.
///
/// Only Twitch+streamlink recordings pass this: streamlink filters Twitch ad
/// segments out of the capture (each break becomes a hard cut) and logs each one
/// as `Detected advertisement break of N second(s)`. yt-dlp/ffmpeg have no
/// equivalent, and on-demand video downloads never set it.
struct AdSink {
    store: Arc<Store>,
    events: EventTx,
    monitor_id: i64,
    /// Always > 0 (the sink is only built for a real recording row).
    recording_id: i64,
    /// Take start (unix secs); the live-edge wall-clock fallback anchor.
    started_at: i64,
    /// Broadcast go-live time when known — a better fallback anchor than the
    /// process start for capture-from-start/DVR takes (their file timeline begins
    /// at go-live, not at recording start).
    went_live_at: Option<i64>,
    /// Whether this take rewinds to the broadcast start (DVR), which decides the
    /// fallback anchor.
    from_start: bool,
    /// The growing capture file; its media duration is the true cut position
    /// (ad segments are filtered out, so captured content == the finished file).
    capture_path: PathBuf,
    /// Shared map the UI reads to tint a row while an ad is playing.
    ad_active: AdActive,
    /// Twitch channel login, when known — feeds `Supervisor::spawn_ad_probe`
    /// (the live-manifest ad detector, see `ad_probe.rs`) alongside the
    /// log-line detector above; `None` skips the probe (URL didn't parse).
    login: Option<String>,
}

/// The plan for one recording: the command to run plus the files involved.
#[derive(Debug, Clone)]
pub struct DownloadPlan {
    pub program: String,
    pub args: Vec<String>,
    /// File the tool writes directly.
    pub capture_path: PathBuf,
    /// Final file after any remux (== capture_path when no remux).
    pub final_path: PathBuf,
    pub remux_to_mkv: bool,
    /// True when the tool writes its own thumbnail inline (normal yt-dlp with
    /// `--write-thumbnail`). When false and a thumbnail is wanted, the supervisor
    /// fetches it over HTTP instead (streamlink, ffmpeg, and SABR captures).
    pub writes_own_thumbnail: bool,
    /// Download mode string, e.g. "live", "sabr", "dash", "hybrid", "hybrid-dash", "direct", "vod", "chat".
    pub mode: String,
}

mod ad_probe;
mod alerts;
mod backfill;
mod cache;
mod finalize;
mod gap_recover;
mod lock_culprit;
mod naming;
mod plan;
mod process;
mod remux;
mod supervisor;
mod tools;
mod vod;

#[allow(unused_imports)]
use {ad_probe::*, alerts::*, backfill::*, gap_recover::*, supervisor::*, vod::*};
pub use alerts::alert_category;
pub use ad_probe::K_AD_PROBE;
pub use gap_recover::K_GAP_RECOVER;
#[allow(unused_imports)]
use lock_culprit::*;
pub use cache::*;
pub use finalize::*;
pub use naming::*;
pub use plan::*;
pub use process::*;
pub use remux::*;
pub use tools::*;

/// Head-backfill's "missed" length: prefer measuring the capture's own
/// duration (`captured`) against `reference` — accounts for any partial
/// rewind — falling back to the plain start delay when the duration can't be
/// measured. `reference` must be a fixed point in time for an already-
/// finished capture (its `ended_at`), not a live `now_unix()` — the capture's
/// duration is static once it's done, so pairing it with an ever-advancing
/// "now" would make `missed` grow unboundedly with how long ago the take
/// ended (see `head_backfill_job`'s `missed_reference` parameter).
fn compute_missed_secs(went_live_at: i64, started_at: i64, captured: Option<i64>, reference: i64) -> i64 {
    match captured {
        Some(c) => (reference - went_live_at - c).max(0),
        None => (started_at - went_live_at).max(0),
    }
}

/// One MPEG-TS 33-bit PTS wrap (2^33 ticks at 90 kHz), ≈ 26.5 hours.
const MPEGTS_PTS_WRAP_SECS: f64 = 8_589_934_592.0 / 90_000.0;

/// The exact stream position (seconds since go-live) where the live capture's
/// first frame sits, derived from PTS instead of wall-clock arithmetic:
/// Twitch's live segments and DVR-playlist segments share the broadcast's own
/// MPEG-TS timeline, so `capture start_time − DVR segment-0 start_time` is the
/// precise head/live splice point. The wall-clock `estimate` (today's
/// `compute_missed_secs` result) systematically overshoots it by the broadcast
/// latency (~5-15s), duplicating that much media at the `full.mkv` seam.
///
/// `None` when the PTS delta disagrees with the estimate by more than 60s —
/// that rejects a wrong-timeline pairing wholesale: a remuxed capture whose
/// timestamps were reset to ~0, a non-TS container, a PTS discontinuity, or a
/// wrap we can't attribute. A single 33-bit wrap (streams > 26.5h) is
/// corrected for before the check.
fn pts_capture_offset(live_start: f64, seg0_start: f64, estimate_secs: f64) -> Option<f64> {
    if !live_start.is_finite() || !seg0_start.is_finite() {
        return None;
    }
    let mut delta = live_start - seg0_start;
    if delta < 0.0 {
        delta += MPEGTS_PTS_WRAP_SECS;
    }
    // A capture joining a >26.5h-old stream can also be one wrap short in the
    // positive direction; pick whichever candidate lands nearest the estimate.
    let wrapped = delta + MPEGTS_PTS_WRAP_SECS;
    if (wrapped - estimate_secs).abs() < (delta - estimate_secs).abs() {
        delta = wrapped;
    }
    ((delta - estimate_secs).abs() <= 60.0).then_some(delta)
}
#[derive(Clone)]
pub struct Supervisor {
    store: Arc<Store>,
    events: EventTx,
    active: ActiveSet,
    /// monitor_id -> child PID of the DASH companion capture (dual capture). The
    /// primary capture occupies `active`; this holds the second concurrent process
    /// so manual stop / shutdown can kill both.
    active_secondary: ActiveSet,
    /// video_id -> child PID of in-flight on-demand video downloads.
    active_videos: ActiveSet,
    /// video_id -> live download progress fraction, for the UI bar.
    video_progress: VideoProgress,
    /// video_id -> live download speed (bytes/sec), for the UI Speed column.
    video_speed: VideoSpeed,
    /// video_ids whose download was asked to stop (so it finalizes as `stopped`).
    stopping_videos: Arc<Mutex<HashSet<i64>>>,
    /// monitor_ids whose live recording was manually stopped (so it finalizes as
    /// `stopped` rather than `failed` even when bytes = 0).
    stopping_monitors: Arc<Mutex<HashSet<i64>>>,
    /// `(kind, ref_id)` pairs the stall watchdog killed — consumed at finalize
    /// for classification. Deliberately NOT the user-stop tombstones: reusing
    /// those broke the SABR live-edge fallback, skipped backoff, and (via the
    /// secondary finalize's `contains`) could leave a permanent tombstone that
    /// silently skipped the channel's next take.
    stall_killed: Arc<Mutex<HashSet<(DetachedKind, i64)>>>,
    /// monitor_id -> broadcast key of the last blacklist-veto notification, so
    /// the "recording suppressed" event fires once per broadcast rather than on
    /// every poll while the stream stays live.
    blocked_notified: Arc<Mutex<HashMap<i64, String>>>,
    /// monitor_id -> child PID of in-flight live-chat sidecar downloads.
    /// Shared with AppCore so the UI can show the chat-active indicator.
    pub active_chats: Arc<Mutex<HashMap<i64, u32>>>,
    /// Capture-ended-but-finalize-pending takes (see [`Finalizing`]).
    /// Shared with AppCore so the UI shows "finalizing" instead of "recording".
    finalizing: Finalizing,
    /// monitor_ids whose chat download was asked to stop.
    stopping_chats: Arc<Mutex<HashSet<i64>>>,
    shutdown: Arc<AtomicBool>,
    /// A clone of the manual-command sender, so background poller tasks (the VOD
    /// archivers) can enqueue+start a Video download without a Supervisor handle.
    manual_tx: mpsc::UnboundedSender<ManualCommand>,
    /// Shared detection context for on-demand (manual Start) liveness checks.
    ctx: Arc<DetectContext>,
    /// monitor_id -> unix time the current ad break ends (for the UI row tint).
    ad_active: AdActive,
    sem: Arc<Semaphore>,
    backoff: Arc<Mutex<HashMap<i64, BackoffEntry>>>,
    /// Streams where SABR live-from-start hit the DVR window limit ("not near
    /// live head"). The next attempt falls back to live-edge so we at least
    /// capture the ongoing stream instead of looping forever. Cleared on a
    /// successful capture (bytes > 0); in-memory only (resets on app restart).
    ///
    /// Keyed by `(monitor_id, stream_id)`. When a stable video ID is available
    /// (scraped from `videoDetails.videoId` or the YouTube Data API), the key is
    /// fully per-stream, so cross-broadcast stickiness cannot occur. When
    /// `stream_id` is `None` (non-YouTube monitors or degraded scrape), the key
    /// degrades to per-monitor — same as the previous behaviour.
    sabr_dvr_exceeded: Arc<Mutex<HashSet<SabrKey>>>,
    /// Per-stream count of *consecutive* from-start SABR stalls. Once it reaches
    /// [`SABR_STALL_FALLBACK_TRIES`] (with deep-rewind on) the stream is added to
    /// `sabr_dvr_exceeded` so the next attempt captures the live edge. Reset on
    /// any non-stall outcome (success, ended, manual) so it tracks back-to-back
    /// stalls only; in-memory only. Keyed by `(monitor_id, stream_id)`.
    sabr_stall_count: Arc<Mutex<HashMap<SabrKey, u32>>>,
    /// (channel_name, platform_str, account_slug) triples for which an asset-fetch
    /// task is currently in flight. Prevents stacking duplicate fetches when the
    /// user clicks "Re-fetch" repeatedly or a periodic fetch fires while one is
    /// already running — while letting a same-platform sibling account fetch
    /// concurrently.
    running_asset_fetches: Arc<Mutex<HashSet<(String, String, String)>>>,
    /// rec_ids with a head+live concat currently in flight. Every finalize path
    /// (finish, re-attach, head-backfill completion, supersede, startup healing)
    /// spawns `maybe_concat_backfill` independently and they all derive the SAME
    /// `.cache\{stem}.full.mkv` temp path — two racing joins would interleave
    /// writes into one file AND double the multi-GB disk pass. First caller
    /// wins; later ones return immediately (the winner's own post-concat DB
    /// re-check keeps the result correct).
    running_concats: Arc<Mutex<HashSet<i64>>>,
    /// Streams (keyed `"{monitor_id}:{stream_id}"`) already restarted once by
    /// the quality-upgrade watcher — an upgrade fires at most once per stream
    /// so a flapping rendition list can never cause a restart loop.
    quality_upgraded: Arc<Mutex<HashSet<String>>>,
    /// Monitors whose automatic restarts are suppressed after a user Stop —
    /// see [`StopHold`]. Shared with the UI (state-cell badge) and persisted
    /// across restarts (`K_STOP_HOLDS`).
    stop_holds: StopHolds,
    /// rec_ids with a lost-segment recovery job in flight (the log scanner and
    /// the finalize sweep both spawn `gap_recover_job`; first caller wins).
    gap_jobs: Arc<Mutex<HashSet<i64>>>,
}

/// Why automatic restarts are suppressed for a monitor after a user Stop.
/// Blocks polls and pushes always; blocks trigger-word matches too unless
/// `allow_triggers` is set (the "Stop (allow triggers)" action) — a manual
/// ▶ Start always clears it regardless.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum StopHold {
    /// Until a NEW broadcast appears: a different stream id, or a newer
    /// go-live time (i.e. the channel went offline and live again).
    FreshStream {
        stream_id: Option<String>,
        went_live_at: Option<i64>,
        /// A trigger-word match can still start a fresh recording during
        /// the hold (e.g. an impromptu karaoke segment mid-broadcast) —
        /// only plain Auto-record (polls/pushes) is suppressed. `#[serde(default)]`
        /// so a hold persisted before this field existed loads as `false`
        /// (the original, fully-blocking behavior).
        #[serde(default)]
        allow_triggers: bool,
    },
    /// Until this unix timestamp, regardless of offline/online cycles.
    Until {
        at: i64,
        /// See `FreshStream::allow_triggers`.
        #[serde(default)]
        allow_triggers: bool,
    },
}

impl StopHold {
    pub fn allow_triggers(&self) -> bool {
        match self {
            StopHold::FreshStream { allow_triggers, .. } | StopHold::Until { allow_triggers, .. } => {
                *allow_triggers
            }
        }
    }
}

pub type StopHolds = Arc<Mutex<HashMap<i64, StopHold>>>;

/// Settings key persisting the stop-holds across restarts (JSON pairs).
const K_STOP_HOLDS: &str = "manual_stop_holds";

fn load_stop_holds(store: &Store) -> HashMap<i64, StopHold> {
    store
        .get_setting(K_STOP_HOLDS)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<Vec<(i64, StopHold)>>(&v).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn persist_stop_holds(store: &Store, holds: &HashMap<i64, StopHold>) {
    let pairs: Vec<(&i64, &StopHold)> = holds.iter().collect();
    if let Ok(json) = serde_json::to_string(&pairs) {
        let _ = store.set_setting(K_STOP_HOLDS, &json);
    }
}

/// RAII guard removing a rec_id from [`Supervisor::running_concats`] on drop,
/// so every early-return in `maybe_concat_backfill` releases the slot.
struct ConcatGuard {
    set: Arc<Mutex<HashSet<i64>>>,
    id: i64,
}

impl Drop for ConcatGuard {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

#[derive(Clone, Copy)]
struct BackoffEntry {
    fails: u32,
    until: Instant,
}
/// Settings key: restart a young Twitch `best` capture when a better rendition
/// appears after join (`"0"` disables; default on — see
/// `Supervisor::quality_upgrade_watcher`).
pub const K_QUALITY_UPGRADE: &str = "quality_upgrade_restart";

#[cfg(test)]
pub(crate) mod test_util {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use crate::models::{Channel, Container, DetectionMethod, Monitor, Tool};

    pub fn row(tool: Tool, container: Container, platform: Platform) -> MonitorWithChannel {
        // The instance URL now drives the platform-specific plan, so give it one
        // that matches `platform`.
        let url = match platform {
            Platform::Twitch => "https://twitch.tv/cool",
            Platform::YouTube => "https://youtube.com/@cool",
            Platform::Kick => "https://kick.com/cool",
            Platform::Nrk => "https://tv.nrk.no/direkte/nrk1",
            Platform::Nebula => "https://nebula.tv/videos/cool",
            Platform::Generic => "https://example.com/cool",
        };
        MonitorWithChannel {
            channel: Channel {
                id: 1,
                name: "Cool Streamer".into(),
                url: url.into(),
                platform,
                created_at: 0,
                color: String::new(),
                preferred_asset: None,
                enabled: true,
                automation_enabled: true,
            },
            monitor: Monitor {
                id: 7,
                channel_id: 1,
                url: url.into(),
                enabled: true,
                automation_enabled: true,
                tool,
                detection_method: DetectionMethod::TwitchApi,
                poll_interval_secs: 60,
                quality: "best".into(),
                output_dir: "C:/rec".into(),
                filename_template: "{name}_{date}_{time}".into(),
                container,
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
            },
            last_recording_started: None,
            last_recording_ended: None,
            last_recording_status: None,
            last_recording_went_live: None,
            last_recording_went_live_approx: false,
            last_recording_lost_secs: None,
            last_recording_ad_count: 0,
            last_recording_ad_secs: 0,
            last_recording_meta_changes: 0,
            last_recording_title: String::new(),
            last_recording_category: String::new(),
            last_recording_log: String::new(),
            last_recording_trigger: String::new(),
            ad_free_sub: None,
            recording_count: 0,
            next_stream_at: None,
            next_stream_title: String::new(),
            last_title: String::new(),
            last_game: String::new(),
            last_thumbnail_url: String::new(),
            last_viewers: -1,
            live_collab: None,
            capture_offline: false,
            last_tags: String::new(),
            last_language: String::new(),
        }
    }
    pub fn video(tool: Tool, url: &str) -> Video {
        Video {
            id: 1,
            url: url.into(),
            title: "Clip".into(),
            channel: String::new(),
            platform: Platform::detect(url),
            tool,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_missed_secs_matches_reference_not_wall_clock() {
        // Went live at t=0, this take started (and, if finished, ended) at
        // t=1000 having captured 100s of footage (e.g. a partial rewind).
        let went_live_at = 0;
        let started_at = 1000;
        let ended_at = 1200;
        let captured = Some(100);

        // Still-live case: reference IS "now" (whatever it currently is) --
        // matches today's pre-existing behavior of pairing a growing capture
        // with a live now_unix().
        assert_eq!(compute_missed_secs(went_live_at, started_at, captured, 1300), 1200);

        // Finished-take case: using the take's fixed ended_at as the
        // reference gives a stable, correct answer: 1100.
        assert_eq!(compute_missed_secs(went_live_at, started_at, captured, ended_at), 1100);
        // The bug this fixes: pairing a static `captured` duration with a
        // live wall-clock reference instead of the fixed ended_at inflates
        // "missed" the longer after the fact you (re-)trigger it -- a manual
        // backfill an entire day later would wrongly see ~87,500 missed
        // seconds instead of the true, unchanging 1100.
        let one_day_later = ended_at + 86_400;
        assert_eq!(compute_missed_secs(went_live_at, started_at, captured, one_day_later), 87_500);
        assert_ne!(
            compute_missed_secs(went_live_at, started_at, captured, one_day_later),
            compute_missed_secs(went_live_at, started_at, captured, ended_at),
        );

        // No measurable duration -> plain start-delay fallback, independent
        // of any reference.
        assert_eq!(compute_missed_secs(went_live_at, started_at, None, 999_999), started_at);
    }

    #[test]
    fn stop_hold_allow_triggers_roundtrips_and_defaults_on_old_json() {
        let store = Store::open_in_memory().unwrap();

        // Round-trip through the settings JSON, both variants, both flags.
        let holds: HashMap<i64, StopHold> = HashMap::from([
            (1, StopHold::Until { at: 1_000, allow_triggers: true }),
            (2, StopHold::FreshStream { stream_id: Some("s".into()), went_live_at: Some(5), allow_triggers: false }),
        ]);
        persist_stop_holds(&store, &holds);
        let loaded = load_stop_holds(&store);
        assert!(loaded.get(&1).unwrap().allow_triggers());
        assert!(!loaded.get(&2).unwrap().allow_triggers());

        // A FreshStream hold persisted before this field existed (no
        // "allow_triggers" key at all) must still load — defaulting to the
        // original, fully-blocking behavior — not silently vanish.
        store
            .set_setting(
                K_STOP_HOLDS,
                r#"[[3,{"FreshStream":{"stream_id":null,"went_live_at":null}}]]"#,
            )
            .unwrap();
        let loaded = load_stop_holds(&store);
        assert!(!loaded.get(&3).unwrap().allow_triggers());
    }

    #[test]
    fn pts_capture_offset_exact_within_sanity_window() {
        // The Octopimp case: wall-clock said 377s missed, PTS says the capture
        // actually joined at 371.4s -- the ~6s broadcast-latency overshoot.
        let got = pts_capture_offset(371.433, 0.033, 377.0).unwrap();
        assert!((got - 371.4).abs() < 0.01);

        // DVR playlists whose timeline doesn't start at ~0 still work -- only
        // the delta matters.
        let got = pts_capture_offset(1500.0, 1128.6, 377.0).unwrap();
        assert!((got - 371.4).abs() < 0.01);
    }

    #[test]
    fn pts_capture_offset_rejects_wrong_timelines() {
        // Remuxed MKV (timestamps reset to ~0) probed by mistake: delta ~0
        // vs an estimate of 377s -> rejected, caller falls back to wall-clock.
        assert_eq!(pts_capture_offset(0.0, 0.033, 377.0), None);
        // Disagreement beyond the 60s window (PTS discontinuity, wrong file).
        assert_eq!(pts_capture_offset(500.0, 0.0, 377.0), None);
        // Non-finite probes never pass.
        assert_eq!(pts_capture_offset(f64::NAN, 0.0, 377.0), None);
        // Just inside the window still passes (latency can be large).
        assert!(pts_capture_offset(420.0, 0.0, 377.0).is_some());
    }

    #[test]
    fn pts_capture_offset_corrects_33bit_wrap() {
        // A capture joining >26.5h in: the raw 90kHz PTS wrapped, so the naive
        // delta is hugely negative; one wrap forward recovers the true offset.
        let estimate = MPEGTS_PTS_WRAP_SECS + 400.0;
        let live_start = 402.0; // wrapped back near zero
        let got = pts_capture_offset(live_start, 0.0, estimate).unwrap();
        assert!((got - (MPEGTS_PTS_WRAP_SECS + 402.0)).abs() < 0.01);

        // Positive-but-one-wrap-short delta (seg0 sits just above the capture's
        // wrapped PTS) also lands on the wrap-corrected candidate.
        let got = pts_capture_offset(402.0, 1.0, estimate).unwrap();
        assert!((got - (MPEGTS_PTS_WRAP_SECS + 401.0)).abs() < 0.01);
    }
}
