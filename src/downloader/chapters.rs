//! Orchestration for embedding chapter markers into a finalized recording —
//! see `crate::chapters` for the settings/scope chain and every pure piece
//! (event coalescing, timeline rebasing, ffmetadata construction). This
//! module only wires those pieces to `Supervisor` state, the trigger call
//! sites, and the actual `embed_chapters_into_mkv` ffmpeg pass.
//!
//! Same "cheap, speculative, re-checks every precondition" shape as
//! `gap_splice`'s `maybe_spawn_gap_splice`: safe to call from multiple
//! trigger sites without reasoning about which one "actually" fired.

use super::*;
use crate::chapters::{self as ch, ChapterKinds, SplicedGap};

/// Whether chapter embedding may proceed for a recording in this state —
/// every condition must hold, or defer without touching anything. Pure so
/// it's directly unit-testable. `take_group_size` is the number of
/// recording rows sharing this take's `take_group` (`1` = solo) — multi-part
/// merged recordings are excluded for the same reason gap-splice excludes
/// them: `started_at`-relative event offsets aren't trustworthy across legs.
fn chapters_precondition_met(
    status: &str,
    head_backfill_state: &str,
    chapters_state: &str,
    take_group_size: i64,
) -> bool {
    status == "completed"
        && head_backfill_state != "queued"
        && chapters_state.is_empty()
        && take_group_size <= 1
}

