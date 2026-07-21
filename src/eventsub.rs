//! Twitch EventSub real-time detection over WebSocket.
//!
//! Two transport modes are supported, chosen automatically:
//!
//! * **Conduit + app token** (Client ID + Secret): app `client_credentials`
//!   token, conduit bound to the WebSocket shard. Scales to 10,000 channels.
//! * **Direct WebSocket + user token** (Client ID + "Connect with Twitch"
//!   OAuth): the stored user access token is used directly. No conduit or
//!   Client Secret required. Twitch caps this at 10 `stream.online`
//!   subscriptions — sufficient for most users (each channel also gets a
//!   `stream.offline` subscription, so the real subscription count is double
//!   the channel count).
//!
//! When both are present the conduit path is preferred (higher channel cap).
//!
//! Flow (conduit): app token -> resolve logins to user IDs -> ensure a conduit
//! -> open the websocket -> bind shard 0 -> subscribe `stream.online` AND
//! `stream.offline` for each channel -> reconcile current live state via Get
//! Streams -> stream events.
//!
//! Flow (user token): resolve user IDs -> open websocket -> read welcome ->
//! subscribe `stream.online`/`stream.offline` (websocket transport) ->
//! reconcile -> stream events.
//!
//! EventSub has no replay, so we reconcile on every (re)connect: any
//! subscribed channel not currently live clears a stale "live" state (e.g. a
//! `stream.offline` push missed while disconnected) — this is also why
//! `stream.offline` is subscribed at all: without it (or a scheduler poll
//! fallback, which pure `EventSub` deliberately has none of — see
//! `scheduler.rs`), a channel marked "live" would stay that way forever once
//! the broadcast actually ended.
//!
//! Conduit mode additionally subscribes `channel.shared_chat.begin/update/end`
//! ("Stream Together" collabs, default on via the `collab_eventsub` setting) —
//! those events don't carry state themselves here; they just mark the monitor
//! poll-due so the scheduler's collab refresh runs within a tick. User-token
//! mode skips them: WebSocket transport caps TOTAL subscription cost at 10.
//!
//! NOTE: This path needs live Twitch credentials + a channel going live to
//! verify end-to-end; use `--eventsub-test` to exercise it.

