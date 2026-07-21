//! Live-detection adapters.
//!
//! - [`DetectContext::detect_twitch`] — batched Helix `Get Streams` (app token,
//!   up to 100 logins per request).
//! - [`DetectContext::detect_scrape`] — YouTube `/live` and Kick channel JSON
//!   (no credentials); falls back to a generic probe for other platforms.
//! - [`DetectContext::detect_generic`] — `streamlink --stream-url` probe for any
//!   supported URL.
//!
//! All methods take/return plain data so the scheduler can batch and dispatch
//! without trait objects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::app_core::sleep_cancellable;
use crate::browser_ua::{BrowserFingerprint, build_browser_fingerprint};
use crate::events::{AppEvent, EventTx};
use crate::models::{
    K_DISCORD_SCHEDULE, K_DISCORD_TOKEN, K_YT_API_DETECT, K_YT_API_SCHEDULE, MonitorWithChannel,
    NotificationKind, Platform, ScheduleSegment, now_unix,
};
use crate::schedule_ocr::{accumulate_ocr_stats, ocr_opts_from_settings, ocr_schedule_image, record_ocr_cache_hit};
use crate::schedule_source::{
    ChannelSourceConfig, ScheduleSourceKind, SourceEntry, community_max_posts,
    effective_order_from, effective_title_fill_from, global_title_fill, load_channel_cfg_map,
    load_channel_scope_map, load_monitor_scope_map, load_source_order,
};
use crate::store::Store;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A single channel to check.
#[derive(Clone, Debug)]
pub struct DetectItem {
    pub monitor_id: i64,
    pub url: String,
    pub platform: Platform,
}

/// The result of checking one monitor.
#[derive(Clone, Debug)]
pub struct DetectOutcome {
    pub monitor_id: i64,
    pub live: bool,
    pub detail: String,
    pub error: bool,
    /// Platform-reported go-live time (unix seconds), when the source provides it
    /// (Twitch Helix). `None` means callers should approximate it.
    pub went_live_at: Option<i64>,
    /// Platform stream/video id, when the source provides it (groups recording
    /// takes of one broadcast). `None` for id-less methods (scrape/probe).
    pub stream_id: Option<String>,
    /// Live stream thumbnail URL (may contain `{width}`/`{height}` placeholders).
    pub thumbnail_url: Option<String>,
    /// Platform user/channel identifier for asset fetching: Twitch numeric user_id,
    /// YouTube UC… channel ID, or Kick slug. `None` for id-less detection paths.
    pub broadcaster_id: Option<String>,
    /// Stream title at detection time, when the platform provides it.
    pub stream_title: Option<String>,
    /// Game/category at detection time, when the platform provides it
    /// (Twitch `game_name`, Kick category). Feeds the trigger-word matcher.
    pub stream_game: Option<String>,
    /// Live viewer count at detection time (Twitch `viewer_count`, Kick
    /// viewers; YouTube best-effort). `None` when the platform/path omits it.
    pub stream_viewers: Option<i64>,
    /// Follower total at detection time (Kick only — see
    /// [`StreamMeta::followers`]). Feeds the `viewer_history` sampling.
    pub stream_followers: Option<i64>,
}

impl DetectOutcome {
    fn live(monitor_id: i64, detail: impl Into<String>) -> DetectOutcome {
        DetectOutcome {
            monitor_id,
            live: true,
            detail: detail.into(),
            error: false,
            went_live_at: None,
            stream_id: None,
            thumbnail_url: None,
            broadcaster_id: None,
            stream_title: None,
            stream_game: None,
            stream_viewers: None,
            stream_followers: None,
        }
    }
    fn live_at(
        monitor_id: i64,
        detail: impl Into<String>,
        went_live_at: Option<i64>,
    ) -> DetectOutcome {
        DetectOutcome {
            went_live_at,
            ..DetectOutcome::live(monitor_id, detail)
        }
    }
    /// Attach a platform stream id (builder-style).
    fn with_stream_id(mut self, stream_id: Option<String>) -> DetectOutcome {
        self.stream_id = stream_id;
        self
    }
    fn with_thumbnail_url(mut self, thumbnail_url: Option<String>) -> DetectOutcome {
        self.thumbnail_url = thumbnail_url;
        self
    }
    fn with_broadcaster_id(mut self, broadcaster_id: Option<String>) -> DetectOutcome {
        self.broadcaster_id = broadcaster_id;
        self
    }
    fn with_stream_title(mut self, stream_title: Option<String>) -> DetectOutcome {
        self.stream_title = stream_title;
        self
    }
    fn with_stream_game(mut self, stream_game: Option<String>) -> DetectOutcome {
        self.stream_game = stream_game;
        self
    }
    fn with_stream_viewers(mut self, stream_viewers: Option<i64>) -> DetectOutcome {
        self.stream_viewers = stream_viewers;
        self
    }
    fn with_stream_followers(mut self, stream_followers: Option<i64>) -> DetectOutcome {
        self.stream_followers = stream_followers;
        self
    }
    fn offline(monitor_id: i64) -> DetectOutcome {
        DetectOutcome {
            monitor_id,
            live: false,
            detail: String::new(),
            error: false,
            went_live_at: None,
            stream_id: None,
            thumbnail_url: None,
            broadcaster_id: None,
            stream_title: None,
            stream_game: None,
            stream_viewers: None,
            stream_followers: None,
        }
    }
    fn err(monitor_id: i64, detail: impl Into<String>) -> DetectOutcome {
        DetectOutcome {
            monitor_id,
            live: false,
            detail: detail.into(),
            error: true,
            went_live_at: None,
            stream_id: None,
            thumbnail_url: None,
            broadcaster_id: None,
            stream_title: None,
            stream_game: None,
            stream_viewers: None,
            stream_followers: None,
        }
    }
}

/// Parse an RFC3339/ISO8601 timestamp (e.g. Twitch `started_at`) to unix seconds.
pub(crate) fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Unix seconds at the start of the local day (00:00). Used as the floor for the
/// Discord/platform dedup so it matches the calendar's start-of-today display.
fn local_day_start() -> i64 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    now.timestamp() - now.time().num_seconds_from_midnight() as i64
}

/// Turn a raw "no Twitch auth available" error into actionable UI guidance.
fn twitch_auth_error(raw: &str) -> String {
    if raw.contains("credentials not set") {
        "Twitch not authenticated — Connect Twitch in Settings, or set Client ID + Secret".into()
    } else {
        raw.to_string()
    }
}

struct TwitchToken {
    access_token: String,
    expires_at: Instant,
}

/// Shared detection state: one HTTP client + cached app tokens.
/// A live Twitch stream's mutable metadata, polled during a recording to log
/// title and game/category changes.
#[derive(Clone, Debug, Default)]
pub struct StreamMeta {
    pub title: String,
    pub game: String,
    /// Live viewer count at fetch time (Twitch/Kick; YouTube's `/live` scrape
    /// has no reliable viewer field, so always `None` there — mirrors
    /// `DetectOutcome::stream_viewers`'s own platform coverage).
    pub viewers: Option<i64>,
    /// Follower total at fetch time. Kick only — its channel JSON carries it
    /// in responses we already pull. Twitch's follower total needs a
    /// moderator-scoped user token and YouTube's subscriber count isn't in
    /// the `/live` page, so both stay `None` (feeds `viewer_history`).
    pub followers: Option<i64>,
}

/// A live Twitch "Stream Together" (Shared Chat) session, as returned by
/// Helix `GET /shared_chat/session`. Participants are broadcaster ids only —
/// the queried channel itself is INCLUDED; display names come from a separate
/// Get Users resolution (`twitch_user_names`).
#[derive(Clone, Debug)]
pub struct SharedChatSession {
    pub session_id: String,
    pub host_id: String,
    pub participant_ids: Vec<String>,
    /// Session start (Twitch `created_at`), unix seconds; 0 = unparsable.
    pub created_at: i64,
}

/// Outcome of one in-recording metadata fetch (`*_stream_meta`). The split
/// between [`Offline`](MetaFetch::Offline) and [`Failed`](MetaFetch::Failed)
/// matters to the caller's failure-streak warning (`meta_watcher`): a stream
/// that simply ended while the capture drains its tail answers `Offline` —
/// frozen title/game/viewer fields are *expected* then, not a defect — whereas
/// `Failed` means the refresh itself is broken and worth warning about.
#[derive(Clone, Debug)]
pub enum MetaFetch {
    /// Channel confirmed live; fresh metadata.
    Live(StreamMeta),
    /// The source answered authoritatively: not live right now.
    Offline,
    /// No answer: network/auth/HTTP/parse failure or missing configuration
    /// (the specific reason is logged at DEBUG by the fetcher).
    Failed,
}

#[cfg(test)]
impl MetaFetch {
    /// The metadata if live, else `None` (test convenience).
    fn live(self) -> Option<StreamMeta> {
        match self {
            MetaFetch::Live(m) => Some(m),
            _ => None,
        }
    }
}

pub struct DetectContext {
    http: Client,
    pub store: Arc<Store>,
    events: EventTx,
    twitch_token: Mutex<Option<TwitchToken>>,
    kick_token: Mutex<Option<TwitchToken>>,
    /// Serializes user-token refresh: Twitch device-code refresh tokens are
    /// one-time-use, so concurrent detection passes must not double-spend one.
    twitch_refresh: Mutex<()>,
    /// Browser fingerprint (UA + Sec-CH-UA headers) derived from the configured
    /// cookies browser. Applied to all YouTube/Kick scrapes.
    fingerprint: BrowserFingerprint,
    /// FNV-1a image hash → last OCR result per `(monitor_id, source_id)`. Skips a
    /// (multi-second, token-spending) re-OCR when the source image is unchanged.
    /// Persisted to `app_settings` (K_OCR_IMAGE_HASHES) so cache hits survive restarts.
    ocr_cache: Mutex<HashMap<(i64, String), (u64, Vec<ScheduleSegment>)>>,
}

