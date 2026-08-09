//! CDN capture for **subscriber-only** Twitch broadcasts.
//!
//! When Twitch refuses the live edge with `UNAUTHORIZED_ENTITLEMENTS` (see
//! [`crate::models::sub_only_rejected`]) the stream can still be archived: the
//! broadcast's own DVR segments stay readable on the CDN, which is what
//! [head backfill](crate::head_backfill) already fetches.
//!
//! Before this module that happened *by accident*. Every doomed streamlink take
//! queued a head backfill, and each one re-fetched the broadcast **from its
//! start** — 22 takes, 11.8 GB per pass and growing, in the two-hour incident
//! that prompted this (Nyana Banyana, 2026-08-07). It worked, at roughly
//! quadratic cost, and only because a capture kept failing on a timer.
//!
//! Here it is deliberate instead. One session per broadcast:
//!
//! * **Nothing spawns streamlink.** While a session holds the monitor,
//!   `try_begin` declines to start captures at all — the live edge is not ours
//!   to take, and asking again every few minutes only produces log noise.
//! * **Each pass fetches only what's new.** `build_playlist`'s `skip_secs`
//!   window turns the fetch into a delta against what's already on disk, so a
//!   pass costs one refresh interval of video rather than the whole broadcast.
//! * **Parts are joined at the end, never rewritten during.** A pass writes a
//!   new numbered part; nothing already captured is touched again until the
//!   broadcast ends, at which point the parts are concatenated into the take's
//!   real file. A part is only deleted after that join verifies.
//! * **It resumes.** Parts carry the broadcast id in their name and a global
//!   index, so a restart mid-broadcast adopts what's on disk (including a head
//!   an earlier take had already fetched) and continues from there instead of
//!   starting over.

use super::*;

/// How often a live session extends its coverage.
///
/// This is the archive's lag behind the live edge, and — because the CDN can't
/// serve what hasn't been segmented yet — also roughly the worst-case gap at
/// the end of a broadcast. Five minutes is affordable now that a pass costs one
/// interval of video instead of the whole stream.
pub(super) const REFRESH_SECS: u64 = 300;

/// Don't chase the very live edge: segments need a moment to land on the CDN,
/// and a pass that reaches for the last few seconds returns a short/empty
/// window and wastes the round trip.
const EDGE_LAG_SECS: f64 = 45.0;

/// Skip a pass whose delta is shorter than this — a sub-minute part costs more
/// in mux overhead and concat entries than it's worth, and the next pass picks
/// the span up anyway.
const MIN_CHUNK_SECS: f64 = 45.0;

/// How long the loop waits for the broadcast's CDN folder to appear before
/// giving up (it can lag go-live by a minute or two).
const RESOLVE_ATTEMPTS: usize = 5;

/// `{stem}.cdnpart-007.mkv` — the parts a session writes.
///
/// The index is **per broadcast, not per take**: adoption continues from the
/// highest index already on disk, so parts written by an earlier session (or
/// before a restart) stay in order with the ones written after it.
pub(crate) fn part_name(stem: &str, index: usize) -> String {
    format!("{stem}{PART_INFIX}{index:03}.mkv")
}

/// What marks a file as one of a broadcast's CDN parts. Shared with the media
/// player, which offers the parts as a playlist while the session is still
/// running — the two must recognise the same files or "Play local recording"
/// silently finds nothing for exactly the takes that need it most.
pub(crate) const PART_INFIX: &str = ".cdnpart-";

/// Pull the index back out of a part filename, for ordering and for continuing
/// the numbering after a restart.
pub(crate) fn part_index(name: &str) -> Option<usize> {
    let tail = name.rsplit_once(PART_INFIX)?.1;
    tail.strip_suffix(".mkv")?.parse().ok()
}

/// How much new video a pass should ask the CDN for, or `None` to skip this
/// interval.
///
/// `elapsed` is seconds since go-live, `covered` is what's already on disk.
/// Two guards, both learned the hard way in the incident this module comes
/// from: never reach into the last [`EDGE_LAG_SECS`] (segments that haven't
/// been published yet come back as an empty window and waste the round trip),
/// and never bother with a sliver — the next pass picks it up.
fn next_window(elapsed: f64, covered: f64) -> Option<f64> {
    let want = elapsed - EDGE_LAG_SECS - covered;
    (want >= MIN_CHUNK_SECS).then_some(want)
}

