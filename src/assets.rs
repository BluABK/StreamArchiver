use std::path::Path;

use anyhow::{Result, bail};
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

use crate::models::now_unix;

// ---------- Cache stamps ----------

/// True if the channel asset directory has not been fetched in the last 24 hours.
pub fn should_refetch_assets(asset_dir: &Path) -> bool {
    let stamp = asset_dir.join(".assets_fetched_at");
    match std::fs::read_to_string(&stamp) {
        Ok(s) => {
            let fetched: i64 = s.trim().parse().unwrap_or(0);
            now_unix() - fetched > 86_400
        }
        Err(_) => true,
    }
}

fn write_fetched_stamp(asset_dir: &Path) {
    let _ = std::fs::write(asset_dir.join(".assets_fetched_at"), now_unix().to_string());
}

fn should_refetch_global_badges(platform_dir: &Path) -> bool {
    let stamp = platform_dir.join("twitch").join(".global_badges_fetched_at");
    match std::fs::read_to_string(&stamp) {
        Ok(s) => s.trim().parse::<i64>().map(|t| now_unix() - t > 86_400).unwrap_or(true),
        Err(_) => true,
    }
}

fn write_global_badges_stamp(platform_dir: &Path) {
    let dir = platform_dir.join("twitch");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(".global_badges_fetched_at"), now_unix().to_string());
}

// ---------- Core utility ----------

/// Derive a file extension from a URL path (before `?` query string).
fn ext_from_url(url: &str) -> Option<&str> {
    let path = url.split('?').next()?;
    let ext = path.rsplit('.').next()?;
    if ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(ext)
    } else {
        None
    }
}

/// Download a URL to a file path; creates parent directories as needed.
async fn download_image(client: &Client, url: &str, dest: &Path) -> Result<()> {
    let url = if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_string()
    };
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {}", resp.status(), url);
    }
    let bytes = resp.bytes().await?;
    tokio::fs::write(dest, bytes).await?;
    Ok(())
}

// ---------- Per-recording thumbnail ----------

/// Download the stream thumbnail to `dest` (e.g., `{stem}.thumbnail.jpg`).
/// Expands Twitch's `{width}x{height}` template to 1280×720 before fetching.
pub async fn fetch_stream_thumbnail(client: &Client, url: &str, dest: &Path) -> Result<()> {
    let url = url
        .replace("{width}", "1280")
        .replace("{height}", "720");
    download_image(client, &url, dest).await
}

// ---------- Twitch channel assets ----------

/// Download Twitch channel icon and offline banner into `asset_dir/`.
async fn fetch_twitch_channel_assets(
    client: &Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    asset_dir: &Path,
) -> Result<()> {
    #[derive(Deserialize)]
    struct TwitchUser {
        profile_image_url: String,
        #[serde(default)]
        offline_image_url: String,
    }
    #[derive(Deserialize)]
    struct UsersResp {
        data: Vec<TwitchUser>,
    }

    let resp = client
        .get("https://api.twitch.tv/helix/users")
        .query(&[("id", broadcaster_id)])
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Helix users: {}", resp.status());
    }
    let r: UsersResp = resp.json().await?;
    let Some(user) = r.data.into_iter().next() else {
        bail!("no Helix user for id {broadcaster_id}");
    };

    tokio::fs::create_dir_all(asset_dir).await?;

    let icon_ext = ext_from_url(&user.profile_image_url).unwrap_or("jpg");
    if let Err(e) =
        download_image(client, &user.profile_image_url, &asset_dir.join(format!("icon.{icon_ext}")))
            .await
    {
        warn!("twitch icon: {e}");
    }

    if !user.offline_image_url.is_empty() {
        let banner_ext = ext_from_url(&user.offline_image_url).unwrap_or("jpg");
        if let Err(e) = download_image(
            client,
            &user.offline_image_url,
            &asset_dir.join(format!("banner.{banner_ext}")),
        )
        .await
        {
            warn!("twitch banner: {e}");
        }
    }
    Ok(())
}

// ---------- Twitch badges ----------

#[derive(Deserialize)]
struct HelixBadgeVersion {
    id: String,
    image_url_1x: String,
    image_url_2x: String,
    image_url_4x: String,
}
#[derive(Deserialize)]
struct HelixBadgeSet {
    set_id: String,
    versions: Vec<HelixBadgeVersion>,
}
#[derive(Deserialize)]
struct HelixBadgesResp {
    data: Vec<HelixBadgeSet>,
}

