//! YouTube auto-heal: surgically re-fetch a broadcast's MISSING spans from
//! the published VOD — the YouTube counterpart of Twitch's lost-segment
//! recovery, riding the exact same `gap_range` rows, `.recovered-{tag}.mkv`
//! patch naming, and gap-splice handoff.
//!
//! The local capture stays the PRIMARY copy — VODs get trimmed, edited,
//! struck after the fact, or (DVR off / never published) don't exist at all,
//! and the capture may hold content the VOD no longer does. The VOD is only
//! a donor for spans the capture provably lacks:
//!
//! - the head, when a from-start capture couldn't rewind to the true start
//!   (🕘 DVR window exceeded) or the capture simply joined late;
//! - inter-take gaps (capture died — PO-token wave, crash, mid-stream
//!   platform suspension — and the retry joined later);
//! - the tail, when the capture never resumed but the broadcast went on
//!   (detected by the VOD running longer than our coverage).
//!
//! Uncovered spans are computed from the takes' actual media durations, not
//! their bookkeeping, and written as `gap_range` rows on the broadcast's
//! primary (oldest) take. Sections are fetched with yt-dlp
//! `--download-sections "*A-B"`, quality-matched to the capture so the
//! gap-splice compatibility gate has a chance; patches land beside the take
//! as `{stem}.recovered-{tag}.mkv` either way (playable standalone even when
//! a splice isn't possible).
//!
//! **Trimmed-VOD guard:** creators cut intros or edit published VODs, which
//! shifts every VOD timestamp against the broadcast clock. If the VOD's
//! duration falls short of our own coverage span by more than
//! [`TRIM_TOLERANCE_SECS`], auto-heal refuses to fetch (a 🚫-adjacent alert
//! says why) rather than splice the wrong footage into an archive.

use super::*;

/// Settings key: `"0"` disables YouTube auto-heal from the published VOD
/// (default on).
pub const K_YT_GAP_HEAL: &str = "yt_gap_heal";

pub(super) fn yt_heal_enabled(store: &Store) -> bool {
    store.get_setting(K_YT_GAP_HEAL).ok().flatten().map(|v| v != "0").unwrap_or(true)
}

/// Ignore uncovered spans shorter than this — section cuts are keyframe-
/// aligned and a couple of seconds of overlap always exists at take seams.
const MIN_GAP_SECS: f64 = 10.0;
/// Fetch padding on each side of a span, so keyframe snapping can't leave a
/// sliver uncovered and the splice has overlap to anchor on.
const FETCH_PAD_SECS: f64 = 2.0;
/// A published VOD shorter than our own coverage by more than this is
/// treated as trimmed/edited → auto-heal aborts for the broadcast.
const TRIM_TOLERANCE_SECS: f64 = 60.0;
/// A VOD longer than our coverage by more than this means the broadcast
/// continued after our last take died → heal the tail too.
const TAIL_TOLERANCE_SECS: f64 = 30.0;
/// VOD availability re-checks inside one job run (the platform can take a
/// while to publish/process after the stream ends).
const VOD_WAIT_TRIES: u32 = 6;
const VOD_WAIT_SECS: u64 = 600;

/// The complement of `coverages` (merged) within `[0, span_end]`, where
/// `span_end` is the later of our own coverage end and the VOD duration
/// (when known) — spans shorter than `min_gap` are dropped. Pure, tested.
pub(super) fn uncovered_ranges(
    coverages: &[(f64, f64)],
    vod_dur: Option<f64>,
    min_gap: f64,
) -> Vec<(f64, f64)> {
    let mut cov: Vec<(f64, f64)> =
        coverages.iter().copied().filter(|(s, e)| e > s).collect();
    cov.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (s, e) in cov {
        match merged.last_mut() {
            Some((_, le)) if s <= *le => *le = le.max(e),
            _ => merged.push((s, e)),
        }
    }
    let our_end = merged.last().map(|&(_, e)| e).unwrap_or(0.0);
    let span_end = match vod_dur {
        // Only extend past our own coverage when the VOD is meaningfully
        // longer (the tail case); tiny diffs are encoder/rounding noise.
        Some(v) if v > our_end + TAIL_TOLERANCE_SECS => v,
        _ => our_end,
    };
    let mut out = Vec::new();
    let mut cursor = 0.0f64;
    for &(s, e) in &merged {
        if s - cursor >= min_gap {
            out.push((cursor, s));
        }
        cursor = cursor.max(e);
    }
    if span_end - cursor >= min_gap {
        out.push((cursor, span_end));
    }
    out
}