/// What a session already holds when it starts: the parts on disk for this
/// broadcast, in order, and how many seconds from go-live they cover.
struct Adopted {
    parts: Vec<PathBuf>,
    covered_secs: f64,
    next_index: usize,
}

impl Supervisor {
    /// Start a CDN capture session for a subscriber-only broadcast, unless one
    /// is already running for this monitor.
    ///
    /// Called from the capture-failed path the moment a take is refused. The
    /// take that was refused becomes the session's anchor: it owns the final
    /// joined file, so the broadcast keeps exactly one row in the archive
    /// rather than one per doomed retry.
    pub(super) fn maybe_start_sub_only_session(
        &self,
        row: &MonitorWithChannel,
        rec_id: i64,
        stream_id: Option<&str>,
        went_live_at: Option<i64>,
        final_path: &Path,
    ) {
        if row.monitor.platform() != Platform::Twitch {
            return; // the CDN segment path is Twitch-only
        }
        let monitor_id = row.monitor.id;
        let (Some(stream_id), Some(went_live_at)) =
            (stream_id.filter(|s| !s.is_empty()), went_live_at.filter(|t| *t > 0))
        else {
            // Without the broadcast id and its go-live time there's no CDN
            // folder to resolve — fall back to the retry cadence.
            info!(
                monitor_id,
                rec_id, "sub-only: no stream id / go-live time — cannot open a CDN session"
            );
            return;
        };
        let abort = Arc::new(AtomicBool::new(false));
        {
            let mut sessions = self.sub_only_sessions.lock().unwrap();
            if sessions.contains_key(&monitor_id) {
                return; // already covered
            }
            sessions.insert(monitor_id, CdnCapture { rec_id, abort: abort.clone() });
        }
        info!(
            monitor_id,
            rec_id,
            stream_id,
            "sub-only: opening a CDN capture session (streamlink suppressed for this broadcast)"
        );
        let this = self.clone();
        let row = row.clone();
        let stream_id = stream_id.to_string();
        let final_path = final_path.to_path_buf();
        tokio::spawn(async move {
            this.sub_only_session(row, rec_id, stream_id, went_live_at, final_path, abort).await;
            this.sub_only_sessions.lock().unwrap().remove(&monitor_id);
        });
    }

    /// True while a CDN session owns this monitor — `try_begin` uses it to stop
    /// spawning captures that can only be refused.
    pub(super) fn sub_only_session_active(&self, monitor_id: i64) -> bool {
        self.sub_only_sessions.lock().unwrap().contains_key(&monitor_id)
    }

    /// Stop a monitor's CDN session (manual Stop, shutdown). The session
    /// finishes its current pass, joins what it has, and exits.
    pub(super) fn abort_sub_only_session(&self, monitor_id: i64) {
        if let Some(s) = self.sub_only_sessions.lock().unwrap().get(&monitor_id) {
            s.abort.store(true, Ordering::SeqCst);
            info!(monitor_id, "sub-only: CDN session asked to wrap up");
        }
    }