/// FNV-1a 64-bit hash — simple, stable, and fast; used instead of `DefaultHasher`
/// (which is not guaranteed stable across Rust versions) for the persisted OCR
/// image cache.
pub(crate) fn fnv64(data: &[u8]) -> u64 {
    const PRIME: u64 = 1_099_511_628_211;
    const BASIS: u64 = 14_695_981_039_346_656_037;
    let mut h = BASIS;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

impl DetectContext {
    pub fn new(store: Arc<Store>, events: EventTx) -> DetectContext {
        let browser = store
            .get_setting("cookies_browser")
            .ok()
            .flatten()
            .unwrap_or_default();
        let browser_name = browser.split(':').next().unwrap_or("chrome");
        let fingerprint = build_browser_fingerprint(if browser_name.is_empty() {
            "chrome"
        } else {
            browser_name
        });
        let http = Client::builder()
            .user_agent(fingerprint.ua.clone())
            .timeout(Duration::from_secs(20))
            .build()
            .expect("building reqwest client");

        // Pre-populate the OCR cache from the persisted hash map so unchanged
        // images are not re-OCR'd after an app restart. The segments come from
        // the DB (they were already stored by the previous OCR run).
        let ocr_cache = {
            let hashes: HashMap<String, u64> = store
                .get_setting(crate::models::K_OCR_IMAGE_HASHES)
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            let mut map: HashMap<(i64, String), (u64, Vec<ScheduleSegment>)> = HashMap::new();
            for (key_str, hash) in hashes {
                if let Some((mid_str, source_id)) = key_str.split_once(':') {
                    if let Ok(monitor_id) = mid_str.parse::<i64>() {
                        let segs = store
                            .schedule_segments_for_source(monitor_id, source_id)
                            .unwrap_or_default();
                        map.insert((monitor_id, source_id.to_string()), (hash, segs));
                    }
                }
            }
            Mutex::new(map)
        };

        DetectContext {
            http,
            store,
            events,
            twitch_token: Mutex::new(None),
            kick_token: Mutex::new(None),
            twitch_refresh: Mutex::new(()),
            fingerprint,
            ocr_cache,
        }
    }

    /// Persist the FNV-1a hash for a single `(monitor_id, source_id)` to the
    /// settings store so the OCR cache survives app restarts.
    fn persist_ocr_hash(&self, monitor_id: i64, source_id: &str, hash: u64) {
        let key_str = format!("{monitor_id}:{source_id}");
        let mut hashes: HashMap<String, u64> = self
            .store
            .get_setting(crate::models::K_OCR_IMAGE_HASHES)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        hashes.insert(key_str, hash);
        if let Ok(json) = serde_json::to_string(&hashes) {
            let _ = self.store.set_setting(crate::models::K_OCR_IMAGE_HASHES, &json);
        }
    }

    /// Persist the OCR re-check cadence stamp (unix seconds) for a single
    /// `(monitor_id, source_id)` so the slow OCR cadence holds across restarts —
    /// a rebuild/restart can't reset the timer and trigger a fresh re-OCR sweep.
    fn persist_ocr_attempt(&self, monitor_id: i64, source_id: &str, ts: i64) {
        let key_str = format!("{monitor_id}:{source_id}");
        let mut stamps: HashMap<String, i64> = self
            .store
            .get_setting(crate::models::K_OCR_LAST_ATTEMPT)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        stamps.insert(key_str, ts);
        if let Ok(json) = serde_json::to_string(&stamps) {
            let _ = self.store.set_setting(crate::models::K_OCR_LAST_ATTEMPT, &json);
        }
    }

    /// Clone of the shared HTTP client for use outside this struct (e.g. asset fetching).
    pub fn http_client(&self) -> Client {
        self.http.clone()
    }

    /// True when today's YouTube Data API usage has reached or exceeded the
    /// configured cutoff (default 9000 units). Callers skip API calls when true.
    fn youtube_quota_exceeded(&self) -> bool {
        let cutoff: i64 = self
            .store
            .get_setting(crate::models::K_YT_API_QUOTA_CUTOFF)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(9000);
        let used = self.store.get_quota_today("youtube").unwrap_or(0);
        used >= cutoff
    }

    /// Obtain a valid Twitch app token and the configured Client-Id.
    /// Suitable for Helix API calls that don't need a user scope (badges, emotes, users).
    pub async fn twitch_helix_auth(&self) -> anyhow::Result<(String, String)> {
        let client_id = self
            .store
            .get_setting("twitch_client_id")?
            .unwrap_or_default();
        let token = self.twitch_app_token().await?;
        Ok((client_id, token))
    }

    // ----- Twitch Helix -----

    async fn twitch_app_token(&self) -> Result<String> {
        if let Some(tok) = self.twitch_token.lock().await.as_ref() {
            if tok.expires_at > Instant::now() {
                return Ok(tok.access_token.clone());
            }
        }
        let client_id = self
            .store
            .get_setting("twitch_client_id")?
            .unwrap_or_default();
        let client_secret = self
            .store
            .get_setting("twitch_client_secret")?
            .unwrap_or_default();
        if client_id.is_empty() || client_secret.is_empty() {
            bail!("Twitch credentials not set (Settings)");
        }

        let resp = self
            .http
            .post("https://id.twitch.tv/oauth2/token")
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Twitch token request failed: {status} {body}");
        }

        #[derive(Deserialize)]
        struct TokenResp {
            access_token: String,
            expires_in: u64,
        }
        let tr: TokenResp = resp.json().await?;
        let token = tr.access_token.clone();
        // Refresh a minute early.
        let ttl = Duration::from_secs(tr.expires_in.saturating_sub(60).max(60));
        *self.twitch_token.lock().await = Some(TwitchToken {
            access_token: tr.access_token,
            expires_at: Instant::now() + ttl,
        });
        Ok(token)
    }

    /// Check whether the connected Twitch account is subscribed to
    /// `broadcaster_login`. `Some(true)`/`Some(false)` when conclusively known;
    /// `None` when undeterminable (not connected, missing the
    /// `user:read:subscriptions` scope / 401, or a lookup error).
    pub async fn check_twitch_sub(&self, broadcaster_login: &str) -> Option<bool> {
        // Cheap local gates first, so an account that can't yield a determinate
        // answer (no stored user id, no Client ID) bails before any network work.
        let user_id = crate::oauth::connected_user_id(self.store.as_ref())?;
        let client_id = self
            .store
            .get_setting("twitch_client_id")
            .ok()
            .flatten()
            .unwrap_or_default();
        if client_id.is_empty() {
            return None;
        }
        // Serialize token refresh (device-code refresh tokens are one-time-use), as
        // detect_twitch does, so a concurrent detection pass can't double-spend it.
        let token = {
            let _guard = self.twitch_refresh.lock().await;
            crate::oauth::valid_user_token(&self.http, self.store.as_ref()).await
        }?;
        let broadcaster_id = self
            .twitch_user_id(&client_id, &token, broadcaster_login)
            .await?;
        let resp = self
            .http
            .get("https://api.twitch.tv/helix/subscriptions/user")
            .header("Client-Id", &client_id)
            .bearer_auth(&token)
            .query(&[
                ("broadcaster_id", broadcaster_id.as_str()),
                ("user_id", user_id.as_str()),
            ])
            .send()
            .await
            .ok()?;
        match resp.status() {
            s if s.is_success() => Some(true),
            reqwest::StatusCode::NOT_FOUND => Some(false), // 404 = not subscribed
            // 400 is returned when broadcaster_id == user_id (you can't subscribe to
            // your own channel). That's conclusive ("no sub benefit"), so cache it
            // instead of re-querying this monitor every refresh pass forever.
            reqwest::StatusCode::BAD_REQUEST => Some(false),
            _ => None, // 401 (scope missing/expired), 5xx -> unknown (retry later)
        }
    }

    /// Resolve a Twitch login to its numeric user id (Helix Get Users).
    pub(crate) async fn twitch_user_id(&self, client_id: &str, token: &str, login: &str) -> Option<String> {
        let resp = self
            .http
            .get("https://api.twitch.tv/helix/users")
            .header("Client-Id", client_id)
            .bearer_auth(token)
            .query(&[("login", login)])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        v["data"][0]["id"].as_str().map(str::to_string)
    }

    pub async fn detect_twitch(&self, items: &[DetectItem]) -> Vec<DetectOutcome> {
        let client_id = self
            .store
            .get_setting("twitch_client_id")
            .ok()
            .flatten()
            .unwrap_or_default();

        let mut outcomes = Vec::new();
        let mut login_to_mons: HashMap<String, Vec<i64>> = HashMap::new();
        for it in items {
            match twitch_login(&it.url) {
                Some(login) => login_to_mons.entry(login).or_default().push(it.monitor_id),
                None => outcomes.push(DetectOutcome::err(
                    it.monitor_id,
                    "cannot parse twitch login",
                )),
            }
        }

        // Resolve an auth token: a connected user token (refreshed if needed,
        // serialized because device-code refresh tokens are one-time-use), else an
        // app token (client-credentials, which needs the Client Secret).
        let user_token = {
            let _guard = self.twitch_refresh.lock().await;
            crate::oauth::valid_user_token(&self.http, self.store.as_ref()).await
        };
        let mut using_user_token = user_token.is_some();
        let mut token = match user_token {
            Some(t) => t,
            None => match self.twitch_app_token().await {
                Ok(t) => t,
                Err(e) => {
                    let msg = twitch_auth_error(&e.to_string());
                    for mons in login_to_mons.values() {
                        for mid in mons {
                            outcomes.push(DetectOutcome::err(*mid, msg.clone()));
                        }
                    }
                    return outcomes;
                }
            },
        };

        #[derive(Deserialize)]
        struct Stream {
            id: String,
            user_login: String,
            user_id: String,
            thumbnail_url: String,
            #[serde(rename = "type")]
            kind: String,
            started_at: Option<String>,
            title: Option<String>,
            game_name: Option<String>,
            viewer_count: Option<i64>,
        }
        #[derive(Deserialize)]
        struct StreamsResp {
            data: Vec<Stream>,
        }

        let logins: Vec<String> = login_to_mons.keys().cloned().collect();
        for chunk in logins.chunks(100) {
            let query: Vec<(&str, &str)> =
                chunk.iter().map(|l| ("user_login", l.as_str())).collect();
            // Up to two attempts: the chosen token, then an app-token fallback if a
            // user token is rejected (stale/revoked/Client-Id mismatch -> 401).
            loop {
                let resp = self
                    .http
                    .get("https://api.twitch.tv/helix/streams")
                    .header("Client-Id", &client_id)
                    .bearer_auth(&token)
                    .query(&query)
                    .send()
                    .await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        // login -> (went_live, stream_id, user_id, thumbnail_url, title, game, viewers)
                        #[allow(clippy::type_complexity)]
                        let live: HashMap<String, (Option<i64>, Option<String>, String, String, Option<String>, Option<String>, Option<i64>)> =
                            match r.json::<StreamsResp>().await
                        {
                            Ok(sr) => sr
                                .data
                                .into_iter()
                                .filter(|s| s.kind == "live")
                                .map(|s| {
                                    let when = s.started_at.as_deref().and_then(parse_rfc3339);
                                    (
                                        s.user_login.to_lowercase(),
                                        (when, Some(s.id), s.user_id, s.thumbnail_url, s.title, s.game_name, s.viewer_count),
                                    )
                                })
                                .collect(),
                            Err(e) => {
                                for l in chunk {
                                    for mid in &login_to_mons[l] {
                                        outcomes.push(DetectOutcome::err(
                                            *mid,
                                            format!("helix parse: {e}"),
                                        ));
                                    }
                                }
                                break;
                            }
                        };
                        for l in chunk {
                            let key = l.to_lowercase();
                            for mid in &login_to_mons[l] {
                                outcomes.push(match live.get(&key) {
                                    Some((went, id, uid, thumb, title, game, viewers)) => {
                                        DetectOutcome::live_at(*mid, "live", *went)
                                            .with_stream_id(id.clone())
                                            .with_broadcaster_id(Some(uid.clone()))
                                            .with_thumbnail_url(Some(thumb.clone()))
                                            .with_stream_title(title.clone())
                                            .with_stream_game(game.clone())
                                            .with_stream_viewers(*viewers)
                                    }
                                    None => DetectOutcome::offline(*mid),
                                });
                            }
                        }
                        break;
                    }
                    // A user token was rejected — fall back to the app token once.
                    Ok(r)
                        if r.status() == reqwest::StatusCode::UNAUTHORIZED && using_user_token =>
                    {
                        match self.twitch_app_token().await {
                            Ok(app) => {
                                token = app;
                                using_user_token = false;
                                continue;
                            }
                            Err(_) => {
                                for l in chunk {
                                    for mid in &login_to_mons[l] {
                                        outcomes.push(DetectOutcome::err(
                                            *mid,
                                            "Twitch token expired — reconnect in Settings, or set a Client Secret",
                                        ));
                                    }
                                }
                                break;
                            }
                        }
                    }
                    Ok(r) => {
                        let status = r.status();
                        let msg = if status == reqwest::StatusCode::UNAUTHORIZED {
                            "Twitch auth rejected (401) — reconnect in Settings or check Client ID/Secret".to_string()
                        } else {
                            format!("helix {status}")
                        };
                        for l in chunk {
                            for mid in &login_to_mons[l] {
                                outcomes.push(DetectOutcome::err(*mid, msg.clone()));
                            }
                        }
                        break;
                    }
                    Err(e) => {
                        // A transport-level failure (DNS/connect/TLS/timeout) hits
                        // the WHOLE batched request, not one channel — and
                        // reqwest's error Display embeds the full request URL
                        // (every login in the chunk as a query param, easily a
                        // few hundred bytes for a full batch). Log the real error
                        // ONCE here; give each monitor a short detail instead, so
                        // one network hiccup doesn't repeat a multi-KB line once
                        // per channel in the scheduler's per-monitor state-change
                        // log (100 channels -> the same error dozens of times).
                        warn!(
                            "Twitch Helix streams: batch request failed for {} channel(s): {e}",
                            chunk.len()
                        );
                        for l in chunk {
                            for mid in &login_to_mons[l] {
                                outcomes.push(DetectOutcome::err(
                                    *mid,
                                    "Twitch Helix request failed (network/DNS) — see the warning above for detail",
                                ));
                            }
                        }
                        break;
                    }
                }
            }
        }
        outcomes
    }

    /// Title + game/category of a currently-live Twitch channel, for the
    /// in-recording metadata change log (the scheduler pauses polling while a
    /// monitor records, so the [`Supervisor`](crate::downloader::Supervisor)
    /// polls this directly). [`MetaFetch::Offline`] when the channel isn't in
    /// Helix's live response (ended); [`MetaFetch::Failed`] on error or when
    /// Twitch credentials aren't configured. Mirrors `detect_twitch`'s token
    /// handling: a connected user token if present, else the app token, with a
    /// one-shot app-token fallback on a 401.
    pub async fn twitch_stream_meta(&self, url: &str) -> MetaFetch {
        // Silent `None` here used to be undiagnosable — a mid-take auth hiccup
        // (e.g. a user token expiring) would just freeze viewer count/title/
        // game for the rest of the recording with nothing in the log to explain
        // why. Every early exit below logs its specific reason at DEBUG (the
        // caller, `meta_watcher`, warns once at the point it notices repeated
        // failures — this function itself has no per-take failure-streak state).
        let Some(login) = twitch_login(url) else {
            debug!("twitch_stream_meta: couldn't parse a login from {url}");
            return MetaFetch::Failed;
        };
        let client_id = self
            .store
            .get_setting("twitch_client_id")
            .ok()
            .flatten()
            .unwrap_or_default();
        if client_id.is_empty() {
            debug!("twitch_stream_meta: no twitch_client_id configured");
            return MetaFetch::Failed;
        }
        let user_token = {
            let _guard = self.twitch_refresh.lock().await;
            crate::oauth::valid_user_token(&self.http, self.store.as_ref()).await
        };
        let mut using_user_token = user_token.is_some();
        let mut token = match user_token {
            Some(t) => t,
            None => match self.twitch_app_token().await {
                Ok(t) => t,
                Err(e) => {
                    debug!("twitch_stream_meta: app-token fallback failed: {e:#}");
                    return MetaFetch::Failed;
                }
            },
        };

        #[derive(Deserialize)]
        struct Stream {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            title: String,
            #[serde(default)]
            game_name: String,
            #[serde(default)]
            viewer_count: i64,
        }
        #[derive(Deserialize)]
        struct Resp {
            data: Vec<Stream>,
        }

        loop {
            let resp = match self
                .http
                .get("https://api.twitch.tv/helix/streams")
                .header("Client-Id", &client_id)
                .bearer_auth(&token)
                .query(&[("user_login", login.as_str())])
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    debug!("twitch_stream_meta: request for {login} failed: {e}");
                    return MetaFetch::Failed;
                }
            };
            match resp.status() {
                s if s.is_success() => {
                    let sr: Resp = match resp.json().await {
                        Ok(sr) => sr,
                        Err(e) => {
                            debug!("twitch_stream_meta: response parse for {login} failed: {e}");
                            return MetaFetch::Failed;
                        }
                    };
                    let Some(s) = sr.data.into_iter().find(|s| s.kind == "live") else {
                        debug!("twitch_stream_meta: {login} not in the live response (ended?)");
                        return MetaFetch::Offline;
                    };
                    return MetaFetch::Live(StreamMeta {
                        title: s.title,
                        game: s.game_name,
                        viewers: Some(s.viewer_count),
                        followers: None,
                    });
                }
                reqwest::StatusCode::UNAUTHORIZED if using_user_token => {
                    token = match self.twitch_app_token().await {
                        Ok(t) => t,
                        Err(e) => {
                            debug!(
                                "twitch_stream_meta: user token rejected for {login} and \
                                 app-token fallback failed: {e:#}"
                            );
                            return MetaFetch::Failed;
                        }
                    };
                    using_user_token = false;
                    continue;
                }
                s => {
                    debug!("twitch_stream_meta: {login} got HTTP {s} from Helix");
                    return MetaFetch::Failed;
                }
            }
        }
    }

    // ----- Twitch "Stream Together" (Shared Chat) collabs -----

    /// The active Shared Chat session for `broadcaster_id`, via Helix
    /// `GET /shared_chat/session` (no scope needed — app token works; verified
    /// live 2026-07-21). `Ok(None)` = definitively no session (the normal
    /// case); `Err` = transient/auth failure, in which case the caller must
    /// NOT end stored sessions or clear live state.
    pub async fn twitch_shared_chat(
        &self,
        broadcaster_id: &str,
    ) -> Result<Option<SharedChatSession>> {
        let (client_id, token) = self.twitch_helix_auth().await?;
        let resp = self
            .http
            .get("https://api.twitch.tv/helix/shared_chat/session")
            .header("Client-Id", &client_id)
            .bearer_auth(&token)
            .query(&[("broadcaster_id", broadcaster_id)])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("shared_chat/session got HTTP {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        let Some(item) = v["data"].as_array().and_then(|a| a.first()) else {
            return Ok(None);
        };
        Ok(Some(SharedChatSession {
            session_id: item["session_id"].as_str().unwrap_or("").to_string(),
            host_id: item["host_broadcaster_id"].as_str().unwrap_or("").to_string(),
            participant_ids: item["participants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["broadcaster_id"].as_str())
                .map(str::to_string)
                .collect(),
            created_at: item["created_at"].as_str().and_then(parse_rfc3339).unwrap_or(0),
        }))
    }

    /// Resolve Twitch broadcaster ids to `(login, display_name)` via Helix Get
    /// Users (100/call), backed by the persistent `twitch_user_name_cache`
    /// settings blob — collab partners recur, so steady-state polls resolve
    /// entirely from cache. Ids that fail to resolve are absent from the map.
    /// (Renames go stale in the cache by design: names are captured for
    /// display, and session history keeps the name at observation time anyway.)
    pub async fn twitch_user_names(
        &self,
        ids: &[String],
    ) -> HashMap<String, (String, String)> {
        const K_CACHE: &str = "twitch_user_name_cache";
        let mut cache: HashMap<String, (String, String)> = self
            .store
            .get_setting(K_CACHE)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let missing: Vec<String> =
            ids.iter().filter(|id| !cache.contains_key(*id)).cloned().collect();
        if !missing.is_empty()
            && let Ok((client_id, token)) = self.twitch_helix_auth().await
        {
            let mut dirty = false;
            for chunk in missing.chunks(100) {
                let query: Vec<(&str, &str)> =
                    chunk.iter().map(|id| ("id", id.as_str())).collect();
                let resp = self
                    .http
                    .get("https://api.twitch.tv/helix/users")
                    .header("Client-Id", &client_id)
                    .bearer_auth(&token)
                    .query(&query)
                    .send()
                    .await;
                let Ok(resp) = resp else { continue };
                let Ok(v) = resp.json::<serde_json::Value>().await else { continue };
                for u in v["data"].as_array().into_iter().flatten() {
                    let (Some(id), Some(login)) = (u["id"].as_str(), u["login"].as_str()) else {
                        continue;
                    };
                    let display = u["display_name"].as_str().unwrap_or(login);
                    cache.insert(id.to_string(), (login.to_string(), display.to_string()));
                    dirty = true;
                }
            }
            if dirty && let Ok(json) = serde_json::to_string(&cache) {
                let _ = self.store.set_setting(K_CACHE, &json);
            }
        }
        ids.iter()
            .filter_map(|id| cache.get(id).map(|v| (id.clone(), v.clone())))
            .collect()
    }

    /// One live Twitch monitor's full collab refresh — the single routine both
    /// feeds share (scheduler poll for live-not-recording, `meta_watcher` while
    /// recording, EventSub pokes indirectly by forcing a poll): fetch the
    /// Shared Chat session, resolve partner names, parse title `@mentions`,
    /// upsert `collab_session` rows, end vanished sessions, log set changes to
    /// the 📝 ledger (`kind = "collab"`), and write `monitor.last_collab` for
    /// the grid. A failed Shared Chat fetch degrades gracefully: nothing is
    /// ended or cleared, and previously-known shared-chat partners are kept in
    /// the live JSON so the UI doesn't flicker.
    pub async fn refresh_twitch_collab(
        &self,
        monitor_id: i64,
        login: &str,
        broadcaster_id: Option<String>,
        stream_id: &str,
        title: &str,
    ) {
        let now = crate::models::now_unix();
        let bid = match broadcaster_id {
            Some(b) if !b.is_empty() => Some(b),
            _ => self.twitch_id_for_login(login).await,
        };

        let mut partners: Vec<crate::models::CollabPartner> = Vec::new();
        let mut keep_sessions: Vec<String> = Vec::new();
        let mut keep_streams: Vec<String> = Vec::new();
        let mut host_id = String::new();
        let mut since = 0i64;
        let mut shared_ok = false;

        if let Some(bid) = &bid {
            match self.twitch_shared_chat(bid).await {
                Ok(Some(sess)) => {
                    shared_ok = true;
                    let other_ids: Vec<String> = sess
                        .participant_ids
                        .iter()
                        .filter(|id| *id != bid)
                        .cloned()
                        .collect();
                    let names = self.twitch_user_names(&other_ids).await;
                    let ps: Vec<crate::models::CollabPartner> = other_ids
                        .iter()
                        .map(|id| {
                            let (l, n) = names
                                .get(id)
                                .cloned()
                                .unwrap_or_else(|| (String::new(), format!("#{id}")));
                            crate::models::CollabPartner {
                                id: id.clone(),
                                login: l,
                                name: n,
                                from_title: false,
                            }
                        })
                        .collect();
                    host_id = sess.host_id.clone();
                    since = sess.created_at;
                    if !ps.is_empty() {
                        keep_sessions.push(sess.session_id.clone());
                        match self.store.upsert_collab_session(
                            monitor_id,
                            "shared_chat",
                            &sess.session_id,
                            stream_id,
                            &sess.host_id,
                            &ps,
                            sess.created_at,
                            now,
                        ) {
                            Ok(Some((old, new))) => {
                                let _ = self.store.insert_monitor_stream_change(
                                    monitor_id, now, "collab", &old, &new,
                                );
                            }
                            Ok(None) => {}
                            Err(e) => debug!("collab: session upsert failed: {e:#}"),
                        }
                    }
                    partners.extend(ps);
                }
                Ok(None) => shared_ok = true,
                Err(e) => {
                    debug!("collab: shared-chat fetch for {login} failed: {e:#}");
                    // Keep showing what we knew — don't flicker on a blip.
                    if let Some(prev) = self
                        .store
                        .monitor_last_collab(monitor_id)
                        .ok()
                        .and_then(|j| crate::models::CollabLive::parse(&j))
                    {
                        host_id = prev.host_id;
                        since = prev.since_unix;
                        partners.extend(prev.partners.into_iter().filter(|p| !p.from_title));
                    }
                }
            }
        }

        let mentions = crate::models::title_mentions(title, login);
        let title_ps: Vec<crate::models::CollabPartner> = mentions
            .iter()
            .filter(|m| {
                !partners
                    .iter()
                    .any(|p| p.login.eq_ignore_ascii_case(m) || p.name.eq_ignore_ascii_case(m))
            })
            .map(|m| crate::models::CollabPartner {
                id: String::new(),
                login: m.to_lowercase(),
                name: m.clone(),
                from_title: true,
            })
            .collect();
        if !title_ps.is_empty() && !stream_id.is_empty() {
            keep_streams.push(stream_id.to_string());
            match self.store.upsert_collab_session(
                monitor_id, "title", "", stream_id, "", &title_ps, 0, now,
            ) {
                Ok(Some((old, new))) => {
                    let _ = self
                        .store
                        .insert_monitor_stream_change(monitor_id, now, "collab", &old, &new);
                }
                Ok(None) => {}
                Err(e) => debug!("collab: title-session upsert failed: {e:#}"),
            }
        }
        partners.extend(title_ps);

        // End sessions that vanished — but only when the Shared Chat answer was
        // authoritative (success or definitive "none"), never on a failed fetch.
        if shared_ok {
            let keep_s: Vec<&str> = keep_sessions.iter().map(String::as_str).collect();
            let keep_t: Vec<&str> = keep_streams.iter().map(String::as_str).collect();
            match self.store.end_open_collab_sessions(monitor_id, &keep_s, &keep_t, now) {
                Ok(closed) => {
                    for names in closed {
                        let _ = self
                            .store
                            .insert_monitor_stream_change(monitor_id, now, "collab", &names, "");
                    }
                }
                Err(e) => debug!("collab: ending stale sessions failed: {e:#}"),
            }
        }

        let json = if partners.is_empty() {
            String::new()
        } else {
            crate::models::CollabLive { host_id, since_unix: since, partners }.to_json()
        };
        let _ = self.store.set_monitor_live_collab(monitor_id, &json);
    }

    /// Resolve a Twitch login to its broadcaster id via Helix Get Users,
    /// seeding the name cache along the way. Used by the in-recording
    /// `meta_watcher` path, which knows only the monitor URL.
    pub async fn twitch_id_for_login(&self, login: &str) -> Option<String> {
        let (client_id, token) = self.twitch_helix_auth().await.ok()?;
        let resp = self
            .http
            .get("https://api.twitch.tv/helix/users")
            .header("Client-Id", &client_id)
            .bearer_auth(&token)
            .query(&[("login", login)])
            .send()
            .await
            .ok()?;
        let v: serde_json::Value = resp.json().await.ok()?;
        let u = v["data"].as_array().and_then(|a| a.first())?;
        Some(u["id"].as_str()?.to_string())
    }

    // ----- schedule (upcoming streams) -----

    /// A Twitch channel's upcoming scheduled streams via Helix `Get Channel Stream
    /// Schedule`. Mirrors [`twitch_stream_meta`](Self::twitch_stream_meta)'s token
    /// handling. `Some(vec)` on success (empty when no schedule is set up — Helix
    /// returns 404); `None` on error / missing credentials, so the refresher won't
    /// wipe a previously-fetched schedule on a transient failure.
    pub async fn twitch_schedule(&self, url: &str) -> Option<Vec<ScheduleSegment>> {
        let login = twitch_login(url)?;
        let client_id = self
            .store
            .get_setting("twitch_client_id")
            .ok()
            .flatten()
            .unwrap_or_default();
        if client_id.is_empty() {
            return None;
        }
        let user_token = {
            let _guard = self.twitch_refresh.lock().await;
            crate::oauth::valid_user_token(&self.http, self.store.as_ref()).await
        };
        let mut using_user_token = user_token.is_some();
        let mut token = match user_token {
            Some(t) => t,
            None => self.twitch_app_token().await.ok()?,
        };
        // Resolve the broadcaster id; if a user token is rejected here, fall back
        // to the app token (the 401 loop below only covers the schedule call).
        let broadcaster_id = match self.twitch_user_id(&client_id, &token, &login).await {
            Some(id) => id,
            None if using_user_token => {
                token = self.twitch_app_token().await.ok()?;
                using_user_token = false;
                self.twitch_user_id(&client_id, &token, &login).await?
            }
            None => return None,
        };

        #[derive(Deserialize)]
        struct Cat {
            #[serde(default)]
            name: String,
        }
        #[derive(Deserialize)]
        struct Seg {
            start_time: Option<String>,
            end_time: Option<String>,
            #[serde(default)]
            title: String,
            canceled_until: Option<String>,
            category: Option<Cat>,
        }
        #[derive(Deserialize)]
        struct Data {
            #[serde(default)]
            segments: Vec<Seg>,
        }
        #[derive(Deserialize)]
        struct Resp {
            data: Data,
        }

        loop {
            let resp = self
                .http
                .get("https://api.twitch.tv/helix/schedule")
                .header("Client-Id", &client_id)
                .bearer_auth(&token)
                .query(&[("broadcaster_id", broadcaster_id.as_str()), ("first", "25")])
                .send()
                .await
                .ok()?;
            match resp.status() {
                s if s.is_success() => {
                    let r: Resp = resp.json().await.ok()?;
                    let segs = r
                        .data
                        .segments
                        .into_iter()
                        .filter_map(|seg| {
                            let start = seg.start_time.as_deref().and_then(parse_rfc3339)?;
                            Some(ScheduleSegment {
                                id: 0,
                                monitor_id: 0,
                                start_time: start,
                                end_time: seg.end_time.as_deref().and_then(parse_rfc3339),
                                title: seg.title,
                                category: seg.category.map(|c| c.name).unwrap_or_default(),
                                canceled: seg.canceled_until.is_some(),
                                video_id: None,
                                collab: String::new(),
                            })
                        })
                        .collect();
                    return Some(segs);
                }
                // The broadcaster hasn't set up a schedule.
                reqwest::StatusCode::NOT_FOUND => return Some(Vec::new()),
                reqwest::StatusCode::UNAUTHORIZED if using_user_token => {
                    token = self.twitch_app_token().await.ok()?;
                    using_user_token = false;
                    continue;
                }
                _ => return None,
            }
        }
    }

    /// A YouTube channel's upcoming/scheduled livestreams, scraped from its
    /// `/streams` page (no credentials, no quota). `Some(vec)` on a successful
    /// fetch (possibly empty); `None` on a network/HTTP error.
    pub async fn youtube_schedule(&self, url: &str) -> Option<Vec<ScheduleSegment>> {
        let jitter_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 2000) as u64;
        tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        let streams_url = youtube_streams_url(url);
        let rb = self
            .http
            .get(&streams_url)
            // Force English + US locale. YouTube geo-localizes the server-rendered
            // schedule strings to the viewer's IP, so a non-US IP yields e.g. the
            // Norwegian "Planlagt for 18.06.2026, 20:00" instead of the
            // "Scheduled for 6/18/26, 8:00 PM" that `parse_yt_scheduled_text`
            // expects — every segment was silently dropped. The `hl`/`gl` query
            // params override that more reliably than `Accept-Language` alone.
            .query(&[("hl", "en"), ("gl", "US")])
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI");
        let resp = self.fingerprint.apply_yt_nav_headers(rb).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        let mut segs = parse_youtube_schedule(&body);
        // Shape-independent fallback: if the structured `ytInitialData` walk found
        // nothing (Polymer markup drift), scan the raw page for scheduled-start
        // markers so a layout change doesn't silently zero out the schedule.
        if segs.is_empty() {
            collect_upcoming_fallback(&body, &mut segs);
        }
        Some(segs)
    }

    /// A YouTube channel's upcoming streams via the Data API (source `YouTubeApi`):
    /// resolve the channel id, `search.list?eventType=upcoming` for the upcoming
    /// video ids (~100 quota units), then `videos.list` for each one's exact
    /// `scheduledStartTime` + title (~1 unit). `Some(vec)` on success (possibly
    /// empty); `None` on error / missing key, so a transient failure won't wipe a
    /// stored schedule. Gated by the caller on `youtube_api_enabled(K_YT_API_SCHEDULE)`.
    pub async fn youtube_schedule_api(&self, url: &str) -> Option<Vec<ScheduleSegment>> {
        let key = self
            .store
            .get_setting("youtube_api_key")
            .ok()
            .flatten()
            .unwrap_or_default();
        if key.is_empty() {
            return None;
        }
        if self.youtube_quota_exceeded() {
            debug!("YouTube schedule API skipped — daily quota limit reached");
            return None;
        }
        let channel_id = self.youtube_channel_id(url, &key).await.ok()?;
        // search.list for upcoming live broadcasts on this channel.
        let resp = self
            .http
            .get("https://www.googleapis.com/youtube/v3/search")
            .query(&[
                ("part", "id"),
                ("channelId", channel_id.as_str()),
                ("eventType", "upcoming"),
                ("type", "video"),
                ("maxResults", "50"),
                ("key", key.as_str()),
            ])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let _ = self.store.record_quota_usage("youtube", 100);
        let _ = self.store.record_quota_usage("youtube_search", 1);
        let v: Value = resp.json().await.ok()?;
        let ids: Vec<String> = v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|it| it["id"]["videoId"].as_str().map(str::to_string))
            .collect();
        if ids.is_empty() {
            // The API definitively reports no upcoming streams.
            return Some(Vec::new());
        }
        // videos.list for exact scheduled start + title (one batched call).
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let starts = self.youtube_videos_list(&id_refs).await?;
        let titles = self.youtube_video_titles(&id_refs, &key).await.unwrap_or_default();
        let mut out: Vec<ScheduleSegment> = ids
            .iter()
            .filter_map(|id| {
                let start = *starts.get(id)?;
                Some(ScheduleSegment {
                    id: 0,
                    monitor_id: 0,
                    start_time: start,
                    end_time: None,
                    title: titles.get(id).cloned().unwrap_or_default(),
                    category: String::new(),
                    canceled: false,
                    video_id: Some(id.clone()),
                    collab: String::new(),
                })
            })
            .collect();
        out.sort_by(|a, b| a.start_time.cmp(&b.start_time).then_with(|| a.title.cmp(&b.title)));
        out.dedup_by(|a, b| a.start_time == b.start_time && a.title == b.title);
        Some(out)
    }

    /// Batch `videos.list?part=snippet` to fetch each video's title. Returns a map
    /// of `video_id → title` (~1 quota unit per 50 ids). `None` on error.
    async fn youtube_video_titles(
        &self,
        video_ids: &[&str],
        key: &str,
    ) -> Option<HashMap<String, String>> {
        if key.is_empty() || video_ids.is_empty() {
            return None;
        }
        let mut result: HashMap<String, String> = HashMap::new();
        for chunk in video_ids.chunks(50) {
            let ids_str = chunk.join(",");
            let resp = self
                .http
                .get("https://www.googleapis.com/youtube/v3/videos")
                .query(&[("part", "snippet"), ("id", ids_str.as_str()), ("key", key)])
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let _ = self.store.record_quota_usage("youtube", 1);
            let v: Value = resp.json().await.ok()?;
            for item in v["items"].as_array().into_iter().flatten() {
                if let (Some(id), Some(title)) =
                    (item["id"].as_str(), item["snippet"]["title"].as_str())
                {
                    result.insert(id.to_string(), title.to_string());
                }
            }
        }
        Some(result)
    }

    // ----- Multi-source schedule resolution -----

    /// Resolve a single schedule source for one monitor. `Some(v)` = the source
    /// produced an authoritative answer (possibly empty, meaning "definitively
    /// nothing scheduled"); `None` = a transient failure / not-configured, so the
    /// caller leaves any stored rows untouched. `Discord` is resolved by the batch
    /// sweep in [`refresh_schedules_once`], not here, so it returns `None`.
    pub async fn resolve_source(
        &self,
        kind: ScheduleSourceKind,
        row: &MonitorWithChannel,
        cfg: &ChannelSourceConfig,
    ) -> Option<Vec<ScheduleSegment>> {
        let url = row.monitor.url.as_str();
        match kind {
            ScheduleSourceKind::TwitchSchedule => self.twitch_schedule(url).await,
            ScheduleSourceKind::YouTubeScrape => self.youtube_schedule(url).await,
            ScheduleSourceKind::YouTubeApi => self.youtube_schedule_api(url).await,
            ScheduleSourceKind::TwitchBannerOcr => self.ocr_twitch_banner(row, cfg).await,
            ScheduleSourceKind::YouTubeCommunityOcr => self.ocr_youtube_community(row, cfg).await,
            ScheduleSourceKind::TwitterPinned => self.ocr_twitter_pinned(row, cfg).await,
            ScheduleSourceKind::OtherImageOcr => self.ocr_other_image(row, cfg).await,
            ScheduleSourceKind::Discord => None,
        }
    }

    /// OCR an on-disk image into schedule segments, skipping the (multi-second,
    /// token-spending) CLI call when the image bytes are unchanged since the last
    /// pass for this `(monitor, source)`. Returns the cached result on a hash hit.
    async fn ocr_image_cached(
        &self,
        monitor_id: i64,
        source_id: &str,
        channel_name: &str,
        path: &Path,
        cfg: &ChannelSourceConfig,
    ) -> Option<Vec<ScheduleSegment>> {
        let bytes = crate::iomon::fs::read(crate::iomon::Cat::Detector, path).await.ok()?;
        let hash = fnv64(&bytes);
        let key = (monitor_id, source_id.to_string());
        if let Some((cached, segs)) = self.ocr_cache.lock().await.get(&key) {
            if *cached == hash {
                debug!(
                    "OCR skipped — image unchanged, reusing in-memory cached result \
                     (monitor {monitor_id}, source {source_id}; no API call, no files touched)"
                );
                record_ocr_cache_hit(self.store.as_ref());
                return Some(segs.clone());
            }
        }
        let opts = ocr_opts_from_settings(self.store.as_ref(), cfg);
        info!(
            "OCR: scheduling claude call (monitor {monitor_id}, source {source_id}, image {})",
            path.display()
        );

        let source_label = ScheduleSourceKind::from_id(source_id)
            .map(|k| k.label())
            .unwrap_or(source_id);
        let detail = format!("{source_label} · {}", opts.model);
        let result = self
            .ocr_one_with_events(channel_name, detail, path, &opts)
            .await;

        let segs = result.segments?;
        self.ocr_cache.lock().await.insert(key.clone(), (hash, segs.clone()));
        // Persist the hash so this cache hit survives an app restart.
        self.persist_ocr_hash(key.0, &key.1, hash);
        Some(segs)
    }

    /// Run one OCR call on `path`, emitting the background-task start/finish events
    /// and accumulating CLI stats. `detail` is the task's detail line. Returns the
    /// raw run result so the caller decides how to cache/persist it — shared by the
    /// single-image cache path and the multi-post community walk.
    async fn ocr_one_with_events(
        &self,
        channel_name: &str,
        detail: String,
        path: &Path,
        opts: &crate::schedule_ocr::OcrOpts,
    ) -> crate::schedule_ocr::OcrRunResult {
        let task_id = crate::events::next_task_id();
        let _ = self.events.send(crate::events::AppEvent::BackgroundTaskStarted(
            crate::events::BackgroundTask {
                id: task_id,
                kind: crate::events::BackgroundTaskKind::OcrCall,
                label: channel_name.to_string(),
                detail,
                started_at: now_unix(),
                progress: None,
                progress_info: None,
            },
        ));

        let result = ocr_schedule_image(path, opts).await;
        accumulate_ocr_stats(self.store.as_ref(), &result);

        let outcome = match &result.segments {
            Some(segs) => {
                let n = segs.len();
                let note = if n == 1 {
                    "1 event decoded".to_string()
                } else if n > 1 {
                    format!("{n} events decoded")
                } else {
                    // Parsed OK but the model found nothing — likely a non-schedule image
                    // or all cards had vague/null times.
                    "0 events (nothing found)".to_string()
                };
                crate::events::TaskOutcome::CompletedWithNote(note)
            }
            None => {
                if result.cli_failures > 0 && result.cli_calls.is_empty() {
                    crate::events::TaskOutcome::Failed(format!(
                        "CLI failed — is '{}' on PATH?",
                        opts.command
                    ))
                } else {
                    crate::events::TaskOutcome::Failed("Parse failed".into())
                }
            }
        };
        let _ = self
            .events
            .send(crate::events::AppEvent::BackgroundTaskFinished { id: task_id, outcome });
        result
    }

    /// Destination for a downloaded schedule-source image, kept in a `schedule_src/`
    /// subdir of the channel asset dir so it never collides with archival assets.
    fn schedule_src_path(&self, row: &MonitorWithChannel, stem: &str, ext: &str) -> PathBuf {
        let platform = row.monitor.platform();
        let account = crate::assets::account_slug(&row.monitor.url, platform);
        crate::assets::channel_asset_dir(&row.channel.name, platform, &account)
            .join("schedule_src")
            .join(format!("{stem}.{ext}"))
    }

    /// OCR the channel's already-downloaded Twitch offline banner (`banner.<ext>`
    /// in the asset dir — fetched by the asset pipeline, no re-fetch here).
    async fn ocr_twitch_banner(
        &self,
        row: &MonitorWithChannel,
        cfg: &ChannelSourceConfig,
    ) -> Option<Vec<ScheduleSegment>> {
        // The row may be a non-Twitch instance of the container; and even a
        // Twitch row's banner may live under a sibling account — search the
        // row's own account first, then any account, then the legacy dir.
        let banner = if row.monitor.platform() == Platform::Twitch {
            let account = crate::assets::account_slug(&row.monitor.url, Platform::Twitch);
            let dir = crate::assets::channel_asset_dir(&row.channel.name, Platform::Twitch, &account);
            crate::assets::find_asset(&dir, "banner.").or_else(|| {
                crate::assets::find_asset_any_account(&row.channel.name, Platform::Twitch, "banner.")
            })
        } else {
            crate::assets::find_asset_any_account(&row.channel.name, Platform::Twitch, "banner.")
        }?;
        self.ocr_image_cached(
            row.monitor.id,
            ScheduleSourceKind::TwitchBannerOcr.id(),
            &row.channel.name,
            &banner,
            cfg,
        )
        .await
    }

    /// OCR a user-supplied schedule image (`cfg.other_image`): a local path is read
    /// directly; a URL is downloaded first. Returns `None` when unset.
    async fn ocr_other_image(
        &self,
        row: &MonitorWithChannel,
        cfg: &ChannelSourceConfig,
    ) -> Option<Vec<ScheduleSegment>> {
        let src = cfg.other_image.trim();
        if src.is_empty() {
            return None;
        }
        let path = if is_url(src) {
            let dest = self.schedule_src_path(row, "other", url_image_ext(src));
            crate::assets::download_image(&self.http, src, &dest).await.ok()?;
            dest
        } else {
            PathBuf::from(src)
        };
        self.ocr_image_cached(
            row.monitor.id,
            ScheduleSourceKind::OtherImageOcr.id(),
            &row.channel.name,
            &path,
            cfg,
        )
        .await
    }

    /// OCR images on the channel's recent YouTube community posts. Scans up to the
    /// configured backlog depth ([`community_max_posts`]) in order, returning the
    /// first that decodes a schedule.
    ///
    /// Two layers of caching keep this cheap. An in-memory combined-URL hash of the
    /// whole post set short-circuits the entire pass when nothing changed (no
    /// downloads, no OCR). When the set *has* changed (typically one new post
    /// pushing the others down the feed), every pulled image is archived to a
    /// content-addressed file + `community_post_archive` row; an image whose bytes
    /// match an already-decoded archive entry reuses that result instead of
    /// re-OCR'ing — so only genuinely new images spend tokens.
    async fn ocr_youtube_community(
        &self,
        row: &MonitorWithChannel,
        cfg: &ChannelSourceConfig,
    ) -> Option<Vec<ScheduleSegment>> {
        let max_posts = community_max_posts(self.store.as_ref(), cfg);

        let community_url = youtube_community_url(&row.monitor.url);
        let jitter_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 2000) as u64;
        tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        let rb = self
            .http
            .get(&community_url)
            .query(&[("hl", "en"), ("gl", "US")])
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI");
        let resp = self.fingerprint.apply_yt_nav_headers(rb).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        let data = extract_json_after(&body, "ytInitialData")?;

        let mut imgs: Vec<String> = Vec::new();
        community_images(&data, &mut imgs);
        imgs.truncate(max_posts);
        if imgs.is_empty() {
            return None;
        }

        // Combined URL hash: stable identifier for the current set of posts.
        // If no post URL has changed the hash matches and we skip the whole pass.
        let url_bytes: Vec<u8> = imgs
            .iter()
            .flat_map(|u| u.as_bytes().iter().copied())
            .collect();
        let combined_hash = fnv64(&url_bytes);
        let source_id = ScheduleSourceKind::YouTubeCommunityOcr.id();
        let cache_key = (row.monitor.id, source_id.to_string());
        {
            let guard = self.ocr_cache.lock().await;
            if let Some((cached, segs)) = guard.get(&cache_key) {
                if *cached == combined_hash {
                    debug!(
                        "OCR skipped — {} community post image(s) unchanged, reusing in-memory \
                         cached result (monitor {}; no API call, no files touched)",
                        imgs.len(),
                        row.monitor.id,
                    );
                    record_ocr_cache_hit(self.store.as_ref());
                    return Some(segs.clone());
                }
            }
        }

        // Set changed: pull + archive every post image (durable record), then OCR
        // in feed order until one decodes a schedule. Unchanged images hit the
        // per-image archive cache below and skip OCR.
        let now = crate::models::now_unix();
        let mut archived: Vec<(String, PathBuf)> = Vec::new();
        for img_url in &imgs {
            if let Some(entry) = self.archive_community_image(row, img_url, now).await {
                archived.push(entry);
            }
        }
        if archived.is_empty() {
            return None;
        }

        let opts = ocr_opts_from_settings(self.store.as_ref(), cfg);
        let n = archived.len();
        let mut winner: Option<Vec<ScheduleSegment>> = None;
        for (i, (content_hash, path)) in archived.iter().enumerate() {
            // Per-image archive cache: this exact image already OCR'd?
            if let Ok(Some(ap)) = self.store.community_post_get(row.monitor.id, content_hash) {
                if ap.ocr_attempted {
                    record_ocr_cache_hit(self.store.as_ref());
                    let segs: Vec<ScheduleSegment> =
                        serde_json::from_str(&ap.decoded_json).unwrap_or_default();
                    if !segs.is_empty() {
                        winner = Some(segs);
                        break;
                    }
                    continue;
                }
            }

            let detail = if n > 1 {
                format!(
                    "{} · {} (post {}/{})",
                    ScheduleSourceKind::YouTubeCommunityOcr.label(),
                    opts.model,
                    i + 1,
                    n
                )
            } else {
                format!(
                    "{} · {}",
                    ScheduleSourceKind::YouTubeCommunityOcr.label(),
                    opts.model
                )
            };
            let result = self
                .ocr_one_with_events(&row.channel.name, detail, path, &opts)
                .await;

            // Persist the decode (empty included) so an unchanged image is never
            // re-OCR'd. `None` (CLI/parse failure) is left un-attempted to retry.
            if let Some(segs) = result.segments {
                let json = serde_json::to_string(&segs).unwrap_or_else(|_| "[]".to_string());
                self.store
                    .community_post_set_decoded(
                        row.monitor.id,
                        content_hash,
                        segs.len() as i64,
                        &json,
                    )
                    .ok();
                if !segs.is_empty() {
                    winner = Some(segs);
                    break;
                }
            }
        }

        // Cache the pass result (winner, or empty = "nothing found this set") under
        // the combined hash so an unchanged feed skips everything next pass.
        let segs = winner.unwrap_or_default();
        self.ocr_cache
            .lock()
            .await
            .insert(cache_key.clone(), (combined_hash, segs.clone()));
        self.persist_ocr_hash(cache_key.0, source_id, combined_hash);
        Some(segs)
    }

    /// Download a community-post image, content-hash its bytes, persist it to a
    /// content-addressed path under `schedule_src/`, and upsert its
    /// `community_post_archive` row. Returns `(content_hash, local_path)` for the
    /// OCR step, or `None` on a download/read failure. Idempotent: re-seeing the
    /// same image reuses the on-disk file and just refreshes the archive row.
    async fn archive_community_image(
        &self,
        row: &MonitorWithChannel,
        img_url: &str,
        fetched_at: i64,
    ) -> Option<(String, PathBuf)> {
        let ext = url_image_ext(img_url);
        // Download to a temp path first so we can hash the bytes before naming.
        let tmp = self.schedule_src_path(row, "community_tmp", ext);
        crate::assets::download_image(&self.http, img_url, &tmp)
            .await
            .ok()?;
        use crate::iomon::Cat;
        let bytes = crate::iomon::fs::read(Cat::Detector, &tmp).await.ok()?;
        let content_hash = fnv64(&bytes).to_string();

        // Content-addressed final path: every distinct image kept (durable archive).
        let dest = self.schedule_src_path(row, &format!("community_{content_hash}"), ext);
        if crate::iomon::fs::try_exists(Cat::Detector, &dest).await.unwrap_or(false) {
            // Already archived (identical bytes) — drop the temp, keep the original.
            let _ = crate::iomon::fs::remove_file(Cat::Detector, &tmp).await;
        } else if crate::iomon::fs::rename(Cat::Detector, &tmp, &dest).await.is_err() {
            // Rename failed (e.g. cross-device) — fall back to a copy.
            let _ = crate::iomon::fs::write(Cat::Detector, &dest, &bytes).await;
            let _ = crate::iomon::fs::remove_file(Cat::Detector, &tmp).await;
        }

        self.store
            .community_post_upsert(
                row.monitor.id,
                ScheduleSourceKind::YouTubeCommunityOcr.id(),
                img_url,
                &content_hash,
                &dest.to_string_lossy(),
                fetched_at,
            )
            .ok();
        Some((content_hash, dest))
    }

    /// Download a community-post attachment into the channel's `posts/` asset
    /// subdir, content-addressed by the fnv64 of the bytes (identical images are
    /// stored once). Returns `(content_hash, local_path)` or `None` on failure.
    async fn download_post_image(
        &self,
        row: &MonitorWithChannel,
        img_url: &str,
    ) -> Option<(String, PathBuf)> {
        let ext = url_image_ext(img_url);
        let platform = row.monitor.platform();
        let account = crate::assets::account_slug(&row.monitor.url, platform);
        let posts_dir =
            crate::assets::channel_asset_dir(&row.channel.name, platform, &account).join("posts");
        let tmp = posts_dir.join(format!("tmp.{ext}"));
        crate::assets::download_image(&self.http, img_url, &tmp).await.ok()?;
        use crate::iomon::Cat;
        let bytes = crate::iomon::fs::read(Cat::Detector, &tmp).await.ok()?;
        let content_hash = fnv64(&bytes).to_string();
        let dest = posts_dir.join(format!("{content_hash}.{ext}"));
        if crate::iomon::fs::try_exists(Cat::Detector, &dest).await.unwrap_or(false) {
            let _ = crate::iomon::fs::remove_file(Cat::Detector, &tmp).await;
        } else if crate::iomon::fs::rename(Cat::Detector, &tmp, &dest).await.is_err() {
            let _ = crate::iomon::fs::write(Cat::Detector, &dest, &bytes).await;
            let _ = crate::iomon::fs::remove_file(Cat::Detector, &tmp).await;
        }
        Some((content_hash, dest))
    }

    /// Fetch + parse the channel's community posts, upsert full post rows +
    /// attachments (images downloaded into `posts/`), and emit a `youtube_post`
    /// notification for each newly-seen post. Best-effort: a fetch/parse miss is
    /// a no-op (never destructive).
    ///
    /// Beyond the first page, older posts are archived via InnerTube browse
    /// continuations: a one-time full-history backfill per monitor (paced like a
    /// person scrolling, no notifications, resumable across restarts via
    /// `community_post_backfill`), and afterwards a bounded gap-fill walk
    /// whenever an entire first page turns out to be unseen (more than a page of
    /// posts landed between rounds).
    async fn fetch_channel_posts(
        &self,
        row: &MonitorWithChannel,
        events: &EventTx,
        shutdown: &Arc<AtomicBool>,
    ) {
        const MAX_POSTS: usize = 20;
        /// Full-history walk cap (~10 posts/page → ~4000 posts) — a runaway
        /// guard, not an expected limit.
        const BACKFILL_MAX_PAGES: i64 = 400;
        /// Gap-fill walk cap once the backfill is complete.
        const GAPFILL_MAX_PAGES: i64 = 40;
        let community_url = youtube_community_url(&row.monitor.url);
        let jitter_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 2000) as u64;
        tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        let rb = self
            .http
            .get(&community_url)
            .query(&[("hl", "en"), ("gl", "US")])
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI");
        let Ok(resp) = self.fingerprint.apply_yt_nav_headers(rb).send().await else {
            return;
        };
        if !resp.status().is_success() {
            return;
        }
        let Ok(body) = resp.text().await else { return };
        let Some(data) = extract_json_after(&body, "ytInitialData") else {
            return;
        };

        let mut posts = Vec::new();
        parse_community_posts(&data, &mut posts);
        posts.truncate(MAX_POSTS);

        // The page owner's `UC…` id (from the canonical `/channel/` link) tells
        // the channel's own posts apart from viewer posts. Reused for the whole
        // continuation walk — continuation JSON carries no page HTML.
        let owner_id = crate::websub::find_uc(&body).unwrap_or_default();

        // Gap-fill heuristics track CHANNEL posts only: viewer posts (fans
        // commenting) churn constantly and must not trigger a deep walk every
        // round.
        let mut channel_on_page = 0usize;
        let mut channel_new = 0usize;
        for p in &posts {
            let kind = classify_author_kind(p, &owner_id);
            let is_new = self.ingest_post(row, p, kind, true, events).await;
            if kind == "channel" {
                channel_on_page += 1;
                if is_new {
                    channel_new += 1;
                }
            }
        }

        // ── Older pages (continuation walk) ──
        let backfill_done = self.store.posts_backfill_done(row.monitor.id).unwrap_or(true);
        let gap_suspected = channel_on_page > 0 && channel_new == channel_on_page;
        if backfill_done && !gap_suspected {
            return;
        }
        let Some(mut token) = find_continuation_token(&data) else {
            // No continuation on the first page. If the page did show posts,
            // the whole feed fits on one page and the history is trivially
            // archived. A ZERO-post page is left unrecorded: it could equally
            // be a channel without a community tab or an interstitial that
            // parsed empty, so re-check next round rather than lock in a
            // completion that would block a real walk later.
            if !backfill_done && !posts.is_empty() {
                let _ = self.store.posts_backfill_record(row.monitor.id, true, 0, 0);
                tracing::info!(
                    channel = %row.channel.name,
                    posts = posts.len(),
                    "community posts fit on one page; backfill complete"
                );
            } else if posts.is_empty() {
                tracing::debug!(
                    channel = %row.channel.name,
                    url = %community_url,
                    "community posts page parsed empty (no posts, no continuation) — will retry next round"
                );
            }
            return;
        };
        let (Some(api_key), Some(client_version)) = (
            ytcfg_str(&body, "INNERTUBE_API_KEY"),
            ytcfg_str(&body, "INNERTUBE_CONTEXT_CLIENT_VERSION"),
        ) else {
            return;
        };

        if backfill_done {
            tracing::info!(
                channel = %row.channel.name,
                new_posts = channel_new,
                "entire first page of channel posts was unseen; gap-filling older pages"
            );
        } else {
            tracing::info!(
                channel = %row.channel.name,
                "starting full-history community posts backfill"
            );
        }
        let max_pages = if backfill_done { GAPFILL_MAX_PAGES } else { BACKFILL_MAX_PAGES };
        let mut pages = 0i64;
        let mut walked = 0i64;
        let mut completed = false;
        while pages < max_pages {
            if shutdown.load(Ordering::SeqCst) {
                break; // partial session is recorded below; resumes next round
            }
            // Human-ish scroll pacing between continuation requests.
            tokio::time::sleep(Duration::from_millis(2500 + spread_rand_u64() % 4500)).await;
            let Some(resp) = self
                .fetch_posts_continuation(&community_url, &api_key, &client_version, &token)
                .await
            else {
                break;
            };
            let mut older = Vec::new();
            parse_community_posts(&resp, &mut older);
            pages += 1;
            walked += older.len() as i64;
            let mut channel_new_here = 0usize;
            for p in &older {
                // No notifications for walked pages: these are old posts being
                // archived, not fresh activity.
                let kind = classify_author_kind(p, &owner_id);
                let is_new = self.ingest_post(row, p, kind, false, events).await;
                if kind == "channel" && is_new {
                    channel_new_here += 1;
                }
            }
            match find_continuation_token(&resp) {
                Some(next) if next != token && !older.is_empty() => token = next,
                _ => {
                    completed = true; // end of the feed
                    break;
                }
            }
            if backfill_done && channel_new_here == 0 {
                completed = true; // gap-fill reached already-archived channel posts
                break;
            }
        }
        if !backfill_done {
            let _ = self
                .store
                .posts_backfill_record(row.monitor.id, completed, pages, walked);
            if completed {
                tracing::info!(
                    channel = %row.channel.name,
                    pages,
                    posts = walked,
                    "community posts backfill complete"
                );
            } else {
                tracing::info!(
                    channel = %row.channel.name,
                    pages,
                    posts = walked,
                    "community posts backfill paused; resuming next round"
                );
            }
        }
    }

    /// Upsert one parsed community post (row + cached images) and return whether
    /// it was newly seen. `notify == true` additionally emits the `youtube_post`
    /// notification for a new **channel** post — the backfill/gap walks pass
    /// `false` so archiving old posts never spams the feed, and viewer posts
    /// never notify regardless. `author_kind` (`channel`/`viewer`) is persisted
    /// on the row so the feed can hide viewer posts.
    async fn ingest_post(
        &self,
        row: &MonitorWithChannel,
        p: &ParsedPost,
        author_kind: &str,
        notify: bool,
        events: &EventTx,
    ) -> bool {
        // One builder for both the initial upsert and the post-icon refresh
        // (they differ only in `author_icon`).
        let mk = |author_icon: String| crate::store::NewCommunityPost {
            monitor_id: row.monitor.id,
            channel_id: row.channel.id,
            post_id: p.post_id.clone(),
            author: p.author.clone(),
            author_icon,
            published_text: p.published_text.clone(),
            body_text: p.body_text.clone(),
            links_json: p.links_json.clone(),
            vote_count: p.vote_count.clone(),
            shared_json: p.shared_json.clone(),
            raw_json: p.raw_json.clone(),
            author_kind: author_kind.to_string(),
            author_channel_id: p.author_channel_id.clone(),
            ..Default::default()
        };
        let (pk, is_new) = match self.store.community_post_upsert_full(&mk(String::new())) {
            Ok(v) => v,
            Err(_) => return false,
        };

        // Download the images/avatar only for a newly-seen post (or one whose
        // media somehow wasn't cached yet). `download_post_image` always
        // re-fetches the bytes to hash them, so re-downloading an existing
        // post's images on every 6 h re-scan would be pure wasted traffic —
        // gate it so steady-state re-scans make no image requests at all.
        let need_media =
            is_new || self.store.community_post_media_count(pk).unwrap_or(0) == 0;
        let mut first_image: Option<String> = None;
        if need_media {
            // Cache the author icon (best-effort) and record it on the row.
            if !p.author_icon_url.is_empty()
                && let Some((_, path)) = self.download_post_image(row, &p.author_icon_url).await
            {
                let _ = self
                    .store
                    .community_post_upsert_full(&mk(path.to_string_lossy().into_owned()));
            }

            // Download + record each attachment image, in order. A small
            // randomized delay between images makes an image-heavy post's
            // first fetch look like a page loading its images progressively
            // rather than a scripted download loop.
            for (ordinal, url) in p.image_urls.iter().enumerate() {
                if let Some((hash, path)) = self.download_post_image(row, url).await {
                    let local = path.to_string_lossy().into_owned();
                    if first_image.is_none() {
                        first_image = Some(local.clone());
                    }
                    let _ = self.store.community_post_media_upsert(
                        pk,
                        ordinal as i64,
                        "image",
                        url,
                        &hash,
                        &local,
                    );
                    tokio::time::sleep(Duration::from_millis(150 + spread_rand_u64() % 500))
                        .await;
                }
            }

            // A reshare's quoted-original images, continuing the ordinal
            // sequence and tagged `shared_image` so the feed renders them inside
            // the quote card rather than as the post's own attachments.
            let base = p.image_urls.len();
            for (i, url) in p.shared_image_urls.iter().enumerate() {
                if let Some((hash, path)) = self.download_post_image(row, url).await {
                    let local = path.to_string_lossy().into_owned();
                    let _ = self.store.community_post_media_upsert(
                        pk,
                        (base + i) as i64,
                        "shared_image",
                        url,
                        &hash,
                        &local,
                    );
                    tokio::time::sleep(Duration::from_millis(150 + spread_rand_u64() % 500))
                        .await;
                }
            }
        }

        // A newly-seen CHANNEL post → one notification (fresh activity only; the
        // backfill/gap walks archive silently, and viewer posts never notify).
        if is_new && notify && author_kind == "channel" {
            let snippet: String = p.body_text.chars().take(140).collect();
            let _ = self.store.insert_notification(&crate::store::NewNotification {
                kind: NotificationKind::YoutubePost.id().to_string(),
                severity: "info".to_string(),
                title: format!("{} posted", row.channel.name),
                body: snippet,
                monitor_id: Some(row.monitor.id),
                channel: row.channel.name.clone(),
                action_label: "Open post".to_string(),
                action_url: format!("https://www.youtube.com/post/{}", p.post_id),
                image_path: first_image.unwrap_or_default(),
                ref_key: format!("post:{}:{}", row.monitor.id, p.post_id),
                ..Default::default()
            });
        }
        if is_new {
            // Nudge the UI to reload the posts feed if it's open.
            let _ = events.send(AppEvent::MonitorState {
                monitor_id: row.monitor.id,
                state: row.monitor.last_state.clone(),
            });
        }
        is_new
    }

    /// POST one InnerTube `browse` continuation for the community feed and
    /// return the parsed response (the next page's posts + follow-up token live
    /// in `onResponseReceivedEndpoints[].appendContinuationItemsAction`). The
    /// key/version come from the page's own inline `ytcfg`, so the request
    /// matches what the page's JS would send; the shared client already carries
    /// the fingerprint user-agent.
    async fn fetch_posts_continuation(
        &self,
        referer: &str,
        api_key: &str,
        client_version: &str,
        token: &str,
    ) -> Option<Value> {
        let body = serde_json::json!({
            "context": { "client": {
                "clientName": "WEB",
                "clientVersion": client_version,
                "hl": "en",
                "gl": "US",
            }},
            "continuation": token,
        });
        let url =
            format!("https://www.youtube.com/youtubei/v1/browse?key={api_key}&prettyPrint=false");
        let resp = self
            .http
            .post(&url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI")
            .header("Origin", "https://www.youtube.com")
            .header("Referer", referer)
            .header("X-Youtube-Client-Name", "1")
            .header("X-Youtube-Client-Version", client_version)
            .json(&body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    /// OCR the image on the channel's pinned tweet. Best-effort: hits the public
    /// syndication timeline endpoint (no auth), grabs the first `pbs.twimg.com`
    /// media image (the pinned tweet renders first), downloads it, then OCRs. X
    /// actively fights this, so any miss returns `None` and falls through.
    async fn ocr_twitter_pinned(
        &self,
        row: &MonitorWithChannel,
        cfg: &ChannelSourceConfig,
    ) -> Option<Vec<ScheduleSegment>> {
        let handle = cfg.twitter_handle.trim().trim_start_matches('@');
        if handle.is_empty() {
            return None;
        }
        let url =
            format!("https://syndication.twitter.com/srv/timeline-profile/screen-name/{handle}");
        let resp = self
            .http
            .get(&url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        let img = first_twimg_media(&body)?;
        let dest = self.schedule_src_path(row, "twitter", url_image_ext(&img));
        crate::assets::download_image(&self.http, &img, &dest).await.ok()?;
        self.ocr_image_cached(
            row.monitor.id,
            ScheduleSourceKind::TwitterPinned.id(),
            &row.channel.name,
            &dest,
            cfg,
        )
        .await
    }

    // ----- YouTube Data API (API key) -----

    /// Whether a given YouTube operation should use the Data API instead of
    /// scraping: its per-operation setting (`setting_key`) is on AND an API key is
    /// configured.
    pub fn youtube_api_enabled(&self, setting_key: &str) -> bool {
        let on = self
            .store
            .get_setting(setting_key)
            .ok()
            .flatten()
            .as_deref()
            == Some("1");
        on && !self
            .store
            .get_setting("youtube_api_key")
            .ok()
            .flatten()
            .unwrap_or_default()
            .is_empty()
    }

    /// Whether Discord schedule import is on: the toggle is set AND a token exists.
    pub fn discord_enabled(&self) -> bool {
        let on = self
            .store
            .get_setting(K_DISCORD_SCHEDULE)
            .ok()
            .flatten()
            .as_deref()
            == Some("1");
        on && !self
            .store
            .get_setting(K_DISCORD_TOKEN)
            .ok()
            .flatten()
            .unwrap_or_default()
            .is_empty()
    }

    /// GET a Discord API endpoint with the user token, parsing JSON. Handles one
    /// 429 (rate-limit) retry honoring `retry-after`. `None` on auth/HTTP error.
    /// The token is sent raw in the Authorization header (Discord user-token form).
    async fn discord_get(&self, token: &str, url: &str) -> Option<Value> {
        for attempt in 0..2 {
            let resp = self
                .http
                .get(url)
                .header("Authorization", token)
                .send()
                .await
                .ok()?;
            match resp.status() {
                s if s.is_success() => return resp.json::<Value>().await.ok(),
                reqwest::StatusCode::TOO_MANY_REQUESTS if attempt == 0 => {
                    let wait = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(2.0)
                        .clamp(0.0, 10.0);
                    tokio::time::sleep(Duration::from_millis((wait * 1000.0) as u64 + 250)).await;
                }
                _ => return None,
            }
        }
        None
    }

    /// The ids of the guilds (servers) the token's user is a member of.
    async fn discord_guild_ids(&self, token: &str) -> Option<Vec<String>> {
        let v = self
            .discord_get(token, "https://discord.com/api/v10/users/@me/guilds?limit=200")
            .await?;
        Some(
            v.as_array()?
                .iter()
                .filter_map(|g| g["id"].as_str().map(String::from))
                .collect(),
        )
    }

    /// One guild's upcoming scheduled events as [`DiscordEvt`] (with matchable text).
    async fn discord_guild_events(&self, token: &str, guild_id: &str) -> Option<Vec<DiscordEvt>> {
        let url = format!("https://discord.com/api/v10/guilds/{guild_id}/scheduled-events");
        let v = self.discord_get(token, &url).await?;
        let mut out = Vec::new();
        for e in v.as_array()? {
            let Some(start) = e["scheduled_start_time"].as_str().and_then(parse_rfc3339) else {
                continue;
            };
            // 1 SCHEDULED, 2 ACTIVE, 3 COMPLETED, 4 CANCELED.
            let status = e["status"].as_i64().unwrap_or(1);
            if status == 3 {
                continue;
            }
            let name = e["name"].as_str().unwrap_or("").to_string();
            let desc = e["description"].as_str().unwrap_or("");
            let loc = e["entity_metadata"]["location"].as_str().unwrap_or("");
            out.push(DiscordEvt {
                start_time: start,
                end_time: e["scheduled_end_time"].as_str().and_then(parse_rfc3339),
                title: name.clone(),
                canceled: status == 4,
                text: format!("{name}\n{desc}\n{loc}").to_lowercase(),
            });
        }
        Some(out)
    }

    /// Sweep the user's Discord guilds for scheduled events and match them to
    /// monitors by the stream URL appearing in the event (location/description/
    /// name). Returns `(monitor_id -> events, complete)` where `complete` is true
    /// only if every guild was fetched successfully — the caller reconciles
    /// (clears unmatched) only on a complete sweep, so a partial outage never wipes
    /// a streamer whose guild happened to fail. `None` if the guild list itself
    /// couldn't be fetched (auth/network). Paced + shutdown-aware.
    async fn discord_sweep(
        &self,
        rows: &[MonitorWithChannel],
        shutdown: &Arc<AtomicBool>,
    ) -> Option<(HashMap<i64, Vec<ScheduleSegment>>, bool)> {
        let token = self
            .store
            .get_setting(K_DISCORD_TOKEN)
            .ok()
            .flatten()
            .unwrap_or_default();
        if token.is_empty() {
            return None;
        }
        let guilds = self.discord_guild_ids(&token).await?;
        // URL needles per active monitor (skip dormant ones + those we can't
        // match by URL). Auto-record (`enabled`) does NOT gate schedule scraping
        // — only the master automation switch does.
        let needles: Vec<(i64, Vec<String>)> = rows
            .iter()
            .filter(|r| r.automation_on())
            .map(|r| (r.monitor.id, monitor_needles(&r.monitor.url)))
            .filter(|(_, n)| !n.is_empty())
            .collect();
        if needles.is_empty() {
            return Some((HashMap::new(), true));
        }
        let mut matched: HashMap<i64, Vec<ScheduleSegment>> = HashMap::new();
        let guild_count = guilds.len();
        let mut ok_guilds = 0usize;
        for gid in guilds {
            if shutdown.load(Ordering::SeqCst) {
                return None;
            }
            // Gentle pacing so the sweep isn't a bursty rate-limit magnet.
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Some(events) = self.discord_guild_events(&token, &gid).await else {
                continue;
            };
            ok_guilds += 1;
            for ev in events {
                for (mid, ns) in &needles {
                    if ns.iter().any(|n| text_contains_token(&ev.text, n)) {
                        matched.entry(*mid).or_default().push(ScheduleSegment {
                            id: 0,
                            monitor_id: 0,
                            start_time: ev.start_time,
                            end_time: ev.end_time,
                            title: ev.title.clone(),
                            category: String::new(),
                            canceled: ev.canceled,
                            video_id: None,
                            collab: String::new(),
                        });
                    }
                }
            }
        }
        // If the guild list was non-empty but every per-guild fetch failed, treat
        // the sweep as failed (return None) rather than silently wiping stored
        // Discord events on a transient outage.
        if guild_count > 0 && ok_guilds == 0 {
            return None;
        }
        Some((matched, ok_guilds == guild_count))
    }

    pub async fn detect_youtube_api(&self, item: &DetectItem) -> DetectOutcome {
        let key = self
            .store
            .get_setting("youtube_api_key")
            .ok()
            .flatten()
            .unwrap_or_default();
        if key.is_empty() {
            return DetectOutcome::err(item.monitor_id, "no YouTube API key (Settings)");
        }
        if self.youtube_quota_exceeded() {
            return DetectOutcome::err(item.monitor_id, "YouTube API daily quota limit reached");
        }
        let channel_id = match self.youtube_channel_id(&item.url, &key).await {
            Ok(id) => id,
            Err(e) => return DetectOutcome::err(item.monitor_id, e.to_string()),
        };
        // search.list?eventType=live (note: 100 quota units per call).
        let resp = self
            .http
            .get("https://www.googleapis.com/youtube/v3/search")
            .query(&[
                ("part", "id"),
                ("channelId", channel_id.as_str()),
                ("eventType", "live"),
                ("type", "video"),
                ("key", key.as_str()),
            ])
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let _ = self.store.record_quota_usage("youtube", 100);
                let _ = self.store.record_quota_usage("youtube_search", 1);
                let v: Value = r.json().await.unwrap_or_default();
                match v["items"][0]["id"]["videoId"].as_str() {
                    Some(vid) => {
                        let went = self.youtube_actual_start(vid, &key).await;
                        let thumb = format!("https://i.ytimg.com/vi/{vid}/maxresdefault.jpg");
                        DetectOutcome::live_at(item.monitor_id, "live", went)
                            .with_stream_id(Some(vid.to_string()))
                            .with_broadcaster_id(Some(channel_id.clone()))
                            .with_thumbnail_url(Some(thumb))
                    }
                    None => DetectOutcome::offline(item.monitor_id),
                }
            }
            Ok(r) => DetectOutcome::err(item.monitor_id, format!("youtube api {}", r.status())),
            Err(e) => DetectOutcome::err(item.monitor_id, e.to_string()),
        }
    }

    /// Resolve a YouTube channel URL to its UC… channel id.
    async fn youtube_channel_id(&self, url: &str, key: &str) -> Result<String> {
        if let Some(pos) = url.find("/channel/") {
            let id = url[pos + "/channel/".len()..]
                .split(['/', '?', '#'])
                .next()
                .unwrap_or("");
            if id.starts_with("UC") {
                return Ok(id.to_string());
            }
        }
        if let Some(handle) = youtube_handle(url) {
            let resp = self
                .http
                .get("https://www.googleapis.com/youtube/v3/channels")
                .query(&[("part", "id"), ("forHandle", handle.as_str()), ("key", key)])
                .send()
                .await?;
            let v: Value = resp.json().await?;
            if let Some(id) = v["items"][0]["id"].as_str() {
                return Ok(id.to_string());
            }
            bail!("could not resolve YouTube handle {handle}");
        }
        bail!("unsupported YouTube URL (use /channel/UC… or @handle)")
    }

    async fn youtube_actual_start(&self, video_id: &str, key: &str) -> Option<i64> {
        let resp = self
            .http
            .get("https://www.googleapis.com/youtube/v3/videos")
            .query(&[
                ("part", "liveStreamingDetails"),
                ("id", video_id),
                ("key", key),
            ])
            .send()
            .await
            .ok()?;
        let v: Value = resp.json().await.ok()?;
        let s = v["items"][0]["liveStreamingDetails"]["actualStartTime"].as_str()?;
        parse_rfc3339(s)
    }

    /// A YouTube channel's upcoming streams via the Data API (instead of the
    /// `/streams` scrape): `search.list?eventType=upcoming` (~100 quota units) for
    /// the upcoming video ids, then `videos.list` for each one's scheduled start +
    /// title (~1 unit). `Some(vec)` on success (possibly empty); `None` on error /
    /// missing key (so the refresher won't wipe stored data on a transient fail).
    /// Batch `videos.list` call to get exact `scheduledStartTime` for up to 50
    /// YouTube video IDs at a time. Returns a map of `video_id → Unix timestamp`
    /// for items that have `liveStreamingDetails.scheduledStartTime` set.
    ///
    /// Cost: **~1 quota unit per call** (all IDs batched; chunks of 50 if needed).
    /// The video IDs come from scraping — no `search.list` (100 units/call) needed.
    pub async fn youtube_videos_list(
        &self,
        video_ids: &[&str],
    ) -> Option<HashMap<String, i64>> {
        let key = self
            .store
            .get_setting("youtube_api_key")
            .ok()
            .flatten()
            .unwrap_or_default();
        if key.is_empty() || video_ids.is_empty() {
            return None;
        }
        let mut result: HashMap<String, i64> = HashMap::new();
        for chunk in video_ids.chunks(50) {
            let ids_str = chunk.join(",");
            let resp = self
                .http
                .get("https://www.googleapis.com/youtube/v3/videos")
                .query(&[
                    ("part", "liveStreamingDetails"),
                    ("id", ids_str.as_str()),
                    ("key", key.as_str()),
                ])
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let _ = self.store.record_quota_usage("youtube", 1);
            let v: Value = resp.json().await.ok()?;
            for item in v["items"].as_array().into_iter().flatten() {
                if let (Some(id), Some(ts)) = (
                    item["id"].as_str(),
                    item["liveStreamingDetails"]["scheduledStartTime"]
                        .as_str()
                        .and_then(parse_rfc3339),
                ) {
                    result.insert(id.to_string(), ts);
                }
            }
        }
        Some(result)
    }

    // ----- Kick official API (client-credentials app token) -----

    async fn kick_app_token(&self) -> Result<String> {
        if let Some(tok) = self.kick_token.lock().await.as_ref() {
            if tok.expires_at > Instant::now() {
                return Ok(tok.access_token.clone());
            }
        }
        let client_id = self
            .store
            .get_setting("kick_client_id")?
            .unwrap_or_default();
        let client_secret = self
            .store
            .get_setting("kick_client_secret")?
            .unwrap_or_default();
        if client_id.is_empty() || client_secret.is_empty() {
            bail!("Kick credentials not set (Settings)");
        }
        let resp = self
            .http
            .post("https://id.kick.com/oauth/token")
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("Kick token request failed: {}", resp.status());
        }
        let v: Value = resp.json().await?;
        let access = v["access_token"]
            .as_str()
            .context("no access_token")?
            .to_string();
        let ttl = v["expires_in"]
            .as_u64()
            .unwrap_or(3600)
            .saturating_sub(60)
            .max(60);
        *self.kick_token.lock().await = Some(TwitchToken {
            access_token: access.clone(),
            expires_at: Instant::now() + Duration::from_secs(ttl),
        });
        Ok(access)
    }

    pub async fn detect_kick_api(&self, item: &DetectItem) -> DetectOutcome {
        let token = match self.kick_app_token().await {
            Ok(t) => t,
            Err(e) => return DetectOutcome::err(item.monitor_id, e.to_string()),
        };
        let Some(slug) = kick_slug(&item.url) else {
            return DetectOutcome::err(item.monitor_id, "cannot parse kick slug");
        };
        let resp = self
            .http
            .get("https://api.kick.com/public/v1/channels")
            .query(&[("slug", slug.as_str())])
            .bearer_auth(&token)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let v: Value = r.json().await.unwrap_or_default();
                // Defensive: the public API exposes the channel under data[0];
                // a live stream populates a stream/livestream object.
                let ch = &v["data"][0];
                let stream = if ch["stream"].is_object() {
                    &ch["stream"]
                } else {
                    &ch["livestream"]
                };
                let live = stream["is_live"].as_bool().unwrap_or(stream.is_object());
                let went = stream["start_time"]
                    .as_str()
                    .or_else(|| stream["created_at"].as_str())
                    .and_then(parse_rfc3339);
                if live {
                    // The livestream id (when present) identifies the broadcast.
                    let id = stream["id"]
                        .as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| stream["id"].as_str().map(str::to_string));
                    let thumb = stream["thumbnail"]["url"]
                        .as_str()
                        .or_else(|| stream["thumbnail"].as_str())
                        .map(str::to_string);
                    let title = stream["session_title"]
                        .as_str()
                        .or_else(|| stream["title"].as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let game = stream["category"]["name"]
                        .as_str()
                        .or_else(|| ch["category"]["name"].as_str())
                        .or_else(|| stream["categories"][0]["name"].as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let viewers = stream["viewer_count"]
                        .as_i64()
                        .or_else(|| stream["viewers"].as_i64());
                    DetectOutcome::live_at(item.monitor_id, "live", went)
                        .with_stream_id(id)
                        .with_broadcaster_id(kick_slug(&item.url).map(|s| s.to_string()))
                        .with_thumbnail_url(thumb)
                        .with_stream_title(title)
                        .with_stream_game(game)
                        .with_stream_viewers(viewers)
                        // Best-effort: present in some responses, harmlessly
                        // absent otherwise (the v2 scrape path carries it too).
                        .with_stream_followers(ch["followers_count"].as_i64())
                } else {
                    DetectOutcome::offline(item.monitor_id)
                }
            }
            Ok(r) => DetectOutcome::err(item.monitor_id, format!("kick api {}", r.status())),
            Err(e) => DetectOutcome::err(item.monitor_id, e.to_string()),
        }
    }

    // ----- scrape (no credentials) -----

    pub async fn detect_scrape(&self, item: &DetectItem) -> DetectOutcome {
        match item.platform {
            // Opt-in (Settings): use the Data API for liveness instead of scraping
            // the /live page. (Monitors set to the "YouTube Data API" detection
            // method already use it directly, never reaching here.)
            Platform::YouTube if self.youtube_api_enabled(K_YT_API_DETECT) => {
                self.detect_youtube_api(item).await
            }
            Platform::YouTube => self.scrape_youtube(item).await,
            Platform::Kick => self.scrape_kick(item).await,
            _ => self.detect_generic(item).await,
        }
    }

    async fn scrape_youtube(&self, item: &DetectItem) -> DetectOutcome {
        let jitter_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 2000) as u64;
        tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        let url = youtube_live_url(&item.url);
        let rb = self
            .http
            .get(&url)
            .header("Accept-Language", "en-US,en;q=0.9")
            // Bypass the EU consent interstitial that otherwise replaces the page.
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI");
        let resp = self.fingerprint.apply_yt_nav_headers(rb).send().await;
        match resp {
            Ok(r) => {
                let body = r.text().await.unwrap_or_default();
                // Prefer the structured player response: `videoDetails.isLive` is
                // authoritative and stays `false` for ended/upcoming streams even
                // when other live-related strings (e.g. `hqdefault_live`, or
                // `"isLive":true` in badge/DVR metadata nodes) still appear on the
                // page for a while after a stream ends.  This stops the scrape from
                // returning a false positive that immediately re-triggers a recording
                // attempt on a just-concluded stream.
                let pr_opt = extract_json_after(&body, "ytInitialPlayerResponse");
                let live = if let Some(pr) = &pr_opt {
                    pr["videoDetails"]["isLive"].as_bool().unwrap_or(false)
                } else {
                    // Fallback: structured data absent (degraded/bot-challenged
                    // page, network truncation). The string probes are less
                    // precise but better than silently returning offline.
                    body.contains("hqdefault_live")
                        || body.contains("\"isLive\":true")
                        || body.contains("\"isLiveNow\":true")
                };
                if live {
                    let (broadcaster_id, thumbnail_url, video_id, title) = if let Some(pr) = &pr_opt {
                        let ch_id =
                            pr["videoDetails"]["channelId"].as_str().map(str::to_string);
                        let thumb = pr["videoDetails"]["thumbnail"]["thumbnails"]
                            .as_array()
                            .and_then(|arr| arr.last())
                            .and_then(|t| t["url"].as_str())
                            .map(str::to_string);
                        let vid = pr["videoDetails"]["videoId"].as_str().map(str::to_string);
                        let title = pr["videoDetails"]["title"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        (ch_id, thumb, vid, title)
                    } else {
                        (None, None, None, None)
                    };
                    DetectOutcome::live(item.monitor_id, "live")
                        .with_broadcaster_id(broadcaster_id)
                        .with_thumbnail_url(thumbnail_url)
                        .with_stream_id(video_id)
                        .with_stream_title(title)
                } else {
                    DetectOutcome::offline(item.monitor_id)
                }
            }
            Err(e) => DetectOutcome::err(item.monitor_id, e.to_string()),
        }
    }

    /// Title + (broad) content category of a currently-live YouTube channel,
    /// scraped from the `/live` watch page's `ytInitialPlayerResponse` (no
    /// credentials). For the in-recording metadata change log. `None` when the
    /// page has no live video details (offline) or on error. YouTube exposes no
    /// public "current game" field, so `game` carries the page's content category
    /// (e.g. "Gaming") — the closest stable signal.
    /// Title + category of a currently-live YouTube channel (scraping the
    /// `/live` page). [`MetaFetch::Offline`] when the channel isn't live (the
    /// page redirects to a browse page, or points at the just-ended VOD);
    /// [`MetaFetch::Failed`] on network/HTTP errors or an unreadable page
    /// (consent/bot interstitial, layout change) — each reason logged at
    /// DEBUG, mirroring [`twitch_stream_meta`](Self::twitch_stream_meta).
    pub async fn youtube_stream_meta(&self, url: &str) -> MetaFetch {
        let live_url = youtube_live_url(url);
        let rb = self
            .http
            .get(&live_url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI");
        let resp = match self.fingerprint.apply_yt_nav_headers(rb).send().await {
            Ok(r) => r,
            Err(e) => {
                debug!("youtube_stream_meta: request for {live_url} failed: {e}");
                return MetaFetch::Failed;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            debug!("youtube_stream_meta: {live_url} got HTTP {status}");
            return MetaFetch::Failed;
        }
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => {
                debug!("youtube_stream_meta: reading the body for {live_url} failed: {e}");
                return MetaFetch::Failed;
            }
        };
        let out = parse_youtube_meta(&body);
        if matches!(out, MetaFetch::Failed) {
            debug!(
                "youtube_stream_meta: {live_url} page had no readable live player data \
                 (consent/bot interstitial or a layout change?)"
            );
        }
        out
    }

    async fn scrape_kick(&self, item: &DetectItem) -> DetectOutcome {
        let Some(slug) = kick_slug(&item.url) else {
            return DetectOutcome::err(item.monitor_id, "cannot parse kick slug");
        };
        let jitter_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 2000) as u64;
        tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        let url = format!("https://kick.com/api/v2/channels/{slug}");
        let rb = self
            .http
            .get(&url)
            .header("Accept", "application/json, text/plain, */*");
        let resp = self.fingerprint.apply_kick_xhr_headers(rb).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                #[derive(Deserialize)]
                struct Livestream {
                    is_live: Option<bool>,
                }
                #[derive(Deserialize)]
                struct KickResp {
                    livestream: Option<Livestream>,
                    // Follower total, free in the same response (viewer_history).
                    followers_count: Option<i64>,
                }
                match r.json::<KickResp>().await {
                    Ok(k) => {
                        let live = k
                            .livestream
                            .map(|l| l.is_live.unwrap_or(true))
                            .unwrap_or(false);
                        if live {
                            DetectOutcome::live(item.monitor_id, "live")
                                .with_stream_followers(k.followers_count)
                        } else {
                            DetectOutcome::offline(item.monitor_id)
                        }
                    }
                    Err(e) => DetectOutcome::err(item.monitor_id, format!("kick parse: {e}")),
                }
            }
            // Kick is behind Cloudflare; a 403 usually means bot-challenge.
            Ok(r) => DetectOutcome::err(item.monitor_id, format!("kick {}", r.status())),
            Err(e) => DetectOutcome::err(item.monitor_id, e.to_string()),
        }
    }

    /// Title + category of a currently-live Kick channel (the unofficial v2
    /// channel JSON; no credentials). For the in-recording metadata change log.
    /// [`MetaFetch::Offline`] when not live; [`MetaFetch::Failed`] on error or
    /// behind a Cloudflare challenge (reasons logged at DEBUG). Always uses
    /// the v2 endpoint (even when Kick API credentials are configured), so
    /// metadata may be unavailable if v2 is Cloudflare-blocked while detection
    /// runs on the official API — detection and recording are unaffected.
    pub async fn kick_stream_meta(&self, url: &str) -> MetaFetch {
        let Some(slug) = kick_slug(url) else {
            debug!("kick_stream_meta: couldn't parse a slug from {url}");
            return MetaFetch::Failed;
        };
        let api = format!("https://kick.com/api/v2/channels/{slug}");
        let rb = self
            .http
            .get(&api)
            .header("Accept", "application/json, text/plain, */*");
        let resp = match self.fingerprint.apply_kick_xhr_headers(rb).send().await {
            Ok(r) => r,
            Err(e) => {
                debug!("kick_stream_meta: request for {slug} failed: {e}");
                return MetaFetch::Failed;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            // 403 on the v2 endpoint usually means a Cloudflare bot challenge.
            debug!("kick_stream_meta: {slug} got HTTP {status}");
            return MetaFetch::Failed;
        }
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                debug!("kick_stream_meta: response parse for {slug} failed: {e}");
                return MetaFetch::Failed;
            }
        };
        let out = parse_kick_meta(&v);
        if matches!(out, MetaFetch::Failed) {
            debug!("kick_stream_meta: {slug} live but the response had no readable title (partial payload?)");
        }
        out
    }

    // ----- generic probe via streamlink -----

    pub async fn detect_generic(&self, item: &DetectItem) -> DetectOutcome {
        // Streamlink has no NRK/Nebula plugin (its probe would read "offline"
        // forever), but yt-dlp has real extractors for both — probe those
        // platforms via yt-dlp's live_status instead. Plain Generic keeps the
        // streamlink probe (raw HLS pages / streamlink's live-site list).
        let ytdlp_probe = item.platform.streamlink_unsupported()
            && item.platform != crate::models::Platform::Generic;
        let mut cmd = if ytdlp_probe {
            let bin = self
                .store
                .get_setting("ytdlp_binary_path") // ui.rs K_YTDLP_BINARY
                .ok()
                .flatten()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "yt-dlp".to_string());
            let mut cmd = tokio::process::Command::new(bin);
            cmd.arg("--quiet")
                .arg("--no-warnings")
                .arg("--skip-download")
                .arg("--no-playlist")
                .arg("--print")
                .arg("%(live_status)s")
                .arg(&item.url);
            cmd
        } else {
            let mut cmd = tokio::process::Command::new("streamlink");
            cmd.arg("--stream-url").arg(&item.url).arg("best");
            cmd
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let tool = if ytdlp_probe { "yt-dlp" } else { "streamlink" };
                return DetectOutcome::err(item.monitor_id, format!("spawn {tool}: {e}"));
            }
        };
        match tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await {
            Ok(Ok(out)) => {
                let live = if ytdlp_probe {
                    // live_status: is_live / was_live / not_live / is_upcoming /
                    // post_live / NA. Only an actually-running live counts.
                    out.status.success()
                        && String::from_utf8_lossy(&out.stdout).trim() == "is_live"
                } else {
                    out.status.success() && !out.stdout.is_empty()
                };
                debug!(monitor = item.monitor_id, live, ytdlp_probe, "generic probe done");
                if live {
                    DetectOutcome::live(item.monitor_id, "live")
                } else {
                    DetectOutcome::offline(item.monitor_id)
                }
            }
            Ok(Err(e)) => DetectOutcome::err(item.monitor_id, e.to_string()),
            Err(_) => DetectOutcome::err(item.monitor_id, "probe timed out"),
        }
    }
}

