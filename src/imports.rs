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

/// Settings key for the persisted YouTube URL → `UC…` id resolution cache
/// (JSON object, lowercased monitor URL → channel id). Bounded by the number
/// of YouTube monitors, so it never needs eviction.
const K_YT_UC_CACHE: &str = "yt_uc_resolve_cache";

/// How many channel pages to scrape concurrently when resolving uncached URLs.
const RESOLVE_CONCURRENCY: usize = 4;

/// Resolve existing YouTube monitor `urls` (typically `@handle`-form, whose
/// `UC…` id can't be read from the URL itself) to lowercased channel-id
/// identities, for exact import dedup against a subscription's channel id.
///
/// Resolutions come from [`crate::websub::resolve_channel_uc`] (URL parse,
/// else a channel-page scrape — no API key or quota) and are persisted in the
/// settings table under [`K_YT_UC_CACHE`], so reopening the dialog never
/// re-scrapes. A URL that fails to resolve is simply absent from the result —
/// the dialog's fuzzy name dedup still applies to it.
pub async fn resolve_yt_identities(
    http: &Client,
    store: &Store,
    urls: &[String],
) -> Vec<(String, String)> {
    if urls.is_empty() {
        return Vec::new();
    }
    let mut cache: std::collections::HashMap<String, String> = store
        .get_setting(K_YT_UC_CACHE)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut missing: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for u in urls {
        let key = u.to_lowercase();
        if !cache.contains_key(&key) && seen.insert(key) {
            missing.push(u.clone());
        }
    }

    let mut dirty = false;
    let mut tasks = tokio::task::JoinSet::new();
    let mut queue = missing.into_iter();
    loop {
        while tasks.len() < RESOLVE_CONCURRENCY {
            let Some(url) = queue.next() else { break };
            let http = http.clone();
            tasks.spawn(async move {
                let uc = crate::websub::resolve_channel_uc(&http, &url).await;
                (url, uc)
            });
        }
        let Some(joined) = tasks.join_next().await else {
            break;
        };
        if let Ok((url, Some(uc))) = joined {
            cache.insert(url.to_lowercase(), uc);
            dirty = true;
        }
    }
    if dirty && let Ok(json) = serde_json::to_string(&cache) {
        let _ = store.set_setting(K_YT_UC_CACHE, &json);
    }

    urls.iter()
        .filter_map(|u| {
            cache
                .get(&u.to_lowercase())
                .map(|uc| (u.clone(), uc.to_lowercase()))
        })
        .collect()
}