impl Supervisor {
    /// Cheap, speculative entry point. `gap_meta` carries gap-splice's own
    /// already-computed `(local_start, local_end, orig_start, orig_end,
    /// muted_segs)` data when called right after a successful splice (see
    /// `gap_splice_job`) — every other trigger site passes an empty slice,
    /// which just means the "recovered"/"muted" chapter kinds produce
    /// nothing for this run (correct: for an un-spliced take there's no
    /// final-file position to derive their markers from).
    pub(super) fn maybe_spawn_chapters(&self, rec_id: i64, gap_meta: Vec<SplicedGap>) {
        if !self.chapter_jobs.lock().unwrap().insert(rec_id) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.chapters_job(rec_id, gap_meta).await;
            this.chapter_jobs.lock().unwrap().remove(&rec_id);
        });
    }

    /// Startup sweep: anything left over after a restart interrupted
    /// chapter embedding before it could run, PLUS — the first time this
    /// feature runs against an existing library — every pre-existing
    /// recording, since the new `chapters_state` column defaults to `''`
    /// for all of them at once. Unlike `sweep_pending_gap_splices` (whose
    /// candidate list is naturally tiny — it requires an actual `done` gap
    /// range, rare in practice), this sweep's candidate list can be the
    /// entire historical library, so it awaits each take's embed pass in
    /// turn instead of fanning out via `maybe_spawn_chapters` — the same
    /// "sequential, not fan-out" reasoning `cmd_reembed_chapters_all`
    /// already applies, to avoid flooding the shared disk-gate with a
    /// pile of concurrent full-file ffmpeg passes on top of live captures.
    pub async fn sweep_pending_chapters(&self) {
        for rec_id in self.store.recordings_needing_chapters_check().unwrap_or_default() {
            if self.chapter_jobs.lock().unwrap().insert(rec_id) {
                self.chapters_job(rec_id, Vec::new()).await;
                self.chapter_jobs.lock().unwrap().remove(&rec_id);
            }
        }
    }

    /// [`ManualCommand::RetriggerChapters`]: the "📑 Embed chapters"/"🔁
    /// Re-embed chapters" context-menu action. Resets `chapters_state` back
    /// to `""` first — `maybe_spawn_chapters`'s own precondition requires
    /// it, and this is exactly what makes the action double as a retry
    /// after `"failed"`/`"skipped"`, or a plain re-run after changing which
    /// chapter kinds are enabled.
    pub(super) fn cmd_retrigger_chapters(&self, rec_id: i64) {
        let _ = self.store.set_chapters_state(rec_id, "");
        self.maybe_spawn_chapters(rec_id, Vec::new());
    }

    /// [`ManualCommand::ReembedChaptersAll`]: re-embed chapters across every
    /// stable, single-part, eligible recording — regardless of its current
    /// `chapters_state`, unlike the startup sweep (which only picks up
    /// untouched `""` rows). Sequential, not fan-out: each take's job is
    /// awaited before starting the next, both so progress reporting is
    /// meaningful and so this doesn't flood the disk-gate with concurrent
    /// full-file ffmpeg passes.
    pub(super) fn cmd_reembed_chapters_all(&self) {
        let this = self.clone();
        tokio::spawn(async move { this.reembed_chapters_all_job().await });
    }

    async fn reembed_chapters_all_job(&self) {
        let task_id = crate::events::next_task_id();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::ReembedChaptersAll,
            label: "Re-embed chapters (all)".into(),
            detail: String::new(),
            started_at: now_unix(),
            progress: Some(0.0),
            progress_info: None,
        }));
        let recs = self.store.recordings_eligible_for_chapters_reembed().unwrap_or_default();
        let total = recs.len();
        let started = std::time::Instant::now();
        info!("chapters: re-embed-all starting — {total} eligible recording(s)");
        let mut done = 0usize;
        for rec_id in recs {
            // Respect an already-in-flight job for this rec_id (a manual
            // per-take retrigger, or another concurrent sweep) rather than
            // double-processing it.
            if self.chapter_jobs.lock().unwrap().insert(rec_id) {
                let _ = self.store.set_chapters_state(rec_id, "");
                self.chapters_job(rec_id, Vec::new()).await;
                self.chapter_jobs.lock().unwrap().remove(&rec_id);
            }
            done += 1;
            let _ = self.events.send(AppEvent::BackgroundTaskProgress {
                id: task_id,
                progress: Some(done as f32 / total.max(1) as f32),
                info: format!("{done}/{total}"),
            });
        }
        info!(
            "chapters: re-embed-all finished — {total} recording(s) checked in {:.1}s",
            started.elapsed().as_secs_f64(),
        );
        let _ = self.events.send(AppEvent::BackgroundTaskFinished {
            id: task_id,
            outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} recording(s) checked")),
        });
    }

    async fn chapters_job(&self, rec_id: i64, gap_meta: Vec<SplicedGap>) {
        let Some(rec) = self.store.get_recording(rec_id).ok().flatten() else { return };
        let take_group_size = self.store.recording_take_group_size(rec_id).unwrap_or(1);
        if !chapters_precondition_met(&rec.status, &rec.head_backfill_state, &rec.chapters_state, take_group_size) {
            return;
        }
        // Gap ranges might still be resolving even when `gap_meta` is empty
        // (e.g. this call came from `finalize_recording`, not a completed
        // gap-splice) — wait for them to settle before touching anything,
        // same "unsettled" check `gap_splice_job` runs on itself.
        let unsettled = self.store.gap_ranges_in_state(rec_id, "pending").map(|v| !v.is_empty()).unwrap_or(true)
            || self.store.gap_ranges_in_state(rec_id, "fetching").map(|v| !v.is_empty()).unwrap_or(true);
        if unsettled {
            return;
        }

        let Some(row) = self.store.get_monitor_with_channel(rec.monitor_id).ok().flatten() else {
            return;
        };
        if !ch::effective_chapters_enabled(&self.store, row.channel.id, row.monitor.id) {
            debug!(rec_id, channel = %row.channel.name, "chapters: disabled for this instance/channel — skipping");
            let _ = self.store.set_chapters_state(rec_id, "skipped");
            return;
        }

        let output = PathBuf::from(&rec.output_path);
        if !crate::iomon::fs::is_file_sync(Cat::Promote, &output) {
            debug!(rec_id, "chapters: output file not found on disk — deferring");
            return;
        }

        // No fresh gap-splice data (this run wasn't triggered right off a
        // successful splice — e.g. a restart sweep, a manual retry, or a
        // bulk re-embed) but a splice DID complete at some point: rebuild
        // the "recovered"/"muted" positions from what's still on disk,
        // rather than silently losing those two chapter kinds for every
        // run except the one immediately after gap-splice itself.
        let gap_meta = if gap_meta.is_empty() && rec.gap_splice_state == "done" {
            reconstruct_gap_meta(&self.store, rec_id, head_shift_for(&rec)).await.unwrap_or_default()
        } else {
            gap_meta
        };

        let kinds = ch::chapter_kinds(&self.store);
        let events = collect_chapter_events(&self.store, &rec, &kinds, &gap_meta);
        let chapters = ch::merge_close_events(events);
        if chapters.is_empty() {
            debug!(rec_id, "chapters: no chapter-worthy events found — skipping");
            let _ = self.store.set_chapters_state(rec_id, "skipped");
            return;
        }

        let task_id = crate::events::next_task_id();
        let channel = archive_channel_name(&self.store, rec_id).unwrap_or_default();
        info!(
            rec_id,
            channel = %channel,
            "chapters: starting embed — {} marker(s) into {}",
            chapters.len(),
            output.display(),
        );
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::Chapters(rec_id),
            label: channel,
            detail: format!("embedding {} chapter marker(s)", chapters.len()),
            started_at: now_unix(),
            progress: None,
            progress_info: None,
        }));
        let finish = |outcome: crate::events::TaskOutcome| {
            let _ = self.events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        };

        let started = std::time::Instant::now();
        let total_duration = media_duration_secs(&output).await.map(|d| d as f64);
        let ffmetadata = ch::build_ffmetadata(&chapters, total_duration);
        let progress_tx = Some((self.events.clone(), task_id));
        match embed_chapters_into_mkv(&output, &ffmetadata, total_duration, progress_tx).await {
            Ok(()) => {
                let _ = self.store.set_chapters_state(rec_id, "done");
                if let Ok(json) = serde_json::to_string(&chapters) {
                    let _ = self.store.set_chapters_json(rec_id, &json);
                }
                let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                info!(
                    rec_id,
                    "chapters: embedded {} marker(s) in {:.1}s",
                    chapters.len(),
                    started.elapsed().as_secs_f64(),
                );
                finish(crate::events::TaskOutcome::CompletedWithNote(format!(
                    "{} chapter(s) embedded",
                    chapters.len()
                )));
            }
            Err(e) => {
                warn!(
                    rec_id,
                    "chapters: embed failed after {:.1}s: {e:#}",
                    started.elapsed().as_secs_f64(),
                );
                let _ = self.store.set_chapters_state(rec_id, "failed");
                finish(crate::events::TaskOutcome::Failed(format!("{e:#}")));
            }
        }
    }
}