impl Supervisor {
    /// Queue + run the auto-heal for the broadcast a finished YouTube take
    /// belongs to. Coverage math needs the takes' files settled, so this
    /// waits out a short settle first; the job itself then recomputes the
    /// uncovered set from scratch every run (like the Twitch scanner), so
    /// repeated calls per broadcast converge instead of duplicating.
    pub(super) fn maybe_queue_youtube_heal(&self, rec_id: i64) {
        if !yt_heal_enabled(&self.store) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            crate::app_core::sleep_cancellable(Duration::from_secs(120), &this.shutdown).await;
            if this.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let Some(primary) = this.youtube_heal_primary(rec_id) else { return };
            if !this.gap_jobs.lock().unwrap().insert(primary) {
                return; // a heal/recovery job for this take is already running
            }
            this.youtube_gap_heal_job(primary).await;
            this.gap_jobs.lock().unwrap().remove(&primary);
        });
    }

    /// The broadcast's PRIMARY take (oldest with a real file) — the take the
    /// `gap_range` rows and patch files attach to.
    fn youtube_heal_primary(&self, rec_id: i64) -> Option<i64> {
        let rec = self.store.get_recording(rec_id).ok().flatten()?;
        let sid = rec.stream_id.clone().filter(|s| !s.is_empty())?;
        let takes = self.store.takes_for_stream(rec.monitor_id, &sid).ok()?;
        takes
            .iter()
            .find(|t| t.bytes > 0 && !t.output_path.is_empty() && t.ended_at.is_some())
            .map(|t| t.id)
    }

    /// One auto-heal run for the broadcast `rec_id` (a primary take) belongs
    /// to: recompute uncovered spans from the takes on disk, wait for the
    /// published VOD, guard against a trimmed VOD, fetch each span as a
    /// quality-matched section, and hand the take to gap-splice.
    pub(super) async fn youtube_gap_heal_job(&self, rec_id: i64) {
        if !yt_heal_enabled(&self.store) {
            return;
        }
        let Some(rec) = self.store.get_recording(rec_id).ok().flatten() else { return };
        let Some(row) = self.store.get_monitor_with_channel(rec.monitor_id).ok().flatten() else {
            return;
        };
        let Some(sid) = rec.stream_id.clone().filter(|s| !s.is_empty()) else { return };
        let Some(went_live) = rec.went_live_at else { return };
        if rec.went_live_approx {
            // Section offsets are broadcast-relative; an approximate go-live
            // anchor could pull minutes of the WRONG footage into an archive.
            info!(rec_id, "yt heal: go-live time is approximate — refusing to anchor sections");
            return;
        }

        // Coverage from what's actually on disk, broadcast-relative.
        let takes = self.store.takes_for_stream(rec.monitor_id, &sid).unwrap_or_default();
        if takes.iter().any(|t| t.is_active()) {
            return; // broadcast still capturing — heal after it settles
        }
        let mut coverages: Vec<(f64, f64)> = Vec::new();
        for t in takes.iter().filter(|t| t.bytes > 0 && !t.output_path.is_empty()) {
            let Some(ended) = t.ended_at else { continue };
            let Some(dur) = media_duration_secs(Path::new(&t.output_path)).await else {
                continue;
            };
            let end_off = (ended - went_live) as f64;
            coverages.push(((end_off - dur as f64).max(0.0), end_off.max(0.0)));
        }
        if coverages.is_empty() {
            return;
        }
        // A quick pre-check without the VOD: nothing plausibly missing and
        // no tail question → don't even probe the platform.
        if uncovered_ranges(&coverages, None, MIN_GAP_SECS).is_empty()
            && (now_unix() - went_live) as f64
                - coverages.iter().fold(0.0f64, |m, &(_, e)| m.max(e))
                < TAIL_TOLERANCE_SECS
        {
            return;
        }

        // Wait for the published VOD to be up + processed.
        let url = crate::vod_archive::youtube_vod_url(&sid);
        let bins = load_ytdlp_bins(&self.store);
        let ytdlp_global = split_args(
            &self.store.get_setting("ytdlp_default_args").ok().flatten().unwrap_or_default(),
        );
        let mut vod_dur: Option<f64> = None;
        for attempt in 0..VOD_WAIT_TRIES {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            if attempt > 0 {
                crate::app_core::sleep_cancellable(
                    Duration::from_secs(VOD_WAIT_SECS),
                    &self.shutdown,
                )
                .await;
            }
            match youtube_vod_probe(&bins, &ytdlp_global, &url).await {
                Some((dur, false)) if dur > 0.0 => {
                    vod_dur = Some(dur);
                    break;
                }
                other => {
                    debug!(rec_id, ?other, "yt heal: VOD not ready yet");
                }
            }
        }
        let Some(vod_dur) = vod_dur else {
            info!(
                rec_id,
                "yt heal: published VOD not available/processed after waiting — \
                 pending ranges stay for the startup sweep"
            );
            // Persist what we know is missing so a later sweep resumes.
            let missing = uncovered_ranges(&coverages, None, MIN_GAP_SECS);
            if !missing.is_empty() {
                let _ = self.store.replace_pending_gap_ranges(rec_id, &missing);
            }
            return;
        };

        // Trimmed/edited VOD → every offset is a lie; refuse.
        let our_end = coverages.iter().fold(0.0f64, |m, &(_, e)| m.max(e));
        if vod_dur < our_end - TRIM_TOLERANCE_SECS {
            warn!(
                rec_id,
                vod_dur, our_end, "yt heal: published VOD is shorter than the capture \
                 coverage — trimmed/edited; refusing to heal from shifted timestamps"
            );
            let alert = crate::store::NewCaptureAlert {
                kind: "yt_heal_skipped".to_string(),
                severity: "warn".to_string(),
                source: "capture".to_string(),
                take_key: format!("yt_heal_trim:{sid}"),
                monitor_id: Some(rec.monitor_id),
                recording_id: Some(rec_id),
                video_id: None,
                channel: row.channel.name.clone(),
                count: 1,
                lost_segments: 0,
                last_line: format!(
                    "The published VOD runs {:.0}s but the local capture covers {:.0}s of \
                     broadcast — the VOD was trimmed or edited, so its timestamps no longer \
                     line up with the broadcast clock and auto-heal cannot safely cut \
                     sections from it. The local capture is untouched.",
                    vod_dur, our_end
                ),
            };
            let _ = self.store.upsert_capture_alert(&alert);
            return;
        }

        let desired = uncovered_ranges(&coverages, Some(vod_dur), MIN_GAP_SECS);
        if desired.is_empty() {
            return;
        }
        let _ = self.store.replace_pending_gap_ranges(rec_id, &desired);
        let ranges: Vec<crate::store::GapRangeRow> = self
            .store
            .gap_ranges_in_state(rec_id, "pending")
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.attempts < 5)
            .collect();
        if ranges.is_empty() {
            return;
        }

        // Patch files land beside the real output, never in `.sa-cache\`.
        let anchor = PathBuf::from(&rec.output_path);
        let anchor = strip_cache_component(&anchor).unwrap_or(anchor);
        let Some(out_dir) = anchor.parent().map(Path::to_path_buf) else { return };
        let Some(stem) = anchor.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            return;
        };
        let cache = cache_dir(&out_dir);
        let _ = crate::iomon::fs::create_dir_all(Cat::Recovery, &cache).await;
        set_cache_hidden(&cache);

        // Match the capture's rendition so the patch can pass gap-splice's
        // compatibility gate (same idea as Twitch's playlist_at_quality).
        let height_cap =
            probe_media(&rec.output_path).await.and_then(|m| m.height.parse::<i64>().ok());

        let total: i64 = ranges.iter().map(|r| (r.end_secs - r.start_secs) as i64).sum();
        let task_id = crate::events::next_task_id();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::GapRecover(rec_id),
            label: row.channel.name.clone(),
            detail: format!(
                "{} missing span(s), ~{total}s — healing from the published VOD",
                ranges.len()
            ),
            started_at: now_unix(),
            progress: Some(0.0),
            progress_info: None,
        }));

        let mut ok = 0usize;
        for (i, r) in ranges.iter().enumerate() {
            if self.shutdown.load(Ordering::SeqCst) {
                break; // 'fetching' rows are requeued by the startup sweep
            }
            let start = (r.start_secs - FETCH_PAD_SECS).max(0.0);
            let end = (r.end_secs + FETCH_PAD_SECS).min(vod_dur);
            let tag = super::gap_recover::fmt_gap_tag(start, end - start);
            let _ = self.store.set_gap_range_state(r.id, "fetching", "", 0);
            let _ = self.events.send(AppEvent::BackgroundTaskProgress {
                id: task_id,
                progress: Some(i as f32 / ranges.len() as f32),
                info: format!("span {}/{} — {tag}", i + 1, ranges.len()),
            });
            let tmp_tmpl = cache.join(format!("{stem}.gap-{tag}.%(ext)s"));
            let tmp = cache.join(format!("{stem}.gap-{tag}.mkv"));
            let fetched = fetch_vod_section(
                &bins, &ytdlp_global, &url, start, end, height_cap, &tmp_tmpl,
            )
            .await;
            let next = |a: i64| if a + 1 >= 5 { "failed" } else { "pending" };
            match fetched {
                Ok(()) if file_len(&tmp).await > 0 => {
                    match rename_or_shorten(&tmp, &out_dir, &stem, &format!("recovered-{tag}.mkv"))
                        .await
                    {
                        Ok(dest) => {
                            ok += 1;
                            info!(
                                rec_id,
                                "yt heal: span {tag} restored from the published VOD -> {}",
                                dest.display()
                            );
                            let _ = self.store.set_gap_range_state(
                                r.id,
                                "done",
                                &dest.to_string_lossy(),
                                0,
                            );
                        }
                        Err(e) => {
                            let _ = crate::iomon::fs::remove_file(Cat::Recovery, &tmp).await;
                            warn!(rec_id, "yt heal: promote failed for {tag}: {e:#}");
                            let _ =
                                self.store.set_gap_range_state(r.id, next(r.attempts), "", 0);
                        }
                    }
                }
                Ok(()) => {
                    warn!(rec_id, "yt heal: section {tag} produced no file");
                    let _ = self.store.set_gap_range_state(r.id, next(r.attempts), "", 0);
                }
                Err(e) => {
                    let _ = crate::iomon::fs::remove_file(Cat::Recovery, &tmp).await;
                    warn!(rec_id, "yt heal: section {tag} failed: {e:#}");
                    let _ = self.store.set_gap_range_state(r.id, next(r.attempts), "", 0);
                }
            }
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
        }
        let outcome = if ok == ranges.len() {
            crate::events::TaskOutcome::CompletedWithNote(format!(
                "{ok}/{} span(s) healed from the VOD",
                ranges.len()
            ))
        } else {
            crate::events::TaskOutcome::Failed(format!(
                "{ok}/{} span(s) healed from the VOD",
                ranges.len()
            ))
        };
        let _ = self.events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        // Patches are on disk — see if the take can now be spliced gapless.
        self.maybe_spawn_gap_splice(rec_id);
    }
}

