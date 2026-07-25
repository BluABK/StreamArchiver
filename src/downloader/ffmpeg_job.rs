//! Restart-survival for the long `ffmpeg -c copy` post-processing passes
//! (chapters/thumbnail embed, remux, gap-splice/head-backfill concat, the
//! head-backfill split-part merge) — the same problem the `detached_process`
//! registry solves for capture/download tools (`downloader/process.rs`), but
//! for these jobs instead. A registry row (`FfmpegJobRow`) is written right
//! after spawn and cleared at finalize.
//!
//! Unlike the capture/download registry, there is no separate startup
//! reconcile pass here: every one of these jobs is either re-driven by an
//! existing startup sweep (chapters, gap-splice) or only ever started
//! on-demand by the user (remux, thumbnail embed, split-merge) — neither of
//! which changes with this feature. Instead, [`adopt_or_clear_prior_ffmpeg_job`]
//! is the one check every ffmpeg-spawning call site runs FIRST, before
//! building a fresh `Command`: it transparently waits out (or discovers
//! already-finished) a process that outlived a restart, so the surrounding
//! business logic — chapter computation, gap-splice's PTS verification,
//! head-backfill's duration check — never needs to know whether this run is
//! fresh or resumed, and a second writer never races the first against the
//! same `.tmp` file.
//!
//! Added after a 12+ hour, 27%-done chapters-embed pass (Nihmune, throttled
//! by the shared disk-gate) was lost outright to an app restart.

use super::*;
use super::process::line_aligned_tail_offset;
use super::remux::media_duration_secs;
use crate::platform::DetachedJob;

/// Build the named Job Object + spawn `cmd` so the child can survive an app
/// restart (no `kill_on_drop`, no `KILL_ON_JOB_CLOSE` job — same mechanism
/// `run_process` uses for capture/download tools), and persist an
/// `FfmpegJobRow` so a relaunch can find it (skipped when `ref_id == 0`).
/// `cmd` must already have every arg/stdio set except `kill_on_drop` (this
/// fn always forces it off); the caller keeps driving the returned `Child`
/// exactly as a normal spawn (stdout/stderr/wait), and should call
/// [`finish_ffmpeg_job`] once it succeeds.
#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_ffmpeg_job(
    store: &Store,
    kind: FfmpegJobKind,
    ref_id: i64,
    mut cmd: Command,
    tmp_path: &Path,
    final_path: &Path,
    progress_log: &str,
    total_secs: Option<i64>,
) -> std::io::Result<(tokio::process::Child, Option<DetachedJob>)> {
    cmd.kill_on_drop(false);
    let job_name = format!("Local\\StreamArchiver_ffmpeg_{}_{ref_id}", kind.as_str());
    let job = DetachedJob::create(&job_name).ok();
    let child = cmd.spawn()?;
    if let Some(j) = &job
        && let Err(e) = j.assign_child(&child)
    {
        warn!("ffmpeg job: job assign failed: {e:#}");
    }
    let pid = child.id().unwrap_or(0);
    let proc_start = crate::platform::process_start_time(pid).unwrap_or(0);
    if ref_id != 0 && pid != 0 {
        let row = FfmpegJobRow {
            kind,
            ref_id,
            pid,
            proc_start,
            job_name,
            tmp_path: tmp_path.to_string_lossy().into_owned(),
            final_path: final_path.to_string_lossy().into_owned(),
            progress_log: progress_log.to_string(),
            total_secs,
            started_at: now_unix(),
            spawn_build: crate::version::build_id().to_string(),
        };
        if let Err(e) = store.register_ffmpeg_job(&row) {
            warn!("ffmpeg job: register failed: {e:#}");
        }
    }
    Ok((child, job))
}

/// Drop the registry row once a job has finalized in-session. `job` is
/// killed defensively (a `-c copy` pass spawns no children of its own, so
/// this should be a no-op) before its handle closes.
pub(super) fn finish_ffmpeg_job(store: &Store, kind: FfmpegJobKind, ref_id: i64, job: Option<DetachedJob>) {
    if let Some(j) = job {
        j.kill();
    }
    let _ = store.clear_ffmpeg_job(kind, ref_id);
}

/// Whether a `.tmp` output is complete enough to treat as a finished ffmpeg
/// pass without re-running it — pure so it's directly unit-testable. `None`
/// for either duration means "can't verify," which must never be treated as
/// complete.
pub(super) fn ffmpeg_job_tmp_is_complete(total_secs: Option<i64>, tmp_duration_secs: Option<i64>) -> bool {
    match (total_secs, tmp_duration_secs) {
        (Some(total), Some(tmp)) if total > 0 => tmp as f64 >= total as f64 * 0.99,
        _ => false,
    }
}