/// Background task: while a Twitch account is connected, periodically refresh the
/// auto Twitch-sub ad-free status for Twitch monitors. Cheap and idle-friendly:
/// at most one Helix lookup per unique broadcaster, no more than every few hours,
/// and nothing at all when no account is connected. A short poll tick means a
/// just-connected account is picked up within ~`TICK_SECS`, not a refresh period.
pub async fn refresh_ad_free(
    ctx: Arc<DetectContext>,
    events: EventTx,
    shutdown: Arc<AtomicBool>,
    jobs: crate::events::JobRegistry,
) {
    const INITIAL_DELAY_SECS: u64 = 20;
    const TICK_SECS: u64 = 60;
    const STALE_AFTER_SECS: i64 = 6 * 3600;

    sleep_cancellable(Duration::from_secs(INITIAL_DELAY_SECS), &shutdown).await;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        // Need a stored account id (set on connect with the subscriptions scope) to
        // resolve subs at all; legacy connections without it do no work until a
        // reconnect. The pass itself only spends a Helix call on stale/unchecked
        // monitors, so an idle tick is just one local DB query.
        if ctx.store.job_enabled("job_ad_free_refresh") {
            if crate::oauth::connected_user_id(ctx.store.as_ref()).is_some() {
                refresh_ad_free_once(&ctx, &events, &shutdown, STALE_AFTER_SECS).await;
            }
            crate::events::mark_job(&jobs, "Ad-free / sub refresh", TICK_SECS as i64);
        }
        sleep_cancellable(Duration::from_secs(TICK_SECS), &shutdown).await;
    }
}

