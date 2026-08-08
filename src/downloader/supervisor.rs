//! Supervisor lifecycle: the main loop, start/stop/holds, detection,
//! `record`, chat + video downloads, asset/scheduled loops.

use super::*;

/// Base hold-off before the next automatic attempt after YouTube rejected the
/// capture's GVS PO Token — the wait is this × the consecutive-failure count
/// (see `note_result`), capped at [`PO_TOKEN_COOLDOWN_MAX_SECS`].
///
/// The first observed episode cleared in ~7 minutes, which a flat 5-minute
/// cooldown fits. The night of 2026-07-31 didn't: two concurrent SABR
/// captures (Dokibird, Zentreya) had every token rejected for over three
/// hours straight — ~25 burned takes *each* under the generic ladder's
/// 10-minute cap, with the provider demonstrably healthy the whole time
/// (fresh distinct tokens per attempt; a healthy capture mints exactly one).
/// Escalating to a 15-minute cap roughly halves the take churn in a long
/// wave while still rejoining within 15 minutes of it lifting.
const PO_TOKEN_COOLDOWN_SECS: u64 = 300;
/// Ceiling for the escalating PO-token cooldown. Deliberately NOT higher:
/// these monitors capture at the live edge (`--no-live-from-start`), so every
/// extra minute of hold-off after the wave lifts is a permanent gap in the
/// recording — 15 minutes trades take spam against lost footage about evenly.
const PO_TOKEN_COOLDOWN_MAX_SECS: u64 = 900;

/// Retry cadence for a **subscriber-only** Twitch stream we aren't entitled to
/// (`UNAUTHORIZED_ENTITLEMENTS`; see [`crate::models::sub_only_rejected`]).
///
/// Every attempt dies in seconds, so the old ladder produced a take every ~5
/// minutes — 22 of them in one two-hour broadcast, each spawning streamlink,
/// filing a warning, AND kicking off a *full* CDN head backfill that
/// re-downloads the entire broadcast from its start (10.8 GB, and growing, per
/// cycle in the case that prompted this).
///
/// Ten minutes is the deliberate compromise: the CDN backfill is the only thing
/// capturing this stream, and it only ever reaches the *current take's start
/// time*, so this interval is exactly the worst-case gap at the end of the
/// broadcast — while halving the re-download churn of the old cadence.
pub const SUB_ONLY_COOLDOWN_SECS: u64 = 600;
/// Retry cadence for a gated broadcast with **no** fallback capture path
/// ([`Gated::NoFallback`]). Long on purpose: nothing is being archived either
/// way, so this only exists to notice the broadcast being opened up.
pub const GATED_NO_FALLBACK_COOLDOWN_SECS: u64 = 3600;

// The PO-token rejection predicate lives in `models::po_token_rejected` —
// the store's failed-take alert filing needs it too (a rejected take that
// had already captured bytes reaches the finalize catch-all, and must file
// as the same 🎫 kind as a zero-byte one).
pub(super) use crate::models::po_token_rejected;

/// The wait before the next automatic attempt after a capture that produced
/// nothing, given how many times in a row it has now failed. Three tiers:
///
/// - The generic ladder: 30s × fails, capped at 10 minutes.
/// - An instant death (a few seconds, e.g. "No video formats found" during a
///   pre-roll-ad window, or an unrecordable configuration) floors at 5
///   minutes — it must not re-spawn every ~30s for the whole stream.
/// - A GVS PO-token rejection escalates 5/10/15 minutes then stays at 15.
///   Nothing local can fix one — the provider is minting fresh, distinct
///   tokens and the platform refuses them (`sps:ATTESTATION_REQUIRED`) — and
///   the episodes range from ~7 minutes (2026-07-31 00:03) to 3+ hours
///   rejecting two concurrent captures' every attempt (same night, 02:20).
///   The ordinary ladder just burns takes against that wall.
/// Why a capture was refused entitlement, and therefore whether retrying it
/// achieves anything.
///
/// Both platforms refuse a broadcast we aren't entitled to, but the value of
/// asking again could not be more different, so one "gated" flag cannot set the
/// cadence for both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gated {
    /// Not gated — an ordinary failure, escalating backoff applies.
    No,
    /// Twitch subscriber-only: the live edge refuses us, but each new take
    /// kicks off a fresh CDN head backfill, and that is the ONLY thing
    /// archiving the broadcast. Retrying buys real footage.
    WithCdnFallback,
    /// YouTube members-only: nothing to fall back to. yt-dlp cannot even see
    /// the stream, so every retry is another 0-byte failure and another
    /// failed-take row. Ask rarely, purely in case the streamer opens it up.
    NoFallback,
}

pub(super) fn failure_backoff_secs(
    fails: u32,
    duration_secs: i64,
    po_token_rejected: bool,
    gated: Gated,
) -> u64 {
    let mut wait = (30u64 * fails as u64).min(600);
    const INSTANT_FAIL_SECS: i64 = 10;
    if duration_secs < INSTANT_FAIL_SECS {
        wait = wait.max(300);
    }
    if po_token_rejected {
        wait = wait
            .max((PO_TOKEN_COOLDOWN_SECS * fails as u64).min(PO_TOKEN_COOLDOWN_MAX_SECS));
    }
    // A subscriber-only stream is not a fault to escalate against: retrying the
    // live edge can only fail identically until the streamer opens it up. But
    // the retry is not pointless either — each new take triggers a fresh CDN
    // head backfill, which is the ONLY thing archiving this broadcast, and each
    // one extends coverage to that take's start. So this is a FLAT cadence
    // rather than an escalating cooldown: escalating would stretch the archive
    // gap wider and wider, and never retrying at all would freeze coverage
    // where it stands.
    match gated {
        Gated::No => {}
        Gated::WithCdnFallback => wait = SUB_ONLY_COOLDOWN_SECS,
        // No footage to gain, so the escalating ladder above is pointless
        // noise: it produced a doomed capture every few minutes for hours on a
        // members-only broadcast (Mori Calliope, 2026-08-08). One attempt an
        // hour is enough to notice the stream being opened up.
        Gated::NoFallback => wait = GATED_NO_FALLBACK_COOLDOWN_SECS,
    }
    wait
}

/// The live-state facts a start signal carries, snapshotted once per
/// [`Supervisor::try_begin`] so its several "live, but not captured here"
/// branches all write the same thing (see
/// [`Supervisor::mark_live_not_recording`]).
struct LiveMeta {
    went_live_at: Option<i64>,
    /// `went_live_at` is our own first-seen time, not the platform's.
    approximate: bool,
    stream_id: Option<String>,
    title: Option<String>,
    game: Option<String>,
    thumbnail_url: Option<String>,
    viewers: Option<i64>,
    tags: Option<String>,
}

