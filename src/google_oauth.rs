//! Google OAuth2 **Device Code** flow ("Connect YouTube").
//!
//! Mirrors the Twitch device flow in [`crate::oauth`], but for a Google account so
//! we can read the user's YouTube subscriptions (`youtube.readonly`). Device code
//! suits a desktop app: the user is shown a short code + URL, authorizes in a
//! browser, and we poll for the token. Unlike Twitch, Google's device flow needs
//! both a Client ID **and** a Client Secret (a "TV and Limited Input devices" OAuth
//! client created in Google Cloud Console); refresh keeps the same refresh token.
//!
//! NOTE: live verification needs a Google Cloud OAuth client with the device flow
//! enabled and the YouTube Data API turned on for the project.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::Value;
use tracing::warn;

use crate::models::now_unix;
use crate::oauth::{DeviceCode, Tokens};
use crate::store::Store;

const DEVICE_URL: &str = "https://oauth2.googleapis.com/device/code";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CHANNELS_URL: &str = "https://www.googleapis.com/youtube/v3/channels";

/// Read-only access to the user's YouTube account (subscriptions, channel info).
const SCOPE: &str = "https://www.googleapis.com/auth/youtube.readonly";

// Settings keys for the stored Google connection + credentials.
pub const K_CLIENT_ID: &str = "google_client_id";
pub const K_CLIENT_SECRET: &str = "google_client_secret";
pub const K_USER_TOKEN: &str = "google_user_token";
pub const K_REFRESH: &str = "google_refresh_token";
pub const K_EXPIRY: &str = "google_token_expiry";
/// Display identity (the connected account's YouTube channel title), best-effort.
pub const K_IDENTITY: &str = "google_user_identity";

/// Request a device code to begin authorization.
pub async fn start_device(http: &Client, client_id: &str) -> Result<DeviceCode> {
    let resp = http
        .post(DEVICE_URL)
        .form(&[("client_id", client_id), ("scope", SCOPE)])
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
        // Google returns `verification_url` (Twitch uses `verification_uri`).
        verification_uri: v["verification_url"]
            .as_str()
            .or_else(|| v["verification_uri"].as_str())
            .context("no verification_url")?
            .to_string(),
        interval: v["interval"].as_u64().unwrap_or(5).max(1),
        expires_in: v["expires_in"].as_u64().unwrap_or(1800),
    })
}

/// Poll the token endpoint until the user authorizes (or it expires). Google needs
/// the Client Secret here in addition to the Client ID.
pub async fn poll_token(
    http: &Client,
    client_id: &str,
    client_secret: &str,
    dc: &DeviceCode,
) -> Result<Tokens> {
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
                ("client_secret", client_secret),
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
                // Google access tokens last 1 h; default to that if the field is
                // ever missing so the token isn't treated as instantly expired.
                expires_in: v["expires_in"].as_u64().unwrap_or(3600),
            });
        }
        // Google signals flow state via the JSON `error` field, not the HTTP code.
        let err = v["error"].as_str().unwrap_or("");
        if err == "authorization_pending" {
            continue;
        }
        if err == "slow_down" {
            wait += 5;
            continue;
        }
        let desc = v["error_description"].as_str().unwrap_or(err);
        bail!("authorization failed: {status} {desc}");
    }
}

/// Exchange a refresh token for a fresh access token. Google does not rotate the
/// refresh token, so the caller keeps the existing one.
pub async fn refresh(
    http: &Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token refresh failed: {status} — {body}");
    }
    let v: Value = resp.json().await?;
    Ok(Tokens {
        access: v["access_token"]
            .as_str()
            .context("no access_token")?
            .to_string(),
        // Google omits refresh_token on refresh — keep the existing one.
        refresh: v["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(refresh_token)
            .to_string(),
        expires_in: v["expires_in"].as_u64().unwrap_or(3600),
    })
}

/// Best-effort display name: the connected account's own YouTube channel title.
/// Returns `None` (and the connection is still kept) when it can't be resolved.
pub async fn fetch_identity(http: &Client, token: &str) -> Option<String> {
    let resp = http
        .get(CHANNELS_URL)
        .query(&[("part", "snippet"), ("mine", "true")])
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v["items"][0]["snippet"]["title"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Persist a freshly obtained set of tokens.
pub fn store_tokens(store: &Store, tokens: &Tokens) -> Result<()> {
    store.set_setting(K_USER_TOKEN, &tokens.access)?;
    if !tokens.refresh.is_empty() {
        store.set_setting(K_REFRESH, &tokens.refresh)?;
    } else if store
        .get_setting(K_REFRESH)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        // No refresh token now and none stored before — the access token will
        // expire in ~1 h with no way to renew. Google only re-issues a refresh
        // token on fresh consent, so a reconnect after revoking access is needed.
        warn!(
            "Google returned no refresh token; the YouTube connection will expire in ~1h. \
             Revoke this app under your Google account's third-party access, then reconnect."
        );
    }
    let expiry = now_unix() + tokens.expires_in as i64;
    store.set_setting(K_EXPIRY, &expiry.to_string())?;
    Ok(())
}

/// Forget the stored Google connection.
pub fn disconnect(store: &Store) -> Result<()> {
    for k in [K_USER_TOKEN, K_REFRESH, K_EXPIRY, K_IDENTITY] {
        store.set_setting(k, "")?;
    }
    Ok(())
}

/// The connected account's display identity, if any.
pub fn connected_identity(store: &Store) -> Option<String> {
    store
        .get_setting(K_IDENTITY)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// Whether a Google account is connected (a user token is stored).
pub fn is_connected(store: &Store) -> bool {
    store
        .get_setting(K_USER_TOKEN)
        .ok()
        .flatten()
        .is_some_and(|s| !s.is_empty())
}

/// Return a *usable* user access token, refreshing it if it's near/past expiry.
/// `None` when not connected or the token is dead and can't be refreshed.
pub async fn valid_token(http: &Client, store: &Store) -> Option<String> {
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
    if now_unix() < expiry - 300 {
        return Some(token);
    }
    let refresh_token = store.get_setting(K_REFRESH).ok().flatten().unwrap_or_default();
    let client_id = store.get_setting(K_CLIENT_ID).ok().flatten().unwrap_or_default();
    let client_secret = store
        .get_setting(K_CLIENT_SECRET)
        .ok()
        .flatten()
        .unwrap_or_default();
    if !refresh_token.is_empty() && !client_id.is_empty() && !client_secret.is_empty() {
        match refresh(http, &client_id, &client_secret, &refresh_token).await {
            Ok(t) => {
                let _ = store_tokens(store, &t);
                return Some(t.access);
            }
            Err(e) => warn!("Google token refresh failed: {e}"),
        }
    } else if !refresh_token.is_empty() {
        warn!(
            "Google token refresh skipped: add your Google Client ID + Secret in \
             Settings → YouTube account (Google OAuth)."
        );
    }
    // Return the old token while it's still within its stated lifetime so the
    // caller can make one more attempt; once fully expired, return None.
    if now_unix() < expiry {
        Some(token)
    } else {
        None
    }
}
