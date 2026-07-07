use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

use crate::browser_ua::BrowserFingerprint;
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

/// True if this channel's assets have been fetched at least once (the freshness
/// stamp exists). Used to suppress change-log noise on the very first fetch: the
/// baseline run establishes the initial state, so a name colour appearing for the
/// first time is not a "change". The stamp is written only at the END of a fetch
/// run, so during the first run this returns false and the first-seen colour is
/// recorded silently — matching how emote/icon/banner baselines are silent.
fn assets_ever_fetched(asset_dir: &Path) -> bool {
    asset_dir.join(".assets_fetched_at").exists()
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

/// A monitor URL's stable per-ACCOUNT identity slug, used as the last segment of
/// the asset cache path so two same-platform instances of one channel (a main +
/// alt Twitch account) never share a directory. Purely syntactic (no network):
/// Twitch login / Kick slug / YouTube handle-or-UC-id parsed from the URL; any
/// unparseable URL falls back to a sanitized excerpt + a stable FNV hash so
/// distinct URLs can't collide. Always lowercase, filename-safe, non-empty.
pub fn account_slug(url: &str, platform: crate::models::Platform) -> String {
    use crate::models::Platform;
    let raw = match platform {
        Platform::Twitch => crate::detectors::twitch_login(url),
        Platform::Kick => crate::detectors::kick_slug(url).map(|s| s.to_lowercase()),
        Platform::YouTube => youtube_account_token(url),
        Platform::Generic => None,
    };
    let slug = match raw {
        Some(s) => crate::downloader::sanitize_filename(&s).to_lowercase(),
        None => url_fallback_slug(url),
    };
    if slug.is_empty() { url_fallback_slug(url) } else { slug }
}

/// YouTube account token from a channel URL: `@handle` (sans `@`), `/channel/UC…`
/// id, or a `/c/{name}` / `/user/{name}` path segment — all lowercased.
fn youtube_account_token(url: &str) -> Option<String> {
    let lower = url.trim().to_lowercase();
    if let Some(pos) = lower.find("/@") {
        let handle = lower[pos + 2..].split(['/', '?', '#']).next()?.trim();
        if !handle.is_empty() {
            return Some(handle.to_string());
        }
    }
    for marker in ["/channel/", "/c/", "/user/"] {
        if let Some(pos) = lower.find(marker) {
            let seg = lower[pos + marker.len()..].split(['/', '?', '#']).next()?.trim();
            if !seg.is_empty() {
                return Some(seg.to_string());
            }
        }
    }
    None
}

/// Fallback account slug for URLs no platform parser understands: a short
/// sanitized excerpt for readability + a stable FNV-1a hash for uniqueness.
fn url_fallback_slug(url: &str) -> String {
    let trimmed = url.trim().trim_start_matches("https://").trim_start_matches("http://");
    let mut excerpt = crate::downloader::sanitize_filename(trimmed).to_lowercase();
    excerpt.truncate(40);
    let excerpt = excerpt.trim_matches(['.', ' ', '_']).to_string();
    let hash = crate::detectors::fnv64(url.trim().as_bytes());
    if excerpt.is_empty() {
        format!("url_{:08x}", hash as u32)
    } else {
        format!("{excerpt}_{:08x}", hash as u32)
    }
}

/// Per-account channel asset directory:
/// `…/channel_assets/{name}/{platform}/{account}/`. The single source of truth
/// for the layout — shared by the asset fetcher, the UI (avatars / status grid),
/// and desktop notifications, so they never drift. `account` is
/// [`account_slug`] of the owning monitor's URL; two instances on the SAME
/// platform (main + alt account) therefore get separate trees, while two tools
/// on the SAME URL share one.
pub fn channel_asset_dir(name: &str, platform: crate::models::Platform, account: &str) -> PathBuf {
    legacy_platform_dir(name, platform).join(account)
}

/// The pre-account layout (`…/channel_assets/{name}/{platform}/`) — kept as a
/// read-fallback and as the startup migration's source. New writes never land
/// here.
pub fn legacy_platform_dir(name: &str, platform: crate::models::Platform) -> PathBuf {
    crate::app_paths::asset_cache_dir()
        .join("channel_assets")
        .join(crate::downloader::sanitize_filename(name))
        .join(platform.as_str())
}

/// The directories to consult when READING an asset: the account dir first,
/// then the legacy per-platform dir (pre-migration layouts / renamed channels).
pub fn asset_read_dirs(
    name: &str,
    platform: crate::models::Platform,
    account: &str,
) -> [PathBuf; 2] {
    [
        channel_asset_dir(name, platform, account),
        legacy_platform_dir(name, platform),
    ]
}

/// Entries the startup migration moves from a legacy `{name}/{platform}/` dir
/// into its `{account}/` subdir. STRICTLY allow-listed: `posts/` and
/// `schedule_src/` hold files whose ABSOLUTE paths are persisted in the DB
/// (`community_post_media.local_path`, `schedule_source_image.local_path`), so
/// they must stay put (both self-heal into account dirs on the next fetch);
/// unknown entries (including already-migrated account subdirs) are never
/// touched.
fn legacy_payload_entry(name: &str) -> bool {
    matches!(name, "name_color.txt" | "asset_changes.jsonl" | ".assets_fetched_at"
        | "emotes" | "badges" | "history")
        || name.starts_with("icon.")
        || name.starts_with("icon_")
        || name.starts_with("banner.")
}

/// One-time startup migration: move each channel's legacy per-platform asset
/// payload into the per-ACCOUNT subdir of the FIRST monitor for that
/// (channel, platform) — matching the pre-account layout's de-facto winner.
/// Channels with no matching monitor (renamed/removed) are left untouched; the
/// read-fallback covers them. Stamped with `.accounts_migrated` so it runs once.
pub fn migrate_assets_to_account_dirs(store: &crate::store::Store) {
    let root = crate::app_paths::asset_cache_dir().join("channel_assets");
    let mut first_urls: std::collections::HashMap<(String, crate::models::Platform), String> =
        std::collections::HashMap::new();
    if let Ok(rows) = store.list_monitors_with_channels() {
        for row in &rows {
            let key = (
                crate::downloader::sanitize_filename(&row.channel.name),
                row.monitor.platform(),
            );
            first_urls.entry(key).or_insert_with(|| row.monitor.url.clone());
        }
    }
    migrate_assets_root(&root, &first_urls);
}

/// Testable core of [`migrate_assets_to_account_dirs`] (takes the tree root and
/// the (sanitized-channel, platform) → first-monitor-URL map directly).
pub(crate) fn migrate_assets_root(
    root: &Path,
    first_urls: &std::collections::HashMap<(String, crate::models::Platform), String>,
) {
    use crate::models::Platform;
    if !root.is_dir() {
        return; // nothing fetched yet — first run populates account dirs directly
    }
    let stamp = root.join(".accounts_migrated");
    if stamp.exists() {
        return;
    }
    let Ok(channels) = std::fs::read_dir(root) else { return };
    for chan in channels.flatten() {
        let chan_dir = chan.path();
        if !chan_dir.is_dir() {
            continue;
        }
        let chan_key = chan.file_name().to_string_lossy().into_owned();
        for plat_name in ["twitch", "youtube", "kick", "generic"] {
            let plat_dir = chan_dir.join(plat_name);
            if !plat_dir.is_dir() {
                continue;
            }
            let platform = Platform::parse(plat_name);
            // Legacy payload present directly in the platform dir?
            let legacy: Vec<PathBuf> = std::fs::read_dir(&plat_dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(legacy_payload_entry)
                })
                .collect();
            if legacy.is_empty() {
                continue;
            }
            let Some(url) = first_urls.get(&(chan_key.clone(), platform)) else {
                warn!(
                    "asset migration: no monitor matches {chan_key}/{plat_name} — leaving legacy layout in place"
                );
                continue;
            };
            let account_dir = plat_dir.join(account_slug(url, platform));
            let _ = std::fs::create_dir_all(&account_dir);
            for src in legacy {
                let Some(fname) = src.file_name() else { continue };
                let dest = account_dir.join(fname);
                if dest.exists() {
                    continue; // a newer account-side copy exists — keep both, prefer it
                }
                if let Err(e) = std::fs::rename(&src, &dest) {
                    warn!("asset migration: could not move {} -> {}: {e}", src.display(), dest.display());
                }
            }
            tracing::info!(
                "asset migration: {chan_key}/{plat_name} -> per-account dir {}",
                account_dir.display()
            );
        }
    }
    let _ = std::fs::write(&stamp, now_unix().to_string());
}

/// Find `{prefix}*` under ANY account subdir of `{name}/{platform}/` (then the
/// legacy platform dir itself) — for readers that know the channel but not
/// which account produced the asset (e.g. the banner-OCR schedule source).
pub fn find_asset_any_account(
    name: &str,
    platform: crate::models::Platform,
    prefix: &str,
) -> Option<PathBuf> {
    let root = legacy_platform_dir(name, platform);
    if let Ok(entries) = std::fs::read_dir(&root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir()
                && !p.file_name().is_some_and(|n| {
                    matches!(n.to_str(), Some("history" | "emotes" | "badges" | "posts" | "schedule_src"))
                })
                && let Some(hit) = find_asset(&p, prefix)
            {
                return Some(hit);
            }
        }
    }
    find_asset(&root, prefix)
}

/// First file in `dir` whose name starts with `prefix` (e.g. `"banner."`). Used to
/// locate a canonical channel asset (`icon.png`, `banner.jpg`) without knowing its
/// extension. Skips the `history/` subdir and archived `{stem}_{ts}.ext` variants
/// since those don't start with `{stem}.`.
pub(crate) fn find_asset(dir: &Path, prefix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
        })
}

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