impl Supervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        events: EventTx,
        active: ActiveSet,
        active_videos: ActiveSet,
        video_progress: VideoProgress,
        video_speed: VideoSpeed,
        active_chats: Arc<Mutex<HashMap<i64, u32>>>,
        shutdown: Arc<AtomicBool>,
        manual_tx: mpsc::UnboundedSender<ManualCommand>,
        ctx: Arc<DetectContext>,
        ad_active: AdActive,
        max_concurrent: usize,
        stop_holds: StopHolds,
        finalizing: Finalizing,
    ) -> Supervisor {
        // Restore the persisted holds into the shared map (the UI reads it).
        *stop_holds.lock().unwrap() = load_stop_holds(&store);
        Supervisor {
            store,
            events,
            active,
            active_secondary: Arc::new(Mutex::new(HashMap::new())),
            active_videos,
            video_progress,
            video_speed,
            stopping_videos: Arc::new(Mutex::new(HashSet::new())),
            stopping_monitors: Arc::new(Mutex::new(HashSet::new())),
            stall_killed: Arc::new(Mutex::new(HashSet::new())),
            stall_ended_at: Arc::new(Mutex::new(HashMap::new())),
            gap_jobs: Arc::new(Mutex::new(HashSet::new())),
            gap_splice_jobs: Arc::new(Mutex::new(HashSet::new())),
            head_backfill_aborts: Arc::new(Mutex::new(HashMap::new())),
            sub_only_sessions: Arc::new(Mutex::new(HashMap::new())),
            chapter_jobs: Arc::new(Mutex::new(HashSet::new())),
            blocked_notified: Arc::new(Mutex::new(HashMap::new())),
            active_chats,
            take_chat_done: Arc::new(Mutex::new(HashMap::new())),
            stopping_chats: Arc::new(Mutex::new(HashSet::new())),
            chat_only: Arc::new(Mutex::new(HashSet::new())),
            chat_only_user_stopped: Arc::new(Mutex::new(HashMap::new())),
            shutdown,
            manual_tx,
            ctx,
            ad_active,
            sem: Arc::new(Semaphore::new(max_concurrent.max(1))),
            backoff: Arc::new(Mutex::new(HashMap::new())),
            po_fallback_takes: Arc::new(Mutex::new(HashSet::new())),
            sabr_dvr_exceeded: Arc::new(Mutex::new(HashSet::new())),
            sabr_stall_count: Arc::new(Mutex::new(HashMap::new())),
            running_asset_fetches: Arc::new(Mutex::new(HashSet::new())),
            running_concats: Arc::new(Mutex::new(HashSet::new())),
            quality_upgraded: Arc::new(Mutex::new(HashSet::new())),
            stop_holds,
            finalizing,
            raid_follow_ad_hoc: Arc::new(Mutex::new(HashSet::new())),
            remux_jobs: Arc::new(Mutex::new(HashSet::new())),
            thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
            split_merge_jobs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Consume live signals (from detectors), offline pushes (EventSub
    /// `stream.offline`), and manual Start/Stop commands.
    pub async fn run(
        self,
        mut live_rx: mpsc::UnboundedReceiver<LiveSignal>,
        mut offline_rx: mpsc::UnboundedReceiver<crate::events::OfflineSignal>,
        mut manual_rx: mpsc::UnboundedReceiver<ManualCommand>,
    ) {
        loop {
            tokio::select! {
                Some(signal) = live_rx.recv() => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        continue; // draining: don't start new recordings
                    }
                    self.try_begin(signal.monitor_id, signal.went_live_at, signal.approximate, signal.stream_id, signal.thumbnail_url, signal.broadcaster_id, signal.stream_title, signal.stream_game, signal.stream_viewers, signal.stream_tags, false, false);
                }
                Some(monitor_id) = offline_rx.recv() => {
                    self.handle_offline_signal(monitor_id);
                }
                Some(cmd) = manual_rx.recv() => self.handle_manual_command(cmd),
                else => break,
            }
        }
    }

    /// Dispatch one manual command from the UI / scheduler (the body of
    /// [`Self::run`]'s manual-command select arm).
    fn handle_manual_command(&self, cmd: ManualCommand) {
        match cmd {
            ManualCommand::Start { id, user_initiated } => {
                let this = self.clone();
                tokio::spawn(async move { this.manual_start(id, user_initiated).await });
            }
            ManualCommand::Stop(id) => self.manual_stop(id),
            ManualCommand::StopHoldFor { monitor_id, hours, allow_triggers } => {
                self.manual_stop_hold(monitor_id, hours, allow_triggers)
            }
            ManualCommand::StartVideo(id) => {
                if !self.shutdown.load(Ordering::SeqCst) {
                    let this = self.clone();
                    tokio::spawn(async move { this.start_video(id).await });
                }
            }
            ManualCommand::StopVideo(id) => self.stop_video(id),
            ManualCommand::StopChat(id) => {
                // A user Stop on a chat-only session has to STAY stopped —
                // `try_begin` would otherwise restart it on the next poll.
                self.note_chat_only_user_stop(id);
                self.stop_chat_download(id);
            }
            ManualCommand::RefetchAssets(id) => {
                if let Ok(Some(row)) = self.store.get_monitor_with_channel(id) {
                    // Manual: bypass the 24h stamp + the fetch_chat_assets
                    // toggle, and resolve the platform id from the URL.
                    self.fetch_channel_assets(&row, None, true);
                }
            }
            ManualCommand::ReRemux { rec_id, capture, final_ } => {
                self.cmd_re_remux(rec_id, capture, final_)
            }
            ManualCommand::ReRemuxAll => self.cmd_re_remux_all(),
            ManualCommand::RecoverVod { inputs, quality, sink, probe_all } => {
                let store = self.store.clone();
                let tx = self.events.clone();
                let client = self.ctx.http_client();
                tokio::spawn(async move {
                    let task_id = crate::events::next_task_id();
                    crate::recovery::run_recovery(
                        client, store, tx, inputs, quality, sink, probe_all, task_id,
                    )
                    .await;
                });
            }
            ManualCommand::ScanRecoverableVods { window_days, quality } => {
                self.cmd_scan_recoverable_vods(window_days, quality)
            }
            ManualCommand::ArchiveVodNow(rec_id) => self.cmd_archive_vod_now(rec_id),
            ManualCommand::BackfillMissedVodNow(rec_id) => {
                attempt_missed_stream_backfill(
                    self.ctx.clone(),
                    self.store.clone(),
                    self.events.clone(),
                    self.manual_tx.clone(),
                    rec_id,
                );
            }
            ManualCommand::ScanForMissedStreams(monitor_id) => self.cmd_scan_for_missed_streams(monitor_id),
            ManualCommand::RescanScheduleEvent { segment_id, model, effort } => {
                self.cmd_rescan_schedule_event(segment_id, model, effort)
            }
            ManualCommand::PlayVodNow(rec_id) => self.cmd_play_vod_now(rec_id),
            ManualCommand::OpenVodWebpage(rec_id) => self.cmd_open_vod_webpage(rec_id),
            ManualCommand::BackfillHeadNow(rec_id) => {
                let this = self.clone();
                tokio::spawn(async move {
                    this.manual_head_backfill(rec_id, None).await;
                });
            }
            ManualCommand::BackfillHeadMatchLive(rec_id) => {
                let this = self.clone();
                tokio::spawn(async move {
                    this.refetch_head_matching_live(rec_id).await;
                });
            }
            ManualCommand::AbortHeadBackfill(rec_id) => {
                self.abort_head_backfill(rec_id);
            }
            ManualCommand::MergeSplitCapture(rec_id) => {
                let this = self.clone();
                tokio::spawn(async move {
                    this.merge_split_capture(rec_id).await;
                });
            }
            ManualCommand::RefreshCdnHosts => self.cmd_refresh_cdn_hosts(),
            ManualCommand::FinalizeRecording(rec_id) => {
                let this = self.clone();
                tokio::spawn(async move {
                    this.finalize_recording_now(rec_id).await;
                });
            }
            ManualCommand::RecoverStuckCapture { rec_id, capture, output_dir } => {
                self.cmd_recover_stuck_capture(rec_id, capture, output_dir)
            }
            ManualCommand::EmbedMissingThumbnails => self.cmd_embed_missing_thumbnails(),
            ManualCommand::FetchMissingThumbnails { embed } => {
                self.cmd_fetch_missing_thumbnails(embed)
            }
            ManualCommand::ReorganizeAll => self.cmd_reorganize_all(),
            ManualCommand::ReorganizeTake(rec_id) => self.cmd_reorganize_take(rec_id),
            ManualCommand::ReorganizeMonitor(mid) => self.cmd_reorganize_monitor(mid),
            ManualCommand::ReorganizeChannel(channel_id) => {
                self.cmd_reorganize_channel(channel_id)
            }
            ManualCommand::RerunJoinCleanup => self.cmd_rerun_join_cleanup(),
            ManualCommand::MigrateChatLogs => self.cmd_migrate_chat_logs(),
            ManualCommand::RenameRecording { rec_id, new_stem } => {
                self.cmd_rename_recording(rec_id, new_stem)
            }
            ManualCommand::RetriggerChapters(rec_id) => self.cmd_retrigger_chapters(rec_id),
            ManualCommand::ReembedChaptersAll => self.cmd_reembed_chapters_all(),
            ManualCommand::FetchMissingChatEmotes => self.cmd_fetch_missing_chat_emotes(),
        }
    }

    /// [`ManualCommand::ReRemux`]: re-remux one captured `.ts` to MKV in the
    /// background and update the recording row on success.
    fn cmd_re_remux(&self, rec_id: i64, capture: PathBuf, final_: PathBuf) {
        // Guards against this racing a concurrent `ReRemuxAll` pass (or a
        // second click) over the same rec_id — nothing did before.
        if !self.remux_jobs.lock().unwrap().insert(rec_id) {
            return;
        }
        let store = self.store.clone();
        let shutdown = self.shutdown.clone();
        let remux_jobs = self.remux_jobs.clone();
        let tx = self.events.clone();
        let task_id = rec_id as u64;
        let channel = archive_channel_name(&store, rec_id).unwrap_or_default();
        let dst_name = final_
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _ = tx.send(AppEvent::BackgroundTaskStarted(
            crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::Remux(rec_id),
                label: channel,
                detail: format!("→ {dst_name}"),
                started_at: now_unix(),
                progress: None,
progress_info: None,
            },
        ));
        let tx2 = tx.clone();
        tokio::spawn(async move {
            info!("re-remux start: {}", capture.display());
            // ffmpeg writes the destination directly, so shorten
            // proactively — this also covers a Re-remux retry after
            // the FIRST attempt failed because this exact name was
            // too long (see path_with_safe_stem).
            let final_ = path_with_safe_stem(&final_);
            // The user's embed settings apply to manual re-remuxes too (a
            // bare Default here silently skipped thumbnail/title/subs).
            let opts = remux_opts_for_recording(&store, rec_id);
            match remux_ts_to_mkv(&store, &shutdown, rec_id, &capture, &final_, Some((tx2, task_id)), &opts).await {
                Ok(()) => {
                    let _ = crate::iomon::fs::remove_file(Cat::CacheSweep, &capture).await;
                    let path_s = final_.to_string_lossy();
                    if let Err(e) = store.update_recording_output_path(rec_id, &path_s) {
                        warn!("re-remux: DB update failed for rec_id={rec_id}: {e:#}");
                    }
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Completed,
                    });
                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                    info!("re-remux done: {}", final_.display());
                }
                Err(e) => {
                    warn!("re-remux failed for rec_id={rec_id}: {e:#}");
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                }
            }
            remux_jobs.lock().unwrap().remove(&rec_id);
        });
    }

    /// [`ManualCommand::ReRemuxAll`]: re-remux every recording that still has
    /// a `.ts` source next to its planned MKV.
    fn cmd_re_remux_all(&self) {
        let store = self.store.clone();
        let shutdown = self.shutdown.clone();
        let remux_jobs = self.remux_jobs.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::ReRemuxAll,
                label: "Re-remux all".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let recs = match store.list_recordings_with_mkv() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                    return;
                }
            };
            let total = recs.len();
            let mut done = 0usize;
            for (rec_id, output_path) in &recs {
                let opts = remux_opts_for_recording(&store, *rec_id);
                let planned_mkv = PathBuf::from(output_path);
                // The sibling .ts (the actual source to remux) lives
                // under the ORIGINAL stem — only the destination we're
                // about to write gets proactively shortened.
                let ts = planned_mkv.with_extension("ts");
                if !crate::iomon::fs::exists_sync(Cat::FsProbe, &ts) {
                    done += 1;
                    continue;
                }
                let mkv = path_with_safe_stem(&planned_mkv);
                // Skip a rec_id a concurrent manual re-remux already owns —
                // that job's own completion is this file done either way.
                if !remux_jobs.lock().unwrap().insert(*rec_id) {
                    done += 1;
                    continue;
                }
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id,
                    progress: Some(done as f32 / total as f32),
                    info: format!("{}/{total}: {}", done + 1, mkv.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
                });
                match remux_ts_to_mkv(&store, &shutdown, *rec_id, &ts, &mkv, None, &opts).await {
                    Ok(()) => {
                        let _ = crate::iomon::fs::remove_file(Cat::CacheSweep, &ts).await;
                        if mkv != planned_mkv {
                            let _ = store.update_recording_output_path(*rec_id, &mkv.to_string_lossy());
                        }
                        let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                    }
                    Err(e) => warn!("re-remux-all failed for rec_id={rec_id}: {e:#}"),
                }
                remux_jobs.lock().unwrap().remove(rec_id);
                done += 1;
            }
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
            });
        });
    }

    /// [`ManualCommand::ScanRecoverableVods`]: sweep deleted/muted recordings
    /// within the CDN window and recover each (bounded concurrency).
    fn cmd_scan_recoverable_vods(&self, window_days: i64, quality: String) {
        let store = self.store.clone();
        let tx = self.events.clone();
        let client = self.ctx.http_client();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::RecoverVodScan,
                label: "VOD recovery scan".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let within = window_days.max(1) * 86_400;
            let takes = store
                .recordings_recoverable(within, now_unix())
                .unwrap_or_default();
            let total = takes.len();
            // Bound concurrent recoveries; each keeps its own inner
            // segment-HEAD semaphore, so total load stays sane.
            let sem = Arc::new(Semaphore::new(2));
            let mut set: JoinSet<()> = JoinSet::new();
            for take in takes {
                let Some(login) = crate::detectors::twitch_login(&take.monitor_url)
                else {
                    continue;
                };
                let (client, sem, store, tx, quality) = (
                    client.clone(),
                    sem.clone(),
                    store.clone(),
                    tx.clone(),
                    quality.clone(),
                );
                set.spawn(async move {
                    let _permit = sem.acquire().await.expect("semaphore");
                    // Re-check under the permit: the take may
                    // have queued for a long time and been
                    // recovered meanwhile by the auto-recovery
                    // hook (avoids a duplicate multi-GB pull).
                    let state = store
                        .recording_recovery_state(take.rec_id)
                        .ok()
                        .flatten();
                    if !matches!(state.as_deref(), None | Some("failed")) {
                        return;
                    }
                    let sub = crate::events::next_task_id();
                    let inputs = crate::recovery::RecoveryInputs {
                        login,
                        broadcast_id: take.stream_id,
                        start_epoch: take.start_epoch,
                        went_live_approx: take.went_live_approx,
                        vod_id: take.vod_id,
                    };
                    crate::recovery::run_recovery(
                        client,
                        store,
                        tx,
                        inputs,
                        quality,
                        crate::recovery::RecoverySink::Recording(take.rec_id),
                        take.deleted,
                        sub,
                    )
                    .await;
                });
            }
            while set.join_next().await.is_some() {}
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!(
                    "{total} recording(s) processed"
                )),
            });
        });
    }

    /// [`ManualCommand::ArchiveVodNow`]: resolve the published VOD URL for a
    /// recording and enqueue its download.
    fn cmd_archive_vod_now(&self, rec_id: i64) {
        let store = self.store.clone();
        let manual_tx = self.manual_tx.clone();
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            match resolve_vod_url(&ctx, &store, rec_id).await {
                Some((u, _platform)) => {
                    enqueue_vod_archive(&store, &manual_tx, rec_id, &u);
                }
                None => {
                    let _ = store.set_recording_vod_dl(rec_id, "failed", None);
                }
            }
        });
    }

    /// [`ManualCommand::PlayVodNow`]: resolve this take's VOD URL the same
    /// way `cmd_archive_vod_now`/`attempt_missed_stream_backfill` do, then
    /// open it in the configured media player. Works on a past broadcast
    /// regardless of whether it was ever captured, since nothing here reads
    /// `output_path`.
    fn cmd_play_vod_now(&self, rec_id: i64) {
        let store = self.store.clone();
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            let Ok(Some(rec)) = store.get_recording(rec_id) else { return };
            let Ok(Some(row)) = store.get_monitor_with_channel(rec.monitor_id) else { return };
            let player = store.get_setting(crate::ui::K_MEDIA_PLAYER).ok().flatten().unwrap_or_default();
            let player = player.trim();
            if player.is_empty() {
                tracing::warn!(rec_id, "play-vod: no media player configured");
                return;
            }
            let Some((url, _platform)) = resolve_vod_url(&ctx, &store, rec_id).await else {
                tracing::info!(rec_id, "play-vod: no VOD URL resolvable for this take");
                return;
            };
            let settings = crate::ui::SettingsForm::for_auto_play(&store);
            if let Some(msg) =
                crate::ui::player::spawn_play_vod(&row, &url, &rec.title, player, &settings, &store)
            {
                tracing::info!(rec_id, "play-vod: {msg}");
            }
        });
    }

    /// [`ManualCommand::OpenVodWebpage`]: resolve this take's VOD URL and
    /// open it in the OS default browser — unlike `Recording::vod_url()`
    /// (Twitch-only, needs an already-known `vod_id`), this re-resolves live
    /// so it also covers YouTube/Kick and a take whose VOD was never
    /// archived/downloaded.
    fn cmd_open_vod_webpage(&self, rec_id: i64) {
        let store = self.store.clone();
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            match resolve_vod_url(&ctx, &store, rec_id).await {
                Some((url, _platform)) => crate::platform::open_url(&url),
                None => tracing::info!(rec_id, "open-vod-webpage: no VOD URL resolvable for this take"),
            }
        });
    }

    /// [`ManualCommand::ScanForMissedStreams`]: one on-demand discovery pass
    /// for a single channel/instance (see `backfill_discover`), independent
    /// of the `K_AUTO_BACKFILL_MISSED` setting.
    fn cmd_scan_for_missed_streams(&self, monitor_id: i64) {
        let Ok(Some(row)) = self.store.get_monitor_with_channel(monitor_id) else {
            return;
        };
        let (ctx, store, events, manual_tx) =
            (self.ctx.clone(), self.store.clone(), self.events.clone(), self.manual_tx.clone());
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::BackfillDiscoverScan(monitor_id),
                label: format!("Scan for missed streams · {}", row.channel.name),
                detail: String::new(),
                started_at: now_unix(),
                progress: None,
                progress_info: None,
            }));
            let found = crate::downloader::backfill_discover::discover_missed_streams_for_monitor(
                ctx, store, events.clone(), manual_tx, &row,
            )
            .await;
            let _ = events.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!(
                    "{found} missed stream(s) found"
                )),
            });
        });
    }

    /// [`ManualCommand::RescanScheduleEvent`]: the event Properties window's
    /// "🔄 Rescan this event" action. Forces a fresh OCR pass over the
    /// segment's stored source image with a model/effort override, then
    /// applies the result through the same write path a normal periodic scan
    /// uses. Because `replace_schedule_source` deletes-and-reinserts a
    /// source's future segments (ids aren't stable across a refresh), this
    /// necessarily re-scans the segment's WHOLE source image, not just the
    /// one event — the event Properties window closes afterward since its
    /// `segment_id` no longer refers to a live row.
    fn cmd_rescan_schedule_event(&self, segment_id: i64, model: String, effort: String) {
        let (ctx, store, events) = (self.ctx.clone(), self.store.clone(), self.events.clone());
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::OcrRescan(segment_id),
                label: format!("Rescan schedule event · {model}"),
                detail: String::new(),
                started_at: now_unix(),
                progress: None,
                progress_info: None,
            }));

            let finish = |outcome: crate::events::TaskOutcome| {
                let _ = events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
            };

            let Ok(Some(seg)) = store.get_schedule_segment(segment_id) else {
                finish(crate::events::TaskOutcome::Failed("event no longer exists".into()));
                return;
            };
            if seg.ocr_image_path.is_empty() {
                finish(crate::events::TaskOutcome::Failed("not an OCR-scanned event".into()));
                return;
            }
            let Ok(Some(source)) = store.schedule_segment_source(segment_id) else {
                finish(crate::events::TaskOutcome::Failed("event no longer exists".into()));
                return;
            };
            let Ok(Some(row)) = store.get_monitor_with_channel(seg.monitor_id) else {
                finish(crate::events::TaskOutcome::Failed("owning channel no longer exists".into()));
                return;
            };

            let cfg = crate::schedule_source::load_channel_cfg(&store, row.channel.id);
            let mut opts = crate::schedule_ocr::ocr_opts_from_settings(&store, &cfg);
            opts.model = model;
            opts.effort = effort;

            let image_path = std::path::PathBuf::from(&seg.ocr_image_path);
            let result = crate::schedule_ocr::ocr_schedule_image(&image_path, &opts).await;
            crate::schedule_ocr::accumulate_ocr_stats(&store, &result);

            match result.segments {
                Some(segs) => {
                    let n = segs.len();
                    crate::detectors::replace_schedule_and_notify(&store, seg.monitor_id, &source, &segs);
                    let _ = store.clear_other_schedule_sources(seg.monitor_id, &source);
                    ctx.refresh_ocr_cache(seg.monitor_id, &source, &image_path, segs).await;
                    finish(crate::events::TaskOutcome::CompletedWithNote(format!(
                        "rescanned with {} — {n} event(s) updated",
                        result.accepted_model
                    )));
                }
                None => finish(crate::events::TaskOutcome::Failed(
                    "rescan found no schedule in the source image".into(),
                )),
            }
        });
    }

    /// [`ManualCommand::RefreshCdnHosts`]: harvest current Twitch CDN hosts
    /// from published VODs.
    fn cmd_refresh_cdn_hosts(&self) {
        let store = self.store.clone();
        let tx = self.events.clone();
        let client = self.ctx.http_client();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::RefreshCdnHosts,
                label: "Refresh CDN hosts".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let vod_ids = store.published_vod_ids(300).unwrap_or_default();
            let (learned, checked) =
                crate::recovery::harvest_hosts(&store, &client, &vod_ids).await;
            let total = crate::recovery::load_hosts(&store).len();
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!(
                    "{learned} new host(s) from {checked} VOD(s) · {total} known"
                )),
            });
        });
    }

    /// [`ManualCommand::RecoverStuckCapture`]: move a capture whose promote
    /// step failed out of `.cache\` to its output directory.
    fn cmd_recover_stuck_capture(&self, rec_id: i64, capture: PathBuf, output_dir: PathBuf) {
        let store = self.store.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let Some(stem) =
                capture.file_stem().map(|s| s.to_string_lossy().into_owned())
            else {
                warn!(rec_id, "recover stuck capture: no file stem for {}", capture.display());
                return;
            };
            let ext = capture
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, &output_dir).await;
            match rename_or_shorten(&capture, &output_dir, &stem, &ext).await {
                Ok(actual) => {
                    if let Err(e) = store
                        .update_recording_output_path(rec_id, &actual.to_string_lossy())
                    {
                        warn!(rec_id, "recover stuck capture: DB update failed: {e:#}");
                    }
                    info!(rec_id, "recovered stuck capture -> {}", actual.display());
                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                }
                Err(e) => warn!(rec_id, "recover stuck capture failed: {e:#}"),
            }
        });
    }

    /// [`ManualCommand::EmbedMissingThumbnails`]: embed the thumbnail sidecar
    /// into all MKVs that don't already carry one.
    fn cmd_embed_missing_thumbnails(&self) {
        let store = self.store.clone();
        let shutdown = self.shutdown.clone();
        let thumbnail_jobs = self.thumbnail_jobs.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::EmbedMissingThumbnails,
                label: "Embed missing thumbnails".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let recs = match store.list_recordings_with_mkv() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                    return;
                }
            };
            let total = recs.len();
            let mut embedded = 0usize;
            for (i, (rec_id, output_path)) in recs.iter().enumerate() {
                let mkv = PathBuf::from(output_path);
                if !crate::iomon::fs::exists_sync(Cat::Thumbnail, &mkv) { continue; }
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id,
                    progress: Some(i as f32 / total as f32),
                    info: format!("{}/{total}: {}", i + 1, mkv.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
                });
                let mkv2 = mkv.clone();
                let has = tokio::task::spawn_blocking(move || mkv_has_thumbnail(&mkv2)).await.unwrap_or(false);
                if has { continue; }
                if let Some(thumb) = find_thumbnail_for(&mkv) {
                    if !thumbnail_jobs.lock().unwrap().insert(*rec_id) {
                        continue; // a concurrent embed already owns this rec_id
                    }
                    match embed_thumbnail_into_mkv(&store, &shutdown, *rec_id, &mkv, &thumb).await {
                        Ok(()) => {
                            embedded += 1;
                            let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                        }
                        Err(e) => warn!("embed-thumbnail failed for rec_id={rec_id}: {e:#}"),
                    }
                    thumbnail_jobs.lock().unwrap().remove(rec_id);
                }
            }
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{embedded} embedded")),
            });
        });
    }

    /// [`ManualCommand::FetchMissingThumbnails`]: fetch (and optionally embed)
    /// thumbnails for recordings without a sidecar.
    fn cmd_fetch_missing_thumbnails(&self, embed: bool) {
        let store = self.store.clone();
        let shutdown = self.shutdown.clone();
        let thumbnail_jobs = self.thumbnail_jobs.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::FetchMissingThumbnails,
                label: "Fetch missing thumbnails".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let recs = match store.list_recordings_with_stream_id() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                    return;
                }
            };
            let total = recs.len();
            let mut fetched = 0usize;
            for (i, (rec_id, output_path, _stream_id)) in recs.iter().enumerate() {
                let output = PathBuf::from(output_path);
                if !crate::iomon::fs::exists_sync(Cat::Thumbnail, &output) { continue; }
                // Skip if a thumbnail sidecar already exists.
                if find_thumbnail_for(&output).is_some() { continue; }
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id,
                    progress: Some(i as f32 / total as f32),
                    info: format!("{}/{total}: {}", i + 1, output.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
                });
                // We don't have a standalone thumbnail-fetch API here;
                // log a note for now — actual YouTube thumbnail fetching
                // requires the YT API helpers which live in detectors.rs.
                info!("fetch-missing-thumbnails: rec_id={rec_id} has no thumbnail sidecar (manual implementation required per-platform)");
                if embed {
                    if let Some(thumb) = find_thumbnail_for(&output) {
                        if !thumbnail_jobs.lock().unwrap().insert(*rec_id) {
                            continue; // a concurrent embed already owns this rec_id
                        }
                        if let Err(e) = embed_thumbnail_into_mkv(&store, &shutdown, *rec_id, &output, &thumb).await {
                            warn!("embed after fetch failed rec_id={rec_id}: {e:#}");
                        } else {
                            fetched += 1;
                            let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                        }
                        thumbnail_jobs.lock().unwrap().remove(rec_id);
                    }
                }
            }
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{fetched} processed")),
            });
        });
    }

    /// [`ManualCommand::ReorganizeAll`]: apply the current subdir config to
    /// every recording, then sweep unlinked companion files.
    fn cmd_reorganize_all(&self) {
        let store = self.store.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::ReorganizeAll,
                label: "Re-organize all".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let cfg = store.subdir_config();
            let ids = match store.list_all_recording_ids() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                    return;
                }
            };
            let total = ids.len();
            for (i, rec_id) in ids.iter().enumerate() {
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id,
                    progress: Some(i as f32 / total.max(1) as f32),
                    info: format!("{}/{total}", i + 1),
                });
                let reverse = !cfg.enabled;
                match reorganize_recording_files(*rec_id, &store, &cfg, reverse).await {
                    Ok(Some(_)) => { let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id }); }
                    Ok(None) => {}
                    Err(e) => warn!("reorganize-all rec_id={rec_id}: {e:#}"),
                }
            }
            // Second pass: sweep every monitor output directory for companion
            // files that aren't linked to any recording (e.g. chat logs from
            // recordings that failed before an output_path was set).
            if cfg.enabled {
                if let Ok(dirs) = store.list_monitor_output_dirs() {
                    for dir in dirs {
                        sweep_companion_files(std::path::Path::new(&dir), &cfg, &store).await;
                    }
                }
            }
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
            });
        });
    }

    /// [`ManualCommand::MigrateChatLogs`]: one-shot sweep moving every existing
    /// chat sidecar into the dedicated chat root — the catch-up pass for files
    /// written before the root was configured (new takes write there directly).
    /// Cross-drive, so each file is copy → size-verify → delete, never a
    /// rename; `recording.chat_path` is re-pointed per moved file. Skips takes
    /// whose chat may still be written (open sessions), files already under
    /// the root, and anything whose copy can't be verified. Deliberately
    /// sequential — this is the I/O-relief feature; it shouldn't hammer the
    /// recording drives to prove it.
    fn cmd_migrate_chat_logs(&self) {
        let store = self.store.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::MigrateChatLogs,
                label: "Migrate chat logs".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let Some(root) = crate::chat::chat_root() else {
                let _ = tx.send(AppEvent::BackgroundTaskFinished {
                    id: task_id,
                    outcome: crate::events::TaskOutcome::CompletedWithNote(
                        "no chat folder configured".into(),
                    ),
                });
                return;
            };
            let (mut moved, mut skipped, mut failed) = (0usize, 0usize, 0usize);
            // Sidecars of still-open sessions: never move a file a logger may
            // hold open; also excluded from the unlinked pass 2 below.
            let mut active_files: std::collections::HashSet<PathBuf> =
                std::collections::HashSet::new();

            // Pass 1 — sidecars reachable from a recording row.
            let recs = store.list_recordings_for_chat_migration().unwrap_or_default();
            let total = recs.len();
            for (i, (rec_id, output_path, chat_path, open)) in recs.iter().enumerate() {
                if i % 50 == 0 {
                    let _ = tx.send(AppEvent::BackgroundTaskProgress {
                        id: task_id,
                        progress: Some(i as f32 / total.max(1) as f32),
                        info: format!("{}/{total}", i + 1),
                    });
                }
                let src = crate::chat::chat_file_candidates(chat_path, output_path)
                    .into_iter()
                    .find(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::ChatSidecar, p));
                let Some(src) = src else { continue };
                if *open {
                    active_files.insert(src);
                    skipped += 1;
                    continue;
                }
                if src.starts_with(&root) {
                    continue; // already under the chat root
                }
                let dst = crate::chat::chat_sidecar_path(&src);
                if dst == src {
                    continue;
                }
                match migrate_chat_file(&src, &dst).await {
                    Ok(true) => {
                        moved += 1;
                        let _ = store.set_recording_chat_path(*rec_id, &dst.to_string_lossy());
                    }
                    Ok(false) => skipped += 1, // target already exists
                    Err(e) => {
                        failed += 1;
                        warn!("chat migration: {} -> {}: {e:#}", src.display(), dst.display());
                    }
                }
            }

            // Pass 2 — unlinked chat-suffix files sitting in monitor output
            // dirs (and their configured chat/ subdirs): logs from takes that
            // never got an output_path, or pre-chat_path history.
            let cfg = store.subdir_config();
            for dir in store.list_monitor_output_dirs().unwrap_or_default() {
                let base = PathBuf::from(&dir);
                if base.starts_with(&root) {
                    continue;
                }
                for scan in [base.clone(), base.join(&cfg.chat)] {
                    let Ok(mut rd) =
                        crate::iomon::fs::read_dir(crate::iomon::Cat::ChatSidecar, &scan).await
                    else {
                        continue;
                    };
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if !is_chat_suffix(&name) {
                            continue;
                        }
                        let src = entry.path();
                        if active_files.contains(&src) {
                            continue; // still being written
                        }
                        let dst = crate::chat::chat_dir_for(&scan).join(&name);
                        if dst == src {
                            continue;
                        }
                        match migrate_chat_file(&src, &dst).await {
                            Ok(true) => {
                                moved += 1;
                                // No-op unless some take points at this file.
                                let _ = store.update_chat_path_by_path(
                                    &src.to_string_lossy(),
                                    &dst.to_string_lossy(),
                                );
                            }
                            Ok(false) => skipped += 1,
                            Err(e) => {
                                failed += 1;
                                warn!(
                                    "chat migration: {} -> {}: {e:#}",
                                    src.display(),
                                    dst.display()
                                );
                            }
                        }
                    }
                }
            }

            info!("chat migration done: {moved} moved, {skipped} skipped, {failed} failed");
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: if failed == 0 {
                    crate::events::TaskOutcome::CompletedWithNote(format!(
                        "{moved} moved, {skipped} skipped"
                    ))
                } else {
                    crate::events::TaskOutcome::Failed(format!(
                        "{moved} moved, {skipped} skipped, {failed} FAILED — see log"
                    ))
                },
            });
        });
    }

    /// [`ManualCommand::FetchMissingChatEmotes`]: one-shot sweep over every
    /// archived Twitch chat log (`.chat.jsonl`), fetching any first-party
    /// emote id that resolves nowhere — not the log's own channel, not any
    /// other monitored channel, not the global on-demand cache — straight
    /// from Twitch's CDN by id (`assets::twitch_emote_cdn_fetch`). The
    /// retroactive counterpart to `ui::chat::build_twitch_segments`'s
    /// per-popup on-demand fetch, for logs recorded before that existed or
    /// never opened since. Skips YouTube sidecars (`.live_chat.json` — no
    /// Twitch CDN concept to backfill) and takes still being written (never
    /// read a file a live logger may still hold open).
    ///
    /// One global stem index (every archived channel's own emotes + the
    /// on-demand cache) is built ONCE up front rather than resolving each
    /// log's own channel/account — a log's own channel is just one of the
    /// many directories that index already walks, so there's nothing extra
    /// a per-channel lookup would find. Scan pass is sequential (same I/O-
    /// relief reasoning as `cmd_migrate_chat_logs`); every miss across every
    /// log is collected and deduped BEFORE any network request, so a
    /// spammed emote across thousands of messages in many logs still costs
    /// exactly one fetch.
    fn cmd_fetch_missing_chat_emotes(&self) {
        let store = self.store.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::FetchMissingChatEmotes,
                label: "Fetch missing chat emotes".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));

            let fallback_index = std::sync::Arc::new(
                tokio::task::spawn_blocking(|| {
                    crate::assets::index_emote_stems(&crate::assets::all_twitch_emote_dirs())
                })
                .await
                .unwrap_or_default(),
            );
            let empty_map = std::sync::Arc::new(std::collections::HashMap::new());
            // This sweep only backfills missing emote images — it has no
            // recording/take context to resolve a "which channel" indicator
            // against, and doesn't need one (it never renders the log).
            let empty_partners = std::sync::Arc::new(std::collections::HashMap::new());
            // Likewise, badge icon resolution doesn't matter here (it never
            // renders the log either) — global-only dirs are enough to
            // satisfy the parser's signature.
            let no_channel_badges = std::sync::Arc::new(crate::ui::chat::TwitchBadgeDirs {
                channel: None,
                global: crate::ui::chat::twitch_global_badge_dir(),
            });

            let recs = store.list_recordings_for_chat_migration().unwrap_or_default();
            let candidates: Vec<PathBuf> = recs
                .iter()
                .filter(|(_, _, _, open)| !open)
                .filter_map(|(_, output_path, chat_path, _)| {
                    crate::chat::chat_file_candidates(chat_path, output_path)
                        .into_iter()
                        .find(|p| {
                            // Twitch only: `.chat.jsonl` carries the first-party
                            // `emotes` IRC tag this sweep backfills against.
                            // YouTube's `.live_chat.json` has no Twitch CDN
                            // concept.
                            p.extension().is_some_and(|e| e == "jsonl")
                                && crate::iomon::fs::exists_sync(crate::iomon::Cat::ChatSidecar, p)
                        })
                })
                .collect();
            let total = candidates.len();

            let mut all_fetches: Vec<crate::ui::chat::EmojiFetch> = Vec::new();
            for (i, path) in candidates.iter().enumerate() {
                if i % 20 == 0 {
                    let _ = tx.send(AppEvent::BackgroundTaskProgress {
                        id: task_id,
                        progress: Some(0.9 * i as f32 / total.max(1) as f32),
                        info: format!("scanning {}/{total}", i + 1),
                    });
                }
                if let Ok(chunk) = crate::ui::chat::parse_chunk_blocking(
                    path.clone(),
                    0,
                    None,
                    0,
                    empty_map.clone(),
                    None,
                    fallback_index.clone(),
                    true,
                    empty_partners.clone(),
                    no_channel_badges.clone(),
                )
                .await
                {
                    all_fetches.extend(chunk.fetches);
                }
            }

            all_fetches.sort_by(|a, b| a.dest.cmp(&b.dest));
            all_fetches.dedup();
            let queued = all_fetches.len();

            let _ = tx.send(AppEvent::BackgroundTaskProgress {
                id: task_id,
                progress: Some(0.95),
                info: format!("fetching {queued} emote(s)…"),
            });
            // Paced (unlike the interactive per-popup path — nobody's waiting
            // on this one, and a sweep across months of logs can turn up
            // hundreds of distinct ids): 150ms after each successful
            // download, matching every other bulk emote fetcher in
            // `assets.rs` (BTTV/FFZ/7TV/Twitch channel emotes).
            for batch in all_fetches.chunks(250) {
                crate::ui::chat::download_emoji_images(
                    batch,
                    Some(std::time::Duration::from_millis(150)),
                )
                .await;
            }
            let fetched = all_fetches
                .iter()
                .filter(|f| crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &f.dest))
                .count();

            info!(
                "chat emote backfill done: {total} log(s) scanned, {queued} missing, {fetched} fetched"
            );
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!(
                    "{total} chat log(s) scanned, {fetched}/{queued} emote(s) fetched"
                )),
            });
        });
    }

    /// [`ManualCommand::ReorganizeTake`]: re-organize one recording.
    fn cmd_reorganize_take(&self, rec_id: i64) {
        let store = self.store.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::ReorganizeTake(rec_id),
                label: format!("Re-organize recording #{rec_id}"),
                detail: String::new(),
                started_at: now_unix(),
                progress: None,
                progress_info: None,
            }));
            let cfg = store.subdir_config();
            let reverse = !cfg.enabled;
            match reorganize_recording_files(rec_id, &store, &cfg, reverse).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Completed,
                    });
                }
                Err(e) => {
                    warn!("reorganize take {rec_id}: {e:#}");
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                }
            }
        });
    }

    /// [`ManualCommand::ReorganizeMonitor`]: re-organize a monitor's recordings.
    fn cmd_reorganize_monitor(&self, mid: i64) {
        let store = self.store.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::ReorganizeMonitor(mid),
                label: format!("Re-organize monitor #{mid}"),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let cfg = store.subdir_config();
            let ids = match store.list_recording_ids_for_monitor(mid) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                    return;
                }
            };
            let total = ids.len();
            let reverse = !cfg.enabled;
            for (i, rec_id) in ids.iter().enumerate() {
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id, progress: Some(i as f32 / total.max(1) as f32), info: format!("{}/{total}", i+1),
                });
                match reorganize_recording_files(*rec_id, &store, &cfg, reverse).await {
                    Ok(Some(_)) => { let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id }); }
                    Err(e) => warn!("reorganize monitor {mid} rec_id={rec_id}: {e:#}"),
                    _ => {}
                }
            }
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
            });
        });
    }

    /// [`ManualCommand::ReorganizeChannel`]: re-organize a channel's recordings.
    fn cmd_reorganize_channel(&self, channel_id: i64) {
        let store = self.store.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::ReorganizeChannel(channel_id),
                label: format!("Re-organize channel #{channel_id}"),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let cfg = store.subdir_config();
            let ids = match store.list_recording_ids_for_channel(channel_id) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                    return;
                }
            };
            let total = ids.len();
            let reverse = !cfg.enabled;
            for (i, rec_id) in ids.iter().enumerate() {
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id, progress: Some(i as f32 / total.max(1) as f32), info: format!("{}/{total}", i+1),
                });
                match reorganize_recording_files(*rec_id, &store, &cfg, reverse).await {
                    Ok(Some(_)) => { let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id }); }
                    Err(e) => warn!("reorganize channel {channel_id} rec_id={rec_id}: {e:#}"),
                    _ => {}
                }
            }
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
            });
        });
    }

    /// [`ManualCommand::RerunJoinCleanup`]: catch-up pass for takes joined
    /// while `join_cleanup` was still "Keep".
    ///
    /// The setting is only ever consulted at the moment a join lands, so
    /// switching it later leaves every earlier take carrying its head + live
    /// capture next to a full that already contains both — permanently double
    /// the stream's size (199 GB on one drive when this was found, 2026-07-31).
    ///
    /// Deletes nothing on trust: each take is re-verified against the SAME
    /// duration gate the original join had to pass (`|full - (head + live)|`
    /// within tolerance) before its parts are disposed, because the parts are
    /// the only remaining evidence that the full is complete. A take that
    /// fails the gate, or whose full is missing, is skipped and counted.
    fn cmd_rerun_join_cleanup(&self) {
        let this = self.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            let task_id = crate::events::next_task_id();
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::RerunJoinCleanup,
                label: "Re-run join cleanup".into(),
                detail: String::new(),
                started_at: now_unix(),
                progress: Some(0.0),
                progress_info: None,
            }));
            let takes = match this.store.joined_takes_with_parts() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(AppEvent::BackgroundTaskFinished {
                        id: task_id,
                        outcome: crate::events::TaskOutcome::Failed(e.to_string()),
                    });
                    return;
                }
            };
            let total = takes.len();
            let (mut cleaned, mut skipped, mut unsafe_) = (0u32, 0u32, 0u32);
            for (i, (rec_id, full, live, head)) in takes.iter().enumerate() {
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id,
                    progress: Some(i as f32 / total.max(1) as f32),
                    info: format!("{}/{total}", i + 1),
                });
                let (full_p, live_p, head_p) =
                    (Path::new(full), Path::new(live), Path::new(head));
                if crate::iomon::fs::metadata(Cat::Promote, full_p).await.is_err() {
                    debug!(rec_id, "re-run join cleanup: full.mkv is gone — skipping");
                    skipped += 1;
                    continue;
                }
                // Nothing left to reclaim: the take already points at the full
                // and holds no head.
                let has_live = live != full
                    && crate::iomon::fs::metadata(Cat::Promote, live_p).await.is_ok();
                let has_head =
                    !head.is_empty() && crate::iomon::fs::metadata(Cat::Promote, head_p).await.is_ok();
                if !has_live && !has_head {
                    skipped += 1;
                    continue;
                }
                if !Self::join_still_verifies(*rec_id, full_p, live_p, head_p, has_live, has_head)
                    .await
                {
                    unsafe_ += 1;
                    continue;
                }
                let note = this.post_join_cleanup(*rec_id, head_p, live_p, full_p).await;
                info!(rec_id, "re-run join cleanup: {note}");
                let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                cleaned += 1;
            }
            let note = format!(
                "{cleaned} cleaned, {skipped} already done, {unsafe_} left alone (unverifiable)"
            );
            info!("re-run join cleanup finished: {note}");
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(note),
            });
        });
    }

    /// Re-check that `full` really does contain the parts still sitting next to
    /// it, using the same tolerance the original join applied
    /// (`head_backfill`'s duration sanity gate). `false` = don't touch the
    /// parts: either a probe failed or the durations don't add up, and in both
    /// cases keeping a redundant copy beats deleting the only good one.
    async fn join_still_verifies(
        rec_id: i64,
        full_p: &Path,
        live_p: &Path,
        head_p: &Path,
        has_live: bool,
        has_head: bool,
    ) -> bool {
        let Some(full_d) = media_duration_secs(full_p).await.filter(|d| *d > 0) else {
            warn!(rec_id, "re-run join cleanup: could not probe the full.mkv — parts kept");
            return false;
        };
        let mut expected = 0i64;
        for (p, present) in [(live_p, has_live), (head_p, has_head)] {
            if !present {
                continue;
            }
            let Some(d) = media_duration_secs(p).await.filter(|d| *d > 0) else {
                warn!(rec_id, "re-run join cleanup: could not probe {} — parts kept", p.display());
                return false;
            };
            expected += d;
        }
        // Only one part survives on some takes, so the full is legitimately
        // LONGER than what we can measure. Require it to be at least as long
        // as the parts (never shorter) — a truncated full is the failure this
        // gate exists to catch.
        let both = has_live && has_head;
        let ok = if both {
            (full_d - expected).abs() <= 5 + expected / 50
        } else {
            full_d + 5 >= expected
        };
        if !ok {
            warn!(
                rec_id,
                full_d,
                expected,
                "re-run join cleanup: the full.mkv doesn't account for its parts — keeping them"
            );
        }
        ok
    }

    /// [`ManualCommand::RenameRecording`]: rename a recording's files to a new stem.
    fn cmd_rename_recording(&self, rec_id: i64, new_stem: String) {
        let store = self.store.clone();
        let tx = self.events.clone();
        tokio::spawn(async move {
            match rename_recording_files(rec_id, &store, &new_stem).await {
                Ok(Some(_)) => {
                    let _ = tx.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("rename rec_id={rec_id}: {e:#}");
                    let _ = tx.send(AppEvent::Error {
                        context: format!("Rename recording #{rec_id}"),
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    /// Fetch a monitor's channel assets (icon/banner/badges/emotes) as a background
    /// task. `force` skips the 24h freshness stamp (manual refetch). `broadcaster_id`
    /// is the platform id when detection supplied it; otherwise it's resolved from
    /// the channel URL, so an offline channel still fetches.
    fn fetch_channel_assets(
        &self,
        row: &MonitorWithChannel,
        broadcaster_id: Option<String>,
        force: bool,
    ) {
        let platform = row.monitor.platform();
        // Per-platform, per-ACCOUNT asset dir: one container can hold the same
        // creator on Twitch + YouTube + Kick — and multiple accounts on ONE
        // platform (main + alt Twitch). Namespacing by platform + account slug
        // keeps them from overwriting each other (and the 24h freshness stamp
        // becomes per-(channel, platform, account) for free).
        let account = crate::assets::account_slug(&row.monitor.url, platform);
        let asset_dir = crate::assets::channel_asset_dir(&row.channel.name, platform, &account);
        if !force && !crate::assets::should_refetch_assets(&asset_dir) {
            return;
        }
        // Guard: skip if a fetch for this (channel, platform, account) is already
        // in flight. Two tools on the SAME URL share the key (one fetch); a
        // sibling account fetches independently.
        let fetch_key = (
            row.channel.name.clone(),
            platform.as_str().to_string(),
            account.clone(),
        );
        {
            let mut running = self.running_asset_fetches.lock().unwrap();
            if running.contains(&fetch_key) {
                return;
            }
            running.insert(fetch_key.clone());
        }
        let http = self.ctx.http_client();
        let ctx = self.ctx.clone();
        let store = self.store.clone();
        let tx = self.events.clone();
        let url = row.monitor.url.clone();
        let known_bid = broadcaster_id.unwrap_or_default();
        let monitor_id = row.monitor.id;
        let channel_id = row.channel.id;
        let running_asset_fetches = self.running_asset_fetches.clone();

        let task_id = crate::events::next_task_id();
        let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
            id: task_id,
            kind: crate::events::BackgroundTaskKind::AssetFetch,
            label: row.channel.name.clone(),
            detail: format!("{} ({account}) · icon, banner, badges, emotes, about", platform.label()),
            started_at: now_unix(),
            progress: None,
            progress_info: None,
        }));

        tokio::spawn(async move {
            use crate::events::TaskOutcome;
            // The About-page archive rides every asset fetch: same cadence,
            // dedup, and job gating; snapshots go to the store keyed like the
            // asset dirs (channel + platform + account).
            let sink = crate::assets::AboutSink {
                store: store.clone(),
                channel_id,
                platform: platform.as_str().to_string(),
                account: account.clone(),
            };
            let outcome = match platform {
                Platform::Twitch => match ctx.twitch_helix_auth().await {
                    Ok((client_id, token)) => {
                        let bid = if !known_bid.is_empty() {
                            Some(known_bid)
                        } else if let Some(login) = crate::detectors::twitch_login(&url) {
                            ctx.twitch_user_id(&client_id, &token, &login).await
                        } else {
                            None
                        };
                        match bid {
                            Some(bid) => {
                                let platform_dir = crate::app_paths::platform_assets_dir();
                                if crate::assets::run_twitch_assets(
                                    &http, &client_id, &token, &bid, &asset_dir, &platform_dir,
                                    Some(&sink),
                                )
                                .await
                                {
                                    TaskOutcome::Completed
                                } else {
                                    TaskOutcome::Failed("channel asset fetch failed".into())
                                }
                            }
                            None => TaskOutcome::Failed("could not resolve Twitch user id".into()),
                        }
                    }
                    Err(e) => TaskOutcome::Failed(format!("Twitch auth: {e}")),
                },
                Platform::YouTube => {
                    let api_key = store
                        .get_setting("youtube_api_key")
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    // Only resolve the UC channel ID when we have an API key to use it;
                    // the page-banner scrape only needs the channel URL.
                    let uc = if !known_bid.is_empty() {
                        Some(known_bid)
                    } else if !api_key.is_empty() {
                        crate::websub::resolve_channel_uc(&http, &url).await
                    } else {
                        None
                    };
                    let yt_channel_id = uc.as_deref().unwrap_or("");
                    let browser = store
                        .get_setting("cookies_browser")
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    let browser_name = browser.split(':').next().unwrap_or("chrome");
                    let fp = crate::browser_ua::build_browser_fingerprint(
                        if browser_name.is_empty() { "chrome" } else { browser_name }
                    );
                    if crate::assets::run_youtube_assets(
                        &http, &api_key, yt_channel_id, &url, &asset_dir, Some(&fp), Some(&sink),
                    )
                    .await
                    {
                        TaskOutcome::Completed
                    } else {
                        TaskOutcome::Failed("YouTube channel asset fetch failed".into())
                    }
                }
                Platform::Kick => {
                    let slug = if !known_bid.is_empty() {
                        Some(known_bid)
                    } else {
                        crate::detectors::kick_slug(&url)
                    };
                    match slug {
                        Some(slug)
                            if crate::assets::run_kick_assets(
                                &http,
                                &slug,
                                &asset_dir,
                                Some(&sink),
                            )
                            .await =>
                        {
                            TaskOutcome::Completed
                        }
                        Some(_) => TaskOutcome::Failed("channel asset fetch failed".into()),
                        None => TaskOutcome::Failed("could not resolve Kick slug".into()),
                    }
                }
                _ => TaskOutcome::Failed("no asset source for this platform".into()),
            };
            if let TaskOutcome::Failed(ref e) = outcome {
                tracing::warn!(monitor_id, "asset fetch failed: {e}");
            }
            running_asset_fetches.lock().unwrap().remove(&fetch_key);
            let _ = tx.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        });
    }

    /// Periodically refresh stale channel assets for enabled monitors that have
    /// asset fetching on, so channels that rarely (or never) record still keep a
    /// current icon/banner/badges/emotes. Cheap: a fresh channel is a no-op
    /// (`fetch_channel_assets` returns early when not stale), so only channels past
    /// the 24h window actually fetch.
    pub async fn asset_refresh_loop(
        &self,
        shutdown: Arc<AtomicBool>,
        jobs: crate::events::JobRegistry,
    ) {
        const INITIAL_DELAY_SECS: u64 = 45;
        const TICK_SECS: u64 = 3600; // re-scan hourly; per-channel staleness is 24h

        crate::app_core::sleep_cancellable(Duration::from_secs(INITIAL_DELAY_SECS), &shutdown).await;
        loop {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            if self.store.job_enabled("job_asset_refresh") {
                self.refresh_stale_assets_once();
                crate::events::mark_job(&jobs, "Channel asset refresh", TICK_SECS as i64);
            }
            crate::app_core::sleep_cancellable(Duration::from_secs(TICK_SECS), &shutdown).await;
        }
    }

    /// One asset-refresh pass: trigger a (staleness-gated) fetch for each eligible
    /// channel, de-duplicated across instances that share an asset dir.
    fn refresh_stale_assets_once(&self) {
        let rows = match self.store.list_monitors_with_channels() {
            Ok(r) => r,
            Err(e) => {
                warn!("asset refresh: failed to load monitors: {e:#}");
                return;
            }
        };
        // YouTube asset fetch needs the Data API; skip it without a key rather than
        // failing every pass (the manual Refetch button still surfaces the reason).
        let yt_key_set = !self
            .store
            .get_setting("youtube_api_key")
            .ok()
            .flatten()
            .unwrap_or_default()
            .is_empty();
        let recording: std::collections::HashSet<i64> =
            self.active.lock().unwrap().keys().copied().collect();
        let mut seen: std::collections::HashSet<(String, Platform, String)> =
            std::collections::HashSet::new();
        for row in &rows {
            // Master switch off → fully dormant: skip the automatic asset sweep
            // (the manual ⟳ Refetch still works). Auto-record (`enabled`) is NOT
            // checked here — an Auto-off channel's assets stay archived; only the
            // per-instance fetch toggle opts out.
            if !row.automation_on() {
                continue;
            }
            if !row.monitor.fetch_chat_assets {
                continue;
            }
            // A recording channel's record() path already handles its assets.
            if recording.contains(&row.monitor.id) {
                continue;
            }
            if row.monitor.platform() == Platform::YouTube && !yt_key_set {
                continue;
            }
            // Instances of one (channel, platform, ACCOUNT) share an asset dir —
            // fetch it once per pass. Two tools on one URL dedup here; a sibling
            // account on the same platform (main + alt) gets its own fetch.
            let account = crate::assets::account_slug(&row.monitor.url, row.monitor.platform());
            if !seen.insert((sanitize_filename(&row.channel.name), row.monitor.platform(), account)) {
                continue;
            }
            // force=false: a no-op when the channel's assets are still fresh.
            self.fetch_channel_assets(row, None, false);
        }
    }

    /// Periodically fire due [`crate::models::ScheduledRecording`] rules
    /// (schema v51) — force a recording to start at a specific time or on a
    /// weekly repeat, the same way a trigger-word match does, and auto-stop
    /// duration-bound occurrences. Independent of `run()`'s live-signal/
    /// manual-command loop: a scheduled rule calls `try_begin`/`manual_stop`
    /// directly rather than routing through a [`ManualCommand`], since
    /// `manual_start` calls `check_one` first for non-`Disabled` detection
    /// methods and would surface a "not live" toast — wrong for a headless
    /// timer that must fire unconditionally.
    pub async fn scheduled_recordings_loop(
        &self,
        shutdown: Arc<AtomicBool>,
        jobs: crate::events::JobRegistry,
    ) {
        const TICK_SECS: u64 = 20;
        loop {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            if self.store.job_enabled("job_scheduled_recordings") {
                self.scheduled_recordings_tick();
                crate::events::mark_job(&jobs, "Scheduled recordings", TICK_SECS as i64);
            }
            crate::app_core::sleep_cancellable(Duration::from_secs(TICK_SECS), &shutdown).await;
        }
    }

    fn scheduled_recordings_tick(&self) {
        let now = now_unix();
        match self.store.due_scheduled_recordings(now) {
            Ok(due) => {
                for rule in due {
                    let row = match self.store.get_monitor_with_channel(rule.monitor_id) {
                        Ok(Some(r)) => r,
                        Ok(None) => continue, // monitor gone; FK cascade already dropped the rule
                        Err(e) => {
                            warn!(
                                "scheduled recordings: failed to load monitor {}: {e:#}",
                                rule.monitor_id
                            );
                            continue;
                        }
                    };
                    // The master On/Off switch still fully gates this (dormant means
                    // dormant); leave the rule untouched so it fires the moment
                    // automation resumes instead of silently consuming the occurrence.
                    if !row.automation_on() {
                        continue;
                    }
                    let occurrence_start = rule.next_run_at.unwrap_or(now);
                    if self.try_begin(
                        rule.monitor_id, Some(now), true, None, None, None, None, None, None, None, true, true,
                    ) {
                        info!(
                            monitor_id = rule.monitor_id,
                            rule_id = rule.id,
                            "scheduled recording: force-started"
                        );
                    }
                    let next = crate::scheduled_recordings::compute_next_run(&rule, occurrence_start);
                    let pending_stop = rule.duration_secs.map(|d| now + d);
                    if let Err(e) = self.store.mark_scheduled_recording_fired(
                        rule.id,
                        occurrence_start,
                        next,
                        pending_stop,
                    ) {
                        warn!("scheduled recordings: failed to mark rule {} fired: {e:#}", rule.id);
                    }
                }
            }
            Err(e) => warn!("scheduled recordings: failed to load due rules: {e:#}"),
        }
        match self.store.due_scheduled_stops(now) {
            Ok(stops) => {
                for (id, monitor_id) in stops {
                    self.manual_stop(monitor_id);
                    if let Err(e) = self.store.clear_scheduled_recording_stop(id) {
                        warn!("scheduled recordings: failed to clear stop for rule {id}: {e:#}");
                    }
                }
            }
            Err(e) => warn!("scheduled recordings: failed to load due stops: {e:#}"),
        }
    }

    /// Whether `try_begin` should suppress an AUTOMATIC start due to a recent
    /// stall-kill for this monitor (`killed_secs_ago` = seconds since
    /// `stall_ended_at`'s timestamp, or `None` if never/not recently killed).
    /// Pure so it's unit-testable without a real clock.
    fn stall_cooldown_blocks(killed_secs_ago: Option<u64>) -> bool {
        killed_secs_ago.is_some_and(|secs| secs < STALL_RESTART_COOLDOWN_SECS)
    }

    /// Everything the "mark it live, we're not capturing it here" bookkeeping
    /// needs, snapshotted once in [`Self::try_begin`] so its several decline
    /// branches can't drift apart — they did, three copies deep, before this
    /// existed.
    fn mark_live_not_recording(
        &self,
        row: &MonitorWithChannel,
        meta: &LiveMeta,
        session_reason: Option<&str>,
    ) {
        let monitor_id = row.monitor.id;
        // The channel IS live — keep last_state and the live meta
        // (title/game/thumbnail/viewers/go-live time) as fresh as a poll would,
        // so Went Live/Started On/Duration/viewers aren't blank just because
        // this broadcast arrived via a push signal instead.
        let _ = self.store.set_monitor_check_result(monitor_id, "live", now_unix());
        let (live_since, live_since_approx) = match meta.went_live_at {
            Some(t) => (Some(t), meta.approximate),
            None => (Some(now_unix()), true),
        };
        let _ = self.store.set_monitor_live_meta(
            monitor_id,
            meta.title.as_deref().unwrap_or(""),
            meta.game.as_deref().unwrap_or(""),
            meta.thumbnail_url.as_deref().unwrap_or(""),
            // Hardcoding -1 here would clobber the correct count the
            // scheduler's own poll wrote moments earlier in the same tick.
            meta.viewers.unwrap_or(-1),
            live_since,
            live_since_approx,
        );
        if let Some(t) = meta.tags.as_deref() {
            let _ = self.store.set_monitor_tags(monitor_id, t);
        }
        let Some(reason) = session_reason else {
            return;
        };
        // The broadcast still happened — track it as a take-shaped row with no
        // capture behind it (see `insert_not_recorded_session`), so the Streams
        // grid shows a 👁 "not recorded" row instead of leaving no trace. One
        // session per broadcast: this runs on every poll while the stream stays
        // live, so reuse the open one rather than inserting a second.
        let session = match self.store.open_not_recorded_session(monitor_id) {
            Ok(Some(open)) => Some(open),
            Ok(None) => match self.store.insert_not_recorded_session(
                monitor_id,
                live_since.unwrap_or_else(now_unix),
                meta.went_live_at,
                live_since_approx,
                meta.stream_id.as_deref(),
            ) {
                Ok(rec_id) => {
                    // Stamped on insert only: a session reused across polls
                    // keeps the reason it opened with.
                    if !reason.is_empty() {
                        let _ = self.store.set_not_recorded_reason(rec_id, reason);
                    }
                    if let Some(t) = meta.title.as_deref().filter(|t| !t.is_empty()) {
                        let _ = self.store.insert_meta_change(rec_id, 0, "title", "", t);
                    }
                    if let Some(g) = meta.game.as_deref().filter(|g| !g.is_empty()) {
                        let _ = self.store.insert_meta_change(rec_id, 0, "category", "", g);
                    }
                    Some((rec_id, live_since.unwrap_or_else(now_unix)))
                }
                Err(e) => {
                    warn!(monitor_id, "failed to record not-recorded stream session: {e:#}");
                    None
                }
            },
            Err(e) => {
                warn!(monitor_id, "failed to look up the not-recorded stream session: {e:#}");
                None
            }
        };
        // Not recording the video doesn't mean not archiving the chat: chat is
        // tiny, per-platform, and unrecoverable once the broadcast ends (see
        // `chat_only.rs`). Attached to the session so it starts and stops with
        // the broadcast; a no-op when already running, which matters because
        // this re-runs every poll.
        if let Some((rec_id, session_started_at)) = session {
            self.maybe_start_chat_only(
                row,
                rec_id,
                session_started_at,
                meta.stream_id.as_deref(),
                meta.title.as_deref(),
                meta.game.as_deref(),
                meta.went_live_at,
            );
        }
    }

    /// Resolve simulcast dedup for one start attempt: gather this channel's
    /// instances into [`crate::simulcast::InstanceState`]s and ask
    /// [`crate::simulcast::decide`].
    ///
    /// All the policy lives in that pure function; this only supplies the
    /// facts it can't see — which captures are running here, and which takes
    /// they opened.
    fn simulcast_decision(
        &self,
        monitor_id: i64,
        row: &MonitorWithChannel,
        siblings: &[MonitorWithChannel],
    ) -> crate::simulcast::SimulcastDecision {
        use crate::simulcast::{InstanceState, SimulcastCtx, SimulcastDecision};
        // Nothing to dedup against: skip even the settings read.
        if siblings.is_empty() {
            return SimulcastDecision::Record;
        }
        let ctx = SimulcastCtx::load(&self.store);
        let active = self.active.lock().unwrap().keys().copied().collect::<Vec<i64>>();
        let finalizing = self.finalizing.lock().unwrap().keys().copied().collect::<Vec<i64>>();
        let states: Vec<InstanceState> = std::iter::once(row)
            .chain(siblings.iter())
            .map(|m| {
                let mid = m.monitor.id;
                // This monitor's slot was reserved a moment ago (`try_begin`
                // inserts before loading the row), so `active` would claim it
                // is capturing when it hasn't decided to yet.
                let capturing = mid != monitor_id && active.contains(&mid);
                InstanceState {
                    monitor_id: mid,
                    platform: m.monitor.platform(),
                    capturing,
                    finalizing: finalizing.contains(&mid),
                    live_state: m.monitor.last_state == "live",
                    live_since: m.monitor.last_live_since,
                    last_take_ended: m.last_recording_ended,
                    take_started_at: capturing
                        .then(|| self.store.open_recording_for_monitor(mid).ok().flatten())
                        .flatten()
                        .map(|open| open.started_at),
                    automation_on: m.automation_on(),
                    auto_record_on: m.auto_record_on(),
                    detection_disabled: m.monitor.detection_method == DetectionMethod::Disabled,
                    stop_held: self.stop_hold_blocks(mid, None, None).is_some(),
                    ad_free: m.monitor.ad_free || m.ad_free_sub == Some(true),
                    policy: ctx.policy_for(m.channel.id, mid),
                }
            })
            .collect();
        crate::simulcast::decide(&states, monitor_id, now_unix(), ctx.settle_secs)
    }

    /// Reserve the monitor and spawn its recording task. Returns false if it was
    /// skipped (already active, or in backoff when not bypassing). `forced`
    /// marks a user-initiated start: it additionally bypasses the Auto gate —
    /// the user can always record an Auto-off instance explicitly.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn try_begin(
        &self,
        monitor_id: i64,
        went_live_at: Option<i64>,
        approximate: bool,
        stream_id: Option<String>,
        thumbnail_url: Option<String>,
        broadcaster_id: Option<String>,
        stream_title: Option<String>,
        stream_game: Option<String>,
        stream_viewers: Option<i64>,
        stream_tags: Option<String>,
        bypass_backoff: bool,
        forced: bool,
    ) -> bool {
        // Manual-stop hold: a user Stop means "leave this alone" — no
        // automatic restart (poll, push, or trigger rule) until a NEW
        // broadcast appears or the timed hold expires. A manual ▶ Start
        // clears it before reaching here (`manual_start`). A "Stop (allow
        // triggers)" hold (`allow_triggers`) is more lenient: a trigger-word
        // match may still bypass it, so we can't reject it yet — trigger
        // rules aren't evaluated until after `row`/`block_rules`/`trigger_hit`
        // below. Enforced for real once we know whether one matched.
        let hold_block = self.stop_hold_blocks(monitor_id, stream_id.as_deref(), went_live_at);
        if let Some((reason, allow_triggers)) = &hold_block
            && !allow_triggers
        {
            tracing::debug!(monitor_id, "auto start suppressed: {reason}");
            return false;
        }
        // Stall-kill cooldown (see `stall_ended_at`'s doc comment): a forced/
        // manual start always bypasses it, same as a manual-stop hold above —
        // only an AUTOMATIC start needs a moment for the platform's own
        // offline propagation to catch up with our own stall watchdog.
        if !forced {
            let killed_secs_ago =
                self.stall_ended_at.lock().unwrap().get(&monitor_id).map(Instant::elapsed).map(|d| d.as_secs());
            if Self::stall_cooldown_blocks(killed_secs_ago) {
                tracing::debug!(
                    monitor_id,
                    "auto start suppressed: stall-killed {}s ago — waiting for the platform's own \
                     offline signal to catch up before trying again",
                    killed_secs_ago.unwrap_or(0)
                );
                return false;
            }
        }
        // A subscriber-only broadcast is being archived from the CDN instead
        // (see `sub_only.rs`). The live edge is not ours to take, so asking
        // again every few minutes only burns a process and a log line — and
        // the session already covers this broadcast. A forced/manual start
        // still goes through: the user may know something we don't (they just
        // subscribed, the streamer opened it up).
        if !forced && self.sub_only_session_active(monitor_id) {
            tracing::debug!(
                monitor_id,
                "auto start suppressed: subscriber-only CDN capture session owns this broadcast"
            );
            return false;
        }
        {
            let mut active = self.active.lock().unwrap();
            if active.contains_key(&monitor_id) {
                return false;
            }
            if !bypass_backoff && self.in_backoff(monitor_id) {
                return false;
            }
            active.insert(monitor_id, 0); // reserve; real PID set after spawn
        }
        debug!(monitor_id, forced, "active: reserved (try_begin)");
        if bypass_backoff {
            self.backoff.lock().unwrap().remove(&monitor_id);
        }

        // Siblings ride along free: this is the same one-pass load
        // `get_monitor_with_channel` was already doing, split instead of
        // discarded (see its doc). Simulcast dedup needs them below.
        let (mut row, siblings) = match self.store.get_monitor_with_siblings(monitor_id) {
            Ok(Some(v)) => v,
            _ => {
                self.release_active(monitor_id, "try_begin: monitor row vanished/query failed after reserving");
                return false;
            }
        };
        // The live-state snapshot every "it's live, we're just not capturing
        // it here" branch below writes.
        let live_meta = LiveMeta {
            went_live_at,
            approximate,
            stream_id: stream_id.clone(),
            title: stream_title.clone(),
            game: stream_game.clone(),
            thumbnail_url: thumbnail_url.clone(),
            viewers: stream_viewers,
            tags: stream_tags.clone(),
        };
        // Trigger words: a title/game match starts a recording even with Auto
        // off, and its per-rule overrides apply even with Auto on.
        // `enabled` is the Auto-record flag — it gates AUTOMATIC recording to
        // disk ONLY (a disk-space control), never detection/metadata/fetch. The
        // master dormancy switch is handled upstream (scheduler skips dormant
        // monitors, so this path isn't reached for them).
        let auto_off = !row.channel.enabled || !row.monitor.enabled;
        // Blacklist rules resolve up front: they veto any automatic start
        // below, and a metadata-less push signal must fetch the title/game
        // before starting whenever any exist.
        let block_rules = if forced {
            Vec::new()
        } else {
            crate::triggers::effective_block_rules(&self.store, row.channel.id, monitor_id)
        };
        let mut trigger_hit: Option<crate::triggers::TriggerHit> = None;
        {
            let rules =
                crate::triggers::effective_rules(&self.store, row.channel.id, monitor_id);
            let has_meta = stream_title.is_some() || stream_game.is_some();
            if !rules.is_empty() && has_meta {
                trigger_hit = crate::triggers::first_match(
                    &rules,
                    stream_title.as_deref(),
                    stream_game.as_deref(),
                    now_unix(),
                );
            }
            // The signal carried no title/game (EventSub push) but rules need
            // them: whitelist triggers on an Auto-off monitor (a match is the
            // only thing that could start it), or ANY blacklist rule (a match
            // must veto the start). Re-detect to fetch the metadata, then
            // re-enter with it filled in (the has_meta guard above prevents a
            // second re-check loop).
            let need_meta_for_trigger = !rules.is_empty() && auto_off;
            let need_meta_for_block = !block_rules.is_empty();
            if !has_meta && !forced && (need_meta_for_trigger || need_meta_for_block) {
                self.release_active(monitor_id, "try_begin: re-checking metadata for a trigger/blacklist rule before deciding");
                let this = self.clone();
                let row_bg = row.clone();
                let stream_tags_bg = stream_tags.clone();
                tokio::spawn(async move {
                    let o = this.check_one(&row_bg).await;
                    if o.live {
                        let got_meta = o.stream_title.is_some() || o.stream_game.is_some();
                        if got_meta {
                            this.try_begin(
                                monitor_id,
                                o.went_live_at.or(went_live_at),
                                o.went_live_at.is_none() && approximate,
                                o.stream_id.or(stream_id),
                                o.thumbnail_url.or(thumbnail_url),
                                o.broadcaster_id.or(broadcaster_id),
                                o.stream_title,
                                o.stream_game,
                                o.stream_viewers.or(stream_viewers),
                                o.stream_tags.or(stream_tags_bg),
                                bypass_backoff,
                                forced,
                            );
                        } else if !need_meta_for_trigger {
                            // Only the blacklist needed the metadata and none
                            // could be fetched: fail OPEN (record) — an
                            // archiver errs on capturing. Some("") marks the
                            // metadata as checked so this can't loop.
                            this.try_begin(
                                monitor_id,
                                o.went_live_at.or(went_live_at),
                                o.went_live_at.is_none() && approximate,
                                o.stream_id.or(stream_id),
                                o.thumbnail_url.or(thumbnail_url),
                                o.broadcaster_id.or(broadcaster_id),
                                Some(String::new()),
                                None,
                                o.stream_viewers.or(stream_viewers),
                                o.stream_tags,
                                bypass_backoff,
                                forced,
                            );
                        }
                    }
                });
                let _ = self.store.set_monitor_check_result(monitor_id, "live", now_unix());
                // Title/game aren't known yet here (that's why the re-check
                // above was spawned) but the go-live time is, so at least
                // Went Live/Started On/Duration have data instead of sitting
                // blank until the re-check (or the next poll) fills the rest in.
                // `stream_viewers` may still be known even without title/game
                // (e.g. the caller's own poll had it) — preserve it rather
                // than clobbering to unknown.
                let (live_since, live_since_approx) = match went_live_at {
                    Some(t) => (Some(t), approximate),
                    None => (Some(now_unix()), true),
                };
                let _ = self.store.set_monitor_live_meta(
                    monitor_id, "", "", "", stream_viewers.unwrap_or(-1), live_since, live_since_approx,
                );
                if let Some(t) = stream_tags.as_deref() {
                    let _ = self.store.set_monitor_tags(monitor_id, t);
                }
                return false;
            }
        }
        // Blacklist triggers: the inverse of trigger words — a title/game
        // match VETOES any automatic start (Auto-record or a trigger-word
        // match alike); only an explicit user ▶ Start records. An explicit
        // "don't record this" beats "record this", so a blacklist hit wins
        // over a whitelist trigger hit.
        if !forced
            && let Some(block) = crate::triggers::first_match(
                &block_rules,
                stream_title.as_deref(),
                stream_game.as_deref(),
                now_unix(),
            )
        {
            self.release_active(monitor_id, "try_begin: blacklist trigger matched — vetoing automatic start");
            // Keep the UI's live state fresh exactly like the Auto-off path
            // below — the channel IS live, it's just not being recorded.
            self.mark_live_not_recording(&row, &live_meta, None);
            // Log + notify once per broadcast — try_begin re-runs on every
            // poll while the stream stays live.
            let key = stream_id
                .clone()
                .unwrap_or_else(|| went_live_at.unwrap_or(0).to_string());
            let fresh = self
                .blocked_notified
                .lock()
                .unwrap()
                .insert(monitor_id, key.clone())
                != Some(key);
            if fresh {
                let desc = block.describe();
                info!(
                    monitor_id,
                    hit = desc.as_str(),
                    "blacklist trigger matched for {} {} — automatic recording suppressed",
                    row.monitor.platform().tag(),
                    row.channel.name
                );
                let _ = self.events.send(AppEvent::TriggerBlocked {
                    monitor_id,
                    desc,
                    matched: block.matched.clone(),
                    went_live_at: went_live_at.unwrap_or(0),
                });
            }
            return false;
        }
        if !forced && auto_off && trigger_hit.is_none() {
            // Auto-record is off for this channel/instance: detection keeps the
            // state fresh, but only an explicit user Start (or a trigger-word
            // match) records. Update last_state so the UI shows "live", and the
            // live meta (title/game/thumbnail/viewers/go-live time) the same way
            // the poll scheduler does, so Went Live/Started On/Duration/viewers
            // aren't blank just because this channel was seen live via a push
            // signal instead. `stream_viewers` was previously hardcoded to -1
            // here, clobbering the correct value the scheduler's own poll had
            // just written moments earlier in the same tick (every live poll
            // sends a LiveSignal here regardless of Auto) — see `manual_start`'s
            // parallel branch below, which already got this right.
            self.release_active(monitor_id, "try_begin: Auto-record is off for this channel/instance");
            // `""` is the historical reason: Auto-record was off. Anything else
            // names a newer one (see `Recording::not_recorded_reason`).
            self.mark_live_not_recording(&row, &live_meta, Some(""));
            return false;
        }
        // Simulcast dedup: another instance of this channel is carrying this
        // same broadcast on the preferred platform, so don't capture it twice.
        // Sits after the blacklist veto and the Auto-off branch (both are
        // stronger, more specific "don't record" answers) and skips a forced
        // start or a trigger-word match, which are explicit "record this".
        if !forced && trigger_hit.is_none() {
            match self.simulcast_decision(monitor_id, &row, &siblings) {
                crate::simulcast::SimulcastDecision::Record => {}
                crate::simulcast::SimulcastDecision::Standby { winner, winner_platform } => {
                    self.release_active(
                        monitor_id,
                        "try_begin: simulcast dedup — another instance has this broadcast",
                    );
                    let reason = format!(
                        "{} recording this broadcast on the {} instance instead",
                        crate::simulcast::SKIP_REASON_PREFIX,
                        winner_platform.label()
                    );
                    // Same bookkeeping as Auto-off, chat-only included: chat is
                    // per-platform, so the standby instance still archives its
                    // own conversation while the other records the video.
                    self.mark_live_not_recording(&row, &live_meta, Some(&reason));
                    // Once per broadcast, not once per poll.
                    let key = stream_id
                        .clone()
                        .unwrap_or_else(|| went_live_at.unwrap_or(0).to_string());
                    let fresh = self
                        .blocked_notified
                        .lock()
                        .unwrap()
                        .insert(monitor_id, key.clone())
                        != Some(key);
                    if fresh {
                        info!(
                            monitor_id,
                            winner,
                            "simulcast dedup: standing by for the {} instance of {} (failover armed)",
                            winner_platform.tag(),
                            row.channel.name
                        );
                    }
                    return false;
                }
                crate::simulcast::SimulcastDecision::Takeover { stop } => {
                    // This instance is the preferred source and the broadcast is
                    // still settling: stop the duplicate(s) and record here.
                    // Plain `manual_stop` sets no hold (unlike
                    // `manual_stop_hold`), so those instances stay armed as
                    // failover — the same automated-stop path the
                    // quality-upgrade restart uses.
                    for loser in stop {
                        info!(
                            monitor_id,
                            loser,
                            "simulcast dedup: taking {} over to the preferred {} instance",
                            row.channel.name,
                            row.monitor.platform().tag()
                        );
                        self.manual_stop(loser);
                    }
                }
            }
        }
        // A "Stop (allow triggers)" hold still blocks plain Auto-record: only
        // an actual trigger match (or a forced/manual start, which already
        // cleared the hold before reaching here) may proceed past it. Same
        // "mark live, not recording" bookkeeping as the auto_off branch above.
        if !forced
            && trigger_hit.is_none()
            && let Some((reason, true)) = &hold_block
        {
            tracing::debug!(monitor_id, "auto start suppressed (no trigger matched): {reason}");
            self.release_active(monitor_id, "try_begin: manual-stop hold blocks plain Auto-record");
            self.mark_live_not_recording(&row, &live_meta, None);
            return false;
        }
        let trigger_info = trigger_hit.as_ref().map(|h| h.describe()).unwrap_or_default();
        // The whole matched rule (not just its description), frozen at start
        // time — the stop-on-unmatch watcher and head-backfill leadtime both
        // need it, and re-resolving `effective_rules()` live later would let a
        // mid-broadcast rule edit/reorder silently retarget an already-running
        // take (rules have no stable id).
        let trigger_rule = trigger_hit.as_ref().map(|h| h.rule.clone());
        if let Some(hit) = &trigger_hit {
            // Per-rule override: the recording this rule starts captures from
            // the start (or not) regardless of the monitor's own flag. Applied
            // on the row clone so every downstream read sees it.
            if let Some(v) = hit.rule.capture_from_start {
                row.monitor.capture_from_start = v;
            }
            info!(
                monitor_id,
                channel = row.channel.name.as_str(),
                hit = trigger_info.as_str(),
                forced_start = auto_off,
                "trigger word matched — starting recording"
            );
            let _ = self.events.send(AppEvent::TriggerMatched {
                monitor_id,
                desc: trigger_info.clone(),
                matched: hit.matched.clone(),
                went_live_at: went_live_at.unwrap_or(0),
                forced_start: auto_off,
            });
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.record(row, went_live_at, approximate, stream_id, thumbnail_url, broadcaster_id, stream_title, trigger_info, trigger_rule).await;
        });
        true
    }

    /// EventSub `stream.offline` push: the counterpart to a `LiveSignal` that
    /// clears rather than starts. A monitor currently owned by an active
    /// recording keeps its "recording" state — the tool's own exit path sets
    /// the final status, and a push racing that would otherwise regress the UI
    /// from "recording" back to "offline" while the file is still being
    /// finalized. Otherwise (Auto off and/or nothing recording, e.g. Milk's
    /// case: EventSub only ever stamped "live" and had no way back) mark the
    /// monitor offline so a stale "live" state doesn't linger forever, since
    /// pure `DetectionMethod::EventSub` is deliberately excluded from the
    /// scheduler's poll fallback (`scheduler.rs`'s `handled` set).
    fn handle_offline_signal(&self, monitor_id: i64) {
        if self.active.lock().unwrap().contains_key(&monitor_id) {
            return;
        }
        let now = now_unix();
        let _ = self.store.set_monitor_check_result(monitor_id, "offline", now);
        // This push is the only "went offline" signal this monitor gets when
        // EventSub beats the scheduler's own poll to writing `last_state`
        // (routine for a hybrid eventsub_helix monitor) — the scheduler's
        // tick-based close (see its `old_state == Some("live")` check) never
        // observes a live->offline edge in that case, since by the time it
        // polls, `last_state` here has already flipped to "offline". Without
        // this, any not-recorded session (Auto off; see
        // `insert_not_recorded_session`) opened for this broadcast is left
        // open forever (found live 2026-07-28: monitor 50/GEEGA, rec_id=1098).
        if let Ok(closed) = self.store.close_open_not_recorded_sessions(monitor_id, now)
            && crate::downloader::vod::setting_true(&self.store, crate::downloader::vod::K_AUTO_BACKFILL_MISSED)
        {
            for rec_id in closed {
                crate::downloader::vod::attempt_missed_stream_backfill(
                    self.ctx.clone(),
                    self.store.clone(),
                    self.events.clone(),
                    self.manual_tx.clone(),
                    rec_id,
                );
            }
        }
    }

    /// "Start" command: check the channel now and record if live. A
    /// user-initiated start records even when Auto is off (Auto only gates
    /// *automatic* starts) and toasts when the channel isn't live; an
    /// automatic trigger (WebSub push) honors the Auto gate and just keeps
    /// the stream state fresh. `Disabled` detection skips the check entirely
    /// (see below).
    pub(super) async fn manual_start(&self, monitor_id: i64, user_initiated: bool) {
        if self.active.lock().unwrap().contains_key(&monitor_id) {
            return; // already recording
        }
        // An explicit user Start overrides (and removes) any stop-hold.
        if user_initiated {
            self.clear_stop_hold(monitor_id);
        }
        let row = match self.store.get_monitor_with_channel(monitor_id) {
            Ok(Some(r)) => r,
            _ => return,
        };
        // A dormant monitor (master switch off) ignores automatic push triggers
        // (WebSub/EventSub) entirely — it does nothing until manually acted on.
        // An explicit user Start still works (it's a manual trigger).
        if !user_initiated && !row.automation_on() {
            return;
        }
        // Disabled detection has no configured way to check liveness at all
        // (the scheduler never polls it and no push is subscribed either) — a
        // manual Start is the only way such an instance ever records, so it
        // trusts the user and skips straight to recording instead of calling
        // check_one (which would just report "not live" and never proceed).
        if row.monitor.detection_method == DetectionMethod::Disabled {
            if user_initiated {
                self.try_begin(monitor_id, Some(now_unix()), true, None, None, None, None, None, None, None, true, true);
            }
            return;
        }
        let auto = row.auto_record_on();
        let name = row.channel.name.clone();
        let outcome = self.check_one(&row).await;
        if outcome.live {
            if auto || user_initiated {
                let (went, approx) = match outcome.went_live_at {
                    Some(t) => (Some(t), false),
                    None => (Some(now_unix()), true),
                };
                self.try_begin(monitor_id, went, approx, outcome.stream_id, outcome.thumbnail_url, outcome.broadcaster_id, outcome.stream_title, outcome.stream_game, outcome.stream_viewers, outcome.stream_tags, true, user_initiated);
            } else {
                // Auto off + automatic trigger: just update the state + live
                // meta (title/game/thumbnail/viewers/go-live time) so the UI
                // shows "live" with Went Live/Started On/Duration populated;
                // nothing records.
                let _ = self.store.set_monitor_check_result(monitor_id, "live", now_unix());
                let (live_since, live_since_approx) = match outcome.went_live_at {
                    Some(t) => (Some(t), false),
                    None => (Some(now_unix()), true),
                };
                let _ = self.store.set_monitor_live_meta(
                    monitor_id,
                    outcome.stream_title.as_deref().unwrap_or(""),
                    outcome.stream_game.as_deref().unwrap_or(""),
                    outcome.thumbnail_url.as_deref().unwrap_or(""),
                    outcome.stream_viewers.unwrap_or(-1),
                    live_since,
                    live_since_approx,
                );
                if let Some(t) = outcome.stream_tags.as_deref() {
                    let _ = self.store.set_monitor_tags(monitor_id, t);
                }
            }
        } else if user_initiated {
            let message = if outcome.error && !outcome.detail.is_empty() {
                format!("{name}: {}", outcome.detail)
            } else {
                format!("{name} is not live")
            };
            let _ = self.events.send(AppEvent::Error {
                context: "Start".into(),
                message,
            });
        } else {
            // Automatic trigger and offline: update state silently.
            let _ = self.store.set_monitor_check_result(monitor_id, "offline", now_unix());
        }
    }

    /// Manual "Stop": abort the active recording and apply a short cooldown so it
    /// doesn't immediately restart on the next poll.
    fn manual_stop(&self, monitor_id: i64) {
        // A subscriber-only CDN session has no process to kill, but Stop means
        // stop: it wraps up after its current pass and joins what it holds.
        self.abort_sub_only_session(monitor_id);
        let pid = self.active.lock().unwrap().get(&monitor_id).copied();
        // Kill the DASH companion (dual capture) too, if one is running.
        let companion_pid = self.active_secondary.lock().unwrap().get(&monitor_id).copied();
        if let Some(p) = companion_pid {
            self.stopping_monitors.lock().unwrap().insert(monitor_id);
            if p > 0 {
                crate::platform::kill_process_tree(p);
            }
        }
        if let Some(pid) = pid {
            self.stopping_monitors.lock().unwrap().insert(monitor_id);
            if pid > 0 {
                crate::platform::kill_process_tree(pid);
            }
            self.backoff.lock().unwrap().insert(
                monitor_id,
                BackoffEntry {
                    fails: 0,
                    until: Instant::now() + Duration::from_secs(120),
                    po_rejected: false,
                },
            );
            info!(monitor_id, "manual stop");
        }
    }

    /// User Stop with restart suppression: stop the active take (if any) and
    /// hold automatic restarts — `hours: None` until a NEW broadcast appears
    /// (the channel goes offline and live again), `Some(h)` for a fixed
    /// number of hours regardless of stream cycles. A manual ▶ Start clears
    /// the hold. Automated stops (trigger stop-on-unmatch, scheduled stops,
    /// the quality-upgrade restart) use plain [`manual_stop`] and never hold.
    /// `allow_triggers`: see [`StopHold::allow_triggers`] — a trigger-word
    /// match can still start a fresh recording during the hold.
    pub fn manual_stop_hold(&self, monitor_id: i64, hours: Option<i64>, allow_triggers: bool) {
        let hold = match hours {
            Some(h) => StopHold::Until { at: now_unix() + h * 3600, allow_triggers },
            None => {
                let (stream_id, went_live_at) = self
                    .store
                    .latest_stream_identity(monitor_id)
                    .ok()
                    .flatten()
                    .unwrap_or((None, None));
                StopHold::FreshStream { stream_id, went_live_at, allow_triggers }
            }
        };
        {
            let mut holds = self.stop_holds.lock().unwrap();
            holds.insert(monitor_id, hold);
            persist_stop_holds(&self.store, &holds);
        }
        info!(monitor_id, hours, allow_triggers, "manual stop with restart hold");
        self.manual_stop(monitor_id);
    }

    /// Remove a monitor's stop-hold (manual ▶ Start, or expiry).
    fn clear_stop_hold(&self, monitor_id: i64) {
        let mut holds = self.stop_holds.lock().unwrap();
        if holds.remove(&monitor_id).is_some() {
            persist_stop_holds(&self.store, &holds);
            info!(monitor_id, "stop hold cleared");
        }
    }

    /// `Some((reason, allow_triggers))` when a stop hold is still in effect
    /// for this monitor; expired/superseded holds are removed here as a side
    /// effect. `allow_triggers` tells the caller whether a trigger-word match
    /// should be allowed to bypass this particular hold — plain Auto-record
    /// (polls/pushes) is ALWAYS blocked by a live hold regardless.
    fn stop_hold_blocks(
        &self,
        monitor_id: i64,
        stream_id: Option<&str>,
        went_live_at: Option<i64>,
    ) -> Option<(String, bool)> {
        let mut holds = self.stop_holds.lock().unwrap();
        let hold = holds.get(&monitor_id)?.clone();
        let expired = match &hold {
            StopHold::Until { at, .. } => now_unix() >= *at,
            StopHold::FreshStream { stream_id: held_sid, went_live_at: held_wl, .. } => {
                // A NEW broadcast = a different stream id, or a strictly newer
                // go-live. Unknown identities on either side keep the hold —
                // never resume on a guess.
                let new_sid = matches!(
                    (held_sid.as_deref(), stream_id),
                    (Some(h), Some(n)) if h != n
                );
                let newer_wl = matches!((held_wl, went_live_at), (Some(h), Some(n)) if n > *h);
                new_sid || newer_wl
            }
        };
        if expired {
            holds.remove(&monitor_id);
            persist_stop_holds(&self.store, &holds);
            return None;
        }
        let allow_triggers = hold.allow_triggers();
        Some((
            match hold {
                StopHold::Until { at, .. } => format!("held until unix {at}"),
                StopHold::FreshStream { .. } => "held until a new broadcast".to_string(),
            },
            allow_triggers,
        ))
    }

    /// Watch a young Twitch `best`-quality capture for a better rendition
    /// appearing after join: Twitch's master playlist often lists only
    /// transcodes for the first moments of a broadcast, so a capture that
    /// joins seconds after go-live can lock onto e.g. 720p60 while the source
    /// (1080p60) shows up shortly after — and the VOD/head backfill is always
    /// source, which is how head/live joins end up mismatched. When a better
    /// rendition appears within the first checks, the take is stopped like
    /// the Stop button (finalizes as "stopped") with a SHORT backoff so
    /// automation restarts it at the better quality; the new take's head
    /// backfill covers the seam — at source on both sides, so it joins into
    /// a complete `full.mkv` at the better quality. At most one restart per
    /// stream (see `quality_upgraded`).
    async fn quality_upgrade_watcher(
        self,
        monitor_id: i64,
        stream_key: String,
        url: String,
        capture_path: PathBuf,
        channel: String,
    ) {
        // First check after the rendition list has had time to fill in;
        // second catches a late transcode→source flip.
        const CHECKS_AT_SECS: [u64; 2] = [180, 480];
        let mut elapsed = 0u64;
        for at in CHECKS_AT_SECS {
            while elapsed < at {
                if self.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                elapsed += 2;
                if !self.active.lock().unwrap().contains_key(&monitor_id) {
                    return; // take ended on its own
                }
            }
            if self.quality_upgraded.lock().unwrap().contains(&stream_key) {
                return;
            }
            // What the capture is actually recording (the growing TS probes
            // fine once a few seconds exist).
            let Some(current) = probe_media(&capture_path.to_string_lossy()).await else {
                continue;
            };
            let (Ok(cur_h), Ok(cur_fps)) =
                (current.height.parse::<i64>(), current.fps.parse::<f64>())
            else {
                continue;
            };
            let cur_fps = cur_fps.round() as i64;
            // What Twitch offers right now.
            let Some((best_h, best_fps, name)) = best_available_rendition(&url).await else {
                continue;
            };
            if (best_h, best_fps) <= (cur_h, cur_fps) {
                continue; // already recording the best on offer
            }
            if !self.quality_upgraded.lock().unwrap().insert(stream_key.clone()) {
                return;
            }
            info!(
                monitor_id,
                "quality upgrade: {name} appeared (capturing {cur_h}p{cur_fps}) — restarting the take"
            );
            let _ = self.events.send(AppEvent::QualityUpgraded {
                monitor_id,
                channel: channel.clone(),
                from: format!("{cur_h}p{cur_fps}"),
                to: name,
            });
            // Stop like the Stop button (tombstone → finalizes as "stopped"),
            // but with a short backoff: the next poll restarts the capture at
            // the better quality within roughly a minute.
            self.stopping_monitors.lock().unwrap().insert(monitor_id);
            let pid = self.active.lock().unwrap().get(&monitor_id).copied();
            if let Some(pid) = pid.filter(|&p| p > 0) {
                crate::platform::kill_process_tree(pid);
            }
            let companion = self.active_secondary.lock().unwrap().get(&monitor_id).copied();
            if let Some(p) = companion.filter(|&p| p > 0) {
                crate::platform::kill_process_tree(p);
            }
            self.backoff.lock().unwrap().insert(
                monitor_id,
                BackoffEntry {
                    fails: 0,
                    until: Instant::now() + Duration::from_secs(10),
                    po_rejected: false,
                },
            );
            return;
        }
    }

    /// Stop the live-chat sidecar download for a monitor, if one is running.
    /// A registered PID of 0 (an in-process Twitch logger) has nothing to
    /// kill; the stop is delivered through flags instead — `stopping_chats`
    /// for a chat-only session (its watcher polls it), and the take logger's
    /// own `chat_done` flag (registered in `take_chat_done`) for a sidecar
    /// running alongside a recording.
    pub(super) fn stop_chat_download(&self, monitor_id: i64) {
        let pid = self.active_chats.lock().unwrap().get(&monitor_id).copied();
        let Some(pid) = pid else { return };
        self.stopping_chats.lock().unwrap().insert(monitor_id);
        if pid > 0 {
            crate::platform::kill_process_tree(pid);
        } else if let Some(done) = self.take_chat_done.lock().unwrap().get(&monitor_id) {
            done.store(true, Ordering::SeqCst);
        }
        info!(monitor_id, "stop chat download");
    }

    /// Stop the YouTube chat sidecar for `monitor_id` (if running) and wait
    /// up to `timeout` for it to release its `live_chat.json` file handle.
    /// Called before `rename_companion_sidecars` so the rename isn't blocked
    /// by an actively-writing chat process (Windows os error 32). PID-0
    /// entries (in-process Twitch loggers) are deliberately ignored: they
    /// hold no rename-blocking handle, and a finalize for an OLD take must
    /// not stop the chat logger a newer take of the same monitor owns.
    pub(super) async fn stop_and_wait_for_chat(&self, monitor_id: i64, timeout: Duration) {
        if self.active_chats.lock().unwrap().get(&monitor_id).is_none_or(|&pid| pid == 0) {
            return;
        }
        self.stop_chat_download(monitor_id);
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if !self.active_chats.lock().unwrap().contains_key(&monitor_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// Run a live-chat sidecar yt-dlp process for `monitor_id`. Spawns yt-dlp
    /// with `--skip-download --sub-langs=live_chat --write-subs` so it captures
    /// only chat alongside the video recording. Registers its PID in
    /// `active_chats` (visible to the UI), and removes it when the process exits
    /// (either stream ended naturally, or the user called `stop_chat_download`).
    /// Also used verbatim by a chat-only session (`chat_only.rs`), which
    /// differs only in there being no video capture alongside it.
    pub(super) async fn run_chat_download(&self, monitor_id: i64, platform: Platform, plan: DownloadPlan) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // At most ONE chat sidecar per monitor. Both handles on this process —
        // `active_chats` and the `detached_process` row — are keyed by
        // `monitor_id` alone, so starting a second one for the same monitor
        // overwrites both and the first becomes unreachable: no registry row,
        // no PID in `active_chats`, nothing that can ever stop it or reap it
        // at shutdown. It just keeps running, writing its own now-abandoned
        // `.live_chat.json` and re-fetching PO tokens forever.
        //
        // That is not hypothetical: a YouTube stream failing repeatedly
        // (invalid GVS PO token) restarted the take every ~2 minutes, and each
        // restart orphaned the previous take's sidecar — three live, untracked
        // yt-dlp processes for one monitor inside eight minutes, all appending
        // the same chat to different files (found 2026-07-31).
        //
        // The take that starts last owns the monitor's chat, so stop the
        // incumbent and wait for it to release before claiming the slot.
        if self.active_chats.lock().unwrap().contains_key(&monitor_id) {
            /// Long enough for the old sidecar to die and run its own cleanup
            /// (which is what frees the `active_chats` slot); past that we
            /// proceed anyway — a missing chat log beats no chat log at all.
            const SUPERSEDE_GRACE: Duration = Duration::from_secs(10);
            info!(monitor_id, "chat download: superseding the previous sidecar for this monitor");
            self.stop_and_wait_for_chat(monitor_id, SUPERSEDE_GRACE).await;
            if self.active_chats.lock().unwrap().contains_key(&monitor_id) {
                warn!(
                    monitor_id,
                    "chat download: the previous sidecar didn't exit within {SUPERSEDE_GRACE:?} — \
                     starting the new one anyway (the old process may linger untracked)"
                );
            }
        }
        let tag = platform.tag();
        // Detached like every other download: a named job without kill-on-close,
        // no kill_on_drop, and output to a log file so the sidecar survives an app
        // restart and a relaunch can re-attach. yt-dlp writes the `.live_chat.json`
        // directly; this log only captures its diagnostics.
        let log_path = capture_log_path(&plan.capture_path, "chat.log");
        let (out_h, err_h) = match open_log_pair(&log_path) {
            Ok(p) => p,
            Err(e) => {
                warn!(monitor_id, "chat log open failed: {e}");
                return;
            }
        };
        let job_name = format!("Local\\StreamArchiver_chat_{monitor_id}");
        let job = DetachedJob::create(&job_name).ok();

        let mut cmd = Command::new(&plan.program);
        // UTF-8 std streams for Python tools — see run_process for why.
        cmd.args(&plan.args)
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_h))
            .stderr(Stdio::from(err_h));
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                warn!(monitor_id, "chat download spawn failed: {e}");
                return;
            }
        };
        if let Some(j) = &job {
            if let Err(e) = j.assign_child(&child) {
                warn!(monitor_id, "chat job assign failed: {e:#}");
            }
        }
        let pid = child.id().unwrap_or(0);
        if pid != 0 {
            self.active_chats.lock().unwrap().insert(monitor_id, pid);
            let row = DetachedRow {
                kind: DetachedKind::Chat,
                ref_id: monitor_id,
                monitor_id: Some(monitor_id),
                pid,
                proc_start: crate::platform::process_start_time(pid).unwrap_or(0),
                job_name: job_name.clone(),
                log_path: log_path.to_string_lossy().into_owned(),
                capture_path: plan.capture_path.to_string_lossy().into_owned(),
                final_path: plan.final_path.to_string_lossy().into_owned(),
                remux_to_mkv: false,
                take_group: None,
                spawn_build: crate::version::build_id().to_string(),
                started_at: now_unix(),
                secondary: false,
                stream_id: None,
                went_live_at: None,
            };
            if let Err(e) = self.store.register_detached(&row) {
                warn!(monitor_id, "register chat detached failed: {e:#}");
            }
        }
        // I/O-monitor registration; guard drops after the wait below.
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
                    purpose: "chat capture".to_string(),
                    region: crate::iomon::classify(&plan.capture_path),
                    net: crate::iomon::NetKind::Chat,
                    proc_start: crate::platform::process_start_time(pid).unwrap_or(0),
                },
            )
        });
        info!(monitor_id, "chat download started {tag}");
        // Fire any event so the UI repaints and shows the chat-active indicator.
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "chat_active".into(),
        });

        let _ = child.wait().await;
        if let Some(j) = &job {
            j.kill(); // clean up any straggler before we drop the handle
        }
        drop(job);
        let _ = self.store.clear_detached(DetachedKind::Chat, monitor_id);
        let stopped = self.stopping_chats.lock().unwrap().remove(&monitor_id);
        self.active_chats.lock().unwrap().remove(&monitor_id);
        // Surface any yt-dlp diagnostics (auth failure, format unavailable, …)
        // — but only genuinely diagnostic lines. A clean stream-end's tail is
        // just `\r` progress rewrites, and dumping it raw leaked noise like
        // "[download] 100% of 4.75MiB …" into the app log at WARN on every
        // normal chat end.
        let tail = read_log_tail(&log_path, 12).await;
        let diag = diagnostic_log_lines(&tail, 8);
        if !diag.is_empty() {
            warn!(monitor_id, "chat yt-dlp diagnostics {tag}:\n{diag}");
        }
        if stopped {
            info!(monitor_id, "chat download stopped by user {tag}");
        } else {
            info!(monitor_id, "chat download ended {tag}");
        }
        // Repaint so the indicator disappears.
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "idle".into(),
        });
    }

    /// Begin an on-demand video download: reserve it and spawn its task.
    async fn start_video(&self, video_id: i64) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut active = self.active_videos.lock().unwrap();
            if active.contains_key(&video_id) {
                return; // already downloading/queued
            }
            active.insert(video_id, 0); // reserve; real PID set after spawn
        }
        let video = match self.store.get_video(video_id) {
            Ok(Some(v)) => v,
            _ => {
                self.active_videos.lock().unwrap().remove(&video_id);
                return;
            }
        };
        let this = self.clone();
        tokio::spawn(async move { this.download_video(video).await });
    }

    /// Abort an in-flight (or queued) on-demand video download.
    ///
    /// The stop "tombstone" is recorded only while the download is actually
    /// active, and under the `active_videos` lock — so it can never linger after
    /// the task has finalized (which would otherwise silently cancel a later
    /// retry of the same id) and can never race the finalize into the wrong
    /// status. `download_video` consumes the tombstone under the same lock.
    fn stop_video(&self, video_id: i64) {
        let pid = {
            let active = self.active_videos.lock().unwrap();
            let Some(pid) = active.get(&video_id).copied() else {
                return; // not active: nothing to stop, don't leave a tombstone
            };
            self.stopping_videos.lock().unwrap().insert(video_id);
            pid
        };
        if pid > 0 {
            crate::platform::kill_process_tree(pid);
            // Already downloading: reflect the stop immediately (download_video
            // will re-confirm with the final byte count).
            let _ = self.store.set_video_status(video_id, "stopped");
        }
        info!(video_id, pid, "stop video download");
    }

    /// Atomically decide a video's final status and drop its `active_videos`
    /// membership: a stop tombstone (set under the same lock by `stop_video`)
    /// wins over the byte-count classification. `media_ok` is the caller's
    /// verdict that the final file is a real media output (plausible extension
    /// and, when the exit code is nonzero/unknown, an ffprobe-confirmed
    /// duration) — without it a nonzero-size `.log` promoted by mistake would
    /// classify "completed". Returns the chosen status.
    fn finalize_video(&self, id: i64, bytes: i64, media_ok: bool, shutting_down: bool) -> &'static str {
        let mut active = self.active_videos.lock().unwrap();
        let stopped = self.stopping_videos.lock().unwrap().remove(&id);
        let stalled = self
            .stall_killed
            .lock()
            .unwrap()
            .remove(&(DetachedKind::Video, id));
        active.remove(&id);
        self.video_progress.lock().unwrap().remove(&id);
        self.video_speed.lock().unwrap().remove(&id);
        if stopped {
            "stopped"
        } else if stalled {
            // Watchdog-killed mid-download: possibly-truncated bytes must not
            // classify as "completed" (a completed VOD archive may replace the
            // live capture) — surface as a retryable failure instead.
            "failed"
        } else if shutting_down {
            // We're quitting and killed the tree; treat any in-flight download as
            // incomplete regardless of how many bytes landed.
            "orphaned"
        } else if bytes > 0 && media_ok {
            "completed"
        } else {
            "failed"
        }
    }

    async fn download_video(&self, video: Video) {
        let id = video.id;
        let _permit = self.sem.acquire().await.expect("semaphore");

        // Cancelled (or shutting down) before we got a slot: finalize and bail.
        if self.stopping_videos.lock().unwrap().contains(&id)
            || self.shutdown.load(Ordering::SeqCst)
        {
            let status = self.finalize_video(id, 0, false, self.shutdown.load(Ordering::SeqCst));
            let _ = self
                .store
                .finish_video(id, now_unix(), 0, None, status, "", "");
            return;
        }

        let started_at = now_unix();
        let _ = self.store.set_video_started(id, started_at);

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
        let auth = resolve_auth_for(
            video.auth_kind,
            &video.auth_value,
            &global_method,
            &global_browser,
        );
        // Optionally resolve the real title + channel + id (for
        // {title}/{channel}/{video_id}/{name} and the list display).
        let (title, channel, mut video_id) = if video.auto_title {
            resolve_meta(&video, &auth).await
        } else {
            (String::new(), String::new(), String::new())
        };
        // Fall back to URL-extracted video ID so {video_id} is always filled when
        // the URL contains an explicit ID (YouTube watch?v=, youtu.be/, /live/ID).
        if video_id.is_empty() {
            video_id = extract_yt_video_id(&video.url).unwrap_or_default();
        }
        if !title.is_empty() && video.title.trim().is_empty() {
            let _ = self.store.set_video_title(id, &title);
        }
        if !channel.is_empty() {
            let _ = self.store.set_video_channel(id, &channel);
        }
        // Filename media-info ({resolution}/{fps}/…): pre-probe before download if
        // configured; the finished file is probed/renamed below for post modes.
        let media_mode = media_info_mode(&self.store);
        let want_media = template_wants_media(&video.filename_template);
        let pre_media = if want_media && media_mode.pre() {
            preprobe_media(video.tool, &video.url, &video.quality, &auth).await
        } else {
            None
        };
        let ytdlp_global_raw = self
            .store
            .get_setting("ytdlp_default_args")
            .ok()
            .flatten()
            .unwrap_or_default();
        let ytdlp_global_args = split_args(&ytdlp_global_raw);
        let ytdlp_bins = load_ytdlp_bins(&self.store);
        let plan = build_video_plan(
            &video, started_at, &title, &channel, &video_id, &auth, &ytdlp_global_args,
            pre_media.as_ref(), &ytdlp_bins,
        );
        if let Some(parent) = plan.capture_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, parent).await;
            set_cache_hidden(parent); // mark the working dir (or its central root) hidden
        }
        if let Some(out_dir) = plan.final_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, out_dir).await;
        }
        let label = if !video.title.trim().is_empty() {
            video.title.clone()
        } else if !title.is_empty() {
            title.clone()
        } else {
            video.url.clone()
        };
        info!(video = id, program = %plan.program, "downloading video -> {}", plan.final_path.display());

        let outcome = self
            .run_process(
                &self.active_videos,
                id,
                &plan,
                Some(self.video_progress.clone()),
                Some(self.video_speed.clone()),
                None, // on-demand downloads don't track ad breaks
                DetachReg {
                    kind: DetachedKind::Video,
                    ref_id: id,
                    monitor_id: None,
                    take_group: None,
                    started_at,
                    secondary: false,
                    stream_id: None,
                    went_live_at: None,
                },
            )
            .await;

        // Promote from .cache\ to the output dir. streamlink/ffmpeg remux .ts→.mkv;
        // yt-dlp already produced the (M)KV in .cache — but its extension may differ
        // from the predicted .mkv, so fall back to the newest {stem}.* in .cache\.
        let cache = plan.capture_path.parent().map(Path::to_path_buf);
        let capstem = plan
            .final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut final_path;
        if plan.remux_to_mkv {
            final_path = promote_capture(&self.store, &self.shutdown, &plan, &self.store.remux_opts(), None).await;
        } else {
            let produced = if file_len(&plan.capture_path).await > 0 {
                Some(plan.capture_path.clone())
            } else {
                newest_with_stem(&plan.capture_path).await
            };
            match produced {
                Some(src) => {
                    let dest = plan.final_path.with_file_name(
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
                        Ok(actual) => final_path = actual,
                        Err(e) => {
                            warn!(
                                video = id,
                                "promote move failed — the download is intact but stays in the \
                                 on-disk capture cache instead of the output dir: {e:#}"
                            );
                            final_path = src;
                        }
                    }
                }
                None => final_path = plan.capture_path.clone(),
            }
        }
        // Promoted iff the file now lives in the output dir (not still in .cache\).
        let promoted = final_path.parent() == plan.final_path.parent();
        if promoted {
            if let (Some(cache), Some(out_dir)) = (cache.as_deref(), final_path.parent()) {
                move_companions(cache, out_dir, &capstem).await;
            }
            // Post-capture: probe the finished file for actual media info and rename.
            if want_media && media_mode.post() {
                if let Some(mi) = probe_media(&final_path.to_string_lossy()).await {
                    let quality = resolved_quality(&video.quality);
                    let stem = video_stem(
                        &video, started_at, &title, &channel, &video_id, &quality, Some(&mi),
                        video.tool.label(), Platform::detect(&video.url).as_str(),
                    );
                    final_path = rename_for_media(final_path, &stem, &self.store).await;
                }
            }
            if let Some(cache) = cache.as_deref() {
                purge_cache(cache, &capstem).await;
            }
            // Embed subtitle sidecars (per `video.subtitle_tracks`) into the file
            // itself rather than leaving them beside it — unlike live recordings'
            // per-channel subdirs, every Video download lands in one flat folder,
            // where a `.en.vtt` next to the mkv is just clutter. No-ops when there
            // are none (e.g. subtitle_tracks was empty, or yt-dlp found none).
            // No overlap with the remux pass's own embedding: sidecars are a
            // yt-dlp-only output and yt-dlp video plans never set remux_to_mkv,
            // so a video reaches at most ONE embedding pass (audited 2026-07-10).
            if final_path.extension().and_then(|e| e.to_str()) == Some("mkv") {
                if let Err(e) = embed_subtitles_into_mkv(&final_path).await {
                    warn!(video = id, "embed subtitles failed: {e:#}");
                }
            }
        }

        let bytes = file_len(&final_path).await as i64;
        // A clean exit with a media-named file is trusted; a nonzero/unknown
        // exit must additionally prove itself to ffprobe (partial-but-playable
        // files stay "completed"-eligible, promoted logs never are).
        let exit_ok = matches!(outcome.exit_code, None | Some(0));
        let name_ok = final_path
            .file_name()
            .map(|n| plausible_media_output(&format!(".{}", n.to_string_lossy())))
            .unwrap_or(false);
        let media_ok = name_ok && (exit_ok || media_duration_secs(&final_path).await.is_some());
        // Decide status + drop the active_videos entry atomically so a concurrent
        // stop can't be lost (and its tombstone can't outlive this task).
        let status = self.finalize_video(id, bytes, media_ok, self.shutdown.load(Ordering::SeqCst));
        let _ = self.store.finish_video(
            id,
            now_unix(),
            bytes,
            outcome.exit_code,
            status,
            &final_path.to_string_lossy(),
            &outcome.log,
        );
        if status == "failed" {
            let _ = self.events.send(AppEvent::Error {
                context: "Video".into(),
                message: format!("{label}: download failed"),
            });
        }
        // If this download was a post-stream VOD archive, file it on the recording
        // (alongside) and optionally replace the live capture. No-op otherwise.
        self.finalize_vod_archive(id, &final_path, status).await;
        info!(video = id, bytes, status, "video download finished");
    }

    /// One-shot liveness check for a monitor, dispatched by detection method.
    async fn check_one(&self, row: &MonitorWithChannel) -> DetectOutcome {
        let item = DetectItem {
            monitor_id: row.monitor.id,
            url: row.monitor.url.clone(),
            platform: row.monitor.platform(),
        };
        match row.monitor.detection_method {
            // EventSub is push-only; check liveness now via Helix.
            DetectionMethod::TwitchApi
            | DetectionMethod::EventSub
            | DetectionMethod::EventSubHelix => self
                .ctx
                .detect_twitch(std::slice::from_ref(&item))
                .await
                .into_iter()
                .next()
                .unwrap_or_else(|| DetectOutcome {
                    monitor_id: item.monitor_id,
                    live: false,
                    detail: "no result".into(),
                    error: true,
                    went_live_at: None,
                    stream_id: None,
                    thumbnail_url: None,
                    broadcaster_id: None,
                    stream_title: None,
                    stream_game: None,
                    stream_viewers: None,
                    stream_followers: None,
                    stream_tags: None,
                    stream_language: None,
                    stream_game_id: None,
                    members_only: false,
                }),
            DetectionMethod::GenericProbe => self.ctx.detect_generic(&item).await,
            DetectionMethod::YouTubeApi => self.ctx.detect_youtube_api(&item).await,
            DetectionMethod::KickApi => self.ctx.detect_kick_api(&item).await,
            // No configured way to check — callers should avoid reaching this
            // (manual_start special-cases it), but never make a network call.
            DetectionMethod::Disabled => DetectOutcome {
                monitor_id: item.monitor_id,
                live: false,
                detail: "detection disabled for this instance".into(),
                error: false,
                went_live_at: None,
                stream_id: None,
                thumbnail_url: None,
                broadcaster_id: None,
                stream_title: None,
                stream_game: None,
                stream_viewers: None,
                stream_followers: None,
                stream_tags: None,
                stream_language: None,
                stream_game_id: None,
                members_only: false,
            },
            _ => self.ctx.detect_scrape(&item).await,
        }
    }

    fn in_backoff(&self, monitor_id: i64) -> bool {
        self.backoff
            .lock()
            .unwrap()
            .get(&monitor_id)
            .map(|b| Instant::now() < b.until)
            .unwrap_or(false)
    }

    pub(super) fn note_result(
        &self,
        monitor_id: i64,
        duration_secs: i64,
        ok: bool,
        po_token_rejected: bool,
        used_po_fallback: bool,
        gated: Gated,
    ) {
        let mut map = self.backoff.lock().unwrap();
        // Back off on any capture that produced no footage (bytes == 0), even one
        // that ran a while before dying: a long run that wrote nothing is still a
        // failure (e.g. a SABR from-start stall that downloads ~hundreds of MiB to
        // its cache, then crashes without finalizing the MKV). Without this such a
        // capture would re-spawn on the very next poll and tight-loop, re-fetching
        // the same opening segments forever.
        if ok {
            map.remove(&monitor_id);
        } else {
            let entry = map.entry(monitor_id).or_insert(BackoffEntry {
                fails: 0,
                until: Instant::now(),
                po_rejected: false,
            });
            entry.fails = entry.fails.saturating_add(1);
            entry.po_rejected = entry.po_rejected || po_token_rejected;
            // The escalating 5-15m PO cooldown is a last resort for when
            // there's nothing better to do than wait the wave out. When a
            // fallback client is configured and this take didn't use it yet,
            // retry on the ordinary short ladder instead — the fallback
            // client needs no PO token, so the retry is expected to succeed,
            // and every held-off minute is live-edge footage lost for good.
            let escalate_po = po_token_rejected
                && (used_po_fallback || sabr_po_fallback_client(&self.store).is_empty());
            let wait = failure_backoff_secs(entry.fails, duration_secs, escalate_po, gated);
            entry.until = Instant::now() + Duration::from_secs(wait);
            warn!(
                monitor_id,
                fails = entry.fails,
                wait,
                duration_secs,
                po_token_rejected,
                used_po_fallback,
                "recording captured nothing; backing off"
            );
        }
    }

    /// Whether this monitor's next capture attempt should swap in the
    /// PO-fallback client: its failure chain includes a rejected GVS PO
    /// token and no capture has succeeded since. (Whether a fallback client
    /// is actually configured is the caller's check — this is only the
    /// per-monitor state.)
    fn po_fallback_pending(&self, monitor_id: i64) -> bool {
        self.backoff
            .lock()
            .unwrap()
            .get(&monitor_id)
            .map(|b| b.po_rejected)
            .unwrap_or(false)
    }

    /// Swap the PO-fallback client into the loaded SABR preset when this
    /// monitor's previous take died to a rejected GVS PO token. The `tv`
    /// client (default) has no GVS PO-token policy in yt-dlp — no token is
    /// minted or attached, so a platform-side rejection wave can't touch the
    /// retry (verified live 2026-07-31: full-speed from-start SABR capture
    /// via tv while every web token was refused). Web stays the primary:
    /// the swap is per-take, and the next successful capture clears the
    /// state. Also marks the take in `po_fallback_takes` so finalize knows
    /// the fallback was already spent if the take still fails.
    pub(super) fn apply_po_fallback(&self, row: &MonitorWithChannel, bins: &mut YtDlpBins) {
        if row.monitor.platform() != Platform::YouTube
            || !bins.sabr.usable()
            || bins.sabr.po_fallback_client.is_empty()
            || !self.po_fallback_pending(row.monitor.id)
        {
            return;
        }
        bins.sabr.extractor_args =
            with_player_client(&bins.sabr.extractor_args, &bins.sabr.po_fallback_client);
        self.po_fallback_takes.lock().unwrap().insert(row.monitor.id);
        info!(
            monitor_id = row.monitor.id,
            client = %bins.sabr.po_fallback_client,
            "🎫 previous take's GVS PO token was rejected {} — capturing via the \
             no-token fallback client",
            row.monitor.platform().tag()
        );
    }

    /// Surface a PO-token rejection in the 🚨 Warnings window. Without this the
    /// only trace is a traceback buried in a per-capture log file: the take
    /// just reads "failed", which looks identical to a hundred other causes
    /// and sends you looking for a local misconfiguration that isn't there.
    /// One alert per take (`take_key`), so a retry ladder doesn't spam.
    pub(super) fn file_po_token_alert(
        &self,
        row: &MonitorWithChannel,
        monitor_id: i64,
        rec_id: i64,
        used_po_fallback: bool,
    ) {
        // What happens next depends on whether a PO-fallback client is still
        // in reserve: a prompt no-token retry, or the escalating cooldown.
        let fallback = sabr_po_fallback_client(&self.store);
        let next = if !fallback.is_empty() && !used_po_fallback {
            format!(
                "The next attempt retries promptly via the '{fallback}' fallback client, \
                 which doesn't use a GVS PO token at all."
            )
        } else {
            format!(
                "The next automatic attempt is held off with an escalating cooldown \
                 ({}-{}m) rather than retrying immediately.",
                PO_TOKEN_COOLDOWN_SECS / 60,
                PO_TOKEN_COOLDOWN_MAX_SECS / 60,
            )
        };
        let alert = crate::store::NewCaptureAlert {
            kind: "po_token_rejected".to_string(),
            severity: "error".to_string(),
            source: "capture".to_string(),
            take_key: format!("po_token:rec{rec_id}"),
            monitor_id: Some(monitor_id),
            recording_id: Some(rec_id),
            video_id: None,
            channel: row.channel.name.clone(),
            count: 1,
            lost_segments: 0,
            last_line: format!(
                "{} rejected this capture's GVS PO Token (stream protection status \
                 ATTESTATION_REQUIRED). The token server is working — it mints a fresh token \
                 per retry and the platform refuses each one — so this is a YouTube-side \
                 condition. {next}",
                row.monitor.platform().label(),
            ),
        };
        match self.store.upsert_capture_alert(&alert) {
            Ok((id, true)) => {
                info!(rec_id, "filed PO-token rejection alert #{id} for {}", row.channel.name)
            }
            Ok(_) => {}
            Err(e) => warn!(rec_id, "failed to file PO-token alert: {e:#}"),
        }
    }

    /// File the 🔒 "subscriber-only stream" alert — once per **broadcast**, not
    /// once per take.
    ///
    /// The take key is the stream id (falling back to the monitor when Twitch
    /// gave us none), so the ~dozen doomed takes one sub-only broadcast
    /// produces collapse into a single row that just increments its count,
    /// instead of a wall of "capture tool error" noise that buries real
    /// failures elsewhere in the feed.
    pub(super) fn file_sub_only_alert(
        &self,
        row: &MonitorWithChannel,
        monitor_id: i64,
        rec_id: i64,
        stream_id: Option<&str>,
    ) {
        let key = match stream_id.filter(|s| !s.is_empty()) {
            Some(sid) => format!("sub_only:{sid}"),
            None => format!("sub_only:mon{monitor_id}"),
        };
        let alert = crate::store::NewCaptureAlert {
            kind: "sub_only".to_string(),
            severity: "warning".to_string(),
            source: "capture".to_string(),
            take_key: key,
            monitor_id: Some(monitor_id),
            recording_id: Some(rec_id),
            video_id: None,
            channel: row.channel.name.clone(),
            count: 1,
            lost_segments: 0,
            last_line: match row.monitor.platform() {
                // Twitch gates the manifest but not the DVR segments, so the
                // broadcast is still archivable — just behind the live edge.
                Platform::Twitch => format!(
                    "This Twitch broadcast is subscriber-only and the connected account isn't \
                     entitled to it (UNAUTHORIZED_ENTITLEMENTS), so the live edge can't be \
                     captured. It is NOT lost: a CDN capture session is archiving the \
                     broadcast from its start, extending every few minutes — that copy lags \
                     the live edge, and the last minutes before the stream ends may be \
                     missing. Subscribing with the connected account would let it capture \
                     normally.",
                ),
                // YouTube gates the manifest itself: without the membership
                // there is nothing to fetch, so say so plainly rather than
                // implying something is being archived.
                _ => format!(
                    "This {} broadcast is members-only and the credentials in use don't hold \
                     that membership, so it can't be captured — unlike Twitch, there's no \
                     public CDN copy to fall back on. The broadcast is still recorded in the \
                     history (👁 seen live, not recorded). Point Settings → Accounts → \
                     Download authentication at a browser profile signed in with the \
                     membership and it will capture normally; retries are held off to every \
                     {}m meanwhile.",
                    row.monitor.platform().label(),
                    SUB_ONLY_COOLDOWN_SECS / 60,
                ),
            },
        };
        match self.store.upsert_capture_alert(&alert) {
            Ok((id, true)) => {
                info!(rec_id, "filed subscriber-only alert #{id} for {}", row.channel.name)
            }
            Ok(_) => {}
            Err(e) => warn!(rec_id, "failed to file subscriber-only alert: {e:#}"),
        }
    }

    /// Release this monitor's `self.active` reservation, logging why —
    /// `self.active` has been observed to desync from reality (the Layna
    /// incident, 2026-07-24: it silently lost track of a still-healthy
    /// recording, letting a second process start for the same monitor; see
    /// also the scheduler's own periodic consistency check in
    /// `scheduler.rs`). Logging every release builds a paper trail so a
    /// recurrence is diagnosable from the log instead of requiring file-size
    /// forensics days later.
    pub(super) fn release_active(&self, monitor_id: i64, reason: &str) {
        let had = self.active.lock().unwrap().remove(&monitor_id);
        debug!(monitor_id, reason, had_entry = had.is_some(), "active: released");
    }

    /// Whether `path`'s mtime is within `fresh_secs` of now — an independent
    /// liveness signal for the duplicate-recording safety net in `record`,
    /// the same "trust the file, not just our own bookkeeping" technique the
    /// stall watchdog already relies on (see `stall_sample`). `false` on any
    /// read error (missing file, clock skew) — the conservative direction,
    /// since callers treat `true` as "definitely still alive, refuse".
    async fn file_written_within(path: &str, fresh_secs: u64) -> bool {
        let Ok(meta) = crate::iomon::fs::metadata(Cat::FsProbe, Path::new(path)).await else {
            return false;
        };
        meta.modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|elapsed| elapsed.as_secs() < fresh_secs)
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        row: MonitorWithChannel,
        went_live_at: Option<i64>,
        approximate: bool,
        stream_id: Option<String>,
        thumbnail_url: Option<String>,
        broadcaster_id: Option<String>,
        stream_title: Option<String>,
        // Human description of the trigger-word match that started this
        // recording (empty when it started normally). Stored on the row.
        trigger_info: String,
        // The whole matched rule, frozen at start time — `None` when this
        // wasn't a trigger start. Drives stop-on-unmatch (meta_watcher) and
        // head-backfill leadtime; also persisted (as JSON) so a re-attach
        // after an app restart can recover it.
        trigger_rule: Option<crate::triggers::TriggerRule>,
    ) {
        let monitor_id = row.monitor.id;
        // Duplicate-recording safety net: `try_begin`'s `self.active` check a
        // moment ago found this monitor free, but `self.active` has been
        // observed to desync from reality (a confirmed incident: a monitor's
        // recording process stayed alive and healthy for hours while
        // `self.active` briefly lacked its entry, letting a second process
        // start for the same monitor — root cause not fully pinned down).
        // Cross-check against the DB independently of `self.active`: if this
        // monitor's last take is still open (no `ended_at`) AND its own
        // capture file was written to within the last few seconds, something
        // is still actively writing it right now — refuse rather than
        // silently doubling up. A merely *open* row with a STALE file (the
        // ordinary "crashed, not yet finalized" case) falls through
        // unchanged — this never blocks legitimate crash-recovery.
        const DUPLICATE_GUARD_FRESH_SECS: u64 = 60;
        if let Ok(Some(open)) = self.store.open_recording_for_monitor(monitor_id)
            && !open.output_path.is_empty()
            && Self::file_written_within(&open.output_path, DUPLICATE_GUARD_FRESH_SECS).await
        {
            warn!(
                monitor_id,
                open_rec_id = open.id,
                "record: an earlier take for this monitor is still actively writing (fresh \
                 capture file) — refusing to start a duplicate recording"
            );
            self.release_active(monitor_id, "record: duplicate-recording safety net refused this start (see the warning above)");
            return;
        }
        // This broadcast is about to get a real capture, whose own chat logger
        // owns the monitor's chat from here on. Any chat-only session running
        // for it (Auto was off; a trigger word or manual Start just overrode
        // that) has to be wound down FIRST — both platforms' loggers key their
        // bookkeeping on `monitor_id`, so two of them would fight over
        // `active_chats` and write a second, redundant sidecar. Done here, up
        // front, rather than next to `spawn_chat_loggers`: everything between
        // is auth probing and a semaphore wait, so the stop overlaps work that
        // was going to happen anyway.
        self.stop_chat_only(monitor_id).await;
        let trigger_rule_json = trigger_rule
            .as_ref()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .unwrap_or_default();
        // Per-stream key for the SABR stall maps. Fully per-stream when a video ID
        // is available (YouTube scrape / API); degrades to per-monitor when not.
        let sabr_key = (monitor_id, stream_id.clone());
        // SABR from-start fallback: prior attempts stalled "not near live head"
        // (DVR window expired, or persistent from-start stalls under deep-rewind).
        // Override capture_from_start so we capture the live edge this time instead
        // of stalling from the beginning again. Cleared when a capture succeeds
        // (bytes > 0).
        let mut row = row;
        let sabr_live_edge_fallback = row.monitor.capture_from_start
            && self.sabr_dvr_exceeded.lock().unwrap().contains(&sabr_key);
        if sabr_live_edge_fallback {
            row.monitor.capture_from_start = false;
            info!(
                monitor_id,
                "SABR from-start unavailable for {} {}; capturing live edge",
                Platform::YouTube.tag(),
                row.channel.name
            );
        }
        let (auth, media_mode, want_media, pre_media) =
            self.resolve_auth_and_preprobe(&row).await;

        let _permit = self.sem.acquire().await.expect("semaphore");
        // The probe + permit wait may have spanned a shutdown; don't start new work.
        if self.shutdown.load(Ordering::SeqCst) {
            self.release_active(monitor_id, "record: shutdown signaled while waiting on the auth probe/semaphore");
            return;
        }
        let (ytdlp_global_args, ytdlp_bins, started_at, plan) = self
            .build_record_plan(&row, &auth, &stream_id, &stream_title, &pre_media, went_live_at)
            .await;

        let (take_group, rec_id) = self.insert_recording_row(
            &row,
            monitor_id,
            started_at,
            &plan,
            went_live_at,
            approximate,
            &stream_id,
            &trigger_info,
            &trigger_rule_json,
        );
        if sabr_live_edge_fallback {
            let _ = self.store.set_sabr_live_edge_fallback(rec_id);
        }

        self.spawn_dash_companion(
            &row,
            &plan,
            &auth,
            &ytdlp_global_args,
            &ytdlp_bins,
            monitor_id,
            take_group.clone(),
            &stream_id,
            went_live_at,
            approximate,
            trigger_info,
            trigger_rule_json,
        );
        self.spawn_asset_fetches(&row, &plan, monitor_id, thumbnail_url, broadcaster_id);

        info!(
            monitor_id,
            program = %plan.program,
            "starting recording: {} {} -> {}",
            row.monitor.platform().tag(),
            row.channel.name,
            plan.capture_path.display()
        );
        {
            let redacted: Vec<String> = plan.args.iter().map(|a| {
                if a.contains("Authorization=OAuth ") {
                    let prefix = &a[..a.find("OAuth ").map(|i| i + 6).unwrap_or(a.len())];
                    format!("{prefix}<redacted>")
                } else {
                    a.clone()
                }
            }).collect();
            info!(monitor_id, "args: {}", redacted.join(" "));
        }

        let (from_start, resolve_lost, watcher_done, watcher) =
            self.spawn_catch_up_watcher(&row, &plan, monitor_id, rec_id, went_live_at);
        self.spawn_head_backfill(
            &row,
            &plan,
            &trigger_rule,
            monitor_id,
            rec_id,
            &stream_id,
            went_live_at,
            approximate,
            started_at,
        );
        let ad_sink = self.make_ad_sink(
            &row,
            &plan,
            monitor_id,
            rec_id,
            started_at,
            went_live_at,
            from_start,
        );
        let (meta_done, meta_task) =
            self.spawn_meta_watcher(&row, &trigger_rule, monitor_id, rec_id, started_at);
        self.spawn_quality_upgrade_watcher(&row, &plan, monitor_id, &stream_id);
        let (chat_done, chat_task) = self.spawn_chat_loggers(
            &row,
            &plan,
            &auth,
            &ytdlp_global_args,
            &ytdlp_bins,
            monitor_id,
            rec_id,
            stream_id.as_deref().unwrap_or(""),
        );

        // If a manual stop arrived while we were setting up (pid was 0 so kill
        // couldn't fire yet), honour it now: skip spawning the process entirely.
        let outcome = if self.stopping_monitors.lock().unwrap().contains(&monitor_id) {
            ProcessOutcome { exit_code: None, log: String::new() }
        } else {
            let mut outcome = self
                .run_process(
                    &self.active,
                    monitor_id,
                    &plan,
                    None,
                    None,
                    ad_sink,
                    DetachReg {
                        kind: DetachedKind::Recording,
                        ref_id: rec_id,
                        monitor_id: Some(monitor_id),
                        take_group: Some(take_group.clone()),
                        started_at,
                        secondary: false,
                        stream_id: stream_id.clone(),
                        went_live_at,
                    },
                )
                .await;
            // A from-start SABR capture can die from a transient local hiccup
            // (antivirus/backup briefly locking its `.state` checkpoint file —
            // 2026-07-16: a 2h15m/1.75GB Maid Mint capture died exactly this way
            // and nothing recovered it) without the stream itself ending. Retry
            // the identical take a few times — same `-o`, same `plan`, so yt-dlp's
            // own SABR resume continues from the surviving `.state` — before
            // giving up and letting it finalize as failed. `ad_sink` is always
            // `None` here (only Twitch+streamlink recordings get one), so it's
            // safe to reuse `None` across retries without cloning.
            const MAX_SABR_RETRIES: u32 = 3;
            const SABR_RETRY_DELAY: Duration = Duration::from_secs(5);
            // The cap guards against tight CRASH-LOOPS (girl_dm_'s dead POT
            // server: attempts dying ~40s apart), not against a long-lived
            // take accumulating occasional transients — so an attempt that
            // ran this long before dying refunds the whole budget. Without
            // this, a 2h41m Maid Mint take (2026-07-20) was finalized failed
            // by its 4th transient ever, two of whose attempts had each run
            // over an hour (deep-rewind segment mismatches after connection
            // resets on a DVR-disabled stream).
            const SABR_RETRY_REFUND_SECS: u64 = 600;
            let mut retries = 0;
            while retries < MAX_SABR_RETRIES
                && sabr_resumable_failure(
                    row.monitor.platform() == Platform::YouTube
                        && row.monitor.tool == Tool::YtDlp
                        && row.monitor.capture_from_start,
                    ytdlp_bins.sabr.usable(),
                    sabr_state_exists(&plan.final_path.to_string_lossy()),
                    &outcome.log,
                )
                && !self.stopping_monitors.lock().unwrap().contains(&monitor_id)
                && !self.shutdown.load(Ordering::SeqCst)
            {
                retries += 1;
                // Quote the dying attempt's error here: the tool's own log
                // file is truncated by the retry we're about to spawn, so this
                // line is the only durable record of what killed it.
                warn!(
                    monitor_id,
                    retries,
                    "SABR capture died with resumable state left behind; retrying same take {} — cause: {}",
                    Platform::YouTube.tag(),
                    log_death_reason(&outcome.log),
                );
                // Access-denied death: name the process holding the file
                // (Restart Manager) while its lock is likely still live —
                // the actionable output is "add this to the exclusion list".
                self.log_lock_culprits(&outcome.log, &plan.capture_path, monitor_id).await;
                // If it died for lack of a GVS PO token, the provider server is
                // down — retrying against it dead fails identically (observed
                // 2026-07-18: girl_dm_ burned all 3 retries per take for 20+
                // minutes). Bring the managed server up first so this retry
                // resumes the same take against a live one.
                if pot_token_failure(&outcome.log)
                    && !crate::pot_server::ensure_up(std::time::Duration::from_secs(30)).await
                {
                    warn!(monitor_id, "PO token server still unreachable; retrying anyway");
                }
                crate::app_core::sleep_cancellable(SABR_RETRY_DELAY, &self.shutdown).await;
                let attempt_started = std::time::Instant::now();
                outcome = self
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
                            take_group: Some(take_group.clone()),
                            started_at,
                            secondary: false,
                            stream_id: stream_id.clone(),
                            went_live_at,
                        },
                    )
                    .await;
                // The resumed attempt may have ended any way here (another
                // death, or the stream finishing cleanly) — either way a long
                // run means the take is healthy, not crash-looping.
                let ran = attempt_started.elapsed();
                if ran >= Duration::from_secs(SABR_RETRY_REFUND_SECS) {
                    info!(
                        monitor_id,
                        "SABR retry budget refunded — the resumed attempt ran {}m (occasional transience, not a crash loop)",
                        ran.as_secs() / 60
                    );
                    retries = 0;
                }
            }
            outcome
        };

        // The take is over (retries exhausted, or the failure wasn't same-take
        // resumable — e.g. a PO-token death at t=0 before any `.state` existed).
        // If it died for lack of a PO token, kick the server watchdog now so the
        // provider is healthy again before this monitor's ≥30s backoff expires
        // and the NEXT take succeeds instead of repeating the crash.
        if pot_token_failure(&outcome.log) {
            crate::pot_server::nudge();
        }

        Self::stop_record_watchers(watcher_done, watcher, meta_done, meta_task, chat_done, chat_task)
            .await;
        // Capture over, finalize begins — the promote below can sit in the disk-
        // gate queue for hours, so tell the UI this monitor is "finalizing", not
        // still recording. Crucially, FREE THE ACTIVE SLOT NOW: while a monitor
        // is in `active`, the scheduler skips polling it and try_begin refuses
        // new takes — holding it through a queued remux made a dropped-and-
        // restarted stream invisible for the whole wait (DougDoug, 2026-07-14:
        // his restart went uncaptured for 2+ h behind a 7 GB remux queue).
        self.finalizing.lock().unwrap().insert(monitor_id, rec_id);
        self.release_active(monitor_id, "record: capture tool process exited, finalize begins");
        self.ad_active.lock().unwrap().remove(&monitor_id);
        // Broadcast end ~= when the tool exited; snapshot it before remux so the
        // span (and thus lost-time) isn't inflated by remux duration.
        let ended = now_unix();

        let final_path = self
            .promote_and_rename(
                &row, &plan, monitor_id, rec_id, started_at, &stream_id, went_live_at,
                want_media, media_mode, &pre_media,
            )
            .await;

        let bytes = file_len(&final_path).await as i64;

        self.maybe_clear_lost_time(resolve_lost, went_live_at, &final_path, ended, rec_id)
            .await;

        let duration = now_unix() - started_at;
        let ok = bytes > 0;
        let manually_stopped = self.stopping_monitors.lock().unwrap().remove(&monitor_id);
        let shutting_down = self.shutdown.load(Ordering::SeqCst);
        let sabr_stall =
            self.note_sabr_stall(sabr_key, monitor_id, ok, manually_stopped, shutting_down, &outcome);
        // File the 🎫 alert BEFORE finalize: `finish_recording` files a
        // generic `capture_failed` error for any failed take that has no
        // error alert yet, so the more specific PO row must already exist or
        // the take ends up with both.
        // Whether THIS take ran with the PO-fallback client swapped in
        // (removed here unconditionally so the marker can't leak to the next
        // take).
        let used_po_fallback = self.po_fallback_takes.lock().unwrap().remove(&monitor_id);
        let po_rejected = !manually_stopped && !ok && po_token_rejected(&outcome.log);
        if po_rejected {
            self.file_po_token_alert(&row, monitor_id, rec_id, used_po_fallback);
        }
        // Subscriber-only stream we hold no entitlement for: the live edge is
        // simply not ours to capture, so this is a *state of the broadcast*
        // rather than a fault. It changes the retry cadence (see
        // `SUB_ONLY_COOLDOWN_SECS`) and files one 🔒 alert per broadcast
        // instead of a "capture tool error" per take.
        // Both platforms' "you aren't entitled to this broadcast" refusals
        // share a cadence and a badge; only Twitch has a CDN fallback to
        // hand the broadcast to afterwards.
        // The third arm is detection's knowledge rather than the tool's. A
        // members-only YouTube stream is INVISIBLE to an unauthenticated
        // yt-dlp — it reports "The channel is not currently live", which says
        // nothing about entitlement and matches neither refusal above. Our own
        // poll saw the stream (badged members-only on the /streams tab), so a
        // capture that then fails is a gated broadcast, not a transient error,
        // and must not be relaunched every few minutes for hours.
        let is_gated = !manually_stopped
            && !ok
            && (crate::models::sub_only_rejected(&outcome.log)
                || crate::models::members_only_rejected(&outcome.log)
                || self.store.monitor_members_only(monitor_id));
        // Twitch has somewhere to put a refused broadcast (the CDN segments);
        // nothing else does. That decides whether asking again is worth
        // anything, and so the retry cadence.
        let gated = match (is_gated, row.monitor.platform()) {
            (false, _) => Gated::No,
            (true, Platform::Twitch) => Gated::WithCdnFallback,
            (true, _) => Gated::NoFallback,
        };
        if is_gated {
            // Stamp the TAKE before finalizing it. The 🔒 alert below is keyed
            // by the broadcast (one Warnings row however many attempts it
            // takes), so it can name only one take — this flag is what lets
            // `finish_recording` suppress the red `capture_failed` for every
            // attempt, and the grid render 🔒 on every one of them.
            if let Err(e) = self.store.set_recording_gated(rec_id) {
                warn!(rec_id, "failed to mark take as gated: {e:#}");
            }
            self.file_sub_only_alert(&row, monitor_id, rec_id, stream_id.as_deref());
            // Hand the broadcast to a CDN capture session: it archives from the
            // segments Twitch does serve, incrementally, and holds the monitor
            // so nothing keeps asking the live edge for a stream it will only
            // refuse. See `sub_only.rs`.
            self.maybe_start_sub_only_session(
                &row,
                rec_id,
                stream_id.as_deref(),
                went_live_at,
                &final_path,
            );
        }
        self.finalize_recording(
            &row,
            monitor_id,
            rec_id,
            &outcome,
            &final_path,
            bytes,
            ok,
            manually_stopped,
            shutting_down,
            sabr_stall,
            went_live_at,
            approximate,
            ended,
        );

        // A manual stop already installed its own 120s cooldown (see `manual_stop`);
        // don't let the subprocess's exit clobber it — a 0-byte stopped capture would
        // otherwise reset the wait to 30s, and a captured one would clear it entirely,
        // either way re-triggering the moment the next LIVE signal arrives.
        if !manually_stopped {
            self.note_result(monitor_id, duration, ok, po_rejected, used_po_fallback, gated);
        }
        self.finalizing.lock().unwrap().remove(&monitor_id);
    }

    /// Resolve the effective auth source and, when the filename template wants
    /// media variables in a pre-probe mode, probe the stream before capture.
    async fn resolve_auth_and_preprobe(
        &self,
        row: &MonitorWithChannel,
    ) -> (AuthSource, MediaInfoMode, bool, Option<MediaInfo>) {
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
        let auth = resolve_auth(row, &global_method, &global_browser);
        // Filename media-info ({resolution}/{fps}/…): pre-probe the stream if the
        // template uses it and the mode asks for it. Do this BEFORE taking the
        // concurrency permit (so a slow probe can't block other recordings) and
        // BEFORE the start timestamp (so it reflects when capture actually begins).
        // The finished file is probed again (and renamed) below for post modes.
        let media_mode = media_info_mode(&self.store);
        let want_media = template_wants_media(&row.monitor.filename_template);
        let pre_media = if want_media && media_mode.pre() {
            preprobe_media(row.monitor.tool, &row.monitor.url, &row.monitor.quality, &auth).await
        } else {
            None
        };
        (auth, media_mode, want_media, pre_media)
    }

    /// Build the download plan for a new take and ensure its working (`.cache\`)
    /// and output directories exist.
    async fn build_record_plan(
        &self,
        row: &MonitorWithChannel,
        auth: &AuthSource,
        stream_id: &Option<String>,
        stream_title: &Option<String>,
        pre_media: &Option<MediaInfo>,
        went_live_at: Option<i64>,
    ) -> (Vec<String>, YtDlpBins, i64, DownloadPlan) {
        let ytdlp_global_raw = self
            .store
            .get_setting("ytdlp_default_args")
            .ok()
            .flatten()
            .unwrap_or_default();
        let ytdlp_global_args = split_args(&ytdlp_global_raw);
        let mut ytdlp_bins = load_ytdlp_bins(&self.store);
        self.apply_po_fallback(row, &mut ytdlp_bins);
        let started_at = now_unix();
        let plan = build_plan(row, started_at, auth, &ytdlp_global_args, stream_id.as_deref(), stream_title.as_deref().unwrap_or(""), pre_media.as_ref(), went_live_at.unwrap_or(0), &ytdlp_bins);
        if let Some(parent) = plan.capture_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, parent).await;
            set_cache_hidden(parent); // mark the working dir (or its central root) hidden
        }
        // Also ensure the output dir exists (the final file is promoted there).
        if let Some(out_dir) = plan.final_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, out_dir).await;
        }
        (ytdlp_global_args, ytdlp_bins, started_at, plan)
    }

    /// Insert the recording row and emit the recording-started events.
    /// Returns `(take_group, rec_id)`.
    #[allow(clippy::too_many_arguments)]
    fn insert_recording_row(
        &self,
        row: &MonitorWithChannel,
        monitor_id: i64,
        started_at: i64,
        plan: &DownloadPlan,
        went_live_at: Option<i64>,
        approximate: bool,
        stream_id: &Option<String>,
        trigger_info: &str,
        trigger_rule_json: &str,
    ) -> (String, i64) {
        // A real capture is starting for this monitor — if a "seen live but
        // not recorded" session (see `insert_not_recorded_session`) was still
        // open for it (e.g. Auto just got turned on, or a trigger matched
        // mid-broadcast after Auto-off had already opened one), close it now
        // rather than leaving it open forever: the scheduler stops polling
        // (and thus stops seeing "went offline") for any monitor that's
        // actively recording.
        let _ = self.store.close_open_not_recorded_sessions(monitor_id, started_at);
        // A take key links the recordings of this capture attempt: the primary
        // and, in dual capture, the DASH companion share it (they're one "take").
        let take_group = format!("{monitor_id}:{started_at}");
        let rec_id = self
            .store
            .insert_recording(
                monitor_id,
                started_at,
                &plan.final_path.to_string_lossy(),
                went_live_at,
                approximate,
                stream_id.as_deref(),
                Some(&take_group),
                trigger_info,
                trigger_rule_json,
            )
            .unwrap_or(0);
        // Rolling mode is resolved ONCE here and frozen onto the take, the same
        // way `trigger_rule_json` is: a later settings change must not re-time
        // (or newly endanger) a take that is already recorded. See
        // `crate::rolling`.
        if let Some(ttl) = crate::disposal::effective_rolling(&self.store, row.channel.id, monitor_id)
        {
            let _ = self.store.set_recording_rolling_ttl(rec_id, ttl);
        }
        let _ = self
            .store
            .set_monitor_check_result(monitor_id, "recording", started_at);
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "recording".into(),
        });
        // Compute the expected thumbnail path before the fire-and-forget fetch below
        // so the notification handler can find it (file may not exist yet).
        let toast_thumbnail = (row.monitor.fetch_thumbnail && !plan.writes_own_thumbnail)
            .then(|| plan.capture_path.with_extension("thumbnail.jpg"));
        let _ = self.events.send(AppEvent::RecordingStarted {
            monitor_id,
            recording_id: rec_id,
            channel: row.channel.name.clone(),
            thumbnail_path: toast_thumbnail,
        });
        (take_group, rec_id)
    }

    /// Spawn the DASH companion capture when dual capture applies to this take.
    #[allow(clippy::too_many_arguments)]
    fn spawn_dash_companion(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        auth: &AuthSource,
        ytdlp_global_args: &[String],
        ytdlp_bins: &YtDlpBins,
        monitor_id: i64,
        take_group: String,
        stream_id: &Option<String>,
        went_live_at: Option<i64>,
        approximate: bool,
        trigger_info: String,
        trigger_rule_json: String,
    ) {
        // Dual capture: also run a DASH companion via the system yt-dlp for formats
        // that only DASH carries. It captures from the live edge (SABR owns
        // from-start), writes a sibling `{stem}.dash.mkv`, and finalizes as its own
        // recording sharing this take. Only meaningful when SABR drives the primary.
        if row.monitor.dual_capture
            && row.monitor.platform() == Platform::YouTube
            && row.monitor.capture_from_start
            && ytdlp_bins.sabr.usable()
        {
            let dash_plan = build_dash_companion_plan(
                &plan.final_path,
                row,
                auth,
                ytdlp_global_args,
                &ytdlp_bins.system_program(),
                &load_dash_format(&self.store),
                &ytdlp_bins.sabr.pot_args,
            );
            let this = self.clone();
            let tg = take_group.clone();
            let sid = stream_id.clone();
            let cname = row.channel.name.clone();
            let tinfo = trigger_info.clone();
            let trule_json = trigger_rule_json.clone();
            tokio::spawn(async move {
                this.run_dash_companion(
                    monitor_id, dash_plan, tg, sid, went_live_at, approximate, cname, tinfo,
                    trule_json,
                )
                .await;
            });
        }
    }

    /// Fire-and-forget asset fetches for a new take (stream thumbnail over HTTP
    /// when the tool doesn't write its own, plus channel assets).
    fn spawn_asset_fetches(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        monitor_id: i64,
        thumbnail_url: Option<String>,
        broadcaster_id: Option<String>,
    ) {
        // Asset fetching — fire-and-forget tasks that don't block the recording.
        // Normal yt-dlp writes its own thumbnail inline (`--write-thumbnail`); for
        // streamlink and SABR captures (which don't) we fetch it over HTTP instead.
        if row.monitor.fetch_thumbnail && !plan.writes_own_thumbnail {
            if let Some(ref url) = thumbnail_url {
                let http = self.ctx.http_client();
                let url = url.clone();
                // Into the .cache\ working dir; promoted up with the recording on
                // success (and dropped with it if the capture fails).
                let dest = plan.capture_path.with_extension("thumbnail.jpg");
                let task_id = crate::events::next_task_id();
                let task_label = row.channel.name.clone();
                let _ = self.events.send(AppEvent::BackgroundTaskStarted(
                    crate::events::BackgroundTask {
                        id: task_id,
                        kind: crate::events::BackgroundTaskKind::ThumbnailFetch,
                        label: task_label,
                        detail: "stream thumbnail".into(),
                        started_at: crate::models::now_unix(),
                        progress: None,
                progress_info: None,
                    },
                ));
                let tx = self.events.clone();
                tokio::spawn(async move {
                    let outcome = match crate::assets::fetch_stream_thumbnail(&http, &url, &dest).await {
                        Ok(_) => crate::events::TaskOutcome::Completed,
                        Err(e) => {
                            tracing::warn!(monitor_id, "thumbnail fetch failed: {e}");
                            crate::events::TaskOutcome::Failed(e.to_string())
                        }
                    };
                    let _ = tx.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
                });
            }
        }
        if row.monitor.fetch_chat_assets {
            self.fetch_channel_assets(row, broadcaster_id.clone(), false);
        }
    }

    /// Spawn the DVR catch-up watcher for capture-from-start takes.
    /// Returns `(from_start, resolve_lost, watcher_done, watcher)`.
    fn spawn_catch_up_watcher(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        monitor_id: i64,
        rec_id: i64,
        went_live_at: Option<i64>,
    ) -> (bool, bool, Arc<AtomicBool>, Option<tokio::task::JoinHandle<()>>) {
        // When capturing from the start of the broadcast (live-from-start /
        // hls-live-restart), the early footage isn't lost — it's pulled from the
        // DVR. Watch the growing capture and zero out "lost time" once it catches
        // up to the live edge; finalize then recomputes the exact residual (in
        // case the stream ends before catch-up completes).
        let from_start = row.monitor.capture_from_start
            && matches!(row.monitor.tool, Tool::Streamlink | Tool::YtDlp);
        let resolve_lost = from_start && went_live_at.is_some();
        let watcher_done = Arc::new(AtomicBool::new(false));
        let watcher = resolve_lost.then(|| {
            tokio::spawn(catch_up_watcher(
                self.store.clone(),
                self.events.clone(),
                monitor_id,
                row.monitor.platform(),
                rec_id,
                plan.capture_path.clone(),
                went_live_at.unwrap_or(0),
                watcher_done.clone(),
            ))
        });
        (from_start, resolve_lost, watcher_done, watcher)
    }

    /// Spawn the Twitch head-backfill job when this take rewinds to the
    /// broadcast start (or a trigger leadtime asks for a short lead-in).
    #[allow(clippy::too_many_arguments)]
    fn spawn_head_backfill(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        trigger_rule: &Option<crate::triggers::TriggerRule>,
        monitor_id: i64,
        rec_id: i64,
        stream_id: &Option<String>,
        went_live_at: Option<i64>,
        approximate: bool,
        started_at: i64,
    ) {
        // Trigger-configured backfill leadtime (0/absent = off) — a fixed,
        // short lead-in buffer before this take started, for a mid-broadcast
        // trigger start (e.g. one GDQ segment) rather than the whole missed
        // stream. Independent of the monitor's own capture_from_start.
        let lead_secs: Option<i64> = trigger_rule
            .as_ref()
            .map(|r| r.lead_secs)
            .filter(|&l| l > 0);

        // Twitch capture-from-start: streamlink's --hls-live-restart only rewinds
        // within its own DVR view and usually misses. The published VOD's
        // playlist, however, already exists on the CDN and grows while the
        // stream is live — a backfill job downloads the missed head from it
        // (pre-mute originals!) and the post-stream concat joins head + live.
        // A configured trigger leadtime also spawns this job even when
        // capture_from_start itself is off — that flag drives unrelated
        // behavior (launch args, the catch-up watcher above, dual capture,
        // SABR stall handling) a "just this segment" user doesn't want, so
        // leadtime is checked independently rather than implied by it.
        if rec_id != 0
            && row.monitor.platform() == Platform::Twitch
            && (row.monitor.capture_from_start || lead_secs.is_some())
            && let (Some(sid), Some(wl)) = (stream_id.clone(), went_live_at)
        {
            let this = self.clone();
            let capture = plan.capture_path.clone();
            let final_p = plan.final_path.clone();
            let url = row.monitor.url.clone();
            let channel = row.channel.name.clone();
            let channel_id = row.channel.id;
            // Mark pending immediately (before the job's own settle wait) so
            // the Streams grid shows "queued" from the very start instead of
            // going quiet for the first ~2 minutes.
            let _ = self.store.set_head_backfill_state(rec_id, "queued");
            tokio::spawn(async move {
                this.head_backfill_job(
                    monitor_id, channel_id, rec_id, capture, final_p, url, channel, sid, wl,
                    approximate, started_at, None, false, lead_secs, None,
                )
                .await;
            });
        }
    }

    /// Build the ad-break sink for Twitch+streamlink takes (None otherwise).
    #[allow(clippy::too_many_arguments)]
    fn make_ad_sink(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        monitor_id: i64,
        rec_id: i64,
        started_at: i64,
        went_live_at: Option<i64>,
        from_start: bool,
    ) -> Option<AdSink> {
        // Twitch+streamlink filters ads into hard cuts and logs each break; record
        // them so the UI can show ad count/time and the cut timestamps. Skip when
        // the recording row failed to insert (rec_id 0) — an ad break with a 0
        // recording_id would violate the FK and be dropped anyway.
        (rec_id != 0
            && row.monitor.tool == Tool::Streamlink
            && row.monitor.platform() == Platform::Twitch)
            .then(|| AdSink {
                store: self.store.clone(),
                events: self.events.clone(),
                monitor_id,
                recording_id: rec_id,
                started_at,
                went_live_at,
                from_start,
                capture_path: plan.capture_path.clone(),
                ad_active: self.ad_active.clone(),
                login: crate::detectors::twitch_login(&row.monitor.url),
            })
    }

    /// Spawn the title/game metadata watcher for the take.
    /// Returns `(meta_done, meta_task)`.
    fn spawn_meta_watcher(
        &self,
        row: &MonitorWithChannel,
        trigger_rule: &Option<crate::triggers::TriggerRule>,
        monitor_id: i64,
        rec_id: i64,
        started_at: i64,
    ) -> (Arc<AtomicBool>, Option<tokio::task::JoinHandle<()>>) {
        // Log title / game-category changes during the take (the scheduler pauses
        // normal polling while recording, so poll the source directly). Supported
        // for Twitch (Helix), Kick (v2 JSON), and YouTube (/live scrape); no-ops
        // gracefully when the source is unavailable. Generic URLs have no source.
        let meta_platform = row.monitor.platform();
        let meta_done = Arc::new(AtomicBool::new(false));
        // Only armed when the matched rule opted into "only recording while
        // matching" — a trigger with e.g. just a leadtime configured (but
        // stop_on_unmatch off) still records until the stream ends, unchanged.
        let stop_rule = trigger_rule.clone().filter(|r| r.stop_on_unmatch);
        let meta_task = (rec_id != 0 && meta_platform.has_stream_meta()).then(|| {
            tokio::spawn(meta_watcher(
                self.ctx.clone(),
                self.store.clone(),
                self.events.clone(),
                monitor_id,
                rec_id,
                started_at,
                row.monitor.url.clone(),
                meta_platform,
                meta_done.clone(),
                self.shutdown.clone(),
                self.manual_tx.clone(),
                stop_rule,
                row.last_title.clone(),
                row.last_game.clone(),
                row.last_tags.clone(),
            ))
        });
        (meta_done, meta_task)
    }

    /// Spawn the restart-at-better-quality watcher when it applies to this take.
    fn spawn_quality_upgrade_watcher(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        monitor_id: i64,
        stream_id: &Option<String>,
    ) {
        // Restart-at-better-quality watcher: a Twitch capture that joins
        // seconds after go-live often sees only transcodes (the source
        // rendition is listed late) and locks onto e.g. 720p60 while the
        // stream is really 1080p60 — which is also why its head backfill
        // (always source) can't join it. Only for `best`-quality streamlink
        // captures; once per stream; default on (K_QUALITY_UPGRADE).
        if row.monitor.tool == Tool::Streamlink
            && row.monitor.platform() == Platform::Twitch
            && resolved_quality(&row.monitor.quality) == "best"
            && self
                .store
                .get_setting(K_QUALITY_UPGRADE)
                .ok()
                .flatten()
                .as_deref()
                != Some("0")
        {
            let this = self.clone();
            let key = format!("{monitor_id}:{}", stream_id.clone().unwrap_or_default());
            let url = row.monitor.url.clone();
            let capture = plan.capture_path.clone();
            let channel = row.channel.name.clone();
            tokio::spawn(async move {
                this.quality_upgrade_watcher(monitor_id, key, url, capture, channel).await;
            });
        }
    }

    /// Spawn the chat loggers for the take (native Twitch IRC logger and/or the
    /// yt-dlp live-chat sidecar). Returns `(chat_done, chat_task)`.
    #[allow(clippy::too_many_arguments)]
    fn spawn_chat_loggers(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        auth: &AuthSource,
        ytdlp_global_args: &[String],
        ytdlp_bins: &YtDlpBins,
        monitor_id: i64,
        rec_id: i64,
        stream_id: &str,
    ) -> (Arc<AtomicBool>, Option<tokio::task::JoinHandle<()>>) {
        // Twitch chat -> a native anonymous IRC-over-WebSocket logger, written as
        // a `.chat.jsonl` sidecar in the OUTPUT dir (next to the final file, not
        // in .cache\) so it isn't promoted/purged from under a still-writing
        // logger — or, with a dedicated chat root configured, in that root's
        // mirror of the output dir (chat I/O off the recording drive; see
        // `chat::chat_dir_for`). It follows the file's stem on the post-rename
        // either way. `recording.chat_path` is persisted up front so readers
        // and renames never have to guess which layout a take used. Twitch only.
        let chat_done = Arc::new(AtomicBool::new(false));
        let chat_task = (row.monitor.chat_log && row.monitor.platform() == Platform::Twitch)
            .then(|| {
                let chat_path =
                    crate::chat::chat_sidecar_path(&plan.final_path.with_extension("chat.jsonl"));
                if let Err(e) = self
                    .store
                    .set_recording_chat_path(rec_id, &chat_path.to_string_lossy())
                {
                    warn!("set_recording_chat_path (twitch take): {e}");
                }
                // Register the in-process logger in `active_chats` (pid 0, the
                // chat-only convention) so a recording Twitch row gets the same
                // 💬 badge, "Stop chat download" action, and shutdown accounting
                // as a YouTube recording's external sidecar — before this, only
                // YouTube recordings and chat-only sessions ever showed chat as
                // running (2026-08-01). The guard travels INSIDE the spawned
                // task so cleanup runs however the task ends — normal exit,
                // `chat_done`, or the force-abort in `stop_record_watchers` —
                // and its Arc::ptr_eq ownership check keeps an old take's
                // teardown from unregistering the logger a newer take of the
                // same monitor already owns.
                self.active_chats.lock().unwrap().insert(monitor_id, 0);
                self.take_chat_done.lock().unwrap().insert(monitor_id, chat_done.clone());
                let _ = self.events.send(AppEvent::MonitorState {
                    monitor_id,
                    state: "chat_active".into(),
                });
                let guard = TakeChatGuard {
                    active_chats: self.active_chats.clone(),
                    take_chat_done: self.take_chat_done.clone(),
                    stopping_chats: self.stopping_chats.clone(),
                    monitor_id,
                    flag: chat_done.clone(),
                };
                // Under a dedicated chat root the mirror dir may not exist yet
                // (the capture pipeline only creates the OUTPUT dir); ChatSink
                // opens lazily but never mkdirs.
                let mkdir = crate::chat::chat_root()
                    .is_some()
                    .then(|| chat_path.parent().map(std::path::Path::to_path_buf))
                    .flatten();
                let fut = crate::chat::log_twitch_chat(
                    row.monitor.url.clone(),
                    chat_path,
                    chat_done.clone(),
                    self.shutdown.clone(),
                    // Live event capture (subs/bits/raids -> stream_event).
                    Some(crate::chat::ChatEventCtx {
                        store: self.store.clone(),
                        monitor_id,
                        stream_id: stream_id.to_string(),
                        events: self.events.clone(),
                    }),
                );
                tokio::spawn(async move {
                    let _guard = guard;
                    if let Some(d) = mkdir
                        && let Err(e) =
                            crate::iomon::fs::create_dir_all(crate::iomon::Cat::DirSetup, &d).await
                    {
                        warn!("chat root: create {}: {e}", d.display());
                    }
                    fut.await;
                })
            });

        // YouTube chat -> separate yt-dlp sidecar process with --skip-download
        // --sub-langs=live_chat. Runs concurrently with (and outlives) the video
        // recording so the video download is never blocked by the chat stream.
        // Visible in the UI as a "Chat ●" indicator; user can stop it independently.
        if row.monitor.chat_log
            && row.monitor.tool == Tool::YtDlp
            && row.monitor.platform() != Platform::Twitch
        {
            // Base the YouTube chat sidecar on the final (output-dir) path, not the
            // .cache\ capture: this process outlives the video, so its
            // `.live_chat.json` must not be promoted/purged mid-write. With a
            // dedicated chat root, the base is that root's mirror of the final
            // path instead — yt-dlp's `--write-subs` REPLACES the `-o` value's
            // extension with the subtitle's own (verified live 2026-08-04,
            // yt-dlp stable@2026.06.09: `-o ....mkv` -> `....live_chat.json` on
            // disk, NOT `....mkv.live_chat.json` — an older assumption here had
            // it appending instead, which persisted a chat_path matching no
            // real file and broke lookup for every YouTube sidecar since).
            let chat_base = crate::chat::chat_sidecar_path(&plan.final_path);
            // Persist the predicted sidecar name (same prediction chat_only.rs
            // makes): `{-o value with its extension replaced}.live_chat.json`.
            if let Err(e) = self.store.set_recording_chat_path(
                rec_id,
                &chat_base.with_extension("live_chat.json").to_string_lossy(),
            ) {
                warn!("set_recording_chat_path (yt chat take): {e}");
            }
            let mkdir = crate::chat::chat_root()
                .is_some()
                .then(|| chat_base.parent().map(std::path::Path::to_path_buf))
                .flatten();
            let chat_plan = build_chat_plan(row, &chat_base, auth, ytdlp_global_args, &ytdlp_bins.system_program());
            let this = self.clone();
            let mid = monitor_id;
            let chat_platform = row.monitor.platform();
            tokio::spawn(async move {
                if let Some(d) = mkdir
                    && let Err(e) =
                        crate::iomon::fs::create_dir_all(crate::iomon::Cat::DirSetup, &d).await
                {
                    warn!("chat root: create {}: {e}", d.display());
                }
                this.run_chat_download(mid, chat_platform, chat_plan).await;
            });
        }
        (chat_done, chat_task)
    }

    /// Stop the per-take watchers once the capture process has exited.
    async fn stop_record_watchers(
        watcher_done: Arc<AtomicBool>,
        watcher: Option<tokio::task::JoinHandle<()>>,
        meta_done: Arc<AtomicBool>,
        meta_task: Option<tokio::task::JoinHandle<()>>,
        chat_done: Arc<AtomicBool>,
        chat_task: Option<tokio::task::JoinHandle<()>>,
    ) {
        /// How long the chat logger gets to notice `chat_done` and flush/exit
        /// on its own before it's force-aborted (see the comment below).
        const CHAT_STOP_GRACE: Duration = Duration::from_secs(10);
        // Stop the catch-up watcher before we touch the capture file (so it can't
        // race finalize's authoritative lost-time write). Abort rather than wait:
        // the watcher only checks its done flag at the start of each sleep tick, so
        // a mid-ffprobe call would otherwise block here for several seconds.
        watcher_done.store(true, Ordering::SeqCst);
        if let Some(w) = watcher {
            w.abort();
            let _ = w.await;
        }
        // Same for the metadata watcher: it only checks `done` between API poll
        // cycles, so if it's mid-request (youtube_stream_meta scrapes a full page,
        // twitch_stream_meta hits Helix) we'd stall here for up to 30 s — keeping
        // the monitor in `active` and the UI stuck on "Stop recording" even though
        // the process has already exited. Abort cancels the in-flight request
        // immediately; no finalized insert can race because the task is gone.
        meta_done.store(true, Ordering::SeqCst);
        if let Some(t) = meta_task {
            t.abort();
            let _ = t.await;
        }
        // Stop the chat logger and let it flush/close its sidecar before we touch
        // the capture file (the post-rename moves the .chat.jsonl alongside it).
        // Bounded grace period, NOT an unconditional wait: `log_twitch_chat`
        // checks its `done` flag reliably in the steady state, but a half-dead
        // WebSocket (TCP black-holed after a network blip — no clean close, no
        // read error) could leave an in-flight write hung indefinitely with
        // nothing here to time it out — and since this join has no abort
        // fallback, that would wedge this WHOLE function forever: `active`
        // never gets freed (below), the DB row never leaves "recording", and
        // the monitor can never be re-recorded. Confirmed live (2026-07-23):
        // girl_dm_ and Nihmune both stuck for hours/days this exact way. Give
        // it CHAT_STOP_GRACE to exit cleanly like the comment above always
        // intended; abort past that — a truncated last line or two in the
        // sidecar beats a permanently wedged monitor.
        chat_done.store(true, Ordering::SeqCst);
        if let Some(mut t) = chat_task
            && tokio::time::timeout(CHAT_STOP_GRACE, &mut t).await.is_err()
        {
            warn!("chat logger didn't stop within {CHAT_STOP_GRACE:?} — aborting it");
            t.abort();
            let _ = t.await;
        }
    }

    /// Promote the finished capture out of `.cache\`, apply the post-capture
    /// rename (media vars / games / title), and purge working leftovers.
    /// Returns the final path.
    #[allow(clippy::too_many_arguments)]
    async fn promote_and_rename(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        monitor_id: i64,
        rec_id: i64,
        started_at: i64,
        stream_id: &Option<String>,
        went_live_at: Option<i64>,
        want_media: bool,
        media_mode: MediaInfoMode,
        pre_media: &Option<MediaInfo>,
    ) -> PathBuf {
        // Promote the finished capture from the hidden `.cache\` up to the output
        // dir (remux .ts→.mkv, or move an already-final container); a failed/0-byte
        // capture is left in `.cache\` for the startup sweep. The raw `.ts`'s
        // first PTS must be saved before the remux resets timestamps.
        persist_capture_start_pts(&self.store, rec_id, &plan.capture_path).await;
        let mut final_path = promote_capture(
            &self.store,
            &self.shutdown,
            plan,
            &remux_opts_for_recording(&self.store, rec_id),
            Some((self.events.clone(), rec_id as u64)),
        )
        .await;
        let promoted = final_path != plan.capture_path;
        let cache = plan.capture_path.parent().map(Path::to_path_buf);
        // The capture stem (== final stem before any post-rename) used to match this
        // recording's files within `.cache\`.
        let capstem = plan
            .final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if promoted {
            // Promote subtitle/thumbnail companions up next to the video (chat
            // sidecars are already written in the output dir).
            if let (Some(cache), Some(out_dir)) = (cache.as_deref(), final_path.parent()) {
                move_companions(cache, out_dir, &capstem).await;
            }
            // Post-capture: fill in the filename bits that are only known now and
            // rename. `{resolution}/{fps}/…` come from probing the finished file;
            // `{games}` and `{title}` are only fully known after the stream ends, and
            // also trigger a rename even when probing is off.
            let want_games = template_wants_games(&row.monitor.filename_template);
            let want_title = template_wants_title(&row.monitor.filename_template);
            let want_went_live = template_wants_went_live(&row.monitor.filename_template);
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
                // Prefer the post-probe; fall back to the pre-probe so a {games}
                // rename in pre-probe mode doesn't drop already-resolved media vars.
                let stem = monitor_stem(
                    &row.monitor,
                    &row.channel.name,
                    started_at,
                    stream_id.as_deref(),
                    &title,
                    row.recording_count,
                    &quality,
                    mi.as_ref().or(pre_media.as_ref()),
                    &games,
                    row.monitor.tool.label(),
                    &plan.mode,
                    row.monitor.platform().as_str(),
                    went_live_at.unwrap_or(0),
                );
                // Stop the YouTube chat sidecar before renaming so its open
                // live_chat.json handle is released before companion rename.
                self.stop_and_wait_for_chat(monitor_id, Duration::from_secs(6)).await;
                final_path = rename_for_media(final_path, &stem, &self.store).await;
            }
            // Drop this recording's working leftovers (SABR parts/state, etc.).
            if let Some(cache) = cache.as_deref() {
                purge_cache(cache, &capstem).await;
            }
        }
        final_path
    }

    /// Zero out "lost time" when the finished capture spans the whole broadcast.
    async fn maybe_clear_lost_time(
        &self,
        resolve_lost: bool,
        went_live_at: Option<i64>,
        final_path: &Path,
        ended: i64,
        rec_id: i64,
    ) {
        // Conclude "no footage missed" only when the capture actually spans the
        // whole broadcast (reached the live edge with the head intact). If it
        // ended before catching up (stopped/crashed/stream ended early), the gap
        // is the not-yet-downloaded *tail*, not missed *beginning* — so don't
        // record it as Lost time; leave it unset and let the UI fall back to the
        // provisional `started - went_live` estimate.
        if resolve_lost {
            if let (Some(wl), Some(captured)) =
                (went_live_at, media_duration_secs(final_path).await)
            {
                let span = (ended - wl).max(0);
                if captured + CATCHUP_TOLERANCE_SECS >= span {
                    let _ = self.store.set_recording_lost_secs(rec_id, 0);
                }
            }
        }
    }

    /// Track SABR from-start stalls (and the live-edge fallback flag) for this
    /// take's outcome. Returns whether this outcome was a SABR from-start stall.
    ///
    /// `pub(super)`, not private: `process.rs`'s `resume_recording` (a SABR
    /// resume of an interrupted from-start capture, not a fresh one) calls
    /// this too — its stalls are exactly the same "not near live head" DVR-
    /// window class and must count toward the same live-edge fallback
    /// threshold, or a persistently-stalling broadcast that keeps dying via
    /// the resume path (rather than a fresh capture) never trips the
    /// fallback and retries from-start forever.
    pub(super) fn note_sabr_stall(
        &self,
        sabr_key: (i64, Option<String>),
        monitor_id: i64,
        ok: bool,
        manually_stopped: bool,
        shutting_down: bool,
        outcome: &ProcessOutcome,
    ) -> bool {
        // SABR from-start stall ("not near live head"): YouTube only serves the
        // last ~4 hours of a live stream via SABR, so once a stream is older than
        // its DVR window each from-start attempt downloads the opening segments
        // then stalls. The next attempt should fall back to live-edge capture (see
        // override at top of fn) so we at least record the ongoing stream.
        //
        // With deep-rewind OFF this is a true window expiry — fall back on the very
        // first stall. With deep-rewind ON the flag extends the window, so an early
        // stall *might* be transient; tolerate a few consecutive stalls before
        // giving up. (Empirically a persistent stall repeats every attempt — each
        // re-fetching ~hundreds of MiB before dying — so without a bound we'd never
        // fall back and never record anything.)
        let deep_rewind = setting_str(&self.store, "ytdlp_sabr_deep_rewind") == "1";
        let sabr_stall = !ok
            && !manually_stopped
            && !shutting_down
            && sabr_dvr_window_exceeded(&outcome.log);
        if sabr_stall {
            let threshold = if deep_rewind { SABR_STALL_FALLBACK_TRIES } else { 1 };
            let stalls = {
                let mut counts = self.sabr_stall_count.lock().unwrap();
                let n = counts.entry(sabr_key.clone()).or_insert(0);
                *n = n.saturating_add(1);
                *n
            };
            if stalls >= threshold {
                self.sabr_dvr_exceeded.lock().unwrap().insert(sabr_key.clone());
                self.sabr_stall_count.lock().unwrap().remove(&sabr_key);
                warn!(monitor_id, stalls, "SABR stalled from-start; next attempt will use live-edge");
            } else {
                warn!(monitor_id, stalls, threshold, "SABR stalled from-start; will retry from-start");
            }
        } else {
            // Any non-stall outcome breaks the consecutive-stall streak, so the
            // counter only ever reflects *back-to-back* from-start stalls. Clear
            // the live-edge fallback flag only when the capture actually succeeded
            // — an "ended"/"aborted"/manual outcome shouldn't un-stick a stream
            // we already decided to capture at the live edge.
            self.sabr_stall_count.lock().unwrap().remove(&sabr_key);
            if ok {
                self.sabr_dvr_exceeded.lock().unwrap().remove(&sabr_key);
            }
        }
        sabr_stall
    }

    /// Classify the outcome, finish the recording row, emit events, and kick
    /// off the post-take follow-ups (VOD check/archive, backfill concat).
    /// `ended` is the capture-tool-exit timestamp snapshotted by the caller
    /// *before* promoting/remuxing — never re-derive it with a fresh
    /// `now_unix()` here, or `ended_at` (and everything computed from it:
    /// the Streams grid's Duration column, the recording Properties dialog,
    /// head-backfill's missed-secs estimate) balloons by however long this
    /// take's remux happened to wait in the disk-gate queue.
    #[allow(clippy::too_many_arguments)]
    fn finalize_recording(
        &self,
        row: &MonitorWithChannel,
        monitor_id: i64,
        rec_id: i64,
        outcome: &ProcessOutcome,
        final_path: &Path,
        bytes: i64,
        ok: bool,
        manually_stopped: bool,
        shutting_down: bool,
        sabr_stall: bool,
        went_live_at: Option<i64>,
        approximate: bool,
        ended: i64,
    ) {
        // A 0-byte capture isn't always a failure: a livestream that had already
        // ended (or hadn't started, or exposed no live video formats) leaves
        // nothing to capture but isn't an error. Classify those as `ended` so they
        // don't show as red failures. (`ok` still drives backoff, so we don't
        // hammer an ended broadcast.)
        let stall_killed = self
            .stall_killed
            .lock()
            .unwrap()
            .remove(&(DetachedKind::Recording, rec_id));
        let status = if manually_stopped {
            // User explicitly stopped the recording; never show it as `failed`.
            if ok { "completed" } else { "stopped" }
        } else if shutting_down {
            // App shutdown killed the process tree; recording was cut short.
            "aborted"
        } else if ok {
            "completed"
        } else if stall_killed {
            // Watchdog-reaped with nothing captured: the tool wedged. Not a
            // manual stop (so backoff + the SABR fallback still apply), not a
            // red failure either.
            "ended"
        } else if sabr_stall || stream_ended_or_unavailable(&outcome.log) {
            "ended"
        } else {
            "failed"
        };
        let _ = self.store.finish_recording(
            rec_id,
            ended,
            bytes,
            outcome.exit_code,
            status,
            &final_path.to_string_lossy(),
            &outcome.log,
        );
        // The active slot was freed at capture exit (finalize may have queued
        // for hours) — a NEW take can already be recording this monitor. Don't
        // overwrite its live state with this old take's terminal status.
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
        self.schedule_vod_check(rec_id, row.monitor.platform(), status, &row.monitor.url, went_live_at, approximate);
        self.schedule_vod_archive(rec_id, row, went_live_at, status);
        // Final lost-segment sweep: anything the in-flight recovery didn't
        // get (VOD lag, resolve failures) is fetchable now that the take is
        // over. No-op without pending ranges.
        self.maybe_spawn_gap_recover(rec_id, true);
        // Covers the ordering gap_recover_job's own post-loop call can't:
        // every gap range may have ALREADY gone terminal while this take was
        // still recording (fast in-flight recovery), in which case no later
        // range-transition event fires to catch it — status just flipped to
        // "completed" right above, so check now. Cheap no-op otherwise.
        self.maybe_spawn_gap_splice(rec_id);
        // Join a backfilled head with the finished capture (no-op without one).
        {
            let this = self.clone();
            tokio::spawn(async move { this.maybe_concat_backfill(rec_id).await });
        }
        info!(
            monitor_id,
            bytes,
            status,
            "recording finished: {} {}",
            row.monitor.platform().tag(),
            row.channel.name
        );
        if status == "failed" && !outcome.log.is_empty() {
            warn!(
                monitor_id,
                "recording stderr for {} {}:\n{}",
                row.monitor.platform().tag(),
                row.channel.name,
                outcome.log
            );
        }
    }

    /// Run the DASH companion capture (dual capture): a self-contained second
    /// recording (system yt-dlp, live edge) that shares the primary's take. Inserts
    /// its own recording row, runs the process tracked under `active_secondary`,
    /// remuxes, and finalizes independently of the primary. Watchers, chat, and
    /// asset fetching all stay on the primary — this just grabs the extra formats.
    #[allow(clippy::too_many_arguments)]
    async fn run_dash_companion(
        &self,
        monitor_id: i64,
        plan: DownloadPlan,
        take_group: String,
        stream_id: Option<String>,
        went_live_at: Option<i64>,
        approximate: bool,
        channel_name: String,
        // Same trigger marker as the primary — the companion is part of the take.
        trigger_info: String,
        trigger_rule_json: String,
    ) {
        if let Some(parent) = plan.capture_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, parent).await;
            set_cache_hidden(parent);
        }
        if let Some(out_dir) = plan.final_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(Cat::DirSetup, out_dir).await;
        }
        let started_at = now_unix();
        let rec_id = self
            .store
            .insert_recording(
                monitor_id,
                started_at,
                &plan.final_path.to_string_lossy(),
                went_live_at,
                approximate,
                stream_id.as_deref(),
                Some(&take_group),
                &trigger_info,
                &trigger_rule_json,
            )
            .unwrap_or(0);
        let _ = self.events.send(AppEvent::RecordingStarted {
            monitor_id,
            recording_id: rec_id,
            channel: channel_name.clone(),
            thumbnail_path: None,
        });

        let outcome = if self.stopping_monitors.lock().unwrap().contains(&monitor_id) {
            ProcessOutcome { exit_code: None, log: String::new() }
        } else {
            self.run_process(
                &self.active_secondary,
                monitor_id,
                &plan,
                None,
                None,
                None,
                DetachReg {
                    kind: DetachedKind::Recording,
                    ref_id: rec_id,
                    monitor_id: Some(monitor_id),
                    take_group: Some(take_group.clone()),
                    started_at,
                    secondary: true,
                    stream_id: stream_id.clone(),
                    went_live_at,
                },
            )
            .await
        };
        // Broadcast end ~= when the tool exited; snapshot it before remux so the
        // span (and thus ended_at) isn't inflated by remux duration.
        let ended = now_unix();

        // Promote the companion out of .cache\ (remux .ts→.mkv) on success; a failed
        // one stays in .cache\ for the sweep.
        let final_path = promote_capture(&self.store, &self.shutdown, &plan, &self.store.remux_opts(), None).await;
        if final_path != plan.capture_path {
            if let Some(cache) = plan.capture_path.parent() {
                let stem = plan
                    .final_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                purge_cache(cache, &stem).await;
            }
        }

        let bytes = file_len(&final_path).await as i64;
        let ok = bytes > 0;
        let manually_stopped = self.stopping_monitors.lock().unwrap().contains(&monitor_id);
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
            ended,
            bytes,
            outcome.exit_code,
            status,
            &final_path.to_string_lossy(),
            &outcome.log,
        );
        let _ = self.events.send(AppEvent::RecordingFinished {
            recording_id: rec_id,
            channel: channel_name,
            status: status.into(),
        });
        info!(monitor_id, bytes, status, "dash companion finished");
        self.active_secondary.lock().unwrap().remove(&monitor_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three backoff tiers, pinned against the incidents that shaped
    /// them: the generic ladder, the instant-death floor, and the escalating
    /// PO-token cooldown (2026-07-31 overnight: two concurrent captures, every
    /// token rejected for 3+ hours, ~25 takes each under the old 10-min cap).
    #[test]
    fn failure_backoff_escalates_po_token_rejections_to_a_15_minute_cap() {
        // Generic ladder: 30s × fails, capped at 10 min.
        assert_eq!(failure_backoff_secs(1, 120, false, Gated::No), 30);
        assert_eq!(failure_backoff_secs(3, 120, false, Gated::No), 90);
        assert_eq!(failure_backoff_secs(25, 120, false, Gated::No), 600);
        // Instant deaths floor at 5 min regardless of fail count.
        assert_eq!(failure_backoff_secs(1, 3, false, Gated::No), 300);
        // PO rejection: 5/10/15 then flat 15 — never the 30s burn, never
        // unbounded either.
        assert_eq!(failure_backoff_secs(1, 120, true, Gated::No), 300);
        assert_eq!(failure_backoff_secs(2, 120, true, Gated::No), 600);
        assert_eq!(failure_backoff_secs(3, 120, true, Gated::No), 900);
        assert_eq!(failure_backoff_secs(25, 120, true, Gated::No), 900);
        // Tiers compose: an instant PO death takes the larger wait.
        assert_eq!(failure_backoff_secs(1, 3, true, Gated::No), 300);
        assert_eq!(failure_backoff_secs(4, 3, true, Gated::No), 900);
    }

    /// A subscriber-only stream is a FLAT cadence, not a ladder — every retry
    /// also refreshes the CDN head backfill that is the only thing archiving
    /// the broadcast, so escalating would widen the archive gap with each
    /// attempt. (Nyana Banyana, 2026-08-07: 22 doomed takes in two hours, each
    /// re-downloading the whole broadcast from Twitch's CDN.)
    #[test]
    fn subscriber_only_retries_at_a_flat_cadence_and_never_escalates() {
        for fails in [1u32, 3, 25] {
            assert_eq!(
                failure_backoff_secs(fails, 19, false, Gated::WithCdnFallback),
                SUB_ONLY_COOLDOWN_SECS,
                "fails={fails}"
            );
        }
        // It also wins over the instant-death floor and the PO ladder: those
        // escalate against a fault, and this isn't one.
        assert_eq!(failure_backoff_secs(9, 3, true, Gated::WithCdnFallback), SUB_ONLY_COOLDOWN_SECS);
        // Without the flag this exact failure shape is what caused the churn:
        // a 19-second death clears the instant-death floor (10s), so the first
        // retry comes 30 seconds later and the ladder crawls up from there.
        assert_eq!(failure_backoff_secs(1, 19, false, Gated::No), 30);
    }


    /// YouTube members-only is gated with NOTHING to fall back to: yt-dlp
    /// cannot see the stream at all, so every retry is another 0-byte take.
    /// Twitch's flat 10-minute cadence would be wrong here — it exists because
    /// each Twitch retry refreshes a CDN backfill that is really archiving
    /// footage. (Mori Calliope, 2026-08-08: a members-only broadcast relaunched
    /// a doomed capture every 6-11 minutes for hours, archiving nothing.)
    #[test]
    fn a_gated_broadcast_with_no_fallback_is_asked_about_hourly_not_every_few_minutes() {
        for fails in [1u32, 3, 25] {
            assert_eq!(
                failure_backoff_secs(fails, 3, false, Gated::NoFallback),
                GATED_NO_FALLBACK_COOLDOWN_SECS,
                "fails={fails}"
            );
        }
        // Well clear of the cadence that produced the loop, and of the Twitch
        // one — which would still be six doomed attempts an hour.
        assert!(GATED_NO_FALLBACK_COOLDOWN_SECS > SUB_ONLY_COOLDOWN_SECS);
        // Still bounded: it is a cooldown, not a giving-up. A stream opened to
        // the public mid-broadcast is picked up within the hour.
        assert_eq!(GATED_NO_FALLBACK_COOLDOWN_SECS, 3600);
    }

    /// Straight off the wire (Nyana Banyana, 2026-08-07): streamlink's own
    /// error line, and the usher token's rejection reason as it appears in the
    /// ad-probe URL. Both mean the same thing and must both be recognised.
    #[test]
    fn subscriber_only_rejection_is_recognised_but_ordinary_failures_are_not() {
        use crate::models::sub_only_rejected;
        assert!(sub_only_rejected("[plugins.twitch][error] UNAUTHORIZED_ENTITLEMENTS"));
        assert!(sub_only_rejected(
            r#"...%22reason%22%3A%22UNAUTHORIZED_ENTITLEMENTS%22..."#
        ));
        // Neither a healthy capture nor an unrelated failure.
        assert!(!sub_only_rejected(""));
        assert!(!sub_only_rejected("[cli][info] Writing output to file"));
        assert!(!sub_only_rejected(
            "[plugins.twitch][error] Unable to open URL: https://usher.ttvnw.net/"
        ));
    }

    /// Detected from the real logs of the 2026-07-31 Dokibird incident. All
    /// three spellings appeared there, at different layers of yt-dlp.
    #[test]
    fn po_token_rejection_is_recognised_but_healthy_captures_are_not() {
        // The SABR stream's own protection status (the earliest signal).
        assert!(po_token_rejected(
            "[debug] [SABR Debug Info]: v:7EoBQWYGnXM c:WEB t:353900 rn:20 sr:0 act:N pot:Y \
             sps:ATTESTATION_REQUIRED live 2s bid:1 hs:177"
        ));
        // The typed exception.
        assert!(po_token_rejected(
            "yt_dlp.extractor.youtube._streaming.sabr.exceptions.PoTokenError: This stream \
             requires a GVS PO Token to continue and the one provided is invalid"
        ));
        // The plain message (no traceback, e.g. a truncated tail).
        assert!(po_token_rejected(
            "ERROR: This stream requires a GVS PO Token to continue and the one provided is invalid"
        ));

        // A HEALTHY capture mints tokens too — that must never look like a
        // rejection, or every YouTube capture would get a 5-minute cooldown.
        assert!(!po_token_rejected(
            "[youtube] [pot:bgutil:http] Generating a gvs PO Token for web client via bgutil \
             HTTP server\n[debug] [youtube] 7EoBQWYGnXM: Retrieved a gvs PO Token for web client"
        ));
        // Unrelated failures keep the ordinary backoff ladder.
        assert!(!po_token_rejected("ERROR: No video formats found!"));
        assert!(!po_token_rejected(""));
    }

    /// The fix for the DyaRikku incident (2026-07-24): a stall-killed
    /// recording must refuse a new AUTOMATIC start for a short cooldown
    /// (giving the platform's own offline propagation time to catch up),
    /// but never one that's old or absent.
    #[test]
    fn stall_cooldown_blocks_only_within_the_window() {
        assert!(!Supervisor::stall_cooldown_blocks(None), "never killed — must not block");
        assert!(Supervisor::stall_cooldown_blocks(Some(0)), "just killed — must block");
        assert!(
            Supervisor::stall_cooldown_blocks(Some(STALL_RESTART_COOLDOWN_SECS - 1)),
            "still inside the window — must block"
        );
        assert!(
            !Supervisor::stall_cooldown_blocks(Some(STALL_RESTART_COOLDOWN_SECS)),
            "window elapsed — must not block"
        );
        assert!(
            !Supervisor::stall_cooldown_blocks(Some(STALL_RESTART_COOLDOWN_SECS + 60)),
            "long past the window — must not block"
        );
    }

    /// Regression test for the girl_dm_/Nihmune incident (2026-07-23): a chat
    /// task that never notices its `done` flag (simulating a `ws.send()` hung
    /// on a half-dead connection) must not wedge `stop_record_watchers`
    /// forever — it has to abort and return within its bounded grace period.
    /// Real-time (no `tokio::time::pause` — this crate doesn't pull in
    /// tokio's `test-util` feature), so this genuinely takes a few seconds;
    /// the hung task sleeps well past the grace period, not forever, so a
    /// regression (the old unconditional `t.await`) fails fast instead of
    /// hanging the whole suite.
    #[tokio::test]
    async fn stop_record_watchers_aborts_a_hung_chat_task() {
        let watcher_done = Arc::new(AtomicBool::new(false));
        let meta_done = Arc::new(AtomicBool::new(false));
        let chat_done = Arc::new(AtomicBool::new(false));
        // Deliberately ignores `chat_done` — stands in for a task stuck inside
        // an unprotected network/disk await, same as the real bug.
        let chat_task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let started = std::time::Instant::now();
        Supervisor::stop_record_watchers(
            watcher_done,
            None,
            meta_done,
            None,
            chat_done.clone(),
            Some(chat_task),
        )
        .await;

        // Returned (didn't hang forever) and did so around the grace window,
        // not after waiting out the hung task's own 30s sleep.
        assert!(started.elapsed() < Duration::from_secs(20));
        // The done flag was still set even though the task ignored it — the
        // abort path is what actually recovers, not a missed signal.
        assert!(chat_done.load(Ordering::SeqCst));
    }

    /// The common case: a chat task that DOES notice `done` promptly exits on
    /// its own, well inside the grace period — no abort needed.
    #[tokio::test]
    async fn stop_record_watchers_lets_a_cooperative_chat_task_exit_on_its_own() {
        let watcher_done = Arc::new(AtomicBool::new(false));
        let meta_done = Arc::new(AtomicBool::new(false));
        let chat_done = Arc::new(AtomicBool::new(false));
        let task_done = chat_done.clone();
        let chat_task = tokio::spawn(async move {
            while !task_done.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let started = std::time::Instant::now();
        Supervisor::stop_record_watchers(
            watcher_done,
            None,
            meta_done,
            None,
            chat_done,
            Some(chat_task),
        )
        .await;

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// The duplicate-recording safety net's independent liveness signal
    /// (Layna incident, 2026-07-24): a just-written file reads as fresh, one
    /// backdated past the threshold reads as stale, and a missing path never
    /// panics — it just isn't fresh.
    #[tokio::test]
    async fn file_written_within_detects_fresh_vs_stale_vs_missing() {
        let dir = std::env::temp_dir().join(format!("sa_test_fwiw_{}", std::process::id()));
        crate::iomon::fs::create_dir_all_sync(Cat::FsProbe, &dir).unwrap();

        let fresh = dir.join("fresh.ts");
        crate::iomon::fs::write_sync(Cat::FsProbe, &fresh, b"x").unwrap();
        assert!(Supervisor::file_written_within(fresh.to_str().unwrap(), 60).await);

        let stale = dir.join("stale.ts");
        crate::iomon::fs::write_sync(Cat::FsProbe, &stale, b"x").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(300);
        crate::iomon::fs::open_with_sync(Cat::FsProbe, &stale, |o| {
            o.write(true);
        })
        .unwrap()
        .set_modified(old)
        .unwrap();
        assert!(!Supervisor::file_written_within(stale.to_str().unwrap(), 60).await);

        let missing = dir.join("missing.ts");
        assert!(!Supervisor::file_written_within(missing.to_str().unwrap(), 60).await);

        let _ = crate::iomon::fs::remove_dir_all_sync(Cat::FsProbe, &dir);
    }
}
