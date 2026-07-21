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

/// One stream event parsed from a raw IRC line ([`parse_chat_event`]).
/// Field semantics match the `stream_event` table (see `StreamEventRow`).
#[derive(Debug, PartialEq)]
struct ChatEvent {
    kind: &'static str,
    actor: String,
    target: String,
    amount: i64,
    tier: String,
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
                        if let Some(ev_ctx) = events
                            && let Some(ev) = parse_chat_event(line)
                        {
                            match ev_ctx.store.record_stream_event(
                                ev_ctx.monitor_id,
                                ev.ts,
                                &ev_ctx.stream_id,
                                ev.kind,
                                &ev.actor,
                                &ev.target,
                                ev.amount,
                                &ev.tier,
                            ) {
                                Ok(true) => debug!(
                                    "chat ({login}): event {} by {} (x{})",
                                    ev.kind, ev.actor, ev.amount
                                ),
                                Ok(false) => {} // deduped (EventSub saw the raid first)
                                Err(e) => debug!("chat ({login}): event record failed: {e:#}"),
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

    let (mut display, mut color, mut badges, mut emotes, mut ts_ms) = ("", "", "", "", 0i64);
    for kv in tags.split(';') {
        let mut it = kv.splitn(2, '=');
        let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match k {
            "display-name" => display = v,
            "color" => color = v,
            "badges" => badges = v,
            "emotes" => emotes = v,
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

    if after.starts_with("USERNOTICE ") {
        return match msg_id {
            "sub" => Some(ChatEvent { kind: "sub", actor, target: String::new(), amount: 1, tier, ts }),
            "resub" => Some(ChatEvent {
                kind: "resub",
                actor,
                target: String::new(),
                amount: months.max(1),
                tier,
                ts,
            }),
            "subgift" if !community_batch => Some(ChatEvent {
                kind: "subgift",
                actor,
                target: recipient,
                amount: 1,
                tier,
                ts,
            }),
            "submysterygift" => Some(ChatEvent {
                kind: "subgift",
                actor,
                target: String::new(), // community batch, no single recipient
                amount: gift_count.max(1),
                tier,
                ts,
            }),
            "raid" => Some(ChatEvent {
                kind: "raid_in",
                actor: if raider.is_empty() { actor } else { raider },
                target: String::new(),
                amount: viewer_count,
                tier: String::new(),
                ts,
            }),
            _ => None,
        };
    }
    if bits > 0 && after.starts_with("PRIVMSG ") {
        return Some(ChatEvent { kind: "bits", actor, target: String::new(), amount: bits, tier: String::new(), ts });
    }
    None
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
}