/// Ensure a `px × px` downscaled copy of the channel icon exists at
/// `asset_dir/icon_{px}.png`. Generated with Lanczos3 on first call; the cached
/// file is reused on subsequent calls unless the source icon is newer (mtime
/// check). Returns the path to the scaled file, or `None` when no source icon
/// is present or image processing fails. When the source is already ≤ `px` the
/// source path is returned as-is (no unnecessary upscaling).
pub fn ensure_scaled_icon(asset_dir: &Path, px: u32) -> Option<PathBuf> {
    let out = asset_dir.join(format!("icon_{px}.png"));
    let src = find_asset(asset_dir, "icon.")?;

    if out.exists() {
        // Regenerate only if the source icon was updated after the last scale.
        let src_mtime = std::fs::metadata(&src).ok()?.modified().ok()?;
        let out_mtime = std::fs::metadata(&out).ok()?.modified().ok()?;
        if out_mtime >= src_mtime {
            return Some(out);
        }
    }

    let bytes = std::fs::read(&src).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    if img.width() <= px && img.height() <= px {
        return Some(src);
    }
    let scaled = image::imageops::resize(&img, px, px, image::imageops::FilterType::Lanczos3);
    scaled.save(&out).ok()?;
    Some(out)
}

/// Download a URL to a file path; creates parent directories as needed.
pub(crate) async fn download_image(client: &Client, url: &str, dest: &Path) -> Result<()> {
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

/// The current canonical asset file `dir/{stem}.<ext>` (any extension), if one
/// exists. Matches `icon.png` but never the `history/` dir or an archived
/// `icon_<ts>.png` (those use `{stem}_`, not `{stem}.`).
async fn current_asset(dir: &Path, stem: &str) -> Option<PathBuf> {
    let prefix = format!("{stem}.");
    let mut rd = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            return Some(entry.path());
        }
    }
    None
}

/// Download a per-channel **singular** asset (icon / banner) into `dir`,
/// preserving history — this is an archiver, so a profile pic / banner the
/// channel later changes must not be lost.
///
/// `dir/{stem}.{ext}` always holds the latest version. When the freshly fetched
/// image differs (byte-for-byte) from the current canonical file, the old one is
/// moved into `dir/history/{stem}_{retired_at}.{old_ext}` before being replaced;
/// `retired_at` is the unix time it was supplanted, so the history reads as a
/// timeline. An identical re-download is a no-op (no spurious history entry).
async fn download_image_archival(
    client: &Client,
    url: &str,
    dir: &Path,
    stem: &str,
    ext: &str,
) -> Result<()> {
    let url = if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_string()
    };
    tokio::fs::create_dir_all(dir).await?;
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {}", resp.status(), url);
    }
    let bytes = resp.bytes().await?;
    archive_and_write(dir, stem, ext, &bytes).await
}

/// Place `bytes` as the canonical `dir/{stem}.{ext}`, archiving any differing
/// current version into `dir/history/` first. Network-free (the testable core of
/// [`download_image_archival`]). A byte-identical current file is left untouched
/// (no spurious history entry); a differing one is moved to
/// `history/{stem}_{retired_at}.{old_ext}` so it is never lost.
async fn archive_and_write(dir: &Path, stem: &str, ext: &str, bytes: &[u8]) -> Result<()> {
    if let Some(cur_path) = current_asset(dir, stem).await {
        match tokio::fs::read(&cur_path).await {
            // Unchanged since last fetch — leave everything as-is.
            Ok(cur) if cur == bytes => return Ok(()),
            // Changed — archive the old version before it's overwritten.
            Ok(_) => {
                let hist = dir.join("history");
                tokio::fs::create_dir_all(&hist).await?;
                let cur_ext = cur_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("img");
                // Name by retirement time, but never collide with an existing
                // archived version (two changes in the same second, or a clock
                // that didn't advance) — append a counter so nothing is lost.
                let ts = now_unix();
                let mut archived = hist.join(format!("{stem}_{ts}.{cur_ext}"));
                let mut n = 1;
                while tokio::fs::try_exists(&archived).await.unwrap_or(false) {
                    n += 1;
                    archived = hist.join(format!("{stem}_{ts}_{n}.{cur_ext}"));
                }
                // Move the old canonical into history (rename; fall back to
                // copy+remove if the move fails). This also clears a stale
                // canonical whose extension differs from the new one.
                if tokio::fs::rename(&cur_path, &archived).await.is_err() {
                    tokio::fs::copy(&cur_path, &archived).await?;
                    let _ = tokio::fs::remove_file(&cur_path).await;
                }
                // Log the replacement so the change-history can show it. `stem` is
                // "icon"/"banner"; `id` points at the archived previous version.
                let archived_name = archived
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                append_asset_changes(
                    dir,
                    &[AssetChange {
                        at: ts,
                        kind: stem.to_string(),
                        provider: String::new(),
                        action: "changed".to_string(),
                        name: String::new(),
                        id: archived_name,
                        old: String::new(),
                        new: String::new(),
                    }],
                )
                .await;
            }
            // Unreadable current file — just overwrite it.
            Err(_) => {}
        }
    }

    tokio::fs::write(dir.join(format!("{stem}.{ext}")), bytes).await?;
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

/// Download Twitch channel icon and offline banner into `asset_dir/`. Returns
/// the broadcaster's channel description (bio) from the same Helix response —
/// input to the About-page snapshot, no extra request.
async fn fetch_twitch_channel_assets(
    client: &Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    asset_dir: &Path,
) -> Result<String> {
    #[derive(Deserialize)]
    struct TwitchUser {
        profile_image_url: String,
        #[serde(default)]
        offline_image_url: String,
        #[serde(default)]
        description: String,
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
        download_image_archival(client, &user.profile_image_url, asset_dir, "icon", icon_ext).await
    {
        warn!("twitch icon: {e}");
    }

    if !user.offline_image_url.is_empty() {
        let banner_ext = ext_from_url(&user.offline_image_url).unwrap_or("jpg");
        if let Err(e) =
            download_image_archival(client, &user.offline_image_url, asset_dir, "banner", banner_ext)
                .await
        {
            warn!("twitch banner: {e}");
        }
    }
    Ok(user.description)
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
    name: String,
    #[serde(default)]
    format: Vec<String>,
    images: HelixEmoteImages,
}
#[derive(Deserialize)]
struct HelixEmotesResp {
    data: Vec<HelixEmote>,
}

/// Download Twitch channel emotes into `asset_dir/emotes/twitch/` and write a
/// per-channel manifest `asset_dir/emotes/twitch.json`. Mirrors the BTTV/FFZ/7TV
/// pattern so Twitch emotes also have named files and diff/history tracking.
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

    let mut manifest: Vec<EmoteManifestEntry> = Vec::new();

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
        manifest.push(EmoteManifestEntry {
            name: emote.name.clone(),
            id: emote.id.clone(),
            ext: ext.to_string(),
            shared: false,
        });
        // New downloads get `{id}_{name}.{ext}`; old `{id}.{ext}` files are kept
        // as-is (the viewer resolver falls back to them).
        let new_dest = emote_dir.join(format!(
            "{}_{}.{ext}",
            emote.id,
            sanitize_emote_name(&emote.name)
        ));
        let old_dest = emote_dir.join(format!("{}.{ext}", emote.id));
        if asset_present(&new_dest) || asset_present(&old_dest) {
            continue;
        }
        if let Err(e) = download_image(client, &src_url, &new_dest).await {
            warn!("Twitch emote {}: {e}", emote.id);
        }
    }

    if !manifest.is_empty() {
        record_manifest_change(asset_dir, "twitch", &manifest).await;
        if let Ok(json) = serde_json::to_string(&manifest) {
            let _ = tokio::fs::write(asset_dir.join("emotes").join("twitch.json"), json).await;
        }
    }

    Ok(())
}

// ---------- BTTV ----------

/// Manifest entry written to `asset_dir/emotes/{bttv,ffz,7tv}.json`. The chat
/// replay reads these back to map a typed emote word → its on-disk image, so the
/// `name` (emote code) is required. `#[serde(default)]` keeps pre-name manifests
/// loadable (empty name → simply unmatchable until the channel's assets refetch).
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct EmoteManifestEntry {
    /// Emote CODE, case-sensitive as typed in chat: BTTV `code`, FFZ `name`,
    /// 7TV top-level `name` (the channel alias).
    #[serde(default)]
    pub name: String,
    pub id: String,
    pub ext: String,
    /// BTTV only: `true` ⇒ image is in the shared global cache
    /// (`platform_assets/bttv/emotes/`); `false` ⇒ per-channel
    /// (`asset_dir/emotes/bttv/`). Ignored for FFZ/7TV (always global).
    #[serde(default)]
    pub shared: bool,
}

/// A previously-downloaded asset is "present" only if it exists AND is non-empty.
/// `download_image` writes non-atomically (truncate-then-write), so an interrupted
/// fetch can leave a 0-byte file; treating that as absent lets a later pass repair
/// it instead of the `exists()` guard pinning the corrupt file forever.
fn asset_present(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
}

/// Sanitize an emote code for use as a filename component. Keeps alphanumerics,
/// underscores, and hyphens; replaces anything else with `_`.
pub(crate) fn sanitize_emote_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

// ---------- Asset change history ----------