/// Probe the published VOD: `(duration_secs, is_live)`. `None` when the page
/// doesn't resolve/parse (not published yet, still processing, private…).
async fn youtube_vod_probe(
    bins: &YtDlpBins,
    ytdlp_global: &[String],
    url: &str,
) -> Option<(f64, bool)> {
    let mut args: Vec<String> = vec![
        "-s".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--print".into(),
        "duration".into(),
        "--print".into(),
        "is_live".into(),
        "--extractor-args".into(),
        "youtube:player_client=mweb".into(),
    ];
    if !bins.sabr.pot_args.is_empty() {
        args.push("--extractor-args".into());
        args.push(bins.sabr.pot_args.clone());
    }
    args.extend_from_slice(ytdlp_global);
    args.push(url.to_string());
    let out = run_ytdlp(&bins.resolve_program(""), &args, Duration::from_secs(120)).await?;
    let mut lines = out.lines().map(str::trim);
    let dur: f64 = lines.next()?.parse().ok()?;
    let is_live = lines.next().map(|l| l.eq_ignore_ascii_case("true")).unwrap_or(false);
    Some((dur, is_live))
}

/// Download one `[start, end]` section of the published VOD to `out_tmpl`
/// (an `%(ext)s` template resolving to `.mkv` after remux), quality-matched
/// to the capture's height when known.
async fn fetch_vod_section(
    bins: &YtDlpBins,
    ytdlp_global: &[String],
    url: &str,
    start: f64,
    end: f64,
    height_cap: Option<i64>,
    out_tmpl: &Path,
) -> anyhow::Result<()> {
    let selector = match height_cap {
        Some(h) if h > 0 => format!("bv*[height<=?{h}]+ba/b[height<=?{h}]"),
        _ => "bv*+ba/b".to_string(),
    };
    let mut args: Vec<String> = vec![
        "--no-part".into(),
        "--no-playlist".into(),
        "--merge-output-format".into(),
        "mkv".into(),
        "--remux-video".into(),
        "mkv".into(),
        "--download-sections".into(),
        format!("*{start:.0}-{end:.0}"),
        "-f".into(),
        selector,
        "-o".into(),
        out_tmpl.to_string_lossy().into_owned(),
        "--extractor-args".into(),
        "youtube:player_client=mweb".into(),
    ];
    if !bins.sabr.pot_args.is_empty() {
        args.push("--extractor-args".into());
        args.push(bins.sabr.pot_args.clone());
    }
    args.extend_from_slice(ytdlp_global);
    args.push(url.to_string());
    let span = (end - start).max(1.0) as u64;
    // Generous ceiling: a section rarely takes longer than its own length
    // at CDN speeds, but leave room for slow days.
    let timeout = Duration::from_secs((span * 2).clamp(300, 3600));
    match run_ytdlp(&bins.resolve_program(""), &args, timeout).await {
        Some(_) => Ok(()),
        None => anyhow::bail!("yt-dlp section download failed or timed out"),
    }
}

