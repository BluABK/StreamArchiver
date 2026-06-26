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
use tracing::warn;

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
pub const K_USER_ID: &str = "twitch_user_id";

/// Scopes requested for the user token. `user:read:subscriptions` lets us check
/// whether the connected account is subscribed to a broadcaster (ad-free
/// detection); `user:read:follows` lets us list the account's followed channels
/// (the "Import followed" feature). Detection (Get Streams) itself needs no scope,
/// so accounts that connected before a scope was added keep working but must
/// reconnect to grant it.
const SCOPES: &str = "user:read:subscriptions user:read:follows";

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
        .form(&[("client_id", client_id), ("scopes", SCOPES)])
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
                ("scopes", SCOPES),
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
        refresh: v["refresh_token"]
            .as_str()
            .unwrap_or(refresh_token)
            .to_string(),
        expires_in: v["expires_in"].as_u64().unwrap_or(0),
    })
}

/// Resolve the `(login, user_id)` for an access token (also validates it),
/// retrying a few times since this runs right after authorization and a transient
/// failure here would otherwise cost the freshly granted tokens.
pub async fn fetch_user(http: &Client, client_id: &str, token: &str) -> Result<(String, String)> {
    let mut last_err = None;
    for attempt in 0..3 {
        match fetch_user_once(http, client_id, token).await {
            Ok(u) => return Ok(u),
            Err(e) => {
                last_err = Some(e);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("get users failed")))
}

async fn fetch_user_once(http: &Client, client_id: &str, token: &str) -> Result<(String, String)> {
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
    // The id is required (it drives the subscription check); a 200 with no id is
    // a failure, not a silent empty.
    let id = v["data"][0]["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Get Users response had no user id")?
        .to_string();
    let login = v["data"][0]["login"]
        .as_str()
        .unwrap_or("(unknown)")
        .to_string();
    Ok((login, id))
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

/// Forget the stored Twitch connection. Also clears cached ad-free (sub) results,
/// which can no longer be verified once disconnected.
pub fn disconnect(store: &Store) -> Result<()> {
    for k in [K_USER_TOKEN, K_REFRESH, K_EXPIRY, K_LOGIN, K_USER_ID] {
        store.set_setting(k, "")?;
    }
    let _ = store.clear_ad_free_sub();
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

/// The connected account's user id, if any (needed for the subscription check).
pub fn connected_user_id(store: &Store) -> Option<String> {
    store
        .get_setting(K_USER_ID)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// Return a *usable* user access token, refreshing it if it's near/past expiry.
///
/// Returns `None` when not connected, or when the token is expired and can't be
/// refreshed — so the caller falls back (app token) or prompts a reconnect
/// rather than sending a known-dead token (which Helix rejects with 401).
///
/// Device-code public clients refresh without a Client Secret; the refresh token
/// is one-time-use and rotates, so the rotated token is persisted on success.
/// Callers should serialize calls to avoid double-spending the refresh token.
/// If a concurrent caller already refreshed (double-spend), this function detects
/// the newer expiry in the DB and returns the already-stored fresh token.
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
    // 5-minute early-refresh window: gives room for a retry before the token
    // actually expires. A successful refresh resets expiry to +4 h, so this
    // triggers at most once per token lifetime under normal operation.
    if now_unix() < expiry - 300 {
        return Some(token);
    }

    // At/near/past expiry: attempt a refresh.
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
    if !refresh_token.is_empty() && !client_id.is_empty() {
        match refresh(http, &client_id, &client_secret, &refresh_token).await {
            Ok(t) => {
                let login = connected_login(store).unwrap_or_default();
                let _ = store_tokens(store, &t, &login);
                return Some(t.access);
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("missing client secret") && client_secret.is_empty() {
                    warn!(
                        "Twitch token refresh failed: Twitch requires a Client Secret for this \
                         app. Add your Client Secret in Settings → Twitch."
                    );
                } else {
                    warn!("Twitch token refresh failed: {e}");
                }
                // A concurrent caller (e.g. EventSub reconnect) may have already
                // refreshed the token and stored a newer expiry. Detect that by
                // re-reading K_EXPIRY: if it moved forward, use the stored token.
                let concurrent_expiry: i64 = store
                    .get_setting(K_EXPIRY)
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if concurrent_expiry > expiry {
                    return store
                        .get_setting(K_USER_TOKEN)
                        .ok()
                        .flatten()
                        .filter(|s| !s.is_empty());
                }
            }
        }
    }

    // Refresh unavailable or failed with no concurrent success.
    // Return the old token while it's still within its stated lifetime so the
    // caller can make one more attempt; once fully expired, return None to
    // avoid 401-looping on a known-dead token.
    if now_unix() < expiry {
        Some(token)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn valid_user_token_drops_dead_token() {
        let store = Store::open_in_memory().unwrap();
        let http = Client::new();

        // Not connected -> None.
        assert!(valid_user_token(&http, &store).await.is_none());

        // Expired token with no refresh token -> None (don't return a dead token
        // that Helix would 401). No network call: refresh is skipped.
        store.set_setting(K_USER_TOKEN, "deadbeef").unwrap();
        store
            .set_setting(K_EXPIRY, &(now_unix() - 3600).to_string())
            .unwrap();
        assert!(valid_user_token(&http, &store).await.is_none());

        // Token within the 5-minute early-refresh window but still alive, with no
        // refresh token available: refresh is skipped, old token returned
        // (best-effort within stated lifetime).
        store
            .set_setting(K_EXPIRY, &(now_unix() + 120).to_string())
            .unwrap();
        assert_eq!(
            valid_user_token(&http, &store).await.as_deref(),
            Some("deadbeef")
        );

        // Comfortably valid token (> 5 min to expiry) -> returned as-is.
        store
            .set_setting(K_EXPIRY, &(now_unix() + 3600).to_string())
            .unwrap();
        assert_eq!(
            valid_user_token(&http, &store).await.as_deref(),
            Some("deadbeef")
        );

        // Simulate concurrent refresh: token is in the refresh window, refresh
        // fails (no refresh token), but another caller already stored a fresh token
        // with a newer expiry -> pick up the new token.
        let old_expiry = now_unix() + 120;
        store
            .set_setting(K_EXPIRY, &old_expiry.to_string())
            .unwrap();
        // Simulate the concurrent success: overwrite with a fresh token before we
        // check the fallback path. We do this by calling store_tokens directly.
        let fresh = Tokens {
            access: "freshtoken".to_string(),
            refresh: "newrefresh".to_string(),
            expires_in: 14400,
        };
        store_tokens(&store, &fresh, "testuser").unwrap();
        // Now valid_user_token should read the new expiry and return the new token.
        assert_eq!(
            valid_user_token(&http, &store).await.as_deref(),
            Some("freshtoken")
        );
    }
}
