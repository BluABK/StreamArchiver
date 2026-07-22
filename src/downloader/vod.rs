//! Post-stream VOD archive pipeline and Twitch VOD poll / mute watching.

use super::*;

// ---------- Twitch VOD background checker ----------

/// How long to wait between polling attempts (YouTube/Kick VOD waits; the
/// Twitch check uses the faster [`vod_poll_delay_secs`] schedule).
pub(super) const VOD_POLL_INTERVAL_SECS: u64 = 5 * 60;
/// Maximum number of polls before giving up (5 min × 12 = 60 min total).
pub(super) const VOD_MAX_POLLS: u32 = 12;

/// Delay BEFORE 0-based Twitch VOD poll `n`: immediate first check (Twitch
/// publishes the archive within seconds of stream end), 25 s cadence for the
/// first ~10 minutes, then 5-minute backoff. The fast phase exists to win the
/// race against DMCA muting, which is applied minutes AFTER publication and
/// scrubs the original segments from the CDN — an archive download that
/// starts inside that window captures the un-muted stream.
pub(super) fn vod_poll_delay_secs(poll: u32) -> u64 {
    match poll {
        0 => 0,
        1..=24 => 25,
        _ => 300,
    }
}
/// Twitch poll count: 25 fast polls (~10 min) + 10 five-minute polls ≈ 60 min.
pub(super) const VOD_TOTAL_POLLS: u32 = 35;

/// After a clean (un-muted) VOD is found: how long to keep re-checking its
/// mute status, and how often. Mutes usually land within minutes; 2 h covers
/// the slow tail.
pub(super) const MUTE_WATCH_POLLS: u32 = 40;
pub(super) const MUTE_WATCH_INTERVAL_SECS: u64 = 180;

/// Background task that polls Helix `/videos` for the Twitch VOD produced by a
/// just-finished recording, on the [`vod_poll_delay_secs`] schedule (immediate,
/// then fast, then backed off — [`VOD_TOTAL_POLLS`] attempts). Marks the
/// recording `not_published` if no matching VOD appears. When a clean VOD is
/// found it keeps watching for a late DMCA mute (see [`watch_vod_mute`]).
#[allow(clippy::too_many_arguments)]
pub(super) async fn check_twitch_vod(
    ctx: Arc<DetectContext>,
    store: Arc<Store>,
    events: EventTx,
    manual_tx: mpsc::UnboundedSender<ManualCommand>,
    rec_id: i64,
    login: String,
    stream_id: Option<String>,
    went_live_at: Option<i64>,
) {
    let (client_id, token) = match ctx.twitch_helix_auth().await {
        Ok(t) => t,
        Err(e) => {
            warn!(rec_id, "VOD check: Twitch auth unavailable: {e:#}");
            return;
        }
    };
    let user_id = match ctx.twitch_user_id(&client_id, &token, &login).await {
        Some(id) => id,
        None => {
            warn!(rec_id, login, "VOD check: could not resolve user_id");
            return;
        }
    };

    for poll in 0..VOD_TOTAL_POLLS {
        tokio::time::sleep(Duration::from_secs(vod_poll_delay_secs(poll))).await;
        match poll_twitch_vod(
            &ctx.http_client(),
            &client_id,
            &token,
            &user_id,
            stream_id.as_deref(),
            went_live_at,
        )
        .await
        {
            Ok(Some((vod_id, muted_secs, views))) => {
                let _ = store.set_recording_vod_found(rec_id, &vod_id, muted_secs);
                if views > 0 {
                    let _ = store.set_recording_vod_views(rec_id, views);
                }
                let _ = events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
                info!(rec_id, vod_id, muted_secs, "{} VOD found", Platform::Twitch.tag());
                let archive_on = archive_download_enabled(&store, rec_id);
                if muted_secs > 0 {
                    // A muted VOD is silenced — never a plain download. Un-mute via
                    // the CDN recovery, flag it, and (archive case) raise an issue.
                    if archive_on {
                        let _ = store.set_recording_vod_dl(rec_id, "muted", None);
                        let channel = archive_channel_name(&store, rec_id).unwrap_or_else(|| login.clone());
                        let _ = events.send(AppEvent::VodMuted {
                            recording_id: rec_id,
                            channel,
                            muted_secs,
                        });
                        spawn_auto_recovery(&ctx, &store, &events, rec_id);
                    } else if setting_true(&store, crate::recovery::K_AUTO_RECOVER_MUTED) {
                        spawn_auto_recovery(&ctx, &store, &events, rec_id);
                    }
                } else {
                    if archive_on {
                        // Clean published VOD — download it (alongside / replace)
                        // NOW, before a late mute can scrub the originals.
                        enqueue_vod_archive(&store, &manual_tx, rec_id, &crate::vod_archive::twitch_vod_url(&vod_id));
                    }
                    // Mutes are applied minutes AFTER publication — keep watching.
                    watch_vod_mute(&ctx, &store, &events, rec_id, &login, &vod_id).await;
                }
                return;
            }
            Ok(None) => {} // not yet available
            Err(e) => warn!(rec_id, "VOD poll error: {e:#}"),
        }
    }

    let _ = store.set_recording_vod_not_published(rec_id);
    let _ = events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
    info!(rec_id, login, "{} VOD not published after polling timeout", Platform::Twitch.tag());
    // Auto-recover a deleted VOD (no published archive) when enabled.
    if setting_true(&store, crate::recovery::K_AUTO_RECOVER_DELETED) {
        spawn_auto_recovery(&ctx, &store, &events, rec_id);
    }
}

