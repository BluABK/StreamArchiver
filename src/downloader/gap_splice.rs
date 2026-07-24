//! Splice recovered gap patches into a finalized recording so the result is
//! gapless — the follow-up `gap_recover.rs` always deferred ("v1 does not
//! splice them into the main MKV"). Mirrors `backfill::maybe_concat_backfill`'s
//! exact safety shape for a different source (recovered lost-segment patches
//! instead of a pre-join head): build the result at a brand-new path, verify
//! it hard, promote, re-point `output_path` before any disposal, and dispose
//! the pre-splice original + consumed patches only if the (opt-in, defaults
//! to Keep) cleanup setting says to.
//!
//! Every individual safety gate below fails toward "leave the patches as
//! untouched sibling files, exactly like today" — never a guess. A gap is
//! often only seconds long, so unlike the head/live join (where a
//! multi-minute head has slack to absorb an imprecise splice point), there
//! is no room here to be "close enough": no trustworthy PTS anchor means no
//! splice, full stop.

use super::*;

/// Settings key: `"0"` disables gap-splice (default on).
pub const K_GAP_SPLICE: &str = "gap_splice";

pub(super) fn gap_splice_enabled(store: &Store) -> bool {
    store.get_setting(K_GAP_SPLICE).ok().flatten().as_deref() != Some("0")
}

/// How far a PTS-derived gap position may disagree with the wall-clock
/// estimate before it's rejected as untrustworthy. Tight — a gap has no
/// slack, unlike head-join's 60s tolerance for a multi-minute head.
const GAP_SPLICE_PTS_TOLERANCE_SECS: f64 = 5.0;
/// How far a fresh re-probe of `output_path`'s own start time may disagree
/// with the persisted `capture_start_pts` before the anchor is treated as
/// no-longer-describing-this-file (e.g. it was replaced by a head+live join
/// that re-pointed `output_path`, and now carries a different, `+genpts`
/// regenerated timeline).
const GAP_SPLICE_ANCHOR_REVALIDATE_TOLERANCE_SECS: f64 = 2.0;
/// Per-seam landed-position tolerance — `-c copy` seeking via `inpoint`/
/// `outpoint` is keyframe-bound, not frame-exact (see `concat_mkvs_n`).
const GAP_SPLICE_SEAM_TOLERANCE_SECS: f64 = 1.0;
/// Aggregate duration tolerance — tight (a wrong-by-10s cut is catastrophic
/// at gap timescales, unlike head-join's `5 + expected/50` formula).
const GAP_SPLICE_DURATION_TOLERANCE_SECS: f64 = 2.0;

/// Whether gap-splice may proceed for a recording in this state — every
/// condition must hold, or defer without touching anything. Pure so it's
/// directly unit-testable. `take_group_size` is the number of recording
/// rows sharing this take's `take_group` (`1` = solo, no split-part
/// ambiguity about which leg `capture_start_pts` actually anchors).
fn gap_splice_precondition_met(
    status: &str,
    head_backfill_state: &str,
    gap_splice_state: &str,
    take_group_size: i64,
) -> bool {
    status == "completed"
        && head_backfill_state != "queued"
        && gap_splice_state.is_empty()
        && take_group_size <= 1
}

/// `spliced_duration ≈ original_duration + Σ(patch durations)`, within
/// `tolerance`. Necessary but NOT sufficient on its own (a keyframe-snap
/// error on one seam can cancel one on another in the total) — kept as an
/// independent second signal alongside the per-seam landed-position check.
fn duration_within_tolerance(original: f64, patches_total: f64, spliced: f64, tolerance: f64) -> bool {
    (spliced - (original + patches_total)).abs() <= tolerance
}

/// Build the ordered ffconcat entry list for splicing `gaps` (already
/// sorted by `local_start` ascending, one entry per recovered/"done" range)
/// into `base` — alternating spans of `base` (trimmed around each gap) with
/// each gap's own patch file. `base_duration` (when known) skips emitting a
/// zero-or-negative trailing span when the last gap reaches the file's end.
fn build_gap_splice_entries<'a>(
    base: &'a Path,
    base_duration: Option<f64>,
    gaps: &'a [(f64, f64, &'a Path)],
) -> Vec<ConcatEntry<'a>> {
    let mut entries = Vec::new();
    let mut prev_end: Option<f64> = None;
    for &(start, end, patch) in gaps {
        let span_len = start - prev_end.unwrap_or(0.0);
        if prev_end.is_none() && start > 0.01 || prev_end.is_some_and(|_| span_len > 0.01) {
            entries.push(ConcatEntry::trimmed(base, prev_end, Some(start)));
        }
        entries.push(ConcatEntry::whole(patch));
        prev_end = Some(end);
    }
    let reached_end = match (prev_end, base_duration) {
        (Some(p), Some(d)) => p >= d - 0.01,
        _ => false,
    };
    if !reached_end {
        entries.push(ConcatEntry::trimmed(base, prev_end, None));
    }
    entries
}