use std::collections::{HashMap, HashSet};
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
use crate::events::{LiveSignal, OfflineSignal};
use crate::models::{DetectionMethod, Platform};
use crate::oauth;
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
    offline_tx: UnboundedSender<OfflineSignal>,
    shutdown: Arc<AtomicBool>,
) {
    // Log the "have monitors but no creds" idle reason once, not every retry.
    let mut warned_no_creds = false;
    while !shutdown.load(Ordering::SeqCst) {
        match run_session(&store, &live_tx, &offline_tx, &shutdown, &mut warned_no_creds).await {
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
    offline_tx: &UnboundedSender<OfflineSignal>,
    shutdown: &Arc<AtomicBool>,
    warned_no_creds: &mut bool,
) -> Result<bool> {
    // login(lowercased) -> [monitor_id]
    let login_to_monitors = load_eventsub_monitors(store)?;
    if login_to_monitors.is_empty() {
        return Ok(false);
    }

    let http = Client::builder().timeout(Duration::from_secs(20)).build()?;

    // Prefer conduit + app token (scales to 10k channels). Fall back to direct
    // WebSocket with the stored user OAuth token (≤10 channels, no Client Secret).
    enum Transport {
        Conduit { token: String },
        UserToken { token: String },
    }
    let (client_id, transport) = if let Some((id, secret)) = app_credentials(store) {
        let token = app_token(&http, &id, &secret).await?;
        (id, Transport::Conduit { token })
    } else if let Some((id, token)) = user_credentials(&http, store).await {
        if login_to_monitors.len() > 10 {
            warn!(
                "eventsub: user-token mode supports up to 10 channels; {} configured \
                 (add a Client Secret in Settings for unlimited)",
                login_to_monitors.len()
            );
        }
        (id, Transport::UserToken { token })
    } else {
        if !*warned_no_creds {
            debug!(
                "eventsub: have EventSub monitor(s) but no credentials; idling \
                 (connect with Twitch in Settings, or set Client ID + Client Secret)"
            );
            *warned_no_creds = true;
        }
        return Ok(false);
    };

    let token = match &transport {
        Transport::Conduit { token, .. } | Transport::UserToken { token } => token.as_str(),
    };

    let logins: Vec<String> = login_to_monitors.keys().cloned().collect();
    let login_to_uid = resolve_user_ids(&http, &client_id, token, &logins).await?;
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

    let (mut ws, _) = tokio_tungstenite::connect_async(WS_URL)
        .await
        .context("connecting eventsub websocket")?;
    let (session_id, keepalive) = read_welcome(&mut ws).await?;

    match &transport {
        Transport::Conduit { token } => {
            let conduit_id = ensure_conduit(&http, &client_id, token).await?;
            bind_shard(&http, &client_id, token, &conduit_id, &session_id).await?;
            let sub_transport = json!({ "method": "conduit", "conduit_id": conduit_id });
            let n_on = subscribe_type(&http, &client_id, token, "stream.online", "broadcaster_user_id", &sub_transport, &uid_to_monitors).await;
            let n_off = subscribe_type(&http, &client_id, token, "stream.offline", "broadcaster_user_id", &sub_transport, &uid_to_monitors).await;
            info!(
                "eventsub: connected (conduit {conduit_id}); {n_on}/{} channel(s) subscribed \
                 ({n_off} offline)",
                uid_to_monitors.len()
            );
            // "Stream Together" (Shared Chat) push notifications — conduit
            // mode only: the conduit cost cap is 10,000, while WebSocket
            // transport caps TOTAL cost at 10 (each other-broadcaster sub
            // costs 1), where 3 extra types per channel would be untenable.
            // Default on; the events just poke the scheduler to refresh
            // collab state early (the poll routine stays the source of truth).
            if collab_eventsub_enabled(store) {
                let mut n_collab = 0usize;
                for t in [
                    "channel.shared_chat.begin",
                    "channel.shared_chat.update",
                    "channel.shared_chat.end",
                ] {
                    n_collab += subscribe_type(
                        &http, &client_id, token, t, "broadcaster_user_id",
                        &sub_transport, &uid_to_monitors,
                    )
                    .await;
                }
                debug!(
                    "eventsub: shared-chat (collab) subscriptions active ({n_collab} across 3 types)"
                );
            }
            // Raids in/out — conduit mode only for the same cost-cap reason.
            // `channel.raid` needs NO scope and filters on to_/from_
            // broadcaster ids (one subscription per direction per channel).
            // Events are written straight to `stream_event` for the Channel
            // Stats view; the chat parser's raid capture dedups against these.
            if raid_eventsub_enabled(store) {
                let n_in = subscribe_type(
                    &http, &client_id, token, "channel.raid", "to_broadcaster_user_id",
                    &sub_transport, &uid_to_monitors,
                )
                .await;
                let n_out = subscribe_type(
                    &http, &client_id, token, "channel.raid", "from_broadcaster_user_id",
                    &sub_transport, &uid_to_monitors,
                )
                .await;
                debug!("eventsub: raid subscriptions active ({n_in} incoming / {n_out} outgoing)");
            }
        }
        Transport::UserToken { token } => {
            let sub_transport = json!({ "method": "websocket", "session_id": session_id });
            let n_on = subscribe_type(&http, &client_id, token, "stream.online", "broadcaster_user_id", &sub_transport, &uid_to_monitors).await;
            let n_off = subscribe_type(&http, &client_id, token, "stream.offline", "broadcaster_user_id", &sub_transport, &uid_to_monitors).await;
            info!(
                "eventsub: connected (user-token); {n_on}/{} channel(s) subscribed \
                 ({n_off} offline)",
                uid_to_monitors.len()
            );
            if collab_eventsub_enabled(store) {
                debug!(
                    "eventsub: skipping shared-chat (collab) subscriptions in user-token \
                     mode — WebSocket transport caps total subscription cost at 10 \
                     (collab updates come from polling instead; add a Client Secret \
                     for conduit mode to enable pushes)"
                );
            }
            if raid_eventsub_enabled(store) {
                debug!(
                    "eventsub: skipping raid subscriptions in user-token mode — same \
                     WebSocket cost cap as above (raids are still captured from chat \
                     while recording)"
                );
            }
        }
    }
    // Reconcile: anything already live should start recording now; anything
    // subscribed but not actually live clears a stale "live" state left over
    // from before this (re)connect.
    reconcile(&http, &client_id, token, &login_to_monitors, live_tx, offline_tx).await;

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
                if handle_message(&text, &uid_to_monitors, live_tx, offline_tx, store) {
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
    offline_tx: &UnboundedSender<OfflineSignal>,
    store: &Store,
) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match v["metadata"]["message_type"].as_str().unwrap_or("") {
        "session_keepalive" => {}
        "session_reconnect" => return true,
        "notification" => {
            let event = &v["payload"]["event"];
            match v["payload"]["subscription"]["type"].as_str() {
                Some("stream.online") => {
                    let went_live = event["started_at"].as_str().and_then(parse_ts);
                    let stream_id = event["id"].as_str().map(str::to_string);
                    if let Some(uid) = event["broadcaster_user_id"].as_str() {
                        if let Some(mons) = uid_to_monitors.get(uid) {
                            for &mid in mons {
                                info!(
                                    "eventsub {}: stream.online -> monitor {mid}",
                                    crate::models::Platform::Twitch.tag()
                                );
                                let _ = live_tx.send(
                                    LiveSignal::new(mid, went_live, false)
                                        .with_stream_id(stream_id.clone()),
                                );
                            }
                        }
                    }
                }
                Some("stream.offline") => {
                    if let Some(uid) = event["broadcaster_user_id"].as_str() {
                        if let Some(mons) = uid_to_monitors.get(uid) {
                            for &mid in mons {
                                info!(
                                    "eventsub {}: stream.offline -> monitor {mid}",
                                    crate::models::Platform::Twitch.tag()
                                );
                                let _ = offline_tx.send(mid);
                            }
                        }
                    }
                }
                Some("channel.raid") => {
                    // Record straight into `stream_event`: raid_in on the
                    // target's monitors, raid_out on the source's (both when
                    // both channels are monitored — two different rows on two
                    // different monitors, not a duplicate). The store dedups
                    // against the chat parser's raid capture.
                    let from = event["from_broadcaster_user_name"]
                        .as_str()
                        .or_else(|| event["from_broadcaster_user_login"].as_str())
                        .unwrap_or("");
                    let to = event["to_broadcaster_user_name"]
                        .as_str()
                        .or_else(|| event["to_broadcaster_user_login"].as_str())
                        .unwrap_or("");
                    let viewers = event["viewers"].as_i64().unwrap_or(0);
                    let at = crate::models::now_unix();
                    if let Some(to_id) = event["to_broadcaster_user_id"].as_str()
                        && let Some(mons) = uid_to_monitors.get(to_id)
                    {
                        for &mid in mons {
                            info!(
                                "eventsub {}: {from} raided {to} ({viewers} viewers) -> monitor {mid}",
                                crate::models::Platform::Twitch.tag()
                            );
                            let _ = store
                                .record_stream_event(mid, at, "", "raid_in", from, "", viewers, "");
                        }
                    }
                    if let Some(from_id) = event["from_broadcaster_user_id"].as_str()
                        && let Some(mons) = uid_to_monitors.get(from_id)
                    {
                        for &mid in mons {
                            info!(
                                "eventsub {}: {from} raided out to {to} ({viewers} viewers) -> monitor {mid}",
                                crate::models::Platform::Twitch.tag()
                            );
                            let _ = store
                                .record_stream_event(mid, at, "", "raid_out", from, to, viewers, "");
                        }
                    }
                }
                Some(
                    t @ ("channel.shared_chat.begin"
                    | "channel.shared_chat.update"
                    | "channel.shared_chat.end"),
                ) => {
                    // Poke, don't parse: mark the monitor due so the very next
                    // scheduler tick runs the full collab refresh (the poll
                    // routine is the single source of truth for collab state).
                    // No-op for pure-EventSub monitors (never scheduler-polled)
                    // — their collab state comes from `meta_watcher` while
                    // recording, or the next reconnect reconcile.
                    if let Some(uid) = event["broadcaster_user_id"].as_str()
                        && let Some(mons) = uid_to_monitors.get(uid)
                    {
                        for &mid in mons {
                            debug!(
                                "eventsub {}: {t} -> monitor {mid} (collab poll due)",
                                crate::models::Platform::Twitch.tag()
                            );
                            let _ = store.mark_monitor_poll_due(mid);
                        }
                    }
                }
                _ => {}
            }
        }
        "revocation" => warn!("eventsub: a subscription was revoked"),
        _ => {}
    }
    false
}

/// Whether the "Collab via EventSub" setting is on (default: on). Only takes
/// effect in conduit mode — see the subscribe sites.
fn collab_eventsub_enabled(store: &Store) -> bool {
    store
        .get_setting("collab_eventsub")
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Whether the "Raids via EventSub" setting is on (default: on). Only takes
/// effect in conduit mode — see the subscribe sites. Covers monitors with an
/// EventSub detection method (that's who this task subscribes for); raids on
/// other Twitch monitors are still captured from chat while recording.
fn raid_eventsub_enabled(store: &Store) -> bool {
    store
        .get_setting("raid_eventsub")
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true)
}

fn app_credentials(store: &Store) -> Option<(String, String)> {
    let id = store.get_setting("twitch_client_id").ok().flatten()?;
    let secret = store.get_setting("twitch_client_secret").ok().flatten()?;
    if id.is_empty() || secret.is_empty() {
        None
    } else {
        Some((id, secret))
    }
}

async fn user_credentials(http: &Client, store: &Store) -> Option<(String, String)> {
    let client_id = store
        .get_setting("twitch_client_id")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())?;
    let token = oauth::valid_user_token(http, store).await?;
    Some((client_id, token))
}

fn load_eventsub_monitors(store: &Store) -> Result<HashMap<String, Vec<i64>>> {
    let mut map: HashMap<String, Vec<i64>> = HashMap::new();
    for row in store.list_monitors_with_channels()? {
        if row.monitor.platform() == Platform::Twitch
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
    // The welcome must arrive within ~10s. Skip non-text frames (e.g. Ping)
    // that may precede it; reply to Pings so the server doesn't drop us.
    let text = loop {
        let msg = timeout(Duration::from_secs(15), ws.next())
            .await
            .context("timed out waiting for session_welcome")?
            .context("websocket closed before welcome")??;
        match msg {
            Message::Text(t) => break t,
            Message::Ping(p) => { let _ = ws.send(Message::Pong(p)).await; }
            _ => {}
        }
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

/// Subscribe `event_type` for each user id (skipping ones already enabled),
/// over the given transport JSON (`{"method":"conduit","conduit_id":..}` or
/// `{"method":"websocket","session_id":..}`). `cond_key` is the condition
/// field the uid goes into — `broadcaster_user_id` for most types, but
/// `channel.raid` filters on `to_broadcaster_user_id` /
/// `from_broadcaster_user_id` instead (one direction per call). Returns the
/// count of channels covered.
async fn subscribe_type(
    http: &Client,
    client_id: &str,
    token: &str,
    event_type: &str,
    cond_key: &str,
    transport: &Value,
    uid_to_monitors: &HashMap<String, Vec<i64>>,
) -> usize {
    let existing = existing_subs(http, client_id, token, event_type, cond_key).await;
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
                "type": event_type,
                "version": "1",
                "condition": { cond_key: uid },
                "transport": transport
            }))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => covered += 1,
            // Already exists — a prior connection (or a page `existing_subs`
            // missed, belt-and-braces alongside its own pagination fix below)
            // already created it. Twitch's own idempotency, not an error.
            Ok(r) if r.status() == reqwest::StatusCode::CONFLICT => covered += 1,
            Ok(r) => warn!("eventsub: subscribe {event_type} {uid} failed: {}", r.status()),
            Err(e) => warn!("eventsub: subscribe {event_type} {uid} error: {e}"),
        }
    }
    covered
}

