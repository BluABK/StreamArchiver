//! Supervisor lifecycle: the main loop, start/stop/holds, detection,
//! `record`, chat + video downloads, asset/scheduled loops.

use super::*;

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
            blocked_notified: Arc::new(Mutex::new(HashMap::new())),
            active_chats,
            stopping_chats: Arc::new(Mutex::new(HashSet::new())),
            shutdown,
            manual_tx,
            ctx,
            ad_active,
            sem: Arc::new(Semaphore::new(max_concurrent.max(1))),
            backoff: Arc::new(Mutex::new(HashMap::new())),
            sabr_dvr_exceeded: Arc::new(Mutex::new(HashSet::new())),
            sabr_stall_count: Arc::new(Mutex::new(HashMap::new())),
            running_asset_fetches: Arc::new(Mutex::new(HashSet::new())),
            running_concats: Arc::new(Mutex::new(HashSet::new())),
            quality_upgraded: Arc::new(Mutex::new(HashSet::new())),
            stop_holds,
            finalizing,
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
                    self.try_begin(signal.monitor_id, signal.went_live_at, signal.approximate, signal.stream_id, signal.thumbnail_url, signal.broadcaster_id, signal.stream_title, signal.stream_game, signal.stream_viewers, false, false);
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
            ManualCommand::StopHoldFor { monitor_id, hours } => {
                self.manual_stop_hold(monitor_id, hours)
            }
            ManualCommand::StartVideo(id) => {
                if !self.shutdown.load(Ordering::SeqCst) {
                    let this = self.clone();
                    tokio::spawn(async move { this.start_video(id).await });
                }
            }
            ManualCommand::StopVideo(id) => self.stop_video(id),
            ManualCommand::StopChat(id) => self.stop_chat_download(id),
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
            ManualCommand::RenameRecording { rec_id, new_stem } => {
                self.cmd_rename_recording(rec_id, new_stem)
            }
        }
    }

    /// [`ManualCommand::ReRemux`]: re-remux one captured `.ts` to MKV in the
    /// background and update the recording row on success.
    fn cmd_re_remux(&self, rec_id: i64, capture: PathBuf, final_: PathBuf) {
        let store = self.store.clone();
        let tx = self.events.clone();
        let task_id = rec_id as u64;
        let src_name = capture
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dst_name = final_
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _ = tx.send(AppEvent::BackgroundTaskStarted(
            crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::Remux,
                label: src_name,
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
            match remux_ts_to_mkv(&capture, &final_, Some((tx2, task_id)), &opts).await {
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
        });
    }

    /// [`ManualCommand::ReRemuxAll`]: re-remux every recording that still has
    /// a `.ts` source next to its planned MKV.
    fn cmd_re_remux_all(&self) {
        let store = self.store.clone();
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
                let _ = tx.send(AppEvent::BackgroundTaskProgress {
                    id: task_id,
                    progress: Some(done as f32 / total as f32),
                    info: format!("{}/{total}: {}", done + 1, mkv.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
                });
                match remux_ts_to_mkv(&ts, &mkv, None, &opts).await {
                    Ok(()) => {
                        let _ = crate::iomon::fs::remove_file(Cat::CacheSweep, &ts).await;
                        if mkv != planned_mkv {
                            let _ = store.update_recording_output_path(*rec_id, &mkv.to_string_lossy());
                        }
                        let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                    }
                    Err(e) => warn!("re-remux-all failed for rec_id={rec_id}: {e:#}"),
                }
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
            let Ok(Some((murl, vod_id, stream_id, went_live))) =
                store.recording_archive_now(rec_id)
            else {
                return;
            };
            let url = match Platform::detect(&murl) {
                Platform::Twitch => {
                    // Re-resolve by broadcast id when we can: a stored vod_id
                    // may be a wrong-VOD match from the old window-only poll
                    // (rec 652 downloaded the NEXT stream's VOD). Falls back
                    // to the stored id when Helix is unavailable.
                    let repolled = match stream_id.as_deref().filter(|s| !s.is_empty()) {
                        Some(sid) => {
                            resolve_twitch_vod_by_stream(&ctx, &murl, sid).await.map(|(v, muted)| {
                                let _ = store.set_recording_vod_found(rec_id, &v, muted);
                                v
                            })
                        }
                        None => None,
                    };
                    repolled
                        .or(vod_id.filter(|v| !v.is_empty()))
                        .map(|v| crate::vod_archive::twitch_vod_url(&v))
                }
                Platform::YouTube => stream_id
                    .filter(|s| !s.is_empty())
                    .map(|s| crate::vod_archive::youtube_vod_url(&s)),
                Platform::Kick => match crate::vod_archive::kick_slug(&murl) {
                    Some(slug) => {
                        crate::vod_archive::resolve_kick_vod(
                            &ctx.http_client(),
                            &slug,
                            went_live,
                        )
                        .await
                    }
                    None => None,
                },
                _ => None,
            };
            match url {
                Some(u) => {
                    enqueue_vod_archive(&store, &manual_tx, rec_id, &u);
                }
                None => {
                    let _ = store.set_recording_vod_dl(rec_id, "failed", None);
                }
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
                    match embed_thumbnail_into_mkv(&mkv, &thumb).await {
                        Ok(()) => {
                            embedded += 1;
                            let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                        }
                        Err(e) => warn!("embed-thumbnail failed for rec_id={rec_id}: {e:#}"),
                    }
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
                        if let Err(e) = embed_thumbnail_into_mkv(&output, &thumb).await {
                            warn!("embed after fetch failed rec_id={rec_id}: {e:#}");
                        } else {
                            fetched += 1;
                            let _ = tx.send(AppEvent::RecordingUpdated { recording_id: *rec_id });
                        }
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
                        sweep_companion_files(std::path::Path::new(&dir), &cfg).await;
                    }
                }
            }
            let _ = tx.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(format!("{total} checked")),
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
                        rule.monitor_id, Some(now), true, None, None, None, None, None, None, true, true,
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
        bypass_backoff: bool,
        forced: bool,
    ) -> bool {
        // Manual-stop hold: a user Stop means "leave this alone" — no
        // automatic restart (poll, push, or trigger rule) until a NEW
        // broadcast appears or the timed hold expires. A manual ▶ Start
        // clears it before reaching here (`manual_start`).
        if let Some(reason) =
            self.stop_hold_blocks(monitor_id, stream_id.as_deref(), went_live_at)
        {
            tracing::debug!(monitor_id, "auto start suppressed: {reason}");
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
        if bypass_backoff {
            self.backoff.lock().unwrap().remove(&monitor_id);
        }

        let mut row = match self.store.get_monitor_with_channel(monitor_id) {
            Ok(Some(r)) => r,
            _ => {
                self.active.lock().unwrap().remove(&monitor_id);
                return false;
            }
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
                self.active.lock().unwrap().remove(&monitor_id);
                let this = self.clone();
                let row_bg = row.clone();
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
            )
        {
            self.active.lock().unwrap().remove(&monitor_id);
            // Keep the UI's live state fresh exactly like the Auto-off path
            // below — the channel IS live, it's just not being recorded.
            let _ = self.store.set_monitor_check_result(monitor_id, "live", now_unix());
            let (live_since, live_since_approx) = match went_live_at {
                Some(t) => (Some(t), approximate),
                None => (Some(now_unix()), true),
            };
            let _ = self.store.set_monitor_live_meta(
                monitor_id,
                stream_title.as_deref().unwrap_or(""),
                stream_game.as_deref().unwrap_or(""),
                thumbnail_url.as_deref().unwrap_or(""),
                stream_viewers.unwrap_or(-1),
                live_since,
                live_since_approx,
            );
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
            self.active.lock().unwrap().remove(&monitor_id);
            let _ = self.store.set_monitor_check_result(monitor_id, "live", now_unix());
            let (live_since, live_since_approx) = match went_live_at {
                Some(t) => (Some(t), approximate),
                None => (Some(now_unix()), true),
            };
            let _ = self.store.set_monitor_live_meta(
                monitor_id,
                stream_title.as_deref().unwrap_or(""),
                stream_game.as_deref().unwrap_or(""),
                thumbnail_url.as_deref().unwrap_or(""),
                stream_viewers.unwrap_or(-1),
                live_since,
                live_since_approx,
            );
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
        let _ = self
            .store
            .set_monitor_check_result(monitor_id, "offline", now_unix());
    }

    /// "Start" command: check the channel now and record if live. A
    /// user-initiated start records even when Auto is off (Auto only gates
    /// *automatic* starts) and toasts when the channel isn't live; an
    /// automatic trigger (WebSub push) honors the Auto gate and just keeps
    /// the stream state fresh. `Disabled` detection skips the check entirely
    /// (see below).
    async fn manual_start(&self, monitor_id: i64, user_initiated: bool) {
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
                self.try_begin(monitor_id, Some(now_unix()), true, None, None, None, None, None, None, true, true);
            }
            return;
        }
        let auto = row.channel.enabled && row.monitor.enabled;
        let name = row.channel.name.clone();
        let outcome = self.check_one(&row).await;
        if outcome.live {
            if auto || user_initiated {
                let (went, approx) = match outcome.went_live_at {
                    Some(t) => (Some(t), false),
                    None => (Some(now_unix()), true),
                };
                self.try_begin(monitor_id, went, approx, outcome.stream_id, outcome.thumbnail_url, outcome.broadcaster_id, outcome.stream_title, outcome.stream_game, outcome.stream_viewers, true, user_initiated);
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
    pub fn manual_stop_hold(&self, monitor_id: i64, hours: Option<i64>) {
        let hold = match hours {
            Some(h) => StopHold::Until(now_unix() + h * 3600),
            None => {
                let (stream_id, went_live_at) = self
                    .store
                    .latest_stream_identity(monitor_id)
                    .ok()
                    .flatten()
                    .unwrap_or((None, None));
                StopHold::FreshStream { stream_id, went_live_at }
            }
        };
        {
            let mut holds = self.stop_holds.lock().unwrap();
            holds.insert(monitor_id, hold);
            persist_stop_holds(&self.store, &holds);
        }
        info!(monitor_id, hours, "manual stop with restart hold");
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

    /// `Some(reason)` when an automatic start must be suppressed by a stop
    /// hold; expired/superseded holds are removed here as a side effect.
    fn stop_hold_blocks(
        &self,
        monitor_id: i64,
        stream_id: Option<&str>,
        went_live_at: Option<i64>,
    ) -> Option<String> {
        let mut holds = self.stop_holds.lock().unwrap();
        let hold = holds.get(&monitor_id)?.clone();
        let expired = match &hold {
            StopHold::Until(t) => now_unix() >= *t,
            StopHold::FreshStream { stream_id: held_sid, went_live_at: held_wl } => {
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
        Some(match hold {
            StopHold::Until(t) => format!("held until unix {t}"),
            StopHold::FreshStream { .. } => "held until a new broadcast".to_string(),
        })
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
                BackoffEntry { fails: 0, until: Instant::now() + Duration::from_secs(10) },
            );
            return;
        }
    }

    /// Stop the live-chat sidecar download for a monitor, if one is running.
    fn stop_chat_download(&self, monitor_id: i64) {
        let pid = self.active_chats.lock().unwrap().get(&monitor_id).copied();
        let Some(pid) = pid else { return };
        self.stopping_chats.lock().unwrap().insert(monitor_id);
        if pid > 0 {
            crate::platform::kill_process_tree(pid);
        }
        info!(monitor_id, "stop chat download");
    }

    /// Stop the YouTube chat sidecar for `monitor_id` (if running) and wait
    /// up to `timeout` for it to release its `live_chat.json` file handle.
    /// Called before `rename_companion_sidecars` so the rename isn't blocked
    /// by an actively-writing chat process (Windows os error 32).
    pub(super) async fn stop_and_wait_for_chat(&self, monitor_id: i64, timeout: Duration) {
        if !self.active_chats.lock().unwrap().contains_key(&monitor_id) {
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
    async fn run_chat_download(&self, monitor_id: i64, platform: Platform, plan: DownloadPlan) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
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
        cmd.args(&plan.args)
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
            final_path = promote_capture(&plan, &self.store.remux_opts(), None).await;
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
                    final_path = rename_for_media(final_path, &stem).await;
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

    fn note_result(&self, monitor_id: i64, duration_secs: i64, ok: bool) {
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
            });
            entry.fails = entry.fails.saturating_add(1);
            let mut wait = (30u64 * entry.fails as u64).min(600);
            // A capture that died almost immediately having produced nothing (a
            // few seconds — e.g. "No video formats found" during a transient
            // no-format window / pre-roll ad, or an unrecordable configuration)
            // shouldn't re-spawn every ~30s and tight-loop for the whole stream.
            // Apply a higher floor for these instant failures.
            const INSTANT_FAIL_SECS: i64 = 10;
            if duration_secs < INSTANT_FAIL_SECS {
                wait = wait.max(300);
            }
            entry.until = Instant::now() + Duration::from_secs(wait);
            warn!(
                monitor_id,
                fails = entry.fails,
                wait,
                duration_secs,
                "recording captured nothing; backing off"
            );
        }
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
        if row.monitor.capture_from_start
            && self.sabr_dvr_exceeded.lock().unwrap().contains(&sabr_key)
        {
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
            self.active.lock().unwrap().remove(&monitor_id);
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
        let (chat_done, chat_task) =
            self.spawn_chat_loggers(&row, &plan, &auth, &ytdlp_global_args, &ytdlp_bins, monitor_id);

        // If a manual stop arrived while we were setting up (pid was 0 so kill
        // couldn't fire yet), honour it now: skip spawning the process entirely.
        let outcome = if self.stopping_monitors.lock().unwrap().contains(&monitor_id) {
            ProcessOutcome { exit_code: None, log: String::new() }
        } else {
            // From-start SABR captures get a `.state` guard alongside each
            // attempt (deny-read handles that keep backup/AV from acquiring
            // the locks that kill yt-dlp's checkpoint replace — see
            // `state_guard.rs`). Spawned per attempt and stopped the moment
            // the child exits: a resuming attempt must be able to READ its
            // state, so guards never span a relaunch.
            let sabr_capture = row.monitor.platform() == Platform::YouTube
                && row.monitor.tool == Tool::YtDlp
                && row.monitor.capture_from_start
                && ytdlp_bins.sabr.usable();
            let guard = self.spawn_sabr_state_guard(&plan, monitor_id, sabr_capture);
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
            guard.stop().await;
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
                self.log_lock_culprits(&outcome.log, monitor_id).await;
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
                let guard = self.spawn_sabr_state_guard(&plan, monitor_id, sabr_capture);
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
                guard.stop().await;
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

        self.stop_record_watchers(watcher_done, watcher, meta_done, meta_task, chat_done, chat_task)
            .await;
        // Capture over, finalize begins — the promote below can sit in the disk-
        // gate queue for hours, so tell the UI this monitor is "finalizing", not
        // still recording. Crucially, FREE THE ACTIVE SLOT NOW: while a monitor
        // is in `active`, the scheduler skips polling it and try_begin refuses
        // new takes — holding it through a queued remux made a dropped-and-
        // restarted stream invisible for the whole wait (DougDoug, 2026-07-14:
        // his restart went uncaptured for 2+ h behind a 7 GB remux queue).
        self.finalizing.lock().unwrap().insert(monitor_id, rec_id);
        self.active.lock().unwrap().remove(&monitor_id);
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
        );

        // A manual stop already installed its own 120s cooldown (see `manual_stop`);
        // don't let the subprocess's exit clobber it — a 0-byte stopped capture would
        // otherwise reset the wait to 30s, and a captured one would clear it entirely,
        // either way re-triggering the moment the next LIVE signal arrives.
        if !manually_stopped {
            self.note_result(monitor_id, duration, ok);
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
        let ytdlp_bins = load_ytdlp_bins(&self.store);
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
    fn spawn_chat_loggers(
        &self,
        row: &MonitorWithChannel,
        plan: &DownloadPlan,
        auth: &AuthSource,
        ytdlp_global_args: &[String],
        ytdlp_bins: &YtDlpBins,
        monitor_id: i64,
    ) -> (Arc<AtomicBool>, Option<tokio::task::JoinHandle<()>>) {
        // Twitch chat -> a native anonymous IRC-over-WebSocket logger, written as
        // a `.chat.jsonl` sidecar in the OUTPUT dir (next to the final file, not in
        // .cache\) so it isn't promoted/purged from under a still-writing logger; it
        // follows the file's stem on the post-rename. Twitch only.
        let chat_done = Arc::new(AtomicBool::new(false));
        let chat_task = (row.monitor.chat_log && row.monitor.platform() == Platform::Twitch)
            .then(|| {
                let chat_path = plan.final_path.with_extension("chat.jsonl");
                tokio::spawn(crate::chat::log_twitch_chat(
                    row.monitor.url.clone(),
                    chat_path,
                    chat_done.clone(),
                    self.shutdown.clone(),
                ))
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
            // `.live_chat.json` must not be promoted/purged mid-write.
            let chat_plan = build_chat_plan(row, &plan.final_path, auth, ytdlp_global_args, &ytdlp_bins.system_program());
            let this = self.clone();
            let mid = monitor_id;
            let chat_platform = row.monitor.platform();
            tokio::spawn(async move { this.run_chat_download(mid, chat_platform, chat_plan).await });
        }
        (chat_done, chat_task)
    }

    /// Stop the per-take watchers once the capture process has exited.
    async fn stop_record_watchers(
        &self,
        watcher_done: Arc<AtomicBool>,
        watcher: Option<tokio::task::JoinHandle<()>>,
        meta_done: Arc<AtomicBool>,
        meta_task: Option<tokio::task::JoinHandle<()>>,
        chat_done: Arc<AtomicBool>,
        chat_task: Option<tokio::task::JoinHandle<()>>,
    ) {
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
        chat_done.store(true, Ordering::SeqCst);
        if let Some(t) = chat_task {
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
                final_path = rename_for_media(final_path, &stem).await;
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
    fn note_sabr_stall(
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
            now_unix(),
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

        // Promote the companion out of .cache\ (remux .ts→.mkv) on success; a failed
        // one stays in .cache\ for the sweep.
        let final_path = promote_capture(&plan, &self.store.remux_opts(), None).await;
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
            now_unix(),
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