/// Read a boolean setting (`"true"`/`"1"` = on), defaulting to `false`.
pub(super) fn setting_true(store: &Store, key: &str) -> bool {
    matches!(
        store.get_setting(key).ok().flatten().as_deref(),
        Some("true") | Some("1")
    )
}

/// Whether the post-stream VOD-download feature resolves ON for a recording
/// (global < channel < instance).
pub(super) fn archive_download_enabled(store: &Store, rec_id: i64) -> bool {
    store
        .recording_replace_info(rec_id)
        .ok()
        .flatten()
        .map(|(channel_id, monitor_id, _, _)| {
            crate::vod_archive::effective_vod_download(store, channel_id, monitor_id)
        })
        .unwrap_or(false)
}

/// The display channel name for a recording (for the muted-VOD notification).
pub(super) fn archive_channel_name(store: &Store, rec_id: i64) -> Option<String> {
    let (monitor_id, _) = store.get_recording_paths(rec_id).ok().flatten()?;
    store
        .get_monitor_with_channel(monitor_id)
        .ok()
        .flatten()
        .map(|mw| mw.channel.name)
}

/// Enqueue a detached yt-dlp download of a recording's published VOD (yt-dlp →
/// MKV) as `{live_stem}.vod.mkv` in the recording's output dir, link it to the
/// recording (`vod_dl_video_id`), and start it via the command bus. The completion
/// hook ([`Supervisor::finalize_vod_archive`]) handles alongside/replace.
pub(super) fn enqueue_vod_archive(
    store: &Store,
    manual_tx: &mpsc::UnboundedSender<ManualCommand>,
    rec_id: i64,
    vod_url: &str,
) -> bool {
    let Ok(Some((monitor_id, output_path))) = store.get_recording_paths(rec_id) else {
        return false;
    };
    let Ok(Some(mw)) = store.get_monitor_with_channel(monitor_id) else {
        return false;
    };
    let out = Path::new(&output_path);
    let Some(dir) = out.parent().map(|p| p.to_string_lossy().into_owned()).filter(|d| !d.is_empty())
    else {
        warn!(rec_id, "vod archive: recording has no output dir");
        return false;
    };
    let live_stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("rec_{rec_id}"));
    let m = &mw.monitor;
    let video = crate::models::Video {
        id: 0,
        url: vod_url.to_string(),
        title: format!("VOD · {} · rec #{rec_id}", mw.channel.name),
        channel: mw.channel.name.clone(),
        platform: m.platform(),
        tool: crate::models::Tool::YtDlp,
        tool_binary: String::new(),
        quality: if m.quality.trim().is_empty() { "best".into() } else { m.quality.clone() },
        output_dir: dir,
        // Literal stem (no tokens) → the file is `{live_stem}.vod.mkv`, distinct
        // from the live `{live_stem}.mkv`.
        filename_template: format!("{live_stem}.vod"),
        auth_kind: m.auth_kind,
        auth_value: m.auth_value.clone(),
        audio_tracks: String::new(),
        subtitle_tracks: String::new(),
        chat_log: false,
        extra_args: String::new(),
        auto_title: false,
        status: "queued".into(),
        output_path: String::new(),
        bytes: 0,
        created_at: now_unix(),
        exit_code: None,
        log_excerpt: String::new(),
        started_at: None,
        ended_at: None,
    };
    match store.insert_video(&video) {
        Ok(id) => {
            let _ = store.set_recording_vod_dl(rec_id, "downloading", Some(id));
            let _ = manual_tx.send(ManualCommand::StartVideo(id));
            true
        }
        Err(e) => {
            warn!(rec_id, "vod archive: insert_video failed: {e:#}");
            false
        }
    }
}