/// One refresh pass: check each Twitch monitor whose cached status is stale,
/// de-duplicating per broadcaster login. Only conclusive results are persisted;
/// an undeterminable result (e.g. scope not yet granted) is left to retry. Emits
/// a bus event when a status actually changes so the UI reloads the column.
async fn refresh_ad_free_once(
    ctx: &Arc<DetectContext>,
    events: &EventTx,
    shutdown: &Arc<AtomicBool>,
    stale_after: i64,
) {
    let rows = match ctx.store.twitch_monitors_for_ad_free() {
        Ok(r) => r,
        Err(_) => return,
    };
    let now = now_unix();
    let mut by_login: HashMap<String, Option<bool>> = HashMap::new();
    for row in &rows {
        // Quitting: stop doing network + DB work mid-pass.
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        // The manual flag already wins in the UI, so the auto result would never be
        // shown — don't spend a Helix call on it.
        if row.ad_free {
            continue;
        }
        if let Some(at) = row.ad_free_sub_at {
            if now - at < stale_after {
                continue; // checked recently
            }
        }
        let Some(login) = twitch_login(&row.url) else {
            continue;
        };
        let result = match by_login.get(&login) {
            Some(r) => *r,
            None => {
                let r = ctx.check_twitch_sub(&login).await;
                by_login.insert(login.clone(), r);
                r
            }
        };
        // Persist only a conclusive result, atomically gated on still being
        // connected: a Disconnect during the await above clears cached results, and
        // this guarded write won't resurrect a stale "Yes (sub)" in that race.
        if let Some(sub) = result {
            let wrote = ctx
                .store
                .set_monitor_ad_free_sub_if_connected(
                    row.id,
                    Some(sub),
                    now,
                    crate::oauth::K_USER_ID,
                )
                .unwrap_or(false);
            if wrote {
                info!(monitor_id = row.id, subscribed = sub, "ad-free sub status refreshed");
                // Reload the UI only when the displayed status actually changed.
                if result != row.ad_free_sub {
                    let _ = events.send(AppEvent::MonitorState {
                        monitor_id: row.id,
                        state: row.last_state.clone(),
                    });
                }
            }
        }
    }
}

