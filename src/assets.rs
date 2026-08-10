use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

use crate::browser_ua::BrowserFingerprint;
use crate::iomon::Cat;
use crate::models::now_unix;

// ---------- Cache stamps ----------

/// True if the channel asset directory has not been fetched in the last 24 hours.
pub fn should_refetch_assets(asset_dir: &Path) -> bool {
    let stamp = asset_dir.join(".assets_fetched_at");
    match crate::iomon::fs::read_to_string_sync(Cat::AssetCache, &stamp) {
        Ok(s) => {
            let fetched: i64 = s.trim().parse().unwrap_or(0);
            now_unix() - fetched > 86_400
        }
        Err(_) => true,
    }
}

fn write_fetched_stamp(asset_dir: &Path) {
    let _ = crate::iomon::fs::write_sync(Cat::AssetCache, asset_dir.join(".assets_fetched_at"), now_unix().to_string());
}

/// True if this channel's assets have been fetched at least once (the freshness
/// stamp exists). Used to suppress change-log noise on the very first fetch: the
/// baseline run establishes the initial state, so a name colour appearing for the
/// first time is not a "change". The stamp is written only at the END of a fetch
/// run, so during the first run this returns false and the first-seen colour is
/// recorded silently — matching how emote/icon/banner baselines are silent.
fn assets_ever_fetched(asset_dir: &Path) -> bool {
    crate::iomon::fs::exists_sync(Cat::AssetCache, asset_dir.join(".assets_fetched_at"))
}

/// How long a shared, channel-independent asset set (global badges, a
/// provider's global emotes) stays fresh before it is refetched.
const GLOBAL_ASSET_TTL_SECS: i64 = 86_400;

/// True if `stamp` is missing, unreadable, unparseable, or older than
/// [`GLOBAL_ASSET_TTL_SECS`] — i.e. refetch. Every failure mode says "refetch"
/// deliberately: a corrupt stamp costs one extra fetch a day, whereas trusting
/// it would wedge the asset set forever.
fn global_asset_stale(stamp: &Path) -> bool {
    match crate::iomon::fs::read_to_string_sync(Cat::AssetCache, stamp) {
        Ok(s) => {
            s.trim().parse::<i64>().map(|t| now_unix() - t > GLOBAL_ASSET_TTL_SECS).unwrap_or(true)
        }
        Err(_) => true,
    }
}

/// Mark a shared asset set fetched now. Callers write this **only on success**,
/// so a failed fetch retries on the next pass instead of being blocked for a day.
fn write_global_asset_stamp(stamp: &Path) {
    if let Some(dir) = stamp.parent() {
        let _ = crate::iomon::fs::create_dir_all_sync(Cat::AssetCache, dir);
    }
    let _ = crate::iomon::fs::write_sync(Cat::AssetCache, stamp, now_unix().to_string());
}

fn global_badges_stamp(platform_dir: &Path) -> PathBuf {
    platform_dir.join("twitch").join(".global_badges_fetched_at")
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
        // No account-identity parser — the URL-hash fallback below keeps
        // distinct URLs from colliding.
        Platform::Nrk | Platform::Nebula | Platform::Generic => None,
    };
    let slug = match raw {
        Some(s) => crate::downloader::sanitize_filename(&s).to_lowercase(),
        None => url_fallback_slug(url),
    };
    if slug.is_empty() { url_fallback_slug(url) } else { slug }
}