/// Spawn a recovery for a tracked recording, seeded from its stored broadcast id +
/// go-live time. No-op when the recording lacks a stream id (can't derive the URL)
/// or is past the ~60-day CDN window.
pub(super) fn spawn_auto_recovery(ctx: &Arc<DetectContext>, store: &Arc<Store>, events: &EventTx, rec_id: i64) {
    let Ok(Some(seed)) = store.recording_recovery_seed(rec_id) else {
        return;
    };
    if seed.stream_id.is_empty() {
        return;
    }
    // Don't stack a second recovery on one already running/succeeded (the
    // bulk scan may have gotten to it first).
    let state = store.recording_recovery_state(rec_id).ok().flatten();
    if !matches!(state.as_deref(), None | Some("failed")) {
        return;
    }
    if now_unix() - seed.start_epoch > 60 * 86_400 {
        return; // past the CDN retention window
    }
    let Some(login) = crate::detectors::twitch_login(&seed.monitor_url) else {
        return;
    };
    let inputs = crate::recovery::RecoveryInputs {
        login,
        broadcast_id: seed.stream_id,
        start_epoch: seed.start_epoch,
        went_live_approx: seed.went_live_approx,
        vod_id: seed.vod_id,
    };
    let quality = store
        .get_setting(crate::recovery::K_RECOVERY_QUALITY)
        .ok()
        .flatten()
        .unwrap_or_default();
    let (client, store, events) = (ctx.http_client(), store.clone(), events.clone());
    tokio::spawn(async move {
        let task_id = crate::events::next_task_id();
        crate::recovery::run_recovery(
            client,
            store,
            events,
            inputs,
            quality,
            crate::recovery::RecoverySink::Recording(rec_id),
            seed.deleted,
            task_id,
        )
        .await;
    });
}

/// Pick the recording's VOD out of a Helix `/videos` page (newest first).
///
/// When the recording knows its `stream_id`, only a video whose own
/// `stream_id` matches is accepted — Helix archive videos carry the
/// originating broadcast id, so this is exact. Crucially, a known-but-absent
/// stream id returns `None` (VOD not published yet / deleted) instead of
/// falling back to the window: rec 652 (2026-07-13) had the NEXT broadcast's
/// VOD — published 1h57m after go-live, inside the 2 h window and sorted
/// first — downloaded in place of the real one, which only the finalize
/// sanity check caught. The `created_at` window remains as the fallback for
/// recordings with no stream id (EventSub-less detections, old rows).
pub(super) fn match_twitch_vod(
    data: &[serde_json::Value],
    stream_id: Option<&str>,
    went_live_at: Option<i64>,
) -> Option<(String, i64, i64)> {
    let entry = |item: &serde_json::Value| -> Option<(String, i64, i64)> {
        let vod_id = item["id"].as_str()?;
        let muted_secs: i64 = item["muted_segments"]
            .as_array()
            .map(|segs| segs.iter().filter_map(|s| s["duration"].as_i64()).sum())
            .unwrap_or(0);
        // View count rides the same response — free data.
        let views = item["view_count"].as_i64().unwrap_or(0);
        Some((vod_id.to_string(), muted_secs, views))
    };
    if let Some(want) = stream_id.filter(|s| !s.is_empty()) {
        return data
            .iter()
            .find(|item| item["stream_id"].as_str() == Some(want))
            .and_then(entry);
    }
    for item in data {
        let Some(created_at_str) = item["created_at"].as_str() else {
            continue;
        };
        let Some(created_ts) = crate::detectors::parse_rfc3339(created_at_str) else {
            continue;
        };
        let matches = match went_live_at {
            Some(wl) => (created_ts - wl).abs() <= crate::vod_archive::VOD_MATCH_WINDOW_SECS,
            None => true, // no anchor — accept the most recent archive
        };
        if matches {
            return entry(item);
        }
    }
    None
}