    /// The session itself: resolve the broadcast's playlist once, then extend
    /// coverage until the stream ends, and join the parts.
    async fn sub_only_session(
        &self,
        row: MonitorWithChannel,
        rec_id: i64,
        stream_id: String,
        went_live_at: i64,
        final_path: PathBuf,
        abort: Arc<AtomicBool>,
    ) {
        let monitor_id = row.monitor.id;
        // A take that captured nothing was never promoted, so its `final_path`
        // can still point at the working file inside the capture cache. Parts
        // (and the file they're joined into) belong in the ARCHIVE folder:
        // leaving them in `.sa-cache` would make the finished take read as
        // "stuck in cache" and hand it to sweeps that have no business seeing
        // a finished archive.
        let final_path = strip_cache_component(&final_path).unwrap_or(final_path);
        let (Some(out_dir), Some(stem)) = (
            final_path.parent().map(Path::to_path_buf),
            final_path.file_stem().map(|s| s.to_string_lossy().into_owned()),
        ) else {
            return;
        };
        let Some(login) = crate::detectors::twitch_login(&row.monitor.url) else {
            return;
        };

        let client = self.ctx.http_client();
        let hosts = crate::recovery::load_hosts(&self.store);
        let max_conc = crate::recovery::load_max_conc(&self.store);
        let inputs = crate::recovery::RecoveryInputs {
            login,
            broadcast_id: stream_id.clone(),
            start_epoch: went_live_at,
            went_live_approx: false,
            vod_id: None, // live broadcast — hash-probe path only
        };
        let mut found = None;
        for attempt in 0..RESOLVE_ATTEMPTS {
            if self.shutdown.load(Ordering::SeqCst) || abort.load(Ordering::SeqCst) {
                return;
            }
            if attempt > 0 && !self.sleep_watching(60, &abort).await {
                return;
            }
            found = crate::recovery::resolve_playlist(&client, &inputs, &hosts, max_conc).await;
            if found.is_some() {
                break;
            }
        }
        let Some(found) = found else {
            warn!(
                monitor_id,
                rec_id, "sub-only: the broadcast's CDN playlist never resolved — no session"
            );
            return;
        };
        let playlist_url = found.url.clone();

        // The take that triggered this session may still have an ordinary head
        // backfill in flight — it was queued at capture start, before Twitch
        // refused us. Let it land first: it will be LONGER than anything on
        // disk right now, and adopting the shorter one would make every part
        // this session writes overlap it.
        self.await_head_backfills(monitor_id, &stream_id, &abort).await;

        // What's already on disk for this broadcast: parts from an earlier
        // session (or from before a restart), plus any head an ordinary
        // backfill fetched — that head starts at go-live, so it's part zero.
        let mut state = self.adopt_sub_only_parts(monitor_id, &stream_id, &out_dir).await;
        info!(
            monitor_id,
            rec_id,
            parts = state.parts.len(),
            covered_secs = state.covered_secs as i64,
            "sub-only: CDN session running"
        );

        let cache = cache_dir(&out_dir);
        let _ = crate::iomon::fs::create_dir_all(Cat::Recovery, &cache).await;
        set_cache_hidden(&cache);

        loop {
            let stopping = self.shutdown.load(Ordering::SeqCst) || abort.load(Ordering::SeqCst);
            // The broadcast's own end is the normal exit: one last pass to pick
            // up whatever landed since, then join.
            let live = self
                .store
                .get_monitor_with_channel(monitor_id)
                .ok()
                .flatten()
                .is_some_and(|m| m.monitor.last_state == "live");
            self.sub_only_pass(
                &client,
                &playlist_url,
                max_conc,
                rec_id,
                monitor_id,
                &row.channel.name,
                &out_dir,
                &cache,
                &stem,
                went_live_at,
                &mut state,
            )
            .await;
            if stopping || !live {
                break;
            }
            if !self.sleep_watching(REFRESH_SECS, &abort).await {
                // Shutdown mid-wait: keep the parts, join on the next run.
                info!(monitor_id, rec_id, "sub-only: session interrupted — parts kept for resume");
                return;
            }
        }
        self.finish_sub_only_session(rec_id, monitor_id, &row, &out_dir, &stem, &state).await;
    }

