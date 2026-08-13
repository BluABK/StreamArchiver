//! Chat capture for broadcasts that are **not** being recorded.
//!
//! Auto-record is a disk-space control: turning it off for a channel means
//! "don't spend 30 GB on this stream", not "I don't care what happened in it".
//! Chat is a few MB of JSON at worst, and — unlike the video — it is
//! *unrecoverable* once the broadcast ends: Twitch keeps no public transcript
//! and YouTube's live chat replay dies with the stream (or with the VOD).
//! So when a monitored channel goes live and nothing is capturing it, this
//! module still captures the chat, on by default ([`K_CHAT_NO_RECORD`]).
//!
//! Scope — deliberately just the Auto-off case. A chat-only session is bolted
//! onto the "seen live but not recorded" session `try_begin` already opens
//! (see `Store::insert_not_recorded_session`), which gives it a start, an end,
//! and a take-shaped DB row to hang the sidecar path off for the chat replay.
//! The other two "live but not recording" paths deliberately DON'T get one:
//! a blacklist-trigger veto and a manual "Stop" hold are both the user saying
//! *don't capture this broadcast*, not *don't spend the disk*.
//!
//! Both platforms' loggers are reused as-is: Twitch's native anonymous IRC
//! logger ([`crate::chat::log_twitch_chat`]) and YouTube's yt-dlp `live_chat`
//! sidecar ([`Supervisor::run_chat_download`]) — the same code paths, files
//! and formats a recorded take produces, so everything downstream (the chat
//! replay popup, the subdirectory sweep, the 💬 badge) works unchanged.

use super::*;

/// Settings key: `"0"` disables chat capture for not-recorded broadcasts
/// (default on).
pub const K_CHAT_NO_RECORD: &str = "chat_log_without_recording";

pub(super) fn chat_without_recording_enabled(store: &Store) -> bool {
    store.get_setting(K_CHAT_NO_RECORD).ok().flatten().as_deref() != Some("0")
}

/// How often the session watcher re-checks whether the broadcast is still
/// open. Short enough that the sidecar closes promptly when the stream ends,
/// long enough to be one trivial indexed query per live-but-unrecorded
/// channel per minute.
const SESSION_POLL_SECS: u64 = 15;

/// How often the watcher wakes to check the *local* stop flags (shutdown, a
/// user "Stop chat download"). Kept at a second so app shutdown isn't held up
/// by a sleeping watcher — `stop_all_recordings` blocks until `active_chats`
/// drains, and a chat-only Twitch session occupies a slot in it.
const TICK: Duration = Duration::from_secs(1);

/// A YouTube chat-only attempt that exits (unprompted — see
/// `run_ytdlp_chat_only`'s `select!`) within this long of spawning is treated
/// as a fast failure rather than a legitimately short-lived chat, and backs
/// off. Comfortably above yt-dlp's own near-instant "channel is not currently
/// live" exit (~1-2s observed), comfortably below any real chat session.
const CHAT_ONLY_FAST_FAIL_THRESHOLD: Duration = Duration::from_secs(15);
/// How long a fast-failing monitor waits before the next chat-only attempt,
/// instead of retrying on every ordinary poll (~65-70s). Covers the observed
/// case — a broadcast some other detection method still considers live but
/// yt-dlp categorically can't see (no membership entitlement) for its whole
/// runtime — without needing to positively identify *why* yt-dlp can't see
/// it.
const CHAT_ONLY_BACKOFF: Duration = Duration::from_secs(300);

/// Whether a finished chat-only attempt should trip the backoff: it must have
/// ended because the child process exited on its own (not because we stopped
/// it — shutdown, user action, or the not-recorded session closing), and it
/// must have done so quickly.
fn chat_only_fast_fail(exited_on_its_own: bool, elapsed: Duration) -> bool {
    exited_on_its_own && elapsed < CHAT_ONLY_FAST_FAIL_THRESHOLD
}