/// Query Helix `/helix/videos` for the streamer's most recent archive VODs and
/// find the recording's one (see [`match_twitch_vod`] — exact `stream_id`
/// match when known, `created_at` window fallback otherwise). Returns
/// `Some((vod_id, muted_secs))` on match, `None` if no matching VOD exists
/// yet, or an error on a transient API failure.
pub(super) async fn poll_twitch_vod(
    client: &reqwest::Client,
    client_id: &str,
    token: &str,
    user_id: &str,
    stream_id: Option<&str>,
    went_live_at: Option<i64>,
) -> anyhow::Result<Option<(String, i64, i64)>> {
    use anyhow::bail;
    let resp = client
        .get("https://api.twitch.tv/helix/videos")
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .query(&[("user_id", user_id), ("type", "archive"), ("first", "20")])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Helix /videos: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let Some(data) = v["data"].as_array() else {
        return Ok(None);
    };
    Ok(match_twitch_vod(data, stream_id, went_live_at))
}

/// One-shot exact VOD lookup for a recording whose broadcast id is known:
/// auth → user id → Helix `/videos` → `stream_id` match (no window fallback).
/// `None` on any failure — callers fall back to whatever they already have.
pub(super) async fn resolve_twitch_vod_by_stream(
    ctx: &Arc<DetectContext>,
    monitor_url: &str,
    stream_id: &str,
) -> Option<(String, i64)> {
    let login = crate::detectors::twitch_login(monitor_url)?;
    let (client_id, token) = ctx.twitch_helix_auth().await.ok()?;
    let user_id = ctx.twitch_user_id(&client_id, &token, &login).await?;
    poll_twitch_vod(&ctx.http_client(), &client_id, &token, &user_id, Some(stream_id), None)
        .await
        .ok()
        .flatten()
        .map(|(id, muted, _views)| (id, muted))
}

/// Keep re-checking a freshly-published, currently-clean VOD for a late DMCA
/// mute (every [`MUTE_WATCH_INTERVAL_SECS`] for up to [`MUTE_WATCH_POLLS`]
/// polls, ~2 h). On the first mute detection:
///  - always record the muted seconds (narrow setter — never clobbers
///    `vod_state`/`vod_id`);
///  - archive already `'archived'`/`'replaced'` → leave the state alone: the
///    download finished BEFORE the mute, so our copy has the original audio
///    (the UI shows a "pre-mute" badge from `vod_muted_secs`);
///  - otherwise (no download, still downloading, or failed) → the standard
///    muted flow: state `'muted'`, `VodMuted` issue, CDN auto-recovery. A
///    download still in flight keeps its `'muted'` label when it completes
///    via `finalize_vod_archive`'s re-check — correct, since a mid-mute
///    download may already contain silenced segments.
pub(super) async fn watch_vod_mute(
    ctx: &Arc<DetectContext>,
    store: &Arc<Store>,
    events: &EventTx,
    rec_id: i64,
    login: &str,
    vod_id: &str,
) {
    for _ in 0..MUTE_WATCH_POLLS {
        tokio::time::sleep(Duration::from_secs(MUTE_WATCH_INTERVAL_SECS)).await;
        // Re-resolve auth each poll — the watch outlives short-lived tokens.
        let Ok((client_id, token)) = ctx.twitch_helix_auth().await else {
            continue;
        };
        let muted_secs = match poll_twitch_vod_muted(&ctx.http_client(), &client_id, &token, vod_id).await
        {
            Ok(Some((m, views))) => {
                // The view count rides every poll — keep it fresh while the
                // watch runs (the closest thing to a final per-VOD figure).
                if views > 0 {
                    let _ = store.set_recording_vod_views(rec_id, views);
                }
                m
            }
            Ok(None) => return, // VOD delisted — nothing left to watch
            Err(e) => {
                warn!(rec_id, vod_id, "mute watch poll error: {e:#}");
                continue;
            }
        };
        if muted_secs <= 0 {
            continue;
        }
        info!(
            rec_id,
            vod_id,
            muted_secs,
            "published {} VOD was muted after stream end",
            Platform::Twitch.tag()
        );
        let _ = store.set_recording_vod_muted_secs(rec_id, muted_secs);
        let (state, dl_video_id) = store
            .recording_vod_dl(rec_id)
            .ok()
            .flatten()
            .unwrap_or((None, None));
        match state.as_deref().unwrap_or_default() {
            "archived" | "replaced" => {} // pre-mute archive in hand — the race was won
            _ if archive_download_enabled(store, rec_id) => {
                // Keep the video link: an in-flight download's completion hook
                // looks the recording up BY that link (and re-checks the mute).
                let _ = store.set_recording_vod_dl(rec_id, "muted", dl_video_id);
                let channel =
                    archive_channel_name(store, rec_id).unwrap_or_else(|| login.to_string());
                let _ = events.send(AppEvent::VodMuted {
                    recording_id: rec_id,
                    channel,
                    muted_secs,
                });
                spawn_auto_recovery(ctx, store, events, rec_id);
            }
            _ => {
                if setting_true(store, crate::recovery::K_AUTO_RECOVER_MUTED) {
                    spawn_auto_recovery(ctx, store, events, rec_id);
                }
            }
        }
        let _ = events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
        return; // first detection ends the watch
    }
}

