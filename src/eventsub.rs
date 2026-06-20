//! Twitch EventSub real-time detection over the **conduit + app-token**
//! transport.
//!
//! Why conduit (not a plain user-token websocket): the websocket transport with
//! a user token caps `max_total_cost` at 10 (~10 unowned channels). Binding a
//! websocket shard to a *conduit* created with an **app access token** raises the
//! cap to 10,000 — and reuses the same Client ID/Secret already configured for
//! Helix polling (no interactive OAuth).
//!
//! Flow: app token -> resolve logins to user IDs -> ensure a conduit -> open the
//! websocket -> bind shard 0 to the session -> subscribe `stream.online` for each
//! channel -> reconcile current live state via Get Streams -> stream events.
//! EventSub has no replay, so we reconcile on every (re)connect. Any disconnect
//! drops out and the outer loop reconnects and re-binds.
//!
//! NOTE: This path needs live Twitch credentials + a channel going live to
//! verify end-to-end; use `--eventsub-test` to exercise it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::app_core::sleep_cancellable;
use crate::events::LiveSignal;
use crate::models::{DetectionMethod, Platform};
use crate::store::Store;

const WS_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
const HELIX: &str = "https://api.twitch.tv/helix";
/// How often to re-attempt when idle (no eventsub monitors / no creds).
const IDLE_RETRY: Duration = Duration::from_secs(30);

/// Run the EventSub manager until shutdown. Idles cheaply when there are no
/// Twitch monitors using the EventSub method.
pub async fn run(
    store: Arc<Store>,
    live_tx: UnboundedSender<LiveSignal>,
    shutdown: Arc<AtomicBool>,
) {
    // Log the "have monitors but no creds" idle reason once, not every retry.
    let mut warned_no_creds = false;
    while !shutdown.load(Ordering::SeqCst) {
        match run_session(&store, &live_tx, &shutdown, &mut warned_no_creds).await {
            Ok(true) => warned_no_creds = false, // session ran; reset the one-shot warning
            Ok(false) => sleep_cancellable(IDLE_RETRY, &shutdown).await, // nothing to do
            Err(e) => {
                warn!("eventsub: {e:#}");
                sleep_cancellable(Duration::from_secs(10), &shutdown).await;
            }
        }
    }
}

/// Returns Ok(true) if a websocket session was established, Ok(false) if there
/// was nothing to do (no eventsub monitors / missing creds).
async fn run_session(
    store: &Arc<Store>,
    live_tx: &UnboundedSender<LiveSignal>,
    shutdown: &Arc<AtomicBool>,
    warned_no_creds: &mut bool,
) -> Result<bool> {
    // login(lowercased) -> [monitor_id]
    let login_to_monitors = load_eventsub_monitors(store)?;
    if login_to_monitors.is_empty() {
        return Ok(false);
    }
    let Some((client_id, secret)) = credentials(store) else {
        // We have EventSub monitors but no Client Secret — log the reason once.
        if !*warned_no_creds {
            debug!(
                "eventsub: have EventSub monitor(s) but no Twitch Client Secret; idling \
                 (set a Client Secret in Settings to enable EventSub push)"
            );
            *warned_no_creds = true;
        }
        return Ok(false);
    };

    let http = Client::builder().timeout(Duration::from_secs(20)).build()?;
    let token = app_token(&http, &client_id, &secret).await?;

    let logins: Vec<String> = login_to_monitors.keys().cloned().collect();
    let login_to_uid = resolve_user_ids(&http, &client_id, &token, &logins).await?;
    // user_id -> [monitor_id]
    let mut uid_to_monitors: HashMap<String, Vec<i64>> = HashMap::new();
    for (login, mons) in &login_to_monitors {
        if let Some(uid) = login_to_uid.get(login) {
            uid_to_monitors.entry(uid.clone()).or_default().extend(mons);
        } else {
            warn!("eventsub: could not resolve Twitch user id for '{login}'");
        }
    }
    if uid_to_monitors.is_empty() {
        return Ok(false);
    }

    let conduit_id = ensure_conduit(&http, &client_id, &token).await?;

    let (mut ws, _) = tokio_tungstenite::connect_async(WS_URL)
        .await
        .context("connecting eventsub websocket")?;
    let (session_id, keepalive) = read_welcome(&mut ws).await?;
    bind_shard(&http, &client_id, &token, &conduit_id, &session_id).await?;
    let subscribed =
        subscribe_online(&http, &client_id, &token, &conduit_id, &uid_to_monitors).await;
    info!(
        "eventsub: connected (conduit {conduit_id}); {subscribed}/{} channel(s) subscribed",
        uid_to_monitors.len()
    );

    // Reconcile: anything already live should start recording now.
    reconcile(&http, &client_id, &token, &login_to_monitors, live_tx).await;

    // Event loop. Twitch sends `session_keepalive` every `keepalive` seconds; if
    // none (plus slack) arrives, treat the connection as dead.
    let read_timeout = Duration::from_secs(keepalive + 15);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(true);
        }
        let next = timeout(read_timeout, ws.next()).await;
        let msg = match next {
            Err(_) => bail!("keepalive timeout"),
            Ok(None) => bail!("websocket closed"),
            Ok(Some(Err(e))) => bail!("websocket error: {e}"),
            Ok(Some(Ok(m))) => m,
        };
        use tokio_tungstenite::tungstenite::Message;
        match msg {
            Message::Text(text) => {
                if handle_message(&text, &uid_to_monitors, live_tx) {
                    // session_reconnect — drop out and reconnect fresh.
                    return Ok(true);
                }
            }
            Message::Ping(payload) => {
                let _ = ws.send(Message::Pong(payload)).await;
            }
            Message::Close(_) => bail!("websocket close frame"),
            _ => {}
        }
    }
}