/// Load the persisted OCR re-check cadence stamps (see [`crate::models::K_OCR_LAST_ATTEMPT`])
/// into the in-memory map keyed by `(monitor_id, source_id)`. Mirrors the
/// byte-hash cache restore so the slow OCR cadence is enforced across restarts,
/// not just within one session.
fn load_ocr_attempts(store: &Store) -> HashMap<(i64, String), i64> {
    let raw: HashMap<String, i64> = store
        .get_setting(crate::models::K_OCR_LAST_ATTEMPT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    raw.into_iter()
        .filter_map(|(k, ts)| {
            let (mid, source) = k.split_once(':')?;
            Some(((mid.parse::<i64>().ok()?, source.to_string()), ts))
        })
        .collect()
}

/// Load the persisted per-monitor schedule-fetch timestamps from `app_settings`.
/// Keys are monitor IDs as strings; values are unix seconds of the last fetch.
fn load_schedule_fetched(store: &Store) -> HashMap<i64, i64> {
    let raw: HashMap<String, i64> = store
        .get_setting(crate::models::K_SCHEDULE_LAST_FETCHED)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    raw.into_iter()
        .filter_map(|(k, ts)| Some((k.parse::<i64>().ok()?, ts)))
        .collect()
}

/// Persist the current `last_fetched` map so the staleness window survives
/// app restarts (no unnecessary re-fetches of expensive YouTube API sources).
fn persist_schedule_fetched(store: &Store, map: &HashMap<i64, i64>) {
    let raw: HashMap<String, i64> = map.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    if let Ok(json) = serde_json::to_string(&raw) {
        let _ = store.set_setting(crate::models::K_SCHEDULE_LAST_FETCHED, &json);
    }
}

/// Background task: fetch YouTube community posts for the posts feed. Gated by
/// the `job_community_posts` toggle (Background view); independent of the
/// schedule-OCR scan.
///
/// The fetches are **trickled one channel at a time, spread across the whole
/// cycle** (with a randomized start and jittered gaps), rather than bursted.
/// There is deliberately no "fetch every channel now" pass, so a fresh launch
/// or a just-flipped toggle never produces a scrape burst — from YouTube's side
/// it looks like a person occasionally opening a community tab, not a crawler.
/// A full rotation of all channels takes roughly `CYCLE_SECS`, so each channel
/// is still revisited about every 6 h on average.
pub async fn refresh_community_posts(
    ctx: Arc<DetectContext>,
    events: EventTx,
    shutdown: Arc<AtomicBool>,
    jobs: crate::events::JobRegistry,
) {
    const CYCLE_SECS: u64 = 6 * 3600; // ~how often each channel is revisited
    const MIN_GAP_SECS: u64 = 90; // never fetch two channels closer than this
    const IDLE_POLL_SECS: u64 = 300; // toggle re-check when disabled / no channels

    // Randomized settle delay so the first fetch never lands at a fixed offset
    // after launch.
    sleep_cancellable(Duration::from_secs(rand_range_secs(90, 420)), &shutdown).await;

    // Round-robin queue of channels, rebuilt + reshuffled each rotation so the
    // order (and thus the traffic pattern) isn't predictable.
    let mut queue: Vec<MonitorWithChannel> = Vec::new();
    let mut total: u64 = 1;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        if !ctx.store.job_enabled("job_community_posts") {
            sleep_cancellable(Duration::from_secs(IDLE_POLL_SECS), &shutdown).await;
            continue;
        }
        if queue.is_empty() {
            queue = youtube_channels_deduped(&ctx);
            shuffle_channels(&mut queue);
            total = (queue.len() as u64).max(1);
        }
        let Some(row) = queue.pop() else {
            // No YouTube channels at all — nothing to fetch; check back later.
            sleep_cancellable(Duration::from_secs(IDLE_POLL_SECS), &shutdown).await;
            continue;
        };

        ctx.fetch_channel_posts(&row, &events, &shutdown).await;

        // Spread one channel per (CYCLE / N) so a full rotation ≈ CYCLE, with a
        // polite floor, then jitter ±40% so it isn't a fixed-interval metronome.
        let base = (CYCLE_SECS / total).max(MIN_GAP_SECS);
        let gap = jitter_secs(base, 40);
        crate::events::mark_job(&jobs, "YouTube posts refresh", gap as i64);
        sleep_cancellable(Duration::from_secs(gap), &shutdown).await;
    }
}

/// The YouTube monitors with channel info, de-duplicated per community URL (so
/// multiple instances of one channel are fetched once).
fn youtube_channels_deduped(ctx: &Arc<DetectContext>) -> Vec<MonitorWithChannel> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    ctx.store
        .list_monitors_with_channels()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.automation_on()) // dormant channels fetch no posts
        .filter(|r| r.monitor.platform() == Platform::YouTube)
        .filter(|r| seen.insert(youtube_community_url(&r.monitor.url)))
        .collect()
}

/// Cheap non-cryptographic randomness for traffic-spacing jitter (there is no
/// `rand` crate in the tree). Seeds from the wall clock and runs it through the
/// splitmix64 finalizer for a well-distributed value even when called close in
/// time. Adequate for spreading requests; not for anything security-sensitive.
fn spread_rand_u64() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut z = nanos.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A random whole-second duration in `[lo, hi]`.
fn rand_range_secs(lo: u64, hi: u64) -> u64 {
    if hi <= lo {
        return lo;
    }
    lo + spread_rand_u64() % (hi - lo + 1)
}

/// `base` seconds jittered by ±`pct`% (e.g. `pct = 40` → 60%..140% of `base`).
fn jitter_secs(base: u64, pct: u64) -> u64 {
    let span = base.saturating_mul(pct) / 100;
    rand_range_secs(base.saturating_sub(span), base.saturating_add(span))
}

