//! YouTube WebSub (PubSubHubbub) push detection via the external `yt-websub`
//! VPS relay.
//!
//! streamarchiver runs at home and is not reachable from the internet, so it
//! can't receive YouTube's hub callbacks directly. The `yt-websub` server (see
//! `../yt-websub`) lives on a public VPS, subscribes to the hub for our channels,
//! durably logs every notification, and exposes them over a token-authenticated
//! HTTPS API we **poll** from home.
//!
//! A WebSub notification is *not* an authoritative "is live" signal — YouTube
//! fires it for uploads, go-lives, and metadata edits, and it's neither perfectly
//! reliable nor timely. So each event is treated as a **"check this channel now"
//! trigger**: we send `ManualCommand::Start(monitor_id)`, which runs the existing
//! liveness check and records *only if actually live* (idempotent). The scheduler
//! keeps a slow scrape poll on the same monitors as a safety net (analogous to
//! how `EventSubHelix` pairs push with polling on Twitch).
//!
//! Flow each cycle: load enabled WebSub YouTube monitors -> resolve each to its
//! `UC…` channel id (so incoming events map back) -> reconcile the desired set on
//! the VPS (`POST /api/channels`) when it changes -> pull new events
//! (`GET /api/events?after=<cursor>`) -> trigger a liveness check per mapped
//! event -> advance + persist the cursor and ack the VPS. Idles cheaply when
//! there are no WebSub monitors or the VPS isn't configured.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::app_core::sleep_cancellable;
use crate::events::ManualCommand;
use crate::models::{DetectionMethod, Platform};
use crate::store::Store;

const K_URL: &str = "websub_vps_url";
const K_TOKEN: &str = "websub_token";
const K_CURSOR: &str = "websub_cursor";
const K_POLL: &str = "websub_poll_secs";

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
/// How long to wait between API polls by default (the VPS is our own; cheap).
const DEFAULT_POLL_SECS: u64 = 15;
/// Minimum time between liveness checks for the same (monitor, video_id) pair.
/// YouTube fires repeated `updated` events for the same video after a stream
/// ends (metadata edits, final processing), each with a new VPS seq number.
/// Without a cooldown every one would re-trigger a `ManualCommand::Start`.
/// A new video_id (genuine new stream or upload) always bypasses this gate.
const VIDEO_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// Events fetched per `/api/events` page; a full page means more are pending.
const EVENTS_PAGE: usize = 500;
/// How long to idle when there's nothing to do (no monitors / no VPS config).
const IDLE: Duration = Duration::from_secs(30);

/// One event from the VPS `/api/events` feed (only the fields we use).
struct Event {
    seq: u64,
    kind: String,
    channel_id: String,
    video_id: String,
}