/// Handle one EventSub text frame. Returns true if the caller should reconnect.
fn handle_message(
    text: &str,
    uid_to_monitors: &HashMap<String, Vec<i64>>,
    live_tx: &UnboundedSender<LiveSignal>,
) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match v["metadata"]["message_type"].as_str().unwrap_or("") {
        "session_keepalive" => {}
        "session_reconnect" => return true,
        "notification" => {
            if v["payload"]["subscription"]["type"].as_str() == Some("stream.online") {
                let event = &v["payload"]["event"];
                let went_live = event["started_at"].as_str().and_then(parse_ts);
                let stream_id = event["id"].as_str().map(str::to_string);
                if let Some(uid) = event["broadcaster_user_id"].as_str() {
                    if let Some(mons) = uid_to_monitors.get(uid) {
                        for &mid in mons {
                            info!("eventsub: stream.online -> monitor {mid}");
                            let _ = live_tx.send(
                                LiveSignal::new(mid, went_live, false)
                                    .with_stream_id(stream_id.clone()),
                            );
                        }
                    }
                }
            }
        }
        "revocation" => warn!("eventsub: a subscription was revoked"),
        _ => {}
    }
    false
}

fn credentials(store: &Store) -> Option<(String, String)> {
    let id = store.get_setting("twitch_client_id").ok().flatten()?;
    let secret = store.get_setting("twitch_client_secret").ok().flatten()?;
    if id.is_empty() || secret.is_empty() {
        None
    } else {
        Some((id, secret))
    }
}

fn load_eventsub_monitors(store: &Store) -> Result<HashMap<String, Vec<i64>>> {
    let mut map: HashMap<String, Vec<i64>> = HashMap::new();
    for row in store.list_monitors_with_channels()? {
        if row.monitor.enabled
            && row.monitor.platform() == Platform::Twitch
            && matches!(
                row.monitor.detection_method,
                DetectionMethod::EventSub | DetectionMethod::EventSubHelix
            )
        {
            if let Some(login) = twitch_login(&row.monitor.url) {
                map.entry(login).or_default().push(row.monitor.id);
            }
        }
    }
    Ok(map)
}