async fn download_badge_set(client: &Client, set: &HelixBadgeSet, badge_dir: &Path) {
    for ver in &set.versions {
        let dir = badge_dir.join(&set.set_id).join(&ver.id);
        for (url, fname) in [
            (&ver.image_url_1x, "1x.png"),
            (&ver.image_url_2x, "2x.png"),
            (&ver.image_url_4x, "4x.png"),
        ] {
            let dest = dir.join(fname);
            if dest.exists() {
                continue;
            }
            if let Err(e) = download_image(client, url, &dest).await {
                warn!("badge {}/{}/{fname}: {e}", set.set_id, ver.id);
            }
        }
    }
}

async fn fetch_helix_badges(
    client: &Client,
    client_id: &str,
    token: &str,
    url: &str,
    badge_dir: &Path,
) -> Result<()> {
    let resp = client
        .get(url)
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Helix badges ({}): {}", url, resp.status());
    }
    let r: HelixBadgesResp = resp.json().await?;
    for set in &r.data {
        download_badge_set(client, set, badge_dir).await;
    }
    Ok(())
}

/// Download global Twitch badges into `platform_dir/twitch/global_badges/` (once per 24h)
/// and channel-specific badges into `asset_dir/badges/`.
async fn fetch_twitch_badges(
    client: &Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    asset_dir: &Path,
    platform_dir: &Path,
) -> Result<()> {
    // Global badges are shared across all Twitch channels — fetch once per 24h.
    if should_refetch_global_badges(platform_dir) {
        let global_dir = platform_dir.join("twitch").join("global_badges");
        tokio::fs::create_dir_all(&global_dir).await?;
        match fetch_helix_badges(
            client,
            client_id,
            token,
            "https://api.twitch.tv/helix/chat/badges/global",
            &global_dir,
        )
        .await
        {
            Ok(_) => write_global_badges_stamp(platform_dir),
            Err(e) => warn!("global Twitch badges: {e}"),
        }
    }

    // Channel-specific badges go per-channel.
    if !broadcaster_id.is_empty() {
        let badge_dir = asset_dir.join("badges");
        tokio::fs::create_dir_all(&badge_dir).await?;
        let url = format!(
            "https://api.twitch.tv/helix/chat/badges?broadcaster_id={broadcaster_id}"
        );
        if let Err(e) = fetch_helix_badges(client, client_id, token, &url, &badge_dir).await {
            warn!("channel Twitch badges ({broadcaster_id}): {e}");
        }
    }
    Ok(())
}

// ---------- Twitch emotes ----------

#[derive(Deserialize)]
struct HelixEmoteImages {
    url_4x: String,
}
#[derive(Deserialize)]
struct HelixEmote {
    id: String,
    #[serde(default)]
    format: Vec<String>,
    images: HelixEmoteImages,
}
#[derive(Deserialize)]
struct HelixEmotesResp {
    data: Vec<HelixEmote>,
}

/// Download Twitch channel emotes into `asset_dir/emotes/twitch/`.
/// These are per-channel by nature so no global dedup is applied.
async fn fetch_twitch_emotes(
    client: &Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    asset_dir: &Path,
) -> Result<()> {
    if broadcaster_id.is_empty() {
        return Ok(());
    }
    let emote_dir = asset_dir.join("emotes").join("twitch");
    tokio::fs::create_dir_all(&emote_dir).await?;

    let url = format!(
        "https://api.twitch.tv/helix/chat/emotes?broadcaster_id={broadcaster_id}"
    );
    let resp = client
        .get(&url)
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Helix emotes: {}", resp.status());
    }
    let r: HelixEmotesResp = resp.json().await?;

    for emote in &r.data {
        let animated = emote.format.iter().any(|f| f == "animated");
        let (src_url, ext) = if animated {
            (
                format!(
                    "https://static-cdn.jtvnw.net/emoticons/v2/{}/animated/dark/3.0",
                    emote.id
                ),
                "gif",
            )
        } else {
            (emote.images.url_4x.clone(), "png")
        };
        let dest = emote_dir.join(format!("{}.{ext}", emote.id));
        if dest.exists() {
            continue;
        }
        if let Err(e) = download_image(client, &src_url, &dest).await {
            warn!("Twitch emote {}: {e}", emote.id);
        }
    }
    Ok(())
}

// ---------- BTTV ----------

/// Manifest entry written to `asset_dir/emotes/bttv.json`.
#[derive(serde::Serialize)]
struct EmoteManifestEntry {
    id: String,
    ext: String,
}

