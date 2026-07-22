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

/// Resolve once `done` or `shutdown` is set, to race against a blocking connect.
async fn wait_stopped(done: &AtomicBool, shutdown: &AtomicBool) {
    while !done.load(Ordering::SeqCst) && !shutdown.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Flush the sidecar buffer at least this often while messages are pending.
/// Keeps the on-disk file near-live for the chat replay popup's 3s tail poll
/// while turning per-message write syscalls into a couple of appends per
/// second — the sidecar lives next to the capture on the recordings drive,
/// where per-message writes from several busy chats are pure seek churn.
const FLUSH_EVERY: Duration = Duration::from_secs(2);

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

    async fn flush(&mut self) -> anyhow::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            self.file = Some(
                crate::iomon::fs::open_with(crate::iomon::Cat::ChatSidecar, &self.path, |o| {
                    o.create(true).append(true);
                })
                .await?,
            );
        }
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
    // Moderation tracker (deletions/purges/room modes/role badges) — per
    // connection, so its baselines reset with each reconnect.
    let mut tracker = EventTracker::default();

    let result: anyhow::Result<()> = async {
        loop {
            if done.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
                return Ok(());
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
                            ws.send(Message::Text(format!("PONG {token}").into())).await?;
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
                        if let Some(ev) = contribution {
                            // Sub/gift/bits contributions also feed the
                            // hype-train inference (burst -> one extra event
                            // + a replay notice).
                            if matches!(ev.kind, "sub" | "resub" | "subgift" | "bits")
                                && let Some((hype, marker)) =
                                    tracker.note_contribution(ev.ts, &ev.actor)
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
                                    Ok(true) => debug!(
                                        "chat ({login}): event {} by {} (x{})",
                                        ev.kind, ev.actor, ev.amount
                                    ),
                                    Ok(false) => {} // deduped (EventSub saw the raid first)
                                    Err(e) => {
                                        debug!("chat ({login}): event record failed: {e:#}")
                                    }
                                }
                            }
                        }
                        if let Some(json) = parse_privmsg(line) {
                            sink.push(&json);
                        }
                    }
                    if sink.should_flush() {
                        sink.flush().await?;
                    }
                }
                Message::Ping(payload) => {
                    let _ = ws.send(Message::Pong(payload)).await;
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

    let (mut display, mut color, mut badges, mut emotes, mut id, mut ts_ms) =
        ("", "", "", "", "", 0i64);
    for kv in tags.split(';') {
        let mut it = kv.splitn(2, '=');
        let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match k {
            "display-name" => display = v,
            "color" => color = v,
            "badges" => badges = v,
            "emotes" => emotes = v,
            "id" => id = v,
            "tmi-sent-ts" => ts_ms = v.parse().unwrap_or(0),
            _ => {}
        }
    }
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
    })
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

    let ev = |kind: &'static str, actor: String, target: String, amount: i64, tier: String| {
        ChatEvent { kind, actor, target, amount, tier, detail: String::new(), ts }
    };
    if after.starts_with("USERNOTICE ") {
        return match msg_id {
            "sub" => Some(ev("sub", actor, String::new(), 1, tier)),
            "resub" => Some(ev("resub", actor, String::new(), months.max(1), tier)),
            "subgift" if !community_batch => Some(ev("subgift", actor, recipient, 1, tier)),
            // Community batch: the announcement carries the size, no single recipient.
            "submysterygift" => Some(ev("subgift", actor, String::new(), gift_count.max(1), tier)),
            "raid" => Some(ev(
                "raid_in",
                if raider.is_empty() { actor } else { raider },
                String::new(),
                viewer_count,
                String::new(),
            )),
            _ => None,
        };
    }
    if bits > 0 && after.starts_with("PRIVMSG ") {
        return Some(ev("bits", actor, String::new(), bits, String::new()));
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
    /// Recent sub/gift/bits contributions `(ts, actor)` within
    /// [`HYPE_WINDOW_SECS`] — the hype-train inference input.
    contrib: std::collections::VecDeque<(i64, String)>,
    /// A hype-train-like burst is currently flagged (no re-trigger until the
    /// contribution window drains empty).
    hype_active: bool,
}

/// Hype-train inference window (matches Twitch's 5-minute train timer).
const HYPE_WINDOW_SECS: i64 = 300;
/// Contributions within the window needed to flag a burst…
const HYPE_MIN_EVENTS: usize = 5;
/// …from at least this many distinct people (a single whale gifting in
/// batches is generous, but it isn't a train).
const HYPE_MIN_ACTORS: usize = 3;