/// Run the WebSub poller until shutdown. Idles cheaply when unused.
pub async fn run(
    store: Arc<Store>,
    manual_tx: UnboundedSender<ManualCommand>,
    shutdown: Arc<AtomicBool>,
    jobs: crate::events::JobRegistry,
) {
    let http = Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(20))
        .build()
        .expect("building reqwest client");

    // State carried across cycles.
    let mut uc_cache: HashMap<String, String> = HashMap::new();
    let mut last_sig: Option<String> = None;
    let mut cursor: Option<u64> = None;
    let mut warned_no_config = false;
    // Tracks when each (monitor_id, video_id) last triggered a liveness check,
    // so repeated hub pings for the same ended-stream video don't re-check every
    // poll cycle.  Pruned to a 1-hour horizon each cycle to prevent growth.
    let mut video_last_checked: HashMap<(i64, String), Instant> = HashMap::new();

    while !shutdown.load(Ordering::SeqCst) {
        // Toggleable from the Background view; idle-check for re-enable when off.
        if !store.job_enabled("job_websub_poll") {
            sleep_cancellable(IDLE, &shutdown).await;
            continue;
        }
        let monitors = load_websub_monitors(&store);

        let Some((base, token)) = config(&store) else {
            if !monitors.is_empty() && !warned_no_config {
                debug!(
                    "websub: have WebSub monitor(s) but no VPS URL/token set; idling \
                     (set them in Settings -> YouTube WebSub to enable push)"
                );
                warned_no_config = true;
            }
            sleep_cancellable(IDLE, &shutdown).await;
            continue;
        };
        warned_no_config = false;

        // Resolve every monitored channel to its UC id so events map back.
        let mut uc_to_monitors: HashMap<String, Vec<i64>> = HashMap::new();
        let mut resolve_failed = false;
        for (url, mid) in &monitors {
            match resolve_uc(&http, url, &mut uc_cache).await {
                Some(uc) => uc_to_monitors.entry(uc).or_default().push(*mid),
                None => {
                    warn!("websub: could not resolve YouTube channel id for {url}");
                    resolve_failed = true;
                }
            }
        }

        // Reconcile the VPS subscription set only when it changes (incl. -> empty
        // to unsubscribe when the last WebSub monitor is removed). Skip the
        // reconcile entirely if any channel failed to resolve this cycle: pushing
        // a partial set would unsubscribe channels on the VPS over a transient
        // scrape failure. (Resolutions are cached, so once a channel resolves it
        // keeps resolving; the VPS persists its current subs meanwhile.)
        if !resolve_failed {
            let mut ucs: Vec<String> = uc_to_monitors.keys().cloned().collect();
            ucs.sort();
            let sig = ucs.join(",");
            if last_sig.as_deref() != Some(sig.as_str()) {
                match post_channels(&http, &base, &token, &ucs).await {
                    Ok((sub, unsub, active)) => {
                        info!("websub: reconciled VPS ({active} active; +{sub} -{unsub})");
                        last_sig = Some(sig);
                    }
                    Err(e) => warn!("websub: reconcile failed: {e:#}"),
                }
            }
        }

        if uc_to_monitors.is_empty() {
            sleep_cancellable(IDLE, &shutdown).await;
            continue;
        }

        // First run with no stored cursor: jump to the current max_seq so we don't
        // replay the VPS's whole backlog (old uploads) on startup.
        if cursor.is_none() {
            cursor = match load_cursor(&store) {
                Some(c) => Some(c),
                None => match health(&http, &base, &token).await {
                    Ok(max_seq) => {
                        save_cursor(&store, max_seq);
                        debug!("websub: starting at cursor {max_seq} (skipping backlog)");
                        Some(max_seq)
                    }
                    Err(e) => {
                        warn!("websub: health failed: {e:#}");
                        None
                    }
                },
            };
            if cursor.is_none() {
                sleep_cancellable(IDLE, &shutdown).await;
                continue;
            }
        }

        // Drain new events a page at a time until caught up, so a backlog larger
        // than one page (e.g. after downtime) is processed promptly rather than
        // left for later — and never skipped.
        loop {
            let after = cursor.unwrap_or(0);
            let (events, max_seq) = match poll_events(&http, &base, &token, after).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("websub: poll failed: {e:#}");
                    break;
                }
            };
            let full_page = events.len() >= EVENTS_PAGE;
            // Advance the cursor ONLY to the highest event actually received (never
            // to the server's max_seq) so a >1-page backlog isn't skipped. Dedupe
            // the monitors to check: YouTube fires new + updated and delivery is
            // at-least-once, so one channel can recur within a page — a single
            // liveness check per monitor is enough.
            //
            // Additionally gate on VIDEO_COOLDOWN: YouTube keeps firing `updated`
            // events for the same video_id after a stream ends (metadata edits,
            // final processing), each with a fresh VPS seq.  We suppress repeat
            // checks for the same (monitor, video_id) within the cooldown window.
            // A genuinely new video_id (new stream or upload) always passes through.
            let now = Instant::now();
            let mut to_check: HashSet<i64> = HashSet::new();
            let mut newcur = after;
            for e in &events {
                newcur = newcur.max(e.seq);
                if e.kind == "deleted" {
                    continue;
                }
                if let Some(mons) = uc_to_monitors.get(&e.channel_id) {
                    for &mid in mons {
                        let key = (mid, e.video_id.clone());
                        let cooled_down = video_last_checked
                            .get(&key)
                            .map_or(true, |&t| now.duration_since(t) >= VIDEO_COOLDOWN);
                        if cooled_down {
                            if to_check.insert(mid) {
                                info!(
                                    "websub: {} (video {}) -> check monitor {mid}",
                                    e.kind, e.video_id
                                );
                            }
                            video_last_checked.insert(key, now);
                        } else {
                            debug!(
                                "websub: {} (video {}) suppressed for monitor {mid} (cooldown)",
                                e.kind, e.video_id
                            );
                        }
                    }
                }
            }
            // Prune entries older than 1 hour to keep the map bounded.
            video_last_checked
                .retain(|_, &mut t| now.duration_since(t) < Duration::from_secs(3600));
            for mid in &to_check {
                let _ = manual_tx.send(ManualCommand::Start { id: *mid, user_initiated: false });
            }
            if newcur != after {
                cursor = Some(newcur);
                save_cursor(&store, newcur);
                // Best-effort: let the VPS compact acked events.
                if let Err(e) = ack(&http, &base, &token, newcur).await {
                    debug!("websub: ack failed: {e:#}");
                }
            }
            // Stop draining when caught up (short page), on no progress (defensive
            // against a stuck cursor), or on shutdown; keep going while a full page
            // signals more pending.
            if !full_page || newcur == after || shutdown.load(Ordering::SeqCst) {
                if max_seq > newcur {
                    debug!(
                        "websub: {} event(s) still pending (cursor {newcur}/{max_seq})",
                        max_seq - newcur
                    );
                }
                break;
            }
        }

        let poll = poll_secs(&store);
        crate::events::mark_job(&jobs, "YouTube WebSub poll", poll as i64);
        sleep_cancellable(Duration::from_secs(poll), &shutdown).await;
    }
}