/// One recorded change to a channel's assets, appended as a JSON line to the
/// per-channel-platform `asset_changes.jsonl`. This is the queryable companion to
/// the filesystem `history/` archives: the images/manifests preserve the *bytes*,
/// this log preserves *what changed and when* so the UI can present a timeline.
///
/// Why a log at all: emote manifests are overwritten wholesale on every refetch,
/// so a code the streamer later removes would otherwise vanish without a trace
/// (it's not even caught as "deprecated", since it's no longer in the manifest).
/// Diffing the old manifest against the new before the overwrite records the
/// removal (and additions) here, permanently.
///
/// The `kind`/`action` pair is a small open vocabulary the UI maps to display:
/// - `kind = "emote"`  → `action` `"added"`/`"removed"`, with `provider`
///   (`"7tv"`/`"bttv"`/`"ffz"`), `name` (the code) and `id`.
/// - `kind = "icon"`/`"banner"` → `action` `"changed"`; `id` is the archived
///   filename kept under `history/`.
/// - `kind = "name_color"` → `action` `"added"`/`"removed"`/`"changed"` with the
///   `old`/`new` hex strings.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AssetChange {
    /// Unix seconds the change was recorded (when the refetch saw it).
    pub at: i64,
    pub kind: String,
    /// Emote provider stem for `kind = "emote"` (`"7tv"`/`"bttv"`/`"ffz"`); empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    pub action: String,
    /// Emote code (for `kind = "emote"`); empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Emote id, or the archived `history/` filename for icon/banner; empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Previous value (e.g. old name colour); empty when not applicable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub old: String,
    /// New value (e.g. new name colour); empty when not applicable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub new: String,
}

impl AssetChange {
    fn emote(at: i64, provider: &str, action: &str, name: &str, id: &str) -> AssetChange {
        AssetChange {
            at,
            kind: "emote".to_string(),
            provider: provider.to_string(),
            action: action.to_string(),
            name: name.to_string(),
            id: id.to_string(),
            old: String::new(),
            new: String::new(),
        }
    }
}

/// Diff two emote manifests by **code** (case-sensitive, as typed in chat) and
/// return one [`AssetChange`] per added/removed code. Empty/whitespace codes are
/// ignored (legacy name-less entries can never match chat anyway). An id-only
/// change to an existing code yields nothing — only the code set matters here, so
/// churn in ids/urls doesn't spam the history. Output is sorted by code so the log
/// (and the unit test) is deterministic.
fn diff_emote_manifest(
    old: &[EmoteManifestEntry],
    new: &[EmoteManifestEntry],
    provider: &str,
    at: i64,
) -> Vec<AssetChange> {
    use std::collections::HashMap;
    let index = |m: &[EmoteManifestEntry]| -> HashMap<String, String> {
        m.iter()
            .filter(|e| !e.name.trim().is_empty())
            .map(|e| (e.name.clone(), e.id.clone()))
            .collect()
    };
    let old_idx = index(old);
    let new_idx = index(new);
    let mut out: Vec<AssetChange> = Vec::new();
    for (name, id) in &old_idx {
        if !new_idx.contains_key(name) {
            out.push(AssetChange::emote(at, provider, "removed", name, id));
        }
    }
    for (name, id) in &new_idx {
        if !old_idx.contains_key(name) {
            out.push(AssetChange::emote(at, provider, "added", name, id));
        }
    }
    // A code is in at most one of {added, removed}, so sorting by code alone is a
    // total, stable order.
    out.sort_by_key(|c| c.name.to_lowercase());
    out
}

/// Retry a fallible async file operation up to 4 times with a short delay,
/// tolerating a transient lock/access error (e.g. Windows Defender scanning a
/// just-written file — the same CI flakiness `record_manifest_change`'s
/// manifest-read retry already works around) rather than giving up on the
/// first attempt. Returns the last error if every attempt fails.
async fn retry_transient<F, Fut, T>(mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let mut last_err = None;
    for attempt in 0..4u32 {
        if attempt > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("loop always runs at least once"))
}

/// Append change records to the channel-platform `asset_changes.jsonl` (one JSON
/// object per line, append-only). Best-effort: a write failure is swallowed — the
/// history is a convenience layer and must never abort or fail an asset fetch —
/// but the *open* is retried a few times first (see [`retry_transient`]), since a
/// transient lock failing silently here would drop change entries, not just log a
/// harmless warning. Explicitly flushed before returning: `tokio::fs::File`
/// dispatches writes to a background blocking-thread-pool task, and without an
/// explicit flush a caller that immediately reads the file back (e.g. via
/// `std::fs::read_to_string`, as the UI's synchronous `read_asset_changes` does)
/// can race ahead of it — the write reports success while the bytes aren't
/// visible yet, which is exactly what made this function's own test flaky under
/// heavy parallel load (many tests contending for that same thread pool).
async fn append_asset_changes(asset_dir: &Path, changes: &[AssetChange]) {
    if changes.is_empty() {
        return;
    }
    let mut buf = String::new();
    for c in changes {
        if let Ok(line) = serde_json::to_string(c) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    if buf.is_empty() || tokio::fs::create_dir_all(asset_dir).await.is_err() {
        return;
    }
    use tokio::io::AsyncWriteExt;
    let path = asset_dir.join("asset_changes.jsonl");
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).append(true);
    if let Ok(mut f) = retry_transient(|| opts.open(&path)).await {
        let _ = f.write_all(buf.as_bytes()).await;
        let _ = f.flush().await;
    }
}

/// Read a channel-platform's recorded asset changes (`asset_changes.jsonl`) in
/// chronological append order (oldest first). Malformed/blank lines are skipped;
/// a missing file yields an empty vec. Synchronous — the UI calls it directly on
/// popup-open (the file is tiny: a handful of lines per refetch).
pub fn read_asset_changes(asset_dir: &Path) -> Vec<AssetChange> {
    let Ok(s) = std::fs::read_to_string(asset_dir.join("asset_changes.jsonl")) else {
        return Vec::new();
    };
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<AssetChange>(l).ok())
        .collect()
}