/// Create one monitor for an imported URL, reusing the same per-platform
/// defaults a manual "Add stream" would (max-archival booleans on).
/// `monitor_enabled` is the "Auto" choice (whether the scheduler auto-records
/// it); `channel_enabled` is the container on/off (a "Disabled" import sets it
/// false — the master automation switch, so the channel sits fully dormant
/// until re-enabled, exactly like flipping Enabled off in the grid).
///
/// `target_channel`: `None` creates a brand-new channel container, seeding
/// its flags from the instance's (`create_container_with_flags`) like a
/// manual Add stream, so a fresh import never starts with a channel/instance
/// flag mismatch. `Some(id)` instead adds this as a new instance under an
/// ALREADY-EXISTING channel (the Import dialog's "import into an existing
/// channel" match) — that channel's own flags are left exactly as they are;
/// only this new instance's flags come from `monitor_enabled`/`channel_enabled`.
///
/// `quality`/`out_dir` override the per-platform defaults for this one
/// creation when non-empty (the dialog's "Overrides for this import"
/// section). Returns the channel id (new or reused).
#[allow(clippy::too_many_arguments)]
pub fn create_monitor(
    store: &Store,
    defaults: &MonitorDefaults,
    default_out: &str,
    target_channel: Option<i64>,
    name: &str,
    url: &str,
    monitor_enabled: bool,
    channel_enabled: bool,
    quality: Option<&str>,
    out_dir: Option<&str>,
) -> Result<i64> {
    let platform = Platform::detect(url);
    let channel_id = match target_channel {
        Some(id) => id,
        None => store.create_container_with_flags(name, monitor_enabled, channel_enabled)?,
    };
    let monitor = Monitor {
        id: 0,
        channel_id,
        url: url.to_string(),
        enabled: monitor_enabled,
        automation_enabled: channel_enabled,
        tool: defaults.resolve_tool(platform),
        detection_method: defaults.resolve_detection(platform),
        poll_interval_secs: defaults.resolve_poll_interval(platform).max(5),
        quality: match quality.map(str::trim).filter(|s| !s.is_empty()) {
            Some(q) => q.to_string(),
            None => defaults.resolve_quality(platform),
        },
        output_dir: match out_dir.map(str::trim).filter(|s| !s.is_empty()) {
            Some(o) => o.to_string(),
            None => defaults.resolve_output_dir(platform, name, default_out),
        },
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
        last_live_since: None,
        last_live_since_approx: false,
        sabr_codec_pref: crate::models::SabrCodecPref::Inherit,
        sabr_codec_custom: String::new(),
    };
    store.insert_monitor(&monitor)?;
    Ok(channel_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_yt_identities_uses_url_and_cache_without_scraping() {
        let store = Store::open_in_memory().unwrap();
        let http = Client::new();

        // A `/channel/UC…` URL resolves from the URL alone (no network), and the
        // result lands in the persisted cache.
        let ch = "https://www.youtube.com/channel/UCaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let out = resolve_yt_identities(&http, &store, &[ch.clone()]).await;
        assert_eq!(out, vec![(ch.clone(), "ucaaaaaaaaaaaaaaaaaaaaaa".to_string())]);
        let cached = store.get_setting(K_YT_UC_CACHE).unwrap().unwrap();
        assert!(cached.contains("UCaaaaaaaaaaaaaaaaaaaaaa"));

        // A pre-seeded cache entry (e.g. an @handle resolved on a previous open)
        // is returned without any resolution attempt.
        let handle = "https://www.youtube.com/@SomeHandle".to_string();
        let seeded = serde_json::json!({
            handle.to_lowercase(): "UCbbbbbbbbbbbbbbbbbbbbbb",
        });
        store.set_setting(K_YT_UC_CACHE, &seeded.to_string()).unwrap();
        let out = resolve_yt_identities(&http, &store, &[handle.clone()]).await;
        assert_eq!(out, vec![(handle, "ucbbbbbbbbbbbbbbbbbbbbbb".to_string())]);

        // Empty input short-circuits.
        assert!(resolve_yt_identities(&http, &store, &[]).await.is_empty());
    }

    #[test]
    fn create_monitor_seeds_flags_at_both_levels() {
        let store = Store::open_in_memory().unwrap();
        let defaults = MonitorDefaults::default();

        // Full Auto × Disabled matrix: the container's flags must mirror the
        // instance's (Auto → enabled, master Enabled → automation_enabled) so an
        // import never creates the channel/instance mismatch manual Add-stream
        // avoids via create_container_with_flags.
        for (auto, disabled) in [(false, false), (true, false), (false, true), (true, true)] {
            let cid = create_monitor(
                &store,
                &defaults,
                "C:/out",
                None,
                &format!("Cool-{auto}-{disabled}"),
                &format!("https://twitch.tv/cool_{auto}_{disabled}"),
                auto,
                !disabled,
                None,
                None,
            )
            .unwrap();
            let rows = store.list_monitors_with_channels().unwrap();
            let row = rows.iter().find(|r| r.channel.id == cid).unwrap();
            assert_eq!(row.monitor.enabled, auto, "Auto choice → monitor.enabled");
            assert_eq!(row.channel.enabled, auto, "Auto choice mirrored on the container");
            assert_eq!(
                row.monitor.automation_enabled, !disabled,
                "'Disabled' → instance master switch off"
            );
            assert_eq!(
                row.channel.automation_enabled, !disabled,
                "'Disabled' → container master switch off (fully dormant)"
            );
            assert_eq!(row.monitor.platform(), Platform::Twitch);
            assert_eq!(row.monitor.audio_tracks, "all"); // max-archival defaults
        }
    }

    #[test]
    fn create_monitor_reuses_an_existing_channel_without_touching_its_flags() {
        let store = Store::open_in_memory().unwrap();
        let defaults = MonitorDefaults::default();

        // An existing channel, already Auto-on / Enabled — e.g. tracked via
        // YouTube already.
        let existing_id =
            create_monitor(&store, &defaults, "C:/out", None, "Tenma Maemi",
                "https://www.youtube.com/channel/UCabc", true, true, None, None)
                .unwrap();

        // Importing a Twitch follow "into" that existing channel must add a
        // SECOND instance under the SAME channel id — not a new container —
        // and must not touch the existing channel's own flags, even when this
        // new instance's own Auto/Disabled choice differs.
        let cid = create_monitor(
            &store,
            &defaults,
            "C:/out",
            Some(existing_id),
            "Tenma",
            "https://twitch.tv/tenma",
            false, // Auto off for this new instance
            false, // Disabled for this new instance
            None,
            None,
        )
        .unwrap();
        assert_eq!(cid, existing_id, "reuses the given channel id, creates no new container");

        let rows = store.list_monitors_with_channels().unwrap();
        let monitors: Vec<_> = rows.iter().filter(|r| r.channel.id == existing_id).collect();
        assert_eq!(monitors.len(), 2, "both instances live under the one channel");
        let yt = monitors.iter().find(|r| r.monitor.platform() == Platform::YouTube).unwrap();
        let tw = monitors.iter().find(|r| r.monitor.platform() == Platform::Twitch).unwrap();
        assert!(yt.channel.enabled && yt.channel.automation_enabled, "existing channel flags untouched");
        assert!(!tw.monitor.enabled && !tw.monitor.automation_enabled, "new instance gets its OWN flags");
    }

    #[test]
    fn create_monitor_applies_import_overrides() {
        let store = Store::open_in_memory().unwrap();
        let defaults = MonitorDefaults::default();

        // Non-empty overrides win over the per-platform defaults…
        let cid = create_monitor(
            &store,
            &defaults,
            "C:/out",
            None,
            "Tuber",
            "https://www.youtube.com/channel/UCabc",
            true,
            true,
            Some("720p"),
            Some(r"D:\yt"),
        )
        .unwrap();
        let rows = store.list_monitors_with_channels().unwrap();
        let row = rows.iter().find(|r| r.channel.id == cid).unwrap();
        assert_eq!(row.monitor.quality, "720p");
        assert_eq!(row.monitor.output_dir, r"D:\yt");
        assert_eq!(row.monitor.platform(), Platform::YouTube);

        // …while empty/whitespace overrides fall back to the defaults.
        let cid2 = create_monitor(
            &store,
            &defaults,
            "C:/out",
            None,
            "Cool",
            "https://twitch.tv/cool",
            true,
            true,
            Some("  "),
            Some(""),
        )
        .unwrap();
        let rows = store.list_monitors_with_channels().unwrap();
        let row2 = rows.iter().find(|r| r.channel.id == cid2).unwrap();
        assert_eq!(row2.monitor.quality, defaults.resolve_quality(Platform::Twitch));
        assert_eq!(
            row2.monitor.output_dir,
            defaults.resolve_output_dir(Platform::Twitch, "Cool", "C:/out")
        );
    }
}
