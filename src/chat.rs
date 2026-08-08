//! Anonymous Twitch chat capture over the IRC-over-WebSocket gateway.
//!
//! Twitch chat is plain IRCv3 over `wss://irc-ws.chat.twitch.tv`. We log in
//! anonymously (a `justinfan*` nick — read-only, no token), request the tags +
//! commands capabilities (for timestamps / display names / colors), JOIN the
//! channel, and append every chat message to a `.chat.jsonl` sidecar next to the
//! recording. Uses the already-present `tokio-tungstenite` (no new dependency).
//!
//! YouTube chat is handled separately by yt-dlp (`--sub-langs live_chat`), not
//! here. Kick chat is not yet supported.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use tracing::{debug, info};

const WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";

/// Process-wide counter so concurrent anonymous logins get distinct nicks even
/// when two recordings start in the same second.
static NICK_SEQ: AtomicU64 = AtomicU64::new(0);

/// Bound the connect + login handshake so a slow/unreachable gateway can't block
/// the recording's finalize (which joins this task when the capture ends).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Bound every `ws.send()` once connected, for the same reason as
/// `CONNECT_TIMEOUT` above but for the steady-state PING/PONG replies: a
/// half-dead connection (TCP black-holed after a network blip — no clean
/// close, no read error) can leave an unprotected write pending forever.
/// `ws.next()` already has its own 1s read timeout; writes had none, and a
/// hung write here blocks the read loop from ever reaching its own
/// `done`/`shutdown` check again — confirmed live (2026-07-23): two
/// channels' finalize sequences stuck for hours/days with `status =
/// "recording"` and a long-dead capture process, because `stop_record_watchers`
/// joins this task without an abort fallback.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve once `done` or `shutdown` is set, to race against a blocking connect.
async fn wait_stopped(done: &AtomicBool, shutdown: &AtomicBool) {
    while !done.load(Ordering::SeqCst) && !shutdown.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Flush the sidecar buffer at least this often while messages are pending.
/// Keeps the on-disk file near-live for the chat replay popup's 3s tail poll
/// while turning per-message write syscalls into a couple of appends per
/// second — by default the sidecar lives next to the capture on the
/// recordings drive, where per-message writes from several busy chats are
/// pure seek churn. (With a dedicated chat root configured the sidecar is on
/// its own drive, but the buffering stays — cheap, and the default layout
/// still needs it.)
const FLUSH_EVERY: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Dedicated chat-log root ("chat logs on another drive").
// ---------------------------------------------------------------------------

/// Settings key for the dedicated chat-log root folder (empty = chat sidecars
/// are written next to the recording, the pre-setting behavior).
pub const K_CHAT_ROOT: &str = "chat_log_root";

/// The configured chat-log root. `None` = sidecars live next to recordings.
/// Same static-root pattern as `downloader::cache::CACHE_ROOTS`.
static CHAT_ROOT: parking_lot::RwLock<Option<PathBuf>> = parking_lot::RwLock::new(None);

/// Apply the chat-root setting (startup + live on settings save).
pub fn set_chat_root(raw: &str) {
    let trimmed = raw.trim().trim_end_matches(['\\', '/']);
    let root = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
    if let Some(r) = &root {
        tracing::info!("chat log root: {}", r.display());
    }
    *CHAT_ROOT.write() = root;
}

/// The configured chat-log root, if any.
pub fn chat_root() -> Option<PathBuf> {
    CHAT_ROOT.read().clone()
}

/// Where a take's chat sidecar directory lives: `dir` itself when no chat
/// root is configured (sidecar next to the recording), else the recording
/// dir MIRRORED under the root with the drive letter as the top folder —
/// `A:\VODs\Twitch\GEEGA` → `{root}\A\VODs\Twitch\GEEGA`. The full path (not
/// just the leaf, unlike the capture cache) so the tree can be re-merged onto
/// the recordings drives by hand later (`robocopy {root}\A\ A:\ /E` per drive
/// folder), and so leaf-name collisions across parents/drives can't mix chats.
///
/// Non-drive prefixes (UNC) collapse into one sanitized top folder instead of
/// a drive letter; `..`/`.` components are dropped (an output dir containing
/// them is already anomalous — never let one climb out of the root). A `dir`
/// already under the root is returned unchanged (no recursive nesting).
pub fn chat_dir_for(dir: &Path) -> PathBuf {
    let Some(root) = chat_root() else {
        return dir.to_path_buf();
    };
    if dir.starts_with(&root) {
        return dir.to_path_buf();
    }
    let mut out = root;
    for c in dir.components() {
        match c {
            std::path::Component::Prefix(p) => {
                if let Some(d) = crate::downloader::drive_of(Path::new(c.as_os_str())) {
                    out.push(d.to_string());
                } else {
                    // UNC or other exotic prefix: one sanitized component.
                    out.push(crate::downloader::sanitize_filename(
                        &p.as_os_str().to_string_lossy(),
                    ));
                }
            }
            std::path::Component::Normal(n) => out.push(n),
            std::path::Component::RootDir
            | std::path::Component::CurDir
            | std::path::Component::ParentDir => {}
        }
    }
    out
}

/// Re-root a sidecar path that was computed NEXT TO its recording (the
/// pre-chat-root convention) into the configured chat root: same filename,
/// [`chat_dir_for`]-mirrored directory. Identity when no root is configured —
/// producers call this unconditionally.
pub fn chat_sidecar_path(next_to_recording: &Path) -> PathBuf {
    match (next_to_recording.parent(), next_to_recording.file_name()) {
        (Some(dir), Some(name)) => chat_dir_for(dir).join(name),
        _ => next_to_recording.to_path_buf(),
    }
}

/// Candidate paths for a recording's chat sidecar, in priority order. An
/// explicit `chat_path` (persisted at spawn for every producer since the
/// chat-root feature) is the SOLE candidate. Otherwise (legacy takes) the
/// path is derived from the video's `output_path`: the four historical
/// next-to-the-recording shapes, plus — when a chat root is configured —
/// their chat-root mirrors, so a legacy row whose sidecar was migrated (or a
/// row whose `chat_path` was lost) still resolves.
pub fn chat_file_candidates(chat_path: &str, output_path: &str) -> Vec<PathBuf> {
    if !chat_path.is_empty() {
        return vec![PathBuf::from(chat_path)];
    }
    let base = Path::new(output_path);
    let appended = PathBuf::from(format!("{output_path}.live_chat.json"));
    let swapped = base.with_extension("chat.jsonl");
    let mut v = vec![
        appended.clone(),
        swapped.clone(),
        base.with_extension("live_chat.json"),
        base.with_extension("ts.live_chat.json"),
    ];
    if chat_root().is_some() {
        v.push(chat_sidecar_path(&swapped));
        v.push(chat_sidecar_path(&appended));
    }
    v
}

/// Flush early once this much is buffered (GDQ-scale chat bursts).
const FLUSH_BYTES: usize = 32 * 1024;

/// Buffered appender for the `.chat.jsonl` sidecar. The file is opened lazily
/// on the first flush so a stream with no chat (or a recording that fails
/// immediately) doesn't leave an empty sidecar; append mode means reconnects
/// continue the same file rather than truncating it. Worst case on a hard
/// kill, [`FLUSH_EVERY`] worth of chat is lost — the graceful paths all flush.
struct ChatSink {
    path: PathBuf,
    /// Storage region of `path`, classified once (the sidecar never moves
    /// during a session) so per-flush accounting skips re-classification.
    region: crate::iomon::Region,
    file: Option<tokio::fs::File>,
    buf: String,
    first_buffered: Option<tokio::time::Instant>,
}

impl ChatSink {
    fn new(path: PathBuf) -> ChatSink {
        let region = crate::iomon::classify(&path);
        ChatSink { path, region, file: None, buf: String::new(), first_buffered: None }
    }

    fn push(&mut self, json_line: &str) {
        if self.buf.is_empty() {
            self.first_buffered = Some(tokio::time::Instant::now());
        }
        self.buf.push_str(json_line);
        self.buf.push('\n');
    }

    fn should_flush(&self) -> bool {
        self.buf.len() >= FLUSH_BYTES
            || self
                .first_buffered
                .is_some_and(|t| t.elapsed() >= FLUSH_EVERY)
    }

    /// Create the sidecar now, empty, without waiting for a first message.
    ///
    /// Called once the session has joined, so a quiet stream has an EMPTY chat
    /// log rather than no log at all. Without this, "View chat" stayed greyed
    /// out ("No chat log file found for this stream") until someone happened
    /// to talk — which also meant the send box never appeared, so you couldn't
    /// type the first message from here either. An empty file is the honest
    /// answer: chat was captured, nobody said anything yet.
    async fn ensure_created(&mut self) -> anyhow::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        self.file = Some(
            crate::iomon::fs::open_with(crate::iomon::Cat::ChatSidecar, &self.path, |o| {
                o.create(true).append(true);
            })
            .await?,
        );
        Ok(())
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.ensure_created().await?;
        let bytes = self.buf.len() as u64;
        let start = std::time::Instant::now();
        let res = self.file.as_mut().unwrap().write_all(self.buf.as_bytes()).await;
        crate::iomon::record_region(
            crate::iomon::Cat::ChatSidecar,
            self.region,
            crate::iomon::OpKind::Write,
            bytes,
            start.elapsed(),
            false, // awaited tokio write — no thread sat blocked
        );
        res?;
        self.buf.clear();
        self.first_buffered = None;
        Ok(())
    }
}

/// Context for recording chat-derived stream events (subs / gift subs / bits /
/// raids) into the stats DB (`stream_event`, schema v59) while chat is being
/// captured. Optional — chat capture itself never depends on it.
pub struct ChatEventCtx {
    pub store: Arc<crate::store::Store>,
    pub monitor_id: i64,
    /// Broadcast id of the recording this chat belongs to (`''` if unknown).
    pub stream_id: String,
    /// The app event bus, for raising a mention/highlight notification. See
    /// [`crate::chat_highlight`] for why detection lives here rather than in
    /// the chat window.
    pub events: crate::events::EventTx,
}

