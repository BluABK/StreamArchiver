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
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::app_core::sleep_cancellable;
use crate::events::{AppEvent, EventTx};
use crate::models::{K_YT_API_DETECT, K_YT_API_SCHEDULE, Platform, ScheduleSegment, now_unix};
use crate::store::Store;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

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
    fn offline(monitor_id: i64) -> DetectOutcome {
        DetectOutcome {
            monitor_id,
            live: false,
            detail: String::new(),
            error: false,
            went_live_at: None,
            stream_id: None,
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
        }
    }
}

/// Parse an RFC3339/ISO8601 timestamp (e.g. Twitch `started_at`) to unix seconds.
fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
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
}

pub struct DetectContext {
    http: Client,
    pub store: Arc<Store>,
    twitch_token: Mutex<Option<TwitchToken>>,
    kick_token: Mutex<Option<TwitchToken>>,
    /// Serializes user-token refresh: Twitch device-code refresh tokens are
    /// one-time-use, so concurrent detection passes must not double-spend one.
    twitch_refresh: Mutex<()>,
}

impl DetectContext {
    pub fn new(store: Arc<Store>) -> DetectContext {
        let http = Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(20))
            .build()
            .expect("building reqwest client");
        DetectContext {
            http,
            store,
            twitch_token: Mutex::new(None),
            kick_token: Mutex::new(None),
            twitch_refresh: Mutex::new(()),
        }
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
    async fn twitch_user_id(&self, client_id: &str, token: &str, login: &str) -> Option<String> {
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
            #[serde(rename = "type")]
            kind: String,
            started_at: Option<String>,
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
                        // login -> (go-live time, stream id) for currently-live channels.
                        let live: HashMap<String, (Option<i64>, Option<String>)> =
                            match r.json::<StreamsResp>().await
                        {
                            Ok(sr) => sr
                                .data
                                .into_iter()
                                .filter(|s| s.kind == "live")
                                .map(|s| {
                                    let when = s.started_at.as_deref().and_then(parse_rfc3339);
                                    (s.user_login.to_lowercase(), (when, Some(s.id)))
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
                                    Some((went, id)) => DetectOutcome::live_at(*mid, "live", *went)
                                        .with_stream_id(id.clone()),
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
                        for l in chunk {
                            for mid in &login_to_mons[l] {
                                outcomes.push(DetectOutcome::err(*mid, e.to_string()));
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
    /// polls this directly). `None` when offline, on error, or when Twitch
    /// credentials aren't configured. Mirrors `detect_twitch`'s token handling:
    /// a connected user token if present, else the app token, with a one-shot
    /// app-token fallback on a 401.
    pub async fn twitch_stream_meta(&self, url: &str) -> Option<StreamMeta> {
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

        #[derive(Deserialize)]
        struct Stream {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            title: String,
            #[serde(default)]
            game_name: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            data: Vec<Stream>,
        }

        loop {
            let resp = self
                .http
                .get("https://api.twitch.tv/helix/streams")
                .header("Client-Id", &client_id)
                .bearer_auth(&token)
                .query(&[("user_login", login.as_str())])
                .send()
                .await
                .ok()?;
            match resp.status() {
                s if s.is_success() => {
                    let sr: Resp = resp.json().await.ok()?;
                    let s = sr.data.into_iter().find(|s| s.kind == "live")?;
                    return Some(StreamMeta {
                        title: s.title,
                        game: s.game_name,
                    });
                }
                reqwest::StatusCode::UNAUTHORIZED if using_user_token => {
                    token = self.twitch_app_token().await.ok()?;
                    using_user_token = false;
                    continue;
                }
                _ => return None,
            }
        }
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
        let streams_url = youtube_streams_url(url);
        let resp = self
            .http
            .get(&streams_url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        Some(parse_youtube_schedule(&body))
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
                let v: Value = r.json().await.unwrap_or_default();
                match v["items"][0]["id"]["videoId"].as_str() {
                    Some(vid) => {
                        let went = self.youtube_actual_start(vid, &key).await;
                        DetectOutcome::live_at(item.monitor_id, "live", went)
                            .with_stream_id(Some(vid.to_string()))
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
        let channel_id = self.youtube_channel_id(url, &key).await.ok()?;
        let resp = self
            .http
            .get("https://www.googleapis.com/youtube/v3/search")
            .query(&[
                ("part", "id"),
                ("channelId", channel_id.as_str()),
                ("eventType", "upcoming"),
                ("type", "video"),
                ("maxResults", "25"),
                ("key", key.as_str()),
            ])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        let ids: Vec<String> = v["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|it| it["id"]["videoId"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Some(Vec::new());
        }
        let ids_joined = ids.join(",");
        let resp = self
            .http
            .get("https://www.googleapis.com/youtube/v3/videos")
            .query(&[
                ("part", "snippet,liveStreamingDetails"),
                ("id", ids_joined.as_str()),
                ("key", key.as_str()),
            ])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        let mut segs: Vec<ScheduleSegment> = v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|it| {
                let start = it["liveStreamingDetails"]["scheduledStartTime"]
                    .as_str()
                    .and_then(parse_rfc3339)?;
                Some(ScheduleSegment {
                    id: 0,
                    monitor_id: 0,
                    start_time: start,
                    end_time: None,
                    title: it["snippet"]["title"].as_str().unwrap_or_default().to_string(),
                    category: String::new(),
                    canceled: false,
                })
            })
            .collect();
        segs.sort_by_key(|s| s.start_time);
        Some(segs)
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
                    DetectOutcome::live_at(item.monitor_id, "live", went).with_stream_id(id)
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
        let url = youtube_live_url(&item.url);
        let resp = self
            .http
            .get(&url)
            .header("Accept-Language", "en-US,en;q=0.9")
            // Bypass the EU consent interstitial that otherwise replaces the page.
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI")
            .send()
            .await;
        match resp {
            Ok(r) => {
                let body = r.text().await.unwrap_or_default();
                let live = body.contains("hqdefault_live")
                    || body.contains("\"isLive\":true")
                    || body.contains("\"isLiveNow\":true");
                if live {
                    DetectOutcome::live(item.monitor_id, "live")
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
    pub async fn youtube_stream_meta(&self, url: &str) -> Option<StreamMeta> {
        let live_url = youtube_live_url(url);
        let resp = self
            .http
            .get(&live_url)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().await.ok()?;
        parse_youtube_meta(&body)
    }

    async fn scrape_kick(&self, item: &DetectItem) -> DetectOutcome {
        let Some(slug) = kick_slug(&item.url) else {
            return DetectOutcome::err(item.monitor_id, "cannot parse kick slug");
        };
        let url = format!("https://kick.com/api/v2/channels/{slug}");
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                #[derive(Deserialize)]
                struct Livestream {
                    is_live: Option<bool>,
                }
                #[derive(Deserialize)]
                struct KickResp {
                    livestream: Option<Livestream>,
                }
                match r.json::<KickResp>().await {
                    Ok(k) => {
                        let live = k
                            .livestream
                            .map(|l| l.is_live.unwrap_or(true))
                            .unwrap_or(false);
                        if live {
                            DetectOutcome::live(item.monitor_id, "live")
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
    /// `None` when offline, on error, or behind a Cloudflare challenge. Always
    /// uses the v2 endpoint (even when Kick API credentials are configured), so
    /// metadata may be unavailable if v2 is Cloudflare-blocked while detection
    /// runs on the official API — detection and recording are unaffected.
    pub async fn kick_stream_meta(&self, url: &str) -> Option<StreamMeta> {
        let slug = kick_slug(url)?;
        let api = format!("https://kick.com/api/v2/channels/{slug}");
        let resp = self
            .http
            .get(&api)
            .header("Accept", "application/json")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        parse_kick_meta(&v)
    }

    // ----- generic probe via streamlink -----

    pub async fn detect_generic(&self, item: &DetectItem) -> DetectOutcome {
        let mut cmd = tokio::process::Command::new("streamlink");
        cmd.arg("--stream-url")
            .arg(&item.url)
            .arg("best")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return DetectOutcome::err(item.monitor_id, format!("spawn streamlink: {e}")),
        };
        match tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await {
            Ok(Ok(out)) => {
                let live = out.status.success() && !out.stdout.is_empty();
                debug!(monitor = item.monitor_id, live, "generic probe done");
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
pub async fn refresh_ad_free(ctx: Arc<DetectContext>, events: EventTx, shutdown: Arc<AtomicBool>) {
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
        if crate::oauth::connected_user_id(ctx.store.as_ref()).is_some() {
            refresh_ad_free_once(&ctx, &events, &shutdown, STALE_AFTER_SECS).await;
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

/// Background task: periodically refresh upcoming-stream schedules for enabled
/// Twitch/YouTube monitors. A short poll tick picks up newly-added monitors
/// quickly; each monitor is re-fetched at most every few hours (tracked
/// in-memory), and fetches are de-duplicated per URL within a pass.
pub async fn refresh_schedules(ctx: Arc<DetectContext>, events: EventTx, shutdown: Arc<AtomicBool>) {
    const INITIAL_DELAY_SECS: u64 = 30;
    const TICK_SECS: u64 = 60;
    const REFRESH_SECS: u64 = 6 * 3600;

    let mut last_fetched: HashMap<i64, Instant> = HashMap::new();
    sleep_cancellable(Duration::from_secs(INITIAL_DELAY_SECS), &shutdown).await;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        refresh_schedules_once(&ctx, &events, &shutdown, &mut last_fetched, REFRESH_SECS).await;
        sleep_cancellable(Duration::from_secs(TICK_SECS), &shutdown).await;
    }
}

/// One schedule-refresh pass: fetch + store the schedule for each enabled
/// Twitch/YouTube monitor due for a refresh. A failed fetch (`None`) is left
/// alone (so a transient error doesn't wipe a previously-stored schedule), and
/// retried next tick.
async fn refresh_schedules_once(
    ctx: &Arc<DetectContext>,
    events: &EventTx,
    shutdown: &Arc<AtomicBool>,
    last_fetched: &mut HashMap<i64, Instant>,
    refresh_secs: u64,
) {
    let rows = match ctx.store.list_monitors_with_channels() {
        Ok(r) => r,
        Err(_) => return,
    };
    // Drop staleness entries for monitors that no longer exist (avoid an unbounded
    // leak across the process lifetime as monitors are added/removed).
    let live: std::collections::HashSet<i64> = rows.iter().map(|r| r.monitor.id).collect();
    last_fetched.retain(|id, _| live.contains(id));
    let now = Instant::now();
    // Whether YouTube schedules use the Data API (Settings) — read once per pass.
    let yt_api_schedule = ctx.youtube_api_enabled(K_YT_API_SCHEDULE);
    // Per-URL fetch cache for this pass: the same channel under multiple instances
    // is fetched once. `None` = the fetch failed (don't overwrite stored data).
    let mut fetched: HashMap<String, Option<Vec<ScheduleSegment>>> = HashMap::new();
    let mut changed = false;
    for row in &rows {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        if !row.monitor.enabled {
            continue;
        }
        let platform = row.monitor.platform();
        if !matches!(platform, Platform::Twitch | Platform::YouTube) {
            continue;
        }
        // Re-fetch each monitor at most every `refresh_secs`.
        if let Some(t) = last_fetched.get(&row.monitor.id) {
            if now.duration_since(*t).as_secs() < refresh_secs {
                continue;
            }
        }
        let url = row.monitor.url.clone();
        let segs = match fetched.get(&url) {
            Some(s) => s.clone(),
            None => {
                let s = match platform {
                    Platform::Twitch => ctx.twitch_schedule(&url).await,
                    Platform::YouTube if yt_api_schedule => {
                        ctx.youtube_schedule_api(&url).await
                    }
                    Platform::YouTube => ctx.youtube_schedule(&url).await,
                    _ => None,
                };
                fetched.insert(url.clone(), s.clone());
                s
            }
        };
        if let Some(segs) = segs {
            if ctx.store.replace_schedule(row.monitor.id, &segs).is_ok() {
                changed = true;
            }
            last_fetched.insert(row.monitor.id, now);
        }
    }
    if changed {
        // Wake the UI to reload the Next stream column (monitor_id 0 = "any").
        let _ = events.send(AppEvent::MonitorState {
            monitor_id: 0,
            state: String::new(),
        });
    }
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
fn kick_slug(url: &str) -> Option<String> {
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
fn youtube_streams_url(url: &str) -> String {
    let t = url.trim().trim_end_matches('/');
    let t = t.strip_suffix("/live").or_else(|| t.strip_suffix("/streams")).unwrap_or(t);
    format!("{t}/streams")
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

/// Recursively collect objects carrying `upcomingEventData.startTime` (a
/// `videoRenderer` for an upcoming stream) into `out`.
fn collect_upcoming(v: &Value, out: &mut Vec<ScheduleSegment>) {
    match v {
        Value::Object(map) => {
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
fn extract_json_after(body: &str, marker: &str) -> Option<Value> {
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
/// watch page body. `None` unless the page is a genuinely-live watch page — the
/// `/live` URL can also resolve to the channel page, a finished VOD, or an
/// upcoming premiere, all of which still embed a player response with a title, so
/// we require `videoDetails.isLive == true`. The title comes from
/// `videoDetails.title` (empty/absent ⇒ partial page ⇒ `None`, not a blank
/// title); `game` carries the broad content category (YouTube has no public
/// per-stream "game" field).
fn parse_youtube_meta(body: &str) -> Option<StreamMeta> {
    let pr = extract_json_after(body, "ytInitialPlayerResponse")?;
    let details = &pr["videoDetails"];
    if details["isLive"].as_bool() != Some(true) {
        return None;
    }
    // A live stream always has a non-empty title; an empty/absent one means a
    // degraded page — skip rather than log a spurious empty-title change.
    let title = details["title"]
        .as_str()
        .filter(|s| !s.is_empty())?
        .to_string();
    let game = pr["microformat"]["playerMicroformatRenderer"]["category"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Some(StreamMeta { title, game })
}

/// Extract a live Kick stream's title + category from the v2 channel JSON. `None`
/// when offline (no `livestream` object, or an explicit `is_live: false`) or when
/// the title can't be read (empty/absent `session_title` ⇒ partial response ⇒
/// skip, rather than logging a blank title).
fn parse_kick_meta(v: &Value) -> Option<StreamMeta> {
    let ls = &v["livestream"];
    if !ls.is_object() || ls["is_live"].as_bool() == Some(false) {
        return None;
    }
    let title = ls["session_title"]
        .as_str()
        .filter(|s| !s.is_empty())?
        .to_string();
    Some(StreamMeta {
        title,
        // v2 exposes the category under `categories[0].name`.
        game: ls["categories"][0]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
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
        let m = parse_youtube_meta(body).unwrap();
        assert_eq!(m.title, "Dev & Chill");
        assert_eq!(m.game, "Gaming");
    }

    #[test]
    fn youtube_meta_handles_minified_and_missing() {
        // Minified `name={…}` form, and a missing category -> empty game.
        let body = r#"ytInitialPlayerResponse={"videoDetails":{"title":"Solo","isLive":true}};"#;
        let m = parse_youtube_meta(body).unwrap();
        assert_eq!(m.title, "Solo");
        assert_eq!(m.game, "");
        // No player response at all -> None (treated as offline).
        assert!(parse_youtube_meta("<html>nothing here</html>").is_none());
    }

    #[test]
    fn youtube_meta_requires_live_and_real_assignment() {
        // A finished VOD or upcoming premiere still embeds a player response with
        // a title, but isn't live -> None.
        let vod = r#"var ytInitialPlayerResponse = {"videoDetails":{"title":"Old VOD","isLive":false}};"#;
        assert!(parse_youtube_meta(vod).is_none());
        let upcoming = r#"ytInitialPlayerResponse = {"videoDetails":{"title":"Premiere Soon"}};"#;
        assert!(parse_youtube_meta(upcoming).is_none());

        // A non-assignment mention (quoted key, then a stray `=` inside an href)
        // must not be latched onto; with no real assignment present -> None.
        let decoy = r#"x"ytInitialPlayerResponse" rel="x" href="a=b" {"videoDetails":{"title":"WRONG","isLive":true}}"#;
        assert!(parse_youtube_meta(decoy).is_none());

        // A decoy mention *before* the real assignment -> the real object wins.
        let mixed = r#"if(window.ytInitialPlayerResponse){} var ytInitialPlayerResponse = {"videoDetails":{"title":"Real","isLive":true}};"#;
        assert_eq!(parse_youtube_meta(mixed).unwrap().title, "Real");

        // A longer identifier *ending* in the marker (`fooMARKER = {…}`) must not
        // be latched onto; the real standalone assignment after it wins.
        let suffixed = r#"var preloadytInitialPlayerResponse = {"videoDetails":{"title":"DECOY","isLive":true}}; var ytInitialPlayerResponse = {"videoDetails":{"title":"Real","isLive":true}};"#;
        assert_eq!(parse_youtube_meta(suffixed).unwrap().title, "Real");
    }

    #[test]
    fn kick_meta_parses_title_and_category() {
        let v: Value = serde_json::from_str(
            r#"{"livestream":{"is_live":true,"session_title":"first stream","categories":[{"name":"Just Chatting"}]}}"#,
        )
        .unwrap();
        let m = parse_kick_meta(&v).unwrap();
        assert_eq!(m.title, "first stream");
        assert_eq!(m.game, "Just Chatting");
    }

    #[test]
    fn kick_meta_none_when_offline() {
        let offline: Value = serde_json::from_str(r#"{"livestream":null}"#).unwrap();
        assert!(parse_kick_meta(&offline).is_none());
        let not_live: Value =
            serde_json::from_str(r#"{"livestream":{"is_live":false,"session_title":"x"}}"#).unwrap();
        assert!(parse_kick_meta(&not_live).is_none());
        // Live but no readable title -> None (partial response; don't log a blank).
        let no_title: Value =
            serde_json::from_str(r#"{"livestream":{"is_live":true,"session_title":""}}"#).unwrap();
        assert!(parse_kick_meta(&no_title).is_none());
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
}
