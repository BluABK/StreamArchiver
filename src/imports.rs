//! Bulk-import channels a user already follows/subscribes to, from each platform.
//!
//! * **Twitch** — the connected account's *followed* channels (Helix
//!   `channels/followed`, needs the `user:read:follows` scope).
//! * **YouTube** — the connected Google account's *subscriptions* (YouTube Data
//!   API `subscriptions.list?mine=true`, needs a connected Google account via
//!   [`crate::google_oauth`]).
//!
//! Each returns a flat list of [`ImportCandidate`]s; the UI shows a confirmation
//! dialog (pick which, set Auto / Disabled) and then calls [`create_monitor`] per
//! chosen entry, reusing the exact same per-platform defaults a manual "Add
//! stream" would.

use anyhow::{Result, bail};
use reqwest::Client;
use tracing::warn;

use crate::models::{AuthKind, Monitor, MonitorDefaults, Platform};
use crate::store::Store;
use crate::{google_oauth, oauth};

/// Hard cap on imported pages, so a huge follow/subscription list can't loop
/// unboundedly (100/page Twitch, 50/page YouTube → up to a few thousand).
const MAX_PAGES: usize = 60;

/// One channel offered for import, with enough detail for the confirm dialog.
#[derive(Clone)]
pub struct ImportCandidate {
    pub platform: Platform,
    /// Display name (Twitch display name / YouTube channel title).
    pub name: String,
    /// Stable identity for dedup against existing monitors (lowercased login /
    /// channel id).
    pub identity: String,
    /// Platform id (Twitch broadcaster id / YouTube channel id) — shown in the UI.
    pub id: String,
    /// Canonical channel URL to store on the monitor.
    pub url: String,
    /// Extra one-line detail for the dialog (e.g. "followed 2021-05-03").
    pub detail: String,
}