/// Record what a freshly-fetched emote manifest changed, *before* it overwrites the
/// previous one at `asset_dir/emotes/{provider}.json`. The prior manifest is read,
/// diffed by code against `new`, and on any add/remove: the old manifest is
/// snapshotted to `emotes/history/{provider}_{ts}.json` (full archival, mirroring
/// the icon/banner `history/`) and the per-emote changes are appended to
/// `asset_changes.jsonl`. A no-op on the first fetch (no prior manifest = baseline,
/// not a change) or when the code set is unchanged. `provider` is the manifest stem
/// (`"7tv"`/`"bttv"`/`"ffz"`). The current manifest file is left in place — the
/// caller writes the new one right after, so the canonical manifest is never
/// missing even if this snapshot write fails.
async fn record_manifest_change(asset_dir: &Path, provider: &str, new: &[EmoteManifestEntry]) {
    let emotes_dir = asset_dir.join("emotes");
    let manifest_path = emotes_dir.join(format!("{provider}.json"));
    // Retry on transient lock errors (e.g. Windows Defender scanning a newly written
    // file on CI). Return immediately on NotFound — that's the expected first-fetch
    // baseline case, not an error.
    let old_json = {
        let mut outcome = None;
        for attempt in 0..4u32 {
            if attempt > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
            match tokio::fs::read_to_string(&manifest_path).await {
                Ok(s) => {
                    outcome = Some(s);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => {}
            }
        }
        match outcome {
            Some(s) => s,
            None => return,
        }
    };
    // Treat a corrupt/truncated prior manifest as "unknown" and bail, mirroring the
    // missing-file early return above. Defaulting to an empty Vec would diff every
    // current emote as a fresh "add" and snapshot a manifest we couldn't even parse.
    let Ok(old) = serde_json::from_str::<Vec<EmoteManifestEntry>>(&old_json) else {
        return;
    };
    let at = now_unix();
    let changes = diff_emote_manifest(&old, new, provider, at);
    if changes.is_empty() {
        return;
    }
    // Snapshot the prior manifest (full archival) before it's overwritten. Written
    // from the in-memory bytes, not a rename, so the canonical path stays valid.
    let hist = emotes_dir.join("history");
    if tokio::fs::create_dir_all(&hist).await.is_ok() {
        let mut dest = hist.join(format!("{provider}_{at}.json"));
        let mut n = 1;
        while tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            n += 1;
            dest = hist.join(format!("{provider}_{at}_{n}.json"));
        }
        let _ = retry_transient(|| tokio::fs::write(&dest, old_json.as_bytes())).await;
    }
    append_asset_changes(asset_dir, &changes).await;
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
        /// The emote word as typed in chat (e.g. `modCheck`). `#[serde(default)]`
        /// so one malformed emote can't abort the whole channel's BTTV fetch; an
        /// empty code just yields an unmatchable manifest entry (reader skips it).
        #[serde(default)]
        code: String,
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
                name: emote.code.clone(),
                id: emote.id.clone(),
                ext: emote.image_type.clone(),
                shared: false,
            });
            let new_dest = dir.join(format!(
                "{}_{}.{}",
                emote.id,
                sanitize_emote_name(&emote.code),
                emote.image_type
            ));
            let old_dest = dir.join(format!("{}.{}", emote.id, emote.image_type));
            if asset_present(&new_dest) || asset_present(&old_dest) {
                continue;
            }
            let url = format!(
                "https://cdn.betterttv.net/emote/{}/3x.{}",
                emote.id, emote.image_type
            );
            if let Err(e) = download_image(client, &url, &new_dest).await {
                warn!("BTTV channel emote {}: {e}", emote.id);
            } else {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }

    // Shared emotes — global dedup cache
    if !r.shared_emotes.is_empty() {
        let global_dir = platform_dir.join("bttv").join("emotes");
        tokio::fs::create_dir_all(&global_dir).await?;
        for emote in &r.shared_emotes {
            manifest.push(EmoteManifestEntry {
                name: emote.code.clone(),
                id: emote.id.clone(),
                ext: emote.image_type.clone(),
                shared: true,
            });
            let new_dest = global_dir.join(format!(
                "{}_{}.{}",
                emote.id,
                sanitize_emote_name(&emote.code),
                emote.image_type
            ));
            let old_dest = global_dir.join(format!("{}.{}", emote.id, emote.image_type));
            if asset_present(&new_dest) || asset_present(&old_dest) {
                continue;
            }
            let url = format!(
                "https://cdn.betterttv.net/emote/{}/3x.{}",
                emote.id, emote.image_type
            );
            if let Err(e) = download_image(client, &url, &new_dest).await {
                warn!("BTTV shared emote {}: {e}", emote.id);
            } else {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }

    // Write manifest listing all active emote IDs for this channel
    if !manifest.is_empty() {
        let manifest_dir = asset_dir.join("emotes");
        tokio::fs::create_dir_all(&manifest_dir).await?;
        // Record added/removed codes against the previous manifest before overwriting.
        record_manifest_change(asset_dir, "bttv", &manifest).await;
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
            let Some(name) = emote["name"].as_str() else {
                continue;
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
                name: name.to_string(),
                id: id.clone(),
                ext: ext.to_string(),
                shared: false,
            });
            let new_dest = global_dir.join(format!("{id}_{}.{ext}", sanitize_emote_name(name)));
            let old_dest = global_dir.join(format!("{id}.{ext}"));
            if asset_present(&new_dest) || asset_present(&old_dest) {
                continue;
            }
            if let Err(e) = download_image(client, &full_url, &new_dest).await {
                warn!("FFZ emote {id}: {e}");
            } else {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }

    if !manifest.is_empty() {
        let manifest_dir = asset_dir.join("emotes");
        tokio::fs::create_dir_all(&manifest_dir).await?;
        record_manifest_change(asset_dir, "ffz", &manifest).await;
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
        // Top-level `name` is this channel's alias (what viewers actually type);
        // `data.name` is the original. Match on the alias.
        let Some(name) = emote["name"].as_str() else {
            continue;
        };
        manifest.push(EmoteManifestEntry {
            name: name.to_string(),
            id: id.to_string(),
            ext: "webp".to_string(),
            shared: false,
        });
        let new_dest = global_dir.join(format!("{id}_{}.webp", sanitize_emote_name(name)));
        let old_dest = global_dir.join(format!("{id}.webp"));
        if asset_present(&new_dest) || asset_present(&old_dest) {
            continue;
        }
        // Prefer animated WebP; fall back to static
        let url = format!("https://cdn.7tv.app/emote/{id}/4x.webp");
        if let Err(e) = download_image(client, &url, &new_dest).await {
            warn!("7TV emote {id}: {e}");
        } else {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    if !manifest.is_empty() {
        let manifest_dir = asset_dir.join("emotes");
        tokio::fs::create_dir_all(&manifest_dir).await?;
        record_manifest_change(asset_dir, "7tv", &manifest).await;
        if let Ok(json) = serde_json::to_string(&manifest) {
            let _ = tokio::fs::write(manifest_dir.join("7tv.json"), json).await;
        }
    }

    Ok(())
}

// ---------- YouTube ----------

/// Download YouTube channel icon and banner into `asset_dir/`.
/// Returns `(banner_set, description)`: `banner_set` lets the caller skip the
/// page-scrape banner fallback (two banner sources overwrite each other and
/// spam phantom history); `description` is `snippet.description` from the same
/// response — About-page input, zero extra quota.
async fn fetch_youtube_channel_assets(
    client: &Client,
    api_key: &str,
    channel_id: &str,
    asset_dir: &Path,
) -> Result<(bool, String)> {
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
        if let Err(e) = download_image_archival(client, url, asset_dir, "icon", ext).await {
            warn!("YouTube icon: {e}");
        }
    }

    // Channel banner
    let mut banner_set = false;
    let banner_url = item["brandingSettings"]["image"]["bannerExternalUrl"].as_str();
    if let Some(url) = banner_url {
        let ext = ext_from_url(url).unwrap_or("jpg");
        match download_image_archival(client, url, asset_dir, "banner", ext).await {
            Ok(()) => banner_set = true,
            Err(e) => warn!("YouTube banner: {e}"),
        }
    }
    let description = item["snippet"]["description"].as_str().unwrap_or("").to_string();
    Ok((banner_set, description))
}

// ---------- Kick ----------

/// Download Kick channel icon and banner into `asset_dir/` via the v2 API.
/// Returns the parsed v2 channel JSON so the caller can also archive the
/// about page (bio + socials) from the SAME response — zero extra requests,
/// zero extra Cloudflare exposure.
async fn fetch_kick_channel_assets(
    client: &Client,
    slug: &str,
    asset_dir: &Path,
) -> Result<serde_json::Value> {
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
        if let Err(e) = download_image_archival(client, url, asset_dir, "icon", ext).await {
            warn!("Kick icon: {e}");
        }
    }

    let banner_url = v["banner_image"]["url"]
        .as_str()
        .or_else(|| v["offline_banner_image"]["url"].as_str());
    if let Some(url) = banner_url {
        let ext = ext_from_url(url).unwrap_or("jpg");
        if let Err(e) = download_image_archival(client, url, asset_dir, "banner", ext).await {
            warn!("Kick banner: {e}");
        }
    }
    Ok(v)
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
/// - Broadcaster name colour → `asset_dir/name_color.txt` (Helix `chat/color`)
/// Returns `true` if the channel icon/banner fetch succeeded (badges/emotes/colour
/// are best-effort and don't affect the result). The 24h "fetched" stamp is written
/// **only on success**, so a failed fetch (e.g. empty/invalid `broadcaster_id`,
/// API error) is retried instead of being blocked for a day.
pub async fn run_twitch_assets(
    client: &Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    asset_dir: &Path,
    platform_dir: &Path,
    about: Option<&AboutSink>,
) -> bool {
    let mut description: Option<String> = None;
    let ok = match fetch_twitch_channel_assets(client, client_id, token, broadcaster_id, asset_dir)
        .await
    {
        Ok(desc) => {
            description = Some(desc);
            true
        }
        Err(e) => {
            warn!("Twitch channel assets ({broadcaster_id}): {e}");
            false
        }
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Err(e) =
        fetch_twitch_badges(client, client_id, token, broadcaster_id, asset_dir, platform_dir).await
    {
        warn!("Twitch badges ({broadcaster_id}): {e}");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Err(e) =
        fetch_twitch_emotes(client, client_id, token, broadcaster_id, asset_dir).await
    {
        warn!("Twitch emotes ({broadcaster_id}): {e}");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Err(e) = fetch_bttv_emotes(client, broadcaster_id, asset_dir, platform_dir).await {
        warn!("BTTV ({broadcaster_id}): {e}");
    }
    if let Err(e) = fetch_ffz_emotes(client, broadcaster_id, asset_dir, platform_dir).await {
        warn!("FFZ ({broadcaster_id}): {e}");
    }
    if let Err(e) = fetch_7tv_emotes(client, broadcaster_id, asset_dir, platform_dir).await {
        warn!("7TV ({broadcaster_id}): {e}");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Err(e) =
        fetch_twitch_name_color(client, client_id, token, broadcaster_id, asset_dir).await
    {
        warn!("Twitch name color ({broadcaster_id}): {e}");
    }
    // About-page archive (best-effort like badges/emotes): the Helix bio came
    // with the icon fetch; panels need one anonymous GQL round-trip.
    if let (Some(sink), Some(desc)) = (about, description) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Err(e) = fetch_twitch_about(client, broadcaster_id, desc, asset_dir, sink).await {
            warn!("Twitch about ({broadcaster_id}): {e}");
        }
    }
    if ok {
        write_fetched_stamp(asset_dir);
    }
    ok
}

/// Fetch the broadcaster's chosen Twitch chat name colour (Helix `chat/color`) and
/// cache it as `asset_dir/name_color.txt` (e.g. `#9146FF`). The chat replay uses
/// the IRC `color` tag directly, but this lets the Streams list tint a Twitch
/// channel's name with the streamer's own colour. No file is written when the user
/// hasn't set a colour (Helix returns an empty string), so the UI falls back to its
/// automatic palette.
async fn fetch_twitch_name_color(
    client: &Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    asset_dir: &Path,
) -> Result<()> {
    if broadcaster_id.is_empty() {
        return Ok(());
    }
    let url = format!("https://api.twitch.tv/helix/chat/color?user_id={broadcaster_id}");
    let resp = client
        .get(&url)
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Helix chat color: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let color = v["data"][0]["color"].as_str().unwrap_or("").trim().to_string();
    let dest = asset_dir.join("name_color.txt");
    // Read the previous colour first so we can log a transition (and only a real one).
    let old_color = std::fs::read_to_string(&dest)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if color.is_empty() {
        // Broadcaster cleared their colour — drop any stale cache so the UI reverts
        // to the automatic palette instead of tinting with a colour no longer used.
        let _ = tokio::fs::remove_file(&dest).await;
    } else {
        tokio::fs::create_dir_all(asset_dir).await?;
        let _ = tokio::fs::write(&dest, &color).await;
    }
    // Only log a transition once a baseline exists. On the first-ever fetch the
    // stamp is absent, so a name colour appearing for the first time is the baseline
    // (silent), not an "added" change — consistent with emote/icon/banner baselines.
    if old_color != color && assets_ever_fetched(asset_dir) {
        let action = if old_color.is_empty() {
            "added"
        } else if color.is_empty() {
            "removed"
        } else {
            "changed"
        };
        append_asset_changes(
            asset_dir,
            &[AssetChange {
                at: now_unix(),
                kind: "name_color".to_string(),
                provider: String::new(),
                action: action.to_string(),
                name: String::new(),
                id: String::new(),
                old: old_color,
                new: color,
            }],
        )
        .await;
    }
    Ok(())
}

/// Extract the channel banner URL from a parsed `ytInitialData` blob, trying the
/// newer `pageHeaderRenderer` path first, then the classic `c4TabbedHeaderRenderer`
/// path. Returns `None` when no banner is found (channel has no art set).
fn youtube_banner_from_page_data(data: &serde_json::Value) -> Option<String> {
    // New format (2024+): pageHeaderRenderer → imageBannerViewModel
    if let Some(sources) = data["header"]["pageHeaderRenderer"]["banner"]
        ["imageBannerViewModel"]["image"]["sources"]
        .as_array()
    {
        if let Some(url) = sources.last().and_then(|s| s["url"].as_str()) {
            return Some(normalize_yt_banner_url(url));
        }
    }
    // Legacy format: c4TabbedHeaderRenderer → banner → thumbnails
    if let Some(thumbs) = data["header"]["c4TabbedHeaderRenderer"]["banner"]["thumbnails"]
        .as_array()
    {
        if let Some(url) = thumbs.last().and_then(|t| t["url"].as_str()) {
            return Some(normalize_yt_banner_url(url));
        }
    }
    None
}

/// Request the widest available crop of a YouTube banner URL. YouTube banner URLs
/// on googleusercontent.com carry a `=w<N>-fcrop64=…` suffix; stripping it and
/// appending `=w2560` gives the full-width version (2560 px, the maximum YouTube
/// serves). Non-Google URLs are returned unchanged.
fn normalize_yt_banner_url(url: &str) -> String {
    if url.contains("googleusercontent.com") || url.contains("ggpht.com") {
        if let Some((base, _)) = url.split_once('=') {
            return format!("{base}=w2560");
        }
    }
    url.to_string()
}

/// Fetch the YouTube channel page and download the page-header banner from
/// `ytInitialData`. Works without a YouTube API key — just scrapes the channel
/// home page. Saves as `banner.<ext>` in `asset_dir/` with history preservation.
async fn fetch_youtube_page_banner(
    client: &Client,
    channel_url: &str,
    asset_dir: &Path,
    fingerprint: Option<&BrowserFingerprint>,
) -> Result<()> {
    let base = {
        let t = channel_url.trim().trim_end_matches('/');
        t.strip_suffix("/live")
            .or_else(|| t.strip_suffix("/streams"))
            .or_else(|| t.strip_suffix("/community"))
            .unwrap_or(t)
            .to_string()
    };
    let rb = client
        .get(&base)
        .query(&[("hl", "en"), ("gl", "US")])
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Cookie", "CONSENT=YES+1; SOCS=CAI");
    let rb = if let Some(fp) = fingerprint {
        fp.apply_yt_nav_headers(rb)
    } else {
        rb
    };
    let resp = rb.send().await?;
    if !resp.status().is_success() {
        bail!("YouTube channel page: {}", resp.status());
    }
    let body = resp.text().await?;
    let data = crate::detectors::extract_json_after(&body, "ytInitialData")
        .ok_or_else(|| anyhow::anyhow!("ytInitialData not found"))?;
    let banner_url = youtube_banner_from_page_data(&data)
        .ok_or_else(|| anyhow::anyhow!("no banner found in ytInitialData"))?;
    let ext = ext_from_url(&banner_url).unwrap_or("jpg");
    tokio::fs::create_dir_all(asset_dir).await?;
    download_image_archival(client, &banner_url, asset_dir, "banner", ext).await
}

/// Run YouTube channel asset fetches. Tries two approaches:
/// 1. YouTube Data API (icon + banner + branding) when `api_key` and `channel_id`
///    are both non-empty.
/// 2. Page-scrape banner (fetches the channel home page, extracts the wide
///    page-header banner from `ytInitialData`) — works without an API key.
///
/// The page-scrape banner is a **fallback**: it runs only when the API path did
/// not already write a banner. The two sources expose different banner images
/// (the API's `bannerExternalUrl` vs the page-header banner), so writing both on
/// every fetch made them overwrite each other and spam the change history with
/// phantom "banner replaced" entries. The 24 h stamp is written only when at
/// least one approach succeeds.
pub async fn run_youtube_assets(
    client: &Client,
    api_key: &str,
    channel_id: &str,
    channel_url: &str,
    asset_dir: &Path,
    fingerprint: Option<&BrowserFingerprint>,
    about: Option<&AboutSink>,
) -> bool {
    let mut any_ok = false;
    let mut api_set_banner = false;
    let mut api_description: Option<String> = None;

    if !api_key.is_empty() && !channel_id.is_empty() {
        match fetch_youtube_channel_assets(client, api_key, channel_id, asset_dir).await {
            Ok((banner_set, description)) => {
                any_ok = true;
                api_set_banner = banner_set;
                api_description = Some(description);
            }
            Err(e) => warn!("YouTube channel assets ({channel_id}): {e}"),
        }
    }

    // Fallback only: skip the page scrape entirely when the API already supplied a
    // banner, so a single channel never alternates between two banner sources.
    if !api_set_banner && !channel_url.is_empty() {
        match fetch_youtube_page_banner(client, channel_url, asset_dir, fingerprint).await {
            Ok(()) => any_ok = true,
            Err(e) if !any_ok => warn!("YouTube page banner ({channel_url}): {e}"),
            Err(_) => {}
        }
    }

    // About-page archive (best-effort): API description + /about page links.
    if let Some(sink) = about
        && let Err(e) = fetch_youtube_about(
            client,
            channel_url,
            api_description,
            fingerprint,
            asset_dir,
            sink,
        )
        .await
    {
        warn!("YouTube about ({channel_url}): {e}");
    }

    if any_ok {
        write_fetched_stamp(asset_dir);
    }
    any_ok
}

/// Run Kick channel asset fetches (icon, banner) and, when `about` is given,
/// archive the bio + social links from the SAME v2 response (zero extra
/// requests). Stamps only on success.
pub async fn run_kick_assets(
    client: &Client,
    slug: &str,
    asset_dir: &Path,
    about: Option<&AboutSink>,
) -> bool {
    match fetch_kick_channel_assets(client, slug, asset_dir).await {
        Ok(v) => {
            if let Some(sink) = about {
                let (bio, links) = kick_about_from_channel_json(&v);
                if let Err(e) =
                    persist_about_snapshot(client, asset_dir, sink, bio, Vec::new(), links, v, false)
                        .await
                {
                    warn!("Kick about ({slug}): {e}");
                }
            }
            write_fetched_stamp(asset_dir);
            true
        }
        Err(e) => {
            warn!("Kick channel assets ({slug}): {e}");
            false
        }
    }
}

// ---------- About page archive ----------

/// One Twitch panel / generic about-page block, persisted as JSON in
/// `about_snapshot.panels_json`. All fields default so older snapshots keep
/// deserializing when new per-panel fields are added later.
#[derive(serde::Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AboutPanel {
    #[serde(default)]
    pub title: String,
    /// Twitch panel bodies are markdown; other platforms use plain text here.
    #[serde(default)]
    pub description_md: String,
    #[serde(default)]
    pub image_url: String,
    /// fnv64 of the downloaded image bytes; empty = not downloaded (hashing
    /// then falls back to `image_url`).
    #[serde(default)]
    pub image_hash: String,
    /// Absolute path under the account's `about/` dir; empty = no image.
    #[serde(default)]
    pub image_path: String,
    #[serde(default)]
    pub link: String,
}

/// One external link from an about page (persisted in `links_json`).
#[derive(serde::Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AboutLink {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
}

/// Everything a platform about-step needs to persist a snapshot from inside
/// the spawned asset task.
pub struct AboutSink {
    pub store: std::sync::Arc<crate::store::Store>,
    pub channel_id: i64,
    pub platform: String, // Platform::as_str()
    pub account: String,  // account_slug of the instance URL
}

/// Deterministic version hash over the about-page CONTENT: description +
/// per-panel (title, body, link, image identity) + links. Field values are
/// trimmed and joined with `\x1f`, records with `\x1e`, hashed with fnv64.
/// A panel's image identity is its byte hash when downloaded, else its URL —
/// so CDN URL churn serving identical bytes does NOT create a new version.
pub fn about_content_hash(description: &str, panels: &[AboutPanel], links: &[AboutLink]) -> String {
    let mut s = String::new();
    s.push_str(description.trim());
    for p in panels {
        s.push('\x1e');
        let img = if p.image_hash.is_empty() { p.image_url.trim() } else { &p.image_hash };
        for part in [p.title.trim(), p.description_md.trim(), p.link.trim(), img] {
            s.push_str(part);
            s.push('\x1f');
        }
    }
    for l in links {
        s.push('\x1e');
        s.push_str(l.title.trim());
        s.push('\x1f');
        s.push_str(l.url.trim());
    }
    crate::detectors::fnv64(s.as_bytes()).to_string()
}

/// Parse Twitch GQL `user.panels` into panels, tolerating schema drift: only
/// entries that expose at least one DefaultPanel field are kept, null/missing
/// fields stay empty, non-panel garbage is skipped. Never panics.
pub(crate) fn twitch_panels_from_gql(v: &serde_json::Value) -> Vec<AboutPanel> {
    let Some(arr) = v["data"]["user"]["panels"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|p| {
            if !p.is_object() {
                return None;
            }
            let panel = AboutPanel {
                title: p["title"].as_str().unwrap_or("").to_string(),
                description_md: p["description"].as_str().unwrap_or("").to_string(),
                image_url: p["imageURL"].as_str().unwrap_or("").to_string(),
                link: p["linkURL"].as_str().unwrap_or("").to_string(),
                ..Default::default()
            };
            // ExtensionPanel etc. come back with all DefaultPanel fields null.
            if panel.title.is_empty()
                && panel.description_md.is_empty()
                && panel.image_url.is_empty()
                && panel.link.is_empty()
            {
                None
            } else {
                Some(panel)
            }
        })
        .collect()
}

/// Extract (bio, social links) from a Kick v2 channel JSON blob. Bare handles
/// in the flat social fields are mapped to full profile URLs; empty fields are
/// skipped; a missing `user` object yields `("", [])`.
pub(crate) fn kick_about_from_channel_json(v: &serde_json::Value) -> (String, Vec<AboutLink>) {
    let user = &v["user"];
    let bio = user["bio"].as_str().unwrap_or("").trim().to_string();
    let mut links = Vec::new();
    for (field, base) in [
        ("instagram", "https://instagram.com/"),
        ("twitter", "https://twitter.com/"),
        ("youtube", "https://youtube.com/"),
        ("discord", "https://discord.gg/"),
        ("tiktok", "https://tiktok.com/@"),
        ("facebook", "https://facebook.com/"),
    ] {
        let raw = user[field].as_str().unwrap_or("").trim();
        if raw.is_empty() {
            continue;
        }
        let url = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw.to_string()
        } else {
            format!("{base}{}", raw.trim_start_matches('@'))
        };
        links.push(AboutLink { title: field.to_string(), url });
    }
    (bio, links)
}

/// Depth-first search for the first object stored under `key` anywhere in the
/// tree. The YouTube about node moves around inside `ytInitialData` between
/// layout generations, so fixed index paths are too brittle.
pub(crate) fn find_key_object<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(hit) = map.get(key) {
                return Some(hit);
            }
            map.values().find_map(|c| find_key_object(c, key))
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(|c| find_key_object(c, key)),
        _ => None,
    }
}