/// In-place Fisher–Yates shuffle using [`spread_rand_u64`].
fn shuffle_channels(v: &mut [MonitorWithChannel]) {
    for i in (1..v.len()).rev() {
        let j = (spread_rand_u64() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

/// Background task: periodically refresh upcoming-stream schedules for enabled
/// Twitch/YouTube monitors. A short poll tick picks up newly-added monitors
/// quickly; each monitor is re-fetched at most every few hours (tracked
/// in-memory), and fetches are de-duplicated per URL within a pass.
pub async fn refresh_schedules(
    ctx: Arc<DetectContext>,
    events: EventTx,
    shutdown: Arc<AtomicBool>,
    refresh_now: Arc<tokio::sync::Notify>,
    yt_video_id_refetch: Arc<AtomicBool>,
    refresh_channel: Arc<std::sync::Mutex<Option<i64>>>,
    jobs: crate::events::JobRegistry,
) {
    const INITIAL_DELAY_SECS: u64 = 30;
    const TICK_SECS: u64 = 60;
    const REFRESH_SECS: u64 = 6 * 3600;

    // Persisted across restarts so the 6-hour staleness window is respected even
    // after a rebuild/restart — prevents every launch from burning expensive
    // `search.list` quota on all YouTube API schedule channels at once.
    let mut last_fetched: HashMap<i64, i64> = load_schedule_fetched(ctx.store.as_ref());
    // Separate, slower cadence for the expensive OCR sources, keyed by
    // (monitor, source id) and stamped in wall-clock unix seconds. A forced UI
    // refresh resets `last_fetched` (so cheap API/scrape sources re-run at once)
    // but NOT this map, so an image checked in the last `OCR_MIN_INTERVAL_SECS`
    // is not re-consulted on a config save. Restored from settings so the cadence
    // also survives an app restart — a rebuild can't trigger a fresh OCR sweep.
    let mut last_ocr: HashMap<(i64, String), i64> = load_ocr_attempts(ctx.store.as_ref());
    let mut discord_last: Option<Instant> = None;
    sleep_cancellable(Duration::from_secs(INITIAL_DELAY_SECS), &shutdown).await;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        if ctx.store.job_enabled("job_schedule_refresh") {
            refresh_schedules_once(
                &ctx,
                &events,
                &shutdown,
                &mut last_fetched,
                &mut last_ocr,
                &mut discord_last,
                &yt_video_id_refetch,
                &refresh_channel,
                REFRESH_SECS,
            )
            .await;
            crate::events::mark_job(&jobs, "Schedule refresh", TICK_SECS as i64);
        }
        // Wake on either the periodic tick or a UI-requested reload; the latter
        // forces a full re-fetch immediately (staleness window 0 = refresh all).
        tokio::select! {
            _ = sleep_cancellable(Duration::from_secs(TICK_SECS), &shutdown) => {}
            _ = refresh_now.notified() => {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                refresh_schedules_once(
                    &ctx, &events, &shutdown, &mut last_fetched, &mut last_ocr,
                    &mut discord_last, &yt_video_id_refetch, &refresh_channel, 0,
                )
                .await;
            }
        }
    }
}

/// Half-width (seconds) of the time window used to match a title-fill donor event
/// to a base event: ±2h, so a base event borrows the title of the nearest donor
/// event starting within two hours of it.
const TITLE_FILL_WINDOW_SECS: i64 = 7200;

/// Whether any non-canceled segment is missing a title (empty/whitespace) — the
/// trigger for the title-fill donor walk.
fn has_blank_title(segs: &[ScheduleSegment]) -> bool {
    segs.iter().any(|s| !s.canceled && s.title.trim().is_empty())
}

/// Fill blank titles on `base` from the nearest-in-time `donor` event whose title
/// is non-blank, within `window` seconds (±). Also copies the donor's category
/// when `base`'s is empty. Used when a schedule source publishes times but no
/// titles (e.g. a bare Twitch schedule) and a lower-priority source (banner /
/// community-post OCR) carries the titles. Mutates `base` in place.
fn fill_titles(base: &mut [ScheduleSegment], donor: &[ScheduleSegment], window: i64) {
    for b in base.iter_mut() {
        if b.canceled || !b.title.trim().is_empty() {
            continue;
        }
        let best = donor
            .iter()
            .filter(|d| !d.title.trim().is_empty())
            .map(|d| (d, (d.start_time - b.start_time).abs()))
            .filter(|(_, dist)| *dist <= window)
            .min_by_key(|(_, dist)| *dist);
        if let Some((d, _)) = best {
            b.title = d.title.clone();
            if b.category.trim().is_empty() && !d.category.trim().is_empty() {
                b.category = d.category.clone();
            }
        }
    }
}

/// One schedule-refresh pass over the user's ordered schedule sources.
///
/// For each enabled monitor due for a refresh, the enabled *per-monitor* sources
/// are tried in priority order and the first to return a **non-empty** schedule
/// wins: its rows are stored under the source's id and every other source's rows
/// for that monitor are cleared, so exactly one source owns each monitor's
/// schedule. A source returning `None` is a transient failure (stored rows left
/// alone, retried soon); `Some(empty)` is an authoritative "nothing scheduled".
/// The effective per-monitor source order and title-fill toggle come from the
/// global config plus any per-channel / per-monitor scope override (monitor over
/// channel over global).
///
/// YouTube `/streams` scrape winners are deferred so all their video IDs can be
/// refined in one batched `videos.list` call (exact `scheduledStartTime`) before
/// being stored under `"youtube"`. Discord runs last as a debounced batch sweep,
/// filling only monitors that no higher-priority source already resolved.
async fn refresh_schedules_once(
    ctx: &Arc<DetectContext>,
    events: &EventTx,
    shutdown: &Arc<AtomicBool>,
    last_fetched: &mut HashMap<i64, i64>,
    last_ocr: &mut HashMap<(i64, String), i64>,
    discord_last: &mut Option<Instant>,
    yt_video_id_refetch: &Arc<AtomicBool>,
    refresh_channel: &std::sync::Mutex<Option<i64>>,
    refresh_secs: u64,
) {
    let rows = match ctx.store.list_monitors_with_channels() {
        Ok(r) => r,
        Err(_) => return,
    };
    // Drop staleness entries for monitors that no longer exist.
    let live: std::collections::HashSet<i64> = rows.iter().map(|r| r.monitor.id).collect();
    last_fetched.retain(|id, _| live.contains(id));
    last_ocr.retain(|k, _| live.contains(&k.0));
    // Dormant channels (master switch off) fetch no schedules. Filter AFTER the
    // retain above so cleanup for deleted monitors still runs on the full list.
    let rows: Vec<MonitorWithChannel> = rows.into_iter().filter(|r| r.automation_on()).collect();
    // `now_secs` is the authoritative clock for both `last_fetched` (persisted,
    // must survive restarts) and the OCR cadence.  `now` (Instant) is kept only
    // for `discord_last`, which is in-memory-only and doesn't need wall-clock.
    let now = Instant::now();
    let now_secs = now_unix();

    // If the UI requested a targeted re-scrape for YouTube monitors whose stored
    // schedule segments are missing video IDs, clear those monitors from
    // `last_fetched` so they are refreshed this pass.
    if yt_video_id_refetch.swap(false, Ordering::SeqCst) {
        let missing_ids: std::collections::HashSet<i64> = ctx
            .store
            .youtube_monitors_missing_video_ids()
            .unwrap_or_default()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        last_fetched.retain(|id, _| !missing_ids.contains(id));
    }

    // If the UI requested a per-channel refresh, restrict this pass to monitors
    // belonging to that channel. `last_fetched` is cleared for those monitors so
    // they bypass the staleness gate; all other monitors are skipped entirely
    // (their schedules stay untouched). `None` = normal full pass.
    let only_channel: Option<i64> = refresh_channel.lock().unwrap().take();
    if let Some(cid) = only_channel {
        for row in rows.iter().filter(|r| r.channel.id == cid) {
            last_fetched.remove(&row.monitor.id);
        }
    }

    // The user's global ordered sources, plus per-channel / per-monitor scope
    // overrides (loaded once; resolved per monitor in the walk and, below, for the
    // Discord batch sweep — so Discord honors the same per-channel/instance control).
    let order = load_source_order(ctx.store.as_ref());
    let channel_scope = load_channel_scope_map(ctx.store.as_ref());
    let monitor_scope = load_monitor_scope_map(ctx.store.as_ref());
    let global_fill = global_title_fill(ctx.store.as_ref());
    // Per-channel source configs loaded once up front — avoids re-reading and
    // re-parsing the same JSON setting key on every iteration of the monitor loop.
    let channel_cfg_map = load_channel_cfg_map(ctx.store.as_ref());

    // Whether to refine scraped YouTube timestamps with a batched videos.list call.
    let yt_api_enabled = ctx.youtube_api_enabled(K_YT_API_SCHEDULE);
    // Per-(source, URL) fetch cache: a URL shared across monitors is fetched once
    // per source. `None` = transient failure (don't overwrite stored rows).
    let mut fetched: HashMap<(&'static str, String), Option<Vec<ScheduleSegment>>> = HashMap::new();
    // YouTube scrape winners are deferred so all video IDs batch into one API call.
    let mut yt_pending: Vec<(i64, Vec<ScheduleSegment>)> = Vec::new();
    let mut changed = false;
    // On a pure transient failure, retry sooner than the full interval (but not
    // every 60s tick — that would hammer the source).
    const TRANSIENT_RETRY_SECS: u64 = 300;
    // Expensive OCR sources re-resolve at most this often per (monitor, source),
    // independent of `refresh_secs` — so a forced UI refresh re-runs the cheap
    // sources at once but never re-OCRs an image processed this recently.
    const OCR_MIN_INTERVAL_SECS: u64 = 6 * 3600;

    for row in &rows {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        // When targeting a single channel, skip all other channels entirely.
        if let Some(cid) = only_channel {
            if row.channel.id != cid {
                continue;
            }
        }
        // Auto-off monitors still get schedule refreshes: Auto only gates the
        // automatic recording start, never metadata collection.
        // Re-fetch each monitor at most every `refresh_secs` (wall-clock, survives restarts).
        if let Some(t) = last_fetched.get(&row.monitor.id) {
            if now_secs.saturating_sub(*t) < refresh_secs as i64 {
                continue;
            }
        }
        let platform = row.monitor.platform();
        let cfg = channel_cfg_map
            .get(&row.channel.id.to_string())
            .cloned()
            .unwrap_or_default();

        // Resolve this monitor's effective source order + title-fill toggle from
        // the global config and any channel/monitor scope override.
        let ch_scope = channel_scope.get(&row.channel.id.to_string());
        let mon_scope = monitor_scope.get(&row.monitor.id.to_string());
        let eff_order = effective_order_from(&order, ch_scope, mon_scope);
        let eff_fill = effective_title_fill_from(global_fill, ch_scope, mon_scope);
        let per_monitor: Vec<ScheduleSourceKind> = eff_order
            .iter()
            .filter(|e| e.enabled)
            .filter_map(SourceEntry::kind)
            .filter(|k| k.is_per_monitor())
            .collect();

        // Walk the enabled per-monitor sources in priority order; first non-empty
        // result wins. When title-fill is on and the winner has blank titles, keep
        // walking lower-priority sources to borrow their titles (nearest in time)
        // before stopping. Track whether any source was authoritative (`Some`).
        let mut any_authoritative = false;
        let mut won: Option<(ScheduleSourceKind, Vec<ScheduleSegment>)> = None;
        // True when the winning source read its segments from the DB (OCR cadence
        // cache hit) rather than running fresh. In that case the DB already has the
        // correct rows and we must NOT call replace_schedule_source — doing so
        // would re-insert past segments that the DELETE (start_time >= now) skips,
        // causing exponential row accumulation on every tick.
        let mut won_is_db_cache = false;
        for &kind in &per_monitor {
            if !kind.applies_to(platform, &cfg) {
                continue;
            }

            // Expensive OCR sources resolve at most once per OCR_MIN_INTERVAL_SECS
            // per (monitor, source), tracked separately from `last_fetched`. Within
            // that window we reuse the rows the source last stored (no download, no
            // claude call) so it still wins / donates titles exactly as before;
            // outside it we re-OCR. This stops a forced UI refresh (refresh_secs ==
            // 0, fired on every config save) from re-OCR'ing already-processed
            // images. `None` = not an OCR source; `Some(true/false)` = OCR due/not.
            let ocr_due = kind.is_ocr().then(|| {
                let okey = (row.monitor.id, kind.id().to_string());
                let due = last_ocr
                    .get(&okey)
                    .map(|t| now_secs.saturating_sub(*t) >= OCR_MIN_INTERVAL_SECS as i64)
                    .unwrap_or(true);
                if due {
                    // Stamp at the decision point (write-through to settings) so a
                    // crash or restart can't reset the cadence and re-OCR early.
                    ctx.persist_ocr_attempt(row.monitor.id, kind.id(), now_secs);
                    last_ocr.insert(okey, now_secs);
                }
                due
            });

            let segs_opt = if ocr_due == Some(false) {
                // Within the cadence window: reuse the rows this source stored last
                // run. Empty → skip entirely (an empty cache must not pose as an
                // authoritative "nothing scheduled" and clear a real winner).
                let cached = ctx
                    .store
                    .schedule_segments_for_source(row.monitor.id, kind.id())
                    .unwrap_or_default();
                if cached.is_empty() {
                    continue;
                }
                Some(cached)
            } else {
                let key = (kind.id(), row.monitor.url.clone());
                match fetched.get(&key) {
                    Some(s) => s.clone(),
                    None => {
                        let s = ctx.resolve_source(kind, row, &cfg).await;
                        fetched.insert(key, s.clone());
                        s
                    }
                }
            };
            let Some(segs) = segs_opt else { continue };
            any_authoritative = true;
            if segs.is_empty() {
                continue;
            }
            match won.as_mut() {
                // Winner already chosen; this lower-priority source is a title donor.
                Some((_, win_segs)) => {
                    fill_titles(win_segs, &segs, TITLE_FILL_WINDOW_SECS);
                    if !has_blank_title(win_segs) {
                        break;
                    }
                }
                // First non-empty source wins. Stop here unless it has blank titles
                // and title-fill is on (then keep walking for donors).
                None => {
                    let need_titles = eff_fill && has_blank_title(&segs);
                    won_is_db_cache = ocr_due == Some(false);
                    won = Some((kind, segs));
                    if !need_titles {
                        break;
                    }
                }
            }
        }

        // Stamp staleness: full interval on success/authoritative-empty; sooner on a
        // pure transient failure (every applicable source returned `None`).
        let stamp: i64 = if any_authoritative {
            now_secs
        } else {
            // Back-date so the next check fires after TRANSIENT_RETRY_SECS, not REFRESH_SECS.
            now_secs.saturating_sub(refresh_secs.saturating_sub(TRANSIENT_RETRY_SECS) as i64)
        };
        last_fetched.insert(row.monitor.id, stamp);

        match won {
            // Defer scrape winners for the batched videos.list refinement below.
            Some((ScheduleSourceKind::YouTubeScrape, segs)) => {
                yt_pending.push((row.monitor.id, segs));
            }
            Some((kind, segs)) => {
                if won_is_db_cache {
                    // Rows already in DB from the last real OCR run — writing them
                    // back would duplicate past segments on every tick.
                    let _ = ctx.store.clear_other_schedule_sources(row.monitor.id, kind.id());
                } else if replace_schedule_and_notify(
                    ctx.store.as_ref(),
                    row.monitor.id,
                    kind.id(),
                    &segs,
                ) {
                    let _ = ctx.store.clear_other_schedule_sources(row.monitor.id, kind.id());
                    changed = true;
                }
            }
            None => {
                // No source won. If at least one was authoritative (returned
                // `Some(empty)` — definitively nothing), drop stale per-monitor rows
                // but keep Discord (managed by its own sweep). If every source was
                // transient (`None`), leave the stored schedule untouched.
                if any_authoritative
                    && ctx
                        .store
                        .clear_other_schedule_sources(row.monitor.id, "discord")
                        .is_ok()
                {
                    changed = true;
                }
            }
        }
    }

    // Batch videos.list for all pending YouTube scrape winners (one call, ~1 quota
    // unit). API timestamps supersede the approximate local-time parse from scrape.
    if yt_api_enabled && !yt_pending.is_empty() {
        let all_ids: Vec<&str> = yt_pending
            .iter()
            .flat_map(|(_, segs)| segs.iter())
            .filter_map(|s| s.video_id.as_deref())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        if !all_ids.is_empty() {
            if let Some(api_times) = ctx.youtube_videos_list(&all_ids).await {
                for (_, segs) in &mut yt_pending {
                    for seg in segs.iter_mut() {
                        if let Some(vid) = seg.video_id.as_deref() {
                            if let Some(&t) = api_times.get(vid) {
                                seg.start_time = t;
                            }
                        }
                    }
                }
            }
        }
    }

    // Store YouTube scrape results (after optional API refinement) under "youtube".
    for (monitor_id, segs) in &yt_pending {
        if replace_schedule_and_notify(ctx.store.as_ref(), *monitor_id, "youtube", segs) {
            let _ = ctx.store.clear_other_schedule_sources(*monitor_id, "youtube");
            changed = true;
        }
    }

    // Discord scheduled-events import — only when enabled in the source list AND a
    // token is configured. Runs on the platform cadence, but never more often than
    // DISCORD_MIN_SECS even on a forced reload (refresh_secs == 0) — a sweep hits the
    // user-token endpoints for every guild, so we debounce it to avoid bursty,
    // ban-flaggable traffic.
    const DISCORD_MIN_SECS: u64 = 60;
    let discord_interval = refresh_secs.max(DISCORD_MIN_SECS);
    // Discord runs as one batch sweep (not per-monitor), but still honors per-channel
    // / per-instance scope: resolve each monitor's effective order and sweep only the
    // monitors where Discord is enabled. Built only when a sweep could actually run
    // (token ready + debounce elapsed), so the per-monitor resolve isn't paid every tick.
    let discord_ready = ctx.discord_enabled()
        && discord_last.is_none_or(|t| now.duration_since(t).as_secs() >= discord_interval);
    let discord_monitors: std::collections::HashSet<i64> = if discord_ready {
        rows.iter()
            .filter(|row| {
                let ch_scope = channel_scope.get(&row.channel.id.to_string());
                let mon_scope = monitor_scope.get(&row.monitor.id.to_string());
                effective_order_from(&order, ch_scope, mon_scope)
                    .iter()
                    .any(|e| e.enabled && e.kind() == Some(ScheduleSourceKind::Discord))
            })
            .map(|row| row.monitor.id)
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    let discord_due = discord_ready && !discord_monitors.is_empty();
    if discord_due {
        // Stamp the attempt up front so a failing token / outage retries on the
        // interval, not every 60s tick (which would hammer Discord's auth endpoint).
        *discord_last = Some(now);
        if let Some((matched, complete)) = ctx.discord_sweep(&rows, shutdown).await {
            // Don't attach a Discord event to a monitor that already resolved a
            // schedule from a higher-priority source, so the two never duplicate.
            // Use the start-of-today floor the calendar uses, so an in-progress block
            // (started earlier today) still counts as a resolved schedule.
            let resolved = ctx
                .store
                .monitors_with_upcoming_non_discord(local_day_start())
                .unwrap_or_default();
            if complete {
                // Full sweep: authoritative — reconcile every monitor (clear ones
                // with no matched event).
                for row in &rows {
                    let mid = row.monitor.id;
                    // Clear when Discord is disabled for this monitor's scope, when a
                    // higher-priority source already resolved it, or when no event
                    // matched; otherwise store the swept events.
                    let segs = if !discord_monitors.contains(&mid) || resolved.contains(&mid) {
                        Vec::new()
                    } else {
                        matched.get(&mid).cloned().unwrap_or_default()
                    };
                    let _ = ctx.store.replace_schedule_source(mid, "discord", &segs);
                }
            } else {
                // Partial sweep: only update monitors we actually got events for, so
                // a streamer whose guild failed this pass keeps their stored events.
                for (mid, found) in &matched {
                    // A monitor with Discord disabled in its scope gets cleared even on
                    // a partial sweep, so a per-channel / per-instance opt-out takes
                    // effect without waiting for a full reconciliation pass.
                    let segs: &[ScheduleSegment] =
                        if !discord_monitors.contains(mid) || resolved.contains(mid) {
                            &[]
                        } else {
                            found
                        };
                    let _ = ctx.store.replace_schedule_source(*mid, "discord", segs);
                }
            }
            changed = true;
        }
    }

    if changed {
        // Wake the UI to reload the Next stream column (monitor_id 0 = "any").
        let _ = events.send(AppEvent::MonitorState {
            monitor_id: 0,
            state: String::new(),
        });
    }

    // Persist the updated fetch timestamps so the staleness window survives
    // the next restart (avoids a search.list burst on every rebuild/relaunch).
    persist_schedule_fetched(ctx.store.as_ref(), last_fetched);
}

/// Extract the Twitch login from a channel URL (`twitch.tv/<login>`).
pub(crate) fn twitch_login(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let lower = trimmed.to_lowercase();
    let pos = lower.find("twitch.tv/")?;
    let rest = &trimmed[pos + "twitch.tv/".len()..];
    let login = rest.split(['/', '?', '#']).next()?.trim();
    if login.is_empty() {
        None
    } else {
        Some(login.to_lowercase())
    }
}

/// Extract the Kick channel slug from a URL (`kick.com/<slug>`).
pub(crate) fn kick_slug(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let lower = trimmed.to_lowercase();
    let pos = lower.find("kick.com/")?;
    let rest = &trimmed[pos + "kick.com/".len()..];
    let slug = rest.split(['/', '?', '#']).next()?.trim();
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

/// Extract a YouTube `@handle` from a channel URL (e.g. `.../@LofiGirl/live`).
fn youtube_handle(url: &str) -> Option<String> {
    let pos = url.find("/@")?;
    let handle = url[pos + 1..].split(['/', '?', '#']).next()?.trim();
    if handle.len() > 1 {
        Some(handle.to_string())
    } else {
        None
    }
}

/// One Discord scheduled event reduced to what we need: the segment fields plus a
/// lowercased `text` blob (name + description + location) for URL matching.
struct DiscordEvt {
    start_time: i64,
    end_time: Option<i64>,
    title: String,
    canceled: bool,
    text: String,
}

/// Lowercased substrings that, if present in a Discord event's text, identify it as
/// belonging to this monitor's channel — the platform-specific `host/path`
/// (e.g. `twitch.tv/login`, `youtube.com/@handle`, `kick.com/slug`) plus the bare
/// normalized URL. Empty when nothing distinctive can be derived.
fn monitor_needles(url: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match Platform::detect(url) {
        Platform::Twitch => {
            if let Some(l) = twitch_login(url) {
                out.push(format!("twitch.tv/{l}"));
            }
        }
        Platform::YouTube => {
            if let Some(h) = youtube_handle(url) {
                // Host-qualified only — a bare "@handle" would substring-match
                // unrelated @-mentions in event text.
                out.push(format!("youtube.com/{}", h.to_lowercase()));
            }
        }
        Platform::Kick => {
            if let Some(s) = kick_slug(url) {
                out.push(format!("kick.com/{}", s.to_lowercase()));
            }
        }
        // No account-parser — the bare host/path needle below covers them.
        Platform::Nrk | Platform::Nebula | Platform::Generic => {}
    }
    // The bare host/path (scheme + www stripped), as a catch-all / for generic URLs.
    let norm = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .trim_end_matches('/')
        .to_lowercase();
    if norm.len() > 3 && !out.contains(&norm) {
        out.push(norm);
    }
    out
}

/// Whether `needle` occurs in `haystack` as a whole token — i.e. not immediately
/// preceded or followed by an identifier char (alphanumeric / `_` / `-`). This
/// stops a needle like `twitch.tv/ana` from matching `twitch.tv/anastasia` (and a
/// `youtube.com/@ab` from matching `…@abby`), while still allowing `www.` prefixes
/// and trailing punctuation. Both inputs are expected lowercased.
fn text_contains_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let i = from + rel;
        let before_ok = haystack[..i].chars().next_back().map(is_ident) != Some(true);
        let after = i + needle.len();
        let after_ok = haystack[after..].chars().next().map(is_ident) != Some(true);
        if before_ok && after_ok {
            return true;
        }
        from = i + 1;
    }
    false
}

/// Build the YouTube live URL for a channel URL (append `/live`).
fn youtube_live_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.to_lowercase().ends_with("/live") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/live")
    }
}

/// Build the YouTube `/streams` (live tab) URL for a channel URL, normalizing a
/// trailing `/live` or `/streams` first.
pub(crate) fn youtube_streams_url(url: &str) -> String {
    let t = url.trim().trim_end_matches('/');
    let t = t.strip_suffix("/live").or_else(|| t.strip_suffix("/streams")).unwrap_or(t);
    format!("{t}/streams")
}

/// Replace a monitor's schedule for `source` and emit `schedule_added` /
/// `schedule_updated` notifications for the diff (see
/// [`Store::replace_schedule_source_diffed`]). Returns whether the DB write
/// succeeded (the caller's `changed` flag). The channel name is resolved once,
/// only when there are changes to report.
fn replace_schedule_and_notify(
    store: &Store,
    monitor_id: i64,
    source: &str,
    segs: &[ScheduleSegment],
) -> bool {
    let changes = match store.replace_schedule_source_diffed(monitor_id, source, segs) {
        Ok(c) => c,
        Err(_) => return false,
    };
    if !changes.is_empty() {
        let channel = store
            .get_monitor_with_channel(monitor_id)
            .ok()
            .flatten()
            .map(|r| r.channel.name)
            .unwrap_or_default();
        for ch in &changes {
            use chrono::TimeZone;
            let when = chrono::Local
                .timestamp_opt(ch.start_time, 0)
                .single()
                .map(|dt| dt.format("%a %d %b %H:%M").to_string())
                .unwrap_or_default();
            let (kind, verb) = if ch.added {
                (NotificationKind::ScheduleAdded, "New stream scheduled")
            } else {
                (NotificationKind::ScheduleUpdated, "Schedule updated")
            };
            let title = if ch.title.is_empty() {
                format!("{channel} — {verb}")
            } else {
                format!("{channel} — {}", ch.title)
            };
            let mut body = when;
            if !ch.category.is_empty() {
                if !body.is_empty() {
                    body.push_str(" · ");
                }
                body.push_str(&ch.category);
            }
            let _ = store.insert_notification(&crate::store::NewNotification {
                kind: kind.id().to_string(),
                severity: "info".to_string(),
                title,
                body,
                monitor_id: Some(monitor_id),
                channel: channel.clone(),
                ref_key: format!(
                    "sched:{monitor_id}:{}:{}",
                    ch.start_time,
                    if ch.added { "add" } else { "upd" }
                ),
                ..Default::default()
            });
        }
    }
    true
}

/// Build the YouTube posts-tab URL for a channel URL. YouTube renamed this
/// tab from "Community" to "Posts" and retired the `/community` path — it
/// now serves a "This Community isn't available" placeholder with zero posts
/// instead of a 404, so a fetch against it "succeeds" (HTTP 200) while
/// silently returning nothing. `/posts` is the current canonical path.
pub(crate) fn youtube_community_url(url: &str) -> String {
    let t = url.trim().trim_end_matches('/');
    let t = t
        .strip_suffix("/live")
        .or_else(|| t.strip_suffix("/streams"))
        .or_else(|| t.strip_suffix("/community"))
        .or_else(|| t.strip_suffix("/posts"))
        .unwrap_or(t);
    format!("{t}/posts")
}

/// Whether `s` looks like an http(s) URL (vs. a local filesystem path).
fn is_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("//")
}

/// A short image extension from a URL path (before `?`), defaulting to `jpg` —
/// the OCR CLI needs a real image extension to recognize the file type.
fn url_image_ext(url: &str) -> &str {
    let path = url.split('?').next().unwrap_or(url);
    match path.rsplit('.').next() {
        Some(e) if (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()) => e,
        _ => "jpg",
    }
}

/// Collect all community-post image URLs from `ytInitialData` — each
/// `backstageImageRenderer`'s largest thumbnail, in document order (newest
/// post first). Stops recursing into a renderer once its image is captured.
fn community_images(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            if let Some(url) = map
                .get("backstageImageRenderer")
                .and_then(|img| largest_thumbnail(img.get("image")))
            {
                out.push(url);
                return;
            }
            for val in map.values() {
                community_images(val, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                community_images(val, out);
            }
        }
        _ => {}
    }
}

/// A full YouTube community post parsed from `ytInitialData` (the posts feed).
/// Distinct from [`community_images`] (schedule-OCR), which extracts only images.
struct ParsedPost {
    post_id: String,
    author: String,
    author_icon_url: String,
    /// The author's `UC…` channel id, when extractable — compared against the
    /// page owner id to tell the channel's own posts from viewer posts.
    author_channel_id: String,
    /// `showPostAuthorBackgroundHighlight` present ⇔ the channel owner authored
    /// this post (YouTube renders it with the owner's highlight).
    is_owner_highlight: bool,
    published_text: String,
    body_text: String,
    /// JSON array of `{text, url}` runs (url `""` = plain) for 1:1 link rendering.
    links_json: String,
    vote_count: String,
    /// Attachment image URLs, in document order (single or multi-image posts).
    image_urls: Vec<String>,
    /// For a reshare: the quoted original as JSON `{author, author_channel_id,
    /// published_text, body_text, links_json}` — empty for a normal post.
    shared_json: String,
    /// For a reshare: the quoted original's attachment image URLs, in order.
    shared_image_urls: Vec<String>,
    /// The post renderer subtree, serialized for forward-compat re-parsing.
    raw_json: String,
}

/// Flatten a `{runs:[…]}` content node into concatenated body text + a
/// `[{text,url}]` links array (the 1:1 shape the feed renders). Shared by the
/// normal post's `contentText` and a reshare's `content`.
fn runs_to_body_and_links(node: Option<&Value>) -> (String, String) {
    let mut body_text = String::new();
    let mut runs_json: Vec<Value> = Vec::new();
    if let Some(runs) = node.and_then(|c| c.get("runs")).and_then(|r| r.as_array()) {
        for run in runs {
            let text = run.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let url = run
                .get("navigationEndpoint")
                .and_then(|ne| {
                    ne.get("urlEndpoint")
                        .and_then(|u| u.get("url"))
                        .and_then(|u| u.as_str())
                        .or_else(|| {
                            ne.get("commandMetadata")
                                .and_then(|c| c.get("webCommandMetadata"))
                                .and_then(|w| w.get("url"))
                                .and_then(|u| u.as_str())
                        })
                })
                .unwrap_or("");
            body_text.push_str(text);
            runs_json.push(serde_json::json!({ "text": text, "url": url }));
        }
    }
    let links_json = serde_json::to_string(&runs_json).unwrap_or_else(|_| "[]".to_string());
    (body_text, links_json)
}

/// A post author's `UC…` channel id from an endpoint node (`authorEndpoint`, or
/// a reshare's `endpoint`/`navigationEndpoint`): the current `profileCardCommand`
/// shape, then the legacy `browseEndpoint`.
fn author_channel_id_from(ep: Option<&Value>) -> String {
    ep.and_then(|e| e.get("profileCardCommand"))
        .and_then(|c| c.get("profileOwnerExternalChannelId"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            ep.and_then(|e| e.get("browseEndpoint"))
                .and_then(|b| b.get("browseId"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

/// Recursively collect full community posts from `ytInitialData`. Matches both
/// `backstagePostRenderer` (normal) and `sharedPostRenderer` (a reshare); once a
/// post subtree is captured we don't recurse into it (a shared post embeds the
/// original, which would otherwise double-count).
fn parse_community_posts(v: &Value, out: &mut Vec<ParsedPost>) {
    match v {
        Value::Object(map) => {
            if let Some(post) = map.get("backstagePostRenderer") {
                if let Some(p) = parse_one_post(post, false) {
                    out.push(p);
                }
                return; // don't recurse into a captured post subtree
            }
            if let Some(post) = map.get("sharedPostRenderer") {
                if let Some(p) = parse_one_post(post, true) {
                    out.push(p);
                }
                return;
            }
            for val in map.values() {
                parse_community_posts(val, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                parse_community_posts(val, out);
            }
        }
        _ => {}
    }
}

/// Extract a string field from a page's inline `ytcfg` blob (`"KEY":"value"`).
/// Used for `INNERTUBE_API_KEY` / `INNERTUBE_CONTEXT_CLIENT_VERSION`, which the
/// continuation `browse` POSTs must echo.
fn ytcfg_str(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = body.find(&pat)? + pat.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    let v = &rest[..end];
    if v.is_empty() {
        None
    } else {
        Some(v.replace("\\/", "/"))
    }
}

/// DFS for the feed's `continuationItemRenderer` → continuation token. Present
/// on the initial `ytInitialData` and on every `browse` continuation response
/// while more pages exist; absent on the last page.
fn find_continuation_token(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(t) = map
                .get("continuationItemRenderer")
                .and_then(|c| c.get("continuationEndpoint"))
                .and_then(|e| e.get("continuationCommand"))
                .and_then(|c| c.get("token"))
                .and_then(|t| t.as_str())
            {
                return Some(t.to_string());
            }
            map.values().find_map(find_continuation_token)
        }
        Value::Array(arr) => arr.iter().find_map(find_continuation_token),
        _ => None,
    }
}

/// Classify a parsed post as the channel's own (`channel`) or a viewer's
/// (`viewer`). A post is the channel's when YouTube gives it the owner
/// highlight, or its author id equals the page owner id. It is a viewer's only
/// when both ids are known and differ — anything ambiguous (owner id or author
/// id missing) stays `channel` so a genuine post is never hidden.
fn classify_author_kind(p: &ParsedPost, owner_id: &str) -> &'static str {
    if p.is_owner_highlight || (!owner_id.is_empty() && p.author_channel_id == owner_id) {
        "channel"
    } else if !owner_id.is_empty() && !p.author_channel_id.is_empty() {
        "viewer"
    } else {
        "channel"
    }
}

/// First `runs[0].text` of a `{runs:[…]}` node, else empty.
fn first_run_text(node: Option<&Value>) -> String {
    node.and_then(|n| n.get("runs"))
        .and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .and_then(|r| r.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string()
}

/// Parse one community item. `shared == false` is a normal `backstagePostRenderer`;
/// `shared == true` is a `sharedPostRenderer` (a reshare / quote-post, whose
/// fields are named differently and which embeds the quoted original).
/// Best-effort: a missing `postId` disqualifies the item; every other field
/// defaults to empty.
fn parse_one_post(post: &Value, shared: bool) -> Option<ParsedPost> {
    let post_id = post.get("postId").and_then(|v| v.as_str())?.to_string();
    if shared {
        return parse_shared_post(post_id, post);
    }
    let author = first_run_text(post.get("authorText"));
    let author_icon_url = largest_thumbnail(post.get("authorThumbnail")).unwrap_or_default();
    let author_channel_id = author_channel_id_from(post.get("authorEndpoint"));
    let is_owner_highlight = post.get("showPostAuthorBackgroundHighlight").is_some();
    let published_text = first_run_text(post.get("publishedTimeText"));
    let vote_count = post
        .get("voteCount")
        .and_then(|v| v.get("simpleText").and_then(|s| s.as_str()))
        .unwrap_or("")
        .to_string();

    let (body_text, links_json) = runs_to_body_and_links(post.get("contentText"));

    // Attachment images (single `backstageImageRenderer` or multi-image) in order —
    // reuse the schedule-OCR image walk, scoped to this post's attachment subtree.
    let mut image_urls = Vec::new();
    if let Some(att) = post.get("backstageAttachment") {
        community_images(att, &mut image_urls);
    }

    let raw_json = serde_json::to_string(post).unwrap_or_default();

    Some(ParsedPost {
        post_id,
        author,
        author_icon_url,
        author_channel_id,
        is_owner_highlight,
        published_text,
        body_text,
        links_json,
        vote_count,
        image_urls,
        shared_json: String::new(),
        shared_image_urls: Vec::new(),
        raw_json,
    })
}

/// Parse a `sharedPostRenderer` — the resharer's identity + comment, plus the
/// quoted original (parsed with the normal path) captured into `shared_json` /
/// `shared_image_urls`.
fn parse_shared_post(post_id: String, post: &Value) -> Option<ParsedPost> {
    // The resharer's display name concatenates all runs (usually one).
    let author = post
        .get("displayName")
        .and_then(|d| d.get("runs"))
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|run| run.get("text").and_then(|t| t.as_str()))
                .collect::<String>()
        })
        .unwrap_or_default();
    let author_icon_url = largest_thumbnail(post.get("thumbnail")).unwrap_or_default();
    let author_channel_id = {
        let id = author_channel_id_from(post.get("endpoint"));
        if id.is_empty() {
            author_channel_id_from(post.get("navigationEndpoint"))
        } else {
            id
        }
    };
    let published_text = first_run_text(post.get("publishedTimeText"));
    let (body_text, links_json) = runs_to_body_and_links(post.get("content"));

    // The quoted original is itself a backstagePostRenderer — parse it and keep
    // a snapshot (its own images ride `shared_image_urls`).
    let (shared_json, shared_image_urls) = match post
        .get("originalPost")
        .and_then(|o| o.get("backstagePostRenderer"))
        .and_then(|o| parse_one_post(o, false))
    {
        Some(op) => {
            let sj = serde_json::json!({
                "author": op.author,
                "author_channel_id": op.author_channel_id,
                "published_text": op.published_text,
                "body_text": op.body_text,
                "links_json": op.links_json,
            })
            .to_string();
            (sj, op.image_urls)
        }
        None => (String::new(), Vec::new()),
    };

    let raw_json = serde_json::to_string(post).unwrap_or_default();

    Some(ParsedPost {
        post_id,
        author,
        author_icon_url,
        author_channel_id,
        is_owner_highlight: false,
        published_text,
        body_text,
        links_json,
        vote_count: String::new(),
        image_urls: Vec::new(),
        shared_json,
        shared_image_urls,
        raw_json,
    })
}

/// The largest (last) thumbnail URL from an `image.thumbnails[]` node,
/// [normalized][normalize_yt_image_url] to a stable size so the same logical
/// image yields a byte-identical fetch every pass.
fn largest_thumbnail(image: Option<&Value>) -> Option<String> {
    let url = image?
        .get("thumbnails")?
        .as_array()?
        .last()?
        .get("url")?
        .as_str()?;
    Some(normalize_yt_image_url(url))
}

/// Normalize a Google image-CDN URL (`yt3.ggpht.com`, `*.googleusercontent.com`)
/// to a stable identity: strip the volatile resize/crop directive (`=s512-c-…`,
/// `=w640-h480-…`) that drifts between fetches as YouTube reshuffles the size
/// ladder, then request one fixed canonical size. This keeps both the
/// community-pass URL hash and the downloaded bytes identical for an unchanged
/// post, so an already-OCR'd image reliably hits the cache instead of churning
/// into a fresh `claude` call. Non-Google URLs are returned unchanged.
fn normalize_yt_image_url(url: &str) -> String {
    if !(url.contains("ggpht.com") || url.contains("googleusercontent.com")) {
        return url.to_string();
    }
    // The resize directive lives in the final path segment, after a `=`. Cut at
    // the first `=` (Google CDN paths carry no query string) and pin one size.
    match url.split_once('=') {
        Some((base, _)) => format!("{base}=s1024"),
        None => url.to_string(),
    }
}

/// First `pbs.twimg.com/media/…` image URL in a Twitter syndication response,
/// handling both raw and JSON-escaped (`\/`, `/`) slashes. Best-effort.
fn first_twimg_media(body: &str) -> Option<String> {
    let host_pos = body.find("pbs.twimg.com")?;
    // Back up to this URL's scheme.
    let scheme_pos = body[..host_pos].rfind("http")?;
    let rest = &body[scheme_pos..];
    let end = rest.find('"').unwrap_or(rest.len());
    let url = rest[..end].replace("\\/", "/").replace("\\u002F", "/");
    (url.starts_with("http") && url.contains("pbs.twimg.com/media/")).then_some(url)
}

/// Extract upcoming scheduled streams from a YouTube `/streams` page. Each
/// upcoming entry's `videoRenderer` carries `upcomingEventData.startTime` (unix
/// seconds) plus the title; we walk `ytInitialData` for those. Best-effort —
/// returns an empty vec if the page shape changes.
fn parse_youtube_schedule(body: &str) -> Vec<ScheduleSegment> {
    let Some(data) = extract_json_after(body, "ytInitialData") else {
        return Vec::new();
    };
    let mut out: Vec<ScheduleSegment> = Vec::new();
    collect_upcoming(&data, &mut out);
    // The same renderer can appear under multiple tabs/shelves — de-dup.
    out.sort_by(|a, b| a.start_time.cmp(&b.start_time).then_with(|| a.title.cmp(&b.title)));
    out.dedup_by(|a, b| a.start_time == b.start_time && a.title == b.title);
    out
}

/// Shape-independent fallback: scan the raw `/streams` page for upcoming-stream
/// start markers when the structured `ytInitialData` walk yields nothing (Polymer
/// markup drift). Two markers are recognized:
///   - `"upcomingEventData":{"startTime":"<unix seconds>"`  (classic renderer)
///   - `"scheduledStartTime":"<rfc3339>"`                   (microformat / player)
/// For each, the nearest `"videoId"` and title are pulled from a bounded window so
/// a layout change doesn't silently zero out the schedule. Best-effort: a segment
/// is pushed only when a start time parses; the title falls back to a placeholder.
fn collect_upcoming_fallback(body: &str, out: &mut Vec<ScheduleSegment>) {
    const WINDOW: usize = 4000;
    // 1) upcomingEventData.startTime — unix seconds in a quoted string.
    let marker = "\"upcomingEventData\":{\"startTime\":\"";
    let mut from = 0;
    while let Some(rel) = body[from..].find(marker) {
        let i = from + rel;
        from = i + marker.len();
        if let Some(start) =
            read_until_quote(body, i + marker.len()).and_then(|s| s.parse::<i64>().ok())
        {
            out.push(fallback_seg(body, i, WINDOW, start));
        }
    }
    // 2) scheduledStartTime — rfc3339 in a quoted string.
    let marker = "\"scheduledStartTime\":\"";
    let mut from = 0;
    while let Some(rel) = body[from..].find(marker) {
        let i = from + rel;
        from = i + marker.len();
        if let Some(start) = read_until_quote(body, i + marker.len()).and_then(parse_rfc3339) {
            out.push(fallback_seg(body, i, WINDOW, start));
        }
    }
    out.sort_by(|a, b| a.start_time.cmp(&b.start_time).then_with(|| a.title.cmp(&b.title)));
    out.dedup_by(|a, b| a.start_time == b.start_time && a.title == b.title);
}

/// Build a fallback `ScheduleSegment` for a start marker at byte `center`, pulling
/// the nearest video id and title from the surrounding `window`.
fn fallback_seg(body: &str, center: usize, window: usize, start: i64) -> ScheduleSegment {
    let (lo, hi) = clamp_window(body, center, window);
    let slice = &body[lo..hi];
    let rel = center - lo;
    ScheduleSegment {
        id: 0,
        monitor_id: 0,
        start_time: start,
        end_time: None,
        title: nearest_title(slice, rel).unwrap_or_else(|| "Upcoming stream".to_string()),
        category: String::new(),
        canceled: false,
        video_id: nearest_video_id(slice, rel),
        collab: String::new(),
    }
}

/// Read a JSON string value's content starting at byte `start` (the first char
/// after the opening quote), up to the next unescaped `"`. Returns the raw
/// (still-escaped) slice.
fn read_until_quote(s: &str, start: usize) -> Option<&str> {
    let bytes = s.as_bytes();
    if start > bytes.len() {
        return None;
    }
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return s.get(start..i),
            _ => i += 1,
        }
    }
    None
}