impl EventTracker {
    /// Note one sub/gift/bits contribution and infer a **hype-train-like
    /// burst**: Twitch's real Hype Train API needs a broadcaster-scoped token
    /// (and anonymous PubSub is gone), so this is the honest anonymous proxy —
    /// [`HYPE_MIN_EVENTS`] contributions from [`HYPE_MIN_ACTORS`] distinct
    /// chatters within [`HYPE_WINDOW_SECS`]. One event per burst; re-arms only
    /// after the window drains empty (no contribution for 5 minutes).
    fn note_contribution(&mut self, ts: i64, actor: &str) -> Option<(ChatEvent, String)> {
        self.contrib.push_back((ts, actor.to_lowercase()));
        while self.contrib.front().is_some_and(|(t, _)| ts - t > HYPE_WINDOW_SECS) {
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
            self.contrib.iter().map(|(_, a)| a.as_str()).collect();
        if self.contrib.len() < HYPE_MIN_EVENTS || uniq.len() < HYPE_MIN_ACTORS {
            return None;
        }
        self.hype_active = true;
        let detail = format!(
            "{} contributions from {} chatters in 5 min (inferred)",
            self.contrib.len(),
            uniq.len()
        );
        let ev = ChatEvent {
            kind: "hype_train",
            actor: String::new(),
            target: String::new(),
            amount: self.contrib.len() as i64,
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
        let ev = |kind: &'static str, actor: String, amount: i64, detail: String| ChatEvent {
            kind,
            actor,
            target: String::new(),
            amount,
            tier: String::new(),
            detail,
            ts: ts / 1000,
        };

        if after.starts_with("CLEARMSG ") {
            // A single message deleted by a moderator; the trailing param is
            // the original text (already archived — the marker just flags it).
            let login = tag("login").unwrap_or("").to_string();
            let target_id = tag("target-msg-id").unwrap_or("");
            events.push(ev("msg_deleted", login.clone(), 0, excerpt(trailing, 120)));
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
                events.push(ev("chat_clear", String::new(), 0, String::new()));
                markers.push(format!(r#"{{"ts":{ts},"marker":"clear"}}"#));
            } else {
                let secs = tag("ban-duration").and_then(|v| v.parse::<i64>().ok());
                match secs {
                    Some(d) => {
                        events.push(ev("timeout", target.to_string(), d, String::new()));
                        markers.push(format!(
                            r#"{{"ts":{ts},"marker":"purge","login":{},"secs":{d}}}"#,
                            serde_json::Value::from(target)
                        ));
                    }
                    None => {
                        events.push(ev("ban", target.to_string(), 0, String::new()));
                        markers.push(format!(
                            r#"{{"ts":{ts},"marker":"purge","login":{}}}"#,
                            serde_json::Value::from(target)
                        ));
                    }
                }
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
                        events.push(ev("chat_mode", String::new(), 0, c.clone()));
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
                    events.push(ev("role_change", name.clone(), 0, d.to_string()));
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

        // Plain chat and other notices are not events.
        let plain = "@badges=;display-name=Bob;tmi-sent-ts=1 \
                     :bob!bob@bob.tmi.twitch.tv PRIVMSG #streamer :hi";
        assert!(parse_chat_event(plain).is_none());
        let announce = "@msg-id=announcement;display-name=Mod \
                        :tmi.twitch.tv USERNOTICE #streamer :big news";
        assert!(parse_chat_event(announce).is_none());
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

        let timeout = "@ban-duration=600;tmi-sent-ts=1700000011000 \
                       :tmi.twitch.tv CLEARCHAT #streamer :spammer";
        let (evs, marks) = t.track(timeout);
        assert_eq!((evs[0].kind, evs[0].amount), ("timeout", 600));
        let m: serde_json::Value = serde_json::from_str(&marks[0]).unwrap();
        assert_eq!(m["secs"].as_i64(), Some(600));

        let ban = "@tmi-sent-ts=1700000012000 :tmi.twitch.tv CLEARCHAT #streamer :spammer";
        let (evs, marks) = t.track(ban);
        assert_eq!(evs[0].kind, "ban");
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
        let mut t = EventTracker::default();
        // Four contributions from three people: still below the event floor.
        assert!(t.note_contribution(1000, "a").is_none());
        assert!(t.note_contribution(1010, "b").is_none());
        assert!(t.note_contribution(1020, "c").is_none());
        assert!(t.note_contribution(1030, "a").is_none());
        // Fifth event, three uniques -> burst flagged exactly once.
        let (ev, marker) = t.note_contribution(1040, "b").expect("burst fires");
        assert_eq!((ev.kind, ev.amount), ("hype_train", 5));
        assert!(ev.detail.contains("5 contributions from 3 chatters"));
        let m: serde_json::Value = serde_json::from_str(&marker).unwrap();
        assert!(m["text"].as_str().unwrap().starts_with("Hype-train-like burst"));
        // More contributions during the active burst stay quiet.
        assert!(t.note_contribution(1100, "d").is_none());
        // After the window drains (>5 min gap), a new burst can fire again.
        assert!(t.note_contribution(2000, "a").is_none());
        assert!(t.note_contribution(2010, "b").is_none());
        assert!(t.note_contribution(2020, "c").is_none());
        assert!(t.note_contribution(2030, "d").is_none());
        assert!(t.note_contribution(2040, "e").is_some(), "re-armed after the lapse");

        // A single whale mass-gifting never counts as a train.
        let mut t = EventTracker::default();
        for i in 0..10 {
            assert!(t.note_contribution(3000 + i, "whale").is_none());
        }
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
