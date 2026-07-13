//! Child-process running and re-attach: `run_process`, detached
//! reconcile/adopt, stall watchdog, log tails, progress parsing.

use super::*;

pub(super) struct ProcessOutcome {
    pub(super) exit_code: Option<i64>,
    pub(super) log: String,
}

/// What the startup reconcile decided to do with one persisted detached download.
pub enum ReAction {
    /// Process still running — wait on it, tail its log, then finalize.
    Adopt,
    /// Process already exited (finished while the app was down) — finalize now.
    Finalize,
    /// Process gone but the capture is SABR-resumable — re-run from its `.state`.
    Resume,
}

/// One detached download to drive on the runtime after [`Supervisor::reconcile_detached`].
pub struct ReattachItem {
    row: DetachedRow,
    /// The active map this download occupies while in flight (so the UI shows it
    /// and the scheduler won't double-start its monitor).
    active: ActiveSet,
    action: ReAction,
}

impl ReattachItem {
    /// The OS PID of the detached tool this item drives.
    pub fn pid(&self) -> u32 {
        self.row.pid
    }
    /// Whether this item re-attaches to a *still-running* process (vs. finalizing
    /// one that already exited, or resuming a SABR capture).
    pub fn is_adopt(&self) -> bool {
        matches!(self.action, ReAction::Adopt)
    }
}

/// Open `path` for the child's combined stdout+stderr: truncate any prior log,
/// then return two **append** handles onto the one file (one for stdout, one for
/// stderr). Append mode lets both streams interleave into a single growing file
/// without clobbering each other, and the child keeps writing after we detach.
pub(super) fn open_log_pair(path: &Path) -> std::io::Result<(std::fs::File, std::fs::File)> {
    use crate::iomon::Cat;
    if let Some(parent) = path.parent() {
        let _ = crate::iomon::fs::create_dir_all_sync(Cat::ToolLog, parent);
    }
    let _ = crate::iomon::fs::create_sync(Cat::ToolLog, path)?; // truncate any leftover
    let out = crate::iomon::fs::open_with_sync(Cat::ToolLog, path, |o| {
        o.create(true).append(true);
    })?;
    let err = out.try_clone()?;
    Ok((out, err))
}

/// Tail a tool's log file, parsing progress/speed and streamlink ad-break lines
/// exactly as the old pipe readers did. Starts at `start_offset` (0 for a fresh
/// spawn; end-of-file for a re-attach so already-persisted breaks aren't redone),
/// and returns once `done` is set and the file is drained to EOF.
pub(super) async fn tail_log(
    path: PathBuf,
    start_offset: u64,
    progress: Option<VideoProgress>,
    speed: Option<VideoSpeed>,
    id: i64,
    ad_tx: Option<mpsc::UnboundedSender<(i64, i64)>>,
    done: Arc<AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    // The log is created before the spawn, but tolerate a brief absence.
    let mut file = loop {
        match crate::iomon::fs::open(crate::iomon::Cat::LogRead, &path).await {
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
    let emit = |line: &str| {
        let (f, s) = parse_progress_fields(line);
        if let (Some(f), Some(m)) = (f, progress.as_ref()) {
            m.lock().unwrap().insert(id, f);
        }
        if let (Some(s), Some(m)) = (s, speed.as_ref()) {
            m.lock().unwrap().insert(id, s);
        }
        if let Some(tx) = &ad_tx {
            if let Some(dur) = parse_ad_break_secs(line) {
                let _ = tx.send((now_unix(), dur));
            }
        }
        tracing::trace!(target: "streamarchiver::recproc", "{line}");
    };
    loop {
        let read_start = std::time::Instant::now();
        let n = file.read(&mut buf).await.unwrap_or(0);
        // record() (not record_region) so a slow read / the ops ring can name
        // WHICH log — re-attached pre-relocation rows still tail from `.cache\`
        // on the recordings drive, and those are exactly the slow ones.
        crate::iomon::record(
            crate::iomon::Cat::LogRead,
            &path,
            crate::iomon::OpKind::Read,
            n as u64,
            read_start.elapsed(),
        );
        if n == 0 {
            if done.load(Ordering::SeqCst) {
                // Process exited and we've drained to EOF: flush any trailing
                // partial line, then stop (dropping ad_tx ends the ad processor).
                let tail = String::from_utf8_lossy(&pending);
                let tail = tail.trim_end_matches('\r');
                if !tail.trim().is_empty() {
                    emit(tail);
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(idx) = pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = pending.drain(..=idx).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len().saturating_sub(1)]);
            emit(line.trim_end_matches('\r'));
        }
    }
}

/// Build the minimal [`Recording`] the SABR resume path needs from a detached
/// registry row (it reads `id`, `output_path`, and `take_group`).
pub(super) fn recording_from_detached(row: &DetachedRow) -> Recording {
    Recording {
        id: row.ref_id,
        monitor_id: row.monitor_id.unwrap_or(0),
        started_at: row.started_at,
        ended_at: None,
        status: "recording".into(),
        bytes: 0,
        exit_code: None,
        output_path: row.final_path.clone(),
        went_live_at: row.went_live_at,
        went_live_approx: false,
        lost_secs: None,
        stream_id: row.stream_id.clone(),
        take_group: row.take_group.clone(),
        ad_count: 0,
        ad_secs: 0,
        meta_change_count: 0,
        title: String::new(),
        category: String::new(),
        log_excerpt: String::new(),
        notes: String::new(),
        vod_id: None,
        vod_state: None,
        vod_muted_secs: None,
        recovery_state: None,
        recovered_path: None,
        vod_dl_state: None,
        vod_dl_path: None,
        vod_dl_video_id: None,
        backfill_path: None,
        full_path: None,
        trigger_info: String::new(),
        head_backfill_state: String::new(),
        trigger_rule_json: String::new(),
    }
}

/// The byte offset of the start of the final (possibly partial) line of `path`,
/// so a re-attach tail begins on a line boundary and never splits a marker. If the
/// file ends exactly on a newline (nothing partial) or on any error, returns its
/// length. Only the trailing line is re-read, which is safe: ad inserts are
/// idempotent and progress parsing is overwrite-only.
pub(super) async fn line_aligned_tail_offset(path: &Path) -> u64 {
    use crate::iomon::Cat;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let len = match crate::iomon::fs::metadata(Cat::LogRead, path).await {
        Ok(m) => m.len(),
        Err(_) => return 0,
    };
    if len == 0 {
        return 0;
    }
    let window = len.min(64 * 1024);
    let start = len - window;
    let Ok(mut f) = crate::iomon::fs::open(Cat::LogRead, path).await else {
        return len;
    };
    if f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return len;
    }
    let mut buf = vec![0u8; window as usize];
    let read_start = std::time::Instant::now();
    let read_res = f.read_exact(&mut buf).await;
    crate::iomon::record(Cat::LogRead, path, crate::iomon::OpKind::Read, window, read_start.elapsed());
    if read_res.is_err() {
        return len;
    }
    if buf.last() == Some(&b'\n') {
        return len; // ends on a newline — no partial trailing line
    }
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(idx) => start + idx as u64 + 1, // first byte after the last newline
        None => start,                       // no newline in window
    }
}

/// Read the last `max_lines` lines of a tool log — the failure-reason excerpt
/// stored on the finished recording/video row. Reads only a bounded window
/// from the end of the file (an hours-long yt-dlp progress log can reach tens
/// of MB, and this runs at finalize — the worst moment for an extra full-file
/// read on the recordings drive).
pub(super) async fn read_log_tail(path: &Path, max_lines: usize) -> String {
    use crate::iomon::Cat;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    // 80 lines of tool output fit comfortably in 64 KiB.
    const TAIL_WINDOW: u64 = 64 * 1024;
    let Ok(mut f) = crate::iomon::fs::open(Cat::LogRead, path).await else {
        return String::new();
    };
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_WINDOW);
    if f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return String::new();
    }
    let mut data = Vec::with_capacity(TAIL_WINDOW as usize);
    let read_start = std::time::Instant::now();
    let read_res = f.read_to_end(&mut data).await;
    crate::iomon::record(
        Cat::LogRead,
        path,
        crate::iomon::OpKind::Read,
        data.len() as u64,
        read_start.elapsed(),
    );
    if read_res.is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&data);
    let lines: Vec<&str> = text.lines().collect();
    // Drop the first line of a mid-file window: it's almost certainly partial.
    let skip_partial = usize::from(start > 0 && lines.len() > max_lines);
    let start_line = lines.len().saturating_sub(max_lines).max(skip_partial);
    lines[start_line..].join("\n")
}