/// Check the registry for a previous attempt at this exact `(kind, ref_id)`
/// before a caller builds a fresh ffmpeg pass targeting `tmp_path`. Ground
/// truth for "is a prior attempt still alive" is the same PID + creation-time
/// check `reconcile_detached` uses for capture/download tools — no lock
/// files, no command-line matching.
///
/// - No row, or a dead one whose `tmp_path` doesn't look complete (cleared
///   here): returns `false` — the caller should proceed with a normal fresh
///   spawn.
/// - A still-alive row: waits for it to exit (tailing `progress_log` for a
///   live percentage when `progress` is given and the row has one, so a
///   restart doesn't blank the Active panel's readout for the rest of a long
///   pass) before falling through to the check below.
/// - Either way, once nothing else is running: if `tmp_path` now looks
///   complete against `total_secs`, returns `true` — the caller should skip
///   spawning ffmpeg entirely and use `tmp_path` as-is. This covers both "the
///   adopted process finished successfully while we waited" and "a separate
///   one already finished while the app was down."
pub(super) async fn adopt_or_clear_prior_ffmpeg_job(
    store: &Store,
    kind: FfmpegJobKind,
    ref_id: i64,
    tmp_path: &Path,
    total_secs: Option<i64>,
    shutdown: &Arc<AtomicBool>,
    progress: Option<(EventTx, u64)>,
) -> bool {
    if ref_id == 0 {
        return false;
    }
    let Some(row) = store
        .list_ffmpeg_jobs()
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.kind == kind && r.ref_id == ref_id)
    else {
        return false;
    };
    let alive = row.proc_start != 0
        && crate::platform::pid_alive(row.pid)
        && crate::platform::process_start_time(row.pid) == Some(row.proc_start);
    if alive {
        info!(
            kind = kind.as_str(),
            ref_id,
            pid = row.pid,
            "ffmpeg job: re-attaching to a still-running pass from before a restart \
             instead of starting a duplicate"
        );
        // Register with the I/O monitor for the whole wait, exactly like a
        // fresh spawn's own `track_tool` call would (which never runs here —
        // we never call `Command::spawn` ourselves for an adopted process) —
        // otherwise it's invisible in the Process Manager for the entire
        // remainder of a long pass, same fix `adopt_detached` already needed
        // for re-attached capture/download tools.
        let _io_guard = crate::iomon::track_child(
            row.pid,
            crate::iomon::ChildInfo {
                label: tmp_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                tool: "ffmpeg".to_string(),
                purpose: format!("{} (re-attached)", kind.as_str()),
                region: crate::iomon::classify(tmp_path),
                proc_start: row.proc_start,
            },
        );
        let done = Arc::new(AtomicBool::new(false));
        let tail = (!row.progress_log.is_empty()).then(|| {
            let events = progress.clone().map(|(tx, _)| tx);
            let task_id = progress.as_ref().map(|(_, id)| *id).unwrap_or(0);
            let log = PathBuf::from(&row.progress_log);
            let done = done.clone();
            let total_secs = row.total_secs;
            // No pacing watchdog during an adopted wait — we're just waiting
            // for someone else's process to exit, not driving our own retry
            // logic — so the shared cell is write-only here.
            let last_us = Arc::new(Mutex::new(None));
            tokio::spawn(async move {
                let offset = line_aligned_tail_offset(&log).await;
                tail_ffmpeg_progress(log, offset, total_secs, task_id, events, done, last_us).await;
            })
        });
        let pid = row.pid;
        let shutdown = shutdown.clone();
        let _ = tokio::task::spawn_blocking(move || crate::platform::wait_pid(pid, &shutdown)).await;
        done.store(true, Ordering::SeqCst);
        if let Some(t) = tail {
            let _ = t.await;
        }
    }
    let dur = media_duration_secs(tmp_path).await;
    let complete = ffmpeg_job_tmp_is_complete(total_secs, dur);
    if !complete {
        let _ = store.clear_ffmpeg_job(kind, ref_id);
    }
    complete
}

/// One decoded ffmpeg `-progress` block: accumulated key=value fields, closed
/// out by a `progress=continue`/`progress=end` line. Mirrors the inline
/// parsers this replaces in `remux.rs`.
#[derive(Default, Clone)]
pub(super) struct FfmpegProgressBlock {
    pub(super) frame: String,
    pub(super) fps: String,
    pub(super) speed: String,
    pub(super) pos: String,
    pub(super) out_time_us: Option<i64>,
}

/// Fold one `-progress` output line into `blk`; returns the completed block
/// (then resets `blk`) exactly on a `progress=continue`/`progress=end` line —
/// same grammar as the inline parsers in `remux.rs`/`backfill.rs`.
pub(super) fn parse_ffmpeg_progress_line(
    line: &str,
    blk: &mut FfmpegProgressBlock,
) -> Option<FfmpegProgressBlock> {
    let (k, v) = line.split_once('=')?;
    let (k, v) = (k.trim(), v.trim());
    match k {
        "frame" => blk.frame = v.to_string(),
        "fps" => blk.fps = v.to_string(),
        "speed" => blk.speed = v.to_string(),
        "out_time" => blk.pos = v.to_string(),
        "out_time_ms" => blk.out_time_us = v.parse::<i64>().ok(),
        "progress" => {
            let done = blk.clone();
            *blk = FfmpegProgressBlock::default();
            return Some(done);
        }
        _ => {}
    }
    None
}