/// Current `(muted-segment seconds, view count)` for a KNOWN VOD via Helix
/// `/videos?id=`. `Ok(None)` = the VOD is no longer listed (deleted/expired).
pub(super) async fn poll_twitch_vod_muted(
    client: &reqwest::Client,
    client_id: &str,
    token: &str,
    vod_id: &str,
) -> anyhow::Result<Option<(i64, i64)>> {
    use anyhow::bail;
    let resp = client
        .get("https://api.twitch.tv/helix/videos")
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .query(&[("id", vod_id)])
        .send()
        .await?;
    // Helix answers 404 for an unknown/deleted VOD id.
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("Helix /videos?id: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let Some(item) = v["data"].as_array().and_then(|d| d.first()) else {
        return Ok(None);
    };
    let muted: i64 = item["muted_segments"]
        .as_array()
        .map(|segs| segs.iter().filter_map(|s| s["duration"].as_i64()).sum())
        .unwrap_or(0);
    Ok(Some((muted, item["view_count"].as_i64().unwrap_or(0))))
}

impl Supervisor {
    /// Mark a Twitch recording as VOD-pending and spawn the background poller.
    /// No-op for non-Twitch platforms, or statuses that imply the stream had
    /// already ended before capture began (`ended`).
    /// When `went_live_approx` is true the stored go-live time is our detection
    /// clock rather than the platform-reported start — pass `None` to the VOD
    /// matcher so it falls back to "most recent archive" instead of a stale anchor.
    pub(super) fn schedule_vod_check(
        &self,
        rec_id: i64,
        platform: Platform,
        status: &str,
        monitor_url: &str,
        went_live_at: Option<i64>,
        went_live_approx: bool,
    ) {
        if platform != Platform::Twitch || status == "ended" {
            return;
        }
        let Some(login) = crate::detectors::twitch_login(monitor_url) else {
            return;
        };
        let anchor = if went_live_approx { None } else { went_live_at };
        // The broadcast id makes the Helix match exact (see match_twitch_vod).
        let stream_id = self
            .store
            .monitor_id_for_recording(rec_id)
            .ok()
            .flatten()
            .and_then(|(_, sid)| sid)
            .filter(|s| !s.is_empty());
        let _ = self.store.set_recording_vod_pending(rec_id);
        tokio::spawn(check_twitch_vod(
            Arc::clone(&self.ctx),
            Arc::clone(&self.store),
            self.events.clone(),
            self.manual_tx.clone(),
            rec_id,
            login,
            stream_id,
            anchor,
        ));
    }