/// YouTube monitors using the WebSub or WebSubOnly method, as `(channel_url, monitor_id)`.
/// Includes disabled monitors so we still receive push notifications and can show live status
/// in the UI even for channels with Auto off.
fn load_websub_monitors(store: &Store) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    if let Ok(rows) = store.list_monitors_with_channels() {
        for row in rows {
            if row.monitor.platform() == Platform::YouTube
                && matches!(
                    row.monitor.detection_method,
                    DetectionMethod::WebSub | DetectionMethod::WebSubOnly
                )
            {
                out.push((row.monitor.url.clone(), row.monitor.id));
            }
        }
    }
    out
}

/// The VPS base URL (trailing slash trimmed) + bearer token, if both are set.
fn config(store: &Store) -> Option<(String, String)> {
    let url = store.get_setting(K_URL).ok().flatten()?;
    let token = store.get_setting(K_TOKEN).ok().flatten()?;
    let url = url.trim().trim_end_matches('/').to_string();
    let token = token.trim().to_string();
    if url.is_empty() || token.is_empty() {
        None
    } else {
        Some((url, token))
    }
}

fn poll_secs(store: &Store) -> u64 {
    store
        .get_setting(K_POLL)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_POLL_SECS)
        .max(5)
}

fn load_cursor(store: &Store) -> Option<u64> {
    store
        .get_setting(K_CURSOR)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn save_cursor(store: &Store, cursor: u64) {
    let _ = store.set_setting(K_CURSOR, &cursor.to_string());
}

/// `POST /api/channels` -> `(subscribed, unsubscribed, active)`.
async fn post_channels(
    http: &Client,
    base: &str,
    token: &str,
    channels: &[String],
) -> Result<(u64, u64, u64)> {
    let resp = http
        .post(format!("{base}/api/channels"))
        .bearer_auth(token)
        .json(&json!({ "channels": channels }))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("channels {}", resp.status());
    }
    let v: Value = resp.json().await.unwrap_or_default();
    Ok((
        v["subscribed"].as_u64().unwrap_or(0),
        v["unsubscribed"].as_u64().unwrap_or(0),
        v["active"].as_u64().unwrap_or(0),
    ))
}

/// `GET /api/health` -> current `max_seq`.
async fn health(http: &Client, base: &str, token: &str) -> Result<u64> {
    let resp = http
        .get(format!("{base}/api/health"))
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("health {}", resp.status());
    }
    let v: Value = resp.json().await?;
    Ok(v["max_seq"].as_u64().unwrap_or(0))
}

/// `GET /api/events?after=<cursor>` -> `(events, max_seq)`.
async fn poll_events(
    http: &Client,
    base: &str,
    token: &str,
    after: u64,
) -> Result<(Vec<Event>, u64)> {
    let resp = http
        .get(format!("{base}/api/events"))
        .query(&[("after", after.to_string()), ("max", EVENTS_PAGE.to_string())])
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("events {}", resp.status());
    }
    let v: Value = resp.json().await?;
    let max_seq = v["max_seq"].as_u64().unwrap_or(after);
    let mut events = Vec::new();
    if let Some(arr) = v["events"].as_array() {
        for e in arr {
            events.push(Event {
                seq: e["seq"].as_u64().unwrap_or(0),
                kind: e["kind"].as_str().unwrap_or_default().to_string(),
                channel_id: e["channel_id"].as_str().unwrap_or_default().to_string(),
                video_id: e["video_id"].as_str().unwrap_or_default().to_string(),
            });
        }
    }
    Ok((events, max_seq))
}

/// `POST /api/ack` to advance the VPS compaction horizon.
async fn ack(http: &Client, base: &str, token: &str, through: u64) -> Result<()> {
    let resp = http
        .post(format!("{base}/api/ack"))
        .bearer_auth(token)
        .json(&json!({ "through": through }))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("ack {}", resp.status());
    }
    Ok(())
}