/// Spawn the ad-break processor for an [`AdSink`], returning the channel the log
/// tailer pushes `(detected_at, duration)` pairs onto plus the task handle. For
/// each break, the cut lands at the content captured so far — ad segments are
/// filtered out, so the capture's media duration is that position (correct for
/// live-edge and capture-from-start/DVR). Falls back to wall clock minus
/// already-skipped ad time when ffprobe can't read the still-growing file yet.
/// Shared by the in-session spawn and the re-attach path.
pub(super) fn spawn_ad_processor(
    sink: AdSink,
) -> (
    mpsc::UnboundedSender<(i64, i64)>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<(i64, i64)>();
    let jh = tokio::spawn(async move {
        let mut prior_ad_secs: i64 = 0;
        let mut last_at: i64 = 0;
        while let Some((detected_at, dur)) = rx.recv().await {
            let mut probed = media_duration_secs(&sink.capture_path).await;
            if probed.is_none() {
                tokio::time::sleep(Duration::from_millis(500)).await;
                probed = media_duration_secs(&sink.capture_path).await;
            }
            let at = match probed {
                Some(d) => d,
                None => {
                    let anchor = match (sink.from_start, sink.went_live_at) {
                        (true, Some(wl)) => wl,
                        _ => sink.started_at,
                    };
                    (detected_at - anchor - prior_ad_secs).max(0)
                }
            };
            // Cut positions only move forward; guard against a probe that
            // momentarily reports a smaller duration.
            let at = at.max(last_at);
            last_at = at;
            prior_ad_secs += dur;
            // Mark the ad window so the UI can tint the row while it plays.
            sink.ad_active
                .lock()
                .unwrap()
                .insert(sink.monitor_id, detected_at + dur);
            match sink.store.insert_ad_break(sink.recording_id, at, dur) {
                Ok(_) => {
                    info!(
                        monitor_id = sink.monitor_id,
                        rec_id = sink.recording_id,
                        at,
                        secs = dur,
                        "ad break detected"
                    );
                    // Wake the UI so an expanded history tree refreshes its
                    // Ads / Ad time columns.
                    let _ = sink.events.send(AppEvent::MonitorState {
                        monitor_id: sink.monitor_id,
                        state: "recording".into(),
                    });
                }
                Err(e) => warn!("insert ad_break failed: {e:#}"),
            }
        }
    });
    (tx, jh)
}

/// Parse a yt-dlp progress line
/// (`--progress-template "download:DLPCT=%(progress._percent_str)s;;SPEED=%(progress.speed)s"`)
/// into `(percent fraction 0.0..=1.0, speed bytes/sec)`. Either may be `None`
/// (non-progress line or an unknown/`NA` value).
pub(super) fn parse_progress_fields(line: &str) -> (Option<f32>, Option<f64>) {
    let mut pct = None;
    let mut speed = None;
    for part in line.trim().split(";;") {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("DLPCT=") {
            let s = rest.trim().trim_end_matches('%').trim();
            if let Ok(v) = s.parse::<f32>() {
                pct = Some((v / 100.0).clamp(0.0, 1.0));
            }
        } else if let Some(rest) = part.strip_prefix("SPEED=") {
            // yt-dlp's raw `speed` is bytes/sec (or "NA" when unknown).
            if let Ok(v) = rest.trim().parse::<f64>() {
                if v.is_finite() && v > 0.0 {
                    speed = Some(v);
                }
            }
        }
    }
    (pct, speed)
}

/// Percent-only convenience wrapper around [`parse_progress_fields`].
#[cfg(test)]
pub(super) fn parse_progress(line: &str) -> Option<f32> {
    parse_progress_fields(line).0
}

