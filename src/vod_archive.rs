//! Post-stream VOD download — archive the platform's published VOD after a live
//! recording ends, either *alongside* the live capture or *replacing* it iff the
//! download succeeded.
//!
//! Two independent toggles ("download after end" and "replace on success") each
//! resolve through a **three-level inheritance chain: global default < per-channel <
//! per-instance**. This mirrors the schedule-source scope config
//! ([`crate::schedule_source::SourceScopeConfig`]) exactly: per-channel and
//! per-monitor overrides are `Option<bool>` stored as JSON scope-maps in
//! `app_settings` (no schema migration for the toggles), and precedence is
//! monitor override → channel override → global default.
//!
//! The published-VOD URL is resolved per platform (Twitch via the existing
//! `check_twitch_vod` poller, YouTube from the video id, Kick via its channel
//! videos API) and handed to the normal detached Video download queue.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::store::Store;

// ---------- settings keys ----------

/// Global default: download the published VOD after a stream ends.
pub const K_VOD_DL_ENABLED: &str = "vod_dl_enabled";
/// Global default: replace the live recording when the VOD download succeeds.
pub const K_VOD_DL_REPLACE: &str = "vod_dl_replace";
/// Per-channel scope-config map (`{channel_id -> VodArchiveScope}`).
pub const K_CHANNEL_VOD_SCOPE: &str = "channel_vod_scope";
/// Per-monitor scope-config map (`{monitor_id -> VodArchiveScope}`).
pub const K_MONITOR_VOD_SCOPE: &str = "monitor_vod_scope";

/// Twitch/YouTube VOD match window: the published VOD's start vs. our go-live time.
pub const VOD_MATCH_WINDOW_SECS: i64 = 2 * 3600;

// ---------- three-level scope config (clone of SourceScopeConfig) ----------

/// A channel- or monitor-level override of the VOD-archive toggles. `None` on a
/// field means "inherit the level above"; `Some(true/false)` forces it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VodArchiveScope {
    #[serde(default)]
    pub download: Option<bool>,
    #[serde(default)]
    pub replace: Option<bool>,
}

impl VodArchiveScope {
    /// True when this scope overrides nothing — persisted as a removal so the map
    /// only holds real overrides.
    pub fn is_inherit(&self) -> bool {
        self.download.is_none() && self.replace.is_none()
    }
}