/// One stream event parsed from a raw IRC line ([`parse_chat_event`] /
/// [`EventTracker::track`]). Field semantics match the `stream_event` table
/// (see `StreamEventRow`).
#[derive(Debug, PartialEq)]
struct ChatEvent {
    kind: &'static str,
    actor: String,
    target: String,
    amount: i64,
    tier: String,
    /// Free-text payload: deleted-message excerpt, chat-mode change, role change.
    detail: String,
    /// Event time (unix secs, from `tmi-sent-ts` when present).
    ts: i64,
}

/// One line describing a parsed chat event, for the app log.
///
/// The generic `event {kind} by {actor} (x{amount})` this replaces read as an
/// accusation on the moderation kinds: `event msg_deleted by bwaido_` says
/// bwaido_ deleted something, when in fact a moderator deleted *their* message.
/// [`ChatEvent::actor`] is the person an event happened **to** for those kinds
/// (neither platform discloses which moderator acted), so the voice has to
/// change with the kind — and `(x0)` is noise wherever there's no count.
fn describe_event(ev: &ChatEvent) -> String {
    let who = if ev.actor.is_empty() { "someone" } else { ev.actor.as_str() };
    match ev.kind {
        "msg_deleted" => format!("a moderator deleted a message from {who}"),
        "timeout" => format!("{who} was timed out by a moderator ({}s)", ev.amount),
        "ban" => format!("{who} was banned by a moderator"),
        "chat_purge" => format!("a moderator removed every message from {who}"),
        "chat_clear" => "a moderator cleared the chat".to_string(),
        "chat_mode" => format!("chat mode changed: {}", ev.detail),
        "role_change" => format!("{who} {}", ev.detail),
        // The rest genuinely are things the actor did.
        "bits" => format!("{who} cheered {} bits", ev.amount),
        "sub" => format!("{who} subscribed"),
        "resub" => format!("{who} resubscribed ({} months)", ev.amount.max(1)),
        "subgift" => format!("{who} gifted {} sub(s)", ev.amount.max(1)),
        "raid_in" => format!("{who} raided in with {} viewers", ev.amount),
        "dono" => format!("{who} sent a {} Hype Chat", ev.detail),
        other if ev.amount != 0 => format!("event {other} by {who} (x{})", ev.amount),
        other => format!("event {other} by {who}"),
    }
}

/// One logged chat message (serialized as a JSON line in the sidecar).
#[derive(Serialize)]
struct ChatLine<'a> {
    /// Milliseconds since the epoch (Twitch `tmi-sent-ts` when present).
    ts: i64,
    /// Sender's login (lowercase).
    login: &'a str,
    /// Display name (falls back to `login` when unset).
    name: &'a str,
    /// Message body (the IRC trailing parameter, unescaped).
    text: &'a str,
    /// Chat color `#RRGGBB`, omitted when unset.
    #[serde(skip_serializing_if = "str::is_empty")]
    color: &'a str,
    /// Raw `badges` tag (e.g. `subscriber/12,moderator/1`), omitted when empty.
    #[serde(skip_serializing_if = "str::is_empty")]
    badges: &'a str,
    /// Raw IRCv3 `emotes` tag (e.g. `25:0-4,12-16/1902:6-10`) — first-party emote
    /// ID + inclusive code-point ranges into `text`. Stored verbatim (the value is
    /// only digits/`:`/`-`/`,`/`/`, so no IRCv3 unescaping applies). Omitted when
    /// empty; old logs without it simply render emote words as plain text.
    #[serde(skip_serializing_if = "str::is_empty")]
    emotes: &'a str,
    /// Twitch message id (IRCv3 `id` tag) — what a later `CLEARMSG` deletion
    /// marker references. Omitted when absent; old logs without it simply
    /// can't match single-message deletions.
    #[serde(skip_serializing_if = "str::is_empty")]
    id: &'a str,
    /// Display name of the message this replies to (`reply-parent-display-name`)
    /// — the replay renders an "↩ name" prefix. Omitted when not a reply.
    #[serde(skip_serializing_if = "str::is_empty")]
    reply: &'a str,
    /// Twitch `source-room-id` — the broadcaster id of the channel this
    /// message actually originated in, present only while this channel is in
    /// an active Shared Chat ("Stream Together") session. Equals the local
    /// channel's own room-id for a locally-typed message, another
    /// participant's id for a merged-in one. Omitted outside a shared
    /// session; the replay resolves it against this take's recorded collab
    /// partners (`store::collab`) to show which channel a message came from.
    #[serde(skip_serializing_if = "str::is_empty")]
    source_room_id: &'a str,
    /// Twitch `user-id` — the sender's numeric Twitch id (IRCv3 `user-id`
    /// tag). Omitted when absent (pre-feature logs); the chat usercard uses
    /// it to look up the sender's live Twitch avatar/account-created date.
    #[serde(skip_serializing_if = "str::is_empty")]
    user_id: &'a str,
    /// Twitch `badge-info` — exact per-badge counters (e.g.
    /// `"subscriber/61"` = 61 cumulative months), distinct from the `badges`
    /// tag's display tier bucket. Omitted when absent; the usercard shows
    /// "Subscriber · N months" from this when present.
    #[serde(skip_serializing_if = "str::is_empty")]
    badge_info: &'a str,
    /// Twitch `first-msg=1` — this account's first ever message in the
    /// channel. The replay accents the row and tags it, the way Twitch's own
    /// chat does. Omitted (and so absent from old logs) unless true.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    first: bool,
    /// Twitch `custom-reward-id` — the channel-point reward this message was
    /// sent through. Only redemptions that CARRY A MESSAGE reach IRC at all;
    /// a reward with no message input (a "Hydrate!" style one) is PubSub-only
    /// and can never appear here. IRC never names the reward either, so the
    /// title comes from a separate lookup keyed on this id.
    #[serde(skip_serializing_if = "str::is_empty")]
    reward_id: &'a str,
    /// Twitch's PRIVMSG `msg-id` tag — `highlighted-message` for Highlight My
    /// Message (the one channel-point reward identifiable without a lookup),
    /// `gigantified-emote-message`, and so on. Omitted when absent, which is
    /// the overwhelming majority of messages.
    #[serde(skip_serializing_if = "str::is_empty")]
    msg_kind: &'a str,
}