/// Clamp `[center-window, center+window]` to valid UTF-8 char boundaries within `body`.
fn clamp_window(body: &str, center: usize, window: usize) -> (usize, usize) {
    let mut lo = center.saturating_sub(window);
    while lo > 0 && !body.is_char_boundary(lo) {
        lo -= 1;
    }
    let mut hi = (center + window).min(body.len());
    while hi < body.len() && !body.is_char_boundary(hi) {
        hi += 1;
    }
    (lo, hi)
}

/// Unescape a raw JSON string body by wrapping it back in quotes and letting
/// serde do the work (handles `\"`, `\\`, `\uXXXX`, …). Falls back to the raw text.
fn unescape_json_str(raw: &str) -> String {
    serde_json::from_str::<String>(&format!("\"{raw}\"")).unwrap_or_else(|_| raw.to_string())
}

/// The `"title"` value in `slice` whose key sits closest to byte offset `center`,
/// decoded from whichever YouTube text shape follows it. Center-aware so that with
/// several events in one window each marker keeps its own title.
fn nearest_title(slice: &str, center: usize) -> Option<String> {
    let key = "\"title\":";
    let mut best: Option<usize> = None;
    let mut from = 0;
    while let Some(r) = slice[from..].find(key) {
        let at = from + r;
        from = at + key.len();
        if best.is_none_or(|b: usize| at.abs_diff(center) < b.abs_diff(center)) {
            best = Some(at);
        }
    }
    let at = best?;
    title_from_value(slice[at + key.len()..].trim_start())
}