/// Load a scope-config map (`{id -> VodArchiveScope}`) from the given setting key.
fn load_scope_map(store: &Store, key: &str) -> HashMap<String, VodArchiveScope> {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save one id's scope into the map at `key`, removing inert entries.
fn save_scope(store: &Store, key: &str, id: i64, cfg: &VodArchiveScope) -> anyhow::Result<()> {
    let mut map = load_scope_map(store, key);
    if cfg.is_inherit() {
        map.remove(&id.to_string());
    } else {
        map.insert(id.to_string(), cfg.clone());
    }
    store.set_setting(key, &serde_json::to_string(&map)?)?;
    Ok(())
}

/// One channel's scope config (default = inherit when unset).
pub fn load_channel_vod_scope(store: &Store, channel_id: i64) -> VodArchiveScope {
    load_scope_map(store, K_CHANNEL_VOD_SCOPE)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

/// Save one channel's scope config.
pub fn save_channel_vod_scope(store: &Store, channel_id: i64, cfg: &VodArchiveScope) -> anyhow::Result<()> {
    save_scope(store, K_CHANNEL_VOD_SCOPE, channel_id, cfg)
}

/// One monitor's scope config (default = inherit when unset).
pub fn load_monitor_vod_scope(store: &Store, monitor_id: i64) -> VodArchiveScope {
    load_scope_map(store, K_MONITOR_VOD_SCOPE)
        .remove(&monitor_id.to_string())
        .unwrap_or_default()
}

/// Save one monitor's scope config.
pub fn save_monitor_vod_scope(store: &Store, monitor_id: i64, cfg: &VodArchiveScope) -> anyhow::Result<()> {
    save_scope(store, K_MONITOR_VOD_SCOPE, monitor_id, cfg)
}

/// Read a global boolean setting (`"1"` = on), defaulting to `false`.
fn global_bool(store: &Store, key: &str) -> bool {
    store.get_setting(key).ok().flatten().as_deref() == Some("1")
}

pub fn global_vod_download(store: &Store) -> bool {
    global_bool(store, K_VOD_DL_ENABLED)
}

pub fn global_vod_replace(store: &Store) -> bool {
    global_bool(store, K_VOD_DL_REPLACE)
}

/// Resolve the effective "download VOD after end" toggle: monitor override over
/// channel override over the global default.
pub fn effective_vod_download_from(
    global: bool,
    channel_scope: Option<&VodArchiveScope>,
    monitor_scope: Option<&VodArchiveScope>,
) -> bool {
    if let Some(v) = monitor_scope.and_then(|s| s.download) {
        return v;
    }
    if let Some(v) = channel_scope.and_then(|s| s.download) {
        return v;
    }
    global
}

/// Resolve the effective "replace live recording on success" toggle.
pub fn effective_vod_replace_from(
    global: bool,
    channel_scope: Option<&VodArchiveScope>,
    monitor_scope: Option<&VodArchiveScope>,
) -> bool {
    if let Some(v) = monitor_scope.and_then(|s| s.replace) {
        return v;
    }
    if let Some(v) = channel_scope.and_then(|s| s.replace) {
        return v;
    }
    global
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_vod_download(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    let ch = load_channel_vod_scope(store, channel_id);
    let mon = load_monitor_vod_scope(store, monitor_id);
    effective_vod_download_from(global_vod_download(store), Some(&ch), Some(&mon))
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_vod_replace(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    let ch = load_channel_vod_scope(store, channel_id);
    let mon = load_monitor_vod_scope(store, monitor_id);
    effective_vod_replace_from(global_vod_replace(store), Some(&ch), Some(&mon))
}

// ---------- per-platform published-VOD URL resolution ----------

/// The Twitch VOD watch URL for an archive id.
pub fn twitch_vod_url(vod_id: &str) -> String {
    format!("https://www.twitch.tv/videos/{vod_id}")
}

/// The YouTube VOD is the same video: `watch?v={video_id}` (== `recording.stream_id`).
pub fn youtube_vod_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

/// The Kick VOD watch URL for a video uuid (yt-dlp accepts this form).
pub fn kick_vod_url(uuid: &str) -> String {
    format!("https://kick.com/video/{uuid}")
}

/// Extract the Kick channel slug from a `kick.com/<slug>` URL.
pub fn kick_slug(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let lower = trimmed.to_lowercase();
    let pos = lower.find("kick.com/")?;
    let rest = &trimmed[pos + "kick.com/".len()..];
    let slug = rest.split(['/', '?', '#']).next()?.trim();
    (!slug.is_empty()).then(|| slug.to_lowercase())
}

/// Parse a Kick `/videos` JSON body and pick the newest VOD whose start is within
/// the match window of `went_live_at` (or just the newest when the time is unknown).
/// Returns the VOD uuid. Defensive against schema drift — reads via `serde_json::Value`.
fn parse_kick_vod_uuid(body: &str, went_live_at: Option<i64>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    // The endpoint may return a bare array or `{ "data": [ ... ] }`.
    let arr = v.as_array().or_else(|| v.get("data").and_then(|d| d.as_array()))?;
    let mut best: Option<(i64, String)> = None;
    for item in arr {
        let uuid = item
            .get("uuid")
            .or_else(|| item.get("video").and_then(|x| x.get("uuid")))
            .and_then(|u| u.as_str())?
            .to_string();
        // Best-effort start time: `created_at` / `start_time` (RFC3339 or epoch secs).
        let start = item
            .get("created_at")
            .or_else(|| item.get("start_time"))
            .or_else(|| item.get("session_started_at"))
            .and_then(kick_time_to_epoch)
            .unwrap_or(0);
        if went_live_at.is_some_and(|t| (start - t).abs() > VOD_MATCH_WINDOW_SECS) {
            continue;
        }
        if best.as_ref().is_none_or(|(s, _)| start > *s) {
            best = Some((start, uuid));
        }
    }
    best.map(|(_, uuid)| uuid)
}

/// Coerce a Kick JSON time value (RFC3339 string or epoch number) to unix seconds.
fn kick_time_to_epoch(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Resolve the Kick VOD URL for a just-ended stream via the channel videos API.
/// Best-effort — returns `None` if the API shape changes or no VOD matches yet.
pub async fn resolve_kick_vod(
    client: &reqwest::Client,
    slug: &str,
    went_live_at: Option<i64>,
) -> Option<String> {
    let url = format!("https://kick.com/api/v2/channels/{slug}/videos");
    let body = client.get(&url).send().await.ok()?.text().await.ok()?;
    let uuid = parse_kick_vod_uuid(&body, went_live_at)?;
    Some(kick_vod_url(&uuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(download: Option<bool>, replace: Option<bool>) -> VodArchiveScope {
        VodArchiveScope { download, replace }
    }

    #[test]
    fn precedence_monitor_over_channel_over_global() {
        // monitor override wins
        assert!(effective_vod_download_from(false, Some(&scope(Some(false), None)), Some(&scope(Some(true), None))));
        assert!(!effective_vod_download_from(true, Some(&scope(Some(true), None)), Some(&scope(Some(false), None))));
        // channel override wins when monitor inherits
        assert!(effective_vod_download_from(false, Some(&scope(Some(true), None)), Some(&scope(None, None))));
        // global when both inherit
        assert!(effective_vod_download_from(true, Some(&scope(None, None)), Some(&scope(None, None))));
        assert!(!effective_vod_download_from(false, None, None));
        // replace resolves independently
        assert!(effective_vod_replace_from(false, None, Some(&scope(None, Some(true)))));
        assert!(!effective_vod_replace_from(true, Some(&scope(None, Some(false))), Some(&scope(None, None))));
    }

    #[test]
    fn scope_json_roundtrip_and_inherit() {
        assert!(scope(None, None).is_inherit());
        assert!(!scope(Some(false), None).is_inherit());
        let s = scope(Some(true), Some(false));
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<VodArchiveScope>(&json).unwrap(), s);
        // A legacy/empty object deserializes to inherit.
        assert!(serde_json::from_str::<VodArchiveScope>("{}").unwrap().is_inherit());
    }

    #[test]
    fn vod_url_builders() {
        assert_eq!(twitch_vod_url("123"), "https://www.twitch.tv/videos/123");
        assert_eq!(youtube_vod_url("abcDEF"), "https://www.youtube.com/watch?v=abcDEF");
        assert_eq!(kick_vod_url("uu-id"), "https://kick.com/video/uu-id");
        assert_eq!(kick_slug("https://kick.com/SomeOne?x=1").as_deref(), Some("someone"));
        assert_eq!(kick_slug("https://twitch.tv/x"), None);
    }

    #[test]
    fn parse_kick_vod_picks_newest_in_window() {
        // target go-live = 1000; two VODs, one out of window, one just after.
        let body = r#"{"data":[
            {"uuid":"old","created_at":1},
            {"uuid":"match","created_at":1005},
            {"uuid":"newer-but-far","created_at":999999999}
        ]}"#;
        assert_eq!(parse_kick_vod_uuid(body, Some(1000)), Some("match".to_string()));
        // bare array + epoch-less falls back to newest
        let arr = r#"[{"uuid":"a","created_at":10},{"uuid":"b","created_at":20}]"#;
        assert_eq!(parse_kick_vod_uuid(arr, None), Some("b".to_string()));
        assert_eq!(parse_kick_vod_uuid("not json", None), None);
    }
}