/// Capture `url`'s Twitch chat to `path` until `done` (recording ended) or
/// `shutdown` is set. Best-effort: connection failures are logged and retried
/// with a short interruptible backoff; this never panics. No-ops for a URL that
/// isn't a Twitch channel.
pub async fn log_twitch_chat(
    url: String,
    path: PathBuf,
    done: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    events: Option<ChatEventCtx>,
) {
    let Some(login) = crate::detectors::twitch_login(&url) else {
        return;
    };
    info!(
        "chat: logging {} {login} -> {}",
        crate::models::Platform::Twitch.tag(),
        path.display()
    );
    while !done.load(Ordering::SeqCst) && !shutdown.load(Ordering::SeqCst) {
        if let Err(e) = session(&login, &path, &done, &shutdown, events.as_ref()).await {
            debug!("chat ({login}): {e:#}; reconnecting");
        }
        // Interruptible backoff before reconnecting (checks flags every 250ms).
        for _ in 0..8 {
            if done.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// One connection's lifetime: connect, anonymous login, JOIN, then append every
/// PRIVMSG until a stop flag is set (Ok) or the connection drops (Err).
async fn session(
    login: &str,
    path: &Path,
    done: &AtomicBool,
    shutdown: &AtomicBool,
    events: Option<&ChatEventCtx>,
) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::Message;

    let seq = NICK_SEQ.fetch_add(1, Ordering::Relaxed);
    let nick = format!(
        "justinfan{}",
        100_000 + (crate::models::now_unix() as u64).wrapping_add(seq) % 9_000_000
    );
    // Connect + anonymous, read-only login (request tags+commands for metadata +
    // PINGs). Bounded by a timeout and raced against the stop flags so a stalled
    // handshake can't hang the finalize that joins this task.
    let connect = async {
        let (mut ws, _) = tokio_tungstenite::connect_async(WS_URL).await?;
        ws.send(Message::Text(
            "CAP REQ :twitch.tv/tags twitch.tv/commands".into(),
        ))
        .await?;
        ws.send(Message::Text(format!("NICK {nick}").into())).await?;
        ws.send(Message::Text(format!("JOIN #{login}").into())).await?;
        Ok::<_, anyhow::Error>(ws)
    };
    let mut ws = tokio::select! {
        biased;
        _ = wait_stopped(done, shutdown) => return Ok(()),
        r = tokio::time::timeout(CONNECT_TIMEOUT, connect) => match r {
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow::anyhow!("chat connect/login timed out")),
        },
    };

    // Messages accumulate in the sink and hit disk a couple of times per
    // second at most (see ChatSink); flushed on every exit path below.
    let mut sink = ChatSink::new(path.to_path_buf());
    // Create it empty right away — see `ensure_created`. A failure here is
    // not fatal: the first flush retries, and losing the "there is a log"
    // signal is better than dropping the capture over it.
    if let Err(e) = sink.ensure_created().await {
        debug!("chat ({login}): could not pre-create the sidecar: {e:#}");
    }
    // Moderation tracker (deletions/purges/room modes/role badges) — per
    // connection, so its baselines reset with each reconnect.
    let mut tracker = EventTracker::default();
    // Mention/highlight watcher. Read ONCE per connection: the rules and the
    // connected login change from Settings, which is a human action, so a
    // reconnect picking them up is soon enough — and this must not become a
    // settings read per chat message on a busy channel.
    // NOT filtered on `armed()` here: a rule added while the stream is already
    // running would otherwise never take effect for the whole connection,
    // because there'd be no watcher left to re-read it. `check` re-reads on a
    // timer and no-ops while nothing is armed.
    let mut mentions = events.map(MentionWatch::new);
    if let Some(ctx) = events {
        tracker.tuning = load_hype_tuning(ctx);
    }
    let mut tuning_loaded = crate::models::now_unix();

    let result: anyhow::Result<()> = async {
        loop {
            if done.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }
            // Keep the hype tuning current mid-recording (Settings edits and
            // auto-tune adjustments apply within TUNING_REFRESH_SECS). The
            // loop wakes at least once a second, so this can't starve.
            if let Some(ctx) = events
                && crate::models::now_unix() - tuning_loaded >= TUNING_REFRESH_SECS
            {
                tuning_loaded = crate::models::now_unix();
                tracker.tuning = load_hype_tuning(ctx);
            }
            // 1s read timeout so the stop flags are re-checked even on a quiet
            // chat — and the flush timer fires even when no message arrives.
            let msg = match timeout(Duration::from_secs(1), ws.next()).await {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => return Err(e.into()),
                Ok(None) => return Err(anyhow::anyhow!("chat websocket closed")),
                Err(_) => {
                    if sink.should_flush() {
                        sink.flush().await?;
                    }
                    continue; // read timeout -> re-check flags
                }
            };
            match msg {
                Message::Text(text) => {
                    // A frame can carry several CRLF-separated IRC lines.
                    for line in text.lines() {
                        let line = line.trim_end_matches('\r');
                        if line.is_empty() {
                            continue;
                        }
                        // Twitch IRC keepalive: reply so the server doesn't drop us.
                        if let Some(token) = line.strip_prefix("PING ") {
                            timeout(SEND_TIMEOUT, ws.send(Message::Text(format!("PONG {token}").into())))
                                .await
                                .map_err(|_| anyhow::anyhow!("chat PONG reply timed out"))??;
                            continue;
                        }
                        // Stream events (subs/bits/raids) live in tags the
                        // sidecar's lossy PRIVMSG parse below discards — hook
                        // the raw line first. Rare events, so the synchronous
                        // DB write is fine here.
                        let mut db_events = Vec::new();
                        let contribution = parse_chat_event(line);
                        // Moderation events: DB rows AND sidecar marker lines
                        // (the chat replay strikes deleted/purged messages and
                        // shows mode/role notices). Markers are written even
                        // without a DB context — the archive stands alone.
                        let (mod_events, mod_markers) = tracker.track(line);
                        if events.is_some() {
                            db_events.extend(mod_events);
                        }
                        for m in mod_markers {
                            sink.push(&m);
                        }
                        // Sub/raid/announcement/watch-streak rows for the
                        // replay. Written alongside — not instead of — the DB
                        // event above: the archive has to stand on its own,
                        // exactly as the moderation markers already do.
                        if let Some(m) = usernotice_marker(line) {
                            sink.push(&m);
                        }
                        if let Some(ev) = contribution {
                            // Sub/gift/bits contributions also feed the
                            // hype-train inference (burst -> one extra event
                            // + a replay notice), weighted by the tuning.
                            let pts = crate::hype::contribution_points(
                                ev.kind,
                                ev.amount,
                                &ev.tier,
                                &tracker.tuning,
                            );
                            if matches!(ev.kind, "sub" | "resub" | "subgift" | "bits" | "dono")
                                && let Some((hype, marker)) =
                                    tracker.note_contribution(ev.ts, &ev.actor, pts)
                            {
                                sink.push(&marker);
                                if events.is_some() {
                                    db_events.push(hype);
                                }
                            }
                            if events.is_some() {
                                db_events.push(ev);
                            }
                        }
                        if let Some(ev_ctx) = events {
                            for ev in db_events {
                                match ev_ctx.store.record_stream_event(
                                    ev_ctx.monitor_id,
                                    ev.ts,
                                    &ev_ctx.stream_id,
                                    ev.kind,
                                    &ev.actor,
                                    &ev.target,
                                    ev.amount,
                                    &ev.tier,
                                    &ev.detail,
                                ) {
                                    // First-time-chatter events are common (every
                                    // unique chatter, easily dozens per stream) and
                                    // low-signal — already excluded from the events
                                    // graph for the same reason; skip the per-event
                                    // log line too instead of flooding the app log.
                                    Ok(true) if ev.kind == "first_chat" => {}
                                    Ok(true) => debug!("chat ({login}): {}", describe_event(&ev)),
                                    Ok(false) => {} // deduped (EventSub saw the raid first)
                                    Err(e) => {
                                        debug!("chat ({login}): event record failed: {e:#}")
                                    }
                                }
                            }
                        }
                        if let Some(json) = parse_privmsg(line) {
                            if let Some(w) = mentions.as_mut() {
                                w.check(&json);
                            }
                            sink.push(&json);
                        }
                    }
                    if sink.should_flush() {
                        sink.flush().await?;
                    }
                }
                Message::Ping(payload) => {
                    let _ = timeout(SEND_TIMEOUT, ws.send(Message::Pong(payload))).await;
                }
                Message::Close(_) => return Err(anyhow::anyhow!("chat websocket close frame")),
                _ => {}
            }
        }
    }
    .await;

    // Whatever ended the session (stop flag, socket error, close frame), the
    // buffered tail must land on disk before the reconnect/finalize.
    let flushed = sink.flush().await;
    result.and(flushed)
}

/// The effective hype tuning for this chat session's channel (global merged
/// with the channel's override). Falls back to defaults when the monitor row
/// is gone — better a default-tuned inference than none.
fn load_hype_tuning(ctx: &ChatEventCtx) -> crate::hype::HypeTuning {
    let channel_id = ctx
        .store
        .get_monitor_with_channel(ctx.monitor_id)
        .ok()
        .flatten()
        .map(|m| m.channel.id)
        .unwrap_or(0);
    crate::hype::load_effective(&ctx.store, channel_id)
}

/// Parse a (possibly tag-prefixed) IRC line into a JSON log line, or `None` if it
/// isn't a chat message (`PRIVMSG`). Tag values keep Twitch's IRCv3 escaping in
/// the rare cases it applies; the message body is the unescaped trailing param.
fn parse_privmsg(line: &str) -> Option<String> {
    // Optional IRCv3 tags: "@k=v;k=v <rest>".
    let (tags, rest) = if let Some(s) = line.strip_prefix('@') {
        let sp = s.find(' ')?;
        (&s[..sp], &s[sp + 1..])
    } else {
        ("", line)
    };
    // rest = ":login!user@host PRIVMSG #chan :message"
    let rest = rest.strip_prefix(':')?;
    let sp = rest.find(' ')?;
    let prefix = &rest[..sp];
    let after = &rest[sp + 1..];
    if !after.starts_with("PRIVMSG ") {
        return None;
    }
    // The message is the trailing parameter, after the first " :".
    let text = after.find(" :").map(|i| &after[i + 2..]).unwrap_or("");
    let login = prefix.split('!').next().unwrap_or(prefix);

    let (
        mut display,
        mut color,
        mut badges,
        mut emotes,
        mut id,
        mut reply_raw,
        mut ts_ms,
        mut source_room_id,
        mut user_id,
        mut badge_info,
        mut reward_id,
        mut msg_kind,
    ) = ("", "", "", "", "", "", 0i64, "", "", "", "", "");
    let mut first = false;
    for kv in tags.split(';') {
        let mut it = kv.splitn(2, '=');
        let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match k {
            "display-name" => display = v,
            "color" => color = v,
            "badges" => badges = v,
            "emotes" => emotes = v,
            "id" => id = v,
            "reply-parent-display-name" => reply_raw = v,
            "tmi-sent-ts" => ts_ms = v.parse().unwrap_or(0),
            "source-room-id" => source_room_id = v,
            "user-id" => user_id = v,
            "badge-info" => badge_info = v,
            "first-msg" => first = v == "1",
            "custom-reward-id" => reward_id = v,
            "msg-id" => msg_kind = v,
            _ => {}
        }
    }
    let reply = untag(reply_raw);
    if ts_ms == 0 {
        ts_ms = crate::models::now_unix() * 1000;
    }
    let name = if display.is_empty() { login } else { display };
    serde_json::to_string(&ChatLine {
        ts: ts_ms,
        login,
        name,
        text,
        color,
        badges,
        emotes,
        id,
        reply: &reply,
        source_room_id,
        user_id,
        badge_info,
        first,
        reward_id,
        msg_kind,
    })
    .ok()
}

/// Watches a live chat for messages that name the connected account or match
/// a custom highlight rule, and raises a notification for them.
///
/// Lives here rather than in the chat window because the window is a file-tail
/// replay that may not be open — and being told while you're doing something
/// else is the entire point of "pingable". It also means chat-only sessions
/// (no recording at all) ping just the same.
struct MentionWatch<'a> {
    ctx: &'a ChatEventCtx,
    /// The connected Twitch account's login, lowercased. Empty when no
    /// account is connected, which disarms the mention half.
    login: String,
    rules: Vec<crate::chat_highlight::HighlightRule>,
    /// Whether mentions of `login` should notify at all (the "pingable"
    /// setting). Rules that opted in still notify regardless.
    pingable: bool,
    /// Channel display name, for the notification heading — one store lookup
    /// per connection rather than one per message.
    channel: String,
    /// When the rules were last read. They're edited in Settings, which is a
    /// human action, so re-reading on a timer is soon enough — but it has to
    /// happen at all: a rule added mid-stream must start working without
    /// restarting the recording.
    loaded_at: i64,
    /// When this channel last raised a toast, so a chat spamming a name can't
    /// spawn one per message. Suppressed hits still reach the 🔔 feed.
    last_toast: i64,
}