/// Decode a YouTube title from the value text that follows a `"title":` key:
/// `{"runs":[{"text":…}]}` (concatenated), `{"simpleText":…}`, `{"content":…}`,
/// or a plain `"…"` string.
fn title_from_value(rest: &str) -> Option<String> {
    if let Some(after) = rest.strip_prefix("{\"runs\":[") {
        let end = after.find(']').unwrap_or(after.len());
        let runs = &after[..end];
        let mut title = String::new();
        let text_key = "\"text\":\"";
        let mut from = 0;
        while let Some(r) = runs[from..].find(text_key) {
            let at = from + r;
            from = at + text_key.len();
            if let Some(raw) = read_until_quote(runs, at + text_key.len()) {
                title.push_str(&unescape_json_str(raw));
            }
        }
        return (!title.trim().is_empty()).then_some(title);
    }
    for prefix in ["{\"simpleText\":\"", "{\"content\":\"", "\""] {
        if let Some(after) = rest.strip_prefix(prefix) {
            if let Some(raw) = read_until_quote(after, 0) {
                let t = unescape_json_str(raw);
                if !t.trim().is_empty() {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// The 11-char `videoId` in `slice` closest to byte offset `center`.
fn nearest_video_id(slice: &str, center: usize) -> Option<String> {
    let key = "\"videoId\":\"";
    let mut best: Option<(usize, String)> = None;
    let mut from = 0;
    while let Some(r) = slice[from..].find(key) {
        let at = from + r;
        from = at + key.len();
        if let Some(id) = read_until_quote(slice, at + key.len()) {
            if id.len() == 11 {
                let dist = at.abs_diff(center);
                if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                    best = Some((dist, id.to_string()));
                }
            }
        }
    }
    best.map(|(_, id)| id)
}

/// Recursively collect upcoming-stream entries from `ytInitialData`:
/// - Old format: `videoRenderer.upcomingEventData.startTime` (Unix seconds string).
/// - New Polymer format: `lockupViewModel` with `contentImage` (thumbnail → video ID)
///   and `metadata.lockupMetadataViewModel` containing "Scheduled for M/D/YY, H:MM AM/PM".
fn collect_upcoming(v: &Value, out: &mut Vec<ScheduleSegment>) {
    match v {
        Value::Object(map) => {
            // Old format: videoRenderer → upcomingEventData.startTime (Unix seconds).
            if let Some(start) = map
                .get("upcomingEventData")
                .and_then(|u| u.get("startTime"))
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse::<i64>().ok())
            {
                let title = yt_render_title(map.get("title"));
                if !title.is_empty() {
                    out.push(ScheduleSegment {
                        id: 0,
                        monitor_id: 0,
                        start_time: start,
                        end_time: None,
                        title,
                        category: String::new(),
                        canceled: false,
                        video_id: None,
                        collab: String::new(),
                    });
                }
            }
            // New Polymer format: `lockupViewModel` carries `contentImage` (thumbnail
            // URL encodes the video ID) and `metadata.lockupMetadataViewModel`.
            if map.contains_key("contentImage") {
                if let Some((start, title, vid)) = extract_lockup_viewmodel(map) {
                    out.push(ScheduleSegment {
                        id: 0,
                        monitor_id: 0,
                        start_time: start,
                        end_time: None,
                        title,
                        category: String::new(),
                        canceled: false,
                        video_id: Some(vid),
                        collab: String::new(),
                    });
                }
            }
            for val in map.values() {
                collect_upcoming(val, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                collect_upcoming(val, out);
            }
        }
        _ => {}
    }
}

/// Extract `(start_unix, title, video_id)` from a `lockupViewModel` object.
/// The video ID is parsed from the thumbnail URL in `contentImage`.
fn extract_lockup_viewmodel(
    map: &serde_json::Map<String, Value>,
) -> Option<(i64, String, String)> {
    let video_id = extract_yt_video_id_from_thumbnail(map)?;
    let lmvm = map
        .get("metadata")
        .and_then(|m| m.as_object())
        .and_then(|m| m.get("lockupMetadataViewModel"))
        .and_then(|v| v.as_object())?;
    let (start, title) = extract_lockup_schedule(lmvm)?;
    Some((start, title, video_id))
}

/// Extract the YouTube video ID from the thumbnail URL inside a `lockupViewModel`
/// `contentImage.thumbnailViewModel.image.sources[0].url`.
/// URL shape: `https://i.ytimg.com/vi/<VIDEO_ID>/hqdefault.jpg?...`
fn extract_yt_video_id_from_thumbnail(map: &serde_json::Map<String, Value>) -> Option<String> {
    let url = map
        .get("contentImage")
        .and_then(|ci| ci.get("thumbnailViewModel"))
        .and_then(|tv| tv.get("image"))
        .and_then(|img| img.get("sources"))
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())?;
    let after_vi = url.split("/vi/").nth(1)?;
    let video_id = after_vi.split('/').next().filter(|s| !s.is_empty())?;
    Some(video_id.to_string())
}

/// Extract `(start_unix, title)` from a `lockupMetadataViewModel` object.
/// Returns `None` if no "Scheduled for" row is present.
fn extract_lockup_schedule(
    lmvm: &serde_json::Map<String, Value>,
) -> Option<(i64, String)> {
    let title = lmvm
        .get("title")
        .and_then(|t| t.get("content"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let rows = lmvm
        .get("metadata")
        .and_then(|m| m.get("contentMetadataViewModel"))
        .and_then(|c| c.get("metadataRows"))
        .and_then(|r| r.as_array())?;
    for row in rows {
        if let Some(parts) = row.get("metadataParts").and_then(|p| p.as_array()) {
            for part in parts {
                if let Some(content) = part
                    .get("text")
                    .and_then(|t| t.get("content"))
                    .and_then(|c| c.as_str())
                {
                    if let Some(start) = parse_yt_scheduled_text(content) {
                        return Some((start, title));
                    }
                }
            }
        }
    }
    None
}

/// Parse `"Scheduled for ..."` date text to a Unix timestamp.
///
/// YouTube geo-targets its server-rendered times to the viewer's IP timezone,
/// so we interpret the parsed date/time as local time. Two date formats occur:
///
/// - **US** (2-digit year): `"Scheduled for M/D/YY, H:MM AM/PM"` — seen from
///   US IP addresses with `Accept-Language: en-US`.
/// - **European** (4-digit year): `"Scheduled for D/M/YYYY, H:MM [AM/PM]"` —
///   seen from European IP addresses even with `Accept-Language: en-US`.
///
/// We distinguish them by the year field length: 2 digits → US (M/D), 4 digits
/// → European (D/M). Both 12-hour (AM/PM) and 24-hour time are handled.
fn parse_yt_scheduled_text(s: &str) -> Option<i64> {
    use chrono::{NaiveDate, NaiveTime, TimeZone};
    let rest = s.strip_prefix("Scheduled for ")?;
    let (date_str, time_str) = rest.split_once(", ")?;
    // Parse date; year-field length tells us which ordering to use.
    let mut dp = date_str.split('/');
    let first: u32 = dp.next()?.parse().ok()?;
    let second: u32 = dp.next()?.parse().ok()?;
    let year_raw = dp.next()?.trim();
    let (year, month, day) = if year_raw.len() == 4 {
        // European: D/M/YYYY
        let yr: i32 = year_raw.parse().ok()?;
        (yr, second, first)
    } else {
        // US: M/D/YY
        let yr: i32 = 2000 + year_raw.parse::<i32>().ok()?;
        (yr, first, second)
    };
    // Parse time: "H:MM AM/PM" (12-hour) or "H:MM" / "HH:MM" (24-hour).
    let time_str = time_str.trim();
    let (hour, minute) = if let Some((hm, ampm)) = time_str.split_once(' ') {
        let mut tp = hm.split(':');
        let mut h: u32 = tp.next()?.parse().ok()?;
        let m: u32 = tp.next()?.parse().ok()?;
        match ampm {
            "AM" => {
                if h == 12 {
                    h = 0;
                }
            }
            "PM" => {
                if h != 12 {
                    h += 12;
                }
            }
            _ => return None,
        }
        (h, m)
    } else {
        // 24-hour clock
        let mut tp = time_str.split(':');
        let h: u32 = tp.next()?.parse().ok()?;
        let m: u32 = tp.next()?.parse().ok()?;
        (h, m)
    };
    let naive = NaiveDate::from_ymd_opt(year, month, day)?
        .and_time(NaiveTime::from_hms_opt(hour, minute, 0)?);
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.timestamp())
}

/// Read a YouTube text node (`{simpleText}` or `{runs:[{text}]}`).
fn yt_render_title(v: Option<&Value>) -> String {
    let Some(t) = v else {
        return String::new();
    };
    if let Some(s) = t.get("simpleText").and_then(|s| s.as_str()) {
        return s.to_string();
    }
    if let Some(runs) = t.get("runs").and_then(|r| r.as_array()) {
        return runs
            .iter()
            .filter_map(|r| r.get("text").and_then(|s| s.as_str()))
            .collect();
    }
    String::new()
}

/// Parse the JSON object from a `marker = {…}` assignment in `body`, reading
/// exactly one value and ignoring the surrounding page (so a
/// `name = {…};</script>` blob parses cleanly). Used for YouTube's inline
/// `ytInitialPlayerResponse = {…}`.
///
/// Anchors to a genuine assignment of `marker` as a whole identifier: the match
/// must be a standalone token (the char before it is not a JS identifier char, so
/// `fooMARKER` is rejected) followed (whitespace-insensitively) by `=` then `{`.
/// This rejects unrelated mentions — an identifier *ending* in the marker, a
/// quoted key, a minified deref like `a.ytInitialPlayerResponse,…`, an HTML
/// attribute's stray `=`, or `marker = null` — instead of latching onto the next
/// brace anywhere on the page. Walks every occurrence, returning the first that
/// is a valid assignment whose object parses.
pub(crate) fn extract_json_after(body: &str, marker: &str) -> Option<Value> {
    use serde::Deserialize;
    let bytes = body.as_bytes();
    let mut start = 0;
    while let Some(rel) = body[start..].find(marker) {
        let pos = start + rel; // absolute start of this occurrence
        start = pos + marker.len(); // advance for the next iteration (strictly grows)
        // Left boundary: reject when the marker is a suffix of a longer
        // identifier (`fooMARKER = {…}`). A multibyte char before it has a
        // high byte that isn't in these ASCII ranges, so it reads as a boundary.
        if pos > 0
            && matches!(bytes[pos - 1], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$')
        {
            continue;
        }
        // Right boundary: a real `= {…}` assignment.
        let Some(after_eq) = body[start..].trim_start().strip_prefix('=') else {
            continue;
        };
        let obj = after_eq.trim_start();
        if !obj.starts_with('{') {
            continue;
        }
        // Read exactly one JSON value; trailing JS (`;</script>…`) is ignored.
        if let Ok(v) = Value::deserialize(&mut serde_json::Deserializer::from_str(obj)) {
            return Some(v);
        }
    }
    None
}

/// Extract a *live* YouTube stream's title + content category from a `/live`
/// watch page body. `Live` only for a genuinely-live watch page — the `/live`
/// URL can also resolve to the channel page, a finished VOD, or an upcoming
/// premiere, all of which still embed a player response with a title, so we
/// require `videoDetails.isLive == true` (anything else on a recognizable
/// YouTube page ⇒ `Offline`; an unrecognizable page ⇒ `Failed`). The title
/// comes from `videoDetails.title` (empty/absent ⇒ partial page ⇒ `Failed`,
/// not a blank title); `game` carries the broad content category (YouTube has
/// no public per-stream "game" field).
fn parse_youtube_meta(body: &str) -> MetaFetch {
    let Some(pr) = extract_json_after(body, "ytInitialPlayerResponse") else {
        // A channel with no active live serves a browse page from `/live`
        // (`ytInitialData`, no player object) — that's a normal "not live"
        // answer. A page with *neither* is a consent/bot interstitial, an
        // error page, or a layout change: a real failure.
        return if body.contains("ytInitialData") { MetaFetch::Offline } else { MetaFetch::Failed };
    };
    let details = &pr["videoDetails"];
    if details["isLive"].as_bool() != Some(true) {
        // `/live` keeps pointing at the just-ended VOD for a while after the
        // stream concludes — a player page that isn't live means offline.
        return MetaFetch::Offline;
    }
    // A live stream always has a non-empty title; an empty/absent one means a
    // degraded page — a failed read, not a real empty-title change.
    let Some(title) = details["title"].as_str().filter(|s| !s.is_empty()) else {
        return MetaFetch::Failed;
    };
    let game = pr["microformat"]["playerMicroformatRenderer"]["category"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    MetaFetch::Live(StreamMeta { title: title.to_string(), game, viewers: None, followers: None })
}

/// Extract a live Kick stream's title + category from the v2 channel JSON.
/// `Offline` when not live (no `livestream` object, or an explicit
/// `is_live: false`); `Failed` when live but the title can't be read.
fn parse_kick_meta(v: &Value) -> MetaFetch {
    let ls = &v["livestream"];
    if !ls.is_object() || ls["is_live"].as_bool() == Some(false) {
        return MetaFetch::Offline;
    }
    // Live but no readable title ⇒ partial response ⇒ a failed read, not a
    // real blank title.
    let Some(title) = ls["session_title"].as_str().filter(|s| !s.is_empty()) else {
        return MetaFetch::Failed;
    };
    MetaFetch::Live(StreamMeta {
        title: title.to_string(),
        // v2 exposes the category under `categories[0].name`.
        game: ls["categories"][0]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        viewers: ls["viewer_count"].as_i64().or_else(|| ls["viewers"].as_i64()),
        // Top-level channel field, not part of the livestream object.
        followers: v["followers_count"].as_i64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_twitch_login() {
        assert_eq!(
            twitch_login("https://twitch.tv/Foo").as_deref(),
            Some("foo")
        );
        assert_eq!(
            twitch_login("https://www.twitch.tv/foo/videos?x=1").as_deref(),
            Some("foo")
        );
        assert_eq!(twitch_login("https://twitch.tv/").as_deref(), None);
        assert_eq!(twitch_login("https://youtube.com/foo").as_deref(), None);
    }

    #[test]
    fn parse_kick_slug() {
        assert_eq!(kick_slug("https://kick.com/Bar/").as_deref(), Some("Bar"));
        assert_eq!(kick_slug("https://kick.com/").as_deref(), None);
    }

    #[test]
    fn community_url_uses_posts_path() {
        // YouTube retired the `/community` tab for `/posts`; `/community` now
        // returns a "This Community isn't available" placeholder (HTTP 200,
        // zero posts) instead of an error, so this must not silently regress.
        assert_eq!(
            youtube_community_url("https://www.youtube.com/@Foo"),
            "https://www.youtube.com/@Foo/posts"
        );
        assert_eq!(
            youtube_community_url("https://www.youtube.com/@Foo/"),
            "https://www.youtube.com/@Foo/posts"
        );
        assert_eq!(
            youtube_community_url("https://www.youtube.com/@Foo/live"),
            "https://www.youtube.com/@Foo/posts"
        );
        assert_eq!(
            youtube_community_url("https://www.youtube.com/@Foo/streams"),
            "https://www.youtube.com/@Foo/posts"
        );
        // Idempotent / self-healing against a URL already carrying either the
        // old or new suffix.
        assert_eq!(
            youtube_community_url("https://www.youtube.com/@Foo/community"),
            "https://www.youtube.com/@Foo/posts"
        );
        assert_eq!(
            youtube_community_url("https://www.youtube.com/@Foo/posts"),
            "https://www.youtube.com/@Foo/posts"
        );
    }

    #[test]
    fn normalize_yt_image_url_stabilizes_size_ladder() {
        // Two size variants of the same community image collapse to one stable
        // identity, so the combined-URL hash and the downloaded bytes no longer
        // churn between passes (the cause of spurious community re-OCR).
        let a = "https://yt3.ggpht.com/ABC123=s512-c-fcrop64=1,00000000ffffffff-rj";
        let b = "https://yt3.ggpht.com/ABC123=s1080-c-k-no";
        assert_eq!(normalize_yt_image_url(a), normalize_yt_image_url(b));
        assert_eq!(normalize_yt_image_url(a), "https://yt3.ggpht.com/ABC123=s1024");
        // googleusercontent host is handled too.
        assert_eq!(
            normalize_yt_image_url("https://lh3.googleusercontent.com/XYZ=s256"),
            "https://lh3.googleusercontent.com/XYZ=s1024"
        );
        // Non-Google URLs pass through untouched.
        let other = "https://example.com/banner.png";
        assert_eq!(normalize_yt_image_url(other), other);
    }

    #[test]
    fn detect_outcome_carries_viewers() {
        // Builder plumbs viewer count; the base constructors default it to None.
        let o = DetectOutcome::live(7, "live").with_stream_viewers(Some(1234));
        assert_eq!(o.stream_viewers, Some(1234));
        assert_eq!(DetectOutcome::offline(7).stream_viewers, None);
        assert_eq!(DetectOutcome::err(7, "x").stream_viewers, None);
    }

    #[test]
    fn kick_viewer_count_parses_from_value() {
        // Mirrors the Kick detect path's extraction (viewer_count, then viewers).
        let s = serde_json::json!({ "viewer_count": 87 });
        let got = s["viewer_count"].as_i64().or_else(|| s["viewers"].as_i64());
        assert_eq!(got, Some(87));
        let s2 = serde_json::json!({ "viewers": 42 });
        let got2 = s2["viewer_count"].as_i64().or_else(|| s2["viewers"].as_i64());
        assert_eq!(got2, Some(42));
        let s3 = serde_json::json!({ "is_live": true });
        assert_eq!(s3["viewer_count"].as_i64().or_else(|| s3["viewers"].as_i64()), None);
    }

    #[test]
    fn ocr_attempt_stamps_roundtrip_from_settings() {
        let store = Store::open_in_memory().unwrap();
        // Persisted as {"<monitor_id>:<source_id>": <unix_secs>}; the loader
        // splits the key back and drops anything malformed instead of panicking.
        store
            .set_setting(
                crate::models::K_OCR_LAST_ATTEMPT,
                r#"{"7:youtube_community_ocr":1700000000,"7:other_image_ocr":1700000600,"bad":1}"#,
            )
            .unwrap();
        let map = load_ocr_attempts(&store);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get(&(7, "youtube_community_ocr".to_string())),
            Some(&1700000000)
        );
        assert_eq!(
            map.get(&(7, "other_image_ocr".to_string())),
            Some(&1700000600)
        );
        // Empty/absent setting yields an empty map.
        let empty = Store::open_in_memory().unwrap();
        assert!(load_ocr_attempts(&empty).is_empty());
    }

    #[test]
    fn monitor_needles_are_host_qualified() {
        assert_eq!(
            monitor_needles("https://www.twitch.tv/Layna"),
            vec!["twitch.tv/layna".to_string()],
        );
        // YouTube is host-qualified only (no bare "@handle" needle).
        assert_eq!(
            monitor_needles("https://youtube.com/@Layna"),
            vec!["youtube.com/@layna".to_string()],
        );
    }

    #[test]
    fn token_match_respects_boundaries() {
        // A needle must match as a whole token, not a prefix of a longer name.
        assert!(text_contains_token(
            "join me at twitch.tv/ana tonight!",
            "twitch.tv/ana"
        ));
        assert!(!text_contains_token(
            "watch twitch.tv/anastasia live",
            "twitch.tv/ana"
        ));
        // `www.` prefix and trailing punctuation are fine.
        assert!(text_contains_token(
            "(https://www.twitch.tv/ana).",
            "twitch.tv/ana"
        ));
        // A bare @-mention must not match a host-qualified youtube needle.
        assert!(!text_contains_token("shoutout @ana!", "youtube.com/@ana"));
        assert!(text_contains_token(
            "live on youtube.com/@ana today",
            "youtube.com/@ana"
        ));
    }

    #[test]
    fn youtube_live_url_builds() {
        assert_eq!(
            youtube_live_url("https://youtube.com/@chan"),
            "https://youtube.com/@chan/live"
        );
        assert_eq!(
            youtube_live_url("https://youtube.com/@chan/live/"),
            "https://youtube.com/@chan/live"
        );
    }

    #[test]
    fn youtube_meta_parses_title_and_category() {
        // `&` is a JSON unicode escape that serde must decode to `&`; the
        // trailing `var other = 1;` JS after the object must be ignored.
        let body = r#"<script nonce="x">var ytInitialPlayerResponse = {"videoDetails":{"title":"Dev & Chill","isLive":true},"microformat":{"playerMicroformatRenderer":{"category":"Gaming"}}};var other = 1;</script>"#;
        let m = parse_youtube_meta(body).live().unwrap();
        assert_eq!(m.title, "Dev & Chill");
        assert_eq!(m.game, "Gaming");
    }

    #[test]
    fn youtube_meta_handles_minified_and_missing() {
        // Minified `name={…}` form, and a missing category -> empty game.
        let body = r#"ytInitialPlayerResponse={"videoDetails":{"title":"Solo","isLive":true}};"#;
        let m = parse_youtube_meta(body).live().unwrap();
        assert_eq!(m.title, "Solo");
        assert_eq!(m.game, "");
        // No player response AND no ytInitialData -> not a recognizable
        // YouTube page at all (consent/challenge/error) -> Failed.
        assert!(matches!(parse_youtube_meta("<html>nothing here</html>"), MetaFetch::Failed));
        // A browse page (`ytInitialData`, no player) is what /live serves when
        // the channel isn't live -> an authoritative Offline, not a failure.
        let browse = r#"<script>var ytInitialData = {"contents":{}};</script>"#;
        assert!(matches!(parse_youtube_meta(browse), MetaFetch::Offline));
    }

    #[test]
    fn youtube_meta_requires_live_and_real_assignment() {
        // A finished VOD or upcoming premiere still embeds a player response with
        // a title, but isn't live -> Offline (the stream-just-ended case).
        let vod = r#"var ytInitialPlayerResponse = {"videoDetails":{"title":"Old VOD","isLive":false}};"#;
        assert!(matches!(parse_youtube_meta(vod), MetaFetch::Offline));
        let upcoming = r#"ytInitialPlayerResponse = {"videoDetails":{"title":"Premiere Soon"}};"#;
        assert!(matches!(parse_youtube_meta(upcoming), MetaFetch::Offline));

        // A non-assignment mention (quoted key, then a stray `=` inside an href)
        // must not be latched onto; with no real assignment (and no
        // ytInitialData) present -> Failed.
        let decoy = r#"x"ytInitialPlayerResponse" rel="x" href="a=b" {"videoDetails":{"title":"WRONG","isLive":true}}"#;
        assert!(matches!(parse_youtube_meta(decoy), MetaFetch::Failed));

        // A decoy mention *before* the real assignment -> the real object wins.
        let mixed = r#"if(window.ytInitialPlayerResponse){} var ytInitialPlayerResponse = {"videoDetails":{"title":"Real","isLive":true}};"#;
        assert_eq!(parse_youtube_meta(mixed).live().unwrap().title, "Real");

        // A longer identifier *ending* in the marker (`fooMARKER = {…}`) must not
        // be latched onto; the real standalone assignment after it wins.
        let suffixed = r#"var preloadytInitialPlayerResponse = {"videoDetails":{"title":"DECOY","isLive":true}}; var ytInitialPlayerResponse = {"videoDetails":{"title":"Real","isLive":true}};"#;
        assert_eq!(parse_youtube_meta(suffixed).live().unwrap().title, "Real");
    }

    #[test]
    fn kick_meta_parses_title_and_category() {
        let v: Value = serde_json::from_str(
            r#"{"livestream":{"is_live":true,"session_title":"first stream","categories":[{"name":"Just Chatting"}],"viewer_count":42}}"#,
        )
        .unwrap();
        let m = parse_kick_meta(&v).live().unwrap();
        assert_eq!(m.title, "first stream");
        assert_eq!(m.game, "Just Chatting");
        assert_eq!(m.viewers, Some(42));
    }

    #[test]
    fn kick_meta_viewers_falls_back_to_viewers_field() {
        // Regression guard: while this session's meta-watcher fix mirrors the
        // detect-path's viewer_count/viewers fallback (see
        // `kick_viewer_count_parses_from_value`), keep both call sites in sync.
        let v: Value = serde_json::from_str(
            r#"{"livestream":{"is_live":true,"session_title":"x","viewers":7}}"#,
        )
        .unwrap();
        assert_eq!(parse_kick_meta(&v).live().unwrap().viewers, Some(7));

        let neither: Value =
            serde_json::from_str(r#"{"livestream":{"is_live":true,"session_title":"x"}}"#).unwrap();
        assert_eq!(parse_kick_meta(&neither).live().unwrap().viewers, None);
    }

    #[test]
    fn kick_meta_distinguishes_offline_from_partial() {
        let offline: Value = serde_json::from_str(r#"{"livestream":null}"#).unwrap();
        assert!(matches!(parse_kick_meta(&offline), MetaFetch::Offline));
        let not_live: Value =
            serde_json::from_str(r#"{"livestream":{"is_live":false,"session_title":"x"}}"#).unwrap();
        assert!(matches!(parse_kick_meta(&not_live), MetaFetch::Offline));
        // Live but no readable title -> a failed read (partial response),
        // not an offline answer and not a blank-title change.
        let no_title: Value =
            serde_json::from_str(r#"{"livestream":{"is_live":true,"session_title":""}}"#).unwrap();
        assert!(matches!(parse_kick_meta(&no_title), MetaFetch::Failed));
    }

    #[test]
    fn youtube_streams_url_builds() {
        assert_eq!(
            youtube_streams_url("https://youtube.com/@chan"),
            "https://youtube.com/@chan/streams"
        );
        // Normalizes a trailing /live or /streams.
        assert_eq!(
            youtube_streams_url("https://youtube.com/@chan/live"),
            "https://youtube.com/@chan/streams"
        );
        assert_eq!(
            youtube_streams_url("https://youtube.com/@chan/streams/"),
            "https://youtube.com/@chan/streams"
        );
    }

    #[test]
    fn youtube_schedule_parses_upcoming() {
        // Two upcoming videoRenderers (runs title + simpleText title), nested in
        // ytInitialData. Out of order on purpose; parse sorts by start_time.
        let body = r#"<script nonce="x">var ytInitialData = {"t":[{"videoRenderer":{"videoId":"b","title":{"simpleText":"Q&A"},"upcomingEventData":{"startTime":"1700003600"}}},{"videoRenderer":{"videoId":"a","title":{"runs":[{"text":"Big "},{"text":"Stream"}]},"upcomingEventData":{"startTime":"1700000000"}}}]};</script>"#;
        let segs = parse_youtube_schedule(body);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].start_time, 1_700_000_000);
        assert_eq!(segs[0].title, "Big Stream");
        assert_eq!(segs[1].start_time, 1_700_003_600);
        assert_eq!(segs[1].title, "Q&A");
        // A page with no upcoming events -> empty.
        assert!(parse_youtube_schedule("<html>no data here</html>").is_empty());
    }

    #[test]
    fn youtube_schedule_parses_lockup_viewmodel() {
        // New Polymer format: lockupViewModel with contentImage (thumbnail URL encodes video ID)
        // and lockupMetadataViewModel with "Scheduled for" row.
        let body = r#"<script>var ytInitialData = {"richGridRenderer":{"contents":[{"richItemRenderer":{"content":{"lockupViewModel":{"contentImage":{"thumbnailViewModel":{"image":{"sources":[{"url":"https://i.ytimg.com/vi/ABC123XYZ_0/hqdefault.jpg","width":320,"height":180}]}}},"overlays":[],"metadata":{"lockupMetadataViewModel":{"title":{"content":"Stream Title"},"metadata":{"contentMetadataViewModel":{"metadataRows":[{"metadataParts":[{"text":{"content":"123 waiting"}},{"text":{"content":"Scheduled for 6/22/26, 12:00 PM"}}]}]}}}}}}}}]}};</script>"#;
        let segs = parse_youtube_schedule(body);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].title, "Stream Title");
        assert_eq!(segs[0].video_id, Some("ABC123XYZ_0".to_string()));
        // The timestamp is parsed as local time so we can't assert an exact value,
        // but it must be within the correct calendar day (UTC ±14h of noon on 2026-06-22).
        let june22_noon_utc = 1_782_129_600i64; // 2026-06-22 12:00:00 UTC
        assert!(
            (segs[0].start_time - june22_noon_utc).abs() < 14 * 3600,
            "timestamp {} is more than 14h from 2026-06-22 12:00 UTC",
            segs[0].start_time
        );
        // Streams with no "Scheduled for" row are ignored.
        let no_sched = r#"<script>var ytInitialData = {"lockupMetadataViewModel":{"title":{"content":"Past Stream"},"metadata":{"contentMetadataViewModel":{"metadataRows":[{"metadataParts":[{"text":{"content":"1.2K views"}}]}]}}}};</script>"#;
        assert!(parse_youtube_schedule(no_sched).is_empty());
    }

    #[test]
    fn parse_yt_scheduled_text_cases() {
        // US format M/D/YY, 12-hour AM/PM (2-digit year).
        assert!(parse_yt_scheduled_text("Scheduled for 6/22/26, 12:00 PM").is_some());
        assert!(parse_yt_scheduled_text("Scheduled for 1/1/26, 12:00 AM").is_some());
        assert!(parse_yt_scheduled_text("Scheduled for 12/31/25, 11:59 PM").is_some());
        // European format D/M/YYYY (4-digit year), AM/PM or 24-hour.
        assert!(parse_yt_scheduled_text("Scheduled for 23/06/2026, 3:00 AM").is_some());
        assert!(parse_yt_scheduled_text("Scheduled for 1/1/2026, 12:00 AM").is_some());
        assert!(parse_yt_scheduled_text("Scheduled for 23/06/2026, 03:00").is_some());
        assert!(parse_yt_scheduled_text("Scheduled for 23/06/2026, 21:00").is_some());
        // Unrecognised strings return None.
        assert!(parse_yt_scheduled_text("1.2K views").is_none());
        assert!(parse_yt_scheduled_text("349 waiting").is_none());
        // Midnight: 12:00 AM should parse to hour 0, not 12.
        let midnight = parse_yt_scheduled_text("Scheduled for 6/22/26, 12:00 AM").unwrap();
        let noon = parse_yt_scheduled_text("Scheduled for 6/22/26, 12:00 PM").unwrap();
        assert!(noon > midnight, "noon ({noon}) should be after midnight ({midnight})");
        assert_eq!(noon - midnight, 12 * 3600, "noon and midnight should be 12h apart");
        // US and European format should yield the same timestamp for the same moment.
        let us = parse_yt_scheduled_text("Scheduled for 6/23/26, 3:00 AM").unwrap();
        let eu = parse_yt_scheduled_text("Scheduled for 23/6/2026, 3:00 AM").unwrap();
        assert_eq!(us, eu, "US and European formats should parse to the same timestamp");
        // European 24-hour matches 12-hour for the same time.
        let ampm = parse_yt_scheduled_text("Scheduled for 23/06/2026, 9:00 PM").unwrap();
        let h24 = parse_yt_scheduled_text("Scheduled for 23/06/2026, 21:00").unwrap();
        assert_eq!(ampm, h24, "12h and 24h formats should parse to the same timestamp");
    }

    #[test]
    fn fallback_scan_finds_upcoming_when_structured_misses() {
        // A body whose markers exist but NOT under a recognized renderer shape, so
        // `parse_youtube_schedule` (structured walk) returns nothing.
        let body = r#"junk {"videoId":"vid01234567","title":{"runs":[{"text":"Late "},{"text":"Night"}]},"foo":{"upcomingEventData":{"startTime":"1700000000"}}} more
            {"scheduledStartTime":"2026-06-18T18:00:00Z","videoId":"vidABCDEFGH","title":{"simpleText":"Morning Show"}}"#;
        // Structured parse misses (no videoRenderer / lockup wrapper).
        assert!(parse_youtube_schedule(body).is_empty());
        // Fallback recovers both, sorted by start.
        let mut out = Vec::new();
        collect_upcoming_fallback(body, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_time, 1_700_000_000);
        assert_eq!(out[0].title, "Late Night");
        assert_eq!(out[0].video_id.as_deref(), Some("vid01234567"));
        assert_eq!(out[1].start_time, 1_781_805_600); // 2026-06-18T18:00:00Z
        assert_eq!(out[1].title, "Morning Show");
        assert_eq!(out[1].video_id.as_deref(), Some("vidABCDEFGH"));
    }

    #[test]
    fn fallback_scan_uses_placeholder_title_when_none_nearby() {
        let body = r#"{"upcomingEventData":{"startTime":"1700000000"}}"#;
        let mut out = Vec::new();
        collect_upcoming_fallback(body, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Upcoming stream");
        assert_eq!(out[0].video_id, None);
    }

    fn seg(start: i64, title: &str, category: &str, canceled: bool) -> ScheduleSegment {
        ScheduleSegment {
            id: 0,
            monitor_id: 0,
            start_time: start,
            end_time: None,
            title: title.into(),
            category: category.into(),
            canceled,
            video_id: None,
            collab: String::new(),
        }
    }

    #[test]
    fn has_blank_title_ignores_canceled() {
        // A non-canceled blank trips it; a canceled blank does not.
        assert!(has_blank_title(&[seg(0, "", "", false)]));
        assert!(has_blank_title(&[seg(0, "   ", "", false)]));
        assert!(!has_blank_title(&[seg(0, "Real", "", false)]));
        assert!(!has_blank_title(&[seg(0, "", "", true)]));
        // Mixed: one blank among titled segments still trips it.
        assert!(has_blank_title(&[seg(0, "A", "", false), seg(100, "", "", false)]));
    }

    #[test]
    fn fill_titles_borrows_nearest_in_window() {
        // Base has two timed-but-blank events; donor carries titles + category.
        let mut base = vec![seg(1000, "", "", false), seg(10_000, "", "", false)];
        let donor = vec![
            seg(1500, "Morning Stream", "Just Chatting", false), // 500s from base[0]
            seg(9000, "Evening Stream", "Gaming", false),        // 1000s from base[1]
        ];
        fill_titles(&mut base, &donor, TITLE_FILL_WINDOW_SECS);
        assert_eq!(base[0].title, "Morning Stream");
        assert_eq!(base[0].category, "Just Chatting");
        assert_eq!(base[1].title, "Evening Stream");
        assert_eq!(base[1].category, "Gaming");
    }

    #[test]
    fn fill_titles_respects_window_and_keeps_existing() {
        // Donor too far away (beyond ±2h) -> left blank.
        let mut base = vec![seg(0, "", "", false)];
        let donor = vec![seg(TITLE_FILL_WINDOW_SECS + 1, "Too Far", "", false)];
        fill_titles(&mut base, &donor, TITLE_FILL_WINDOW_SECS);
        assert_eq!(base[0].title, "");

        // An existing title is never overwritten, but a blank category is still filled.
        let mut base = vec![seg(0, "Keep Me", "", false)];
        let donor = vec![seg(60, "Other", "Art", false)];
        fill_titles(&mut base, &donor, TITLE_FILL_WINDOW_SECS);
        assert_eq!(base[0].title, "Keep Me");
        assert_eq!(base[0].category, "");

        // Canceled base events are skipped even when a donor matches.
        let mut base = vec![seg(0, "", "", true)];
        let donor = vec![seg(10, "Donor", "Cat", false)];
        fill_titles(&mut base, &donor, TITLE_FILL_WINDOW_SECS);
        assert_eq!(base[0].title, "");

        // Blank donor titles are not used as fill sources.
        let mut base = vec![seg(0, "", "", false)];
        let donor = vec![seg(10, "  ", "", false)];
        fill_titles(&mut base, &donor, TITLE_FILL_WINDOW_SECS);
        assert_eq!(base[0].title, "");
    }

    #[test]
    fn fill_titles_picks_closest_of_several_donors() {
        // Two in-window donors: the nearer one wins.
        let mut base = vec![seg(5000, "", "", false)];
        let donor = vec![
            seg(3000, "Far", "", false),  // 2000s away
            seg(5400, "Near", "", false), // 400s away
        ];
        fill_titles(&mut base, &donor, TITLE_FILL_WINDOW_SECS);
        assert_eq!(base[0].title, "Near");
    }

    #[test]
    fn ytcfg_str_extracts_innertube_fields() {
        let body = r#"<script>ytcfg.set({"INNERTUBE_API_KEY":"AIzaSyTest123",
            "INNERTUBE_CONTEXT_CLIENT_VERSION":"2.20260701.01.00",
            "PATHY":"a\/b"});</script>"#;
        assert_eq!(
            ytcfg_str(body, "INNERTUBE_API_KEY").as_deref(),
            Some("AIzaSyTest123")
        );
        assert_eq!(
            ytcfg_str(body, "INNERTUBE_CONTEXT_CLIENT_VERSION").as_deref(),
            Some("2.20260701.01.00")
        );
        // Escaped slashes are unescaped (they appear in ytcfg URL values).
        assert_eq!(ytcfg_str(body, "PATHY").as_deref(), Some("a/b"));
        assert_eq!(ytcfg_str(body, "MISSING"), None);
        assert_eq!(ytcfg_str(r#"{"EMPTY":""}"#, "EMPTY"), None);
    }

    #[test]
    fn continuation_token_and_posts_parse_from_browse_response() {
        // The shape of an InnerTube browse continuation response: appended
        // items carry more posts + the next continuationItemRenderer.
        let resp = serde_json::json!({
            "onResponseReceivedEndpoints": [{
                "appendContinuationItemsAction": {
                    "continuationItems": [
                        { "backstagePostThreadRenderer": { "post": {
                            "backstagePostRenderer": {
                                "postId": "OLD_POST_1",
                                "publishedTimeText": { "runs": [{ "text": "2 years ago" }] },
                                "contentText": { "runs": [{ "text": "old news" }] }
                            }
                        }}},
                        { "continuationItemRenderer": {
                            "continuationEndpoint": {
                                "continuationCommand": { "token": "NEXT_TOKEN" }
                            }
                        }}
                    ]
                }
            }]
        });
        let mut posts = Vec::new();
        parse_community_posts(&resp, &mut posts);
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].post_id, "OLD_POST_1");
        assert_eq!(posts[0].published_text, "2 years ago");
        assert_eq!(posts[0].body_text, "old news");
        assert_eq!(find_continuation_token(&resp).as_deref(), Some("NEXT_TOKEN"));

        // Last page: no continuationItemRenderer → no token.
        let last = serde_json::json!({
            "onResponseReceivedEndpoints": [{
                "appendContinuationItemsAction": { "continuationItems": [
                    { "backstagePostThreadRenderer": { "post": {
                        "backstagePostRenderer": { "postId": "OLDEST" }
                    }}}
                ]}
            }]
        });
        assert_eq!(find_continuation_token(&last), None);
    }

    #[test]
    fn classify_channel_vs_viewer_posts() {
        let owner = "UCowner00000000000000000";
        let author_ep = |id: &str| {
            serde_json::json!({
                "profileCardCommand": { "profileOwnerExternalChannelId": id }
            })
        };

        // Owner highlight ⇒ channel, even without an id match.
        let own = serde_json::json!({
            "postId": "own1",
            "authorText": { "runs": [{ "text": "Streamer" }] },
            "authorEndpoint": author_ep(owner),
            "showPostAuthorBackgroundHighlight": { "lightThemeColor": 1 },
            "contentText": { "runs": [{ "text": "hi" }] }
        });
        let p = parse_one_post(&own, false).unwrap();
        assert!(p.is_owner_highlight);
        assert_eq!(p.author_channel_id, owner);
        assert_eq!(classify_author_kind(&p, owner), "channel");

        // No highlight + foreign id ⇒ viewer.
        let fan = serde_json::json!({
            "postId": "fan1",
            "authorText": { "runs": [{ "text": "A Fan" }] },
            "authorEndpoint": author_ep("UCfan0000000000000000000"),
            "contentText": { "runs": [{ "text": "hello there" }] }
        });
        let p = parse_one_post(&fan, false).unwrap();
        assert!(!p.is_owner_highlight);
        assert_eq!(classify_author_kind(&p, owner), "viewer");

        // Unknown owner id (find_uc failed) ⇒ conservative channel.
        assert_eq!(classify_author_kind(&p, ""), "channel");
    }

    #[test]
    fn parse_shared_post_captures_original() {
        let owner = "UCowner00000000000000000";
        let reshare = serde_json::json!({
            "sharedPostRenderer": {
                "postId": "re1",
                "displayName": { "runs": [{ "text": "Streamer" }] },
                "endpoint": {
                    "profileCardCommand": { "profileOwnerExternalChannelId": owner }
                },
                "content": { "runs": [{ "text": "check this out" }] },
                "originalPost": { "backstagePostRenderer": {
                    "postId": "orig1",
                    "authorText": { "runs": [{ "text": "Miniko Mew" }] },
                    "publishedTimeText": { "runs": [{ "text": "1 month ago" }] },
                    "contentText": { "runs": [{ "text": "the original" }] },
                    "backstageAttachment": { "backstageImageRenderer": {
                        "image": { "thumbnails": [{ "url": "https://img/orig.jpg" }] }
                    }}
                }}
            }
        });
        let mut posts = Vec::new();
        parse_community_posts(&reshare, &mut posts);
        assert_eq!(posts.len(), 1, "reshare parsed as one post, original not double-counted");
        let p = &posts[0];
        assert_eq!(p.post_id, "re1");
        assert_eq!(p.author, "Streamer");
        assert_eq!(p.body_text, "check this out");
        assert_eq!(p.author_channel_id, owner);
        assert!(p.image_urls.is_empty(), "reshare has no own attachment");
        assert_eq!(p.shared_image_urls, vec!["https://img/orig.jpg".to_string()]);

        let shared: Value = serde_json::from_str(&p.shared_json).unwrap();
        assert_eq!(shared["author"], "Miniko Mew");
        assert_eq!(shared["body_text"], "the original");
        assert_eq!(shared["published_text"], "1 month ago");

        // A channel resharing its own space still classifies as channel.
        assert_eq!(classify_author_kind(p, owner), "channel");
    }
}