impl Supervisor {
    /// Cheap, speculative entry point — re-checks every precondition (a few
    /// DB reads) and no-ops immediately unless all hold, so it's safe to
    /// call from multiple trigger sites without reasoning about which one
    /// "actually" fired: (a) `gap_recover_job` after a range settles, (b)
    /// `finalize_recording` right after `status` flips to `completed`
    /// (covers ranges that already went terminal while still recording —
    /// no later range-transition event would otherwise catch it), (c) the
    /// startup sweep for anything missed across a restart.
    pub(super) fn maybe_spawn_gap_splice(&self, rec_id: i64) {
        if !gap_splice_enabled(&self.store) {
            // Gap-splice itself never runs, so its own completion can never
            // trigger chapters below — but chapters doesn't need gap-splice
            // to run, only to be SETTLED one way or another (see
            // `chapters_job`'s own unsettled-ranges check), so it's safe to
            // poke it directly here.
            self.maybe_spawn_chapters(rec_id, Vec::new());
            return;
        }
        if !self.gap_splice_jobs.lock().unwrap().insert(rec_id) {
            return; // already in flight — that call's own completion will trigger chapters
        }
        let this = self.clone();
        tokio::spawn(async move {
            let gap_meta = this.gap_splice_job(rec_id).await;
            this.gap_splice_jobs.lock().unwrap().remove(&rec_id);
            // Sequenced strictly after the splice attempt (success, failure,
            // or "nothing to do") so chapter embedding never races
            // gap-splice's own file rewrite, and reuses its already-computed
            // gap positions instead of re-deriving them.
            this.maybe_spawn_chapters(rec_id, gap_meta);
        });
    }

    /// Startup sweep: anything left over after a restart interrupted a
    /// splice before it could run (`maybe_spawn_gap_splice` re-checks every
    /// precondition itself; this just supplies candidates).
    pub async fn sweep_pending_gap_splices(&self) {
        for rec_id in self.store.recordings_needing_gap_splice_check().unwrap_or_default() {
            self.maybe_spawn_gap_splice(rec_id);
        }
    }