/// Download BTTV emotes:
/// - Channel emotes → `asset_dir/emotes/bttv/{id}.ext` (per-channel, unchanged)
/// - Shared emotes  → `platform_dir/bttv/emotes/{id}.ext` (global dedup, skip if present)
/// Writes a manifest `asset_dir/emotes/bttv.json` listing all active emote IDs for this channel.
async fn fetch_bttv_emotes(
    client: &Client,
    broadcaster_id: &str,
    asset_dir: &Path,
    platform_dir: &Path,
) -> Result<()> {
    if broadcaster_id.is_empty() {
        return Ok(());
    }
    #[derive(Deserialize)]
    struct BttvEmote {
        id: String,
        #[serde(rename = "imageType")]
        image_type: String,
    }
    #[derive(Deserialize)]
    struct BttvResp {
        #[serde(rename = "channelEmotes", default)]
        channel_emotes: Vec<BttvEmote>,
        #[serde(rename = "sharedEmotes", default)]
        shared_emotes: Vec<BttvEmote>,
    }

    let url = format!(
        "https://api.betterttv.net/3/cached/users/twitch/{broadcaster_id}"
    );
    let resp = client.get(&url).send().await?;
    // 404 = channel has no BTTV emotes; that's normal
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    if !resp.status().is_success() {
        bail!("BTTV: {}", resp.status());
    }
    let r: BttvResp = resp.json().await?;

    let mut manifest: Vec<EmoteManifestEntry> = Vec::new();

    // Channel emotes — per-channel directory
    if !r.channel_emotes.is_empty() {
        let dir = asset_dir.join("emotes").join("bttv");
        tokio::fs::create_dir_all(&dir).await?;
        for emote in &r.channel_emotes {
            manifest.push(EmoteManifestEntry {
                id: emote.id.clone(),
                ext: emote.image_type.clone(),
            });
            let dest = dir.join(format!("{}.{}", emote.id, emote.image_type));
            if dest.exists() {
                continue;
            }
            let url = format!(
                "https://cdn.betterttv.net/emote/{}/3x.{}",
                emote.id, emote.image_type
            );
            if let Err(e) = download_image(client, &url, &dest).await {
                warn!("BTTV channel emote {}: {e}", emote.id);
            }
        }
    }

    // Shared emotes — global dedup cache
    if !r.shared_emotes.is_empty() {
        let global_dir = platform_dir.join("bttv").join("emotes");
        tokio::fs::create_dir_all(&global_dir).await?;
        for emote in &r.shared_emotes {
            manifest.push(EmoteManifestEntry {
                id: emote.id.clone(),
                ext: emote.image_type.clone(),
            });
            let dest = global_dir.join(format!("{}.{}", emote.id, emote.image_type));
            if dest.exists() {
                continue;
            }
            let url = format!(
                "https://cdn.betterttv.net/emote/{}/3x.{}",
                emote.id, emote.image_type
            );
            if let Err(e) = download_image(client, &url, &dest).await {
                warn!("BTTV shared emote {}: {e}", emote.id);
            }
        }
    }

    // Write manifest listing all active emote IDs for this channel
    if !manifest.is_empty() {
        let manifest_dir = asset_dir.join("emotes");
        tokio::fs::create_dir_all(&manifest_dir).await?;
        if let Ok(json) = serde_json::to_string(&manifest) {
            let _ = tokio::fs::write(manifest_dir.join("bttv.json"), json).await;
        }
    }

    Ok(())
}

// ---------- FFZ ----------