/// YouTube account token from a channel URL: `@handle` (sans `@`), `/channel/UC…`
/// id, or a `/c/{name}` / `/user/{name}` path segment — all lowercased.
pub(crate) fn youtube_account_token(url: &str) -> Option<String> {
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

/// Follow a channel container rename: `channel_asset_dir`/`legacy_platform_dir`
/// key their ENTIRE tree off the channel's display name (`…/channel_assets/
/// {name}/…` — avatar, banner, emotes, badges, and the cached Twitch chat
/// name-color all live under it), so without this a rename silently orphans
/// every cached asset. It doesn't disappear from the UI immediately (this
/// session's in-memory caches are keyed by channel id, not name), but the next
/// cold start reads under the new name, finds nothing, and quietly falls back
/// to defaults — e.g. a manually-observed Twitch name colour reverting to the
/// generic palette. No-op if the sanitized names match (cosmetic-only change)
/// or a directory already sits at the destination (another channel's cache;
/// left alone rather than risk clobbering it — self-heals on the next fetch).
pub fn rename_channel_asset_dir(old_name: &str, new_name: &str) {
    let root = crate::app_paths::asset_cache_dir().join("channel_assets");
    let old_dir = root.join(crate::downloader::sanitize_filename(old_name));
    let new_dir = root.join(crate::downloader::sanitize_filename(new_name));
    if old_dir == new_dir
        || crate::iomon::fs::exists_sync(Cat::AssetCache, &new_dir)
        || !crate::iomon::fs::is_dir_sync(Cat::AssetCache, &old_dir)
    {
        return;
    }
    if let Err(e) = crate::iomon::fs::rename_sync(Cat::AssetCache, &old_dir, &new_dir) {
        warn!("rename_channel_asset_dir: {} -> {}: {e}", old_dir.display(), new_dir.display());
    }
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
    if !crate::iomon::fs::is_dir_sync(Cat::AssetCache, root) {
        return; // nothing fetched yet — first run populates account dirs directly
    }
    let stamp = root.join(".accounts_migrated");
    if crate::iomon::fs::exists_sync(Cat::AssetCache, &stamp) {
        return;
    }
    let Ok(channels) = crate::iomon::fs::read_dir_sync(Cat::AssetCache, root) else { return };
    for chan in channels.flatten() {
        let chan_dir = chan.path();
        if !crate::iomon::fs::is_dir_sync(Cat::AssetCache, &chan_dir) {
            continue;
        }
        let chan_key = chan.file_name().to_string_lossy().into_owned();
        for plat_name in ["twitch", "youtube", "kick", "generic"] {
            let plat_dir = chan_dir.join(plat_name);
            if !crate::iomon::fs::is_dir_sync(Cat::AssetCache, &plat_dir) {
                continue;
            }
            let platform = Platform::parse(plat_name);
            // Legacy payload present directly in the platform dir?
            let legacy: Vec<PathBuf> = crate::iomon::fs::read_dir_sync(Cat::AssetCache, &plat_dir)
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
            let _ = crate::iomon::fs::create_dir_all_sync(Cat::AssetCache, &account_dir);
            for src in legacy {
                let Some(fname) = src.file_name() else { continue };
                let dest = account_dir.join(fname);
                if crate::iomon::fs::exists_sync(Cat::AssetCache, &dest) {
                    continue; // a newer account-side copy exists — keep both, prefer it
                }
                if let Err(e) = crate::iomon::fs::rename_sync(Cat::AssetCache, &src, &dest) {
                    warn!("asset migration: could not move {} -> {}: {e}", src.display(), dest.display());
                }
            }
            tracing::info!(
                "asset migration: {chan_key}/{plat_name} -> per-account dir {}",
                account_dir.display()
            );
        }
    }
    let _ = crate::iomon::fs::write_sync(Cat::AssetCache, &stamp, now_unix().to_string());
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
    if let Ok(entries) = crate::iomon::fs::read_dir_sync(Cat::AssetCache, &root) {
        for e in entries.flatten() {
            let p = e.path();
            if crate::iomon::fs::is_dir_sync(Cat::AssetCache, &p)
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
    crate::iomon::fs::read_dir_sync(Cat::AssetCache, dir)
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

    if crate::iomon::fs::exists_sync(Cat::AssetCache, &out) {
        // Regenerate only if the source icon was updated after the last scale.
        let src_mtime = crate::iomon::fs::metadata_sync(Cat::AssetCache, &src).ok()?.modified().ok()?;
        let out_mtime = crate::iomon::fs::metadata_sync(Cat::AssetCache, &out).ok()?.modified().ok()?;
        if out_mtime >= src_mtime {
            return Some(out);
        }
    }

    let bytes = crate::iomon::fs::read_sync(Cat::AssetCache, &src).ok()?;
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
        crate::iomon::fs::create_dir_all(Cat::AssetCache, parent).await?;
    }
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {} for {}", resp.status(), url);
    }
    let bytes = resp.bytes().await?;
    crate::iomon::fs::write(Cat::AssetCache, dest, bytes).await?;
    Ok(())
}

/// The current canonical asset file `dir/{stem}.<ext>` (any extension), if one
/// exists. Matches `icon.png` but never the `history/` dir or an archived
/// `icon_<ts>.png` (those use `{stem}_`, not `{stem}.`).
async fn current_asset(dir: &Path, stem: &str) -> Option<PathBuf> {
    let prefix = format!("{stem}.");
    let mut rd = crate::iomon::fs::read_dir(Cat::AssetCache, dir).await.ok()?;
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
    crate::iomon::fs::create_dir_all(Cat::AssetCache, dir).await?;
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
        match crate::iomon::fs::read(Cat::AssetCache, &cur_path).await {
            // Unchanged since last fetch — leave everything as-is.
            Ok(cur) if cur == bytes => return Ok(()),
            // Changed — archive the old version before it's overwritten.
            Ok(_) => {
                let hist = dir.join("history");
                crate::iomon::fs::create_dir_all(Cat::AssetCache, &hist).await?;
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
                while crate::iomon::fs::try_exists(Cat::AssetCache, &archived).await.unwrap_or(false) {
                    n += 1;
                    archived = hist.join(format!("{stem}_{ts}_{n}.{cur_ext}"));
                }
                // Move the old canonical into history (rename; fall back to
                // copy+remove if the move fails). This also clears a stale
                // canonical whose extension differs from the new one.
                if crate::iomon::fs::rename(Cat::AssetCache, &cur_path, &archived).await.is_err() {
                    crate::iomon::fs::copy(Cat::AssetCache, &cur_path, &archived).await?;
                    let _ = crate::iomon::fs::remove_file(Cat::AssetCache, &cur_path).await;
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

    crate::iomon::fs::write(Cat::AssetCache, dir.join(format!("{stem}.{ext}")), bytes).await?;
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
/// The bits of a Helix Get Users response the channel asset fetch needs —
/// `broadcaster_type`/`created_at`/`login` ride along for free on the SAME
/// call already made for the icon/banner, so `run_twitch_assets` can cache
/// them (see [`crate::detectors::record_twitch_user_info`]) without any
/// extra request.
struct TwitchChannelInfo {
    description: String,
    login: String,
    broadcaster_type: String,
    created_at: String,
}

async fn fetch_twitch_channel_assets(
    client: &Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    asset_dir: &Path,
) -> Result<TwitchChannelInfo> {
    #[derive(Deserialize)]
    struct TwitchUser {
        login: String,
        profile_image_url: String,
        #[serde(default)]
        offline_image_url: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        broadcaster_type: String,
        #[serde(default)]
        created_at: String,
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

    crate::iomon::fs::create_dir_all(Cat::AssetCache, asset_dir).await?;

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
    Ok(TwitchChannelInfo {
        description: user.description,
        login: user.login,
        broadcaster_type: user.broadcaster_type,
        created_at: user.created_at,
    })
}

// ---------- Twitch chat usercard (live lookup) ----------

/// Result of a chat usercard's live Twitch lookup — the avatar is cached to
/// disk (so a second usercard open for the same user doesn't re-download),
/// `created_at` is Twitch's raw RFC3339 timestamp (the caller formats it).
pub struct UserCardInfo {
    pub avatar_path: Option<PathBuf>,
    pub created_at: Option<String>,
}

/// Fetch a chat usercard's live Twitch data by numeric user id: avatar image
/// (cached under `platform_assets_dir()/twitch/usercards/{user_id}/`) and
/// account-created date. One public Helix `/users` call — no special scope,
/// just the app token every other Helix call in this module already uses.
/// Called on-demand from the chat replay's username-click usercard, gated
/// behind the opt-in "fetch live Twitch info" setting.
pub async fn fetch_usercard_info(client_id: &str, token: &str, user_id: &str) -> Result<UserCardInfo> {
    #[derive(Deserialize)]
    struct TwitchUser {
        profile_image_url: String,
        created_at: String,
    }
    #[derive(Deserialize)]
    struct UsersResp {
        data: Vec<TwitchUser>,
    }

    let client = Client::new();
    let resp = client
        .get("https://api.twitch.tv/helix/users")
        .query(&[("id", user_id)])
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Helix users ({user_id}): {}", resp.status());
    }
    let r: UsersResp = resp.json().await?;
    let Some(user) = r.data.into_iter().next() else {
        bail!("no Twitch user found for id {user_id}");
    };

    let dir = crate::app_paths::platform_assets_dir()
        .join("twitch")
        .join("usercards")
        .join(user_id);
    let ext = ext_from_url(&user.profile_image_url).unwrap_or("jpg");
    let avatar_path = dir.join(format!("avatar.{ext}"));
    let mut have_avatar = crate::iomon::fs::exists_sync(Cat::AssetCache, &avatar_path);
    if !have_avatar {
        have_avatar = download_image(&client, &user.profile_image_url, &avatar_path).await.is_ok();
    }

    Ok(UserCardInfo {
        avatar_path: have_avatar.then_some(avatar_path),
        created_at: Some(user.created_at),
    })
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
            if crate::iomon::fs::exists_sync(Cat::AssetCache, &dest) {
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
    if global_asset_stale(&global_badges_stamp(platform_dir)) {
        let global_dir = platform_dir.join("twitch").join("global_badges");
        crate::iomon::fs::create_dir_all(Cat::AssetCache, &global_dir).await?;
        match fetch_helix_badges(
            client,
            client_id,
            token,
            "https://api.twitch.tv/helix/chat/badges/global",
            &global_dir,
        )
        .await
        {
            Ok(_) => write_global_asset_stamp(&global_badges_stamp(platform_dir)),
            Err(e) => warn!("global Twitch badges: {e}"),
        }
    }

    // Channel-specific badges go per-channel.
    if !broadcaster_id.is_empty() {
        let badge_dir = asset_dir.join("badges");
        crate::iomon::fs::create_dir_all(Cat::AssetCache, &badge_dir).await?;
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
    crate::iomon::fs::create_dir_all(Cat::AssetCache, &emote_dir).await?;

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
            let _ = crate::iomon::fs::write(Cat::AssetCache, asset_dir.join("emotes").join("twitch.json"), json).await;
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
    crate::iomon::fs::metadata_sync(Cat::AssetCache, path).map(|m| m.len() > 0).unwrap_or(false)
}

/// Sanitize an emote code for use as a filename component. Keeps alphanumerics,
/// underscores, and hyphens; replaces anything else with `_`.
pub(crate) fn sanitize_emote_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// Resolve a manifest entry's on-disk image path: try the current
/// `{id}_{sanitized_name}.{ext}` filename fetchers write, falling back to the
/// pre-rename `{id}.{ext}` form for files downloaded before that change.
/// Shared by every reader of an emote manifest (chat render-time lookup, the
/// Emote Properties viewer) so a future filename-scheme change only needs
/// updating here — this used to be duplicated per-caller, and the chat
/// renderer's copy fell out of sync with the fetchers' `{id}_{name}` rename,
/// silently breaking rendering for every emote downloaded since (2026-08-02).
pub(crate) fn resolve_emote_path(base: &Path, entry: &EmoteManifestEntry) -> PathBuf {
    let new_path = base.join(format!(
        "{}_{}.{}",
        entry.id,
        sanitize_emote_name(&entry.name),
        entry.ext
    ));
    if crate::iomon::fs::exists_sync(Cat::AssetCache, &new_path) {
        new_path
    } else {
        base.join(format!("{}.{}", entry.id, entry.ext))
    }
}

/// Every cached channel's Twitch first-party emote directory
/// (`channel_assets/{name}/twitch/{account}/emotes/twitch/`, plus the legacy
/// pre-account `channel_assets/{name}/twitch/emotes/twitch/`), across EVERY
/// channel this app has ever fetched assets for — not just one. Twitch lets
/// any subscriber use their sub emotes in any channel's chat, so a chat log
/// routinely references emotes that were never fetched for the channel whose
/// chat is open, only for whichever OTHER channel(s) the poster is
/// subscribed to. If that other channel also happens to be archived here,
/// the emote is already sitting on disk — this is the fallback search list
/// the chat renderer walks when an emote id isn't in the open channel's own
/// directory, so it still resolves instead of silently falling back to text.
pub(crate) fn all_twitch_emote_dirs() -> Vec<PathBuf> {
    let mut dirs =
        all_twitch_emote_dirs_under(&crate::app_paths::asset_cache_dir().join("channel_assets"));
    // Also fall back to emotes fetched on demand for a channel that isn't
    // monitored/archived here at all (see `twitch_emote_cdn_fetch`) — same
    // fallback mechanism, one more directory, no lookup-code changes needed.
    dirs.push(global_twitch_emote_dir());
    dirs
}

/// Testable core of [`all_twitch_emote_dirs`] (takes the `channel_assets`
/// root directly instead of the real app data dir).
pub(crate) fn all_twitch_emote_dirs_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(channels) = crate::iomon::fs::read_dir_sync(Cat::AssetCache, root) else { return out };
    for chan in channels.flatten() {
        let twitch_dir = chan.path().join("twitch");
        if !crate::iomon::fs::is_dir_sync(Cat::AssetCache, &twitch_dir) {
            continue;
        }
        // Legacy pre-account layout: emotes directly under the platform dir.
        let legacy_emotes = twitch_dir.join("emotes").join("twitch");
        if crate::iomon::fs::is_dir_sync(Cat::AssetCache, &legacy_emotes) {
            out.push(legacy_emotes);
        }
        let Ok(accounts) = crate::iomon::fs::read_dir_sync(Cat::AssetCache, &twitch_dir) else { continue };
        for acc in accounts.flatten() {
            let emote_dir = acc.path().join("emotes").join("twitch");
            if crate::iomon::fs::is_dir_sync(Cat::AssetCache, &emote_dir) {
                out.push(emote_dir);
            }
        }
    }
    out
}

/// Precomputed `{filename stem} -> path` index over every dir in
/// `all_twitch_emote_dirs()` (or a filtered subset), covering both the
/// current `{id}_{name}` and legacy `{id}` filename forms as-is — no id
/// parsing needed, since a lookup just needs to reconstruct the same
/// candidate string a writer would have used and probe the map for it.
///
/// This exists ONLY for performance: a chat log routinely repeats the same
/// handful of first-party emotes hundreds of times (a single spammed emote
/// can appear 3-4x per message across thousands of messages), and with
/// several dozen archived channels each contributing a fallback dir, doing a
/// filesystem `exists_sync` stat per (occurrence × fallback dir × extension ×
/// filename-form) for every miss made a large chat log's tail-first load take
/// **over a minute** (found 2026-08-02, the day the fallback search itself
/// was added) — directory-listing every fallback dir ONCE up front and then
/// doing an in-memory hashmap lookup per occurrence removes that
/// multiplication entirely.
pub(crate) fn index_emote_stems(dirs: &[PathBuf]) -> std::collections::HashMap<String, PathBuf> {
    let mut map = std::collections::HashMap::new();
    for dir in dirs {
        let Ok(entries) = crate::iomon::fs::read_dir_sync(Cat::AssetCache, dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                map.entry(stem.to_string()).or_insert(path);
            }
        }
    }
    map
}

/// Shared cache for first-party Twitch emotes fetched on demand for a
/// channel this app doesn't monitor/archive at all — see
/// `twitch_emote_cdn_fetch`. Platform-wide (not per-channel), same shape as
/// the 7TV/FFZ global emote caches.
pub(crate) fn global_twitch_emote_dir() -> PathBuf {
    crate::app_paths::platform_assets_dir().join("twitch").join("global_emotes")
}

/// `(dest, candidate urls)` for fetching a first-party Twitch emote directly
/// by numeric id, with no broadcaster/channel context at all — Twitch's
/// emote CDN serves images keyed purely by id. This is how an emote whose
/// home channel isn't monitored here (e.g. a poster's own sub emote, used in
/// some OTHER channel's chat — Twitch lets any subscriber use their emotes
/// anywhere) can still render 1:1 without adding that channel.
///
/// Two candidates, animated first: without a Helix "Get Channel Emotes" call
/// (which needs a broadcaster id we don't have for an unknown channel)
/// there's no way to know ahead of time whether an id is animated or
/// static — but the CDN itself answers that for free, since `animated/`
/// 404s outright for a static-only id rather than degrading to a still
/// frame (confirmed against the live CDN: id 25 "Kappa" 404s there). Same
/// try-in-order, first-success-wins shape `EmojiFetch`/`download_emoji_images`
/// already use for Twemoji's irregular FE0F naming — no architecture change
/// needed, just more candidates. `dest` stays a fixed `.png` regardless of
/// which candidate lands: the renderer sniffs actual image format from
/// bytes (`emote_anim::decode`), never from the extension, so a `.png`
/// holding GIF bytes decodes and animates correctly. `dest` is deterministic
/// in `(id, name)` so repeat occurrences of the same code in one log
/// collapse to a single fetch (see `parse_chat_chunk`'s `fetches.dedup()`),
/// and every occurrence's independently-set `pending` promotes together once
/// that one file lands.
pub(crate) fn twitch_emote_cdn_fetch(id: &str, name: &str) -> (PathBuf, Vec<String>) {
    let dest = global_twitch_emote_dir().join(format!("{id}_{}.png", sanitize_emote_name(name)));
    let urls = vec![
        format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/animated/dark/3.0"),
        format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/static/dark/3.0"),
    ];
    (dest, urls)
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
/// a synchronous `read_to_string`, as the UI's `read_asset_changes` does)
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
    if buf.is_empty() || crate::iomon::fs::create_dir_all(Cat::AssetCache, asset_dir).await.is_err() {
        return;
    }
    use tokio::io::AsyncWriteExt;
    let path = asset_dir.join("asset_changes.jsonl");
    let open = || {
        crate::iomon::fs::open_with(Cat::AssetCache, &path, |o| {
            o.create(true).append(true);
        })
    };
    if let Ok(mut f) = retry_transient(open).await {
        let _ = f.write_all(buf.as_bytes()).await;
        let _ = f.flush().await;
    }
}

/// Read a channel-platform's recorded asset changes (`asset_changes.jsonl`) in
/// chronological append order (oldest first). Malformed/blank lines are skipped;
/// a missing file yields an empty vec. Synchronous — the UI calls it directly on
/// popup-open (the file is tiny: a handful of lines per refetch).
pub fn read_asset_changes(asset_dir: &Path) -> Vec<AssetChange> {
    let Ok(s) = crate::iomon::fs::read_to_string_sync(Cat::AssetCache, asset_dir.join("asset_changes.jsonl")) else {
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
            match crate::iomon::fs::read_to_string(Cat::AssetCache, &manifest_path).await {
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
    if crate::iomon::fs::create_dir_all(Cat::AssetCache, &hist).await.is_ok() {
        let mut dest = hist.join(format!("{provider}_{at}.json"));
        let mut n = 1;
        while crate::iomon::fs::try_exists(Cat::AssetCache, &dest).await.unwrap_or(false) {
            n += 1;
            dest = hist.join(format!("{provider}_{at}_{n}.json"));
        }
        let _ = retry_transient(|| crate::iomon::fs::write(Cat::AssetCache, &dest, old_json.as_bytes())).await;
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
        crate::iomon::fs::create_dir_all(Cat::AssetCache, &dir).await?;
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
        crate::iomon::fs::create_dir_all(Cat::AssetCache, &global_dir).await?;
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
        crate::iomon::fs::create_dir_all(Cat::AssetCache, &manifest_dir).await?;
        // Record added/removed codes against the previous manifest before overwriting.
        record_manifest_change(asset_dir, "bttv", &manifest).await;
        if let Ok(json) = serde_json::to_string(&manifest) {
            let _ = crate::iomon::fs::write(Cat::AssetCache, manifest_dir.join("bttv.json"), json).await;
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
    crate::iomon::fs::create_dir_all(Cat::AssetCache, &global_dir).await?;

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
        crate::iomon::fs::create_dir_all(Cat::AssetCache, &manifest_dir).await?;
        record_manifest_change(asset_dir, "ffz", &manifest).await;
        if let Ok(json) = serde_json::to_string(&manifest) {
            let _ = crate::iomon::fs::write(Cat::AssetCache, manifest_dir.join("ffz.json"), json).await;
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
    crate::iomon::fs::create_dir_all(Cat::AssetCache, &global_dir).await?;

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
        crate::iomon::fs::create_dir_all(Cat::AssetCache, &manifest_dir).await?;
        record_manifest_change(asset_dir, "7tv", &manifest).await;
        if let Ok(json) = serde_json::to_string(&manifest) {
            let _ = crate::iomon::fs::write(Cat::AssetCache, manifest_dir.join("7tv.json"), json).await;
        }
    }

    Ok(())
}

// ---------- Third-party GLOBAL emote sets ----------

/// One emote from a provider's global set: what to record, and where to get it.
struct GlobalEmote {
    entry: EmoteManifestEntry,
    url: String,
}

/// Where a provider's global-emote manifest lives.
///
/// Beside the provider's shared image cache rather than under any channel:
/// these emotes belong to nobody in particular and every channel's chat
/// renders them, so filing them per-channel would mean N copies of one list
/// that all say the same thing.
pub(crate) fn global_emote_manifest(platform_dir: &Path, provider: &str) -> PathBuf {
    platform_dir.join(provider).join("global.json")
}

fn global_emotes_stamp(platform_dir: &Path, provider: &str) -> PathBuf {
    platform_dir.join(provider).join(".global_emotes_fetched_at")
}

/// 7TV's global set — the emotes every channel gets for free (`xdx`, `Clueless`,
/// …), which is why they never appear in a channel's own `emote_set`.
async fn seventv_global_emotes(client: &Client) -> Result<Vec<GlobalEmote>> {
    let resp = client.get("https://7tv.io/v3/emote-sets/global").send().await?;
    if !resp.status().is_success() {
        bail!("7TV globals: {}", resp.status());
    }
    Ok(parse_7tv_global(&resp.json().await?))
}

fn parse_7tv_global(v: &serde_json::Value) -> Vec<GlobalEmote> {
    let Some(emotes) = v["emotes"].as_array() else { return Vec::new() };
    emotes
        .iter()
        .filter_map(|e| {
            let id = e["id"].as_str()?;
            let name = e["name"].as_str()?;
            Some(GlobalEmote {
                entry: EmoteManifestEntry {
                    name: name.to_string(),
                    id: id.to_string(),
                    ext: "webp".to_string(),
                    shared: true,
                },
                url: format!("https://cdn.7tv.app/emote/{id}/4x.webp"),
            })
        })
        .collect()
}

async fn bttv_global_emotes(client: &Client) -> Result<Vec<GlobalEmote>> {
    // Same shape as the channel fetch's private `BttvEmote`; `default` on the
    // code for the same reason — one malformed emote must not abort the set.
    #[derive(Deserialize)]
    struct GlobalBttvEmote {
        id: String,
        #[serde(default)]
        code: String,
        #[serde(rename = "imageType")]
        image_type: String,
    }
    let resp = client.get("https://api.betterttv.net/3/cached/emotes/global").send().await?;
    if !resp.status().is_success() {
        bail!("BTTV globals: {}", resp.status());
    }
    let emotes: Vec<GlobalBttvEmote> = resp.json().await?;
    Ok(emotes
        .into_iter()
        .map(|e| GlobalEmote {
            url: format!("https://cdn.betterttv.net/emote/{}/3x.{}", e.id, e.image_type),
            entry: EmoteManifestEntry {
                name: e.code,
                id: e.id,
                ext: e.image_type,
                shared: true,
            },
        })
        .collect())
}

/// FFZ's global set. The payload carries more sets than are actually on by
/// default (`Emote Effects` and friends); `default_sets` names the ones a
/// viewer with stock settings really sees, so only those are cached — anything
/// else would render emotes in our replay that nobody saw on Twitch.
async fn ffz_global_emotes(client: &Client) -> Result<Vec<GlobalEmote>> {
    let resp = client.get("https://api.frankerfacez.com/v1/set/global").send().await?;
    if !resp.status().is_success() {
        bail!("FFZ globals: {}", resp.status());
    }
    Ok(parse_ffz_global(&resp.json().await?))
}

fn parse_ffz_global(v: &serde_json::Value) -> Vec<GlobalEmote> {
    let Some(defaults) = v["default_sets"].as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for set_id in defaults {
        let key = match set_id.as_i64() {
            Some(i) => i.to_string(),
            None => continue,
        };
        let Some(emotes) = v["sets"][&key]["emoticons"].as_array() else { continue };
        for e in emotes {
            let (Some(id), Some(name)) = (e["id"].as_i64(), e["name"].as_str()) else { continue };
            // Best available scale: 4 > 2 > 1.
            let Some(raw) = e["urls"]["4"]
                .as_str()
                .or_else(|| e["urls"]["2"].as_str())
                .or_else(|| e["urls"]["1"].as_str())
            else {
                continue;
            };
            // Protocol-relative (`//cdn…`) — keep both slashes, they are part
            // of the authority.
            let url =
                if raw.starts_with("//") { format!("https:{raw}") } else { raw.to_string() };
            out.push(GlobalEmote {
                entry: EmoteManifestEntry {
                    name: name.to_string(),
                    id: id.to_string(),
                    ext: ext_from_url(&url).unwrap_or("png").to_string(),
                    shared: true,
                },
                url,
            });
        }
    }
    out
}

/// Download a provider's global set into its shared image cache and write the
/// manifest. Returns how many emotes the manifest ended up listing.
///
/// Images land in the *same* `platform_dir/{provider}/emotes/` directory the
/// channel fetchers use, keyed by id — so a channel that also carries a global
/// emote costs no second copy on disk.
async fn store_global_emotes(
    client: &Client,
    platform_dir: &Path,
    provider: &str,
    emotes: Vec<GlobalEmote>,
) -> Result<usize> {
    if emotes.is_empty() {
        return Ok(0);
    }
    let dir = platform_dir.join(provider).join("emotes");
    crate::iomon::fs::create_dir_all(Cat::AssetCache, &dir).await?;
    let mut manifest: Vec<EmoteManifestEntry> = Vec::new();
    for g in emotes {
        if g.entry.name.trim().is_empty() {
            continue;
        }
        let dest = dir.join(format!(
            "{}_{}.{}",
            g.entry.id,
            sanitize_emote_name(&g.entry.name),
            g.entry.ext
        ));
        let legacy = dir.join(format!("{}.{}", g.entry.id, g.entry.ext));
        if !asset_present(&dest) && !asset_present(&legacy) {
            if let Err(e) = download_image(client, &g.url, &dest).await {
                warn!("{provider} global emote {}: {e}", g.entry.id);
            } else {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        // Recorded even if the download above failed: the map builder checks
        // the file exists before using it, so a missing image is harmless,
        // and keeping the entry means the next 24h pass retries that one
        // emote instead of dropping it from the set entirely.
        manifest.push(g.entry);
    }
    if manifest.is_empty() {
        return Ok(0);
    }
    let json = serde_json::to_string(&manifest)?;
    crate::iomon::fs::write(Cat::AssetCache, global_emote_manifest(platform_dir, provider), json)
        .await?;
    Ok(manifest.len())
}

/// Refresh the BTTV / FFZ / 7TV global emote sets (once per provider per 24h).
///
/// These are channel-independent — every Twitch chat renders them for every
/// viewer — but nothing fetched them, so a global like 7TV's `xdx` showed up
/// in the replay as the literal word while Twitch showed the picture.
///
/// Runs once per channel-asset pass, but the stamp means at most three HTTP
/// requests a day across the whole app no matter how many channels are
/// monitored. Written only when a set actually arrived, so a provider outage
/// retries on the next pass rather than blanking those emotes for a day.
async fn fetch_global_emotes(client: &Client, platform_dir: &Path) {
    for provider in ["7tv", "bttv", "ffz"] {
        let stamp = global_emotes_stamp(platform_dir, provider);
        if !global_asset_stale(&stamp) {
            continue;
        }
        let fetched = match provider {
            "7tv" => seventv_global_emotes(client).await,
            "bttv" => bttv_global_emotes(client).await,
            _ => ffz_global_emotes(client).await,
        };
        match fetched {
            Ok(emotes) => match store_global_emotes(client, platform_dir, provider, emotes).await {
                Ok(0) => warn!("{provider} global emotes: provider returned none"),
                Ok(n) => {
                    write_global_asset_stamp(&stamp);
                    tracing::debug!("{provider} global emotes: {n} cached");
                }
                Err(e) => warn!("{provider} global emotes: {e:#}"),
            },
            Err(e) => warn!("{provider} global emotes: {e:#}"),
        }
    }
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
) -> Result<(bool, String, Option<i64>)> {
    if api_key.is_empty() || channel_id.is_empty() {
        bail!("missing YouTube API key or channel ID");
    }
    let url = format!(
        "https://www.googleapis.com/youtube/v3/channels\
         ?part=snippet,brandingSettings,statistics&id={channel_id}&key={api_key}"
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

    crate::iomon::fs::create_dir_all(Cat::AssetCache, asset_dir).await?;

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
    // Absent (not a string, or missing) when the channel hides its
    // subscriber count — best-effort, same idiom as every other
    // opportunistically-cached platform fact in this module.
    let subscriber_count =
        item["statistics"]["subscriberCount"].as_str().and_then(|s| s.parse().ok());
    Ok((banner_set, description, subscriber_count))
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

    crate::iomon::fs::create_dir_all(Cat::AssetCache, asset_dir).await?;

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

/// The channel's channel-point reward titles, cached as `rewards.json` next to
/// the emote/badge assets.
///
/// IRC hands a redeemed message only the reward's UUID, so without this the
/// chat replay can only say "a channel-point reward". The reward list is
/// public (it's what the channel page renders), read from the same anonymous
/// GQL surface the hype-train and goal checks use — no auth, and nothing
/// per-viewer is involved.
///
/// Best-effort: a failure leaves the previous file in place, and the replay
/// degrades to the UUID on hover.
pub async fn fetch_twitch_rewards(client: &Client, login: &str, asset_dir: &Path) -> Result<usize> {
    let body = serde_json::json!({
        "query": format!(
            "query {{ user(login: \"{login}\") {{ channel {{ \
             communityPointsSettings {{ customRewards {{ id title cost }} }} }} }} }}"
        )
    });
    let v: serde_json::Value = client
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", crate::detectors::TWITCH_WEB_CLIENT_ID)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let list = &v["data"]["user"]["channel"]["communityPointsSettings"]["customRewards"];
    let Some(arr) = list.as_array() else {
        anyhow::bail!("no customRewards in response");
    };
    let map: std::collections::HashMap<String, RewardEntry> = arr
        .iter()
        .filter_map(|r| {
            let id = r["id"].as_str()?;
            let title = r["title"].as_str()?;
            (!id.is_empty() && !title.is_empty()).then(|| {
                (id.to_string(), RewardEntry {
                    title: title.to_string(),
                    cost: r["cost"].as_i64().unwrap_or(0),
                })
            })
        })
        .collect();
    crate::iomon::fs::create_dir_all_sync(crate::iomon::Cat::AssetCache, asset_dir)?;
    crate::iomon::fs::write_sync(
        crate::iomon::Cat::AssetCache,
        asset_dir.join("rewards.json"),
        serde_json::to_vec_pretty(&map)?,
    )?;
    Ok(map.len())
}

/// One cached channel-point reward.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RewardEntry {
    pub title: String,
    pub cost: i64,
}

/// Read the cached reward titles for a channel, keyed by reward id. Empty when
/// never fetched or unreadable — the replay then shows the raw id on hover.
pub fn load_reward_titles(
    name: &str,
    account: &str,
) -> std::collections::HashMap<String, RewardEntry> {
    for dir in asset_read_dirs(name, crate::models::Platform::Twitch, account) {
        if let Ok(s) = crate::iomon::fs::read_to_string_sync(
            crate::iomon::Cat::AssetCache,
            dir.join("rewards.json"),
        ) && let Ok(m) = serde_json::from_str(&s)
        {
            return m;
        }
    }
    std::collections::HashMap::new()
}

/// Run all Twitch channel asset fetches:
/// - Icon + banner → `asset_dir/`
/// - Channel badges → `asset_dir/badges/`
/// - Global badges  → `platform_dir/twitch/global_badges/` (once per 24h, shared)
/// - Twitch channel emotes → `asset_dir/emotes/twitch/`
/// - BTTV channel emotes → `asset_dir/emotes/bttv/` + manifest `asset_dir/emotes/bttv.json`
/// - BTTV shared emotes → `platform_dir/bttv/emotes/` (global dedup)
/// - FFZ emotes → `platform_dir/ffz/emotes/` + manifest `asset_dir/emotes/ffz.json`
/// - 7TV emotes → `platform_dir/7tv/emotes/` + manifest `asset_dir/emotes/7tv.json`
/// - BTTV/FFZ/7TV **global** emotes → the same shared per-provider caches +
///   manifest `platform_dir/{provider}/global.json` (once per provider per 24h)
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
    // The reward fetch below is keyed by login, which this first call is what
    // tells us.
    let mut login = String::new();
    let ok = match fetch_twitch_channel_assets(client, client_id, token, broadcaster_id, asset_dir)
        .await
    {
        Ok(info) => {
            login = info.login.clone();
            if let Some(sink) = about {
                crate::detectors::record_twitch_user_info(
                    &sink.store,
                    &info.login,
                    &info.broadcaster_type,
                    &info.created_at,
                );
            }
            description = Some(info.description);
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
    // Channel-independent, so it doesn't take a broadcaster_id and isn't gated
    // on the channel fetches above succeeding — see `fetch_global_emotes`.
    fetch_global_emotes(client, platform_dir).await;
    if !login.is_empty() {
        tokio::time::sleep(Duration::from_millis(300)).await;
        match fetch_twitch_rewards(client, &login, asset_dir).await {
            Ok(n) => tracing::debug!("Twitch rewards ({login}): {n} cached"),
            // Not an error worth a warn!: a channel with points disabled has
            // none, and the replay degrades to showing the reward's id.
            Err(e) => tracing::debug!("Twitch rewards ({login}): {e}"),
        }
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
    let old_color = crate::iomon::fs::read_to_string_sync(Cat::AssetCache, &dest)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if color.is_empty() {
        // Broadcaster cleared their colour — drop any stale cache so the UI reverts
        // to the automatic palette instead of tinting with a colour no longer used.
        let _ = crate::iomon::fs::remove_file(Cat::AssetCache, &dest).await;
    } else {
        crate::iomon::fs::create_dir_all(Cat::AssetCache, asset_dir).await?;
        let _ = crate::iomon::fs::write(Cat::AssetCache, &dest, &color).await;
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
            .or_else(|| t.strip_suffix("/posts"))
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
    crate::iomon::fs::create_dir_all(Cat::AssetCache, asset_dir).await?;
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
            Ok((banner_set, description, subscriber_count)) => {
                any_ok = true;
                api_set_banner = banner_set;
                api_description = Some(description);
                if let Some(sink) = about {
                    // Keyed by the About-sink's account slug (same identity
                    // used for the asset dir / About cache / `AssetAccount`),
                    // NOT the resolved UC `channel_id` — a `/@handle` URL's
                    // `account_slug` is the handle, which wouldn't match the
                    // UC id a later lookup by account would use.
                    crate::detectors::record_youtube_channel_info(
                        &sink.store,
                        &sink.account,
                        subscriber_count,
                    );
                }
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
                // Best-effort: `verified` is an object when the channel has a
                // Kick verification badge, `null` otherwise; `followers_count`
                // is a top-level channel field, both already in this response.
                let follower_count = v["followers_count"].as_i64();
                let verified = v["verified"].is_object();
                crate::detectors::record_kick_channel_info(
                    &sink.store,
                    slug,
                    follower_count,
                    verified,
                );
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
    let bytes = crate::iomon::fs::read(Cat::AssetCache, &tmp).await.ok()?;
    let hash = crate::detectors::fnv64(&bytes).to_string();
    let dest = about_dir.join(format!("{hash}.{ext}"));
    if crate::iomon::fs::try_exists(Cat::AssetCache, &dest).await.unwrap_or(false) {
        let _ = crate::iomon::fs::remove_file(Cat::AssetCache, &tmp).await;
    } else if crate::iomon::fs::rename(Cat::AssetCache, &tmp, &dest).await.is_err() {
        let _ = crate::iomon::fs::write(Cat::AssetCache, &dest, &bytes).await;
        let _ = crate::iomon::fs::remove_file(Cat::AssetCache, &tmp).await;
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
        crate::iomon::fs::create_dir_all(Cat::AssetCache, &about_dir).await?;
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
            .or_else(|| t.strip_suffix("/posts"))
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
                    && let Some(hit) = youtube_about_from_page_data(&data)
                {
                    raw = find_key_object(&data, "aboutChannelViewModel")
                        .or_else(|| find_key_object(&data, "channelAboutFullMetadataRenderer"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    scraped = Some(hit);
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
    #![allow(clippy::disallowed_methods)]

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

    #[test]
    fn resolve_emote_path_prefers_new_scheme_falls_back_to_old() {
        let dir = unique_test_dir("sa-emote-resolve");
        std::fs::create_dir_all(&dir).unwrap();

        // Neither file exists yet: falls back to the (nonexistent) old form —
        // callers gate on `exists_sync` themselves, this just picks a candidate.
        let entry = EmoteManifestEntry { name: "Kappa".into(), id: "425618".into(), ext: "png".into(), shared: false };
        assert_eq!(resolve_emote_path(&dir, &entry), dir.join("425618.png"));

        // Only the OLD file present: still resolves to it.
        std::fs::write(dir.join("425618.png"), b"x").unwrap();
        assert_eq!(resolve_emote_path(&dir, &entry), dir.join("425618.png"));

        // Once the NEW-scheme file is also present, it wins even though the
        // old one still exists too — this is the exact regression that broke
        // chat rendering for every emote fetched after the filename rename
        // (the chat renderer's own copy of this logic hadn't been updated).
        std::fs::write(dir.join("425618_Kappa.png"), b"y").unwrap();
        assert_eq!(resolve_emote_path(&dir, &entry), dir.join("425618_Kappa.png"));

        // A name needing sanitization resolves consistently with the fetcher's
        // own `sanitize_emote_name` call.
        let weird = EmoteManifestEntry { name: "some emote!".into(), id: "999".into(), ext: "webp".into(), shared: false };
        std::fs::write(dir.join("999_some_emote_.webp"), b"z").unwrap();
        assert_eq!(resolve_emote_path(&dir, &weird), dir.join("999_some_emote_.webp"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_twitch_emote_dirs_under_walks_account_and_legacy_layouts() {
        let root = unique_test_dir("sa-twitch-emote-dirs");
        // Account-layout channel: channel_assets/Nihmune/twitch/nihmune/emotes/twitch/
        let nihmune = root.join("Nihmune").join("twitch").join("nihmune").join("emotes").join("twitch");
        std::fs::create_dir_all(&nihmune).unwrap();
        std::fs::write(nihmune.join("111_nihmunHeart.png"), b"x").unwrap();
        // Legacy pre-account layout: channel_assets/OldChan/twitch/emotes/twitch/
        let old_chan = root.join("OldChan").join("twitch").join("emotes").join("twitch");
        std::fs::create_dir_all(&old_chan).unwrap();
        // A channel with a twitch dir but no emotes fetched yet — must be
        // skipped, not produce a dir that doesn't exist.
        std::fs::create_dir_all(root.join("NoEmotesYet").join("twitch")).unwrap();
        // A YouTube-only channel — must never contribute (first-party emotes
        // are a Twitch-only concept).
        std::fs::create_dir_all(root.join("YtOnly").join("youtube")).unwrap();

        let mut dirs = all_twitch_emote_dirs_under(&root);
        dirs.sort();
        let mut want = vec![nihmune, old_chan];
        want.sort();
        assert_eq!(dirs, want);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression guard for the perf fix: `index_emote_stems` must key by the
    /// exact filename stem (new `{id}_{name}` and legacy `{id}` both land as
    /// literal keys, no id-parsing), across every directory passed in, in ONE
    /// pass — this is what turned "one `exists_sync` stat per (occurrence ×
    /// fallback channel × extension × filename-form)" into one directory
    /// listing per channel + O(1) lookups, after the naive version made a
    /// 3000-message chat log take over a minute to load (2026-08-02).
    #[test]
    fn index_emote_stems_keys_by_stem_across_every_dir() {
        let root = unique_test_dir("sa-emote-stem-index");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("111_nihmunHeart.png"), b"x").unwrap();
        std::fs::write(b.join("222.gif"), b"y").unwrap(); // legacy form, other dir
        // A non-image file (e.g. a stray manifest) still indexes by stem —
        // the index doesn't filter by extension, callers probe specific keys.
        std::fs::write(b.join("333_someEmote.webp"), b"z").unwrap();

        let index = index_emote_stems(&[a.clone(), b.clone()]);
        assert_eq!(index.get("111_nihmunHeart"), Some(&a.join("111_nihmunHeart.png")));
        assert_eq!(index.get("222"), Some(&b.join("222.gif")));
        assert_eq!(index.get("333_someEmote"), Some(&b.join("333_someEmote.webp")));
        assert_eq!(index.get("nope"), None);
        // A dir that doesn't exist is silently skipped, not an error.
        let empty = index_emote_stems(&[root.join("missing")]);
        assert!(empty.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn seventv_globals_parse_into_cdn_urls() {
        // Shape from the live `7tv.io/v3/emote-sets/global` payload.
        let v = serde_json::json!({
            "emotes": [
                { "id": "01F6MZGCNG000255K4X1KTKGGZ", "name": "xdx" },
                { "id": "01F6ME1BQ00007WV4YHDCJ6NDS" },          // no name
                { "name": "orphan" },                             // no id
            ]
        });
        let got = parse_7tv_global(&v);
        assert_eq!(got.len(), 1, "entries missing an id or a name are unusable");
        assert_eq!(got[0].entry.name, "xdx");
        assert_eq!(got[0].entry.ext, "webp");
        assert_eq!(got[0].url, "https://cdn.7tv.app/emote/01F6MZGCNG000255K4X1KTKGGZ/4x.webp");
    }

    #[test]
    fn ffz_globals_take_only_the_default_sets() {
        // FFZ ships more sets than a stock viewer sees. Caching the extras
        // would render emotes in the replay that nobody watching Twitch saw.
        let v = serde_json::json!({
            "default_sets": [3],
            "sets": {
                "3": { "emoticons": [
                    { "id": 9, "name": "ZreknarF", "urls": { "1": "//cdn.frankerfacez.com/emote/9/1", "4": "//cdn.frankerfacez.com/emote/9/4.webp" } },
                ]},
                "1539687": { "emoticons": [
                    { "id": 723890, "name": "NotDefault", "urls": { "1": "//cdn.frankerfacez.com/emote/723890/1" } },
                ]},
            }
        });
        let got = parse_ffz_global(&v);
        assert_eq!(got.iter().map(|g| g.entry.name.as_str()).collect::<Vec<_>>(), ["ZreknarF"]);
        // Protocol-relative URLs must be made absolute, and the best scale wins.
        assert_eq!(got[0].url, "https://cdn.frankerfacez.com/emote/9/4.webp");
        assert_eq!(got[0].entry.ext, "webp");
    }

    #[test]
    fn ffz_globals_fall_back_to_a_smaller_scale() {
        let v = serde_json::json!({
            "default_sets": [3],
            "sets": { "3": { "emoticons": [
                { "id": 9, "name": "OnlySmall", "urls": { "1": "https://cdn.frankerfacez.com/emote/9/1" } },
            ]}}
        });
        let got = parse_ffz_global(&v);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "https://cdn.frankerfacez.com/emote/9/1");
        // No extension in the URL at all — PNG is FFZ's default.
        assert_eq!(got[0].entry.ext, "png");
    }

    #[test]
    fn global_asset_stamps_expire_and_fail_open() {
        let dir = std::env::temp_dir().join(format!("sa_stamp_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let stamp = dir.join(".global_emotes_fetched_at");

        assert!(global_asset_stale(&stamp), "a missing stamp must mean refetch");

        write_global_asset_stamp(&stamp);
        assert!(!global_asset_stale(&stamp), "just written — still fresh");

        std::fs::write(&stamp, (now_unix() - GLOBAL_ASSET_TTL_SECS - 1).to_string()).unwrap();
        assert!(global_asset_stale(&stamp), "past the TTL");

        // Fail OPEN: a corrupt stamp costs one extra fetch a day, whereas
        // trusting it would wedge the set forever.
        std::fs::write(&stamp, b"not a timestamp").unwrap();
        assert!(global_asset_stale(&stamp));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_emote_manifests_sit_beside_the_shared_image_cache() {
        // The map builder reads this exact path; if the two ever disagreed,
        // globals would silently stop resolving.
        let plat = Path::new("C:").join("assets");
        assert_eq!(
            global_emote_manifest(&plat, "7tv"),
            plat.join("7tv").join("global.json")
        );
    }

    #[test]
    fn global_twitch_emote_dir_lives_under_platform_assets_twitch() {
        let dir = global_twitch_emote_dir();
        assert!(dir.ends_with(std::path::Path::new("twitch").join("global_emotes")), "{dir:?}");
        assert!(dir.starts_with(crate::app_paths::platform_assets_dir()), "{dir:?}");
    }

    #[test]
    fn twitch_emote_cdn_fetch_is_deterministic_and_sanitizes_the_name() {
        let (dest1, urls1) = twitch_emote_cdn_fetch("425618", "thonkListen");
        let (dest2, urls2) = twitch_emote_cdn_fetch("425618", "thonkListen");
        // Same input twice -> identical dest/urls, so repeat chat occurrences
        // of the same emote collapse to one fetch via `fetches.dedup()`.
        assert_eq!(dest1, dest2);
        assert_eq!(urls1, urls2);
        assert_eq!(dest1, global_twitch_emote_dir().join("425618_thonkListen.png"));
        // Animated tried first, static as the fallback candidate.
        assert_eq!(
            urls1,
            vec![
                "https://static-cdn.jtvnw.net/emoticons/v2/425618/animated/dark/3.0",
                "https://static-cdn.jtvnw.net/emoticons/v2/425618/static/dark/3.0",
            ]
        );

        // A name needing sanitization matches `sanitize_emote_name` exactly,
        // consistent with every other reader/writer of this filename scheme.
        let (dest3, _) = twitch_emote_cdn_fetch("999", "some emote!");
        assert_eq!(dest3, global_twitch_emote_dir().join("999_some_emote_.png"));
    }
}