/// Tail `progress_log` from `start_offset` (0 for a fresh spawn; a
/// line-aligned near-EOF offset for a re-attach), until `done` is set and the
/// file is drained to EOF. Same read-loop shape as `tail_log` (`process.rs`),
/// different line grammar. Every completed block updates `last_out_time_us`
/// (so a caller's own pacing watchdog — see `remux_ts_to_mkv_gated` — can
/// poll the media position without a second reader), and additionally emits
/// an `AppEvent::BackgroundTaskProgress` against `total_secs` when `events`
/// is given (`None` for jobs with no UI progress target — the pacing-only
/// case).
#[allow(clippy::too_many_arguments)]
pub(super) async fn tail_ffmpeg_progress(
    progress_log: PathBuf,
    start_offset: u64,
    total_secs: Option<i64>,
    task_id: u64,
    events: Option<EventTx>,
    done: Arc<AtomicBool>,
    last_out_time_us: Arc<Mutex<Option<i64>>>,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let total_us = total_secs.map(|s| s * 1_000_000);
    let mut file = loop {
        match crate::iomon::fs::open(Cat::LogRead, &progress_log).await {
            Ok(f) => break f,
            Err(_) => {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    if start_offset > 0 {
        let _ = file.seek(std::io::SeekFrom::Start(start_offset)).await;
    }
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut blk = FfmpegProgressBlock::default();
    let emit = |blk: &FfmpegProgressBlock| {
        *last_out_time_us.lock().unwrap() = blk.out_time_us;
        let Some(tx) = &events else { return };
        let progress = blk.out_time_us.and_then(|us| {
            total_us.filter(|&t| t > 0).map(|t| (us as f64 / t as f64).clamp(0.0, 1.0) as f32)
        });
        let pos_short = blk.pos.split('.').next().unwrap_or(&blk.pos);
        let info = format!("frame={} fps={} speed={} pos={pos_short}", blk.frame, blk.fps, blk.speed);
        let _ = tx.send(AppEvent::BackgroundTaskProgress { id: task_id, progress, info });
    };
    loop {
        let read_start = std::time::Instant::now();
        let n = file.read(&mut buf).await.unwrap_or(0);
        crate::iomon::record(Cat::LogRead, &progress_log, crate::iomon::OpKind::Read, n as u64, read_start.elapsed());
        if n == 0 {
            if done.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(idx) = pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = pending.drain(..=idx).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len().saturating_sub(1)]);
            if let Some(block) = parse_ffmpeg_progress_line(line.trim_end_matches('\r'), &mut blk) {
                emit(&block);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_completeness_requires_both_durations_and_the_99_percent_cutoff() {
        assert!(ffmpeg_job_tmp_is_complete(Some(3600), Some(3600)));
        assert!(ffmpeg_job_tmp_is_complete(Some(3600), Some(3564)), "99% boundary");
        assert!(!ffmpeg_job_tmp_is_complete(Some(3600), Some(3500)), "below the cutoff");
        assert!(!ffmpeg_job_tmp_is_complete(Some(3600), None), "tmp duration unknown");
        assert!(!ffmpeg_job_tmp_is_complete(None, Some(3600)), "expected duration unknown");
        assert!(!ffmpeg_job_tmp_is_complete(Some(0), Some(0)), "zero expected duration never verifiable");
    }

    #[test]
    fn progress_line_parsing_accumulates_a_block_and_resets_on_boundary() {
        let mut blk = FfmpegProgressBlock::default();
        assert!(parse_ffmpeg_progress_line("frame=120", &mut blk).is_none());
        assert!(parse_ffmpeg_progress_line("fps=30", &mut blk).is_none());
        assert!(parse_ffmpeg_progress_line("speed=2.5x", &mut blk).is_none());
        assert!(parse_ffmpeg_progress_line("out_time=00:00:04.000000", &mut blk).is_none());
        assert!(parse_ffmpeg_progress_line("out_time_ms=4000000", &mut blk).is_none());
        let done = parse_ffmpeg_progress_line("progress=continue", &mut blk).expect("block complete");
        assert_eq!(done.frame, "120");
        assert_eq!(done.fps, "30");
        assert_eq!(done.speed, "2.5x");
        assert_eq!(done.pos, "00:00:04.000000");
        assert_eq!(done.out_time_us, Some(4_000_000));
        // Accumulator reset for the next block.
        assert_eq!(blk.frame, "");
        assert_eq!(blk.out_time_us, None);
    }

    #[test]
    fn progress_line_end_marker_also_closes_a_block() {
        let mut blk = FfmpegProgressBlock::default();
        parse_ffmpeg_progress_line("out_time_ms=9000000", &mut blk);
        let done = parse_ffmpeg_progress_line("progress=end", &mut blk).expect("block complete");
        assert_eq!(done.out_time_us, Some(9_000_000));
    }

    #[test]
    fn malformed_line_without_equals_is_ignored() {
        let mut blk = FfmpegProgressBlock::default();
        assert!(parse_ffmpeg_progress_line("not a key value line", &mut blk).is_none());
    }
}
