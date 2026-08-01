//! "Follow raid" orchestration: consumes [`RaidOutSignal`] pushes from
//! `eventsub.rs` and either force-starts a tracked raid target (reusing
//! `manual_start`'s existing Auto-off bypass, the same mechanism a manual
//! ▶ Start already uses) or spawns a lightweight ad-hoc capture for an
//! untracked one — no `Channel`/`Monitor`/`Recording` row, just a file under
//! the configured output directory. Settings/resolvers live in the
//! top-level `crate::raid_follow` module; this one owns everything that
//! needs `Supervisor` internals.
//!
//! Single-hop only for now (record until the target's own stream ends) —
//! chain-following (the target itself raiding out further) is intentionally
//! not implemented yet; see the "Follow raid" README section.

use super::*;
use crate::events::{BackgroundTask, BackgroundTaskKind, RaidOutSignal, TaskOutcome};
use crate::models::{MonitorWithChannel, Platform, Tool};

impl Supervisor {
    /// Consume `RaidOutSignal` pushes until shutdown.
    pub async fn run_raid_follow(self, mut raid_out_rx: mpsc::UnboundedReceiver<RaidOutSignal>) {
        while let Some(sig) = raid_out_rx.recv().await {
            if self.shutdown.load(Ordering::SeqCst) {
                continue;
            }
            self.handle_raid_out(sig).await;
        }
    }

    async fn handle_raid_out(&self, sig: RaidOutSignal) {
        let Ok(Some(from_row)) = self.store.get_monitor_with_channel(sig.from_monitor_id) else {
            return;
        };
        // Record and play are fully independent — either, both, or neither
        // may fire for the same raid.
        let want_record = crate::raid_follow::effective_raid_follow_record(
            &self.store,
            from_row.channel.id,
            from_row.monitor.id,
        );
        let want_play = crate::raid_follow::effective_raid_follow_play(
            &self.store,
            from_row.channel.id,
            from_row.monitor.id,
        );
        if !want_record && !want_play {
            return;
        }
        if sig.to_login.is_empty() {
            tracing::debug!(
                monitor_id = sig.from_monitor_id,
                to = sig.to_display_name.as_str(),
                "follow-raid: target login unknown (Twitch omitted it), can't follow"
            );
            return;
        }
        // Case-insensitive login match against every locally-tracked Twitch
        // monitor — same resolution shape as the collab-partner "Play all
        // collab instances" feature, just a one-off scan here rather than a
        // per-frame cache (raids are rare).
        let target_row = self
            .store
            .list_monitors_with_channels()
            .unwrap_or_default()
            .into_iter()
            .find(|r| {
                r.monitor.platform() == Platform::Twitch
                    && crate::detectors::twitch_login(&r.monitor.url).as_deref()
                        == Some(sig.to_login.as_str())
            });
        // Play doesn't consume `sig`/`from_row`/`target_row`, so it goes
        // first — record's tracked/untracked branches take ownership.
        if want_play {
            self.try_follow_raid_play(&sig, target_row.as_ref()).await;
        }
        if want_record {
            match target_row {
                Some(row) => self.follow_raid_tracked(row, &sig).await,
                None => self.follow_raid_ad_hoc(sig, from_row).await,
            }
        }
    }