/// All broadcaster user ids with an enabled `event_type` subscription,
/// across every page — the list endpoint paginates (seen in practice once
/// online+offline subscriptions together exceed one page), and reading only
/// the first page made already-subscribed channels look new, causing
/// spurious 409s on every reconnect.
async fn existing_subs(
    http: &Client,
    client_id: &str,
    token: &str,
    event_type: &str,
    cond_key: &str,
) -> HashSet<String> {
    let mut set = HashSet::new();
    let mut after: Option<String> = None;
    loop {
        let mut req = http
            .get(format!("{HELIX}/eventsub/subscriptions"))
            .header("Client-Id", client_id)
            .bearer_auth(token)
            .query(&[("status", "enabled")]);
        if let Some(cursor) = &after {
            req = req.query(&[("after", cursor.as_str())]);
        }
        let Ok(r) = req.send().await else { break };
        let Ok(v) = r.json::<Value>().await else { break };
        if let Some(arr) = v["data"].as_array() {
            for s in arr {
                if s["type"].as_str() == Some(event_type) {
                    if let Some(uid) = s["condition"][cond_key].as_str() {
                        set.insert(uid.to_string());
                    }
                }
            }
        }
        after = v["pagination"]["cursor"].as_str().filter(|c| !c.is_empty()).map(str::to_string);
        if after.is_none() {
            break;
        }
    }
    set
}