/// Resolve a YouTube channel URL to its `UC…` id (cached). Tries the URL itself
/// (`/channel/UC…`) first, then scrapes the channel page (no API key needed).
/// Resolve a YouTube channel URL to its `UC…` id (from the URL, else by scraping
/// the channel page). Public wrapper for one-off callers (e.g. manual asset refetch)
/// that don't keep a resolution cache.
pub(crate) async fn resolve_channel_uc(http: &Client, url: &str) -> Option<String> {
    let mut cache = HashMap::new();
    resolve_uc(http, url, &mut cache).await
}

async fn resolve_uc(
    http: &Client,
    url: &str,
    cache: &mut HashMap<String, String>,
) -> Option<String> {
    if let Some(uc) = cache.get(url) {
        return Some(uc.clone());
    }
    if let Some(uc) = uc_from_url(url) {
        cache.insert(url.to_string(), uc.clone());
        return Some(uc);
    }
    if let Some(uc) = scrape_uc(http, url).await {
        cache.insert(url.to_string(), uc.clone());
        return Some(uc);
    }
    None
}

/// Fetch the channel page and extract its `UC…` id from the HTML.
async fn scrape_uc(http: &Client, url: &str) -> Option<String> {
    // Fetch the channel page itself, not its `/live` redirect target.
    let page = url.trim().trim_end_matches('/');
    let page = page.strip_suffix("/live").unwrap_or(page);
    let resp = http
        .get(page)
        .header("Accept-Language", "en-US,en;q=0.9")
        // Bypass the EU consent interstitial that otherwise replaces the page.
        .header("Cookie", "CONSENT=YES+1; SOCS=CAI")
        .send()
        .await
        .ok()?;
    let body = resp.text().await.ok()?;
    find_uc(&body)
}

/// True for characters that can appear in a `UC…` channel id (URL-safe base64).
fn is_uc_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// A valid YouTube channel id is `UC` + 22 URL-safe chars (24 total).
fn valid_uc(id: &str) -> bool {
    id.len() == 24 && id.starts_with("UC") && id.chars().all(is_uc_char)
}

/// Extract a `UC…` id directly from a `/channel/UC…` URL.
fn uc_from_url(url: &str) -> Option<String> {
    let pos = url.find("/channel/")?;
    let rest = &url[pos + "/channel/".len()..];
    let id: String = rest.chars().take_while(|c| is_uc_char(*c)).collect();
    valid_uc(&id).then_some(id)
}

/// Find the channel's own `UC…` id in page HTML. Prefers the channel-identifying
/// markers (`externalId`, canonical `/channel/`) over a generic `channelId`.
pub(crate) fn find_uc(html: &str) -> Option<String> {
    for marker in ["\"externalId\":\"", "/channel/", "\"channelId\":\""] {
        let mut from = 0;
        while let Some(rel) = html[from..].find(marker) {
            let start = from + rel + marker.len();
            let id: String = html[start..]
                .chars()
                .take_while(|c| is_uc_char(*c))
                .collect();
            if valid_uc(&id) {
                return Some(id);
            }
            from = start;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uc_from_channel_url() {
        assert_eq!(
            uc_from_url("https://www.youtube.com/channel/UCaaaaaaaaaaaaaaaaaaaaaa/live").as_deref(),
            Some("UCaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            uc_from_url("https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw").as_deref(),
            Some("UC_x5XG1OV2P6uZZ5FSM9Ttw")
        );
        // Handles aren't UC ids.
        assert_eq!(uc_from_url("https://www.youtube.com/@LofiGirl/live"), None);
        // Too short to be a real id.
        assert_eq!(uc_from_url("https://www.youtube.com/channel/UCshort"), None);
    }

    #[test]
    fn finds_uc_in_html() {
        let html = r#"...<link rel="canonical" href="https://www.youtube.com/channel/UC_x5XG1OV2P6uZZ5FSM9Ttw">...,"externalId":"UCaaaaaaaaaaaaaaaaaaaaaa",..."#;
        // externalId is preferred (the channel's own id).
        assert_eq!(find_uc(html).as_deref(), Some("UCaaaaaaaaaaaaaaaaaaaaaa"));

        let html2 = r#"...href="/channel/UCbbbbbbbbbbbbbbbbbbbbbb">..."#;
        assert_eq!(find_uc(html2).as_deref(), Some("UCbbbbbbbbbbbbbbbbbbbbbb"));

        assert_eq!(find_uc("no channel id here"), None);
    }

    #[test]
    fn valid_uc_rules() {
        assert!(valid_uc("UC_x5XG1OV2P6uZZ5FSM9Ttw"));
        assert!(!valid_uc("UC")); // too short
        assert!(!valid_uc("XXaaaaaaaaaaaaaaaaaaaaaa")); // wrong prefix
        assert!(!valid_uc("UCaaaaaaaaaaaaaaaaaaaaaa!")); // 25 chars / bad char
    }
}
