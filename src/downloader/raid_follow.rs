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
        if !crate::raid_follow::effective_raid_follow_record(
            &self.store,
            from_row.channel.id,
            from_row.monitor.id,
        ) {
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
        match target_row {
            Some(row) => self.follow_raid_tracked(row, &sig).await,
            None => self.follow_raid_ad_hoc(sig, from_row).await,
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
                    match remux_ts_to_mkv(&ts_path, &mkv_path, None, &opts).await {
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