    /// Returns the completed splice's per-gap `(local_start, local_end,
    /// orig_start, orig_end, muted_segs)` data on a successful `"done"`
    /// outcome (`orig_*` in the raw, broadcast-relative frame `gap_range`
    /// itself stores) — empty on every other exit path (deferred, blocked,
    /// or failed). The `maybe_spawn_gap_splice` wrapper feeds this straight
    /// into `maybe_spawn_chapters` so chapters' "recovered"/"muted" markers
    /// never have to re-derive gap-splice's own PTS-anchored positions, and
    /// so chapter embedding is always sequenced strictly after this job
    /// finishes (never racing its file rewrite).
    async fn gap_splice_job(&self, rec_id: i64) -> Vec<crate::chapters::SplicedGap> {
        let Some(rec) = self.store.get_recording(rec_id).ok().flatten() else { return Vec::new() };
        let take_group_size = self.store.recording_take_group_size(rec_id).unwrap_or(1);
        if !gap_splice_precondition_met(
            &rec.status,
            &rec.head_backfill_state,
            &rec.gap_splice_state,
            take_group_size,
        ) {
            return Vec::new();
        }

        let done = self.store.gap_ranges_in_state(rec_id, "done").unwrap_or_default();
        if done.is_empty() {
            return Vec::new(); // nothing recovered to splice
        }
        // Belt-and-suspenders: the startup sweep's SQL already filters this,
        // but in-flight trigger sites call this speculatively without that
        // pre-filter.
        let unsettled = self.store.gap_ranges_in_state(rec_id, "pending").map(|v| !v.is_empty()).unwrap_or(true)
            || self.store.gap_ranges_in_state(rec_id, "fetching").map(|v| !v.is_empty()).unwrap_or(true);
        if unsettled {
            return Vec::new();
        }

        let output = PathBuf::from(&rec.output_path);
        if !crate::iomon::fs::is_file_sync(Cat::Promote, &output) {
            return Vec::new();
        }

        // ---- PTS anchor: no trustworthy anchor means no splice, full stop ----
        let Some(capture_start_pts) = self.store.recording_capture_start_pts(rec_id).ok().flatten()
        else {
            let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
            return Vec::new();
        };
        // Re-verify the anchor still describes THIS file before trusting it
        // for anything — e.g. a head+live join could have re-pointed
        // `output_path` at a `+genpts`-regenerated file with a completely
        // different (or absent) broadcast-PTS timeline since the anchor was
        // captured.
        let Some(fresh_start) = media_start_time_secs(&output).await else {
            let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
            return Vec::new();
        };
        if (fresh_start - capture_start_pts).abs() > GAP_SPLICE_ANCHOR_REVALIDATE_TOLERANCE_SECS {
            let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
            return Vec::new();
        }

        let Some(row) = self.store.get_monitor_with_channel(rec.monitor_id).ok().flatten() else {
            return Vec::new();
        };
        if row.monitor.platform() != Platform::Twitch {
            let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
            return Vec::new();
        }
        let Some(login) = crate::detectors::twitch_login(&row.monitor.url) else {
            let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
            return Vec::new();
        };
        let (Some(stream_id), Some(went_live)) = (rec.stream_id.clone(), rec.went_live_at) else {
            let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
            return Vec::new();
        };

        let client = self.ctx.http_client();
        let hosts = crate::recovery::load_hosts(&self.store);
        let max_conc = crate::recovery::load_max_conc(&self.store);
        let inputs = crate::recovery::RecoveryInputs {
            login,
            broadcast_id: stream_id,
            start_epoch: went_live,
            went_live_approx: rec.went_live_approx,
            vod_id: rec.vod_id.clone(),
        };
        let Some(found) = crate::recovery::resolve_playlist(&client, &inputs, &hosts, max_conc).await
        else {
            // VOD gone from the CDN by now — the anchor can't be
            // cross-checked. Leave patches as siblings; nothing lost.
            let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
            return Vec::new();
        };

        let Some(out_dir) = output.parent().map(Path::to_path_buf) else { return Vec::new() };
        let cache = cache_dir(&out_dir);
        let _ = crate::iomon::fs::create_dir_all(Cat::Promote, &cache).await;
        set_cache_hidden(&cache);

        let mut sorted = done;
        sorted.sort_by(|a, b| {
            a.start_secs.partial_cmp(&b.start_secs).unwrap_or(std::cmp::Ordering::Equal)
        });

        let join_estimate = (rec.started_at - went_live).max(0) as f64;
        let mut local_gaps: Vec<(f64, f64, PathBuf)> = Vec::with_capacity(sorted.len());
        for g in &sorted {
            if g.out_path.is_empty()
                || !crate::iomon::fs::is_file_sync(Cat::Promote, Path::new(&g.out_path))
            {
                let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
                return Vec::new();
            }
            let (Some(seg_start_pts), Some(seg_end_pts)) = (
                crate::recovery::segment_start_secs_at(&client, &found.url, max_conc, &cache, g.start_secs).await,
                crate::recovery::segment_start_secs_at(&client, &found.url, max_conc, &cache, g.end_secs).await,
            ) else {
                let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
                return Vec::new();
            };
            let (Some(local_start), Some(local_end)) = (
                pts_gap_position(
                    capture_start_pts,
                    seg_start_pts,
                    g.start_secs - join_estimate,
                    GAP_SPLICE_PTS_TOLERANCE_SECS,
                ),
                pts_gap_position(
                    capture_start_pts,
                    seg_end_pts,
                    g.end_secs - join_estimate,
                    GAP_SPLICE_PTS_TOLERANCE_SECS,
                ),
            ) else {
                let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
                return Vec::new();
            };
            if local_end <= local_start {
                let _ = self.store.set_gap_splice_state(rec_id, "anchor_failed");
                return Vec::new();
            }
            local_gaps.push((local_start, local_end, PathBuf::from(&g.out_path)));
        }

        // ---- codec compatibility gate ----
        let Some(base_info) = probe_media(&rec.output_path).await else {
            let _ = self.store.set_gap_splice_state(rec_id, "mismatch");
            return Vec::new();
        };
        let mut patch_durations = Vec::with_capacity(local_gaps.len());
        for (_, _, patch) in &local_gaps {
            let patch_str = patch.to_string_lossy();
            let patch_info = probe_media(&patch_str).await;
            let compatible = match &patch_info {
                Some(p) => {
                    p.vcodec == base_info.vcodec
                        && p.width == base_info.width
                        && p.height == base_info.height
                        && p.fps == base_info.fps
                        && p.acodec == base_info.acodec
                }
                None => false,
            };
            if !compatible {
                warn!(
                    rec_id,
                    "gap splice: codec parameters differ between a recovered patch and the \
                     capture — leaving patches unspliced (listed under Issues)"
                );
                let _ = self.store.set_gap_splice_state(rec_id, "mismatch");
                let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                return Vec::new();
            }
            let dur = match patch_info.as_ref().and_then(|i| i.duration_secs) {
                Some(d) => d as f64,
                None => match media_duration_secs(patch).await {
                    Some(d) => d as f64,
                    None => {
                        let _ = self.store.set_gap_splice_state(rec_id, "verify_failed");
                        return Vec::new();
                    }
                },
            };
            patch_durations.push(dur);
        }

        let Some(stem) = output.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            return Vec::new();
        };