/// Run yt-dlp, capture stdout; `None` on nonzero exit / spawn failure /
/// timeout.
async fn run_ytdlp(program: &str, args: &[String], timeout: Duration) -> Option<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(timeout, cmd.output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncovered_ranges_finds_head_middle_and_tail() {
        // One take covering [100, 3600] of a 7200s VOD: missing head,
        // missing tail.
        let got = uncovered_ranges(&[(100.0, 3600.0)], Some(7200.0), 10.0);
        assert_eq!(got, vec![(0.0, 100.0), (3600.0, 7200.0)]);

        // Two takes with a gap between them; VOD matches coverage end →
        // no tail.
        let got = uncovered_ranges(&[(0.0, 1000.0), (1300.0, 2000.0)], Some(2010.0), 10.0);
        assert_eq!(got, vec![(1000.0, 1300.0)]);

        // Overlapping takes (from-start rewind) merge — nothing missing.
        let got = uncovered_ranges(&[(0.0, 1500.0), (900.0, 2000.0)], Some(2000.0), 10.0);
        assert!(got.is_empty());

        // Sub-threshold slivers are ignored.
        let got = uncovered_ranges(&[(5.0, 1000.0)], Some(1004.0), 10.0);
        assert!(got.is_empty());

        // A VOD only marginally longer than coverage is rounding noise, not
        // a tail gap.
        let got = uncovered_ranges(&[(0.0, 1000.0)], Some(1020.0), 10.0);
        assert!(got.is_empty());

        // Unsorted input still works.
        let got = uncovered_ranges(&[(1300.0, 2000.0), (0.0, 1000.0)], None, 10.0);
        assert_eq!(got, vec![(1000.0, 1300.0)]);
    }
}
