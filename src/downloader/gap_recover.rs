//! Lost-segment recovery: re-fetch a Twitch capture's sequence-gap ranges
//! from the VOD CDN (the same `crate::recovery` rails head backfill uses —
//! they work even for channels with VODs disabled, via the sha1 folder
//! probe). Runs IN-FLIGHT as soon as the growing VOD should cover a lost
//! range (earliest fetch = best protection against post-stream DMCA mutes),
//! again at finalize for anything left, and at startup for recordings that
//! still have pending ranges (the CDN folder outlives the stream ~60 days).
//!
//! Recovered ranges land as sibling patch files next to the recording
//! (`{stem}.recovered-1h44m24s+36s.mkv`) — v1 does not splice them into the
//! main MKV; the post-stream VOD download remains the seamless-file path.

use super::*;

/// Settings key: `"0"` disables lost-segment recovery (default on).
pub const K_GAP_RECOVER: &str = "gap_recover";

pub(super) fn gap_recover_enabled(store: &Store) -> bool {
    store.get_setting(K_GAP_RECOVER).ok().flatten().as_deref() != Some("0")
}

/// Per-range fetch attempts before a range is marked `failed` for good.
const GAP_MAX_ATTEMPTS: i64 = 5;
/// In-flight cooldown after the CDN playlist can't be resolved, so quiet-cycle
/// kicks don't HEAD-probe the candidate host list every minute.
const GAP_RESOLVE_COOLDOWN_SECS: u64 = 300;

/// `6284.0 s + 36 s` → `"1h44m44s+36s"` — the patch filename's range tag.
fn fmt_gap_tag(start_secs: f64, len_secs: f64) -> String {
    let s = start_secs.max(0.0) as i64;
    format!("{}h{:02}m{:02}s+{}s", s / 3600, (s % 3600) / 60, s % 60, len_secs.round() as i64)
}