    /// Auto-play side of Follow raid: open the target at the live edge, no
    /// recording — the automatic equivalent of the manual "▷🏃 Follow raid"
    /// button, via the same [`crate::ui::player::spawn_follow_raid`].
    /// Unlike record, never gated by the target's disabled state — only by
    /// its own explicit "Exclude from auto-play" override, since opening a
    /// player touches nothing about the target's recording/disk config.
    async fn try_follow_raid_play(&self, sig: &RaidOutSignal, target_row: Option<&MonitorWithChannel>) {
        // "Only when watching" (default on): follow the raid in a player only
        // if a player for the RAIDING instance is open right now — or closed
        // within the grace window, since mpv often hits end-of-stream and
        // exits moments before the raid event arrives. Without this gate,
        // every auto-play-enabled instance pops an unexplained player window
        // whenever it raids out, watched or not.
        if crate::raid_follow::raid_follow_play_only_watched(&self.store)
            && !crate::ui::player::monitor_watched_recently(
                sig.from_monitor_id,
                crate::raid_follow::RAID_PLAY_WATCHED_GRACE_SECS,
            )
        {
            tracing::debug!(
                monitor_id = sig.from_monitor_id,
                to = sig.to_display_name.as_str(),
                "follow-raid: auto-play skipped — the raiding instance wasn't open in a \
                 player (\"Only when watching\" is on)"
            );
            return;
        }
        if let Some(row) = target_row
            && crate::raid_follow::is_excluded_from_auto_play(&self.store, row.channel.id, row.monitor.id)
        {
            tracing::debug!(
                monitor_id = row.monitor.id,
                channel = row.channel.name.as_str(),
                "follow-raid: target excluded from auto-play"
            );
            return;
        }
        let player = self
            .store
            .get_setting(crate::ui::K_MEDIA_PLAYER)
            .ok()
            .flatten()
            .unwrap_or_default();
        let player = player.trim();
        if player.is_empty() {
            tracing::debug!("follow-raid: no media player configured, skipping auto-play");
            return;
        }
        let Ok(Some(from_row)) = self.store.get_monitor_with_channel(sig.from_monitor_id) else {
            return;
        };
        let settings = crate::ui::SettingsForm::for_auto_play(&self.store);
        if let Some(msg) = crate::ui::player::spawn_follow_raid(
            &from_row,
            &sig.to_login,
            &sig.to_display_name,
            player,
            &settings,
            &self.store,
            Some(&crate::ui::player::LiveMetaCtx::from_ctx(&self.ctx)),
        ) {
            tracing::info!(
                to = sig.to_display_name.as_str(),
                broadcaster_id = sig.to_broadcaster_id.as_str(),
                "follow-raid: auto-play — {msg}"
            );
        }
    }

    /// Force-start a tracked raid target that isn't already recording, the
    /// same way a manual ▶ Start does (bypasses Auto-record-off and, since
    /// `manual_start(.., true)` doesn't gate on it either, the master
    /// switch too — which is exactly why `should_record_raid_target`'s own
    /// disabled-check runs first here, not relying on that bypass).
    async fn follow_raid_tracked(&self, row: MonitorWithChannel, sig: &RaidOutSignal) {
        let mid = row.monitor.id;
        if self.active.lock().unwrap().contains_key(&mid) {
            return; // already recording — nothing to do (dedup)
        }
        if !crate::raid_follow::should_record_raid_target(&self.store, &row) {
            tracing::debug!(
                monitor_id = mid,
                channel = row.channel.name.as_str(),
                "follow-raid: target exempted (disabled, or explicitly excluded)"
            );
            return;
        }
        tracing::info!(
            monitor_id = mid,
            channel = row.channel.name.as_str(),
            from_monitor_id = sig.from_monitor_id,
            viewers = sig.viewers,
            raided_at = sig.at,
            broadcaster_id = sig.to_broadcaster_id.as_str(),
            "follow-raid: force-starting tracked raid target"
        );
        self.manual_start(mid, true).await;
    }