impl<'a> MentionWatch<'a> {
    fn new(ctx: &'a ChatEventCtx) -> MentionWatch<'a> {
        let login = ctx
            .store
            .get_setting(crate::oauth::K_LOGIN)
            .ok()
            .flatten()
            .unwrap_or_default()
            .to_lowercase();
        let channel = ctx
            .store
            .get_monitor_with_channel(ctx.monitor_id)
            .ok()
            .flatten()
            .map(|r| r.channel.name)
            .unwrap_or_default();
        MentionWatch {
            login,
            rules: crate::chat_highlight::load_rules(&ctx.store),
            pingable: crate::chat_highlight::pingable(&ctx.store),
            channel,
            last_toast: 0,
            loaded_at: crate::models::now_unix(),
            ctx,
        }
    }

    /// Re-read the rules and the pingable switch if they're stale.
    fn refresh(&mut self, now: i64) {
        if now - self.loaded_at < HIGHLIGHT_REFRESH_SECS {
            return;
        }
        self.loaded_at = now;
        self.rules = crate::chat_highlight::load_rules(&self.ctx.store);
        self.pingable = crate::chat_highlight::pingable(&self.ctx.store);
        if self.login.is_empty() {
            // An account connected after this session started.
            self.login = self
                .ctx
                .store
                .get_setting(crate::oauth::K_LOGIN)
                .ok()
                .flatten()
                .unwrap_or_default()
                .to_lowercase();
        }
    }

    /// Whether there's anything to watch for at all. A watcher with no rules
    /// and no connected login would run a matcher over every message to
    /// always answer "no".
    fn armed(&self) -> bool {
        (self.pingable && !self.login.is_empty())
            || self.rules.iter().any(|r| r.enabled && r.notify)
    }

    /// Check one already-serialized sidecar line.
    ///
    /// Takes the JSON rather than the raw IRC line so the tag parsing isn't
    /// done twice — `parse_privmsg` has already unescaped and extracted
    /// everything, and this runs for every single message on the channel.
    fn check(&mut self, json: &str) {
        let now = crate::models::now_unix();
        self.refresh(now);
        if !self.armed() {
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return };
        let text = v["text"].as_str().unwrap_or("");
        if text.is_empty() {
            return;
        }
        // Every "should this interrupt someone" rule lives in one pure
        // function — self-suppression, notify gating, and the per-channel
        // cooldown — so it can be tested without a socket or a clock.
        let Some(reason) = crate::chat_highlight::notify_reason(
            text,
            v["login"].as_str().unwrap_or(""),
            &self.login,
            self.pingable,
            &self.rules,
            now,
            self.last_toast,
        ) else {
            return;
        };
        self.last_toast = now;
        let _ = self.ctx.events.send(crate::events::AppEvent::ChatMention {
            monitor_id: Some(self.ctx.monitor_id),
            channel: self.channel.clone(),
            author: v["name"].as_str().unwrap_or("").to_string(),
            text: text.to_string(),
            reason,
            msg_id: v["id"].as_str().unwrap_or("").to_string(),
        });
    }
}

/// A sidecar `{"marker":"event",…}` line for a USERNOTICE — sub, resub, gift,
/// raid, announcement, watch-streak milestone — so the chat replay can show
/// them the way Twitch's own chat does. `None` for anything else.
///
/// These already reach the DB as `stream_event` rows via [`parse_chat_event`],
/// but that path feeds statistics; this one feeds the replay, and the two want
/// different things (one wants a typed amount, the other wants a rendered
/// line). Deliberately separate rather than one doing double duty.
///
/// **The headline is Twitch's own `system-msg` tag, verbatim.** It already
/// reads "Bob subscribed at Tier 1. They've subscribed for 12 months!", with
/// the right pluralisation, tier wording and localisation. Composing our own
/// from the `msg-param-*` tags would be reinventing that, worse.
fn usernotice_marker(line: &str) -> Option<String> {
    let (tags, rest) = line.strip_prefix('@').and_then(|s| s.split_once(' '))?;
    let after = rest.strip_prefix(':').and_then(|r| r.split_once(' ')).map(|(_, a)| a)?;
    if !after.starts_with("USERNOTICE ") {
        return None;
    }
    let (mut msg_id, mut login, mut display, mut system_msg, mut ts_ms) = ("", "", "", "", 0i64);
    for kv in tags.split(';') {
        let mut it = kv.splitn(2, '=');
        let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match k {
            "msg-id" => msg_id = v,
            "login" => login = v,
            "display-name" => display = v,
            "system-msg" => system_msg = v,
            "tmi-sent-ts" => ts_ms = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    // Only the kinds the replay renders. Everything else (raid cancels,
    // rituals, charity, unraid…) stays a DB event only.
    let kind = match msg_id {
        "sub" | "resub" | "subgift" | "submysterygift" | "anonsubgift"
        | "anonsubmysterygift" | "giftpaidupgrade" | "anongiftpaidupgrade" => "sub",
        "raid" => "raid",
        "announcement" => "announce",
        "viewermilestone" => "watchstreak",
        _ => return None,
    };
    let text = untag(system_msg);
    if text.is_empty() {
        // Nothing to render. Better no row than an empty one.
        return None;
    }
    let name = untag(display);
    // The user's own message, when they left one alongside the event (a resub
    // message, an announcement body) — the replay draws it under the headline.
    let body = after.find(" :").map(|i| &after[i + 2..]).unwrap_or("");
    serde_json::to_string(&serde_json::json!({
        "marker": "event",
        "kind": kind,
        "ts": if ts_ms > 0 { ts_ms } else { crate::models::now_unix() * 1000 },
        "login": login,
        "name": if name.is_empty() { login } else { name.as_str() },
        "text": text,
        "body": body,
    }))
    .ok()
}

/// Undo IRCv3 tag-value escaping (`\s` space, `\:` `;`, `\\`, `\r`, `\n`) —
/// display names and system messages in `msg-param-*` tags use it.
fn untag(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some(':') => out.push(';'),
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Parse a raw IRC line into a stream event, or `None` for ordinary chat.
/// Sources: `USERNOTICE` (msg-id `sub`/`resub`/`subgift`/`submysterygift`/
/// `raid`) and cheer `PRIVMSG`s (a `bits` tag). Individual `subgift` notices
/// that belong to a mystery-gift batch (they carry
/// `msg-param-community-gift-id`) are skipped — the `submysterygift` notice
/// already carries the batch size, and counting both would double it.
fn parse_chat_event(line: &str) -> Option<ChatEvent> {
    let (tags, rest) = if let Some(s) = line.strip_prefix('@') {
        let sp = s.find(' ')?;
        (&s[..sp], &s[sp + 1..])
    } else {
        return None; // every event source needs tags
    };
    let rest = rest.strip_prefix(':')?;
    let sp = rest.find(' ')?;
    let prefix = &rest[..sp];
    let after = &rest[sp + 1..];

    let mut msg_id = "";
    let mut login = "";
    let mut display = "";
    let mut bits = 0i64;
    let mut ts_ms = 0i64;
    let mut months = 0i64;
    let mut gift_count = 0i64;
    let mut viewer_count = 0i64;
    let mut plan = "";
    let (mut recipient, mut raider) = (String::new(), String::new());
    let mut community_batch = false;
    let mut streak = 0i64;
    let mut lifetime_gifts = 0i64;
    let mut milestone_value = 0i64;
    let mut milestone_cat = "";
    let mut paid_amount = 0i64;
    let mut paid_currency = "";
    let mut paid_exponent = 2u32;
    let mut first_msg = false;
    for kv in tags.split(';') {
        let mut it = kv.splitn(2, '=');
        let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match k {
            "msg-id" => msg_id = v,
            "login" => login = v,
            "display-name" => display = v,
            "bits" => bits = v.parse().unwrap_or(0),
            "tmi-sent-ts" => ts_ms = v.parse().unwrap_or(0),
            "msg-param-cumulative-months" => months = v.parse().unwrap_or(0),
            "msg-param-mass-gift-count" => gift_count = v.parse().unwrap_or(0),
            "msg-param-viewerCount" => viewer_count = v.parse().unwrap_or(0),
            "msg-param-sub-plan" => plan = v,
            "msg-param-recipient-display-name" | "msg-param-recipient-user-name" => {
                if recipient.is_empty() || k.ends_with("display-name") {
                    recipient = untag(v);
                }
            }
            "msg-param-displayName" => raider = untag(v),
            "msg-param-community-gift-id" => community_batch = true,
            "msg-param-streak-months" => streak = v.parse().unwrap_or(0),
            "msg-param-sender-count" => lifetime_gifts = v.parse().unwrap_or(0),
            "msg-param-value" => milestone_value = v.parse().unwrap_or(0),
            "msg-param-category" => milestone_cat = v,
            "pinned-chat-paid-amount" => paid_amount = v.parse().unwrap_or(0),
            "pinned-chat-paid-currency" => paid_currency = v,
            "pinned-chat-paid-exponent" => paid_exponent = v.parse().unwrap_or(2),
            "first-msg" => first_msg = v == "1",
            _ => {}
        }
    }
    let ts = if ts_ms > 0 { ts_ms / 1000 } else { crate::models::now_unix() };
    let actor = if !display.is_empty() {
        untag(display)
    } else if !login.is_empty() {
        login.to_string()
    } else {
        prefix.split('!').next().unwrap_or(prefix).to_string()
    };
    let tier = plan.to_string();

    let ev = |kind: &'static str, actor: String, target: String, amount: i64, tier: String, detail: String| {
        ChatEvent { kind, actor, target, amount, tier, detail, ts }
    };
    let text = after.find(" :").map(|i| &after[i + 2..]).unwrap_or("");
    if after.starts_with("USERNOTICE ") {
        return match msg_id {
            "sub" => Some(ev("sub", actor, String::new(), 1, tier, String::new())),
            "resub" => Some(ev(
                "resub",
                actor,
                String::new(),
                months.max(1),
                tier,
                // Watch/sub streak, when the subscriber chose to share it.
                if streak > 0 { format!("{streak}-month streak") } else { String::new() },
            )),
            "subgift" if !community_batch => Some(ev(
                "subgift",
                actor,
                recipient,
                1,
                tier,
                if lifetime_gifts > 0 { format!("{lifetime_gifts} gifts lifetime") } else { String::new() },
            )),
            // Community batch: the announcement carries the size, no single recipient.
            "submysterygift" => Some(ev(
                "subgift",
                actor,
                String::new(),
                gift_count.max(1),
                tier,
                if lifetime_gifts > 0 { format!("{lifetime_gifts} gifts lifetime") } else { String::new() },
            )),
            "raid" => Some(ev(
                "raid_in",
                if raider.is_empty() { actor } else { raider },
                String::new(),
                viewer_count,
                String::new(),
                String::new(),
            )),
            // Watch-streak (and similar) milestone celebrations.
            "viewermilestone" if milestone_value > 0 => Some(ev(
                "milestone",
                actor,
                String::new(),
                milestone_value,
                String::new(),
                if milestone_cat.is_empty() {
                    format!("milestone {milestone_value}")
                } else {
                    format!("{} {milestone_value}", untag(milestone_cat))
                },
            )),
            // Moderator announcements (the highlighted 📣 messages).
            "announcement" => Some(ev(
                "announcement",
                actor,
                String::new(),
                0,
                String::new(),
                excerpt(text, 160),
            )),
            _ => None,
        };
    }
    if after.starts_with("PRIVMSG ") {
        if bits > 0 {
            return Some(ev("bits", actor, String::new(), bits, String::new(), String::new()));
        }
        // Hype Chat: a PAID pinned message — real on-platform money, carried
        // as minor currency units + exponent (500 + 2 -> 5.00).
        if paid_amount > 0 {
            let value = paid_amount as f64 / 10f64.powi(paid_exponent as i32);
            let detail = format!("{value:.prec$} {paid_currency}", prec = paid_exponent as usize);
            return Some(ev("dono", actor, String::new(), paid_amount, paid_currency.to_string(), detail));
        }
        // First-time chatter (excluded from graph markers — too dense — but
        // listed/filterable in the events table).
        if first_msg {
            return Some(ev("first_chat", actor, String::new(), 0, String::new(), excerpt(text, 80)));
        }
    }
    None
}

/// Char-boundary-safe excerpt of a deleted message for the event ledger.
fn excerpt(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// Stateful per-connection moderation tracker. Turns raw IRC lines into
/// - DB stream events (`msg_deleted` / `timeout` / `ban` / `chat_clear` /
///   `chat_mode` / `role_change`), and
/// - sidecar **marker lines** for the chat replay (deletion/purge markers the
///   replay applies as strikethrough, plus visible notice lines).
///
/// Stateful parts: the first ROOMSTATE after JOIN is the room's baseline
/// (deltas after it are real changes worth logging), and role changes are
/// inferred from a chatter's badge set changing between their own messages
/// (Twitch removed IRC MODE, and the VIP/mod list APIs need broadcaster
/// tokens — badge transitions are the only anonymous signal). Both baselines
/// reset per connection, so a reconnect can never fabricate a change.
#[derive(Default)]
struct EventTracker {
    /// Room baseline: (emote_only, followers_min ( -1 = off), r9k, slow_secs,
    /// subs_only). `None` until the JOIN's first ROOMSTATE.
    room: Option<(bool, i64, bool, i64, bool)>,
    /// login -> (has mod badge, has VIP badge), first seen = baseline.
    roles: std::collections::HashMap<String, (bool, bool)>,
    /// Recent sub/gift/bits contributions `(ts, actor, hype points)` within
    /// the tuning's window — the hype-train inference input.
    contrib: std::collections::VecDeque<(i64, String, i64)>,
    /// A hype-train-like burst is currently flagged (no re-trigger until the
    /// contribution window drains empty).
    hype_active: bool,
    /// Inference weights/thresholds ([`crate::hype`]) — loaded per channel at
    /// session start and refreshed every [`TUNING_REFRESH_SECS`] so Settings
    /// edits reach a running recording; defaults when chat has no DB context.
    tuning: crate::hype::HypeTuning,
}

/// How often a live session re-reads the hype tuning from settings.
const TUNING_REFRESH_SECS: i64 = 300;

/// How often a live session re-reads the chat highlight rules. Shorter than
/// the tuning refresh because this is something a user edits and then
/// immediately expects to work — waiting five minutes reads as broken.
const HIGHLIGHT_REFRESH_SECS: i64 = 30;

impl EventTracker {
    /// Note one sub/gift/bits contribution (pre-scored via
    /// [`crate::hype::contribution_points`]) and infer a **hype-train-like
    /// burst**: enough points/events from enough distinct chatters within the
    /// tuning window (all gates from [`crate::hype::HypeTuning`] — the values
    /// GQL confirmations and manual marks auto-tune). This inference is the
    /// fallback signal; a GQL-confirmed train supersedes and deletes its
    /// `(inferred)` rows. One event per burst; re-arms only after the window
    /// drains empty.
    fn note_contribution(&mut self, ts: i64, actor: &str, points: i64) -> Option<(ChatEvent, String)> {
        self.contrib.push_back((ts, actor.to_lowercase(), points));
        let window = self.tuning.window_secs.max(1);
        while self.contrib.front().is_some_and(|(t, ..)| ts - t > window) {
            self.contrib.pop_front();
        }
        // Everything older drained away -> any previous burst is over.
        if self.contrib.len() == 1 {
            self.hype_active = false;
        }
        if self.hype_active {
            return None;
        }
        let uniq: std::collections::HashSet<&str> =
            self.contrib.iter().map(|(_, a, _)| a.as_str()).collect();
        let pts: i64 = self.contrib.iter().map(|(.., p)| p).sum();
        if (self.contrib.len() as i64) < self.tuning.min_events
            || (uniq.len() as i64) < self.tuning.min_actors
            || (self.tuning.min_points > 0 && pts < self.tuning.min_points)
        {
            return None;
        }
        self.hype_active = true;
        let detail = format!(
            "{} contributions ({pts} pts) from {} chatters in {} min (inferred)",
            self.contrib.len(),
            uniq.len(),
            (window + 59) / 60,
        );
        let ev = ChatEvent {
            kind: "hype_train",
            actor: String::new(),
            target: String::new(),
            amount: pts,
            tier: String::new(),
            detail: detail.clone(),
            ts,
        };
        let marker = format!(
            r#"{{"ts":{},"marker":"notice","text":{}}}"#,
            ts * 1000,
            serde_json::Value::from(format!("Hype-train-like burst: {detail}"))
        );
        Some((ev, marker))
    }

    /// Feed one raw IRC line; returns `(db_events, sidecar_marker_lines)`.
    fn track(&mut self, line: &str) -> (Vec<ChatEvent>, Vec<String>) {
        let mut events = Vec::new();
        let mut markers = Vec::new();
        let Some(s) = line.strip_prefix('@') else {
            return (events, markers);
        };
        let Some(sp) = s.find(' ') else {
            return (events, markers);
        };
        let (tags, rest) = (&s[..sp], &s[sp + 1..]);
        let Some(rest) = rest.strip_prefix(':') else {
            return (events, markers);
        };
        let Some(sp) = rest.find(' ') else {
            return (events, markers);
        };
        let after = &rest[sp + 1..];
        let tag = |key: &str| -> Option<&str> {
            tags.split(';').find_map(|kv| {
                let mut it = kv.splitn(2, '=');
                (it.next() == Some(key)).then(|| it.next().unwrap_or(""))
            })
        };
        let ts = tag("tmi-sent-ts")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or_else(|| crate::models::now_unix() * 1000);
        let trailing = after.find(" :").map(|i| &after[i + 2..]).unwrap_or("");
        // `target` carries the platform's stable id for the chatter in `actor`
        // where one is available (see `StreamEventRow::target`) — for the
        // moderation kinds that's `target-user-id`, which survives a display-
        // name change and is what a usercard matches on.
        let ev = |kind: &'static str, actor: String, target: String, amount: i64, detail: String| {
            ChatEvent { kind, actor, target, amount, tier: String::new(), detail, ts: ts / 1000 }
        };
        let user_id = || tag("target-user-id").unwrap_or("").to_string();

        if after.starts_with("CLEARMSG ") {
            // A single message deleted by a moderator; the trailing param is
            // the original text (already archived — the marker just flags it).
            let login = tag("login").unwrap_or("").to_string();
            let target_id = tag("target-msg-id").unwrap_or("");
            // CLEARMSG identifies the author by login only — no user id — so
            // this row's `target` stays empty and the usercard matches it by
            // name (see `Store::moderation_events_for_user`).
            events.push(ev("msg_deleted", login.clone(), String::new(), 0, excerpt(trailing, 120)));
            if !target_id.is_empty() {
                markers.push(format!(
                    r#"{{"ts":{ts},"marker":"del","id":{}}}"#,
                    serde_json::Value::from(target_id)
                ));
            }
        } else if after.starts_with("CLEARCHAT ") {
            let target = trailing.trim();
            if target.is_empty() {
                // Full chat clear.
                events.push(ev("chat_clear", String::new(), String::new(), 0, String::new()));
                markers.push(format!(r#"{{"ts":{ts},"marker":"clear"}}"#));
            } else {
                let secs = tag("ban-duration").and_then(|v| v.parse::<i64>().ok());
                match secs {
                    Some(d) => {
                        events.push(ev("timeout", target.to_string(), user_id(), d, String::new()));
                        markers.push(format!(
                            r#"{{"ts":{ts},"marker":"purge","login":{},"secs":{d}}}"#,
                            serde_json::Value::from(target)
                        ));
                    }
                    None => {
                        events.push(ev("ban", target.to_string(), user_id(), 0, String::new()));
                        markers.push(format!(
                            r#"{{"ts":{ts},"marker":"purge","login":{}}}"#,
                            serde_json::Value::from(target)
                        ));
                    }
                }
            }
        } else if after.starts_with("USERNOTICE ") {
            // Moderator announcements show as 📣 notice lines in the replay
            // (the DB event comes from `parse_chat_event`; this is display).
            if tag("msg-id") == Some("announcement") && !trailing.is_empty() {
                let name = tag("display-name")
                    .map(untag)
                    .filter(|n| !n.is_empty())
                    .or_else(|| tag("login").map(str::to_string))
                    .unwrap_or_default();
                markers.push(format!(
                    r#"{{"ts":{ts},"marker":"notice","text":{}}}"#,
                    serde_json::Value::from(format!("📣 {name}: {trailing}"))
                ));
            }
        } else if after.starts_with("ROOMSTATE ") {
            let parse_flag = |k: &str| tag(k).map(|v| v == "1");
            let parse_num = |k: &str| tag(k).and_then(|v| v.parse::<i64>().ok());
            match &mut self.room {
                // First ROOMSTATE after JOIN carries the full current state —
                // that's the baseline, not a change.
                None => {
                    self.room = Some((
                        parse_flag("emote-only").unwrap_or(false),
                        parse_num("followers-only").unwrap_or(-1),
                        parse_flag("r9k").unwrap_or(false),
                        parse_num("slow").unwrap_or(0),
                        parse_flag("subs-only").unwrap_or(false),
                    ));
                }
                // Updates carry only the changed tag(s).
                Some(room) => {
                    let mut changes: Vec<String> = Vec::new();
                    if let Some(v) = parse_flag("emote-only")
                        && v != room.0
                    {
                        room.0 = v;
                        changes.push(format!("Emote-only {}", if v { "on" } else { "off" }));
                    }
                    if let Some(v) = parse_num("followers-only")
                        && v != room.1
                    {
                        room.1 = v;
                        changes.push(if v < 0 {
                            "Followers-only off".into()
                        } else if v == 0 {
                            "Followers-only on".into()
                        } else {
                            format!("Followers-only on ({v}m)")
                        });
                    }
                    if let Some(v) = parse_flag("r9k")
                        && v != room.2
                    {
                        room.2 = v;
                        changes.push(format!("Unique-chat {}", if v { "on" } else { "off" }));
                    }
                    if let Some(v) = parse_num("slow")
                        && v != room.3
                    {
                        room.3 = v;
                        changes.push(if v > 0 {
                            format!("Slow mode on ({v}s)")
                        } else {
                            "Slow mode off".into()
                        });
                    }
                    if let Some(v) = parse_flag("subs-only")
                        && v != room.4
                    {
                        room.4 = v;
                        changes.push(format!("Subs-only {}", if v { "on" } else { "off" }));
                    }
                    for c in changes {
                        events.push(ev("chat_mode", String::new(), String::new(), 0, c.clone()));
                        markers.push(format!(
                            r#"{{"ts":{ts},"marker":"notice","text":{}}}"#,
                            serde_json::Value::from(c)
                        ));
                    }
                }
            }
        } else if after.starts_with("PRIVMSG ") {
            // Role inference from badge transitions between a chatter's own
            // messages. Baseline = their first message this connection.
            let prefix = &rest[..sp];
            let login = prefix.split('!').next().unwrap_or("").to_lowercase();
            if login.is_empty() {
                return (events, markers);
            }
            let badges = tag("badges").unwrap_or("");
            if badges.contains("broadcaster/") {
                return (events, markers);
            }
            let now_roles = (badges.contains("moderator/"), badges.contains("vip/"));
            // `None` = first sighting -> baseline only, never an event.
            if let Some(prev) = self.roles.insert(login.clone(), now_roles)
                && prev != now_roles
            {
                let name = tag("display-name")
                    .map(untag)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(login);
                let mut deltas: Vec<&str> = Vec::new();
                match (prev.0, now_roles.0) {
                    (false, true) => deltas.push("gained the moderator badge"),
                    (true, false) => deltas.push("lost the moderator badge"),
                    _ => {}
                }
                match (prev.1, now_roles.1) {
                    (false, true) => deltas.push("gained the VIP badge"),
                    (true, false) => deltas.push("lost the VIP badge"),
                    _ => {}
                }
                for d in deltas {
                    events.push(ev("role_change", name.clone(), String::new(), 0, d.to_string()));
                    markers.push(format!(
                        r#"{{"ts":{ts},"marker":"notice","text":{}}}"#,
                        serde_json::Value::from(format!("{name} {d}"))
                    ));
                }
            }
        }
        (events, markers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three new PRIVMSG tags reach the sidecar, and every one of them is
    /// omitted when absent — an ordinary message must not grow three empty
    /// fields, and an older reader must not meet keys it doesn't know.
    #[test]
    fn privmsg_carries_first_message_and_redemption_tags() {
        let plain = parse_privmsg(
            "@display-name=Bob;tmi-sent-ts=1700000000000 :bob!bob@bob.tmi.twitch.tv \
             PRIVMSG #chan :hello",
        )
        .unwrap();
        assert!(!plain.contains("first"), "no first-msg tag => key omitted: {plain}");
        assert!(!plain.contains("reward_id"), "no reward => key omitted");
        assert!(!plain.contains("msg_kind"), "no msg-id => key omitted");

        let first = parse_privmsg(
            "@display-name=Bob;first-msg=1;tmi-sent-ts=1700000000000 \
             :bob!bob@bob.tmi.twitch.tv PRIVMSG #chan :hello",
        )
        .unwrap();
        assert!(first.contains(r#""first":true"#), "{first}");

        // A reward with a text prompt: IRC gives the id, never the title.
        let redeem = parse_privmsg(
            "@display-name=Bob;custom-reward-id=abc-123;tmi-sent-ts=1700000000000 \
             :bob!bob@bob.tmi.twitch.tv PRIVMSG #chan :hello",
        )
        .unwrap();
        assert!(redeem.contains(r#""reward_id":"abc-123""#), "{redeem}");

        // Highlight My Message is the one reward identifiable without a lookup.
        let hl = parse_privmsg(
            "@display-name=Bob;msg-id=highlighted-message;tmi-sent-ts=1700000000000 \
             :bob!bob@bob.tmi.twitch.tv PRIVMSG #chan :hello",
        )
        .unwrap();
        assert!(hl.contains(r#""msg_kind":"highlighted-message""#), "{hl}");
    }

    /// The event marker carries Twitch's own rendered copy, unescaped.
    /// Composing our own sentence from the msg-param tags would mean
    /// reinventing its pluralisation, tier wording and localisation, worse.
    #[test]
    fn usernotice_marker_uses_twitchs_own_system_msg() {
        let m = usernotice_marker(
            "@msg-id=resub;login=bob;display-name=Bob;tmi-sent-ts=1700000000000;\
system-msg=Bob\\ssubscribed\\sat\\sTier\\s1.\\sThey've\\ssubscribed\\sfor\\s12\\smonths! \
:tmi.twitch.tv USERNOTICE #chan :still here!",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["marker"], "event");
        assert_eq!(v["kind"], "sub");
        assert_eq!(v["name"], "Bob");
        assert_eq!(v["text"], "Bob subscribed at Tier 1. They've subscribed for 12 months!");
        // The user's own resub message rides along under the headline.
        assert_eq!(v["body"], "still here!");
        assert_eq!(v["ts"], 1_700_000_000_000i64);
    }

    #[test]
    fn usernotice_marker_maps_kinds_and_ignores_the_rest() {
        let mk = |msg_id: &str| {
            usernotice_marker(&format!(
                "@msg-id={msg_id};login=bob;system-msg=something\\shappened \
                 :tmi.twitch.tv USERNOTICE #chan"
            ))
            .map(|m| {
                serde_json::from_str::<serde_json::Value>(&m).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
        };
        for id in ["sub", "resub", "subgift", "submysterygift", "anonsubgift"] {
            assert_eq!(mk(id).as_deref(), Some("sub"), "{id}");
        }
        assert_eq!(mk("raid").as_deref(), Some("raid"));
        assert_eq!(mk("announcement").as_deref(), Some("announce"));
        assert_eq!(mk("viewermilestone").as_deref(), Some("watchstreak"));
        // Kinds the replay doesn't render stay DB-only.
        assert_eq!(mk("unraid"), None);
        assert_eq!(mk("ritual"), None);
        // A PRIVMSG is not a USERNOTICE.
        assert!(usernotice_marker(":bob!b@b PRIVMSG #chan :hi").is_none());
        // No system-msg to show => no row at all, rather than an empty one.
        assert!(
            usernotice_marker("@msg-id=resub;login=bob :tmi.twitch.tv USERNOTICE #chan").is_none()
        );
    }

    /// ALL `set_chat_root` calls in the test suite live in this ONE function:
    /// `CHAT_ROOT` is process-global and tests run in parallel, so a second
    /// mutating test would race this one.
    #[test]
    fn chat_dir_for_mirrors_the_recording_dir_under_the_root() {
        // Unset (default): identity — sidecar next to the recording.
        set_chat_root("");
        assert_eq!(chat_dir_for(Path::new(r"A:\VODs\GEEGA")), PathBuf::from(r"A:\VODs\GEEGA"));

        // Trailing slash is normalized away; drive letter becomes the top
        // folder; the rest of the path mirrors verbatim.
        set_chat_root("D:\\ChatLogs\\");
        assert_eq!(
            chat_dir_for(Path::new(r"A:\VODs\Twitch\GEEGA")),
            PathBuf::from(r"D:\ChatLogs\A\VODs\Twitch\GEEGA")
        );
        // Another drive gets its own top folder.
        assert_eq!(
            chat_dir_for(Path::new(r"G:\Streams\YUY")),
            PathBuf::from(r"D:\ChatLogs\G\Streams\YUY")
        );
        // A dir already under the root is returned unchanged (no nesting).
        assert_eq!(
            chat_dir_for(Path::new(r"D:\ChatLogs\A\X")),
            PathBuf::from(r"D:\ChatLogs\A\X")
        );
        // Dot components are dropped — nothing climbs out of the root.
        assert_eq!(
            chat_dir_for(Path::new(r"A:\a\..\b\.\c")),
            PathBuf::from(r"D:\ChatLogs\A\a\b\c")
        );

        // Sidecar re-rooting keeps the filename, mirrors the directory.
        assert_eq!(
            chat_sidecar_path(Path::new(r"A:\VODs\GEEGA\take.chat.jsonl")),
            PathBuf::from(r"D:\ChatLogs\A\VODs\GEEGA\take.chat.jsonl")
        );

        set_chat_root(" ");
        assert!(chat_root().is_none(), "whitespace-only clears the root");
        assert_eq!(
            chat_sidecar_path(Path::new(r"A:\V\t.chat.jsonl")),
            PathBuf::from(r"A:\V\t.chat.jsonl")
        );
    }

    #[test]
    fn parses_tagged_privmsg() {
        let line = "@badges=subscriber/12;color=#FF0000;display-name=CoolViewer;\
                    emotes=25:0-4,12-16/1902:6-10;\
                    tmi-sent-ts=1700000000123 :coolviewer!coolviewer@coolviewer.tmi.twitch.tv \
                    PRIVMSG #streamer :hello there : world";
        let json = parse_privmsg(line).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["login"], "coolviewer");
        assert_eq!(v["name"], "CoolViewer");
        // The first " :" is the param separator; the rest (incl. ": world") is text.
        assert_eq!(v["text"], "hello there : world");
        assert_eq!(v["color"], "#FF0000");
        assert_eq!(v["ts"], 1700000000123i64);
        assert_eq!(v["badges"], "subscriber/12");
        // The raw emotes tag is captured verbatim for first-party emote replay.
        assert_eq!(v["emotes"], "25:0-4,12-16/1902:6-10");
    }

    #[test]
    fn omits_empty_emotes_tag() {
        // A plain message has `emotes=` (empty); the field is skipped, like badges.
        let line = "@badges=;color=;display-name=Bob;emotes=;tmi-sent-ts=1700000000000 \
                    :bob!bob@bob.tmi.twitch.tv PRIVMSG #streamer :hi";
        let json = parse_privmsg(line).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["text"], "hi");
        assert!(v.get("emotes").is_none());
    }

    #[test]
    fn captures_source_room_id_during_shared_chat() {
        // Present only while this channel is in an active Shared Chat session
        // (Twitch adds it to every message, including ones typed locally).
        let line = "@display-name=Bob;tmi-sent-ts=1700000000000;source-room-id=999 \
                    :bob!bob@bob.tmi.twitch.tv PRIVMSG #streamer :hi from elsewhere";
        let json = parse_privmsg(line).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["source_room_id"], "999");
    }

    #[test]
    fn omits_empty_source_room_id_tag() {
        // No shared-chat session active — the tag is absent entirely, not empty.
        let line = "@display-name=Bob;tmi-sent-ts=1700000000000 \
                    :bob!bob@bob.tmi.twitch.tv PRIVMSG #streamer :hi";
        let json = parse_privmsg(line).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("source_room_id").is_none());
    }

    #[test]
    fn captures_user_id_and_badge_info_for_the_usercard() {
        let line = "@display-name=Bob;tmi-sent-ts=1700000000000;user-id=12345;\
                    badge-info=subscriber/61;badges=subscriber/3006 \
                    :bob!bob@bob.tmi.twitch.tv PRIVMSG #streamer :hi";
        let json = parse_privmsg(line).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["user_id"], "12345");
        assert_eq!(v["badge_info"], "subscriber/61");
    }

    #[test]
    fn omits_empty_user_id_and_badge_info_tags() {
        let line = "@display-name=Bob;tmi-sent-ts=1700000000000 \
                    :bob!bob@bob.tmi.twitch.tv PRIVMSG #streamer :hi";
        let json = parse_privmsg(line).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("user_id").is_none());
        assert!(v.get("badge_info").is_none());
    }

    #[test]
    fn untagged_privmsg_falls_back_to_login_and_clock() {
        let line = ":bob!bob@bob.tmi.twitch.tv PRIVMSG #streamer :hi";
        let json = parse_privmsg(line).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["login"], "bob");
        assert_eq!(v["name"], "bob"); // no display-name tag -> login
        assert_eq!(v["text"], "hi");
        // color/badges omitted when empty.
        assert!(v.get("color").is_none());
        assert!(v.get("badges").is_none());
        assert!(v["ts"].as_i64().unwrap() > 0);
    }

    #[test]
    fn ignores_non_privmsg() {
        assert!(parse_privmsg(":tmi.twitch.tv 001 justinfan1 :Welcome").is_none());
        assert!(parse_privmsg("PING :tmi.twitch.tv").is_none());
        assert!(parse_privmsg(":streamer!streamer@streamer.tmi.twitch.tv JOIN #streamer").is_none());
    }

    #[test]
    fn parses_sub_and_resub_events() {
        let sub = "@badges=;display-name=NewFan;login=newfan;msg-id=sub;\
                   msg-param-sub-plan=1000;tmi-sent-ts=1700000005000 \
                   :tmi.twitch.tv USERNOTICE #streamer";
        let ev = parse_chat_event(sub).expect("sub parses");
        assert_eq!((ev.kind, ev.actor.as_str(), ev.amount, ev.tier.as_str()), ("sub", "NewFan", 1, "1000"));
        assert_eq!(ev.ts, 1_700_000_005);

        let resub = "@display-name=OldFan;login=oldfan;msg-id=resub;\
                     msg-param-cumulative-months=14;msg-param-sub-plan=Prime;\
                     tmi-sent-ts=1700000006000 \
                     :tmi.twitch.tv USERNOTICE #streamer :14 months of hype";
        let ev = parse_chat_event(resub).expect("resub parses");
        assert_eq!((ev.kind, ev.amount, ev.tier.as_str()), ("resub", 14, "Prime"));
    }

    #[test]
    fn gift_batches_do_not_double_count() {
        // The mystery-gift announcement carries the batch size…
        let mystery = "@display-name=Whale;login=whale;msg-id=submysterygift;\
                       msg-param-mass-gift-count=20;msg-param-sub-plan=1000;\
                       tmi-sent-ts=1700000007000 :tmi.twitch.tv USERNOTICE #streamer";
        let ev = parse_chat_event(mystery).expect("mystery gift parses");
        assert_eq!((ev.kind, ev.amount), ("subgift", 20));
        assert!(ev.target.is_empty(), "community batch has no single recipient");

        // …so its individual per-recipient notices (community-gift-id) are skipped.
        let batched = "@display-name=Whale;login=whale;msg-id=subgift;\
                       msg-param-community-gift-id=12345;\
                       msg-param-recipient-display-name=LuckyOne;msg-param-sub-plan=1000 \
                       :tmi.twitch.tv USERNOTICE #streamer";
        assert!(parse_chat_event(batched).is_none());

        // A standalone single gift still counts, with its recipient.
        let single = "@display-name=Gifter;login=gifter;msg-id=subgift;\
                      msg-param-recipient-display-name=Friend\\sOne;msg-param-sub-plan=2000 \
                      :tmi.twitch.tv USERNOTICE #streamer";
        let ev = parse_chat_event(single).expect("single gift parses");
        assert_eq!((ev.kind, ev.target.as_str(), ev.amount, ev.tier.as_str()), ("subgift", "Friend One", 1, "2000"));
    }

    #[test]
    fn parses_bits_and_raid_events() {
        let cheer = "@badges=;bits=500;display-name=Cheerer;tmi-sent-ts=1700000008000 \
                     :cheerer!cheerer@cheerer.tmi.twitch.tv PRIVMSG #streamer :cheer500 gg";
        let ev = parse_chat_event(cheer).expect("cheer parses");
        assert_eq!((ev.kind, ev.actor.as_str(), ev.amount), ("bits", "Cheerer", 500));

        let raid = "@display-name=raider;login=raider;msg-id=raid;\
                    msg-param-displayName=Raider;msg-param-viewerCount=1234;\
                    tmi-sent-ts=1700000009000 :tmi.twitch.tv USERNOTICE #streamer";
        let ev = parse_chat_event(raid).expect("raid parses");
        assert_eq!((ev.kind, ev.actor.as_str(), ev.amount), ("raid_in", "Raider", 1234));

        // Plain chat is not an event…
        let plain = "@badges=;display-name=Bob;tmi-sent-ts=1 \
                     :bob!bob@bob.tmi.twitch.tv PRIVMSG #streamer :hi";
        assert!(parse_chat_event(plain).is_none());
        // …but mod announcements are (with the text as detail).
        let announce = "@msg-id=announcement;display-name=Mod \
                        :tmi.twitch.tv USERNOTICE #streamer :big news";
        let ev = parse_chat_event(announce).expect("announcement parses");
        assert_eq!((ev.kind, ev.actor.as_str(), ev.detail.as_str()), ("announcement", "Mod", "big news"));
    }

    #[test]
    fn parses_hype_chat_first_msg_and_milestone() {
        // Hype Chat: paid pinned message, minor units + exponent.
        let hype = "@badges=;display-name=Fan;pinned-chat-paid-amount=500;\
                    pinned-chat-paid-currency=USD;pinned-chat-paid-exponent=2;\
                    pinned-chat-paid-level=ONE;tmi-sent-ts=1700000020000 \
                    :fan!fan@fan.tmi.twitch.tv PRIVMSG #streamer :take my money";
        let ev = parse_chat_event(hype).expect("hype chat parses");
        assert_eq!((ev.kind, ev.amount, ev.tier.as_str()), ("dono", 500, "USD"));
        assert_eq!(ev.detail, "5.00 USD");

        // First-time chatter.
        let first = "@badges=;display-name=Newbie;first-msg=1;tmi-sent-ts=1700000021000 \
                     :newbie!newbie@newbie.tmi.twitch.tv PRIVMSG #streamer :hello world";
        let ev = parse_chat_event(first).expect("first-msg parses");
        assert_eq!((ev.kind, ev.detail.as_str()), ("first_chat", "hello world"));

        // Watch-streak milestone.
        let ms = "@display-name=Loyal;login=loyal;msg-id=viewermilestone;\
                  msg-param-category=watch-streak;msg-param-value=15;\
                  tmi-sent-ts=1700000022000 :tmi.twitch.tv USERNOTICE #streamer";
        let ev = parse_chat_event(ms).expect("milestone parses");
        assert_eq!((ev.kind, ev.amount, ev.detail.as_str()), ("milestone", 15, "watch-streak 15"));

        // Resub streak + gifter lifetime totals land in the detail.
        let resub = "@display-name=OldFan;login=oldfan;msg-id=resub;\
                     msg-param-cumulative-months=24;msg-param-streak-months=12;\
                     msg-param-sub-plan=1000 :tmi.twitch.tv USERNOTICE #streamer";
        let ev = parse_chat_event(resub).expect("resub parses");
        assert_eq!(ev.detail, "12-month streak");
        let gift = "@display-name=Whale;login=whale;msg-id=submysterygift;\
                    msg-param-mass-gift-count=20;msg-param-sender-count=663;\
                    msg-param-sub-plan=1000 :tmi.twitch.tv USERNOTICE #streamer";
        let ev = parse_chat_event(gift).expect("mystery gift parses");
        assert_eq!((ev.amount, ev.detail.as_str()), (20, "663 gifts lifetime"));
    }

    #[test]
    fn moderation_log_lines_dont_blame_the_person_it_happened_to() {
        let ev = |kind: &'static str, actor: &str, amount: i64, detail: &str| ChatEvent {
            kind,
            actor: actor.into(),
            target: String::new(),
            amount,
            tier: String::new(),
            detail: detail.into(),
            ts: 0,
        };
        // The line that prompted this: "event msg_deleted by bwaido_ (x0)".
        assert_eq!(
            describe_event(&ev("msg_deleted", "bwaido_", 0, "")),
            "a moderator deleted a message from bwaido_"
        );
        assert_eq!(
            describe_event(&ev("timeout", "spammer", 600, "")),
            "spammer was timed out by a moderator (600s)"
        );
        assert_eq!(describe_event(&ev("ban", "spammer", 0, "")), "spammer was banned by a moderator");
        assert_eq!(
            describe_event(&ev("chat_purge", "spammer", 0, "")),
            "a moderator removed every message from spammer"
        );
        assert_eq!(describe_event(&ev("chat_clear", "", 0, "")), "a moderator cleared the chat");

        // Things the actor really did keep the active voice…
        assert_eq!(describe_event(&ev("bits", "carol", 500, "")), "carol cheered 500 bits");
        assert_eq!(describe_event(&ev("raid_in", "dave", 250, "")), "dave raided in with 250 viewers");
        // …and an unknown kind still says something, without a bogus "(x0)".
        assert_eq!(describe_event(&ev("milestone", "eve", 0, "")), "event milestone by eve");
    }

    #[test]
    fn tracks_deletions_timeouts_and_bans() {
        let mut t = EventTracker::default();

        let del = "@login=spammer;target-msg-id=abc-123;tmi-sent-ts=1700000010000 \
                   :tmi.twitch.tv CLEARMSG #streamer :buy followers at example.com";
        let (evs, marks) = t.track(del);
        assert_eq!(evs.len(), 1);
        assert_eq!((evs[0].kind, evs[0].actor.as_str()), ("msg_deleted", "spammer"));
        assert_eq!(evs[0].detail, "buy followers at example.com");
        assert_eq!(marks.len(), 1);
        let m: serde_json::Value = serde_json::from_str(&marks[0]).unwrap();
        assert_eq!((m["marker"].as_str(), m["id"].as_str()), (Some("del"), Some("abc-123")));

        // CLEARMSG names the author by login only — no account id to record.
        assert!(evs[0].target.is_empty());

        let timeout = "@ban-duration=600;target-user-id=4242;tmi-sent-ts=1700000011000 \
                       :tmi.twitch.tv CLEARCHAT #streamer :spammer";
        let (evs, marks) = t.track(timeout);
        assert_eq!((evs[0].kind, evs[0].amount), ("timeout", 600));
        // The stable account id, which survives a display-name change.
        assert_eq!(evs[0].target, "4242");
        let m: serde_json::Value = serde_json::from_str(&marks[0]).unwrap();
        assert_eq!(m["secs"].as_i64(), Some(600));

        let ban = "@target-user-id=4242;tmi-sent-ts=1700000012000 \
                   :tmi.twitch.tv CLEARCHAT #streamer :spammer";
        let (evs, marks) = t.track(ban);
        assert_eq!(evs[0].kind, "ban");
        assert_eq!(evs[0].target, "4242");
        let m: serde_json::Value = serde_json::from_str(&marks[0]).unwrap();
        assert!(m["secs"].is_null(), "no duration = permanent ban");

        let clear = "@tmi-sent-ts=1700000013000 :tmi.twitch.tv CLEARCHAT #streamer";
        let (evs, marks) = t.track(clear);
        assert_eq!(evs[0].kind, "chat_clear");
        let m: serde_json::Value = serde_json::from_str(&marks[0]).unwrap();
        assert_eq!(m["marker"].as_str(), Some("clear"));
    }

    #[test]
    fn roomstate_baseline_then_deltas() {
        let mut t = EventTracker::default();
        // The JOIN's full ROOMSTATE is a baseline, not a set of changes.
        let baseline = "@emote-only=0;followers-only=-1;r9k=0;room-id=1;slow=0;subs-only=0 \
                        :tmi.twitch.tv ROOMSTATE #streamer";
        let (evs, marks) = t.track(baseline);
        assert!(evs.is_empty() && marks.is_empty());
        // A delta update carries only the changed tag.
        let slow_on = "@room-id=1;slow=30 :tmi.twitch.tv ROOMSTATE #streamer";
        let (evs, marks) = t.track(slow_on);
        assert_eq!(evs.len(), 1);
        assert_eq!((evs[0].kind, evs[0].detail.as_str()), ("chat_mode", "Slow mode on (30s)"));
        let m: serde_json::Value = serde_json::from_str(&marks[0]).unwrap();
        assert_eq!(m["text"].as_str(), Some("Slow mode on (30s)"));
        // Re-sending the same value is not a change.
        let (evs, _) = t.track(slow_on);
        assert!(evs.is_empty());
        let slow_off = "@room-id=1;slow=0 :tmi.twitch.tv ROOMSTATE #streamer";
        let (evs, _) = t.track(slow_off);
        assert_eq!(evs[0].detail, "Slow mode off");
    }

    #[test]
    fn hype_train_burst_inference() {
        // Defaults: window 300s, min 1000 pts, 3 events, 2 actors.
        let mut t = EventTracker::default();
        // Two subs from two people: event floor not reached yet.
        assert!(t.note_contribution(1000, "a", 500).is_none());
        assert!(t.note_contribution(1010, "b", 500).is_none());
        // Third event crosses all gates (1500 pts, 3 events, 2 actors).
        let (ev, marker) = t.note_contribution(1020, "a", 500).expect("burst fires");
        assert_eq!((ev.kind, ev.amount), ("hype_train", 1500));
        assert!(ev.detail.contains("3 contributions (1500 pts) from 2 chatters"), "{}", ev.detail);
        assert!(ev.detail.ends_with("(inferred)"), "{}", ev.detail);
        let m: serde_json::Value = serde_json::from_str(&marker).unwrap();
        assert!(m["text"].as_str().unwrap().starts_with("Hype-train-like burst"));
        // More contributions during the active burst stay quiet.
        assert!(t.note_contribution(1100, "d", 500).is_none());
        // After the window drains (>5 min gap), a new burst can fire again.
        assert!(t.note_contribution(2000, "a", 500).is_none());
        assert!(t.note_contribution(2010, "b", 500).is_none());
        assert!(t.note_contribution(2020, "c", 500).is_some(), "re-armed after the lapse");

        // A single whale mass-gifting never counts as a train (actor gate).
        let mut t = EventTracker::default();
        for i in 0..10 {
            assert!(t.note_contribution(3000 + i, "whale", 2500).is_none());
        }

        // Points gate: many tiny cheers from many people still need the
        // summed points when the gate is enabled.
        let mut t = EventTracker::default();
        assert!(t.note_contribution(4000, "a", 100).is_none());
        assert!(t.note_contribution(4010, "b", 100).is_none());
        assert!(t.note_contribution(4020, "c", 100).is_none(), "300 pts < 1000");
        assert!(t.note_contribution(4030, "d", 700).is_some(), "1000 pts reached");

        // min_points = 0 disables the points gate entirely.
        let mut t = EventTracker::default();
        t.tuning.min_points = 0;
        assert!(t.note_contribution(5000, "a", 1).is_none());
        assert!(t.note_contribution(5010, "b", 1).is_none());
        assert!(t.note_contribution(5020, "c", 1).is_some(), "count gates alone decide");
    }

    #[test]
    fn role_changes_inferred_from_badges() {
        let mut t = EventTracker::default();
        let msg = |badges: &str| {
            format!(
                "@badges={badges};display-name=Helper;tmi-sent-ts=1700000014000 \
                 :helper!helper@helper.tmi.twitch.tv PRIVMSG #streamer :hi"
            )
        };
        // First sighting = baseline, even with a badge already present.
        let (evs, _) = t.track(&msg("vip/1,subscriber/3"));
        assert!(evs.is_empty());
        // VIP -> mod: one lost + one gained event.
        let (evs, marks) = t.track(&msg("moderator/1,subscriber/3"));
        let details: Vec<&str> = evs.iter().map(|e| e.detail.as_str()).collect();
        assert!(details.contains(&"gained the moderator badge"));
        assert!(details.contains(&"lost the VIP badge"));
        assert_eq!(evs[0].kind, "role_change");
        assert_eq!(evs[0].actor, "Helper");
        assert_eq!(marks.len(), evs.len());
        // Unchanged badges stay quiet; the broadcaster is never tracked.
        let (evs, _) = t.track(&msg("moderator/1,subscriber/3"));
        assert!(evs.is_empty());
        let bc = "@badges=broadcaster/1;display-name=Streamer \
                  :streamer!streamer@streamer.tmi.twitch.tv PRIVMSG #streamer :yo";
        let (evs, _) = t.track(bc);
        assert!(evs.is_empty());
    }
}