impl Supervisor {
    /// Start a chat-only capture for a live-but-not-recorded broadcast, if one
    /// is wanted and isn't already running. Called from `try_begin`'s Auto-off
    /// branch on every poll while the stream stays live, so it must be cheap
    /// and idempotent — the `chat_only` registry is what makes the repeat
    /// calls no-ops, and it's also what lets a session that was lost to an app
    /// restart come back on the next poll.
    ///
    /// `rec_id` is the not-recorded session this chat belongs to; its
    /// `started_at` (not "now") anchors the filename, so the name doesn't
    /// drift if the session is ever re-established mid-broadcast.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn maybe_start_chat_only(
        &self,
        row: &MonitorWithChannel,
        rec_id: i64,
        session_started_at: i64,
        stream_id: Option<&str>,
        stream_title: Option<&str>,
        stream_game: Option<&str>,
        went_live_at: Option<i64>,
    ) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // The per-instance "Log chat" toggle still rules: this feature only
        // decides whether that toggle keeps applying when nothing is being
        // recorded, never whether chat is captured at all.
        if !row.monitor.chat_log || !chat_without_recording_enabled(&self.store) {
            return;
        }
        if !Self::platform_can_chat_only(row) {
            return;
        }
        let monitor_id = row.monitor.id;
        // A real recording owns the monitor's chat: never run both. (The
        // recording path stops any chat-only session before it starts its
        // own — this is the other half of that interlock, covering a signal
        // that arrives while a capture is spinning up.)
        if self.active.lock().unwrap().contains_key(&monitor_id) {
            return;
        }
        // Nor alongside ANY other chat capture for this monitor — including a
        // yt-dlp chat sidecar this app didn't spawn but re-attached to at
        // startup (`DetachedKind::Chat`), which the `chat_only` registry below
        // knows nothing about because it's in-memory only.
        if self.active_chats.lock().unwrap().contains_key(&monitor_id) {
            return;
        }
        // The user stopped this broadcast's chat capture by hand — respect it
        // for the rest of the broadcast rather than restarting on this poll.
        if self.chat_only_user_stopped.lock().unwrap().get(&monitor_id) == Some(&rec_id) {
            return;
        }
        // A recent attempt failed fast (see CHAT_ONLY_FAST_FAIL_THRESHOLD) —
        // don't hammer the same wall on every poll until the cooldown lifts.
        if let Some(&until) = self.chat_only_backoff.lock().unwrap().get(&monitor_id)
            && Instant::now() < until
        {
            return;
        }
        if !self.chat_only.lock().unwrap().insert(monitor_id) {
            return; // already running
        }
        let title = stream_title
            .filter(|t| !t.is_empty())
            .unwrap_or(row.last_title.as_str());
        let game = stream_game
            .filter(|g| !g.is_empty())
            .unwrap_or(row.last_game.as_str());
        let this = self.clone();
        let (row, stream_id, title, game) = (
            row.clone(),
            stream_id.unwrap_or_default().to_string(),
            title.to_string(),
            game.to_string(),
        );
        tokio::spawn(async move {
            this.run_chat_only_session(
                row, rec_id, session_started_at, stream_id, title, game, went_live_at,
            )
            .await;
            this.chat_only.lock().unwrap().remove(&monitor_id);
        });
    }

    /// Whether this monitor has a chat logger at all — the same two
    /// conditions `spawn_chat_loggers` applies for a recorded take. Anything
    /// else (Kick, generic yt-dlp sites, a Twitch monitor is always fine)
    /// has no chat source, so a chat-only session would be an empty file.
    fn platform_can_chat_only(row: &MonitorWithChannel) -> bool {
        row.monitor.platform() == Platform::Twitch
            || (row.monitor.tool == Tool::YtDlp && row.monitor.platform() == Platform::YouTube)
    }

    /// Remember that the user stopped this monitor's chat-only capture, so
    /// `maybe_start_chat_only` doesn't restart it on the next poll. A no-op
    /// unless a chat-only session is what's actually running — a Stop on a
    /// *recording's* chat sidecar means only "stop this one", and must not
    /// suppress anything later.
    pub(super) fn note_chat_only_user_stop(&self, monitor_id: i64) {
        if !self.chat_only.lock().unwrap().contains(&monitor_id) {
            return;
        }
        if let Ok(Some((rec_id, _))) = self.store.open_not_recorded_session(monitor_id) {
            self.chat_only_user_stopped.lock().unwrap().insert(monitor_id, rec_id);
            info!(monitor_id, "chat-only: stopped by user for this broadcast");
        }
    }

    /// Stop the chat-only capture for `monitor_id` (if any) and wait for it to
    /// let go of its sidecar. Called by `record` before it starts a take: the
    /// broadcast is about to get a real recording, whose own chat logger owns
    /// the monitor's chat from here on.
    pub(super) async fn stop_chat_only(&self, monitor_id: i64) {
        if !self.chat_only.lock().unwrap().contains(&monitor_id) {
            return;
        }
        info!(monitor_id, "chat-only: superseded by a recording — stopping");
        // Same signal the user's own "Stop chat download" uses, so there's one
        // stop path for both platforms (see `run_chat_only_session`).
        self.stopping_chats.lock().unwrap().insert(monitor_id);
        /// Enough for a watcher tick plus the logger's own flush/exit; past
        /// that the recording starts anyway rather than being held up by a
        /// wedged sidecar (the same tradeoff `stop_record_watchers` makes).
        const STOP_GRACE: Duration = Duration::from_secs(8);
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        while tokio::time::Instant::now() < deadline {
            if !self.chat_only.lock().unwrap().contains(&monitor_id) {
                return;
            }
            // Re-issued every pass, not just once: the YouTube branch registers
            // its child's PID a moment AFTER the session appears in
            // `chat_only`, and `stop_chat_download` is a no-op until it has one
            // — a single up-front call could miss that window and leave an
            // orphan yt-dlp chat process running against the same monitor.
            self.stop_chat_download(monitor_id);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        warn!(monitor_id, "chat-only session didn't stop within {STOP_GRACE:?}; starting the recording anyway");
        // Drop the stop marker on the way out: the take's OWN chat sidecar is
        // about to register under this same monitor id, and a leftover entry
        // would make its clean end read as "stopped by user" (and, for the
        // yt-dlp path, mislabel the log line).
        self.stopping_chats.lock().unwrap().remove(&monitor_id);
    }

    /// One chat-only session: spawn the platform's logger, then supervise it
    /// until the broadcast ends.
    #[allow(clippy::too_many_arguments)]
    async fn run_chat_only_session(
        &self,
        row: MonitorWithChannel,
        rec_id: i64,
        session_started_at: i64,
        stream_id: String,
        title: String,
        game: String,
        went_live_at: Option<i64>,
    ) {
        let monitor_id = row.monitor.id;
        if row.monitor.output_dir.trim().is_empty() {
            warn!(monitor_id, "chat-only: monitor has no output directory; skipping");
            return;
        }
        // No `.cache\` staging like a capture gets: a chat sidecar is written
        // straight to its final dir (same as a recorded take's — the promote
        // step must never move a file a logger still has open). That dir is
        // the monitor's output dir, or its mirror under the dedicated chat
        // root when one is configured (`chat::chat_dir_for`).
        let dir = crate::chat::chat_dir_for(Path::new(&row.monitor.output_dir));
        if let Err(e) = crate::iomon::fs::create_dir_all(Cat::DirSetup, &dir).await {
            warn!(monitor_id, "chat-only: chat dir {}: {e:#}", dir.display());
            return;
        }
        let twitch = row.monitor.platform() == Platform::Twitch;
        let stem = chat_only_stem(
            &row,
            session_started_at,
            &stream_id,
            &title,
            &game,
            went_live_at.unwrap_or(0),
            &dir,
            twitch,
        );

        // Twitch writes `{stem}.chat.jsonl` itself; yt-dlp's `--write-subs`
        // REPLACES the `-o` value's extension with the subtitle's own (see
        // `supervisor.rs`'s matching comment) — the `-o` given below is
        // `{stem}.mkv` (no such file is ever created — `--skip-download`), so
        // the sidecar comes out at `{stem}.live_chat.json`, byte-identical in
        // shape to a recorded take's. Both are then found by the ordinary
        // companion sweep.
        let chat_path = if twitch {
            dir.join(format!("{stem}.chat.jsonl"))
        } else {
            dir.join(format!("{stem}.live_chat.json"))
        };
        // Point the not-recorded take at the sidecar so the chat replay can
        // find it — there's no video path to derive it from.
        if let Err(e) = self.store.set_recording_chat_path(rec_id, &chat_path.to_string_lossy()) {
            warn!(monitor_id, "chat-only: recording chat path: {e:#}");
        }
        info!(
            monitor_id,
            "chat-only capture for {} {} (not recording) -> {}",
            row.monitor.platform().tag(),
            row.channel.name,
            chat_path.display()
        );

        if twitch {
            self.run_twitch_chat_only(monitor_id, &row, &chat_path, &stream_id).await;
        } else {
            self.run_ytdlp_chat_only(monitor_id, &row, &dir, &stem).await;
        }
        info!(monitor_id, "chat-only capture ended");
    }

    /// Twitch half: the in-process anonymous IRC logger, run until the watcher
    /// says the broadcast is over, then flushed and joined.
    async fn run_twitch_chat_only(
        &self,
        monitor_id: i64,
        row: &MonitorWithChannel,
        chat_path: &Path,
        stream_id: &str,
    ) {
        // pid 0: the native logger has no process, but registering it here is
        // what gives a chat-only Twitch session the same 💬 badge, "Stop chat
        // download" action and shutdown accounting as the yt-dlp one. Nothing
        // tries to kill a 0 pid (`stop_all_recordings` filters it out, and
        // `stop_chat_download` skips the kill) — the watcher below translates
        // the resulting stop signal into `done` instead.
        self.active_chats.lock().unwrap().insert(monitor_id, 0);
        let _ = self.events.send(AppEvent::MonitorState {
            monitor_id,
            state: "chat_active".into(),
        });
        let done = Arc::new(AtomicBool::new(false));
        let mut task = tokio::spawn(crate::chat::log_twitch_chat(
            row.monitor.url.clone(),
            chat_path.to_path_buf(),
            done.clone(),
            self.shutdown.clone(),
            // Same live event capture (subs/bits/raids -> stream_event) a
            // recorded take gets: those events are as unrecoverable as the
            // chat itself, and they're per-monitor, not per-recording.
            Some(crate::chat::ChatEventCtx {
                store: self.store.clone(),
                monitor_id,
                stream_id: stream_id.to_string(),
                events: self.events.clone(),
            }),
        ));

        self.watch_chat_only_session(monitor_id).await;

        done.store(true, Ordering::SeqCst);
        /// The logger checks `done` between its 1s socket reads, so this only
        /// expires on a wedged connection — same failure mode, and the same
        /// bounded-then-abort answer, as `stop_record_watchers`.
        const FLUSH_GRACE: Duration = Duration::from_secs(10);
        if tokio::time::timeout(FLUSH_GRACE, &mut task).await.is_err() {
            warn!(monitor_id, "chat-only logger didn't stop within {FLUSH_GRACE:?} — aborting it");
            task.abort();
            let _ = task.await;
        }
        self.active_chats.lock().unwrap().remove(&monitor_id);
        self.stopping_chats.lock().unwrap().remove(&monitor_id);
        let _ = self.events.send(AppEvent::MonitorState { monitor_id, state: "idle".into() });
    }

    /// YouTube half: the same yt-dlp `live_chat` sidecar a recorded take
    /// spawns, just without a video capture beside it. Its `-o` value is the
    /// `{stem}.mkv` a recording would have written (no such file is created —
    /// `--skip-download`), so the sidecar lands at `{stem}.live_chat.json`
    /// (yt-dlp replaces the `-o` value's extension), identical in shape to a
    /// recorded take's.
    async fn run_ytdlp_chat_only(
        &self,
        monitor_id: i64,
        row: &MonitorWithChannel,
        dir: &Path,
        stem: &str,
    ) {
        let global_method =
            self.store.get_setting("download_auth_method").ok().flatten().unwrap_or_default();
        let global_browser =
            self.store.get_setting("cookies_browser").ok().flatten().unwrap_or_default();
        let auth = resolve_auth(row, &global_method, &global_browser);
        let ytdlp_global_args = split_args(
            &self.store.get_setting("ytdlp_default_args").ok().flatten().unwrap_or_default(),
        );
        let ytdlp_bins = load_ytdlp_bins(&self.store);
        let plan = build_chat_plan(
            row,
            &dir.join(format!("{stem}.mkv")),
            &auth,
            &ytdlp_global_args,
            &ytdlp_bins.system_program(),
        );
        let this = self.clone();
        let chat =
            tokio::spawn(async move { this.run_chat_download(monitor_id, Platform::YouTube, plan).await });
        // Two ways this ends, and the child owns its own bookkeeping either
        // way: yt-dlp exits by itself when the stream ends, or the watcher
        // decides the session is over and we kill it. Dropping the join handle
        // in the second arm only detaches the task (it isn't cancelled), so
        // `run_chat_download` still runs its cleanup after the kill.
        let started = Instant::now();
        let exited_on_its_own = tokio::select! {
            _ = chat => true,
            _ = self.watch_chat_only_session(monitor_id) => {
                self.stop_chat_download(monitor_id);
                false
            }
        };
        // yt-dlp died on its own within seconds of spawning — not stopped by
        // us, not a chat session that ran for a while and then wound down.
        // Some other detection method still thinks this broadcast is live
        // (the not-recorded session is still open, or this wouldn't have been
        // called), but yt-dlp categorically can't see it — back off instead
        // of repeating the same failed spawn on every poll for the rest of
        // the broadcast.
        if chat_only_fast_fail(exited_on_its_own, started.elapsed()) {
            self.chat_only_backoff
                .lock()
                .unwrap()
                .insert(monitor_id, Instant::now() + CHAT_ONLY_BACKOFF);
            warn!(
                monitor_id,
                "chat-only: yt-dlp exited immediately; backing off {CHAT_ONLY_BACKOFF:?} \
                 before the next attempt"
            );
        }
    }

    /// Block until this chat-only session should stop: app shutdown, a stop
    /// request (user action or a recording taking over), or the broadcast
    /// ending.
    ///
    /// "The broadcast ended" is read off the not-recorded session rather than
    /// tracked here, because that row is already closed correctly from all
    /// three directions — the scheduler's live→offline edge, an EventSub
    /// `stream.offline` push, and `insert_recording_row` when a real capture
    /// supersedes it. Tying the sidecar's lifetime to it means chat capture
    /// can't outlive (or under-live) the session it documents.
    async fn watch_chat_only_session(&self, monitor_id: i64) {
        let mut ticks: u64 = 0;
        loop {
            tokio::time::sleep(TICK).await;
            if self.shutdown.load(Ordering::SeqCst)
                || self.stopping_chats.lock().unwrap().contains(&monitor_id)
            {
                return;
            }
            ticks += 1;
            if !ticks.is_multiple_of(SESSION_POLL_SECS) {
                continue;
            }
            if matches!(self.store.open_not_recorded_session(monitor_id), Ok(None)) {
                return;
            }
        }
    }
}

