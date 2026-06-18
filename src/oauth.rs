//! Twitch OAuth2 **Device Code** flow ("Connect with Twitch").
//!
//! Device code is ideal for a desktop app: it needs only a Client ID (no secret,
//! no localhost redirect server). The user is shown a short code + URL, authorizes
//! in a browser, and we poll for the token. We store the access + refresh tokens
//! and auto-refresh them. The resulting user token can authenticate Helix
//! detection (so the Client Secret becomes optional).
//!
//! NOTE: live verification needs a Twitch app with device-code/public-client
//! enabled and an interactive authorization; use `--twitch-login`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::Value;

use crate::models::now_unix;
use crate::store::Store;

const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const USERS_URL: &str = "https://api.twitch.tv/helix/users";

// Settings keys for the stored connection.
pub const K_USER_TOKEN: &str = "twitch_user_token";
pub const K_REFRESH: &str = "twitch_refresh_token";
pub const K_EXPIRY: &str = "twitch_token_expiry";
pub const K_LOGIN: &str = "twitch_user_login";

/// Live state of an interactive connect flow (for the UI to render).
#[derive(Clone, Debug)]
pub enum AuthFlow {
    Idle,
    Pending { user_code: String, url: String },
    Connected { login: String },
    Failed { message: String },
}

pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Request a device code to begin authorization.
pub async fn start_device(http: &Client, client_id: &str) -> Result<DeviceCode> {
    let resp = http
        .post(DEVICE_URL)
        .form(&[("client_id", client_id), ("scopes", "")])
        .send()
        .await?;
    if !resp.status().is_success() {
        let s = resp.status();
        bail!(
            "device code request failed: {s} {}",
            resp.text().await.unwrap_or_default()
        );
    }
    let v: Value = resp.json().await?;
    Ok(DeviceCode {
        device_code: v["device_code"]
            .as_str()
            .context("no device_code")?
            .to_string(),
        user_code: v["user_code"].as_str().context("no user_code")?.to_string(),
        verification_uri: v["verification_uri"]
            .as_str()
            .or_else(|| v["verification_uri_complete"].as_str())
            .context("no verification_uri")?
            .to_string(),
        interval: v["interval"].as_u64().unwrap_or(5).max(1),
        expires_in: v["expires_in"].as_u64().unwrap_or(1800),
    })
}

pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub expires_in: u64,
}

/// Poll the token endpoint until the user authorizes (or it expires).
pub async fn poll_token(http: &Client, client_id: &str, dc: &DeviceCode) -> Result<Tokens> {
    let mut wait = dc.interval;
    let mut elapsed = 0u64;
    loop {
        tokio::time::sleep(Duration::from_secs(wait)).await;
        elapsed += wait;
        if elapsed > dc.expires_in {
            bail!("authorization timed out");
        }
        let resp = http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", client_id),
                ("scopes", ""),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(Tokens {
                access: v["access_token"]
                    .as_str()
                    .context("no access_token")?
                    .to_string(),
                refresh: v["refresh_token"].as_str().unwrap_or_default().to_string(),
                expires_in: v["expires_in"].as_u64().unwrap_or(0),
            });
        }
        let msg = v["message"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or("");
        if msg.contains("authorization_pending") || msg.contains("pending") {
            continue;
        }
        if msg.contains("slow_down") {
            wait += 5;
            continue;
        }
        bail!("authorization failed: {status} {msg}");
    }
}

/// Exchange a refresh token for a fresh access token.
pub async fn refresh(
    http: &Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret));
    }
    let resp = http.post(TOKEN_URL).form(&form).send().await?;
    if !resp.status().is_success() {
        bail!("token refresh failed: {}", resp.status());
    }
    let v: Value = resp.json().await?;
    Ok(Tokens {
        access: v["access_token"]
            .as_str()
            .context("no access_token")?
            .to_string(),
        refresh: v["refresh_token"]
            .as_str()
            .unwrap_or(refresh_token)
            .to_string(),
        expires_in: v["expires_in"].as_u64().unwrap_or(0),
    })
}

/// Resolve the login name for an access token (also validates it).
pub async fn fetch_login(http: &Client, client_id: &str, token: &str) -> Result<String> {
    let resp = http
        .get(USERS_URL)
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("get users failed: {}", resp.status());
    }
    let v: Value = resp.json().await?;
    Ok(v["data"][0]["login"]
        .as_str()
        .unwrap_or("(unknown)")
        .to_string())
}

/// Persist a freshly obtained set of tokens (+ resolved login).
pub fn store_tokens(store: &Store, tokens: &Tokens, login: &str) -> Result<()> {
    store.set_setting(K_USER_TOKEN, &tokens.access)?;
    store.set_setting(K_REFRESH, &tokens.refresh)?;
    let expiry = now_unix() + tokens.expires_in as i64;
    store.set_setting(K_EXPIRY, &expiry.to_string())?;
    store.set_setting(K_LOGIN, login)?;
    Ok(())
}

/// Forget the stored Twitch connection.
pub fn disconnect(store: &Store) -> Result<()> {
    for k in [K_USER_TOKEN, K_REFRESH, K_EXPIRY, K_LOGIN] {
        store.set_setting(k, "")?;
    }
    Ok(())
}

/// The connected login name, if any.
pub fn connected_login(store: &Store) -> Option<String> {
    store
        .get_setting(K_LOGIN)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// Return a valid user access token, refreshing it if it's near expiry. Returns
/// `None` if not connected (or refresh failed with no usable token).
pub async fn valid_user_token(http: &Client, store: &Store) -> Option<String> {
    let token = store.get_setting(K_USER_TOKEN).ok().flatten()?;
    if token.is_empty() {
        return None;
    }
    let expiry: i64 = store
        .get_setting(K_EXPIRY)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if now_unix() < expiry - 60 {
        return Some(token);
    }
    // Near/!past expiry — try to refresh.
    let refresh_token = store
        .get_setting(K_REFRESH)
        .ok()
        .flatten()
        .unwrap_or_default();
    let client_id = store
        .get_setting("twitch_client_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    let client_secret = store
        .get_setting("twitch_client_secret")
        .ok()
        .flatten()
        .unwrap_or_default();
    if refresh_token.is_empty() || client_id.is_empty() {
        return Some(token); // best effort with the (possibly stale) token
    }
    match refresh(http, &client_id, &client_secret, &refresh_token).await {
        Ok(t) => {
            let login = connected_login(store).unwrap_or_default();
            let _ = store_tokens(store, &t, &login);
            Some(t.access)
        }
        Err(_) => Some(token),
    }
}