/// The connected Twitch account's followed channels, sorted by name.
pub async fn twitch_followed(http: &Client, store: &Store) -> Result<Vec<ImportCandidate>> {
    let client_id = store
        .get_setting("twitch_client_id")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Set a Twitch Client ID in Settings first."))?;
    let token = oauth::valid_user_token(http, store)
        .await
        .ok_or_else(|| anyhow::anyhow!("Connect your Twitch account in Settings first."))?;
    // Older connections may predate user-id persistence — resolve it on demand
    // (Get Users) and cache it, so the import works without a reconnect.
    let user_id = match oauth::connected_user_id(store) {
        Some(id) => id,
        None => match oauth::fetch_user(http, &client_id, &token).await {
            Ok((_login, id)) => {
                let _ = store.set_setting(oauth::K_USER_ID, &id);
                id
            }
            Err(_) => bail!("Reconnect your Twitch account in Settings to enable importing."),
        },
    };

    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    let mut truncated = true;
    for _ in 0..MAX_PAGES {
        let mut query: Vec<(&str, &str)> = vec![("user_id", user_id.as_str()), ("first", "100")];
        if let Some(c) = &cursor {
            query.push(("after", c.as_str()));
        }
        let resp = http
            .get("https://api.twitch.tv/helix/channels/followed")
            .header("Client-Id", &client_id)
            .bearer_auth(&token)
            .query(&query)
            .send()
            .await?;
        if !resp.status().is_success() {
            let s = resp.status();
            // 401 here usually means the older token lacks user:read:follows.
            if s == reqwest::StatusCode::UNAUTHORIZED {
                bail!(
                    "Twitch rejected the request ({s}). Reconnect your Twitch account to grant \
                     the 'follows' permission."
                );
            }
            bail!("Twitch followed channels failed: {s}");
        }
        let v: serde_json::Value = resp.json().await?;
        for item in v["data"].as_array().into_iter().flatten() {
            let login = item["broadcaster_login"].as_str().unwrap_or("");
            if login.is_empty() {
                continue;
            }
            let name = item["broadcaster_name"].as_str().unwrap_or(login);
            let id = item["broadcaster_id"].as_str().unwrap_or("");
            let followed = item["followed_at"]
                .as_str()
                .unwrap_or("")
                .split('T')
                .next()
                .unwrap_or("");
            out.push(ImportCandidate {
                platform: Platform::Twitch,
                name: name.to_string(),
                identity: login.to_lowercase(),
                id: id.to_string(),
                url: format!("https://twitch.tv/{login}"),
                detail: if followed.is_empty() {
                    String::new()
                } else {
                    format!("followed {followed}")
                },
            });
        }
        cursor = v["pagination"]["cursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if cursor.is_none() {
            truncated = false;
            break;
        }
    }
    if truncated {
        warn!("Twitch followed list exceeded {MAX_PAGES} pages; import list may be incomplete.");
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// The connected Google account's YouTube subscriptions, sorted by name.
pub async fn youtube_subscriptions(http: &Client, store: &Store) -> Result<Vec<ImportCandidate>> {
    let token = google_oauth::valid_token(http, store)
        .await
        .ok_or_else(|| anyhow::anyhow!("Connect your YouTube (Google) account in Settings first."))?;

    let mut out = Vec::new();
    let mut page: Option<String> = None;
    let mut truncated = true;
    for _ in 0..MAX_PAGES {
        let mut query: Vec<(&str, &str)> = vec![
            ("part", "snippet"),
            ("mine", "true"),
            ("maxResults", "50"),
            ("order", "alphabetical"),
        ];
        if let Some(p) = &page {
            query.push(("pageToken", p.as_str()));
        }
        let resp = http
            .get("https://www.googleapis.com/youtube/v3/subscriptions")
            .bearer_auth(&token)
            .query(&query)
            .send()
            .await?;
        if !resp.status().is_success() {
            let s = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("YouTube subscriptions failed: {s} — {body}");
        }
        let v: serde_json::Value = resp.json().await?;
        for item in v["items"].as_array().into_iter().flatten() {
            let snip = &item["snippet"];
            let channel_id = snip["resourceId"]["channelId"].as_str().unwrap_or("");
            if channel_id.is_empty() {
                continue;
            }
            let title = snip["title"].as_str().unwrap_or(channel_id);
            out.push(ImportCandidate {
                platform: Platform::YouTube,
                name: title.to_string(),
                identity: channel_id.to_lowercase(),
                id: channel_id.to_string(),
                url: format!("https://www.youtube.com/channel/{channel_id}"),
                detail: String::new(),
            });
        }
        page = v["nextPageToken"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if page.is_none() {
            truncated = false;
            break;
        }
    }
    if truncated {
        warn!("YouTube subscriptions exceeded {MAX_PAGES} pages; import list may be incomplete.");
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Create one channel container + its first monitor for an imported URL, reusing
/// the same per-platform defaults a manual "Add stream" would (max-archival
/// booleans on). `monitor_enabled` is the "Auto" choice (whether the scheduler
/// auto-records it); `channel_enabled` is the container on/off (a "Disabled"
/// import sets it false). Returns the new channel id.
pub fn create_monitor(
    store: &Store,
    defaults: &MonitorDefaults,
    default_out: &str,
    name: &str,
    url: &str,
    monitor_enabled: bool,
    channel_enabled: bool,
) -> Result<i64> {
    let platform = Platform::detect(url);
    let channel_id = store.create_container(name)?;
    let monitor = Monitor {
        id: 0,
        channel_id,
        url: url.to_string(),
        enabled: monitor_enabled,
        tool: defaults.resolve_tool(platform),
        detection_method: defaults.resolve_detection(platform),
        poll_interval_secs: defaults.resolve_poll_interval(platform).max(5),
        quality: defaults.resolve_quality(platform),
        output_dir: defaults.resolve_output_dir(platform, default_out),
        filename_template: defaults.resolve_filename_template(platform),
        container: defaults.resolve_container(platform),
        capture_from_start: defaults.resolve_from_start(platform),
        dual_capture: false,
        ad_free: false,
        auth_kind: AuthKind::Inherit,
        auth_value: String::new(),
        audio_tracks: "all".into(),
        subtitle_tracks: "all".into(),
        chat_log: true,
        fetch_thumbnail: true,
        thumbnail_in_toast: false,
        fetch_chat_assets: true,
        extra_args: String::new(),
        max_concurrent: 1,
        last_checked_at: None,
        last_state: "idle".into(),
    };
    store.insert_monitor(&monitor)?;
    if !channel_enabled {
        store.set_channel_enabled(channel_id, false)?;
    }
    Ok(channel_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_monitor_sets_flags_and_platform() {
        let store = Store::open_in_memory().unwrap();
        let defaults = MonitorDefaults::default();

        // Auto off (monitor disabled), not "Disabled" (channel on) → info-only.
        let cid = create_monitor(
            &store,
            &defaults,
            "C:/out",
            "Cool",
            "https://twitch.tv/cool",
            false,
            true,
        )
        .unwrap();
        let rows = store.list_monitors_with_channels().unwrap();
        let row = rows.iter().find(|r| r.channel.id == cid).unwrap();
        assert!(!row.monitor.enabled, "Auto off → monitor disabled");
        assert!(row.channel.enabled, "not 'Disabled' → channel on");
        assert_eq!(row.monitor.platform(), Platform::Twitch);
        assert_eq!(row.monitor.audio_tracks, "all"); // max-archival defaults

        // Auto on + "Disabled" → monitor.enabled true but channel.enabled false.
        let cid2 = create_monitor(
            &store,
            &defaults,
            "C:/out",
            "Tuber",
            "https://www.youtube.com/channel/UCabc",
            true,
            false,
        )
        .unwrap();
        let rows = store.list_monitors_with_channels().unwrap();
        let row2 = rows.iter().find(|r| r.channel.id == cid2).unwrap();
        assert!(row2.monitor.enabled);
        assert!(!row2.channel.enabled);
        assert_eq!(row2.monitor.platform(), Platform::YouTube);
    }
}