async fn app_token(http: &Client, client_id: &str, secret: &str) -> Result<String> {
    let resp = http
        .post("https://id.twitch.tv/oauth2/token")
        .form(&[
            ("client_id", client_id),
            ("client_secret", secret),
            ("grant_type", "client_credentials"),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("token request failed: {}", resp.status());
    }
    let v: Value = resp.json().await?;
    v["access_token"]
        .as_str()
        .map(str::to_string)
        .context("no access_token in token response")
}

async fn resolve_user_ids(
    http: &Client,
    client_id: &str,
    token: &str,
    logins: &[String],
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for chunk in logins.chunks(100) {
        let query: Vec<(&str, &str)> = chunk.iter().map(|l| ("login", l.as_str())).collect();
        let resp = http
            .get(format!("{HELIX}/users"))
            .header("Client-Id", client_id)
            .bearer_auth(token)
            .query(&query)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("get users failed: {}", resp.status());
        }
        let v: Value = resp.json().await?;
        if let Some(arr) = v["data"].as_array() {
            for u in arr {
                if let (Some(login), Some(id)) = (u["login"].as_str(), u["id"].as_str()) {
                    out.insert(login.to_lowercase(), id.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Reuse an existing conduit if present, else create one with a single shard.
async fn ensure_conduit(http: &Client, client_id: &str, token: &str) -> Result<String> {
    let resp = http
        .get(format!("{HELIX}/eventsub/conduits"))
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await?;
    if resp.status().is_success() {
        let v: Value = resp.json().await?;
        if let Some(id) = v["data"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|c| c["id"].as_str())
        {
            return Ok(id.to_string());
        }
    }
    let resp = http
        .post(format!("{HELIX}/eventsub/conduits"))
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .json(&json!({ "shard_count": 1 }))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("create conduit failed: {}", resp.status());
    }
    let v: Value = resp.json().await?;
    v["data"][0]["id"]
        .as_str()
        .map(str::to_string)
        .context("no conduit id in create response")
}

async fn read_welcome(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<(String, u64)> {
    use tokio_tungstenite::tungstenite::Message;
    // The welcome must arrive within ~10s.
    let msg = timeout(Duration::from_secs(15), ws.next())
        .await
        .context("timed out waiting for session_welcome")?
        .context("websocket closed before welcome")??;
    let Message::Text(text) = msg else {
        bail!("expected text welcome frame");
    };
    let v: Value = serde_json::from_str(&text)?;
    if v["metadata"]["message_type"].as_str() != Some("session_welcome") {
        bail!(
            "expected session_welcome, got {}",
            v["metadata"]["message_type"]
        );
    }
    let session_id = v["payload"]["session"]["id"]
        .as_str()
        .context("no session id")?
        .to_string();
    let keepalive = v["payload"]["session"]["keepalive_timeout_seconds"]
        .as_u64()
        .unwrap_or(10);
    Ok((session_id, keepalive))
}

async fn bind_shard(
    http: &Client,
    client_id: &str,
    token: &str,
    conduit_id: &str,
    session_id: &str,
) -> Result<()> {
    let resp = http
        .patch(format!("{HELIX}/eventsub/conduits/shards"))
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .json(&json!({
            "conduit_id": conduit_id,
            "shards": [ { "id": "0", "transport": { "method": "websocket", "session_id": session_id } } ]
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("bind shard failed: {}", resp.status());
    }
    Ok(())
}

/// Subscribe stream.online for each user id (skipping ones already enabled).
/// Returns the count of channels covered.
async fn subscribe_online(
    http: &Client,
    client_id: &str,
    token: &str,
    conduit_id: &str,
    uid_to_monitors: &HashMap<String, Vec<i64>>,
) -> usize {
    let existing = existing_online_subs(http, client_id, token).await;
    let mut covered = 0;
    for uid in uid_to_monitors.keys() {
        if existing.contains(uid) {
            covered += 1;
            continue;
        }
        let resp = http
            .post(format!("{HELIX}/eventsub/subscriptions"))
            .header("Client-Id", client_id)
            .bearer_auth(token)
            .json(&json!({
                "type": "stream.online",
                "version": "1",
                "condition": { "broadcaster_user_id": uid },
                "transport": { "method": "conduit", "conduit_id": conduit_id }
            }))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => covered += 1,
            Ok(r) => warn!("eventsub: subscribe {uid} failed: {}", r.status()),
            Err(e) => warn!("eventsub: subscribe {uid} error: {e}"),
        }
    }
    covered
}

async fn existing_online_subs(
    http: &Client,
    client_id: &str,
    token: &str,
) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let resp = http
        .get(format!("{HELIX}/eventsub/subscriptions"))
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .query(&[("status", "enabled")])
        .send()
        .await;
    if let Ok(r) = resp {
        if let Ok(v) = r.json::<Value>().await {
            if let Some(arr) = v["data"].as_array() {
                for s in arr {
                    if s["type"].as_str() == Some("stream.online") {
                        if let Some(uid) = s["condition"]["broadcaster_user_id"].as_str() {
                            set.insert(uid.to_string());
                        }
                    }
                }
            }
        }
    }
    set
}

/// Send a live signal for any subscribed channel that is currently live (covers
/// the gap since EventSub has no event replay).
async fn reconcile(
    http: &Client,
    client_id: &str,
    token: &str,
    login_to_monitors: &HashMap<String, Vec<i64>>,
    live_tx: &UnboundedSender<LiveSignal>,
) {
    let logins: Vec<String> = login_to_monitors.keys().cloned().collect();
    for chunk in logins.chunks(100) {
        let query: Vec<(&str, &str)> = chunk.iter().map(|l| ("user_login", l.as_str())).collect();
        let resp = http
            .get(format!("{HELIX}/streams"))
            .header("Client-Id", client_id)
            .bearer_auth(token)
            .query(&query)
            .send()
            .await;
        if let Ok(r) = resp {
            if let Ok(v) = r.json::<Value>().await {
                if let Some(arr) = v["data"].as_array() {
                    for s in arr {
                        if s["type"].as_str() == Some("live") {
                            let went_live = s["started_at"].as_str().and_then(parse_ts);
                            let stream_id = s["id"].as_str().map(str::to_string);
                            if let Some(login) = s["user_login"].as_str() {
                                if let Some(mons) = login_to_monitors.get(&login.to_lowercase()) {
                                    for &mid in mons {
                                        let _ = live_tx.send(
                                            LiveSignal::new(mid, went_live, false)
                                                .with_stream_id(stream_id.clone()),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Parse an RFC3339 timestamp (Twitch `started_at`) to unix seconds.
fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Extract the Twitch login from a channel URL (`twitch.tv/<login>`).
fn twitch_login(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let lower = trimmed.to_lowercase();
    let pos = lower.find("twitch.tv/")?;
    let rest = &trimmed[pos + "twitch.tv/".len()..];
    let login = rest.split(['/', '?', '#']).next()?.trim();
    (!login.is_empty()).then(|| login.to_lowercase())
}
