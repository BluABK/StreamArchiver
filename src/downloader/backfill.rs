//! Head backfill jobs, supersede/refetch, split-capture merge and
//! backfill concat.

use super::*;

impl Supervisor {
    /// Re-drive head backfills whose planned state (`head_backfill_state =
    /// 'queued'`) outlived the in-memory job that owned it — the job dies with
    /// the process (restart/crash mid-settle), and without this the Background
    /// panel shows a "Planned" row forever. Rows that can't be re-driven get
    /// the flag cleared instead of dangling.
    pub async fn requeue_stale_head_backfills(&self) {
        let ids = match self.store.recordings_head_backfill_queued() {
            Ok(ids) => ids,
            Err(e) => {
                warn!("head backfill requeue: failed to load queued rows: {e:#}");
                return;
            }
        };
        for rec_id in ids {
            let redrivable = self
                .store
                .get_recording(rec_id)
                .ok()
                .flatten()
                .filter(|rec| rec.stream_id.is_some() && rec.went_live_at.is_some())
                .and_then(|rec| self.store.get_monitor_with_channel(rec.monitor_id).ok().flatten())
                .is_some_and(|mw| mw.monitor.platform() == Platform::Twitch);
            if !redrivable {
                warn!(rec_id, "head backfill requeue: row can no longer be re-driven, clearing its planned state");
                let _ = self.store.set_head_backfill_state(rec_id, "");
                continue;
            }
            info!(rec_id, "head backfill requeue: re-driving job interrupted by a restart");
            let this = self.clone();
            tokio::spawn(async move { this.manual_head_backfill(rec_id, None).await });
        }
    }
    /// Backfill a late-joined Twitch capture's missed beginning from the
    /// growing published-VOD playlist while the stream is still live. Runs as
    /// its own task (it may outlive the recording), then hands off to
    /// [`Self::maybe_concat_backfill`]. Downloading DURING the stream matters:
    /// DMCA mutes land minutes after stream end and scrub the originals, so
    /// the head fetched now carries the original audio.
    ///
    /// Callers mark `Recording.head_backfill_state = "queued"` right as this
    /// is spawned (before awaiting it) so the Streams grid has something to
    /// show from the very start — this function clears it back to `""` at
    /// every exit point once a real determination is made (see
    /// [`HEAD_BACKFILL_SETTLE_SECS`]).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn head_backfill_job(
        &self,
        monitor_id: i64,
        channel_id: i64,
        rec_id: i64,
        capture_path: PathBuf,
        final_path: PathBuf,
        monitor_url: String,
        channel: String,
        stream_id: String,
        went_live_at: i64,
        went_live_approx: bool,
        started_at: i64,
        // `None` = `capture_path` is still actively growing — measure "missed"
        // against a fresh `now_unix()` (today's behavior). `Some(t)` = a fixed
        // historical reference (a take that's already finished, e.g. a manual
        // re-trigger long after the fact): using a live `now_unix()` there
        // would make "missed" grow unboundedly with how long ago the take
        // ended, since `capture_path`'s duration is a static number once the
        // take is done.
        missed_reference: Option<i64>,
        // User-initiated (the "Backfill head" manual action) ⇒ bypass the
        // "fetch new head backfill on new take" setting for a non-first take.
        // Does NOT bypass the `recording_lost_secs == 0` / `missed < 60`
        // sanity short-circuits below (those are factual, not settings), nor
        // the separate "replace old heads" setting later in this function.
        force: bool,
        // `Some(n)` = trigger-configured leadtime mode: fetch a fixed n-second
        // window ending at this take's own start (not the usual "from the
        // stream's true go-live"), for a mid-broadcast trigger start. `None` =
        // today's behavior, unchanged.
        lead_secs: Option<i64>,
        // `Some("720p60")` = fetch the head at this Twitch rendition instead of
        // source — the match-the-live-capture re-fetch for a head/live codec
        // mismatch (live joined before the source rendition was listed).
        // `None` = source (today's behavior).
        quality: Option<String>,
    ) {
        // Let the CDN folder appear and streamlink's own rewind (if any) settle
        // — this is a fixed grace period, not proportional to how late the
        // recording joined; even an instant join needs it. `Recording.
        // head_backfill_state == "queued"` (set by the caller right as this
        // task is spawned) is what covers this window in the UI, since
        // nothing else here is externally visible until it ends.
        for _ in 0..(HEAD_BACKFILL_SETTLE_SECS * 4) {
            if self.shutdown.load(Ordering::SeqCst) {
                let _ = self.store.set_head_backfill_state(rec_id, "");
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // --hls-live-restart actually rewound to the start → nothing was missed.
        if self.store.recording_lost_secs(rec_id).ok().flatten() == Some(0) {
            let _ = self.store.set_head_backfill_state(rec_id, "");
            return;
        }
        // The earliest take of a stream always owns the missed HEAD; a later
        // take (a reconnect/retake) only gets its own FRESH, FULL head fetch
        // (go-live through THIS take's start — the missed/head_secs math
        // below already computes that naturally, unmodified, since it's keyed
        // off the constant `went_live_at`) when the setting is on, or always
        // when `force` (a manual trigger).
        let is_first_take = self
            .store
            .is_first_take_for_stream(monitor_id, &stream_id, started_at)
            .unwrap_or(false);
        if !is_first_take
            && !force
            && lead_secs.is_none()
            && !crate::head_backfill::effective_fetch_new_take(&self.store, channel_id, monitor_id)
        {
            let _ = self.store.set_head_backfill_state(rec_id, "");
            return;
        }
        let Some(login) = crate::detectors::twitch_login(&monitor_url) else {
            let _ = self.store.set_head_backfill_state(rec_id, "");
            return;
        };
        let Some(out_dir) = final_path.parent().map(Path::to_path_buf) else {
            let _ = self.store.set_head_backfill_state(rec_id, "");
            return;
        };
        let Some(stem) = final_path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            let _ = self.store.set_head_backfill_state(rec_id, "");
            return;
        };

        // Missed head length: prefer measuring the growing capture against the
        // wall clock (accounts for any partial rewind); fall back to the plain
        // start-delay. A very young .ts can refuse a duration — retry a bit.
        let mut captured: Option<i64> = None;
        for attempt in 0..3 {
            if attempt > 0 {
                for _ in 0..(30 * 4) {
                    if self.shutdown.load(Ordering::SeqCst) {
                        let _ = self.store.set_head_backfill_state(rec_id, "");
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            captured = media_duration_secs(&capture_path).await;
            if captured.is_some() {
                break;
            }
        }
        // The raw growing `.ts` also carries the broadcast's own MPEG-TS
        // timeline (the PTS-exact splice anchor, see `pts_capture_offset`).
        // Grab it while the un-remuxed file still exists and persist it, so a
        // later manual re-run — by then the take is a timestamp-reset MKV —
        // can still splice exactly. A finished take skips the probe (wrong
        // timeline) and reads back what a live run persisted, if anything.
        let live_start_pts = match if missed_reference.is_none() {
            media_start_time_secs(&capture_path).await
        } else {
            None
        } {
            Some(pts) => {
                let _ = self.store.set_recording_capture_start_pts(rec_id, pts);
                Some(pts)
            }
            None => self.store.recording_capture_start_pts(rec_id).ok().flatten(),
        };
        // Wall-clock estimate of the capture's own join offset (seconds since
        // go-live). It overshoots the true offset by the broadcast latency —
        // the PTS math below corrects that when it can, and sanity-checks
        // against this estimate when it does.
        let mut join_estimate = compute_missed_secs(
            went_live_at,
            started_at,
            captured,
            missed_reference.unwrap_or_else(now_unix),
        );
        // Lead-time mode: `missed` is the configured fixed window, not derived
        // from go-live/captured — and it's an explicit user setting, not an
        // accidental-gap heuristic, so the `< 60s` sanity floor below doesn't
        // apply to it either.
        let mut missed = match lead_secs {
            Some(lead) => lead,
            None => join_estimate,
        };
        if lead_secs.is_none() && missed < 60 {
            info!(rec_id, missed, "head backfill: gap too small, skipping");
            let _ = self.store.set_head_backfill_state(rec_id, "");
            return;
        }

        // The pre-fetch decision is made — from here on the live background
        // task (below) and, on success, `backfill_path` are the visible
        // signals; the "queued" pending state has done its job.
        let _ = self.store.set_head_backfill_state(rec_id, "");
        let task_id = crate::events::next_task_id();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::HeadBackfill(rec_id),
            label: channel.clone(),
            detail: format!("missed ~{missed}s — fetching from the live VOD"),
            started_at: now_unix(),
            progress: Some(0.0),
            progress_info: None,
        }));
        let finish = |outcome: crate::events::TaskOutcome| {
            let _ = self.events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        };

        let client = self.ctx.http_client();
        let hosts = crate::recovery::load_hosts(&self.store);
        let max_conc = crate::recovery::load_max_conc(&self.store);
        let inputs = crate::recovery::RecoveryInputs {
            login,
            broadcast_id: stream_id,
            start_epoch: went_live_at,
            went_live_approx,
            vod_id: None, // not published yet — hash-probe path only
        };
        // The folder can lag the stream start a little; retry a few times.
        let mut found = None;
        for attempt in 0..5 {
            if attempt > 0 {
                for _ in 0..(60 * 4) {
                    if self.shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            found = crate::recovery::resolve_playlist(&client, &inputs, &hosts, max_conc).await;
            if found.is_some() {
                break;
            }
        }
        let Some(found) = found else {
            // VODs disabled and folder absent — quiet failure, no error toast.
            info!(rec_id, "head backfill: {} live playlist not found on the CDN", Platform::Twitch.tag());
            finish(crate::events::TaskOutcome::Failed("live playlist not found".into()));
            return;
        };
        if captured.is_none() {
            // The capture wouldn't probe; the matched folder second is the true
            // go-live moment — refine the fallback estimate with it. `missed`
            // only follows in normal mode: in lead-time mode it's the fixed
            // configured window, not a "distance since go-live" estimate.
            join_estimate = (started_at - found.matched_epoch).max(0);
            if lead_secs.is_none() {
                missed = join_estimate;
            }
        }

        // Source quality unless a specific rendition was requested (the
        // match-the-live-capture re-fetch), verified against what exists.
        let playlist_url = match &quality {
            Some(q) => crate::recovery::playlist_at_quality(&client, &found, q, max_conc).await,
            None => found.url.clone(),
        };
        let cache = cache_dir(&out_dir);
        let _ = crate::iomon::fs::create_dir_all(Cat::Recovery, &cache).await;
        set_cache_hidden(&cache);
        // PTS-exact splice point: the capture's `.ts` and the DVR playlist's
        // segments share the broadcast's own MPEG-TS timeline, so their
        // start_time difference is exactly where the capture joined. The
        // wall-clock estimate overshoots that by the broadcast latency
        // (~5-15s), which used to duplicate that much media at the full.mkv
        // seam (the 6s backwards jumpcut). Fall back to the estimate when
        // either anchor is unavailable or they disagree wildly.
        let pts_offset = match live_start_pts {
            Some(live) => {
                crate::recovery::first_segment_start_secs(&client, &playlist_url, max_conc, &cache)
                    .await
                    .and_then(|seg0| pts_capture_offset(live, seg0, join_estimate as f64))
            }
            None => None,
        };
        match pts_offset {
            Some(o) => info!(
                rec_id,
                "head backfill: PTS-exact splice at {o:.2}s (wall-clock estimate {join_estimate}s, corrected {:+.2}s)",
                o - join_estimate as f64
            ),
            None => info!(
                rec_id,
                "head backfill: no PTS splice anchor — cutting at the wall-clock estimate {join_estimate}s"
            ),
        }
        // Fetch only the head (+ a few seconds of seam overlap), fresh segments,
        // no probing needed; `-t` trims the mux to the splice point. In
        // lead-time mode the slice doesn't start at the playlist's own
        // beginning (the true go-live) — it starts `lead_secs` before THIS
        // take's own start, so the CDN playlist (found via the real go-live
        // anchor, which folder discovery still needs) gets an extra skip
        // offset instead of being read from position zero.
        let head_secs = match (lead_secs, pts_offset) {
            (Some(lead), _) => lead as f64,
            (None, Some(offset)) => offset,
            (None, None) => missed as f64,
        };
        let skip_secs = lead_secs.map(|lead| match pts_offset {
            Some(offset) => (offset - lead as f64).max(0.0),
            None => (started_at - went_live_at - lead).max(0) as f64,
        });
        let playlist = match crate::recovery::build_playlist(
            &client,
            &playlist_url,
            max_conc,
            false,
            Some(head_secs + 4.0),
            skip_secs,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(rec_id, "head backfill: playlist build failed: {e:#}");
                finish(crate::events::TaskOutcome::Failed(format!("playlist: {e:#}")));
                return;
            }
        };
        let pl_path = cache.join(format!("{stem}.head.m3u8"));
        if let Err(e) = crate::iomon::fs::write(Cat::Recovery, &pl_path, &playlist.text).await {
            warn!(rec_id, "head backfill: cannot write playlist: {e:#}");
            finish(crate::events::TaskOutcome::Failed(format!("write playlist: {e}")));
            return;
        }
        let tmp_head = cache.join(format!("{stem}.head.mkv"));
        if let Err(e) = crate::recovery::mux_playlist_to_mkv(
            &pl_path,
            &tmp_head,
            Some((self.events.clone(), task_id)),
            Some(head_secs),
            "head backfill",
        )
        .await
        {
            warn!(rec_id, "head backfill: mux failed: {e:#}");
            finish(crate::events::TaskOutcome::Failed(format!("mux: {e:#}")));
            let _ = crate::iomon::fs::remove_file(Cat::Recovery, &tmp_head).await;
            let _ = crate::iomon::fs::remove_file(Cat::Recovery, &pl_path).await;
            return;
        }
        let _ = crate::iomon::fs::remove_file(Cat::Recovery, &pl_path).await;
        let muted_used = playlist.muted_used;
        match rename_or_shorten(&tmp_head, &out_dir, &stem, "head.mkv").await {
            Ok(dest) => {
                let _ = self
                    .store
                    .set_recording_backfill_path(rec_id, &dest.to_string_lossy());
                let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                info!(
                    rec_id,
                    missed,
                    muted_used,
                    "head backfill {}: {} ready",
                    Platform::Twitch.tag(),
                    dest.display()
                );
                let note = if muted_used > 0 {
                    format!("{missed}s backfilled ({muted_used} segments muted)")
                } else {
                    format!("{missed}s backfilled")
                };
                finish(crate::events::TaskOutcome::CompletedWithNote(note));

                // Integrity gate before ever touching older takes' files: the
                // fresh head must be fully clean (no segment had to fall back
                // to a silenced copy — see `RecoveredPlaylist::muted_used`)
                // and its duration must be plausible against the requested
                // `head_secs` (a truncated/corrupt mux would otherwise look
                // "successful" to `rename_or_shorten` alone). Only the
                // destructive "replace old heads" step is gated on this —
                // the fresh head itself is always kept regardless.
                let dur_ok = media_duration_secs(&dest).await.is_some_and(|d| {
                    (d as f64 - head_secs).abs() <= (5.0f64).max(head_secs * 0.02)
                });
                if muted_used == 0 && dur_ok {
                    if crate::head_backfill::effective_replace_old(&self.store, channel_id, monitor_id) {
                        self.supersede_old_heads(monitor_id, rec_id, &inputs.broadcast_id).await;
                    }
                } else {
                    info!(
                        rec_id, muted_used, dur_ok,
                        "head backfill: skipping supersede of older takes (integrity check failed)"
                    );
                }
            }
            Err(e) => {
                warn!(rec_id, "head backfill: promote failed: {e:#}");
                finish(crate::events::TaskOutcome::Failed(format!("promote: {e}")));
                return;
            }
        }
        // The stream may have ended (and finalized) while we were fetching —
        // this covers that ordering; the finalize-side call covers the other.
        self.maybe_concat_backfill(rec_id).await;
    }

    /// After a fresh, verified-good head backfill for `keep_rec_id`, delete
    /// older takes' now-redundant standalone head files for the same
    /// broadcast (they're a strict subset of the fresh one). Gives each old
    /// take one last, idempotent, safe-anytime chance to join its own
    /// head+live via `maybe_concat_backfill` before removing the file that
    /// join would have needed.
    async fn supersede_old_heads(&self, monitor_id: i64, keep_rec_id: i64, stream_id: &str) {
        let Ok(others) =
            self.store.recordings_with_backfill_for_stream(monitor_id, stream_id, keep_rec_id)
        else {
            return;
        };
        for (old_id, old_path) in others {
            // Last chance to bake the old head into that take's own full.mkv
            // before the head file disappears out from under it.
            self.maybe_concat_backfill(old_id).await;
            let path = PathBuf::from(&old_path);
            let removed = match crate::iomon::fs::remove_file(Cat::CacheSweep, &path).await {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => {
                    warn!(old_id, keep_rec_id, "head backfill: could not remove superseded head: {e:#}");
                    false
                }
            };
            if removed {
                let _ = self.store.clear_recording_backfill_path(old_id);
                let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: old_id });
                info!(old_id, keep_rec_id, "head backfill: superseded by a newer take's fresh head");
            }
        }
    }

    /// Manually (re)trigger a head backfill for an existing recording — the
    /// "🧩 Backfill head" context-menu action, forced regardless of the "fetch
    /// new head backfill on new take" setting (the UI only enables the button
    /// while the channel is live, so this is meant to run). Works for a still
    /// -recording take (finds its growing `.cache` file) or an already
    /// -finished one (uses the promoted final file — its own duration never
    /// changes again, so `missed` is computed against its fixed `ended_at`
    /// rather than a live `now_unix()`, see `head_backfill_job`).
    pub async fn manual_head_backfill(&self, rec_id: i64, quality: Option<String>) {
        let Ok(Some(rec)) = self.store.get_recording(rec_id) else {
            warn!(rec_id, "backfill head: recording not found");
            return;
        };
        let Ok(Some(mw)) = self.store.get_monitor_with_channel(rec.monitor_id) else {
            warn!(rec_id, "backfill head: owning monitor not found");
            return;
        };
        if mw.monitor.platform() != Platform::Twitch {
            warn!(rec_id, "backfill head: only supported for Twitch");
            return;
        }
        let (Some(stream_id), Some(went_live_at)) = (rec.stream_id.clone(), rec.went_live_at) else {
            warn!(rec_id, "backfill head: recording has no known stream id / go-live time");
            return;
        };
        let final_path = PathBuf::from(&rec.output_path);
        let still_recording = rec.status == "recording";
        let capture_path = if still_recording {
            let mut found = None;
            for c in live_capture_candidates(&final_path) {
                if crate::iomon::fs::metadata(Cat::CacheSweep, &c).await.is_ok() {
                    found = Some(c);
                    break;
                }
            }
            found.unwrap_or_else(|| final_path.clone())
        } else {
            final_path.clone()
        };
        let missed_reference =
            if still_recording { None } else { Some(rec.ended_at.unwrap_or(rec.started_at)) };
        let _ = self.store.set_head_backfill_state(rec_id, "queued");
        self.head_backfill_job(
            rec.monitor_id,
            mw.channel.id,
            rec_id,
            capture_path,
            final_path,
            mw.monitor.url.clone(),
            mw.channel.name.clone(),
            stream_id,
            went_live_at,
            rec.went_live_approx,
            rec.started_at,
            missed_reference,
            true, // force — user-initiated
            None, // manual re-trigger always backfills from the true go-live
            quality,
        )
        .await;
    }

    /// Re-fetch a mismatched head at the LIVE capture's own rendition (the
    /// Issues-panel fix for `head_backfill_state == "mismatch"`): probe the
    /// live file, derive its Twitch rendition name (`{height}p{fps}`), clear
    /// the mismatch state, and re-run the head backfill at that quality so
    /// the lossless concat can succeed. The superseded source-quality head is
    /// replaced by the job's normal supersede path.
    pub async fn refetch_head_matching_live(&self, rec_id: i64) {
        let Ok(Some(rec)) = self.store.get_recording(rec_id) else {
            warn!(rec_id, "match-live head re-fetch: recording not found");
            return;
        };
        let live = probe_media(&rec.output_path).await;
        let Some(live) = live else {
            warn!(rec_id, "match-live head re-fetch: live capture won't probe");
            let _ = self.events.send(AppEvent::BackgroundTaskFinished {
                id: crate::events::next_task_id(),
                outcome: crate::events::TaskOutcome::Failed(
                    "live capture won't probe — cannot derive its rendition".into(),
                ),
            });
            return;
        };
        // Twitch rendition names are `{height}p{fps}` with integer fps
        // (e.g. 720p60); fps strings like "59.94" round to the advertised 60.
        let fps: f64 = live.fps.parse().unwrap_or(0.0);
        let quality = format!("{}p{}", live.height, fps.round() as i64);
        info!(rec_id, quality, "re-fetching head at the live capture's rendition");
        let _ = self.store.set_head_backfill_state(rec_id, "");
        self.manual_head_backfill(rec_id, Some(quality)).await;
    }

    /// Merge a stranded split capture — the Issues fix for a take whose
    /// download tool died before running its own format merge: the predicted
    /// capture file never existed (finalize recorded 0 bytes, the take reads
    /// as "gone") but the media survived as bare per-format files in
    /// `.cache\` (see [`find_split_media`]). Losslessly muxes the parts into
    /// the final MKV, promotes it + any companions, and marks the recording
    /// completed. The parts are deleted only on success.
    pub async fn merge_split_capture(&self, rec_id: i64) {
        let Ok(Some(rec)) = self.store.get_recording(rec_id) else {
            warn!(rec_id, "merge split capture: recording not found");
            return;
        };
        let capture = PathBuf::from(&rec.output_path);
        // Finished per-format files first; for a take whose tool died mid-write
        // fall back to the largest surviving `.part` per format — the media is
        // intact up to where the capture stopped (the very tail may be cut).
        let mut parts = find_split_media(&capture);
        if parts.is_empty() {
            parts = find_split_parts(&capture);
        }
        if parts.is_empty() {
            warn!(rec_id, "merge split capture: no split parts found");
            return;
        }
        // Keyed by the recording id (like re-remux/finalize tasks) so grid and
        // Issues rows can match the running merge to their recording.
        let task_id = rec_id as u64;
        let name = capture
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::Remux,
            label: name,
            detail: format!("merging {} split part(s)", parts.len()),
            started_at: now_unix(),
            progress: None,
            progress_info: None,
        }));
        let finish = |outcome: crate::events::TaskOutcome| {
            let _ = self.events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        };

        // A never-promoted take's output_path points into a working dir; the
        // merged file belongs at its promoted location (the same path with the
        // cache component removed — handles the per-dir AND central layouts).
        let cache = parts[0]
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let out_dir = strip_cache_component(&capture)
            .as_deref()
            .unwrap_or(&capture)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cache.clone());
        let Some(orig_stem) = capture.file_stem().map(|s| s.to_string_lossy().into_owned())
        else {
            finish(crate::events::TaskOutcome::Failed("bad capture path".into()));
            return;
        };
        let stem = unique_stem(&out_dir, &orig_stem, "mkv", None);
        let final_path = path_with_safe_stem(&out_dir.join(format!("{stem}.mkv")));
        let tmp = cache.join(format!("{orig_stem}.merged.tmp.mkv"));

        use tokio::io::{AsyncBufReadExt, BufReader};
        // One local full-file pass at a time (see io_gate) + readrate throttle.
        // The queue wait is reported as live progress so a queued merge is
        // visibly waiting (and on what) instead of looking stale.
        let gate = {
            let tx = self.events.clone();
            let label = crate::io_gate::gate_label("merge-split", &final_path);
            crate::io_gate::local_pass_with_progress(&label, &final_path, move |waited, holders, waiting| {
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id,
                    progress: None,
                    info: crate::io_gate::wait_info(waited, holders, waiting),
                });
            })
            .await
        };
        // Total duration for the progress fraction (the video part spans the
        // whole take; the audio part matches it).
        let total_us: Option<i64> = media_duration_secs(&parts[0]).await.map(|s| s * 1_000_000);
        let mut allow_readrate = true;
        let out: std::io::Result<(std::process::ExitStatus, String)> = loop {
            let readrate =
                if allow_readrate { crate::io_gate::readrate_for(&final_path) } else { None };
            let mut cmd = Command::new("ffmpeg");
            cmd.arg("-y");
            if let Some(rate) = readrate {
                cmd.arg("-readrate").arg(format!("{rate}"));
            }
            for p in &parts {
                cmd.arg("-i").arg(p);
            }
            for i in 0..parts.len() {
                cmd.arg("-map").arg(format!("{i}"));
            }
            cmd.arg("-c")
                .arg("copy")
                .arg("-progress")
                .arg("pipe:1")
                .arg("-nostats")
                .arg(&tmp)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            cmd.creation_flags(CREATE_NO_WINDOW);
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => break Err(e),
            };
            let _io_guard = crate::iomon::track_tool(
                child.id(),
                "ffmpeg",
                "merge split capture",
                &final_path,
            );
            let stdout = child.stdout.take().expect("stdout piped");
            let stderr = child.stderr.take().expect("stderr piped");
            let stderr_task = tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                let mut lines: Vec<String> = Vec::new();
                while let Ok(Some(line)) = reader.next_line().await {
                    lines.push(line);
                }
                lines
            });
            // Same pacing watchdog as remux_ts_to_mkv: `-readrate` paces
            // against the input's own timestamps, and live-DVR DASH parts
            // start at a large non-zero timestamp (hours into the broadcast),
            // which can make ffmpeg believe it's far ahead of schedule and
            // sleep — crawling at sub-realtime while holding the 1-permit
            // gate. Media position under 2× wall clock after 3 minutes ⇒
            // kill and retry this merge without the throttle.
            let pace_started = std::time::Instant::now();
            let mut pacing_broken = false;
            {
                let mut reader = BufReader::new(stdout).lines();
                let mut blk_speed = String::new();
                let mut blk_pos = String::new();
                let mut blk_us: Option<i64> = None;
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Some((k, v)) = line.split_once('=') {
                        let (k, v) = (k.trim(), v.trim());
                        match k {
                            "speed" => blk_speed = v.to_string(),
                            "out_time" => blk_pos = v.to_string(),
                            "out_time_ms" => blk_us = v.parse::<i64>().ok(),
                            "progress" => {
                                let progress = blk_us.and_then(|us| {
                                    total_us
                                        .filter(|&t| t > 0)
                                        .map(|t| (us as f64 / t as f64).clamp(0.0, 1.0) as f32)
                                });
                                let pos_short = blk_pos.split('.').next().unwrap_or(&blk_pos);
                                let _ = self.events.send(AppEvent::BackgroundTaskProgress {
                                    id: task_id,
                                    progress,
                                    info: format!("speed={blk_speed} pos={pos_short}"),
                                });
                                if readrate.is_some() && !pacing_broken {
                                    let elapsed = pace_started.elapsed().as_secs_f64();
                                    let media_s = blk_us.unwrap_or(0) as f64 / 1e6;
                                    if elapsed > 180.0 && media_s < elapsed * 2.0 {
                                        pacing_broken = true;
                                        let _ = child.start_kill();
                                    }
                                }
                                blk_speed.clear();
                                blk_pos.clear();
                                blk_us = None;
                            }
                            _ => {}
                        }
                    }
                }
            }
            let status = match child.wait().await {
                Ok(s) => s,
                Err(e) => break Err(e),
            };
            let stderr_text = stderr_task.await.unwrap_or_default().join("\n");
            if pacing_broken {
                warn!(
                    rec_id,
                    "merge split capture: -readrate pacing collapsed (live DASH parts \
                     start at a non-zero timestamp); retrying without the throttle"
                );
                allow_readrate = false;
                continue;
            }
            if !status.success()
                && readrate.is_some()
                && crate::io_gate::is_readrate_error(&stderr_text)
            {
                crate::io_gate::mark_readrate_unsupported();
                continue; // retry once without the throttle
            }
            break Ok((status, stderr_text));
        };
        drop(gate);
        match out {
            Ok((status, _)) if status.success() => {}
            Ok((_, stderr_text)) => {
                let msg = stderr_text.trim().lines().last().unwrap_or("").to_string();
                let _ = crate::iomon::fs::remove_file(Cat::CacheSweep, &tmp).await;
                warn!(rec_id, "merge split capture failed: {msg}");
                finish(crate::events::TaskOutcome::Failed(format!("ffmpeg merge: {msg}")));
                return;
            }
            Err(e) => {
                finish(crate::events::TaskOutcome::Failed(format!("spawn ffmpeg: {e}")));
                return;
            }
        }
        if let Err(e) = crate::iomon::fs::rename(Cat::Promote, &tmp, &final_path).await {
            warn!(rec_id, "merge split capture: promote failed: {e:#}");
            finish(crate::events::TaskOutcome::Failed(format!("promote: {e}")));
            return;
        }
        // Bring any surviving companions (thumbnail, sidecars) up too.
        move_companions(&cache, &out_dir, &orig_stem).await;
        let bytes = crate::iomon::fs::metadata(Cat::Promote, &final_path)
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let _ = self.store.finish_recording(
            rec_id,
            rec.ended_at.unwrap_or_else(now_unix),
            bytes,
            rec.exit_code,
            "completed",
            &final_path.to_string_lossy(),
            &rec.log_excerpt,
        );
        for p in &parts {
            let _ = crate::iomon::fs::remove_file(Cat::CacheSweep, p).await;
        }
        let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
        info!(
            rec_id,
            bytes,
            "merged {} split part(s) -> {}",
            parts.len(),
            final_path.display()
        );
        finish(crate::events::TaskOutcome::Completed);
    }

    /// Join a backfilled head with the finished live capture into one seamless
    /// `{stem}.full.mkv` (lossless concat; both parts are KEPT). Idempotent and
    /// callable from every completion path — whichever of {backfill done, live
    /// finalize, startup healing} runs last performs the join.
    pub async fn maybe_concat_backfill(&self, rec_id: i64) {
        // One join per recording at a time — see `running_concats`.
        if !self.running_concats.lock().unwrap().insert(rec_id) {
            return;
        }
        let _guard = ConcatGuard { set: self.running_concats.clone(), id: rec_id };
        let Some((status, live_path, backfill, full)) =
            self.store.backfill_concat_info(rec_id).ok().flatten()
        else {
            return;
        };
        if full.is_some() || status == "recording" || status != "completed" {
            return;
        }
        let Some(head) = backfill else { return };
        // A previously-diagnosed parameter mismatch is permanent for these two
        // files — don't re-probe (and re-warn) on every completion-path
        // trigger. The Issues panel explains it and offers the fixes
        // (re-fetch the head at the live resolution / grab the published
        // VOD); a re-fetch clears the state.
        if self
            .store
            .get_recording(rec_id)
            .ok()
            .flatten()
            // "mismatch" (listed in Issues) or "mismatch_ack" (dismissed) —
            // both mean these exact files can't join; only a re-fetch resets.
            .is_some_and(|r| r.head_backfill_state.starts_with("mismatch"))
        {
            return;
        }
        let head_p = PathBuf::from(&head);
        let live_p = PathBuf::from(&live_path);
        if !live_path.to_ascii_lowercase().ends_with(".mkv")
            || !crate::iomon::fs::is_file_sync(Cat::Promote, &head_p)
            || !crate::iomon::fs::is_file_sync(Cat::Promote, &live_p)
        {
            return;
        }
        let (Some(out_dir), Some(stem)) = (
            live_p.parent().map(Path::to_path_buf),
            live_p.file_stem().map(|s| s.to_string_lossy().into_owned()),
        ) else {
            return;
        };

        // Same-encode guard: `-c copy` concat only splices cleanly when both
        // parts carry identical codec parameters (they do when the capture ran
        // at source quality — same Twitch encode). Mismatch → keep the parts.
        let (head_info, live_info) = (probe_media(&head).await, probe_media(&live_path).await);
        let compatible = match (&head_info, &live_info) {
            (Some(h), Some(l)) => {
                h.vcodec == l.vcodec
                    && h.width == l.width
                    && h.height == l.height
                    && h.fps == l.fps
                    && h.acodec == l.acodec
            }
            _ => false,
        };
        if !compatible {
            let fmt = |m: &Option<MediaInfo>| {
                m.as_ref()
                    .map(|i| {
                        format!(
                            "{}x{}@{} {}/{}",
                            i.width, i.height, i.fps, i.vcodec, i.acodec
                        )
                    })
                    .unwrap_or_else(|| "unprobeable".into())
            };
            warn!(
                rec_id,
                head = fmt(&head_info),
                live = fmt(&live_info),
                "head concat: codec parameters differ between head and live capture — keeping parts unjoined (listed under Issues)"
            );
            // Persisted so the join isn't re-attempted every trigger and so
            // the Issues panel can list it with fixes.
            let _ = self.store.set_head_backfill_state(rec_id, "mismatch");
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
            return;
        }

        let task_id = crate::events::next_task_id();
        let channel = archive_channel_name(&self.store, rec_id).unwrap_or_default();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::HeadBackfill(rec_id),
            label: channel,
            detail: "joining head + live capture".into(),
            started_at: now_unix(),
            progress: None,
            progress_info: None,
        }));
        let finish = |outcome: crate::events::TaskOutcome| {
            let _ = self.events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        };

        let cache = cache_dir(&out_dir);
        let _ = crate::iomon::fs::create_dir_all(Cat::Promote, &cache).await;
        let tmp_full = cache.join(format!("{stem}.full.mkv"));
        if let Err(e) = concat_mkvs(&cache, &head_p, &live_p, &tmp_full).await {
            warn!(rec_id, "head concat failed: {e:#}");
            finish(crate::events::TaskOutcome::Failed(format!("{e:#}")));
            let _ = crate::iomon::fs::remove_file(Cat::Promote, &tmp_full).await;
            return;
        }
        // Duration sanity: a silently-broken concat (e.g. only one part copied)
        // is discarded rather than promoted. Head/live durations come from the
        // compatibility probes above — only the new full.mkv needs an ffprobe
        // (2 fewer multi-GB-file header reads on the busy recordings drive).
        let part_dur = |info: &Option<MediaInfo>, p: &Path| {
            let known = info.as_ref().and_then(|i| i.duration_secs);
            let p = p.to_path_buf();
            async move {
                match known {
                    Some(d) => d,
                    None => media_duration_secs(&p).await.unwrap_or(0),
                }
            }
        };
        let (head_d, live_d, full_d) = (
            part_dur(&head_info, &head_p).await,
            part_dur(&live_info, &live_p).await,
            media_duration_secs(&tmp_full).await.unwrap_or(0),
        );
        let expected = head_d + live_d;
        if expected > 0 && ((full_d - expected).abs() > 5 + expected / 50) {
            warn!(
                rec_id,
                full_d, expected, "head concat: joined duration implausible — discarding"
            );
            finish(crate::events::TaskOutcome::Failed(format!(
                "joined duration {full_d}s vs expected {expected}s"
            )));
            let _ = crate::iomon::fs::remove_file(Cat::Promote, &tmp_full).await;
            return;
        }
        // Idempotency: another completion path may have joined while we muxed.
        if matches!(
            self.store.backfill_concat_info(rec_id).ok().flatten(),
            Some((_, _, _, Some(_)))
        ) {
            let _ = crate::iomon::fs::remove_file(Cat::Promote, &tmp_full).await;
            finish(crate::events::TaskOutcome::Completed);
            return;
        }
        match rename_or_shorten(&tmp_full, &out_dir, &stem, "full.mkv").await {
            Ok(dest) => {
                let _ = self
                    .store
                    .set_recording_full_path(rec_id, &dest.to_string_lossy());
                let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                info!(
                    rec_id,
                    "head concat {}: full stream ready at {}",
                    Platform::Twitch.tag(),
                    dest.display()
                );
                finish(crate::events::TaskOutcome::CompletedWithNote(
                    "head + live joined (parts kept)".into(),
                ));
            }
            Err(e) => {
                warn!(rec_id, "head concat: promote failed: {e:#}");
                finish(crate::events::TaskOutcome::Failed(format!("promote: {e}")));
            }
        }
    }
}