/// Send a live signal for any subscribed channel that is currently live (covers
/// the gap since EventSub has no event replay), and an offline signal for any
/// subscribed channel that isn't — clearing a "live" state left stale from
/// before this (re)connect (e.g. a `stream.offline` push missed while
/// disconnected). Only clears within a chunk that actually got a successful
/// response, so a failed/partial Get Streams call never falsely marks
/// channels offline.
async fn reconcile(
    http: &Client,
    client_id: &str,
    token: &str,
    login_to_monitors: &HashMap<String, Vec<i64>>,
    live_tx: &UnboundedSender<LiveSignal>,
    offline_tx: &UnboundedSender<OfflineSignal>,
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
        let Ok(r) = resp else { continue };
        let Ok(v) = r.json::<Value>().await else { continue };
        let Some(arr) = v["data"].as_array() else { continue };
        let mut live_logins: HashSet<String> = HashSet::new();
        for s in arr {
            if s["type"].as_str() == Some("live") {
                let went_live = s["started_at"].as_str().and_then(parse_ts);
                let stream_id = s["id"].as_str().map(str::to_string);
                if let Some(login) = s["user_login"].as_str() {
                    let login = login.to_lowercase();
                    live_logins.insert(login.clone());
                    if let Some(mons) = login_to_monitors.get(&login) {
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
        for login in chunk {
            if live_logins.contains(login) {
                continue;
            }
            if let Some(mons) = login_to_monitors.get(login) {
                for &mid in mons {
                    let _ = offline_tx.send(mid);
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