    /// Post-stream published-VOD download for a **YouTube/Kick** recording (Twitch
    /// is handled inside [`check_twitch_vod`]). No-op unless the feature resolves ON
    /// (global < channel < instance). Waits for the platform's VOD to be ready, then
    /// enqueues a detached yt-dlp download linked to the recording.
    pub(super) fn schedule_vod_archive(
        &self,
        rec_id: i64,
        row: &MonitorWithChannel,
        went_live_at: Option<i64>,
        status: &str,
    ) {
        if status == "ended" {
            return;
        }
        let platform = row.monitor.platform();
        if !matches!(platform, Platform::YouTube | Platform::Kick) {
            return;
        }
        if !crate::vod_archive::effective_vod_download(&self.store, row.monitor.channel_id, row.monitor.id) {
            return;
        }
        // stream_id (== the YouTube video id) travels on the recording row.
        let stream_id = self
            .store
            .monitor_id_for_recording(rec_id)
            .ok()
            .flatten()
            .and_then(|(_, sid)| sid);
        let _ = self.store.set_recording_vod_dl(rec_id, "downloading", None);
        let (store, manual_tx, ctx) = (self.store.clone(), self.manual_tx.clone(), self.ctx.clone());
        let monitor_url = row.monitor.url.clone();
        tokio::spawn(async move {
            let url = match platform {
                Platform::YouTube => {
                    // The VOD is the same video; give post-live processing some time.
                    tokio::time::sleep(Duration::from_secs(VOD_POLL_INTERVAL_SECS)).await;
                    stream_id.as_deref().map(crate::vod_archive::youtube_vod_url)
                }
                Platform::Kick => {
                    let Some(slug) = crate::vod_archive::kick_slug(&monitor_url) else {
                        let _ = store.set_recording_vod_dl(rec_id, "failed", None);
                        return;
                    };
                    let client = ctx.http_client();
                    let mut found = None;
                    for _ in 0..VOD_MAX_POLLS {
                        tokio::time::sleep(Duration::from_secs(VOD_POLL_INTERVAL_SECS)).await;
                        if let Some(u) = crate::vod_archive::resolve_kick_vod(&client, &slug, went_live_at).await {
                            found = Some(u);
                            break;
                        }
                    }
                    found
                }
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

    /// Completion hook for a VOD-archive download: record the file on the recording
    /// and, when replace-on-success resolves ON and the VOD isn't muted, swap it in
    /// for the live capture. A no-op for ordinary (non-archive) video downloads.
    /// True when a completed VOD-archive download looks like the real video:
    /// ffprobe can read a duration, and (when an expectation is derivable from
    /// the live capture or the recording's wall-clock span) that duration is
    /// at least 90% of it. The VOD can legitimately be LONGER than the live
    /// capture (late join), never dramatically shorter.
    async fn vod_archive_sane(&self, rec_id: i64, final_path: &Path) -> bool {
        let Some(dur) = media_duration_secs(final_path).await else {
            return false;
        };
        let expected = match self.store.recording_duration_hint(rec_id).ok().flatten() {
            Some((live_path, went_live_at, ended_at)) => {
                let live = if live_path.is_empty() {
                    None
                } else {
                    media_duration_secs(Path::new(&live_path)).await
                };
                live.or(match (went_live_at, ended_at) {
                    (Some(a), Some(b)) if b > a => Some(b - a),
                    _ => None,
                })
            }
            None => None,
        };
        match expected {
            Some(exp) if exp > 0 => dur * 10 >= exp * 9,
            _ => true, // no expectation derivable — a probeable video is enough
        }
    }

    pub(super) async fn finalize_vod_archive(&self, video_id: i64, final_path: &Path, status: &str) {
        let Ok(Some(rec_id)) = self.store.recording_for_vod_video(video_id) else {
            return;
        };
        if status != "completed" {
            let _ = self.store.set_recording_vod_dl(rec_id, "failed", Some(video_id));
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
            return;
        }
        // Belt-and-braces sanity check before this file is trusted as the
        // archive (and possibly replaces the live capture): it must be a
        // probeable video of plausible length. "completed" status alone once
        // let a promoted 10 KB `.vod.log` get archived (2026-07-06 incident).
        if !self.vod_archive_sane(rec_id, final_path).await {
            let _ = self.store.set_recording_vod_dl(rec_id, "failed", Some(video_id));
            let _ = self.store.set_video_status(video_id, "failed");
            let _ = self.events.send(AppEvent::Error {
                context: "VOD archive".into(),
                message: format!(
                    "downloaded VOD failed the sanity check (not a plausible video): {}",
                    final_path.display()
                ),
            });
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
            return;
        }
        let vod_path = final_path.to_string_lossy().into_owned();
        let replace_info = self.store.recording_replace_info(rec_id).ok().flatten();
        // A DMCA-muted VOD stays flagged "muted" even after a manual "Download
        // VOD now": the file is recorded, but the Issues entry must not vanish
        // as if a clean archive landed (the downloaded copy has silenced audio).
        // Ordering invariant vs. the post-find mute watcher: a mute recorded
        // BEFORE/DURING the download is seen here and labels the file 'muted';
        // a mute detected AFTER this hook ran never relabels it — the watcher
        // skips terminal 'archived'/'replaced' states (pre-mute copy is good).
        let muted = replace_info
            .as_ref()
            .is_some_and(|(_, _, _, m)| m.unwrap_or(0) > 0);
        let _ = self.store.set_recording_vod_archived(
            rec_id,
            &vod_path,
            if muted { "muted" } else { "archived" },
        );

        let Some((channel_id, monitor_id, live_path, _)) = replace_info else {
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
            return;
        };
        let replace = !muted
            && !live_path.is_empty()
            && crate::vod_archive::effective_vod_replace(&self.store, channel_id, monitor_id);
        if replace {
            // Only now that the VOD is confirmed good do we touch the live
            // file — and never destructively until the swap is complete: the
            // live capture is first RENAMED aside (instantly reversible), the
            // VOD renamed onto the live stem (so sidecars stay matched), and
            // only then is the backup deleted. A failure at any step restores
            // the original layout instead of losing the live capture (the old
            // delete-then-rename order lost it if the rename failed, e.g. an
            // AV scanner briefly holding the fresh .vod.mkv).
            let live = PathBuf::from(&live_path);
            let backup = live.with_extension("pre-vod.bak");
            match crate::iomon::fs::rename(Cat::Promote, &live, &backup).await {
                Ok(()) => match crate::iomon::fs::rename(Cat::Promote, final_path, &live).await {
                    Ok(()) => {
                        // The displaced live capture follows the configured
                        // disposal method (trash / Recycle Bin / permanent);
                        // a failed disposal just leaves the .pre-vod.bak.
                        let how = match crate::disposal::dispose_media(
                            &self.store,
                            channel_id,
                            monitor_id,
                            &backup,
                        )
                        .await
                        {
                            Ok(d) => d.describe(),
                            Err(e) => {
                                warn!(rec_id, "vod archive: displaced live capture disposal failed: {e:#} ({} left behind)", backup.display());
                                "left behind (disposal failed)"
                            }
                        };
                        let live_s = live.to_string_lossy().into_owned();
                        let _ = self.store.update_recording_output_path(rec_id, &live_s);
                        let _ = self.store.set_recording_vod_archived(rec_id, &live_s, "replaced");
                        info!(rec_id, "vod archive: replaced live recording with the published VOD (original {how})");
                    }
                    Err(e) => {
                        // Put the live capture back; the VOD stays alongside.
                        let _ = crate::iomon::fs::rename(Cat::Promote, &backup, &live).await;
                        warn!(rec_id, "vod archive: replace rename failed: {e:#} (live restored, VOD kept alongside)");
                    }
                },
                Err(e) => warn!(rec_id, "vod archive: could not stage live for replace: {e:#} (kept alongside)"),
            }
        }
        let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
    }

    /// Startup repair pass for post-stream VOD archives, run after
    /// `reconcile_detached`:
    ///  1. **Replay** — a download that completed (possibly under an older
    ///     binary whose reattach path lacked the archive hook) but whose
    ///     recording is still `'downloading'`/`'failed'` gets its
    ///     [`Self::finalize_vod_archive`] re-run; the sanity gate inside
    ///     decides archived/replace vs failed.
    ///  2. **Audit** — `'archived'` rows pointing at a bogus file (empty,
    ///     wrong extension, missing, tiny, or unprobeable — e.g. the promoted
    ///     `.vod.log` of the 2026-07-06 incident) are demoted to `'muted'`
    ///     (when Helix says the VOD is now muted; auto-recovery kicks in) or
    ///     `'failed'`. The bogus file itself is left on disk.
    pub async fn reconcile_vod_archives(&self) {
        for (rec_id, video_id, path) in
            self.store.vod_archive_replay_candidates().unwrap_or_default()
        {
            if path.is_empty() {
                continue;
            }
            info!(rec_id, video_id, "vod archive reconcile: replaying completed download");
            self.finalize_vod_archive(video_id, Path::new(&path), "completed").await;
        }

        let mut helix: Option<(String, String)> = None;
        for (rec_id, video_id, path, stored_muted, vod_id) in
            self.store.vod_archived_rows().unwrap_or_default()
        {
            let p = Path::new(&path);
            let cheap_suspicious = path.is_empty()
                || !path.to_ascii_lowercase().ends_with(".mkv")
                || file_len(p).await < 1024 * 1024;
            if !cheap_suspicious {
                continue;
            }
            // Wrong-looking but probeable (e.g. a healthy .webm) is left alone.
            if !path.is_empty() && media_duration_secs(p).await.is_some() {
                continue;
            }
            warn!(
                rec_id,
                path, "vod archive reconcile: archived file is not a plausible video (kept on disk)"
            );
            // Re-check the mute state now — the VOD may have been muted after
            // the bogus archive landed (exactly the 2026-07-06 sequence).
            let mut muted_secs = stored_muted;
            if let Some(vid) = vod_id.as_deref() {
                if helix.is_none() {
                    helix = self.ctx.twitch_helix_auth().await.ok();
                }
                if let Some((cid, tok)) = helix.as_ref()
                    && let Ok(Some((m, _views))) =
                        poll_twitch_vod_muted(&self.ctx.http_client(), cid, tok, vid).await
                {
                    muted_secs = m;
                    let _ = self.store.set_recording_vod_muted_secs(rec_id, m);
                }
            }
            if let Some(vid) = video_id {
                let _ = self.store.set_video_status(vid, "failed");
            }
            if muted_secs > 0 {
                let _ = self.store.set_recording_vod_dl(rec_id, "muted", video_id);
                let channel =
                    archive_channel_name(&self.store, rec_id).unwrap_or_else(|| "?".into());
                let _ = self.events.send(AppEvent::VodMuted {
                    recording_id: rec_id,
                    channel,
                    muted_secs,
                });
                spawn_auto_recovery(&self.ctx, &self.store, &self.events, rec_id);
            } else {
                let _ = self.store.set_recording_vod_dl(rec_id, "failed", video_id);
            }
            let _ = self.events.send(AppEvent::RecordingUpdated { recording_id: rec_id });
        }

        // Head-concat crash healing: joins that were pending when the app died.
        for rec_id in self.store.recordings_pending_head_concat().unwrap_or_default() {
            self.maybe_concat_backfill(rec_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use crate::models::{Channel, Container, DetectionMethod, Monitor, Tool};
    #[allow(unused_imports)]
    use crate::downloader::test_util::*;

    #[test]
    fn vod_poll_schedule_is_immediate_then_fast_then_backed_off() {
        // First check fires immediately at stream end (the mute race).
        assert_eq!(vod_poll_delay_secs(0), 0);
        // Fast phase: ~10 minutes of 25 s polls.
        let fast: u64 = (1..=24).map(vod_poll_delay_secs).sum();
        assert_eq!(fast, 600);
        // Backoff phase: 5-minute polls; total window stays around an hour.
        assert_eq!(vod_poll_delay_secs(25), 300);
        let total: u64 = (0..VOD_TOTAL_POLLS).map(vod_poll_delay_secs).sum();
        assert!((3600..=3900).contains(&total), "total {total}");
    }

    /// The rec-652 incident: the NEXT broadcast's VOD (newer, first in the
    /// Helix page, inside the 2 h created_at window) must not shadow the
    /// recording's own VOD when the broadcast id is known — and a known id
    /// with no matching VOD means "not published", never a window neighbor.
    #[test]
    fn twitch_vod_match_prefers_stream_id_over_window() {
        // Newest-first Helix page, mirroring the real incident: REACTION VOD
        // (stream 999) created 1h57m after the Mario stream (id 320421899867)
        // went live at t=1783811671.
        let data = vec![
            serde_json::json!({
                "id": "2817892940", "stream_id": "999",
                "created_at": "2026-07-12T01:11:35Z", // ~went_live + 7024s
                "muted_segments": null,
            }),
            serde_json::json!({
                "id": "2817816829", "stream_id": "320421899867",
                "created_at": "2026-07-11T23:14:35Z",
                "muted_segments": [{"duration": 180}, {"duration": 30}],
                "view_count": 4321,
            }),
        ];
        // Known stream id → exact match (and muted segments summed + view
        // count carried), even though the wrong VOD sorts first and sits
        // inside the window.
        let wl = Some(crate::detectors::parse_rfc3339("2026-07-11T23:14:31Z").unwrap());
        assert_eq!(
            match_twitch_vod(&data, Some("320421899867"), wl),
            Some(("2817816829".into(), 210, 4321))
        );
        // Known id, VOD not in the page → not published, no neighbor-grabbing.
        assert_eq!(match_twitch_vod(&data, Some("111"), wl), None);
        // No id (legacy row) → old window behavior: newest within the window.
        assert_eq!(match_twitch_vod(&data, None, wl), Some(("2817892940".into(), 0, 0)));
        // No id and no anchor → most recent archive.
        assert_eq!(match_twitch_vod(&data, None, None), Some(("2817892940".into(), 0, 0)));
    }
}