    /// Untracked target: a lightweight, no-DB-row capture. In-flight guard
    /// (keyed by lowercase login) so two raids into the same untracked
    /// channel — from two different source monitors, or a duplicate push —
    /// don't spawn overlapping captures.
    async fn follow_raid_ad_hoc(&self, sig: RaidOutSignal, from_row: MonitorWithChannel) {
        {
            let mut guard = self.raid_follow_ad_hoc.lock().unwrap();
            if !guard.insert(sig.to_login.clone()) {
                return;
            }
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.run_raid_follow_capture(&sig, &from_row).await;
            this.raid_follow_ad_hoc.lock().unwrap().remove(&sig.to_login);
        });
    }

    async fn run_raid_follow_capture(&self, sig: &RaidOutSignal, from_row: &MonitorWithChannel) {
        let out_dir_setting = crate::raid_follow::raid_follow_output_dir(&self.store);
        if out_dir_setting.trim().is_empty() {
            tracing::warn!(
                to = sig.to_display_name.as_str(),
                "follow-raid: no output directory configured (Settings → Follow raid), \
                 skipping ad-hoc capture"
            );
            return;
        }
        let name = sanitize_filename(&sig.to_display_name);
        let dir = PathBuf::from(out_dir_setting.replace("{name}", &name));
        if let Err(e) = crate::iomon::fs::create_dir_all_sync(crate::iomon::Cat::DirSetup, &dir) {
            tracing::warn!(%e, dir = %dir.display(), "follow-raid: failed to create output dir");
            return;
        }
        let stamp = crate::models::now_unix();
        let ts_path = dir.join(format!("{name}.{stamp}.ts"));
        let mkv_path = dir.join(format!("{name}.{stamp}.mkv"));
        let url = format!("https://twitch.tv/{}", sig.to_login);

        let m = &from_row.monitor;
        // No auth/cookies: a raid target is an arbitrary channel the user
        // has no established subscriber/ad-free relationship with, unlike
        // the raiding monitor's own configured auth.
        let (program, args) = match m.tool {
            Tool::Streamlink => (
                "streamlink".to_string(),
                vec![
                    "--twitch-supported-codecs=h264,h265,av1".to_string(),
                    "--retry-streams".into(),
                    "3".into(),
                    "--retry-max".into(),
                    "5".into(),
                    "-o".into(),
                    ts_path.to_string_lossy().into_owned(),
                    url.clone(),
                    resolved_quality(&m.quality),
                ],
            ),
            Tool::YtDlp | Tool::Ffmpeg => (
                "yt-dlp".to_string(),
                vec![
                    "--no-part".to_string(),
                    "--hls-use-mpegts".into(),
                    "-o".into(),
                    ts_path.to_string_lossy().into_owned(),
                    "--no-live-from-start".into(),
                    url.clone(),
                ],
            ),
        };

        let task_id = crate::events::next_task_id();
        let _ = self.events.send(AppEvent::BackgroundTaskStarted(BackgroundTask {
            id: task_id,
            kind: BackgroundTaskKind::RaidFollow,
            label: sig.to_display_name.clone(),
            detail: format!("Following raid from {} — ad-hoc, not added to Streams", from_row.channel.name),
            started_at: stamp,
            progress: None,
            progress_info: None,
        }));

        let line = format!("{program} {}", args.join(" "));
        tracing::info!(
            %line,
            to = sig.to_display_name.as_str(),
            viewers = sig.viewers,
            broadcaster_id = sig.to_broadcaster_id.as_str(),
            "follow-raid: spawning ad-hoc capture"
        );
        let mut cmd = Command::new(&program);
        cmd.args(&args).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        let outcome = match cmd.spawn() {
            Ok(mut child) => match child.wait().await {
                Ok(status) if status.success() => {
                    // Bring the capture in line with the app's MKV convention —
                    // best-effort; the .ts is kept either way, so a remux
                    // failure just means a slightly less convenient file, not
                    // lost footage.
                    let opts = crate::models::RemuxOpts {
                        embed_thumbnail: false,
                        embed_title: false,
                        title_template: String::new(),
                        embed_subs: false,
                        title_vars: None,
                    };
                    // Untracked ad-hoc capture — `ref_id = 0` opts out of the
                    // restart-survival registry (see `DetachReg`'s own convention).
                    match remux_ts_to_mkv(&self.store, &self.shutdown, 0, &ts_path, &mkv_path, None, &opts).await {
                        Ok(()) => {
                            let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::Promote, &ts_path);
                            TaskOutcome::CompletedWithNote(mkv_path.display().to_string())
                        }
                        Err(e) => {
                            tracing::warn!(%e, "follow-raid: remux to mkv failed, .ts kept as-is");
                            TaskOutcome::CompletedWithNote(ts_path.display().to_string())
                        }
                    }
                }
                Ok(status) => TaskOutcome::Failed(format!("capture tool exited with {status}")),
                Err(e) => TaskOutcome::Failed(format!("failed to wait on capture tool: {e}")),
            },
            Err(e) => {
                tracing::warn!(%line, "follow-raid: failed to spawn capture tool: {e}");
                TaskOutcome::Failed(format!("failed to launch {program}: {e}"))
            }
        };
        let _ = self.events.send(AppEvent::BackgroundTaskFinished { id: task_id, outcome });
    }
}