/// Filename stem for a chat-only session — the monitor's own template, filled
/// the same way a recorded take's is, so these files sort and read like the
/// rest of the archive rather than looking like a separate species.
///
/// Two deliberate differences from [`build_plan`]'s: `{title}`/`{games}` are
/// filled from what detection already knows instead of the `title-tba` /
/// `games-tba` placeholders, because there is no post-capture rename pass to
/// resolve them later; and `{tool}`/`{mode}` read `chat`, since no capture
/// tool is involved.
#[allow(clippy::too_many_arguments)]
fn chat_only_stem(
    row: &MonitorWithChannel,
    started_at: i64,
    stream_id: &str,
    title: &str,
    game: &str,
    went_live: i64,
    dir: &Path,
    twitch: bool,
) -> String {
    let stem = monitor_stem(
        &row.monitor,
        &row.channel.name,
        started_at,
        (!stream_id.is_empty()).then_some(stream_id),
        title,
        row.recording_count,
        &resolved_quality(&row.monitor.quality),
        None,
        game,
        "chat",
        "chat",
        row.monitor.platform().as_str(),
        went_live,
    );
    let stem = stem_capped_for_child_path(dir, &stem);
    // Never append to (or clobber) an existing sidecar: a session that comes
    // back after an app restart gets its own file rather than reopening one
    // whose contents we can no longer vouch for. Probed against the extension
    // each platform's logger actually writes.
    unique_stem(dir, &stem, if twitch { "chat.jsonl" } else { "live_chat.json" }, None)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures, not app I/O paths
    use super::*;

    #[test]
    fn fast_fail_only_trips_on_an_unprompted_quick_exit() {
        // The Panko Ch. case (2026-08-13): yt-dlp itself exits in ~1-2s
        // because the account can't see the (members-only) broadcast.
        assert!(chat_only_fast_fail(true, Duration::from_secs(1)));
        // A long-lived session that eventually wound down on its own (the
        // broadcast genuinely ended) must not back off the next one.
        assert!(!chat_only_fast_fail(true, Duration::from_secs(60)));
        // We stopped it ourselves (shutdown/user/session-closed) — however
        // fast, that's not yt-dlp failing.
        assert!(!chat_only_fast_fail(false, Duration::from_millis(500)));
        assert!(!chat_only_fast_fail(false, Duration::from_secs(60)));
    }

    #[test]
    fn chat_without_recording_default_on_and_opt_out() {
        let store = Store::open_in_memory().unwrap();
        assert!(chat_without_recording_enabled(&store));
        store.set_setting(K_CHAT_NO_RECORD, "0").unwrap();
        assert!(!chat_without_recording_enabled(&store));
        // Anything other than an explicit "0" is on (matches K_AD_PROBE).
        store.set_setting(K_CHAT_NO_RECORD, "1").unwrap();
        assert!(chat_without_recording_enabled(&store));
    }

    /// Only the two platforms that actually have a chat logger qualify —
    /// otherwise a chat-only session would sit there writing nothing.
    #[test]
    fn only_platforms_with_a_chat_logger_qualify() {
        use crate::models::Container;
        let tw = test_util::row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        assert!(Supervisor::platform_can_chat_only(&tw), "Twitch: native IRC logger");

        let yt = test_util::row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        assert!(Supervisor::platform_can_chat_only(&yt), "YouTube + yt-dlp: live_chat sidecar");
        // YouTube captured with another tool has no chat source.
        let yt_ff = test_util::row(Tool::Ffmpeg, Container::Mkv, Platform::YouTube);
        assert!(!Supervisor::platform_can_chat_only(&yt_ff));

        let kick = test_util::row(Tool::YtDlp, Container::Mkv, Platform::Kick);
        assert!(!Supervisor::platform_can_chat_only(&kick), "Kick chat isn't supported");
    }

    /// The stem fills title/game rather than leaving `-tba` placeholders (no
    /// rename pass ever runs for a chat-only session), and lands on the
    /// extension its platform's logger writes.
    #[test]
    fn stem_fills_title_and_game_and_is_collision_free() {
        use crate::models::Container;
        let dir = std::env::temp_dir().join(format!("sa-chatonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut row = test_util::row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        row.channel.name = "Somebody".into();
        row.monitor.filename_template = "{name} - {title} - {games}".into();

        let stem = chat_only_stem(&row, 1_700_000_000, "", "Karaoke night", "Music", 0, &dir, true);
        assert_eq!(stem, "Somebody - Karaoke night - Music");
        assert!(!stem.contains("-tba"), "no placeholder survives: {stem}");

        // An existing sidecar of that name is never appended to or clobbered.
        std::fs::write(dir.join(format!("{stem}.chat.jsonl")), "").unwrap();
        let next = chat_only_stem(&row, 1_700_000_000, "", "Karaoke night", "Music", 0, &dir, true);
        assert_eq!(next, "Somebody - Karaoke night - Music (2)");
        // …and the YouTube variant probes its OWN extension, so it doesn't
        // collide with (or dodge) the Twitch one.
        let yt = chat_only_stem(&row, 1_700_000_000, "", "Karaoke night", "Music", 0, &dir, false);
        assert_eq!(yt, "Somebody - Karaoke night - Music");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
