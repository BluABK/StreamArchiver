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
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::debug;

use crate::models::Platform;
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
}

impl DetectOutcome {
    fn live(monitor_id: i64, detail: impl Into<String>) -> DetectOutcome {
        DetectOutcome {
            monitor_id,
            live: true,
            detail: detail.into(),
            error: false,
            went_live_at: None,
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
    fn offline(monitor_id: i64) -> DetectOutcome {
        DetectOutcome {
            monitor_id,
            live: false,
            detail: String::new(),
            error: false,
            went_live_at: None,
        }
    }
    fn err(monitor_id: i64, detail: impl Into<String>) -> DetectOutcome {
        DetectOutcome {
            monitor_id,
            live: false,
            detail: detail.into(),
            error: true,
            went_live_at: None,
        }
    }
}

/// Parse an RFC3339/ISO8601 timestamp (e.g. Twitch `started_at`) to unix seconds.
fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

struct TwitchToken {
    access_token: String,
    expires_at: Instant,
}

/// Shared detection state: one HTTP client + cached Twitch app token.
pub struct DetectContext {
    http: Client,
    pub store: Arc<Store>,
    twitch_token: Mutex<Option<TwitchToken>>,
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

    pub async fn detect_twitch(&self, items: &[DetectItem]) -> Vec<DetectOutcome> {
        let token = match self.twitch_app_token().await {
            Ok(t) => t,
            Err(e) => {
                return items
                    .iter()
                    .map(|it| DetectOutcome::err(it.monitor_id, e.to_string()))
                    .collect();
            }
        };
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

        let logins: Vec<String> = login_to_mons.keys().cloned().collect();
        for chunk in logins.chunks(100) {
            let query: Vec<(&str, &str)> =
                chunk.iter().map(|l| ("user_login", l.as_str())).collect();
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
                    #[derive(Deserialize)]
                    struct Stream {
                        user_login: String,
                        #[serde(rename = "type")]
                        kind: String,
                        started_at: Option<String>,
                    }
                    #[derive(Deserialize)]
                    struct StreamsResp {
                        data: Vec<Stream>,
                    }
                    // login -> go-live time (unix), for currently-live channels.
                    let live: HashMap<String, Option<i64>> = match r.json::<StreamsResp>().await {
                        Ok(sr) => sr
                            .data
                            .into_iter()
                            .filter(|s| s.kind == "live")
                            .map(|s| {
                                let when = s.started_at.as_deref().and_then(parse_rfc3339);
                                (s.user_login.to_lowercase(), when)
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
                            continue;
                        }
                    };
                    for l in chunk {
                        let key = l.to_lowercase();
                        for mid in &login_to_mons[l] {
                            outcomes.push(match live.get(&key) {
                                Some(went) => DetectOutcome::live_at(*mid, "live", *went),
                                None => DetectOutcome::offline(*mid),
                            });
                        }
                    }
                }
                Ok(r) => {
                    let status = r.status();
                    for l in chunk {
                        for mid in &login_to_mons[l] {
                            outcomes.push(DetectOutcome::err(*mid, format!("helix {status}")));
                        }
                    }
                }
                Err(e) => {
                    for l in chunk {
                        for mid in &login_to_mons[l] {
                            outcomes.push(DetectOutcome::err(*mid, e.to_string()));
                        }
                    }
                }
            }
        }
        outcomes
    }

    // ----- scrape (no credentials) -----

    pub async fn detect_scrape(&self, item: &DetectItem) -> DetectOutcome {
        match item.platform {
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

/// Extract the Twitch login from a channel URL (`twitch.tv/<login>`).
fn twitch_login(url: &str) -> Option<String> {
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

/// Build the YouTube live URL for a channel URL (append `/live`).
fn youtube_live_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.to_lowercase().ends_with("/live") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/live")
    }
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
}