        let task_id = crate::events::next_task_id();
        let channel = archive_channel_name(&self.store, rec_id).unwrap_or_default();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::GapSplice(rec_id),
            label: channel,
            detail: format!("splicing {} recovered gap(s) into a gapless file", local_gaps.len()),
            started_at: now_unix(),
            progress: None,
            progress_info: None,
        }));
        let finish = |outcome: crate::events::TaskOutcome| {
            let _ = self.events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        };

        // ---- N-way concat ----
        let base_duration = base_info
            .duration_secs
            .map(|d| d as f64)
            .or(media_duration_secs(&output).await.map(|d| d as f64));
        let refs: Vec<(f64, f64, &Path)> =
            local_gaps.iter().map(|(s, e, p)| (*s, *e, p.as_path())).collect();
        let entries = build_gap_splice_entries(&output, base_duration, &refs);
        let tmp = cache.join(format!("{stem}.gapless.tmp.mkv"));
        if let Err(e) = concat_mkvs_n(&cache, &entries, &tmp).await {
            warn!(rec_id, "gap splice concat failed: {e:#}");
            let _ = self.store.set_gap_splice_state(rec_id, "verify_failed");
            finish(crate::events::TaskOutcome::Failed(format!("{e:#}")));
            let _ = crate::iomon::fs::remove_file(Cat::Promote, &tmp).await;
            return Vec::new();
        }

        // ---- verification: per-seam landed position, then aggregate duration ----
        let mut cumulative = 0.0f64;
        let mut prev_local_end = 0.0f64;
        let mut seams_ok = true;
        for (i, (local_start, local_end, _)) in local_gaps.iter().enumerate() {
            cumulative += local_start - prev_local_end;
            match packet_pts_near(&tmp, cumulative).await {
                Some(landed) if (landed - cumulative).abs() <= GAP_SPLICE_SEAM_TOLERANCE_SECS => {}
                _ => {
                    seams_ok = false;
                    break;
                }
            }
            cumulative += patch_durations[i];
            prev_local_end = *local_end;
        }
        let spliced_duration = media_duration_secs(&tmp).await.map(|d| d as f64);
        let patches_total: f64 = patch_durations.iter().sum();
        let duration_ok = match (base_duration, spliced_duration) {
            (Some(orig), Some(spliced)) => {
                duration_within_tolerance(orig, patches_total, spliced, GAP_SPLICE_DURATION_TOLERANCE_SECS)
            }
            _ => false,
        };
        if !seams_ok || !duration_ok {
            warn!(
                rec_id,
                seams_ok, duration_ok, "gap splice: post-splice verification failed — discarding"
            );
            let _ = self.store.set_gap_splice_state(rec_id, "verify_failed");
            finish(crate::events::TaskOutcome::Failed("verification failed".into()));
            let _ = crate::iomon::fs::remove_file(Cat::Promote, &tmp).await;
            return Vec::new();
        }

        // Idempotency: another trigger may have already spliced while we
        // worked (each holds the `gap_splice_jobs` slot, but a state change
        // could still have landed via a different path — re-check before
        // promoting).
        if !self
            .store
            .get_recording(rec_id)
            .ok()
            .flatten()
            .is_some_and(|r| r.gap_splice_state.is_empty())
        {
            let _ = crate::iomon::fs::remove_file(Cat::Promote, &tmp).await;
            finish(crate::events::TaskOutcome::Completed);
            return Vec::new();
        }

        match rename_or_shorten(&tmp, &out_dir, &stem, "gapless.mkv").await {
            Ok(dest) => {
                let dest_s = dest.to_string_lossy().into_owned();
                // Re-point BEFORE any disposal — never a window where
                // `output_path` names a deleted file.
                if let Err(e) = self.store.update_recording_output_path(rec_id, &dest_s) {
                    warn!(rec_id, "gap splice: could not re-point output_path: {e:#}");
                    let _ = self.store.set_gap_splice_state(rec_id, "verify_failed");
                    finish(crate::events::TaskOutcome::Failed(format!("{e:#}")));
                    return Vec::new();
                }
                let _ = self.store.set_gap_splice_state(rec_id, "done");
                // For chapters' "recovered"/"muted" markers — `local_gaps`
                // and `sorted` were built in lockstep (one entry each per
                // "done" gap range, same order), so zipping them is safe.
                let gap_meta: Vec<crate::chapters::SplicedGap> = sorted
                    .iter()
                    .zip(local_gaps.iter())
                    .map(|(g, (local_start, local_end, _))| crate::chapters::SplicedGap {
                        local_start: *local_start,
                        local_end: *local_end,
                        orig_start: g.start_secs,
                        orig_end: g.end_secs,
                        muted_segs: g.muted_segs,
                    })
                    .collect();
                let note = self.post_gap_splice_cleanup(rec_id, &output, &sorted, &dest).await;
                let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                info!(rec_id, "gap splice: gapless file ready at {} ({note})", dest.display());
                finish(crate::events::TaskOutcome::CompletedWithNote(format!(
                    "{} gap(s) spliced in ({note})",
                    local_gaps.len()
                )));
                gap_meta
            }
            Err(e) => {
                warn!(rec_id, "gap splice: promote failed: {e:#}");
                let _ = self.store.set_gap_splice_state(rec_id, "verify_failed");
                finish(crate::events::TaskOutcome::Failed(format!("promote: {e}")));
                Vec::new()
            }
        }
    }

    /// Execute the effective post-splice cleanup for a freshly-landed
    /// gapless file; returns a short outcome note. Failure never blocks the
    /// splice: a file whose disposal fails is simply kept, with its DB
    /// pointer (already re-pointed at the gapless result) intact.
    async fn post_gap_splice_cleanup(
        &self,
        rec_id: i64,
        pre_splice: &Path,
        patches: &[crate::store::GapRangeRow],
        gapless: &Path,
    ) -> String {
        let scope = self
            .store
            .get_recording(rec_id)
            .ok()
            .flatten()
            .and_then(|r| self.store.get_monitor_with_channel(r.monitor_id).ok().flatten())
            .map(|mw| (mw.channel.id, mw.monitor.id));
        let Some((channel_id, monitor_id)) = scope else {
            return "original + patches kept".into();
        };
        let cleanup = crate::disposal::effective_gap_splice_cleanup(&self.store, channel_id, monitor_id);
        if cleanup == crate::disposal::GapSpliceCleanup::Keep {
            return "original + patches kept".into();
        }
        let mut patches_disposed = 0usize;
        for p in patches {
            if p.out_path.is_empty() || Path::new(&p.out_path) == gapless {
                continue;
            }
            match crate::disposal::dispose_media(&self.store, channel_id, monitor_id, Path::new(&p.out_path))
                .await
            {
                Ok(_) => patches_disposed += 1,
                Err(e) => warn!(rec_id, "gap splice cleanup: patch disposal failed: {e:#} (kept)"),
            }
        }
        let patch_note = format!("{patches_disposed}/{} patch(es) disposed", patches.len());
        if cleanup != crate::disposal::GapSpliceCleanup::Both {
            return format!("{patch_note}, original kept");
        }
        match crate::disposal::dispose_media(&self.store, channel_id, monitor_id, pre_splice).await {
            Ok(d) => format!("{patch_note}, original {}", d.describe()),
            Err(e) => {
                warn!(rec_id, "gap splice cleanup: original disposal failed: {e:#} (kept)");
                format!("{patch_note}, original kept (disposal failed)")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precondition_requires_every_condition() {
        assert!(gap_splice_precondition_met("completed", "", "", 1));
        assert!(gap_splice_precondition_met("completed", "mismatch", "", 1)); // settled head-join state is fine
        assert!(!gap_splice_precondition_met("recording", "", "", 1), "still recording");
        assert!(!gap_splice_precondition_met("completed", "queued", "", 1), "head-join still pending");
        assert!(!gap_splice_precondition_met("completed", "", "done", 1), "already spliced — terminal");
        assert!(!gap_splice_precondition_met("completed", "", "mismatch", 1), "already blocked — terminal");
        assert!(!gap_splice_precondition_met("completed", "", "", 2), "multi-leg capture");
    }

    #[test]
    fn duration_check_accepts_within_tolerance_rejects_outside() {
        assert!(duration_within_tolerance(100.0, 5.0, 105.0, 2.0));
        assert!(duration_within_tolerance(100.0, 5.0, 106.5, 2.0));
        assert!(!duration_within_tolerance(100.0, 5.0, 110.0, 2.0));
        assert!(!duration_within_tolerance(100.0, 5.0, 98.0, 2.0));
    }

    #[test]
    fn concat_entries_single_gap_mid_file() {
        let base = Path::new("output.mkv");
        let patch = Path::new("recovered-1.mkv");
        let gaps = [(100.0, 102.0, patch)];
        let entries = build_gap_splice_entries(base, Some(300.0), &gaps);
        assert_eq!(entries.len(), 3, "before-span, patch, after-span");
        assert_eq!(entries[0].path, base);
        assert_eq!(entries[0].inpoint, None);
        assert_eq!(entries[0].outpoint, Some(100.0));
        assert_eq!(entries[1].path, patch);
        assert_eq!(entries[1].inpoint, None);
        assert_eq!(entries[2].path, base);
        assert_eq!(entries[2].inpoint, Some(102.0));
        assert_eq!(entries[2].outpoint, None);
    }

    #[test]
    fn concat_entries_gap_at_the_very_start_skips_empty_leading_span() {
        let base = Path::new("output.mkv");
        let patch = Path::new("recovered-1.mkv");
        let gaps = [(0.0, 2.0, patch)];
        let entries = build_gap_splice_entries(base, Some(300.0), &gaps);
        assert_eq!(entries.len(), 2, "no leading span, just patch + trailing base");
        assert_eq!(entries[0].path, patch);
        assert_eq!(entries[1].inpoint, Some(2.0));
    }

    #[test]
    fn concat_entries_gap_reaching_the_end_skips_empty_trailing_span() {
        let base = Path::new("output.mkv");
        let patch = Path::new("recovered-1.mkv");
        let gaps = [(298.0, 300.0, patch)];
        let entries = build_gap_splice_entries(base, Some(300.0), &gaps);
        assert_eq!(entries.len(), 2, "leading base span + patch, no trailing span");
        assert_eq!(entries[0].outpoint, Some(298.0));
        assert_eq!(entries[1].path, patch);
    }

    #[test]
    fn concat_entries_multiple_gaps_in_order() {
        let base = Path::new("output.mkv");
        let p1 = Path::new("recovered-1.mkv");
        let p2 = Path::new("recovered-2.mkv");
        let gaps = [(50.0, 52.0, p1), (200.0, 203.0, p2)];
        let entries = build_gap_splice_entries(base, Some(300.0), &gaps);
        // base[0..50] p1 base[52..200] p2 base[203..end]
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].outpoint, Some(50.0));
        assert_eq!(entries[1].path, p1);
        assert_eq!(entries[2].inpoint, Some(52.0));
        assert_eq!(entries[2].outpoint, Some(200.0));
        assert_eq!(entries[3].path, p2);
        assert_eq!(entries[4].inpoint, Some(203.0));
        assert_eq!(entries[4].outpoint, None);
    }

    #[test]
    fn concat_entries_unknown_base_duration_always_emits_trailing_span() {
        let base = Path::new("output.mkv");
        let patch = Path::new("recovered-1.mkv");
        let gaps = [(100.0, 102.0, patch)];
        let entries = build_gap_splice_entries(base, None, &gaps);
        assert_eq!(entries.len(), 3, "duration unknown -> can't tell the trailing span is empty");
    }
}