/// How much earlier the file's own t=0 sits relative to `Recording.started_at`
/// once head-backfill has prepended the missed intro (`0.0` when no head was
/// ever backfilled — the file's t=0 IS `started_at`). Shared by
/// `collect_chapter_events` (rebasing title/raid timestamps) and
/// `reconstruct_gap_meta` (rebuilding gap-derived positions after the fact) —
/// both need the exact same value.
fn head_shift_for(rec: &crate::models::Recording) -> f64 {
    let join_estimate = (rec.started_at - rec.went_live_at.unwrap_or(rec.started_at)).max(0) as f64;
    if rec.head_backfill_state == "done" { join_estimate } else { 0.0 }
}

/// Rebuild `"recovered"`/`"muted"` gap positions for a take whose gap-splice
/// already completed, from data that outlives the original splice attempt
/// (`gap_range` rows + each patch's ffprobed duration) — see
/// `crate::chapters::reconstruct_spliced_gaps` for the all-or-nothing
/// rationale. `None` when there's nothing to reconstruct (no `"done"` ranges)
/// or any patch is missing/disposed.
async fn reconstruct_gap_meta(store: &Store, rec_id: i64, head_shift: f64) -> Option<Vec<SplicedGap>> {
    let done = store.gap_ranges_in_state(rec_id, "done").unwrap_or_default();
    if done.is_empty() {
        return None;
    }
    let mut tuples = Vec::with_capacity(done.len());
    for g in &done {
        let path = Path::new(&g.out_path);
        if g.out_path.is_empty() || !crate::iomon::fs::is_file_sync(Cat::Promote, path) {
            return None; // a disposed/missing patch breaks the shift for every later gap too
        }
        let dur = media_duration_secs(path).await.map(|d| d as f64);
        tuples.push((g.start_secs, g.end_secs, g.muted_segs, dur));
    }
    ch::reconstruct_spliced_gaps(&tuples, head_shift)
}