    /// Sleep in short slices, giving up early on shutdown/abort. Returns false
    /// when it was cut short.
    async fn sleep_watching(&self, secs: u64, abort: &Arc<AtomicBool>) -> bool {
        for _ in 0..(secs * 4) {
            if self.shutdown.load(Ordering::SeqCst) || abort.load(Ordering::SeqCst) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        true
    }

    /// Block until no head backfill for this broadcast is still running (or the
    /// wait times out — a stuck job must not strand the session forever).
    async fn await_head_backfills(
        &self,
        monitor_id: i64,
        stream_id: &str,
        abort: &Arc<AtomicBool>,
    ) {
        /// Generous: a full head fetch of a long broadcast is minutes of
        /// download. Past this we adopt what's there and carry on — a later
        /// pass's own duration check keeps the accounting honest.
        const MAX_WAIT_SECS: u64 = 20 * 60;
        let mine: HashSet<i64> = self
            .store
            .earlier_takes_for_stream(monitor_id, stream_id, i64::MAX)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        if mine.is_empty() {
            return;
        }
        let mut waited = 0;
        while waited < MAX_WAIT_SECS {
            let busy = {
                let jobs = self.head_backfill_aborts.lock().unwrap();
                jobs.keys().any(|id| mine.contains(id))
            };
            if !busy {
                return;
            }
            info!(monitor_id, "sub-only: waiting for an in-flight head backfill before adopting");
            if !self.sleep_watching(15, abort).await {
                return;
            }
            waited += 15;
        }
    }

    /// Find what this broadcast already has on disk, so a session never
    /// re-fetches a second of it: every `.cdnpart-NNN.mkv` naming this
    /// broadcast, in index order, plus the **longest** head an ordinary
    /// backfill left behind (every head starts at go-live, so the longest one
    /// is the best part zero — and picking deterministically is what keeps the
    /// parts written after it from overlapping).
    async fn adopt_sub_only_parts(
        &self,
        monitor_id: i64,
        stream_id: &str,
        out_dir: &Path,
    ) -> Adopted {
        let mut parts: Vec<(usize, PathBuf)> = Vec::new();
        // Parts are named after the take that wrote them, but every take's
        // stem carries `[Twitch {stream_id}]`, so one broadcast's parts are
        // findable across takes — which is what makes a restart resumable.
        //
        // The capture cache is searched too: parts written before the archive
        // folder became the destination live there, and a part is a part
        // wherever it sits — re-fetching video we already hold would be the
        // one unforgivable outcome here.
        let mut dirs = vec![out_dir.to_path_buf()];
        let cache = cache_dir(out_dir);
        if cache != out_dir {
            dirs.push(cache);
        }
        for dir in dirs {
            let Ok(mut rd) = crate::iomon::fs::read_dir(Cat::Recovery, &dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = rd.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.contains(stream_id) {
                    continue;
                }
                if let Some(i) = part_index(&name) {
                    parts.push((i, entry.path()));
                }
            }
        }
        // One entry per index: the same part can exist in both folders (one
        // was moved, or a copy was left behind), and counting it twice would
        // inflate `covered` and skip past unfetched video — a silent hole.
        parts.sort_by_key(|(i, _)| *i);
        parts.dedup_by_key(|(i, _)| *i);
        let next_index = parts.last().map(|(i, _)| i + 1).unwrap_or(1);

        // Part zero: the longest existing head for this broadcast. Each covers
        // go-live → its own take's start, which is exactly the prefix a fresh
        // session would otherwise re-download in full.
        let mut head: Option<(i64, PathBuf)> = None;
        for (_, p) in self
            .store
            .recordings_with_backfill_for_stream(monitor_id, stream_id, -1)
            .unwrap_or_default()
        {
            let path = PathBuf::from(p);
            if !crate::iomon::fs::is_file_sync(Cat::Recovery, &path) {
                continue;
            }
            let secs = media_duration_secs(&path).await.unwrap_or(0);
            if head.as_ref().is_none_or(|(best, _)| secs > *best) {
                head = Some((secs, path));
            }
        }
        let head = head.map(|(_, p)| p);

        let mut ordered: Vec<PathBuf> = Vec::new();
        if let Some(h) = head {
            ordered.push(h);
        }
        ordered.extend(parts.into_iter().map(|(_, p)| p));

        let mut covered_secs = 0.0;
        for p in &ordered {
            covered_secs += media_duration_secs(p).await.unwrap_or(0) as f64;
        }
        Adopted { parts: ordered, covered_secs, next_index }
    }

    /// One coverage-extending pass: fetch `[covered, live edge)` from the CDN
    /// into a new part. A no-op when there isn't enough new video yet.
    #[allow(clippy::too_many_arguments)]
    async fn sub_only_pass(
        &self,
        client: &reqwest::Client,
        playlist_url: &str,
        max_conc: usize,
        rec_id: i64,
        monitor_id: i64,
        channel: &str,
        out_dir: &Path,
        cache: &Path,
        stem: &str,
        went_live_at: i64,
        state: &mut Adopted,
    ) {
        // How much of the broadcast exists that we don't hold yet. Measured
        // against the wall clock rather than the playlist so a pass can be
        // skipped without paying for a fetch.
        let elapsed = (now_unix() - went_live_at).max(0) as f64;
        let Some(want) = next_window(elapsed, state.covered_secs) else {
            return;
        };
        let task_id = crate::events::next_task_id();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::HeadBackfill(rec_id),
            label: channel.to_string(),
            detail: format!("subscriber-only: fetching {}s from the CDN", want as i64),
            started_at: now_unix(),
            progress: Some(0.0),
            progress_info: None,
        }));
        let finish = |outcome: crate::events::TaskOutcome| {
            let _ = self.events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        };

        let playlist = match crate::recovery::build_playlist(
            client,
            playlist_url,
            max_conc,
            false,
            Some(want),
            Some(state.covered_secs),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(rec_id, "sub-only: playlist window failed: {e:#}");
                finish(crate::events::TaskOutcome::Failed(format!("playlist: {e:#}")));
                return;
            }
        };
        let pl_path = cache.join(format!("{stem}.cdnpart-{:03}.m3u8", state.next_index));
        if let Err(e) = crate::iomon::fs::write(Cat::Recovery, &pl_path, &playlist.text).await {
            warn!(rec_id, "sub-only: cannot write playlist: {e:#}");
            finish(crate::events::TaskOutcome::Failed(format!("write playlist: {e}")));
            return;
        }
        let tmp = cache.join(part_name(stem, state.next_index));
        let muxed = crate::recovery::mux_playlist_to_mkv(
            &pl_path,
            &tmp,
            Some((self.events.clone(), task_id)),
            Some(want),
            "subscriber-only CDN capture",
            None,
        )
        .await;
        let _ = crate::iomon::fs::remove_file(Cat::Recovery, &pl_path).await;
        if let Err(e) = muxed {
            warn!(rec_id, "sub-only: mux failed: {e:#}");
            finish(crate::events::TaskOutcome::Failed(format!("mux: {e:#}")));
            let _ = crate::iomon::fs::remove_file(Cat::Recovery, &tmp).await;
            return;
        }
        // Trust the muxed file's real duration, not what we asked for: the CDN
        // may have had less than the wall clock implied, and over-counting here
        // would make the NEXT pass skip past unfetched video — a silent hole.
        let got = media_duration_secs(&tmp).await.unwrap_or(0) as f64;
        if got < 1.0 {
            info!(rec_id, "sub-only: pass produced nothing usable — retrying next interval");
            let _ = crate::iomon::fs::remove_file(Cat::Recovery, &tmp).await;
            finish(crate::events::TaskOutcome::Failed("empty window".into()));
            return;
        }
        let dest = out_dir.join(part_name(stem, state.next_index));
        if let Err(e) = crate::iomon::fs::rename(Cat::Promote, &tmp, &dest).await {
            warn!(rec_id, "sub-only: cannot move part into place: {e:#}");
            finish(crate::events::TaskOutcome::Failed(format!("move: {e}")));
            return;
        }
        state.covered_secs += got;
        state.next_index += 1;
        state.parts.push(dest);
        info!(
            monitor_id,
            rec_id,
            got = got as i64,
            covered = state.covered_secs as i64,
            "sub-only: extended CDN coverage"
        );
        finish(crate::events::TaskOutcome::CompletedWithNote(format!("+{}s", got as i64)));
        let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
    }

    /// Join the session's parts into the take's real file once the broadcast is
    /// over.
    ///
    /// The parts are the archive until this succeeds, so they are only disposed
    /// of after the joined file exists AND its duration matches what went in.
    /// A failed join leaves everything exactly where it was — the Issues panel
    /// shows the take, and the parts are still playable in order.
    async fn finish_sub_only_session(
        &self,
        rec_id: i64,
        monitor_id: i64,
        row: &MonitorWithChannel,
        out_dir: &Path,
        stem: &str,
        state: &Adopted,
    ) {
        if state.parts.is_empty() {
            info!(monitor_id, rec_id, "sub-only: session ended with nothing captured");
            return;
        }
        if state.parts.len() == 1 {
            // Nothing to join — adopt the single part as the take's file.
            let only = &state.parts[0];
            let bytes = crate::iomon::fs::metadata(Cat::Promote, only)
                .await
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            let _ = self.store.finish_recording(
                rec_id,
                now_unix(),
                bytes,
                Some(0),
                "completed",
                &only.to_string_lossy(),
                "",
            );
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
            info!(monitor_id, rec_id, "sub-only: single CDN part adopted as the take's file");
            return;
        }
        let cache = cache_dir(out_dir);
        let _ = crate::iomon::fs::create_dir_all(Cat::Promote, &cache).await;
        let tmp = cache.join(format!("{stem}.cdn.mkv"));
        let entries: Vec<ConcatEntry> =
            state.parts.iter().map(|p| ConcatEntry::whole(p)).collect();
        let expected = state.covered_secs as i64;
        info!(
            monitor_id,
            rec_id,
            parts = state.parts.len(),
            expected,
            "sub-only: joining CDN parts"
        );
        if let Err(e) = concat_mkvs_n(
            &self.store,
            &self.shutdown,
            FfmpegJobKind::HeadBackfillJoin,
            rec_id,
            Some(expected),
            &cache,
            &entries,
            &tmp,
        )
        .await
        {
            warn!(rec_id, "sub-only: join failed — keeping the parts as-is: {e:#}");
            return;
        }
        // Verify before anything is thrown away: a join that silently dropped a
        // part must not cost us the parts it dropped.
        let joined = media_duration_secs(&tmp).await.unwrap_or(0);
        if (joined - expected).abs() > (30.max(expected / 50)) {
            warn!(
                rec_id,
                joined,
                expected,
                "sub-only: joined file is the wrong length — keeping the parts, not promoting it"
            );
            return;
        }
        let dest = match rename_or_shorten(&tmp, out_dir, stem, "cdn.mkv").await {
            Ok(d) => d,
            Err(e) => {
                warn!(rec_id, "sub-only: cannot promote the joined file: {e:#}");
                return;
            }
        };
        let bytes = crate::iomon::fs::metadata(Cat::Promote, &dest)
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let _ = self.store.finish_recording(
            rec_id,
            now_unix(),
            bytes,
            Some(0),
            "completed",
            &dest.to_string_lossy(),
            "",
        );
        // The take's own head (part zero) is now inside the joined file; clear
        // the pointer so nothing offers to re-join it.
        let _ = self.store.clear_recording_backfill_path(rec_id);
        let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
        info!(
            monitor_id,
            rec_id,
            secs = joined,
            "sub-only: CDN capture joined into the take's file"
        );
        // Only now are the parts redundant. Disposal follows the configured
        // method, and a failure to remove one is never escalated — the joined
        // file already exists, so a leftover part is clutter, not damage.
        for p in &state.parts {
            match crate::disposal::dispose_media(
                &self.store,
                row.channel.id,
                monitor_id,
                p,
                rec_id,
                "subscriber-only CDN part joined",
            )
            .await
            {
                Ok(d) => debug!(rec_id, path = %p.display(), "sub-only: part {}", d.describe()),
                Err(e) => warn!(rec_id, path = %p.display(), "sub-only: part cleanup: {e:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_names_round_trip_and_sort_by_index() {
        let stem = "Chan - 2026-08-07 - t [Twitch 320910935515]";
        let n = part_name(stem, 7);
        assert_eq!(n, format!("{stem}.cdnpart-007.mkv"));
        assert_eq!(part_index(&n), Some(7));
        // Zero-padding is what makes a plain name sort match index order for
        // the first thousand parts (~3.5 days at the refresh interval).
        let mut names = [part_name(stem, 10), part_name(stem, 2), part_name(stem, 1)];
        names.sort();
        assert_eq!(names[0], part_name(stem, 1));
        assert_eq!(names[2], part_name(stem, 10));
    }

    #[test]
    fn a_pass_fetches_only_the_delta_and_stays_off_the_live_edge() {
        // Two hours in, holding the first hour: fetch the hour we're missing,
        // minus the unpublished tail — not the whole broadcast, which is what
        // the accidental head-backfill loop used to do every few minutes.
        let want = next_window(7200.0, 3600.0).expect("a delta this big is worth a pass");
        assert_eq!(want as i64, 3600 - EDGE_LAG_SECS as i64);

        // Caught up: nothing to do until more of the stream exists.
        assert_eq!(next_window(7200.0, 7200.0), None);
        // A sliver isn't worth a mux — the next pass will take it.
        assert_eq!(next_window(7200.0, 7200.0 - EDGE_LAG_SECS - 10.0), None);
        // Just over the floor is.
        assert!(next_window(7200.0, 7200.0 - EDGE_LAG_SECS - MIN_CHUNK_SECS - 1.0).is_some());
        // Holding MORE than the wall clock implies (an over-long adopted head,
        // clock skew) must never produce a negative window.
        assert_eq!(next_window(600.0, 900.0), None);
        // A brand-new broadcast has nothing published yet.
        assert_eq!(next_window(5.0, 0.0), None);
    }

    #[test]
    fn part_index_ignores_everything_that_isnt_a_part() {
        assert_eq!(part_index("Chan - t [Twitch 123].head.mkv"), None);
        assert_eq!(part_index("Chan - t [Twitch 123].mkv"), None);
        assert_eq!(part_index("Chan - t [Twitch 123].cdnpart-.mkv"), None);
        assert_eq!(part_index("Chan - t [Twitch 123].cdnpart-abc.mkv"), None);
        // The joined result must never be mistaken for one of its own inputs.
        assert_eq!(part_index("Chan - t [Twitch 123].cdn.mkv"), None);
    }
}