impl Supervisor {
    /// Spawn the recovery job for `rec_id` unless one is already running,
    /// the feature is off, or there's nothing pending. `final_sweep` fetches
    /// every pending range regardless of live-edge coverage math (the stream
    /// is over — the VOD covers everything it will ever cover).
    pub(super) fn maybe_spawn_gap_recover(&self, rec_id: i64, final_sweep: bool) {
        if !gap_recover_enabled(&self.store) {
            return;
        }
        if self.store.gap_ranges_in_state(rec_id, "pending").map(|v| v.is_empty()).unwrap_or(true) {
            return;
        }
        if !self.gap_jobs.lock().unwrap().insert(rec_id) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.gap_recover_job(rec_id, final_sweep).await;
            this.gap_jobs.lock().unwrap().remove(&rec_id);
        });
    }

    /// Startup sweep: requeue fetches orphaned by a shutdown, then resume
    /// recovery for every recording that still has pending ranges.
    pub async fn sweep_pending_gap_recoveries(&self) {
        let _ = self.store.requeue_stale_gap_fetches();
        for rec_id in self.store.recordings_with_pending_gaps().unwrap_or_default() {
            self.maybe_spawn_gap_recover(rec_id, true);
        }
    }

    async fn gap_recover_job(&self, rec_id: i64, final_sweep: bool) {
        let Some(rec) = self.store.get_recording(rec_id).ok().flatten() else { return };
        let Some(row) = self.store.get_monitor_with_channel(rec.monitor_id).ok().flatten() else {
            return;
        };
        if row.monitor.platform() != Platform::Twitch {
            return;
        }
        let Some(login) = crate::detectors::twitch_login(&row.monitor.url) else { return };
        // The CDN folder derivation needs the numeric broadcast id + go-live
        // epoch (or a published vod_id for the GQL fast path).
        let Some(stream_id) = rec.stream_id.clone() else {
            info!(rec_id, "gap recovery: no stream id on the take — cannot locate the VOD");
            return;
        };
        let Some(went_live) = rec.went_live_at else { return };

        // Which pending ranges are fetchable right now: everything on a final
        // sweep, else only ranges the trailing VOD should already cover.
        let elapsed = now_unix() - went_live;
        let ranges: Vec<crate::store::GapRangeRow> = self
            .store
            .gap_ranges_in_state(rec_id, "pending")
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.attempts < GAP_MAX_ATTEMPTS)
            .filter(|r| final_sweep || (r.end_secs as i64) + 240 < elapsed)
            .collect();
        if ranges.is_empty() {
            return;
        }
        // Patch files must land in the REAL output dir, never `.sa-cache\` —
        // in-flight `output_path` still points inside the cache (promote
        // rewrites it at finalize), and the cache sweep would eat anything
        // left there.
        let anchor = PathBuf::from(&rec.output_path);
        let anchor = strip_cache_component(&anchor).unwrap_or(anchor);
        let Some(out_dir) = anchor.parent().map(Path::to_path_buf) else { return };
        let Some(stem) = anchor.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            return;
        };

        let total_secs: i64 =
            ranges.iter().map(|r| (r.end_secs - r.start_secs).round() as i64).sum();
        let task_id = crate::events::next_task_id();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::GapRecover(rec_id),
            label: row.channel.name.clone(),
            detail: format!(
                "{} lost range(s), ~{total_secs}s — re-fetching from the VOD CDN",
                ranges.len()
            ),
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
            start_epoch: went_live,
            went_live_approx: rec.went_live_approx,
            // The GQL fast path when the VOD checker already confirmed one.
            vod_id: rec.vod_id.clone(),
        };
        let mut found = None;
        for attempt in 0..3 {
            if attempt > 0 {
                for _ in 0..(60 * 4) {
                    if self.shutdown.load(Ordering::SeqCst) {
                        finish(crate::events::TaskOutcome::Failed("shutdown".into()));
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
            info!(rec_id, "gap recovery: VOD playlist not found on the CDN (yet?)");
            finish(crate::events::TaskOutcome::Failed("VOD playlist not found".into()));
            // Hold the job slot briefly so in-flight quiet-cycle kicks don't
            // re-probe the host list every minute.
            if !final_sweep {
                crate::app_core::sleep_cancellable(
                    Duration::from_secs(GAP_RESOLVE_COOLDOWN_SECS),
                    &self.shutdown,
                )
                .await;
            }
            return;
        };
        // Match the live capture's own rendition, not Twitch's default
        // `chunked`/source — a patch at a different resolution/codec than
        // the capture can never pass gap-splice's compatibility gate (see
        // `refetch_head_matching_live`, the exact same technique used to
        // fix a head/live mismatch). Falls back to source (`found.url`
        // unchanged) if the capture won't probe — recovery still proceeds
        // either way, just without the quality match.
        let playlist_url = match probe_media(&rec.output_path).await {
            Some(live) => {
                let fps: f64 = live.fps.parse().unwrap_or(0.0);
                let quality = format!("{}p{}", live.height, fps.round() as i64);
                crate::recovery::playlist_at_quality(&client, &found, &quality, max_conc).await
            }
            None => found.url.clone(),
        };

        // In-flight the output dir may not exist yet (the capture lives in
        // `.sa-cache\` until promote) — create it for the patch files.
        let _ = crate::iomon::fs::create_dir_all(Cat::Recovery, &out_dir).await;
        let cache = cache_dir(&out_dir);
        let _ = crate::iomon::fs::create_dir_all(Cat::Recovery, &cache).await;
        set_cache_hidden(&cache);
        let (mut ok, mut muted_total) = (0usize, 0usize);
        for (i, r) in ranges.iter().enumerate() {
            if self.shutdown.load(Ordering::SeqCst) {
                break; // 'fetching' rows are requeued by the startup sweep
            }
            let start = r.start_secs;
            let len = (r.end_secs - r.start_secs).max(TWITCH_SEG_SECS);
            let _ = self.store.set_gap_range_state(r.id, "fetching", "", 0);
            let _ = self.events.send(AppEvent::BackgroundTaskProgress {
                id: task_id,
                progress: Some(i as f32 / ranges.len() as f32),
                info: format!("range {}/{} — {}", i + 1, ranges.len(), fmt_gap_tag(start, len)),
            });
            let fail = |why: String| {
                let next = if r.attempts + 1 >= GAP_MAX_ATTEMPTS { "failed" } else { "pending" };
                warn!(rec_id, "gap recovery: range {} failed ({next}): {why}", fmt_gap_tag(start, len));
                let _ = self.store.set_gap_range_state(r.id, next, "", 0);
            };
            let playlist = match crate::recovery::build_playlist(
                &client,
                &playlist_url,
                max_conc,
                false,
                Some(len),
                Some(start),
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    fail(format!("playlist: {e:#}"));
                    continue;
                }
            };
            // A muted patch beats no patch — but say so in the filename.
            let tag = if playlist.muted_used > 0 {
                format!("{}-muted", fmt_gap_tag(start, len))
            } else {
                fmt_gap_tag(start, len)
            };
            let pl_path = cache.join(format!("{stem}.gap-{tag}.m3u8"));
            if let Err(e) = crate::iomon::fs::write(Cat::Recovery, &pl_path, &playlist.text).await {
                fail(format!("write playlist: {e}"));
                continue;
            }
            let tmp = cache.join(format!("{stem}.gap-{tag}.mkv"));
            let mux = crate::recovery::mux_playlist_to_mkv(
                &pl_path,
                &tmp,
                Some((self.events.clone(), task_id)),
                Some(len),
                "gap recovery",
                None,
            )
            .await;
            let _ = crate::iomon::fs::remove_file(Cat::Recovery, &pl_path).await;
            if let Err(e) = mux {
                let _ = crate::iomon::fs::remove_file(Cat::Recovery, &tmp).await;
                fail(format!("mux: {e:#}"));
                continue;
            }
            match rename_or_shorten(&tmp, &out_dir, &stem, &format!("recovered-{tag}.mkv")).await {
                Ok(dest) => {
                    ok += 1;
                    muted_total += playlist.muted_used;
                    info!(
                        rec_id,
                        "gap recovery: {} restored ({}s{}) -> {}",
                        tag,
                        len.round() as i64,
                        if playlist.muted_used > 0 {
                            format!(", {} muted segments", playlist.muted_used)
                        } else {
                            String::new()
                        },
                        dest.display()
                    );
                    let _ = self.store.set_gap_range_state(
                        r.id,
                        "done",
                        &dest.to_string_lossy(),
                        playlist.muted_used as i64,
                    );
                }
                Err(e) => {
                    let _ = crate::iomon::fs::remove_file(Cat::Recovery, &tmp).await;
                    fail(format!("promote: {e}"));
                }
            }

            // Reflect progress on the alert row + the takes table after every
            // range — a backlog job can run for a long time, and the Warnings
            // row / grid badge should tick up as ranges land, not at the end.
            if let Ok((total, done, muted)) = self.store.gap_range_progress(rec_id) {
                let _ = self.store.set_alert_recovery(rec_id, total, done, muted);
            }
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
        }
        let note = if muted_total > 0 {
            format!("{ok}/{} range(s) recovered ({muted_total} segments muted)", ranges.len())
        } else {
            format!("{ok}/{} range(s) recovered", ranges.len())
        };
        if ok == ranges.len() {
            finish(crate::events::TaskOutcome::CompletedWithNote(note));
        } else {
            finish(crate::events::TaskOutcome::Failed(note));
        }
        // Every range this job could act on just settled (done or given up
        // for good) — see if the take is now ready for a gapless splice.
        // Cheap no-op unless every precondition actually holds.
        self.maybe_spawn_gap_splice(rec_id);
    }
}