/// Gather every enabled chapter-event kind for one take, in the
/// "seconds since `Recording.started_at`"/final-file-relative mix
/// `crate::chapters::merge_close_events` expects (title/category/raid go
/// through `rebase_to_final_secs`; gap-derived markers are already
/// final-file-relative, see `SplicedGap`'s doc comment).
fn collect_chapter_events(
    store: &Store,
    rec: &crate::models::Recording,
    kinds: &ChapterKinds,
    gap_meta: &[SplicedGap],
) -> Vec<(f64, String)> {
    // `gap_meta`'s `orig_start`/`orig_end` arrive in gap_splice's own raw,
    // broadcast-relative frame (relative to `went_live_at`) — every other
    // timestamp in this function (title/category `at_secs`, raid `at`) is
    // relative to `started_at` instead, so shift them into that same frame
    // once, up front, before handing anything to `rebase_to_final_secs`.
    // `local_start`/`local_end` need no such conversion (already
    // final-file-relative — see `SplicedGap`'s doc comment).
    let join_estimate = (rec.started_at - rec.went_live_at.unwrap_or(rec.started_at)).max(0) as f64;
    let head_shift = head_shift_for(rec);
    let rebase_gaps: Vec<SplicedGap> = gap_meta
        .iter()
        .map(|g| SplicedGap { orig_start: g.orig_start - join_estimate, orig_end: g.orig_end - join_estimate, ..*g })
        .collect();

    let mut events = Vec::new();

    if kinds.title || kinds.category {
        let changes = store.meta_changes_for_recording(rec.id).unwrap_or_default();
        let filtered: Vec<_> = changes
            .into_iter()
            .filter(|c| match c.kind.as_str() {
                "title" => kinds.title,
                "category" => kinds.category,
                _ => false,
            })
            .collect();
        for (at_secs, label) in ch::coalesce_meta_events(&filtered) {
            events.push((ch::rebase_to_final_secs(at_secs, head_shift, &rebase_gaps), label));
        }
    }

    if kinds.raid {
        let raids = store
            .stream_events_for_monitor_range(rec.monitor_id, rec.started_at, rec.ended_at.unwrap_or(rec.started_at))
            .unwrap_or_default();
        for (at_secs, label) in ch::raid_chapter_events(&raids, rec.started_at, kinds.raid_min_viewers) {
            events.push((ch::rebase_to_final_secs(at_secs, head_shift, &rebase_gaps), label));
        }
    }

    events.extend(ch::gap_marker_events(gap_meta, kinds.recovered_segments, kinds.muted_segments));

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precondition_requires_every_condition() {
        assert!(chapters_precondition_met("completed", "", "", 1));
        assert!(chapters_precondition_met("completed", "mismatch", "", 1), "unrelated head-join state name is fine");
        assert!(!chapters_precondition_met("recording", "", "", 1), "still recording");
        assert!(!chapters_precondition_met("completed", "queued", "", 1), "head-join still pending");
        assert!(!chapters_precondition_met("completed", "", "done", 1), "already embedded — terminal");
        assert!(!chapters_precondition_met("completed", "", "skipped", 1), "already decided to skip — terminal");
        assert!(!chapters_precondition_met("completed", "", "", 2), "multi-leg capture");
    }
}