/// Parse streamlink's Twitch `Detected advertisement break of N second(s)` log
/// line into the break duration in seconds. Returns `None` for any other line.
/// Tolerant of streamlink's `[plugins.twitch][info]` line prefix.
pub(super) fn parse_ad_break_secs(line: &str) -> Option<i64> {
    const MARKER: &str = "advertisement break of ";
    let idx = line.find(MARKER)?;
    let rest = &line[idx + MARKER.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

impl Supervisor {
    /// At startup, decide what to do with each in-flight (crash-leftover) recording:
    /// resume the SABR-resumable ones (reserving their monitor so the scheduler
    /// won't double-start them) and mark the rest `orphaned`. Synchronous so the
    /// reservations are in place before detection can fire; returns the (recording,
    /// row) pairs to resume (spawned by the caller on the runtime).
    pub fn resume_inflight(&self) -> Vec<(Recording, MonitorWithChannel)> {
        let recs = match self.store.inflight_recordings() {
            Ok(r) => r,
            Err(e) => {
                warn!("resume: failed to load in-flight recordings: {e:#}");
                return Vec::new();
            }
        };
        let ytdlp = load_ytdlp_bins(&self.store);
        let mut to_resume = Vec::new();
        for rec in recs {
            let row = match self.store.get_monitor_with_channel(rec.monitor_id) {
                Ok(Some(r)) => r,
                _ => {
                    let _ = self.store.mark_recording_orphaned(rec.id);
                    continue;
                }
            };
            let m = &row.monitor;
            let resumable = m.platform() == Platform::YouTube
                && m.tool == Tool::YtDlp
                && m.capture_from_start
                && ytdlp.sabr.usable()
                && sabr_state_exists(&rec.output_path);
            if !resumable {
                let _ = self.store.mark_recording_orphaned(rec.id);
                continue;
            }
            // Reserve the monitor (mirrors try_begin) so a concurrent poll can't
            // start a fresh take while we resume this one.
            {
                let mut active = self.active.lock().unwrap();
                if active.contains_key(&m.id) {
                    continue;
                }
                active.insert(m.id, 0);
            }
            info!(
                monitor_id = m.id,
                rec_id = rec.id,
                "resuming interrupted SABR capture {}",
                Platform::YouTube.tag()
            );
            to_resume.push((rec, row));
        }
        // Orphans whose output file survived intact are promoted (and ones whose
        // capture is stranded in `.cache\` retargeted) by the disk-aware
        // `reconcile_orphan_outputs` pass, spawned by the caller — verifying the
        // files can spin up the recordings drive, so it must not run here
        // synchronously.
        to_resume
    }
    /// Reconcile the persistent detached-process registry at startup (synchronously,
    /// so reservations land before detection can fire). For each row: re-attach to a
    /// still-running download, finalize one that finished while the app was down, or
    /// hand a SABR-resumable one to the resume path; orphan the rest. Registry-backed
    /// recordings are owned here — `resume_inflight` is fed a registry-excluded set.
    /// Returns the items to drive on the runtime plus the capture stems to protect
    /// from the cache sweep. The caller spawns `reattach_one` for each item.
    pub fn reconcile_detached(&self) -> (Vec<ReattachItem>, std::collections::HashSet<String>) {
        let rows = match self.store.list_detached() {
            Ok(r) => r,
            Err(e) => {
                warn!("reattach: failed to load detached registry: {e:#}");
                return (Vec::new(), HashSet::new());
            }
        };
        let ytdlp = load_ytdlp_bins(&self.store);
        let build = crate::version::build_id();
        let mut items = Vec::new();
        let mut skip = HashSet::new();
        for mut row in rows {
            let spawn_build = row.spawn_build.clone();
            if spawn_build != build {
                crate::compat::reattach_fixups(&spawn_build, &mut row);
            }
            if let Some(stem) = Path::new(&row.capture_path).file_stem() {
                skip.insert(stem.to_string_lossy().into_owned());
            }
            // PID-reuse-safe liveness: the PID must still be running *and* have the
            // creation time we recorded. If we couldn't read the creation time at
            // spawn (proc_start == 0), the identity is unverifiable — don't adopt a
            // possibly-recycled PID; fall through to finalize-from-file / resume / orphan.
            let alive = row.proc_start != 0
                && crate::platform::pid_alive(row.pid)
                && crate::platform::process_start_time(row.pid) == Some(row.proc_start);

            // The DASH companion of a dual capture occupies the secondary map; the
            // primary / videos / chat occupy their own. This is recorded explicitly
            // (`secondary`), not guessed from registry order (the two legs race to
            // register), so a re-attach can never swap their roles.
            let active = match (row.kind, row.secondary) {
                (DetachedKind::Video, _) => self.active_videos.clone(),
                (DetachedKind::Chat, _) => self.active_chats.clone(),
                (DetachedKind::Recording, true) => self.active_secondary.clone(),
                (DetachedKind::Recording, false) => self.active.clone(),
            };
            let key = match row.kind {
                DetachedKind::Video => row.ref_id,
                _ => row.monitor_id.unwrap_or(row.ref_id),
            };

            if alive {
                active.lock().unwrap().insert(key, row.pid);
                info!(
                    kind = row.kind.as_str(),
                    ref_id = row.ref_id,
                    pid = row.pid,
                    spawn_build = %row.spawn_build,
                    "re-attaching to detached download"
                );
                items.push(ReattachItem {
                    row,
                    active,
                    action: ReAction::Adopt,
                });
                continue;
            }

            // Process gone. A SABR-resumable recording is recovered FIRST — SABR
            // writes its media directly (`--no-part`), so it always has a non-empty
            // capture; checking `has_capture` before resumability would wrongly
            // finalize an interrupted SABR take and purge its `.state`.
            let resumable = row.kind == DetachedKind::Recording
                && !row.secondary
                && ytdlp.sabr.usable()
                && sabr_state_exists(&row.final_path)
                && self
                    .store
                    .get_monitor_with_channel(row.monitor_id.unwrap_or(0))
                    .ok()
                    .flatten()
                    .map(|r| {
                        r.monitor.platform() == Platform::YouTube
                            && r.monitor.tool == Tool::YtDlp
                            && r.monitor.capture_from_start
                    })
                    .unwrap_or(false);
            if resumable {
                let mid = row.monitor_id.unwrap_or(0);
                let reserve = {
                    let mut a = self.active.lock().unwrap();
                    if a.contains_key(&mid) {
                        false
                    } else {
                        a.insert(mid, 0);
                        true
                    }
                };
                if reserve {
                    // Make sure no straggler (e.g. a broken-away ffmpeg grandchild)
                    // still holds the `.state`/`.part` files before we re-run.
                    if let Some(job) = crate::platform::DetachedJob::open(&row.job_name) {
                        job.kill();
                    }
                    crate::platform::kill_process_tree(row.pid);
                    info!(
                        monitor_id = mid,
                        rec_id = row.ref_id,
                        "resuming detached SABR capture {}",
                        Platform::YouTube.tag()
                    );
                    items.push(ReattachItem {
                        row,
                        active: self.active.clone(),
                        action: ReAction::Resume,
                    });
                    continue;
                }
            }

            // Otherwise finalize when there's a usable capture on disk…
            let has_capture = crate::iomon::fs::metadata_sync(Cat::Startup, &row.capture_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
                || crate::iomon::fs::metadata_sync(Cat::Startup, &row.final_path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);
            if has_capture {
                info!(
                    kind = row.kind.as_str(),
                    ref_id = row.ref_id,
                    "detached download finished while app was down; finalizing"
                );
                items.push(ReattachItem {
                    row,
                    active,
                    action: ReAction::Finalize,
                });
                continue;
            }

            // …otherwise nothing to recover: orphan the row and drop the registry entry.
            match row.kind {
                DetachedKind::Recording => {
                    let _ = self.store.mark_recording_orphaned(row.ref_id);
                }
                DetachedKind::Video => {
                    let _ = self.store.set_video_status(row.ref_id, "orphaned");
                }
                DetachedKind::Chat => {}
            }
            let _ = self.store.clear_detached(row.kind, row.ref_id);
        }
        (items, skip)
    }

    /// Drive one reconciled detached download to completion on the runtime.
    pub async fn reattach_one(&self, item: ReattachItem) {
        let ReattachItem { row, active, action } = item;
        match action {
            ReAction::Resume => {
                let mid = row.monitor_id.unwrap_or(0);
                match self.store.get_monitor_with_channel(mid).ok().flatten() {
                    Some(mrow) => self.resume_recording(recording_from_detached(&row), mrow).await,
                    None => {
                        self.active.lock().unwrap().remove(&mid);
                        let _ = self.store.mark_recording_orphaned(row.ref_id);
                        let _ = self.store.clear_detached(row.kind, row.ref_id);
                    }
                }
            }
            ReAction::Finalize => self.finalize_reattached(&row, &active).await,
            ReAction::Adopt => self.adopt_detached(row, active).await,
        }
    }

    /// Re-attached to a still-running download: repaint the UI, resume full live
    /// detail by tailing the log from its current end (pre-restart ad breaks are
    /// already persisted), wait for the process, then finalize — unless we're
    /// quitting again (then leave the registry row for the next launch).
    async fn adopt_detached(&self, row: DetachedRow, active: ActiveSet) {
        let key = match row.kind {
            DetachedKind::Video => row.ref_id,
            _ => row.monitor_id.unwrap_or(row.ref_id),
        };
        // Re-attached tools get I/O-sampled like fresh spawns; the guard drops
        // (deregisters) when this function returns after the wait — on every
        // branch, including the chat early-returns.
        let _io_guard = (row.pid != 0).then(|| {
            crate::iomon::track_child(
                row.pid,
                crate::iomon::ChildInfo {
                    label: Path::new(&row.capture_path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    tool: "re-attached".to_string(),
                    purpose: match row.kind {
                        DetachedKind::Video => "video download (re-attached)".to_string(),
                        DetachedKind::Chat => "chat capture (re-attached)".to_string(),
                        _ => "live capture (re-attached)".to_string(),
                    },
                    region: crate::iomon::classify(Path::new(&row.capture_path)),
                    proc_start: row.proc_start,
                },
            )
        });
        if let Some(mid) = row.monitor_id {
            let state = if row.kind == DetachedKind::Chat {
                "chat_active"
            } else {
                "recording"
            };
            let _ = self.events.send(AppEvent::MonitorState {
                monitor_id: mid,
                state: state.into(),
            });
        }

        // Chat sidecars carry no live state to reconstruct — just wait and clear.
        if row.kind == DetachedKind::Chat {
            let exited = self.wait_for_exit(row.pid).await;
            // Free the slot either way: on a real exit it's done; on shutdown,
            // leaving it would block stop_all_recordings' drain loop for 120s.
            active.lock().unwrap().remove(&key);
            if !exited {
                return; // re-detaching on quit — keep the registry row
            }
            let _ = self.store.clear_detached(DetachedKind::Chat, row.ref_id);
            if let Some(mid) = row.monitor_id {
                let _ = self.events.send(AppEvent::MonitorState {
                    monitor_id: mid,
                    state: "idle".into(),
                });
            }
            return;
        }

        // Fetch the monitor row once (recordings only) to rebuild the ad pipeline
        // and the live watchers.
        let mrow = if row.kind == DetachedKind::Recording {
            row.monitor_id
                .and_then(|mid| self.store.get_monitor_with_channel(mid).ok().flatten())
        } else {
            None
        };

        let log_path = PathBuf::from(&row.log_path);
        // Start tailing at a line boundary so a partial line at the seek point
        // (e.g. an ad-break marker being written at the restart instant) isn't
        // split and lost; re-reading that single line is safe (ad inserts are
        // idempotent, progress is overwrite).
        let start_offset = line_aligned_tail_offset(&log_path).await;
        let done = Arc::new(AtomicBool::new(false));
        let (progress, speed) = if row.kind == DetachedKind::Video {
            (Some(self.video_progress.clone()), Some(self.video_speed.clone()))
        } else {
            (None, None)
        };
        // Rebuild the ad pipeline for a re-attached Twitch+streamlink recording so
        // new breaks keep being recorded (historical ones are already persisted).
        let ad_sink = mrow
            .as_ref()
            .filter(|m| m.monitor.tool == Tool::Streamlink && m.monitor.platform() == Platform::Twitch)
            .map(|m| AdSink {
                store: self.store.clone(),
                events: self.events.clone(),
                monitor_id: row.monitor_id.unwrap_or(0),
                recording_id: row.ref_id,
                started_at: row.started_at,
                went_live_at: row.went_live_at,
                from_start: m.monitor.capture_from_start,
                capture_path: PathBuf::from(&row.capture_path),
                ad_active: self.ad_active.clone(),
            });
        let (ad_tx, ad_task) = match ad_sink {
            Some(sink) => {
                let (tx, jh) = spawn_ad_processor(sink);
                (Some(tx), Some(jh))
            }
            None => (None, None),
        };

        // Re-spawn the live watchers exactly as the in-session record() path does —
        // otherwise a re-attached take's Lost-time never resolves (catch-up watcher)
        // and its Game/Title freeze (meta watcher) at the pre-restart values.
        let watcher_done = Arc::new(AtomicBool::new(false));
        let mut watchers: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        if let Some(m) = &mrow {
            let from_start = m.monitor.capture_from_start
                && matches!(m.monitor.tool, Tool::Streamlink | Tool::YtDlp);
            if from_start && row.went_live_at.is_some() {
                watchers.push(tokio::spawn(catch_up_watcher(
                    self.store.clone(),
                    self.events.clone(),
                    row.monitor_id.unwrap_or(0),
                    m.monitor.platform(),
                    row.ref_id,
                    PathBuf::from(&row.capture_path),
                    row.went_live_at.unwrap_or(0),
                    watcher_done.clone(),
                )));
            }
            if m.monitor.platform() != Platform::Generic {
                // A restart drops the in-memory TriggerRule the original
                // try_begin/record call matched — recover it (frozen at start
                // time, not re-resolved from the live rule lists) from the
                // recording row so stop-on-unmatch survives re-attach too.
                let stop_rule = self
                    .store
                    .get_recording(row.ref_id)
                    .ok()
                    .flatten()
                    .and_then(|r| serde_json::from_str::<crate::triggers::TriggerRule>(&r.trigger_rule_json).ok())
                    .filter(|r| r.stop_on_unmatch);
                watchers.push(tokio::spawn(meta_watcher(
                    self.ctx.clone(),
                    self.store.clone(),
                    self.events.clone(),
                    row.monitor_id.unwrap_or(0),
                    row.ref_id,
                    row.started_at,
                    m.monitor.url.clone(),
                    m.monitor.platform(),
                    watcher_done.clone(),
                    self.shutdown.clone(),
                    self.manual_tx.clone(),
                    stop_rule,
                )));
            }
            // Twitch chat logger is a native in-process task (not a tracked
            // process), so re-attach must restart it to keep appending to the
            // `.chat.jsonl` sidecar. (YouTube chat is a yt-dlp process and
            // re-attaches via its own registry row.)
            if m.monitor.chat_log && m.monitor.platform() == Platform::Twitch {
                let chat_path = PathBuf::from(&row.final_path).with_extension("chat.jsonl");
                watchers.push(tokio::spawn(crate::chat::log_twitch_chat(
                    m.monitor.url.clone(),
                    chat_path,
                    watcher_done.clone(),
                    self.shutdown.clone(),
                )));
            }
        }

        let tail = tokio::spawn(tail_log(
            log_path,
            start_offset,
            progress,
            speed,
            key,
            ad_tx,
            done.clone(),
        ));

        // Stall watchdog for the adopted process too — a re-attached capture
        // whose tool wedged after the stream ended would otherwise sit
        // "recording" with a growing uptime until the app is restarted.
        let stall = self.spawn_stall_watchdog(
            row.kind,
            row.ref_id,
            row.pid,
            row.proc_start,
            PathBuf::from(&row.log_path),
            PathBuf::from(&row.capture_path),
            done.clone(),
        );

        let exited = self.wait_for_exit(row.pid).await;
        done.store(true, Ordering::SeqCst);
        stall.abort();
        watcher_done.store(true, Ordering::SeqCst);
        let _ = tail.await;
        if let Some(t) = ad_task {
            let _ = t.await;
        }
        for w in watchers {
            let _ = w.await;
        }
        if !exited {
            // Quitting again: keep the registry row for the next launch, but free
            // the in-memory slot so a concurrent stop_all drain loop terminates.
            active.lock().unwrap().remove(&key);
            return;
        }
        self.finalize_reattached(&row, &active).await;
    }

    /// Block until `pid` truly exits, returning false if shutdown was requested
    /// first. Re-checks liveness after each wait so a spurious `WaitForSingleObject`
    /// failure can't trigger a premature finalize while the tool is still writing.
    /// Kill a capture/download whose output has completely stopped changing.
    ///
    /// Recovery for tools that wedge instead of exiting — yt-dlp hanging at the
    /// live edge after a stream ends, streamlink on a dead HLS session, a stuck
    /// VOD download. Without this, `child.wait()` / `wait_for_exit` blocks
    /// forever and the row shows "recording"/"live" with a growing uptime for
    /// hours after the stream ended.
    ///
    /// Every 60s it samples an activity signature — the log file's handle size
    /// plus the handle sizes of every capture-stem file in the capture dir
    /// (covers SABR `.part` files and ffmpeg merge temps; handle sizes because
    /// NTFS dir entries are stale for open files). If NOTHING changes for
    /// [`STALL_KILL_SECS`], it records `(kind, ref_id)` in `stall_killed` (the
    /// finalize paths consume it for classification — a truncated stall-killed
    /// VOD download must never classify "completed" and trigger a replace) and
    /// kills the process tree; the normal wait → finalize path then runs. If
    /// the channel is actually still live, the next detection poll simply
    /// starts a fresh take.
    ///
    /// `proc_start` is the process's recorded start time: together with a
    /// last-instant `done` re-check it guards the kill against firing after a
    /// natural exit (the sample I/O can take seconds; `abort()` only lands at
    /// an await point) — without it a recycled PID could kill an unrelated
    /// process tree.
    #[allow(clippy::too_many_arguments)]
    fn spawn_stall_watchdog(
        &self,
        kind: DetachedKind,
        ref_id: i64,
        pid: u32,
        proc_start: u64,
        log_path: PathBuf,
        capture_path: PathBuf,
        done: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let kill = |why: String| {
                // Last-instant guards: the process may have exited naturally
                // while the (possibly slow) sample ran, and Windows recycles
                // PIDs aggressively.
                if done.load(Ordering::SeqCst)
                    || !crate::platform::pid_alive(pid)
                    || crate::platform::process_start_time(pid).unwrap_or(0) != proc_start
                {
                    return;
                }
                warn!(?kind, ref_id, pid, "{why} — killing stalled process tree");
                if kind != DetachedKind::Chat {
                    this.stall_killed.lock().unwrap().insert((kind, ref_id));
                }
                let _ = this.events.send(AppEvent::Error {
                    context: "Stall watchdog".into(),
                    message: format!(
                        "{why} — stopping the stuck process (pid {pid}) and finalizing."
                    ),
                });
                crate::platform::kill_process_tree(pid);
            };

            // First sample after a short settle: if the newest write on disk is
            // ALREADY older than the threshold, the tool wedged long ago (the
            // app was restarted hours after the stream ended) — kill right
            // away instead of observing another 15 minutes of silence.
            crate::app_core::sleep_cancellable(Duration::from_secs(10), &this.shutdown).await;
            if done.load(Ordering::SeqCst) || this.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let s = stall_sample(&log_path, &capture_path).await;
            let stale_secs = now_unix().saturating_sub(s.newest_mtime);
            if s.newest_mtime > 0 && stale_secs >= STALL_KILL_SECS as i64 {
                kill(format!(
                    "Last output was {} min ago (before this session)",
                    stale_secs / 60
                ));
                return;
            }
            let mut last = (s.log_sig, s.capture_sig);
            let mut any_changed_at = Instant::now();
            let mut capture_changed_at = Instant::now();
            loop {
                crate::app_core::sleep_cancellable(Duration::from_secs(60), &this.shutdown).await;
                if done.load(Ordering::SeqCst) || this.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let s = stall_sample(&log_path, &capture_path).await;
                let now = Instant::now();
                if s.log_sig != last.0 || s.capture_sig != last.1 {
                    any_changed_at = now;
                }
                if s.capture_sig != last.1 {
                    capture_changed_at = now;
                }
                last = (s.log_sig, s.capture_sig);
                if now.duration_since(any_changed_at) >= Duration::from_secs(STALL_KILL_SECS) {
                    kill(format!(
                        "Download produced no output for {} minutes",
                        STALL_KILL_SECS / 60
                    ));
                    return;
                }
                // A tool endlessly retry-logging against a dead stream keeps
                // its log moving while the capture stays frozen — longer leash
                // (metadata/waiting phases write no media and are exempt via
                // has_capture), but not forever.
                if s.has_capture
                    && now.duration_since(capture_changed_at)
                        >= Duration::from_secs(CAPTURE_STALL_KILL_SECS)
                {
                    kill(format!(
                        "Capture file hasn't grown for {} minutes (log still active)",
                        CAPTURE_STALL_KILL_SECS / 60
                    ));
                    return;
                }
            }
        })
    }

    async fn wait_for_exit(&self, pid: u32) -> bool {
        loop {
            let shutdown = self.shutdown.clone();
            let _ = tokio::task::spawn_blocking(move || crate::platform::wait_pid(pid, &shutdown))
                .await;
            if self.shutdown.load(Ordering::SeqCst) {
                return false;
            }
            if !crate::platform::pid_alive(pid) {
                return true;
            }
            // wait_pid returned while the process is still alive (spurious) — retry.
            crate::app_core::sleep_cancellable(Duration::from_millis(500), &self.shutdown).await;
        }
    }

    /// Promote and record a re-attached download once its process is gone: move/remux
    /// the capture into the output dir, do the same post-capture `{games}`/`{title}`/
    /// `{resolution}` rename + lost-time accounting the in-session path does, finalize
    /// the recording/video row, drop the registry entry, and free the active-map slot.
    async fn finalize_reattached(&self, row: &DetachedRow, active: &ActiveSet) {
        // The promote below can queue at the disk gate for hours — mark the
        // monitor "finalizing" for the UI and FREE ITS ACTIVE SLOT now (an
        // adopted capture held it through `wait_for_exit`), so polling resumes
        // and a restarted stream can start a fresh take immediately.
        if row.kind == DetachedKind::Recording
            && let Some(mid) = row.monitor_id
        {
            self.finalizing.lock().unwrap().insert(mid, row.ref_id);
            active.lock().unwrap().remove(&mid);
        }
        let capture_path = PathBuf::from(&row.capture_path);
        let final_pred = PathBuf::from(&row.final_path);
        let mut final_path = if row.remux_to_mkv {
            promote_capture(
                &DownloadPlan {
                    program: String::new(),
                    args: Vec::new(),
                    capture_path: capture_path.clone(),
                    final_path: final_pred.clone(),
                    remux_to_mkv: true,
                    writes_own_thumbnail: false,
                    mode: String::new(),
                },
                &if row.kind == DetachedKind::Recording {
                    remux_opts_for_recording(&self.store, row.ref_id)
                } else {
                    self.store.remux_opts()
                },
                (row.kind == DetachedKind::Recording)
                    .then(|| (self.events.clone(), row.ref_id as u64)),
            )
            .await
        } else {
            // Already-final container: move the predicted file, or the newest
            // `{stem}.*` the tool actually produced (yt-dlp may differ in extension).
            let produced = if file_len(&capture_path).await > 0 {
                Some(capture_path.clone())
            } else {
                newest_with_stem(&capture_path).await
            };
            match produced {
                Some(src) => {
                    let dest = final_pred.with_file_name(
                        src.file_name().map(|n| n.to_os_string()).unwrap_or_default(),
                    );
                    if let Some(p) = dest.parent() {
                        let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, p).await;
                    }
                    // The download landing on disk matters more than a fully-
                    // descriptive name — see rename_or_shorten.
                    let dest_dir = dest.parent().unwrap_or_else(|| Path::new("."));
                    let dest_stem = dest
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dest_ext = dest
                        .extension()
                        .map(|e| e.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match rename_or_shorten(&src, dest_dir, &dest_stem, &dest_ext).await {
                        Ok(actual) => actual,
                        Err(_) => src,
                    }
                }
                None => final_pred.clone(),
            }
        };

        // For a recording, fetch the monitor row and apply the in-session post-capture
        // rename (fill {games}/{title}/{resolution}) so a re-attached take isn't named
        // differently from one finalized in-session (no leftover "tba" placeholders).
        let mrow = if row.kind == DetachedKind::Recording {
            self.store
                .get_monitor_with_channel(row.monitor_id.unwrap_or(0))
                .ok()
                .flatten()
        } else {
            None
        };
        if let Some(mrow) = &mrow {
            let want_games = template_wants_games(&mrow.monitor.filename_template);
            let want_title = template_wants_title(&mrow.monitor.filename_template);
            let want_went_live = template_wants_went_live(&mrow.monitor.filename_template);
            let do_post_media =
                template_wants_media(&mrow.monitor.filename_template) && media_info_mode(&self.store).post();
            if want_games || want_title || do_post_media || want_went_live {
                let mi = if do_post_media {
                    probe_media(&final_path.to_string_lossy()).await
                } else {
                    None
                };
                let games = if want_games {
                    games_for_recording(&self.store, row.ref_id)
                } else {
                    String::new()
                };
                let title = if want_title {
                    title_for_recording(&self.store, row.ref_id)
                } else {
                    String::new()
                };
                let quality = resolved_quality(&mrow.monitor.quality);
                let ytdlp_bins_fa = load_ytdlp_bins(&self.store);
                let use_sabr_fa = sabr_selected(&mrow.monitor, &ytdlp_bins_fa);
                let mode_fa = recording_mode(&mrow.monitor, use_sabr_fa, row.secondary);
                let stem = monitor_stem(
                    &mrow.monitor,
                    &mrow.channel.name,
                    row.started_at,
                    row.stream_id.as_deref(),
                    &title,
                    mrow.recording_count,
                    &quality,
                    mi.as_ref(),
                    &games,
                    mrow.monitor.tool.label(),
                    &mode_fa,
                    mrow.monitor.platform().as_str(),
                    row.went_live_at.unwrap_or(0),
                );
                // Stop the YouTube chat sidecar before renaming so its open
                // live_chat.json handle is released before companion rename.
                if let Some(mid) = row.monitor_id {
                    self.stop_and_wait_for_chat(mid, Duration::from_secs(6)).await;
                }
                final_path = rename_for_media(final_path, &stem).await;
            }
        }

        let ended = now_unix();
        // Lost-time: zero it only when the capture spans the whole broadcast (same
        // rule as the in-session finalize); else leave it for the provisional estimate.
        if row.kind == DetachedKind::Recording {
            if let (Some(wl), Some(captured)) =
                (row.went_live_at, media_duration_secs(&final_path).await)
            {
                if captured + CATCHUP_TOLERANCE_SECS >= (ended - wl).max(0) {
                    let _ = self.store.set_recording_lost_secs(row.ref_id, 0);
                }
            }
        }

        let bytes = file_len(&final_path).await as i64;
        let log = read_log_tail(&PathBuf::from(&row.log_path), RING_MAX_LINES).await;
        let key = match row.kind {
            DetachedKind::Video => row.ref_id,
            _ => row.monitor_id.unwrap_or(row.ref_id),
        };

        match row.kind {
            DetachedKind::Recording => {
                // Honour a manual stop that raced the re-attach so the take shows
                // 'stopped', not 'failed', when it ended empty.
                let manually_stopped = row
                    .monitor_id
                    .map(|mid| self.stopping_monitors.lock().unwrap().remove(&mid))
                    .unwrap_or(false);
                let stall_killed = self
                    .stall_killed
                    .lock()
                    .unwrap()
                    .remove(&(DetachedKind::Recording, row.ref_id));
                let status = if manually_stopped {
                    if bytes > 0 { "completed" } else { "stopped" }
                } else if bytes > 0 {
                    "completed"
                } else if stall_killed || stream_ended_or_unavailable(&log) {
                    "ended"
                } else {
                    "failed"
                };
                let _ = self.store.finish_recording(
                    row.ref_id,
                    ended,
                    bytes,
                    None,
                    status,
                    &final_path.to_string_lossy(),
                    &log,
                );
                // Slot already freed at fn entry; removing `key` here could
                // now evict a NEWER take that started during the gate wait.
                let vod_platform = mrow.as_ref().map(|r| r.monitor.platform());
                let vod_url = mrow.as_ref().map(|r| r.monitor.url.clone()).unwrap_or_default();
                // Post-stream VOD archive (YouTube/Kick) for takes that
                // survived a restart — the in-session finalize schedules this,
                // adopted/re-attached ones used to silently miss it.
                if let Some(m) = &mrow {
                    self.schedule_vod_archive(row.ref_id, m, row.went_live_at, status);
                }
                let channel = mrow.map(|r| r.channel.name).unwrap_or_default();
                let _ = self.events.send(AppEvent::RecordingFinished {
                    recording_id: row.ref_id,
                    channel,
                    status: status.into(),
                });
                if let Some(platform) = vod_platform {
                    let approx = self.store.recording_went_live_approx(row.ref_id);
                    self.schedule_vod_check(row.ref_id, platform, status, &vod_url, row.went_live_at, approx);
                }
                // Join a backfilled head with the adopted capture (no-op without one).
                {
                    let this = self.clone();
                    let rid = row.ref_id;
                    tokio::spawn(async move { this.maybe_concat_backfill(rid).await });
                }
                if let Some(mid) = row.monitor_id
                    && !active.lock().unwrap().contains_key(&mid)
                {
                    // Skipped when a newer take is already recording — its
                    // live state must not be flipped to "offline" by this old
                    // take's late finalize.
                    let _ = self.events.send(AppEvent::MonitorState {
                        monitor_id: mid,
                        state: "offline".into(),
                    });
                }
                info!(rec_id = row.ref_id, bytes, status, "re-attached recording finalized");
            }
            DetachedKind::Video => {
                let stopped = self.stopping_videos.lock().unwrap().remove(&row.ref_id);
                let stalled = self
                    .stall_killed
                    .lock()
                    .unwrap()
                    .remove(&(DetachedKind::Video, row.ref_id));
                // Adopted processes have no exit code, so a real media file
                // must prove itself: plausible media name AND an
                // ffprobe-readable duration (a promoted `.log` has neither).
                let media_ok = final_path
                    .file_name()
                    .map(|n| plausible_media_output(&format!(".{}", n.to_string_lossy())))
                    .unwrap_or(false)
                    && media_duration_secs(&final_path).await.is_some();
                let status = if stopped {
                    "stopped"
                } else if stalled {
                    // Watchdog-killed mid-download: bytes may be present but
                    // the file is truncated — never classify it "completed"
                    // (a completed VOD archive may replace the live capture).
                    "failed"
                } else if bytes > 0 && media_ok {
                    "completed"
                } else {
                    "failed"
                };
                let _ = self.store.finish_video(
                    row.ref_id,
                    ended,
                    bytes,
                    None,
                    status,
                    &final_path.to_string_lossy(),
                    &log,
                );
                active.lock().unwrap().remove(&key);
                self.video_progress.lock().unwrap().remove(&key);
                self.video_speed.lock().unwrap().remove(&key);
                // File a VOD-archive download onto its recording (alongside /
                // replace) — the in-session finalize runs this hook, adopted
                // ones used to leave vod_dl_state stuck at "downloading".
                self.finalize_vod_archive(row.ref_id, &final_path, status).await;
                info!(video = row.ref_id, bytes, status, "re-attached video finalized");
            }
            DetachedKind::Chat => {
                active.lock().unwrap().remove(&key);
            }
        }
        let _ = self.store.clear_detached(row.kind, row.ref_id);
        // Drop this download's `.cache\` working leftovers.
        if let Some(cache) = capture_path.parent() {
            if let Some(stem) = final_pred.file_stem().map(|s| s.to_string_lossy().into_owned()) {
                purge_cache(cache, &stem).await;
            }
        }
        // Recording kind only — a chat/video row for the same monitor must not
        // clear a recording finalize still in flight.
        if row.kind == DetachedKind::Recording
            && let Some(mid) = row.monitor_id
        {
            self.finalizing.lock().unwrap().remove(&mid);
        }
    }

    /// Manual rescue for a row stuck in `recording` with no live capture
    /// process (Issues → "Finalize now"): promote whatever the capture left on
    /// disk and settle the row. Mirrors the startup finalize but is driven from
    /// the DB row alone — no detached-registry entry required (that's exactly
    /// the zombie case: the process died without the app ever seeing it exit).
    pub(super) async fn finalize_recording_now(&self, rec_id: i64) {
        let Ok(Some(rec)) = self.store.get_recording(rec_id) else {
            warn!(rec_id, "finalize now: recording not found");
            return;
        };
        if rec.status != "recording" {
            info!(rec_id, status = %rec.status, "finalize now: row is not 'recording' — nothing to do");
            return;
        }
        // Belt and braces: never finalize under a genuinely-live capture.
        let live = self.active.lock().unwrap().contains_key(&rec.monitor_id)
            || self.active_secondary.lock().unwrap().contains_key(&rec.monitor_id);
        if live {
            info!(rec_id, "finalize now: monitor has an active capture — skipping");
            return;
        }
        let out = PathBuf::from(&rec.output_path);
        // Find the actual media: the final file itself, or the capture left in
        // the working dir.
        let mut src: Option<PathBuf> = None;
        if file_len(&out).await > 0 {
            src = Some(out.clone());
        } else {
            for c in live_capture_candidates(&out) {
                if file_len(&c).await > 0 {
                    src = Some(c);
                    break;
                }
            }
        }
        let ended = now_unix();
        let Some(src) = src else {
            // Nothing on disk. Surviving split parts (if any) are the
            // unmerged-recovery section's job; this row just has to stop
            // pretending to record so the Issues lists can classify it.
            let _ = self.store.finish_recording(
                rec_id,
                ended,
                0,
                None,
                "failed",
                &rec.output_path,
                &rec.log_excerpt,
            );
            let _ = self.store.clear_detached(DetachedKind::Recording, rec_id);
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
            info!(rec_id, "finalize now: no capture on disk — marked failed");
            return;
        };
        let is_ts = src.extension().is_some_and(|e| e.eq_ignore_ascii_case("ts"));
        let in_cache = strip_cache_component(&src).is_some();
        // The promote below can queue at the disk gate; show "finalizing".
        self.finalizing.lock().unwrap().insert(rec.monitor_id, rec_id);
        let final_path = if is_ts || in_cache {
            let final_pred = strip_cache_component(&src)
                .unwrap_or_else(|| src.clone())
                .with_extension("mkv");
            promote_capture(
                &DownloadPlan {
                    program: String::new(),
                    args: Vec::new(),
                    capture_path: src.clone(),
                    final_path: final_pred,
                    remux_to_mkv: is_ts,
                    writes_own_thumbnail: false,
                    mode: String::new(),
                },
                &remux_opts_for_recording(&self.store, rec_id),
                Some((self.events.clone(), rec_id as u64)),
            )
            .await
        } else {
            src.clone()
        };
        let bytes = file_len(&final_path).await as i64;
        let status = if bytes > 0 { "completed" } else { "failed" };
        let _ = self.store.finish_recording(
            rec_id,
            ended,
            bytes,
            None,
            status,
            &final_path.to_string_lossy(),
            &rec.log_excerpt,
        );
        let _ = self.store.clear_detached(DetachedKind::Recording, rec_id);
        let _ = self.events.send(AppEvent::RecordingFinished {
            recording_id: rec_id,
            channel: String::new(),
            status: status.into(),
        });
        let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
        self.finalizing.lock().unwrap().remove(&rec.monitor_id);
        info!(rec_id, bytes, status, "finalize now: settled");
    }

    /// Resume an interrupted SABR capture, reusing the orphaned recording's `rec_id`
    /// and `.cache\` stem so yt-dlp continues from the surviving `.state`/`.part`
    /// (re-invoked with the identical `-o`). Chat and the DASH companion are not
    /// resumed. Caller must have already reserved `active[monitor_id]`.
    pub async fn resume_recording(&self, rec: Recording, row: MonitorWithChannel) {
        let monitor_id = row.monitor.id;
        let rec_id = rec.id;
        let out_path = PathBuf::from(&rec.output_path);
        let Some(out_dir) = out_path.parent().map(Path::to_path_buf) else {
            self.active.lock().unwrap().remove(&monitor_id);
            return;
        };
        let stem = out_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Resume in whichever working dir actually holds the surviving
        // `.state`/`.part` files — a pre-rename capture lives in the legacy
        // `.cache\`, and yt-dlp's `-o` must match the original exactly.
        let capture = sabr_state_dir(&rec.output_path)
            .unwrap_or_else(|| cache_dir(&out_dir))
            .join(format!("{stem}.mkv"));

        // Resolve auth + args exactly as a fresh capture would, so the command — and
        // thus the SABR resume — matches the original (`-o` is identical).
        let global_method = self
            .store
            .get_setting("download_auth_method")
            .ok()
            .flatten()
            .unwrap_or_default();
        let global_browser = self
            .store
            .get_setting("cookies_browser")
            .ok()
            .flatten()
            .unwrap_or_default();
        let auth = resolve_auth(&row, &global_method, &global_browser);
        let ytdlp_global_raw = self
            .store
            .get_setting("ytdlp_default_args")
            .ok()
            .flatten()
            .unwrap_or_default();
        let ytdlp_global_args = split_args(&ytdlp_global_raw);
        let ytdlp_bins = load_ytdlp_bins(&self.store);
        let extra = split_args(&row.monitor.extra_args);
        // Only from-start takes leave resumable SABR `.state` (the resume gates
        // in `resume_inflight`/`reconcile_detached` still require capture_from_start),
        // so every take reaching here is from-start → this reproduces the original
        // `--live-from-start` byte-for-byte. (A from-start monitor whose *take* was
        // downgraded to the edge by the DVR fallback still persists
        // capture_from_start=1; yt-dlp's SABR resume continues from the `.state`
        // sequence cursor regardless of this flag, so it's safe.)
        let args = sabr_capture_args(
            &capture, &auth, &ytdlp_global_args, &ytdlp_bins.sabr, &extra, &row.monitor.url,
            row.monitor.capture_from_start, &resolve_sabr_sort(&row.monitor, &ytdlp_bins.sabr),
        );
        let use_sabr_resume = true; // resume is always SABR
        let mode_resume = recording_mode(&row.monitor, use_sabr_resume, false);
        let plan = DownloadPlan {
            program: ytdlp_bins.sabr.binary.clone(),
            args,
            capture_path: capture,
            final_path: out_path,
            remux_to_mkv: false,
            writes_own_thumbnail: false,
            mode: mode_resume,
        };

        let _permit = self.sem.acquire().await.expect("semaphore");
        if self.shutdown.load(Ordering::SeqCst) {
            self.active.lock().unwrap().remove(&monitor_id);
            return;
        }
        if let Some(parent) = plan.capture_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, parent).await;
            set_cache_hidden(parent);
        }
        let _ = self
            .store
            .set_monitor_check_result(monitor_id, "recording", now_unix());
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "recording".into(),
        });
        let _ = self.events.send(AppEvent::RecordingStarted {
            monitor_id,
            recording_id: rec_id,
            channel: row.channel.name.clone(),
            thumbnail_path: None,
        });
        info!(
            monitor_id,
            rec_id,
            program = %plan.program,
            "resuming SABR capture {} -> {}",
            Platform::YouTube.tag(),
            plan.final_path.display()
        );

        let outcome = self
            .run_process(
                &self.active,
                monitor_id,
                &plan,
                None,
                None,
                None,
                DetachReg {
                    kind: DetachedKind::Recording,
                    ref_id: rec_id,
                    monitor_id: Some(monitor_id),
                    take_group: rec.take_group.clone(),
                    started_at: rec.started_at,
                    secondary: false,
                    stream_id: rec.stream_id.clone(),
                    went_live_at: rec.went_live_at,
                },
            )
            .await;
        let ended = now_unix();

        // Capture over — the promote below may queue at the disk gate; show
        // "finalizing" in the UI rather than a stale "recording", and free the
        // active slot NOW so polling resumes and a restarted stream can start
        // a fresh take instead of being blocked behind this remux.
        self.finalizing.lock().unwrap().insert(monitor_id, rec_id);
        self.active.lock().unwrap().remove(&monitor_id);
        // Finalize: promote .cache → output dir, move companions, post-rename, purge.
        let mut final_path = promote_capture(
            &plan,
            &remux_opts_for_recording(&self.store, rec_id),
            Some((self.events.clone(), rec_id as u64)),
        )
        .await;
        let promoted = final_path != plan.capture_path;
        let cache = plan.capture_path.parent().map(Path::to_path_buf);
        let capstem = plan
            .final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if promoted {
            if let (Some(cache), Some(od)) = (cache.as_deref(), final_path.parent()) {
                move_companions(cache, od, &capstem).await;
            }
            let want_games = template_wants_games(&row.monitor.filename_template);
            let want_title = template_wants_title(&row.monitor.filename_template);
            let want_went_live = template_wants_went_live(&row.monitor.filename_template);
            let want_media = template_wants_media(&row.monitor.filename_template);
            let media_mode = media_info_mode(&self.store);
            let do_post_media = want_media && media_mode.post();
            if do_post_media || want_games || want_title || want_went_live {
                let mi = if do_post_media {
                    probe_media(&final_path.to_string_lossy()).await
                } else {
                    None
                };
                let games = if want_games {
                    games_for_recording(&self.store, rec_id)
                } else {
                    String::new()
                };
                let title = if want_title {
                    title_for_recording(&self.store, rec_id)
                } else {
                    String::new()
                };
                let quality = resolved_quality(&row.monitor.quality);
                let use_sabr_res = sabr_selected(&row.monitor, &ytdlp_bins);
                let mode_res = recording_mode(&row.monitor, use_sabr_res, false);
                let stem = monitor_stem(
                    &row.monitor,
                    &row.channel.name,
                    rec.started_at,
                    rec.stream_id.as_deref(),
                    &title,
                    row.recording_count,
                    &quality,
                    mi.as_ref(),
                    &games,
                    row.monitor.tool.label(),
                    &mode_res,
                    row.monitor.platform().as_str(),
                    rec.went_live_at.unwrap_or(0),
                );
                final_path = rename_for_media(final_path, &stem).await;
            }
            if let Some(cache) = cache.as_deref() {
                purge_cache(cache, &capstem).await;
            }
        }

        let bytes = file_len(&final_path).await as i64;
        // Lost-time: a from-start capture that reached the live edge missed nothing.
        if let (Some(wl), Some(captured)) =
            (rec.went_live_at, media_duration_secs(&final_path).await)
        {
            let span = (ended - wl).max(0);
            if captured + CATCHUP_TOLERANCE_SECS >= span {
                let _ = self.store.set_recording_lost_secs(rec_id, 0);
            }
        }
        let ok = bytes > 0;
        let manually_stopped = self.stopping_monitors.lock().unwrap().remove(&monitor_id);
        let stall_killed = self
            .stall_killed
            .lock()
            .unwrap()
            .remove(&(DetachedKind::Recording, rec_id));
        let shutting_down = self.shutdown.load(Ordering::SeqCst);
        let status = if manually_stopped {
            if ok { "completed" } else { "stopped" }
        } else if shutting_down {
            "aborted"
        } else if ok {
            "completed"
        } else if stall_killed || stream_ended_or_unavailable(&outcome.log) {
            "ended"
        } else {
            "failed"
        };
        let _ = self.store.finish_recording(
            rec_id,
            now_unix(),
            bytes,
            outcome.exit_code,
            status,
            &final_path.to_string_lossy(),
            &outcome.log,
        );
        // Slot freed at capture exit — don't stomp a newer take's live state.
        if !self.active.lock().unwrap().contains_key(&monitor_id) {
            let _ = self
                .store
                .set_monitor_check_result(monitor_id, status, now_unix());
        }
        let _ = self.events.send(AppEvent::RecordingFinished {
            recording_id: rec_id,
            channel: row.channel.name.clone(),
            status: status.into(),
        });
        self.schedule_vod_check(rec_id, row.monitor.platform(), status, &row.monitor.url, rec.went_live_at, rec.went_live_approx);
        {
            let this = self.clone();
            tokio::spawn(async move { this.maybe_concat_backfill(rec_id).await });
        }
        info!(monitor_id, rec_id, bytes, status, "resumed recording finished");
        self.finalizing.lock().unwrap().remove(&monitor_id);
    }
    pub(super) async fn run_process(
        &self,
        active: &ActiveSet,
        id: i64,
        plan: &DownloadPlan,
        progress: Option<VideoProgress>,
        speed: Option<VideoSpeed>,
        ads: Option<AdSink>,
        detach: DetachReg,
    ) -> ProcessOutcome {
        // The tool's combined stdout+stderr go to a log file the app TAILS rather
        // than a pipe it reads: a pipe dies with the parent, but a file the child
        // owns keeps growing after the app detaches/quits — and a later launch can
        // re-open and re-tail it. Lives next to the capture under `.cache\`.
        let log_path = capture_log_path(&plan.capture_path, "log");
        let (out_h, err_h) = match open_log_pair(&log_path) {
            Ok(p) => p,
            Err(e) => {
                return ProcessOutcome {
                    exit_code: None,
                    log: format!("failed to open log {}: {e}", log_path.display()),
                };
            }
        };

        // A *named* job WITHOUT kill-on-close: the tool stays alive when we exit,
        // and a relaunch can re-open the job by name to stop the whole tree.
        let job_name = format!(
            "Local\\StreamArchiver_{}_{}",
            detach.kind.as_str(),
            detach.ref_id
        );
        let job = DetachedJob::create(&job_name).ok();

        let mut cmd = Command::new(&plan.program);
        // No kill_on_drop: the child must survive this task being dropped (detach).
        // A regular file handle (like a pipe) reads as isatty()=False, so yt-dlp
        // skips the console-width probe that crashes on a NUL handle.
        cmd.args(&plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_h))
            .stderr(Stdio::from(err_h));
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ProcessOutcome {
                    exit_code: None,
                    log: format!("failed to spawn {}: {e}", plan.program),
                };
            }
        };
        if let Some(j) = &job {
            if let Err(e) = j.assign_child(&child) {
                warn!("job assign failed: {e:#}");
            }
        }
        // Register the real PID so the scheduler skips this work and shutdown
        // can kill the whole process tree.
        let pid = child.id().unwrap_or(0);
        let proc_start = crate::platform::process_start_time(pid).unwrap_or(0);
        if pid != 0 {
            active.lock().unwrap().insert(id, pid);
        }
        // I/O-monitor registration for this tool + its descendants; the guard
        // drops (deregisters) when this function returns after child.wait().
        let _io_guard = (pid != 0).then(|| {
            crate::iomon::track_child(
                pid,
                crate::iomon::ChildInfo {
                    label: plan
                        .capture_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    tool: Path::new(&plan.program)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| plan.program.clone()),
                    purpose: if detach.secondary {
                        "capture (companion)".to_string()
                    } else if detach.kind == DetachedKind::Video {
                        "video download".to_string()
                    } else {
                        "live capture".to_string()
                    },
                    region: crate::iomon::classify(&plan.capture_path),
                    proc_start,
                },
            )
        });

        // Persist a registry row right after the spawn — synchronously, before the
        // first `.await` (child.wait below) — so a clean detach always has a row, and
        // only a crash in this sub-millisecond window could miss one. Deleted when
        // this download finalizes in-session below.
        if detach.ref_id != 0 && pid != 0 {
            let row = DetachedRow {
                kind: detach.kind,
                ref_id: detach.ref_id,
                monitor_id: detach.monitor_id,
                pid,
                proc_start,
                job_name: job_name.clone(),
                log_path: log_path.to_string_lossy().into_owned(),
                capture_path: plan.capture_path.to_string_lossy().into_owned(),
                final_path: plan.final_path.to_string_lossy().into_owned(),
                remux_to_mkv: plan.remux_to_mkv,
                take_group: detach.take_group.clone(),
                spawn_build: crate::version::build_id().to_string(),
                started_at: detach.started_at,
                secondary: detach.secondary,
                stream_id: detach.stream_id.clone(),
                went_live_at: detach.went_live_at,
            };
            if let Err(e) = self.store.register_detached(&row) {
                warn!("register detached process failed: {e:#}");
            }
        }

        // Ad-break processor (Twitch+streamlink only): the tailer forwards each
        // `(detected_at, duration)` over a channel; a dedicated task does the
        // (potentially slow) ffprobe + DB insert. Same helper drives re-attach.
        let (ad_tx, ad_task) = match ads {
            Some(sink) => {
                let (tx, jh) = spawn_ad_processor(sink);
                (Some(tx), Some(jh))
            }
            None => (None, None),
        };

        // One tailer reads the growing log file and does everything the two pipe
        // readers used to: parse progress/speed (yt-dlp `--progress-template`) and
        // streamlink ad-break lines. It stops once the process has exited (`done`)
        // and it has drained to EOF. Tailing a file (not a pipe) is what lets a
        // re-attach after restart reconstruct live state from the same code path.
        let done = Arc::new(AtomicBool::new(false));
        let tail = tokio::spawn(tail_log(
            log_path.clone(),
            0,
            progress.clone(),
            speed.clone(),
            id,
            ad_tx,
            done.clone(),
        ));

        // Stall watchdog: a wedged tool (hung live capture after stream end, a
        // stuck VOD download) never exits — kill it once output stops so this
        // wait returns and the normal finalize runs.
        let stall = self.spawn_stall_watchdog(
            detach.kind,
            detach.ref_id,
            pid,
            proc_start,
            log_path.clone(),
            plan.capture_path.clone(),
            done.clone(),
        );

        let status = child.wait().await;
        // The process exited *within this session* (we weren't dropped/detached):
        // let the tailer drain the log to EOF, then the ad processor finish, so
        // every line and ad break is recorded before the caller touches the file.
        done.store(true, Ordering::SeqCst);
        stall.abort();
        let _ = tail.await;
        if let Some(t) = ad_task {
            let _ = t.await;
        }
        // Closing the job here terminates any stragglers (e.g. yt-dlp's ffmpeg).
        if let Some(j) = &job {
            j.kill();
        }
        drop(job);

        // Finalized in-session, so drop the registry row (nothing to re-attach).
        if detach.ref_id != 0 {
            if let Err(e) = self.store.clear_detached(detach.kind, detach.ref_id) {
                warn!("clear detached process failed: {e:#}");
            }
        }

        let exit_code = status.ok().and_then(|s| s.code()).map(|c| c as i64);
        // The failure-reason excerpt is the tail of the on-disk log (replaces the
        // old in-memory ring, and works identically after a re-attach).
        let log = read_log_tail(&log_path, RING_MAX_LINES).await;
        ProcessOutcome { exit_code, log }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use crate::models::{Channel, Container, DetectionMethod, Monitor, Tool};
    #[allow(unused_imports)]
    use crate::downloader::test_util::*;

    #[test]
    fn parses_ytdlp_progress() {
        assert_eq!(parse_progress("DLPCT= 50.0%"), Some(0.5));
        assert_eq!(parse_progress("DLPCT=100.0%"), Some(1.0));
        assert_eq!(parse_progress("DLPCT=0.0%"), Some(0.0));
        // Non-marker lines and unknown values yield nothing.
        assert_eq!(parse_progress("[download]  45% of 100MiB"), None);
        assert_eq!(parse_progress("DLPCT=NA%"), None);
        assert_eq!(parse_progress("some other log line"), None);
    }

    #[test]
    fn parses_ytdlp_progress_and_speed() {
        // The combined template emits percent + speed (bytes/sec) per line.
        let (p, s) = parse_progress_fields("DLPCT= 42.0%;;SPEED=1258291.2");
        assert_eq!(p, Some(0.42));
        assert_eq!(s, Some(1_258_291.2));
        // Unknown speed ("NA") -> no speed, but the percent still parses.
        let (p, s) = parse_progress_fields("DLPCT= 5.0%;;SPEED=NA");
        assert_eq!(p, Some(0.05));
        assert_eq!(s, None);
        // Zero/negative speeds are ignored.
        assert_eq!(parse_progress_fields("DLPCT=10.0%;;SPEED=0").1, None);
        // A bare percent line (old format) still yields the fraction, no speed.
        assert_eq!(parse_progress_fields("DLPCT= 50.0%"), (Some(0.5), None));
        // Non-marker lines yield nothing.
        assert_eq!(parse_progress_fields("some other log line"), (None, None));
    }

    #[test]
    fn parses_streamlink_ad_break() {
        assert_eq!(
            parse_ad_break_secs(
                "[plugins.twitch][info] Detected advertisement break of 30 seconds"
            ),
            Some(30)
        );
        // Singular form ("1 second") and no log prefix.
        assert_eq!(
            parse_ad_break_secs("Detected advertisement break of 1 second"),
            Some(1)
        );
        // Other streamlink ad lines and unrelated lines don't match.
        assert_eq!(parse_ad_break_secs("Will skip ad segments"), None);
        assert_eq!(
            parse_ad_break_secs("Waiting for pre-roll ads to finish, be patient"),
            None
        );
        assert_eq!(parse_ad_break_secs("some other log line"), None);
    }
}