/// Download FFZ channel emotes into the global dedup cache `platform_dir/ffz/emotes/`
/// and write a per-channel manifest `asset_dir/emotes/ffz.json`.
async fn fetch_ffz_emotes(
    client: &Client,
    broadcaster_id: &str,
    asset_dir: &Path,
    platform_dir: &Path,
) -> Result<()> {
    if broadcaster_id.is_empty() {
        return Ok(());
    }
    let url = format!("https://api.frankerfacez.com/v1/room/id/{broadcaster_id}");
    let resp = client.get(&url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    if !resp.status().is_success() {
        bail!("FFZ: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let sets = match v["sets"].as_object() {
        Some(s) => s.clone(),
        None => return Ok(()),
    };

    let global_dir = platform_dir.join("ffz").join("emotes");
    tokio::fs::create_dir_all(&global_dir).await?;

    let mut manifest: Vec<EmoteManifestEntry> = Vec::new();

    for set_val in sets.values() {
        let emotes = match set_val["emoticons"].as_array() {
            Some(e) => e.clone(),
            None => continue,
        };
        for emote in &emotes {
            let id = match emote["id"].as_i64() {
                Some(i) => i.to_string(),
                None => continue,
            };
            // Best available scale: 4 > 2 > 1
            let url_raw = emote["urls"]["4"]
                .as_str()
                .or_else(|| emote["urls"]["2"].as_str())
                .or_else(|| emote["urls"]["1"].as_str());
            let Some(url_raw) = url_raw else {
                continue;
            };
            let full_url = if url_raw.starts_with("//") {
                format!("https:{url_raw}")
            } else {
                url_raw.to_string()
            };
            let ext = ext_from_url(&full_url).unwrap_or("png");
            manifest.push(EmoteManifestEntry {
                id: id.clone(),
                ext: ext.to_string(),
            });
            let dest = global_dir.join(format!("{id}.{ext}"));
            if dest.exists() {
                continue;
            }
            if let Err(e) = download_image(client, &full_url, &dest).await {
                warn!("FFZ emote {id}: {e}");
            }
        }
    }

    if !manifest.is_empty() {
        let manifest_dir = asset_dir.join("emotes");
        tokio::fs::create_dir_all(&manifest_dir).await?;
        if let Ok(json) = serde_json::to_string(&manifest) {
            let _ = tokio::fs::write(manifest_dir.join("ffz.json"), json).await;
        }
    }

    Ok(())
}

// ---------- 7TV ----------

/// Download 7TV channel emotes into the global dedup cache `platform_dir/7tv/emotes/`
/// and write a per-channel manifest `asset_dir/emotes/7tv.json`.
async fn fetch_7tv_emotes(
    client: &Client,
    broadcaster_id: &str,
    asset_dir: &Path,
    platform_dir: &Path,
) -> Result<()> {
    if broadcaster_id.is_empty() {
        return Ok(());
    }
    let url = format!("https://7tv.io/v3/users/twitch/{broadcaster_id}");
    let resp = client.get(&url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    if !resp.status().is_success() {
        bail!("7TV: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let emotes = match v["emote_set"]["emotes"].as_array() {
        Some(e) => e.clone(),
        None => return Ok(()),
    };

    let global_dir = platform_dir.join("7tv").join("emotes");
    tokio::fs::create_dir_all(&global_dir).await?;

    let mut manifest: Vec<EmoteManifestEntry> = Vec::new();

    for emote in &emotes {
        let Some(id) = emote["id"].as_str() else {
            continue;
        };
        manifest.push(EmoteManifestEntry {
            id: id.to_string(),
            ext: "webp".to_string(),
        });
        let dest = global_dir.join(format!("{id}.webp"));
        if dest.exists() {
            continue;
        }
        // Prefer animated WebP; fall back to static
        let url = format!("https://cdn.7tv.app/emote/{id}/4x.webp");
        if let Err(e) = download_image(client, &url, &dest).await {
            warn!("7TV emote {id}: {e}");
        }
    }

    if !manifest.is_empty() {
        let manifest_dir = asset_dir.join("emotes");
        tokio::fs::create_dir_all(&manifest_dir).await?;
        if let Ok(json) = serde_json::to_string(&manifest) {
            let _ = tokio::fs::write(manifest_dir.join("7tv.json"), json).await;
        }
    }

    Ok(())
}

// ---------- YouTube ----------

/// Download YouTube channel icon and banner into `asset_dir/`.
async fn fetch_youtube_channel_assets(
    client: &Client,
    api_key: &str,
    channel_id: &str,
    asset_dir: &Path,
) -> Result<()> {
    if api_key.is_empty() || channel_id.is_empty() {
        bail!("missing YouTube API key or channel ID");
    }
    let url = format!(
        "https://www.googleapis.com/youtube/v3/channels\
         ?part=snippet,brandingSettings&id={channel_id}&key={api_key}"
    );
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        bail!("YouTube channels: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let item = &v["items"][0];
    if item.is_null() {
        bail!("YouTube channel not found: {channel_id}");
    }

    tokio::fs::create_dir_all(asset_dir).await?;

    // Profile picture (highest available resolution)
    let icon_url = item["snippet"]["thumbnails"]["high"]["url"]
        .as_str()
        .or_else(|| item["snippet"]["thumbnails"]["default"]["url"].as_str());
    if let Some(url) = icon_url {
        let ext = ext_from_url(url).unwrap_or("jpg");
        if let Err(e) = download_image(client, url, &asset_dir.join(format!("icon.{ext}"))).await {
            warn!("YouTube icon: {e}");
        }
    }

    // Channel banner
    let banner_url = item["brandingSettings"]["image"]["bannerExternalUrl"].as_str();
    if let Some(url) = banner_url {
        let ext = ext_from_url(url).unwrap_or("jpg");
        if let Err(e) = download_image(client, url, &asset_dir.join(format!("banner.{ext}"))).await
        {
            warn!("YouTube banner: {e}");
        }
    }
    Ok(())
}

// ---------- Kick ----------

/// Download Kick channel icon and banner into `asset_dir/` via the v2 API.
async fn fetch_kick_channel_assets(
    client: &Client,
    slug: &str,
    asset_dir: &Path,
) -> Result<()> {
    if slug.is_empty() {
        bail!("empty Kick slug");
    }
    let url = format!("https://kick.com/api/v2/channels/{slug}");
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Kick v2: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;

    tokio::fs::create_dir_all(asset_dir).await?;

    if let Some(url) = v["user"]["profile_pic"].as_str() {
        let ext = ext_from_url(url).unwrap_or("jpg");
        if let Err(e) = download_image(client, url, &asset_dir.join(format!("icon.{ext}"))).await {
            warn!("Kick icon: {e}");
        }
    }

    let banner_url = v["banner_image"]["url"]
        .as_str()
        .or_else(|| v["offline_banner_image"]["url"].as_str());
    if let Some(url) = banner_url {
        let ext = ext_from_url(url).unwrap_or("jpg");
        if let Err(e) = download_image(client, url, &asset_dir.join(format!("banner.{ext}"))).await
        {
            warn!("Kick banner: {e}");
        }
    }
    Ok(())
}

// ---------- Platform orchestrators ----------

/// Run all Twitch channel asset fetches:
/// - Icon + banner → `asset_dir/`
/// - Channel badges → `asset_dir/badges/`
/// - Global badges  → `platform_dir/twitch/global_badges/` (once per 24h, shared)
/// - Twitch channel emotes → `asset_dir/emotes/twitch/`
/// - BTTV channel emotes → `asset_dir/emotes/bttv/` + manifest `asset_dir/emotes/bttv.json`
/// - BTTV shared emotes → `platform_dir/bttv/emotes/` (global dedup)
/// - FFZ emotes → `platform_dir/ffz/emotes/` + manifest `asset_dir/emotes/ffz.json`
/// - 7TV emotes → `platform_dir/7tv/emotes/` + manifest `asset_dir/emotes/7tv.json`
pub async fn run_twitch_assets(
    client: &Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    asset_dir: &Path,
    platform_dir: &Path,
) {
    if let Err(e) =
        fetch_twitch_channel_assets(client, client_id, token, broadcaster_id, asset_dir).await
    {
        warn!("Twitch channel assets ({broadcaster_id}): {e}");
    }
    if let Err(e) =
        fetch_twitch_badges(client, client_id, token, broadcaster_id, asset_dir, platform_dir).await
    {
        warn!("Twitch badges ({broadcaster_id}): {e}");
    }
    if let Err(e) =
        fetch_twitch_emotes(client, client_id, token, broadcaster_id, asset_dir).await
    {
        warn!("Twitch emotes ({broadcaster_id}): {e}");
    }
    if let Err(e) = fetch_bttv_emotes(client, broadcaster_id, asset_dir, platform_dir).await {
        warn!("BTTV ({broadcaster_id}): {e}");
    }
    if let Err(e) = fetch_ffz_emotes(client, broadcaster_id, asset_dir, platform_dir).await {
        warn!("FFZ ({broadcaster_id}): {e}");
    }
    if let Err(e) = fetch_7tv_emotes(client, broadcaster_id, asset_dir, platform_dir).await {
        warn!("7TV ({broadcaster_id}): {e}");
    }
    write_fetched_stamp(asset_dir);
}

/// Run YouTube channel asset fetches (icon, banner).
pub async fn run_youtube_assets(
    client: &Client,
    api_key: &str,
    channel_id: &str,
    asset_dir: &Path,
) {
    if let Err(e) =
        fetch_youtube_channel_assets(client, api_key, channel_id, asset_dir).await
    {
        warn!("YouTube channel assets ({channel_id}): {e}");
    }
    write_fetched_stamp(asset_dir);
}

/// Run Kick channel asset fetches (icon, banner).
pub async fn run_kick_assets(client: &Client, slug: &str, asset_dir: &Path) {
    if let Err(e) = fetch_kick_channel_assets(client, slug, asset_dir).await {
        warn!("Kick channel assets ({slug}): {e}");
    }
    write_fetched_stamp(asset_dir);
}
