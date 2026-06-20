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
) {
    let Some(login) = crate::detectors::twitch_login(&url) else {
        return;
    };
    info!("chat: logging {login} -> {}", path.display());
    while !done.load(Ordering::SeqCst) && !shutdown.load(Ordering::SeqCst) {
        if let Err(e) = session(&login, &path, &done, &shutdown).await {
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

    // Opened lazily on the first message so a stream with no chat (or a recording
    // that fails immediately) doesn't leave an empty sidecar. Append mode means
    // reconnects continue the same file rather than truncating it.
    let mut file: Option<tokio::fs::File> = None;

    loop {
        if done.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        // 1s read timeout so the stop flags are re-checked even on a quiet chat.
        let msg = match timeout(Duration::from_secs(1), ws.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(None) => return Err(anyhow::anyhow!("chat websocket closed")),
            Err(_) => continue, // read timeout -> re-check flags
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
                    if let Some(json) = parse_privmsg(line) {
                        if file.is_none() {
                            file = Some(
                                tokio::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(path)
                                    .await?,
                            );
                        }
                        let f = file.as_mut().unwrap();
                        f.write_all(json.as_bytes()).await?;
                        f.write_all(b"\n").await?;
                    }
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

    let (mut display, mut color, mut badges, mut ts_ms) = ("", "", "", 0i64);
    for kv in tags.split(';') {
        let mut it = kv.splitn(2, '=');
        let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match k {
            "display-name" => display = v,
            "color" => color = v,
            "badges" => badges = v,
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
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_privmsg() {
        let line = "@badges=subscriber/12;color=#FF0000;display-name=CoolViewer;\
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
}