/// Unwrap a YouTube `/redirect?...&q=<encoded>` wrapper to the real target URL
/// (percent-decoded). Non-redirect URLs pass through unchanged.
pub(crate) fn unwrap_yt_redirect(url: &str) -> String {
    let is_redirect = url.starts_with("https://www.youtube.com/redirect")
        || url.starts_with("/redirect");
    if !is_redirect {
        return url.to_string();
    }
    let Some(q) = url.split_once('?').and_then(|(_, qs)| {
        qs.split('&').find_map(|kv| kv.strip_prefix("q="))
    }) else {
        return url.to_string();
    };
    // Minimal percent-decode (%XX → byte); '+' is literal in this parameter.
    let bytes = q.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                (bytes.get(i + 1).copied()).and_then(|c| (c as char).to_digit(16)),
                (bytes.get(i + 2).copied()).and_then(|c| (c as char).to_digit(16)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| q.to_string())
}

/// Extract (description, links) from a channel About page's `ytInitialData`,
/// trying the current `aboutChannelViewModel` first, then the legacy
/// `channelAboutFullMetadataRenderer`. `None` = no about node found at all.
pub(crate) fn youtube_about_from_page_data(
    data: &serde_json::Value,
) -> Option<(String, Vec<AboutLink>)> {
    if let Some(vm) = find_key_object(data, "aboutChannelViewModel") {
        let description = vm["description"].as_str().unwrap_or("").to_string();
        let mut links = Vec::new();
        if let Some(arr) = vm["links"].as_array() {
            for l in arr {
                let l = &l["channelExternalLinkViewModel"];
                let title = l["title"]["content"].as_str().unwrap_or("").to_string();
                let url = l["link"]["content"].as_str().unwrap_or("").trim().to_string();
                if !url.is_empty() {
                    let url = if url.starts_with("http") { url } else { format!("https://{url}") };
                    links.push(AboutLink { title, url: unwrap_yt_redirect(&url) });
                }
            }
        }
        return Some((description, links));
    }
    if let Some(r) = find_key_object(data, "channelAboutFullMetadataRenderer") {
        let description = r["description"]["simpleText"].as_str().unwrap_or("").to_string();
        let mut links = Vec::new();
        if let Some(arr) = r["primaryLinks"].as_array() {
            for l in arr {
                let title = l["title"]["simpleText"].as_str().unwrap_or("").to_string();
                let url = l["navigationEndpoint"]["urlEndpoint"]["url"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if !url.is_empty() {
                    links.push(AboutLink { title, url: unwrap_yt_redirect(&url) });
                }
            }
        }
        return Some((description, links));
    }
    None
}

/// [`crate::detectors`]' post-image downloader's twin for the `about/` subdir:
/// download, hash the bytes with fnv64, store content-addressed as
/// `{hash}.{ext}` (identical bytes reuse the existing file). `None` = failure.
async fn download_about_image(
    client: &Client,
    url: &str,
    about_dir: &Path,
) -> Option<(String, PathBuf)> {
    let ext = ext_from_url(url).unwrap_or("png");
    let tmp = about_dir.join(format!("tmp.{ext}"));
    download_image(client, url, &tmp).await.ok()?;
    let bytes = tokio::fs::read(&tmp).await.ok()?;
    let hash = crate::detectors::fnv64(&bytes).to_string();
    let dest = about_dir.join(format!("{hash}.{ext}"));
    if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(&tmp).await;
    } else if tokio::fs::rename(&tmp, &dest).await.is_err() {
        let _ = tokio::fs::write(&dest, &bytes).await;
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    Some((hash, dest))
}

/// Download panel images into `asset_dir/about/`, hash the content, and record
/// the snapshot (new DB version only when the content actually changed).
///
/// `degraded` marks a round where an OPTIONAL enrichment source failed (Twitch
/// GQL panels, YouTube about-scrape links): such a capture may only ever be
/// the FIRST baseline — over an existing snapshot it is skipped entirely
/// (not even a `last_checked_at` bump, since the content is unverified). This
/// prevents version flip-flop when the enrichment source is temporarily down.
///
/// A genuine new version over an existing baseline also appends an
/// `asset_changes.jsonl` line (`kind: "about"`), so the Asset history window
/// lists the change; the first-ever capture stays silent like all baselines.
#[allow(clippy::too_many_arguments)]
async fn persist_about_snapshot(
    client: &Client,
    asset_dir: &Path,
    sink: &AboutSink,
    description: String,
    mut panels: Vec<AboutPanel>,
    links: Vec<AboutLink>,
    raw: serde_json::Value,
    degraded: bool,
) -> Result<()> {
    if degraded
        && sink
            .store
            .about_snapshot_exists(sink.channel_id, &sink.platform, &sink.account)?
    {
        return Ok(());
    }
    let about_dir = asset_dir.join("about");
    if panels.iter().any(|p| !p.image_url.is_empty()) {
        tokio::fs::create_dir_all(&about_dir).await?;
    }
    for p in &mut panels {
        if p.image_url.is_empty() {
            continue;
        }
        if let Some((hash, path)) = download_about_image(client, &p.image_url, &about_dir).await {
            p.image_hash = hash;
            p.image_path = path.to_string_lossy().into_owned();
        } else {
            warn!("about panel image failed: {}", p.image_url);
        }
    }
    let content_hash = about_content_hash(&description, &panels, &links);
    let outcome = sink.store.about_snapshot_record(&crate::store::NewAboutSnapshot {
        channel_id: sink.channel_id,
        platform: sink.platform.clone(),
        account: sink.account.clone(),
        content_hash: content_hash.clone(),
        description,
        panels_json: serde_json::to_string(&panels).unwrap_or_else(|_| "[]".into()),
        links_json: serde_json::to_string(&links).unwrap_or_else(|_| "[]".into()),
        raw_json: raw.to_string(),
    })?;
    if outcome.inserted && let Some(prev) = outcome.prev_hash {
        append_asset_changes(
            asset_dir,
            &[AssetChange {
                at: now_unix(),
                kind: "about".to_string(),
                provider: String::new(),
                action: "changed".to_string(),
                name: String::new(),
                id: String::new(),
                old: prev,
                new: content_hash,
            }],
        )
        .await;
    }
    Ok(())
}

/// Fetch a broadcaster's public About panels via anonymous Twitch GQL (the
/// same read-only transport recovery uses for `seekPreviewsURL`). Returns the
/// parsed panels plus the raw GQL response for `raw_json`.
async fn fetch_twitch_panels_gql(
    client: &Client,
    broadcaster_id: &str,
) -> Result<(Vec<AboutPanel>, serde_json::Value)> {
    let query = format!(
        "query{{user(id:\"{broadcaster_id}\"){{panels{{__typename id \
         ... on DefaultPanel{{title imageURL linkURL description}}}}}}}}"
    );
    let resp = client
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", crate::recovery::GQL_CLIENT_ID)
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Twitch GQL panels: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    if v["data"]["user"].is_null() {
        bail!("Twitch GQL panels: no user for id {broadcaster_id}");
    }
    Ok((twitch_panels_from_gql(&v), v))
}

/// Archive the Twitch about page: the Helix `description` (already fetched
/// with the icon/banner) plus panels via anonymous GQL. A GQL failure degrades
/// the round (baseline-only persist).
async fn fetch_twitch_about(
    client: &Client,
    broadcaster_id: &str,
    description: String,
    asset_dir: &Path,
    sink: &AboutSink,
) -> Result<()> {
    let (panels, raw, degraded) = match fetch_twitch_panels_gql(client, broadcaster_id).await {
        Ok((panels, raw)) => (panels, raw, false),
        Err(e) => {
            warn!("Twitch panels ({broadcaster_id}): {e}");
            (Vec::new(), serde_json::Value::Null, true)
        }
    };
    persist_about_snapshot(client, asset_dir, sink, description, panels, Vec::new(), raw, degraded)
        .await
}

/// Archive the YouTube about page: description from the Data API response
/// (when the API path ran) with the `/about` page scrape supplying links (and
/// the description fallback). A scrape miss degrades the round.
async fn fetch_youtube_about(
    client: &Client,
    channel_url: &str,
    api_description: Option<String>,
    fingerprint: Option<&BrowserFingerprint>,
    asset_dir: &Path,
    sink: &AboutSink,
) -> Result<()> {
    let base = {
        let t = channel_url.trim().trim_end_matches('/');
        t.strip_suffix("/live")
            .or_else(|| t.strip_suffix("/streams"))
            .or_else(|| t.strip_suffix("/community"))
            .unwrap_or(t)
            .to_string()
    };
    let mut scraped: Option<(String, Vec<AboutLink>)> = None;
    let mut raw = serde_json::Value::Null;
    if !base.is_empty() {
        let rb = client
            .get(format!("{base}/about"))
            .query(&[("hl", "en"), ("gl", "US")])
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cookie", "CONSENT=YES+1; SOCS=CAI");
        let rb = if let Some(fp) = fingerprint { fp.apply_yt_nav_headers(rb) } else { rb };
        match rb.send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.text().await
                    && let Some(data) = crate::detectors::extract_json_after(&body, "ytInitialData")
                {
                    if let Some(hit) = youtube_about_from_page_data(&data) {
                        raw = find_key_object(&data, "aboutChannelViewModel")
                            .or_else(|| find_key_object(&data, "channelAboutFullMetadataRenderer"))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        scraped = Some(hit);
                    }
                }
            }
            Ok(resp) => warn!("YouTube about page ({base}): {}", resp.status()),
            Err(e) => warn!("YouTube about page ({base}): {e}"),
        }
    }
    let degraded = scraped.is_none();
    let (scrape_desc, links) = scraped.unwrap_or_default();
    let description = api_description.filter(|d| !d.trim().is_empty()).unwrap_or(scrape_desc);
    if description.trim().is_empty() && links.is_empty() {
        // Nothing from either source — not even worth a degraded baseline.
        bail!("no about content from API or scrape");
    }
    persist_about_snapshot(client, asset_dir, sink, description, Vec::new(), links, raw, degraded)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique temp directory for a test. Combines the pid, a
    /// process-lifetime counter, and a nanosecond timestamp so a directory left
    /// behind by a *panicking* run (whose end-of-test cleanup never runs) can
    /// never be reused — even if the OS recycles the pid — which would otherwise
    /// let stale `asset_changes.jsonl` lines leak into a later run's assertions.
    fn unique_test_dir(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join(format!("{prefix}-{}-{n}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn about_hash_stable_and_sensitive() {
        let panels = vec![AboutPanel {
            title: "Schedule".into(),
            description_md: "Mon-Fri".into(),
            image_url: "https://cdn/img.png".into(),
            link: "https://example.com".into(),
            ..Default::default()
        }];
        let links = vec![AboutLink { title: "twitter".into(), url: "https://x.com/a".into() }];
        let base = about_content_hash("bio", &panels, &links);
        assert_eq!(base, about_content_hash("bio", &panels, &links), "deterministic");
        assert_eq!(base, about_content_hash("  bio  ", &panels, &links), "trims fields");
        assert_ne!(base, about_content_hash("other bio", &panels, &links));
        let mut p2 = panels.clone();
        p2[0].title = "New title".into();
        assert_ne!(base, about_content_hash("bio", &p2, &links));
        let mut l2 = links.clone();
        l2[0].url = "https://x.com/b".into();
        assert_ne!(base, about_content_hash("bio", &panels, &l2));
        // With a byte hash present, the (churning) CDN URL no longer matters…
        let mut p3 = panels.clone();
        p3[0].image_hash = "1234".into();
        let hashed = about_content_hash("bio", &p3, &links);
        let mut p4 = p3.clone();
        p4[0].image_url = "https://cdn/rotated-url.png".into();
        assert_eq!(hashed, about_content_hash("bio", &p4, &links), "image_hash beats image_url");
        // …but without one, a URL change does.
        let mut p5 = panels.clone();
        p5[0].image_url = "https://cdn/rotated-url.png".into();
        assert_ne!(base, about_content_hash("bio", &p5, &links));
    }

    #[test]
    fn twitch_panels_parse_and_drift() {
        let v = serde_json::json!({"data": {"user": {"panels": [
            {"__typename": "DefaultPanel", "id": "1", "title": "Schedule",
             "imageURL": "https://cdn/p1.png", "linkURL": "https://example.com",
             "description": "**Mon-Fri** 18:00"},
            {"__typename": "ExtensionPanel", "id": "2", "title": null,
             "imageURL": null, "linkURL": null, "description": null},
            {"__typename": "DefaultPanel", "id": "3", "title": null,
             "imageURL": "https://cdn/p3.png", "linkURL": null, "description": null},
            "garbage-entry",
        ]}}});
        let panels = twitch_panels_from_gql(&v);
        assert_eq!(panels.len(), 2, "extension panel + garbage skipped");
        assert_eq!(panels[0].title, "Schedule");
        assert_eq!(panels[0].description_md, "**Mon-Fri** 18:00");
        assert_eq!(panels[1].image_url, "https://cdn/p3.png");
        assert_eq!(panels[1].title, "", "null fields stay empty, panel kept");
        // No user / no panels → empty, never a panic.
        assert!(twitch_panels_from_gql(&serde_json::json!({})).is_empty());
        assert!(twitch_panels_from_gql(&serde_json::json!({"data": {"user": null}})).is_empty());
    }

    #[test]
    fn kick_about_extracts_bio_and_socials() {
        let v = serde_json::json!({"user": {
            "bio": "  VTuber streaming rhythm games  ",
            "instagram": "@somebody",
            "twitter": "somebody",
            "discord": "https://discord.gg/abc123",
            "youtube": "",
            "tiktok": null,
        }});
        let (bio, links) = kick_about_from_channel_json(&v);
        assert_eq!(bio, "VTuber streaming rhythm games");
        assert_eq!(links.len(), 3, "empty/null socials skipped");
        let by = |t: &str| links.iter().find(|l| l.title == t).unwrap().url.clone();
        assert_eq!(by("instagram"), "https://instagram.com/somebody", "@handle mapped to URL");
        assert_eq!(by("twitter"), "https://twitter.com/somebody");
        assert_eq!(by("discord"), "https://discord.gg/abc123", "full URLs pass through");
        // Missing user object.
        let (bio, links) = kick_about_from_channel_json(&serde_json::json!({}));
        assert!(bio.is_empty() && links.is_empty());
    }

    #[test]
    fn youtube_about_new_and_legacy_shapes() {
        // Current layout: aboutChannelViewModel nested somewhere in the tree.
        let new = serde_json::json!({"onResponseReceivedEndpoints": [{"whatever": {
            "aboutChannelViewModel": {
                "description": "I stream things.",
                "links": [
                    {"channelExternalLinkViewModel": {
                        "title": {"content": "Twitter"},
                        "link": {"content": "twitter.com/someone"}}},
                    {"channelExternalLinkViewModel": {
                        "title": {"content": "Shop"},
                        "link": {"content": "https://www.youtube.com/redirect?event=channel_description&q=https%3A%2F%2Fshop.example.com%2Fmerch"}}},
                ],
            }}}]});
        let (desc, links) = youtube_about_from_page_data(&new).unwrap();
        assert_eq!(desc, "I stream things.");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].url, "https://twitter.com/someone", "scheme added");
        assert_eq!(links[1].url, "https://shop.example.com/merch", "redirect unwrapped");

        // Legacy layout.
        let legacy = serde_json::json!({"contents": {"x": {
            "channelAboutFullMetadataRenderer": {
                "description": {"simpleText": "Old style about."},
                "primaryLinks": [{
                    "title": {"simpleText": "Website"},
                    "navigationEndpoint": {"urlEndpoint": {"url": "https://example.com"}}}],
            }}}});
        let (desc, links) = youtube_about_from_page_data(&legacy).unwrap();
        assert_eq!(desc, "Old style about.");
        assert_eq!(links[0].title, "Website");

        assert!(youtube_about_from_page_data(&serde_json::json!({"no": "about"})).is_none());
        // unwrap_yt_redirect passthrough.
        assert_eq!(unwrap_yt_redirect("https://example.com/a?q=x"), "https://example.com/a?q=x");
    }

    #[test]
    fn about_panel_serde_round_trip() {
        let panels = vec![
            AboutPanel {
                title: "A".into(),
                description_md: "body".into(),
                image_url: "u".into(),
                image_hash: "h".into(),
                image_path: "p".into(),
                link: "l".into(),
            },
            AboutPanel::default(),
        ];
        let json = serde_json::to_string(&panels).unwrap();
        let back: Vec<AboutPanel> = serde_json::from_str(&json).unwrap();
        assert_eq!(panels, back);
        // Forward-compat: unknown fields tolerated, missing fields default.
        let sparse: Vec<AboutPanel> =
            serde_json::from_str(r#"[{"title":"T","future_field":123}]"#).unwrap();
        assert_eq!(sparse[0].title, "T");
        assert_eq!(sparse[0].image_hash, "");
    }

    #[test]
    fn refetch_freshness_round_trip() {
        let dir = unique_test_dir("sa-assets");
        std::fs::create_dir_all(&dir).unwrap();

        // No stamp → must refetch (this is what makes a failed fetch retry, since the
        // stamp is now only written on success).
        assert!(should_refetch_assets(&dir));

        // A fresh stamp blocks refetch for 24h.
        write_fetched_stamp(&dir);
        assert!(!should_refetch_assets(&dir));

        // A stale (>24h) stamp refetches again.
        std::fs::write(
            dir.join(".assets_fetched_at"),
            (now_unix() - 90_000).to_string(),
        )
        .unwrap();
        assert!(should_refetch_assets(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Helper: list the archived variant filenames for a stem under history/.
    fn history_variants(dir: &Path, stem: &str) -> Vec<String> {
        let prefix = format!("{stem}_");
        std::fs::read_dir(dir.join("history"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.starts_with(&prefix))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn archival_write_preserves_changed_versions() {
        let dir = unique_test_dir("sa-archival");
        std::fs::create_dir_all(&dir).unwrap();

        // First fetch: no history yet, canonical written.
        archive_and_write(&dir, "icon", "png", b"v1").await.unwrap();
        assert_eq!(std::fs::read(dir.join("icon.png")).unwrap(), b"v1");
        assert_eq!(history_variants(&dir, "icon").len(), 0);

        // Identical re-fetch: no-op, no spurious history entry.
        archive_and_write(&dir, "icon", "png", b"v1").await.unwrap();
        assert_eq!(history_variants(&dir, "icon").len(), 0);

        // Changed pfp: the old version is archived, the new becomes canonical.
        archive_and_write(&dir, "icon", "png", b"v2").await.unwrap();
        assert_eq!(std::fs::read(dir.join("icon.png")).unwrap(), b"v2");
        let variants = history_variants(&dir, "icon");
        assert_eq!(variants.len(), 1, "old version must be kept");
        // The archived bytes are the previous version — no media lost.
        let archived = dir.join("history").join(&variants[0]);
        assert_eq!(std::fs::read(archived).unwrap(), b"v1");

        // A different extension still replaces the canonical and archives the old
        // one (no leftover icon.png alongside the new icon.jpg).
        archive_and_write(&dir, "icon", "jpg", b"v3").await.unwrap();
        assert_eq!(std::fs::read(dir.join("icon.jpg")).unwrap(), b"v3");
        assert!(!dir.join("icon.png").exists(), "stale extension must be cleared");
        assert_eq!(history_variants(&dir, "icon").len(), 2);

        // Each change is logged to asset_changes.jsonl (v1→v2 and v2→v3).
        let log = read_asset_changes(&dir);
        assert_eq!(log.iter().filter(|c| c.kind == "icon").count(), 2);
        assert!(log.iter().all(|c| c.action == "changed"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn entry(name: &str, id: &str) -> EmoteManifestEntry {
        EmoteManifestEntry {
            name: name.to_string(),
            id: id.to_string(),
            ext: "webp".to_string(),
            shared: false,
        }
    }

    #[test]
    fn manifest_diff_detects_adds_and_removes() {
        let old = vec![entry("Keep", "1"), entry("Gone", "2"), entry("", "blank")];
        // Keep stays (id churn ignored), Gone removed, New added; blank code ignored.
        let new = vec![entry("Keep", "1b"), entry("New", "3"), entry("", "blank2")];
        let diff = diff_emote_manifest(&old, &new, "7tv", 1000);

        assert_eq!(diff.len(), 2, "only Gone (removed) + New (added)");
        // Deterministic, sorted by code: Gone < New.
        assert_eq!(diff[0].name, "Gone");
        assert_eq!(diff[0].action, "removed");
        assert_eq!(diff[0].provider, "7tv");
        assert_eq!(diff[0].at, 1000);
        assert_eq!(diff[1].name, "New");
        assert_eq!(diff[1].action, "added");

        // An unchanged code set (even with reordering / id churn) yields nothing.
        let same = vec![entry("New", "3"), entry("Keep", "9")];
        let same2 = vec![entry("Keep", "1"), entry("New", "zzz")];
        assert!(diff_emote_manifest(&same, &same2, "7tv", 1).is_empty());
    }

    #[tokio::test]
    async fn record_manifest_change_logs_and_snapshots() {
        let dir = unique_test_dir("sa-manifest");
        let emotes = dir.join("emotes");
        std::fs::create_dir_all(&emotes).unwrap();

        // First fetch: no prior manifest → baseline, nothing recorded.
        let v1 = vec![entry("Pog", "a"), entry("Kappa", "b")];
        record_manifest_change(&dir, "7tv", &v1).await;
        assert!(read_asset_changes(&dir).is_empty(), "first fetch is the baseline");
        // Simulate the caller writing the manifest.
        std::fs::write(
            emotes.join("7tv.json"),
            serde_json::to_string(&v1).unwrap(),
        )
        .unwrap();

        // Second fetch removes Kappa, adds Sadge.
        let v2 = vec![entry("Pog", "a"), entry("Sadge", "c")];
        record_manifest_change(&dir, "7tv", &v2).await;
        let log = read_asset_changes(&dir);
        assert_eq!(log.len(), 2);
        assert!(log.iter().any(|c| c.name == "Kappa" && c.action == "removed"));
        assert!(log.iter().any(|c| c.name == "Sadge" && c.action == "added"));
        // The prior manifest was snapshotted under emotes/history/.
        let snaps = history_variants(&emotes, "7tv");
        assert_eq!(snaps.len(), 1, "old manifest archived");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn record_manifest_change_bails_on_corrupt_old_manifest() {
        let dir = unique_test_dir("sa-manifest-corrupt");
        let emotes = dir.join("emotes");
        std::fs::create_dir_all(&emotes).unwrap();

        // A truncated / corrupt prior manifest must be treated as "unknown", not as
        // an empty set — otherwise every current emote diffs as a fresh "add" and we
        // snapshot a file we couldn't even parse.
        std::fs::write(emotes.join("7tv.json"), b"{ this is not valid json").unwrap();

        let v = vec![entry("Pog", "a"), entry("Kappa", "b")];
        record_manifest_change(&dir, "7tv", &v).await;

        assert!(
            read_asset_changes(&dir).is_empty(),
            "corrupt manifest must not produce phantom add entries"
        );
        assert!(
            history_variants(&emotes, "7tv").is_empty(),
            "an unparseable manifest must not be snapshotted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_slug_per_platform() {
        use crate::models::Platform;
        // Twitch: login, lowercased, path/query stripped.
        assert_eq!(account_slug("https://twitch.tv/GEEGA", Platform::Twitch), "geega");
        assert_eq!(account_slug("https://www.twitch.tv/geega_alt/videos?x=1", Platform::Twitch), "geega_alt");
        // Kick: slug, lowercased.
        assert_eq!(account_slug("https://kick.com/CoolGuy", Platform::Kick), "coolguy");
        // YouTube: @handle, /channel/UC id, /c/name, /user/name.
        assert_eq!(account_slug("https://www.youtube.com/@LofiGirl/live", Platform::YouTube), "lofigirl");
        assert_eq!(
            account_slug("https://youtube.com/channel/UCabc123XYZ", Platform::YouTube),
            "ucabc123xyz"
        );
        assert_eq!(account_slug("https://youtube.com/c/SomeName", Platform::YouTube), "somename");
        assert_eq!(account_slug("https://youtube.com/user/OldName/videos", Platform::YouTube), "oldname");
        // Same account, two tools → identical slug (shared dir).
        assert_eq!(
            account_slug("https://twitch.tv/geega", Platform::Twitch),
            account_slug("https://TWITCH.tv/GEEGA/", Platform::Twitch)
        );
        // Generic / unparseable: sanitized excerpt + stable hash; distinct URLs differ.
        let a = account_slug("https://example.com/streams/a", Platform::Generic);
        let b = account_slug("https://example.com/streams/b", Platform::Generic);
        assert_ne!(a, b);
        assert_eq!(a, account_slug("https://example.com/streams/a", Platform::Generic));
        assert!(!a.is_empty() && !a.contains('/') && !a.contains(':'), "{a:?}");
    }

    #[test]
    fn migration_moves_legacy_payload_into_first_account_dir() {
        use crate::models::Platform;
        let root = unique_test_dir("sa-acct-migrate");
        let plat = root.join("GEEGA").join("twitch");
        std::fs::create_dir_all(plat.join("emotes")).unwrap();
        std::fs::create_dir_all(plat.join("posts")).unwrap();
        std::fs::create_dir_all(plat.join("schedule_src")).unwrap();
        std::fs::write(plat.join("icon.png"), b"i").unwrap();
        std::fs::write(plat.join("name_color.txt"), b"#123456").unwrap();
        std::fs::write(plat.join("posts").join("p.jpg"), b"p").unwrap();
        std::fs::write(plat.join("schedule_src").join("s.png"), b"s").unwrap();
        // An unmatched channel dir must be left untouched.
        let orphan = root.join("Renamed").join("twitch");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("icon.png"), b"o").unwrap();

        let mut urls = std::collections::HashMap::new();
        urls.insert(
            ("GEEGA".to_string(), Platform::Twitch),
            "https://twitch.tv/geega".to_string(),
        );
        migrate_assets_root(&root, &urls);

        let acct = plat.join("geega");
        assert!(acct.join("icon.png").is_file(), "icon moved into the account dir");
        assert!(acct.join("name_color.txt").is_file());
        assert!(acct.join("emotes").is_dir());
        assert!(!plat.join("icon.png").exists(), "legacy copy gone");
        // DB-referenced dirs must NOT move.
        assert!(plat.join("posts").join("p.jpg").is_file());
        assert!(plat.join("schedule_src").join("s.png").is_file());
        // Unmatched channel untouched.
        assert!(orphan.join("icon.png").is_file());
        // Idempotent: stamp written; a second run with different urls is a no-op.
        assert!(root.join(".accounts_migrated").is_file());
        let mut urls2 = std::collections::HashMap::new();
        urls2.insert(
            ("Renamed".to_string(), Platform::Twitch),
            "https://twitch.tv/other".to_string(),
        );
        migrate_assets_root(&root, &urls2);
        assert!(orphan.join("icon.png").is_file(), "stamped run must not touch anything");

        let _ = std::fs::remove_dir_all(&root);
    }
}
