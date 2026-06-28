//! Schedule sources — the ordered set of methods used to resolve a streamer's
//! upcoming schedule.
//!
//! Historically the app had one schedule method per platform (Twitch Helix,
//! YouTube `/streams` scrape) plus an opt-in Discord import. Many streamers
//! instead publish their week as an *image* — a Twitch offline banner, a YouTube
//! community post, or a pinned tweet — so we generalize to a user-ordered list of
//! [`ScheduleSourceKind`]s. The schedule refresh walks the enabled sources
//! top-to-bottom per channel and the first one to resolve a non-empty schedule
//! wins (see `refresh_schedules_once` in [`crate::detectors`]).
//!
//! Each kind's [`ScheduleSourceKind::id`] is the stable string written to the
//! `schedule_segment.source` column, so the values here MUST NOT change once
//! shipped. The ordered config and per-channel config are persisted as JSON in
//! `app_settings` (no schema migration).

use serde::{Deserialize, Serialize};

use crate::models::{
    K_CHANNEL_SCOPE_CFG, K_CHANNEL_SOURCE_CFG, K_DISCORD_SCHEDULE, K_MONITOR_SCOPE_CFG,
    K_SCHEDULE_SOURCES, K_SCHEDULE_TITLE_FILL, K_YT_COMMUNITY_MAX_POSTS, Platform,
};
use crate::store::Store;

/// Built-in YouTube community-post backlog depth (how many recent posts to scan
/// for a schedule image) when neither the per-channel nor the global setting
/// overrides it. See [`community_max_posts`].
pub const DEFAULT_COMMUNITY_POSTS: usize = 5;

/// A method for resolving a channel's upcoming schedule. The variant order here
/// is irrelevant; user priority lives in the persisted [`SourceEntry`] list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleSourceKind {
    /// Twitch's published schedule via Helix `/schedule`.
    TwitchSchedule,
    /// YouTube upcoming livestreams scraped from the `/streams` page.
    YouTubeScrape,
    /// YouTube upcoming livestreams via the Data API (`search.list` +
    /// `videos.list`); self-gates on a configured API key.
    YouTubeApi,
    /// OCR the channel's already-downloaded Twitch offline banner.
    TwitchBannerOcr,
    /// OCR the image attached to the channel's latest YouTube community post.
    YouTubeCommunityOcr,
    /// OCR the image on the channel's pinned Twitter/X tweet.
    TwitterPinned,
    /// Discord scheduled events (opt-in; uses the user's Discord token).
    Discord,
    /// OCR a user-supplied image (per-channel path or URL).
    OtherImageOcr,
}

impl ScheduleSourceKind {
    /// Every kind, in the seed/default priority order. Risky and OCR sources
    /// default OFF; the existing platform methods default ON.
    pub const DEFAULT_ORDER: [ScheduleSourceKind; 8] = [
        ScheduleSourceKind::TwitchSchedule,
        ScheduleSourceKind::YouTubeApi,
        ScheduleSourceKind::YouTubeScrape,
        ScheduleSourceKind::TwitchBannerOcr,
        ScheduleSourceKind::YouTubeCommunityOcr,
        ScheduleSourceKind::TwitterPinned,
        ScheduleSourceKind::OtherImageOcr,
        ScheduleSourceKind::Discord,
    ];

    /// Stable id — also the `schedule_segment.source` value. NEVER change these.
    pub fn id(self) -> &'static str {
        match self {
            // Kept as "platform" so existing Twitch rows stay valid.
            ScheduleSourceKind::TwitchSchedule => "platform",
            ScheduleSourceKind::YouTubeScrape => "youtube",
            ScheduleSourceKind::YouTubeApi => "youtube_api",
            ScheduleSourceKind::TwitchBannerOcr => "twitch_banner_ocr",
            ScheduleSourceKind::YouTubeCommunityOcr => "youtube_community_ocr",
            ScheduleSourceKind::TwitterPinned => "twitter_pinned",
            ScheduleSourceKind::Discord => "discord",
            ScheduleSourceKind::OtherImageOcr => "other_image_ocr",
        }
    }

    /// Resolve a kind from its stable id (unknown ids → `None`).
    pub fn from_id(id: &str) -> Option<ScheduleSourceKind> {
        ScheduleSourceKind::DEFAULT_ORDER
            .into_iter()
            .find(|k| k.id() == id)
    }

    /// Short human-readable label for the Schedule sources dialog.
    pub fn label(self) -> &'static str {
        match self {
            ScheduleSourceKind::TwitchSchedule => "Twitch schedule",
            ScheduleSourceKind::YouTubeScrape => "YouTube schedule (scrape)",
            ScheduleSourceKind::YouTubeApi => "YouTube schedule (Data API)",
            ScheduleSourceKind::TwitchBannerOcr => "Twitch banner (OCR)",
            ScheduleSourceKind::YouTubeCommunityOcr => "YouTube community post (OCR)",
            ScheduleSourceKind::TwitterPinned => "Twitter/X pinned (scrape)",
            ScheduleSourceKind::Discord => "Discord events",
            ScheduleSourceKind::OtherImageOcr => "Other image (OCR)",
        }
    }

    /// A one-line description for the dialog tooltip.
    pub fn description(self) -> &'static str {
        match self {
            ScheduleSourceKind::TwitchSchedule => "The broadcaster's published Twitch schedule.",
            ScheduleSourceKind::YouTubeScrape => {
                "Upcoming livestreams scraped from the channel's Streams tab (free)."
            }
            ScheduleSourceKind::YouTubeApi => {
                "Upcoming livestreams via the YouTube Data API (needs an API key; spends quota)."
            }
            ScheduleSourceKind::TwitchBannerOcr => {
                "Read the schedule off the channel's Twitch offline banner via the OCR CLI."
            }
            ScheduleSourceKind::YouTubeCommunityOcr => {
                "Read the schedule off the latest YouTube community post image via OCR."
            }
            ScheduleSourceKind::TwitterPinned => {
                "Read the schedule off the channel's pinned tweet image (set the handle in channel Properties)."
            }
            ScheduleSourceKind::Discord => {
                "Import Discord scheduled events (uses your Discord token — against Discord's ToS)."
            }
            ScheduleSourceKind::OtherImageOcr => {
                "Read the schedule off a user-supplied image path/URL (set it in channel Properties)."
            }
        }
    }

    /// Font-safe emoji badge shown on calendar items to indicate where the
    /// schedule came from. Glyphs are restricted to ones the app already renders
    /// in production (egui rasterizes only monochrome outlines from its bundled
    /// NotoEmoji subset + the OS fallback; see
    /// [`crate::models::ContentType::icon`]). `📡` broadcast schedule · `📺`
    /// YouTube · `📷` any image/OCR source · `💬` Discord.
    pub fn badge_icon(self) -> &'static str {
        match self {
            ScheduleSourceKind::TwitchSchedule => "📡",
            ScheduleSourceKind::YouTubeScrape | ScheduleSourceKind::YouTubeApi => "📺",
            ScheduleSourceKind::TwitchBannerOcr
            | ScheduleSourceKind::YouTubeCommunityOcr
            | ScheduleSourceKind::TwitterPinned
            | ScheduleSourceKind::OtherImageOcr => "📷",
            ScheduleSourceKind::Discord => "💬",
        }
    }

    /// Whether "Open source" can resolve a meaningful target for this kind (the
    /// origin page or image). Discord events have no per-event URL.
    pub fn has_open_target(self) -> bool {
        !matches!(self, ScheduleSourceKind::Discord)
    }

    /// Whether this source carries an account-risk / ToS warning.
    pub fn risky(self) -> bool {
        matches!(
            self,
            ScheduleSourceKind::Discord | ScheduleSourceKind::TwitterPinned
        )
    }

    /// Discord is resolved by a separate batch sweep, not the per-monitor walk.
    pub fn is_per_monitor(self) -> bool {
        !matches!(self, ScheduleSourceKind::Discord)
    }

    /// Whether this source can produce a schedule for a given monitor — its
    /// platform matches and any required per-channel config is present.
    pub fn applies_to(self, platform: Platform, cfg: &ChannelSourceConfig) -> bool {
        match self {
            ScheduleSourceKind::TwitchSchedule | ScheduleSourceKind::TwitchBannerOcr => {
                platform == Platform::Twitch
            }
            ScheduleSourceKind::YouTubeScrape
            | ScheduleSourceKind::YouTubeApi
            | ScheduleSourceKind::YouTubeCommunityOcr => platform == Platform::YouTube,
            // Cross-platform per-channel sources: applicable only when configured.
            ScheduleSourceKind::TwitterPinned => !cfg.twitter_handle.trim().is_empty(),
            ScheduleSourceKind::OtherImageOcr => !cfg.other_image.trim().is_empty(),
            // Discord matches per the sweep, regardless of platform.
            ScheduleSourceKind::Discord => true,
        }
    }
}

/// Resolve a `schedule_segment.source` id to a `(icon, label)` pair for the
/// calendar's source badge + hover/detail text. Special-cases `"manual"` (a
/// user-edited row, which has no [`ScheduleSourceKind`]); an unrecognized id
/// falls back to a neutral marker rather than panicking.
pub fn source_badge(source: &str) -> (&'static str, &'static str) {
    if source == "manual" {
        // ✏ pencil — already used elsewhere in the UI, so it renders.
        return ("✏", "Manually edited");
    }
    match ScheduleSourceKind::from_id(source) {
        Some(k) => (k.badge_icon(), k.label()),
        // U+2022 bullet from the base Latin font — guaranteed to render.
        None => ("•", "Other source"),
    }
}

/// One entry in the persisted ordered source list: a stable id + enabled flag.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceEntry {
    pub id: String,
    pub enabled: bool,
}

impl SourceEntry {
    /// The resolved kind, or `None` if the stored id is unknown (a stale entry
    /// from a newer build).
    pub fn kind(&self) -> Option<ScheduleSourceKind> {
        ScheduleSourceKind::from_id(&self.id)
    }
}

/// The default ordered list, seeding Discord's enabled flag from the legacy
/// `K_DISCORD_SCHEDULE` toggle so existing users aren't disrupted.
fn default_order(store: &Store) -> Vec<SourceEntry> {
    let discord_on = store
        .get_setting(K_DISCORD_SCHEDULE)
        .ok()
        .flatten()
        .as_deref()
        == Some("1");
    ScheduleSourceKind::DEFAULT_ORDER
        .into_iter()
        .map(|k| SourceEntry {
            id: k.id().to_string(),
            enabled: match k {
                ScheduleSourceKind::Discord => discord_on,
                // The two free platform schedules are on by default; everything
                // else (API, OCR, scraping) is opt-in.
                ScheduleSourceKind::TwitchSchedule
                | ScheduleSourceKind::YouTubeScrape
                | ScheduleSourceKind::YouTubeApi => true,
                _ => false,
            },
        })
        .collect()
}

/// Normalize an ordered source list: drop unknown ids (stale entries from a newer
/// build) and append any kinds missing from the list at the end, disabled — so a
/// newly-added source never silently activates, and a custom per-channel/monitor
/// order stays valid as kinds come and go. Shared by [`load_source_order`] and the
/// scope-override resolvers so every consumer sees a complete, valid list.
fn merge_order(mut entries: Vec<SourceEntry>) -> Vec<SourceEntry> {
    entries.retain(|e| e.kind().is_some());
    for k in ScheduleSourceKind::DEFAULT_ORDER {
        if !entries.iter().any(|e| e.id == k.id()) {
            entries.push(SourceEntry {
                id: k.id().to_string(),
                enabled: false,
            });
        }
    }
    entries
}

/// Load the user's ordered source list. When nothing is persisted, returns the
/// seeded default order. When something is persisted, keeps the user's order +
/// flags for known ids and appends any kinds added in a newer build at the end
/// (disabled, so a new source never silently activates).
pub fn load_source_order(store: &Store) -> Vec<SourceEntry> {
    let raw = store.get_setting(K_SCHEDULE_SOURCES).ok().flatten();
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return default_order(store);
    };
    match serde_json::from_str::<Vec<SourceEntry>>(&raw) {
        Ok(v) => merge_order(v),
        Err(_) => default_order(store),
    }
}

/// Global "go to the next schedule source when an event has no title" toggle.
pub fn global_title_fill(store: &Store) -> bool {
    store
        .get_setting(K_SCHEDULE_TITLE_FILL)
        .ok()
        .flatten()
        .as_deref()
        == Some("1")
}

/// Persist the ordered source list as JSON.
pub fn save_source_order(store: &Store, entries: &[SourceEntry]) -> anyhow::Result<()> {
    let json = serde_json::to_string(entries)?;
    store.set_setting(K_SCHEDULE_SOURCES, &json)?;
    Ok(())
}

/// Per-channel configuration for the schedule sources that need it: the Twitter/X
/// handle, a manual schedule-image path/URL, and OCR overrides (model + assumed
/// timezone/offset). All fields are optional; empty means "use the global default
/// / source not applicable".
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChannelSourceConfig {
    #[serde(default)]
    pub twitter_handle: String,
    #[serde(default)]
    pub other_image: String,
    #[serde(default)]
    pub ocr_model: String,
    #[serde(default)]
    pub ocr_timezone: String,
    #[serde(default)]
    pub ocr_offset: String,
    /// Per-channel override for the YouTube community-post backlog depth (how many
    /// recent posts to scan). Empty = fall back to the global setting / built-in
    /// default. Stored as a string so the UI's text field round-trips an empty /
    /// invalid value as "unset". See [`community_max_posts`].
    #[serde(default)]
    pub max_community_posts: String,
}

/// Load the per-channel source config map (`{channel_id -> cfg}`).
fn load_channel_cfg_map(
    store: &Store,
) -> std::collections::HashMap<String, ChannelSourceConfig> {
    store
        .get_setting(K_CHANNEL_SOURCE_CFG)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Load one channel's source config (default when unset).
pub fn load_channel_cfg(store: &Store, channel_id: i64) -> ChannelSourceConfig {
    load_channel_cfg_map(store)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

/// Save one channel's source config, merging into the shared map.
pub fn save_channel_cfg(
    store: &Store,
    channel_id: i64,
    cfg: &ChannelSourceConfig,
) -> anyhow::Result<()> {
    let mut map = load_channel_cfg_map(store);
    map.insert(channel_id.to_string(), cfg.clone());
    let json = serde_json::to_string(&map)?;
    store.set_setting(K_CHANNEL_SOURCE_CFG, &json)?;
    Ok(())
}

/// Resolve the effective YouTube community-post backlog depth: the per-channel
/// override (`cfg.max_community_posts`) wins, then the global
/// `K_YT_COMMUNITY_MAX_POSTS` setting, then the built-in [`DEFAULT_COMMUNITY_POSTS`].
/// Parsed values are clamped to `1..=20` so a stray huge number can't trigger a
/// long OCR backlog walk; unparseable/empty falls through to the next level.
pub fn community_max_posts(store: &Store, cfg: &ChannelSourceConfig) -> usize {
    fn parse(s: &str) -> Option<usize> {
        s.trim().parse::<usize>().ok().filter(|&n| n > 0).map(|n| n.clamp(1, 20))
    }
    if let Some(n) = parse(&cfg.max_community_posts) {
        return n;
    }
    if let Some(raw) = store.get_setting(K_YT_COMMUNITY_MAX_POSTS).ok().flatten() {
        if let Some(n) = parse(&raw) {
            return n;
        }
    }
    DEFAULT_COMMUNITY_POSTS
}

/// Per-channel / per-monitor schedule-source *scope* override. Either field may
/// be `None` ("inherit the broader level"):
/// - `order`: a full custom ordered source list that *replaces* the global order
///   for this channel/monitor (normalized through [`merge_order`] on read). `None`
///   inherits the global order.
/// - `title_fill`: a tri-state override of the global title-fill toggle. `None`
///   inherits; `Some(true/false)` forces it on/off for this channel/monitor.
///
/// Precedence is monitor over channel over global; see [`effective_order_from`] /
/// [`effective_title_fill_from`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceScopeConfig {
    #[serde(default)]
    pub order: Option<Vec<SourceEntry>>,
    #[serde(default)]
    pub title_fill: Option<bool>,
}

impl SourceScopeConfig {
    /// True when this scope overrides nothing — equivalent to no entry at all.
    /// Saved as a removal so the persisted map only holds real overrides.
    pub fn is_inherit(&self) -> bool {
        self.order.is_none() && self.title_fill.is_none()
    }
}

/// Load a scope-config map (`{id -> SourceScopeConfig}`) from the given setting key.
fn load_scope_map(
    store: &Store,
    key: &str,
) -> std::collections::HashMap<String, SourceScopeConfig> {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save one id's scope config into the map at `key`, removing the entry entirely
/// when it overrides nothing (keeps the persisted map free of inert entries).
fn save_scope(
    store: &Store,
    key: &str,
    id: i64,
    cfg: &SourceScopeConfig,
) -> anyhow::Result<()> {
    let mut map = load_scope_map(store, key);
    if cfg.is_inherit() {
        map.remove(&id.to_string());
    } else {
        map.insert(id.to_string(), cfg.clone());
    }
    store.set_setting(key, &serde_json::to_string(&map)?)?;
    Ok(())
}

/// Per-channel scope-config map (`{channel_id -> cfg}`).
pub fn load_channel_scope_map(store: &Store) -> std::collections::HashMap<String, SourceScopeConfig> {
    load_scope_map(store, K_CHANNEL_SCOPE_CFG)
}

/// Per-monitor scope-config map (`{monitor_id -> cfg}`).
pub fn load_monitor_scope_map(store: &Store) -> std::collections::HashMap<String, SourceScopeConfig> {
    load_scope_map(store, K_MONITOR_SCOPE_CFG)
}

/// Load one channel's scope config (default = inherit when unset).
pub fn load_channel_scope(store: &Store, channel_id: i64) -> SourceScopeConfig {
    load_channel_scope_map(store)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

/// Save one channel's scope config.
pub fn save_channel_scope(store: &Store, channel_id: i64, cfg: &SourceScopeConfig) -> anyhow::Result<()> {
    save_scope(store, K_CHANNEL_SCOPE_CFG, channel_id, cfg)
}

/// Load one monitor's scope config (default = inherit when unset).
pub fn load_monitor_scope(store: &Store, monitor_id: i64) -> SourceScopeConfig {
    load_monitor_scope_map(store)
        .remove(&monitor_id.to_string())
        .unwrap_or_default()
}

/// Save one monitor's scope config.
pub fn save_monitor_scope(store: &Store, monitor_id: i64, cfg: &SourceScopeConfig) -> anyhow::Result<()> {
    save_scope(store, K_MONITOR_SCOPE_CFG, monitor_id, cfg)
}

/// Resolve the effective ordered source list for a monitor given the already-loaded
/// global order and the optional channel/monitor scope overrides. Precedence:
/// monitor `order` over channel `order` over global. A custom order is normalized
/// through [`merge_order`] so it stays complete and valid. The map-based form (vs a
/// per-monitor store hit) lets the refresh walk resolve every monitor from three
/// up-front loads.
pub fn effective_order_from(
    global: &[SourceEntry],
    channel_scope: Option<&SourceScopeConfig>,
    monitor_scope: Option<&SourceScopeConfig>,
) -> Vec<SourceEntry> {
    if let Some(o) = monitor_scope.and_then(|s| s.order.as_ref()) {
        return merge_order(o.clone());
    }
    if let Some(o) = channel_scope.and_then(|s| s.order.as_ref()) {
        return merge_order(o.clone());
    }
    global.to_vec()
}

/// Resolve the effective title-fill toggle: monitor override over channel override
/// over the global default.
pub fn effective_title_fill_from(
    global: bool,
    channel_scope: Option<&SourceScopeConfig>,
    monitor_scope: Option<&SourceScopeConfig>,
) -> bool {
    if let Some(t) = monitor_scope.and_then(|s| s.title_fill) {
        return t;
    }
    if let Some(t) = channel_scope.and_then(|s| s.title_fill) {
        return t;
    }
    global
}

/// Convenience single-item resolver (hits the store) for the effective source
/// order of one channel+monitor pair — for tests and a future per-monitor
/// "effective order" preview, not the bulk refresh walk (which resolves every
/// monitor from three up-front map loads).
#[allow(dead_code)]
pub fn effective_source_order(store: &Store, channel_id: i64, monitor_id: i64) -> Vec<SourceEntry> {
    let global = load_source_order(store);
    let ch = load_channel_scope(store, channel_id);
    let mon = load_monitor_scope(store, monitor_id);
    effective_order_from(&global, Some(&ch), Some(&mon))
}

/// Convenience single-item resolver (hits the store) for the effective title-fill
/// toggle of one channel+monitor pair — for tests and a future per-monitor preview.
#[allow(dead_code)]
pub fn effective_title_fill(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    let global = global_title_fill(store);
    let ch = load_channel_scope(store, channel_id);
    let mon = load_monitor_scope(store, monitor_id);
    effective_title_fill_from(global, Some(&ch), Some(&mon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_roundtrip() {
        for k in ScheduleSourceKind::DEFAULT_ORDER {
            assert_eq!(ScheduleSourceKind::from_id(k.id()), Some(k));
        }
        let ids: std::collections::HashSet<_> =
            ScheduleSourceKind::DEFAULT_ORDER.iter().map(|k| k.id()).collect();
        assert_eq!(ids.len(), ScheduleSourceKind::DEFAULT_ORDER.len());
    }

    #[test]
    fn load_order_merges_unknown_and_missing() {
        let store = Store::open_in_memory().unwrap();
        // Persist a partial list with one unknown id; expect the unknown dropped
        // and all known kinds present (missing appended disabled).
        let partial = r#"[{"id":"youtube","enabled":false},{"id":"bogus","enabled":true}]"#;
        store.set_setting(K_SCHEDULE_SOURCES, partial).unwrap();
        let order = load_source_order(&store);
        assert!(order.iter().all(|e| e.kind().is_some()));
        assert_eq!(order.len(), ScheduleSourceKind::DEFAULT_ORDER.len());
        // The persisted youtube entry kept its order (first) and disabled flag.
        assert_eq!(order[0].id, "youtube");
        assert!(!order[0].enabled);
    }

    #[test]
    fn source_badge_covers_manual_known_and_unknown() {
        // Manual rows are special-cased (no ScheduleSourceKind).
        assert_eq!(source_badge("manual"), ("✏", "Manually edited"));
        // Every known kind resolves to its badge + label.
        for k in ScheduleSourceKind::DEFAULT_ORDER {
            let (icon, label) = source_badge(k.id());
            assert_eq!(icon, k.badge_icon());
            assert_eq!(label, k.label());
        }
        // An unrecognized id falls back instead of panicking.
        assert_eq!(source_badge("totally_unknown").0, "•");
    }

    #[test]
    fn channel_cfg_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let cfg = ChannelSourceConfig {
            twitter_handle: "layna".into(),
            ..Default::default()
        };
        save_channel_cfg(&store, 7, &cfg).unwrap();
        assert_eq!(load_channel_cfg(&store, 7).twitter_handle, "layna");
        // A different channel is unaffected.
        assert_eq!(load_channel_cfg(&store, 8).twitter_handle, "");
    }

    fn entry(id: &str, enabled: bool) -> SourceEntry {
        SourceEntry { id: id.into(), enabled }
    }

    #[test]
    fn community_max_posts_precedence_and_clamp() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = ChannelSourceConfig::default();
        // Nothing set anywhere → the built-in default.
        assert_eq!(community_max_posts(&store, &cfg), DEFAULT_COMMUNITY_POSTS);
        // Global setting applies when the channel override is empty.
        store.set_setting(K_YT_COMMUNITY_MAX_POSTS, "9").unwrap();
        assert_eq!(community_max_posts(&store, &cfg), 9);
        // Per-channel override wins over the global setting.
        cfg.max_community_posts = "3".into();
        assert_eq!(community_max_posts(&store, &cfg), 3);
        // Out-of-range channel value clamps to 1..=20.
        cfg.max_community_posts = "999".into();
        assert_eq!(community_max_posts(&store, &cfg), 20);
        // Zero/garbage falls through to the global setting, not the default.
        cfg.max_community_posts = "0".into();
        assert_eq!(community_max_posts(&store, &cfg), 9);
        cfg.max_community_posts = "abc".into();
        assert_eq!(community_max_posts(&store, &cfg), 9);
    }

    #[test]
    fn scope_inherit_is_not_persisted() {
        let store = Store::open_in_memory().unwrap();
        // An all-None scope is "inherit" and saved as a removal.
        let inherit = SourceScopeConfig::default();
        assert!(inherit.is_inherit());
        save_channel_scope(&store, 5, &inherit).unwrap();
        assert!(store
            .get_setting(K_CHANNEL_SCOPE_CFG)
            .ok()
            .flatten()
            .map(|s| !s.contains("\"5\""))
            .unwrap_or(true));
        // A real override persists, then clearing it removes the entry again.
        let real = SourceScopeConfig { title_fill: Some(true), ..Default::default() };
        save_channel_scope(&store, 5, &real).unwrap();
        assert_eq!(load_channel_scope(&store, 5).title_fill, Some(true));
        save_channel_scope(&store, 5, &inherit).unwrap();
        assert!(load_channel_scope(&store, 5).is_inherit());
    }

    #[test]
    fn effective_order_precedence_monitor_over_channel_over_global() {
        // Global: twitch enabled first. Channel override: youtube first. Monitor
        // override: discord first. Monitor must win when present.
        let global = vec![entry("platform", true), entry("youtube", true)];
        let ch = SourceScopeConfig {
            order: Some(vec![entry("youtube", true)]),
            ..Default::default()
        };
        let mon = SourceScopeConfig {
            order: Some(vec![entry("discord", true)]),
            ..Default::default()
        };
        // Monitor override wins.
        let eff = effective_order_from(&global, Some(&ch), Some(&mon));
        assert_eq!(eff[0].id, "discord");
        // Falls back to channel when the monitor doesn't override the order.
        let eff = effective_order_from(&global, Some(&ch), Some(&SourceScopeConfig::default()));
        assert_eq!(eff[0].id, "youtube");
        // Falls back to global when neither overrides.
        let eff = effective_order_from(
            &global,
            Some(&SourceScopeConfig::default()),
            Some(&SourceScopeConfig::default()),
        );
        assert_eq!(eff[0].id, "platform");
        // A custom order is normalized to the full set of kinds.
        let eff = effective_order_from(&global, Some(&ch), None);
        assert_eq!(eff.len(), ScheduleSourceKind::DEFAULT_ORDER.len());
        assert!(eff.iter().all(|e| e.kind().is_some()));
    }

    #[test]
    fn effective_title_fill_precedence() {
        // Monitor Some wins over channel Some wins over global.
        assert!(effective_title_fill_from(
            false,
            Some(&SourceScopeConfig { title_fill: Some(false), ..Default::default() }),
            Some(&SourceScopeConfig { title_fill: Some(true), ..Default::default() }),
        ));
        // Channel used when the monitor inherits.
        assert!(effective_title_fill_from(
            false,
            Some(&SourceScopeConfig { title_fill: Some(true), ..Default::default() }),
            Some(&SourceScopeConfig::default()),
        ));
        // Global used when both inherit.
        assert!(effective_title_fill_from(true, None, None));
        assert!(!effective_title_fill_from(false, None, None));
    }

    #[test]
    fn effective_resolvers_hit_the_store() {
        // The single-item store-hitting wrappers must agree with the map-based
        // resolvers after persisting overrides.
        let store = Store::open_in_memory().unwrap();
        save_channel_scope(
            &store,
            11,
            &SourceScopeConfig { title_fill: Some(true), ..Default::default() },
        )
        .unwrap();
        save_monitor_scope(
            &store,
            22,
            &SourceScopeConfig {
                order: Some(vec![entry("discord", true)]),
                ..Default::default()
            },
        )
        .unwrap();
        // Monitor order override surfaces through the store wrapper.
        assert_eq!(effective_source_order(&store, 11, 22)[0].id, "discord");
        // Channel title-fill override (monitor inherits) surfaces too.
        assert!(effective_title_fill(&store, 11, 22));
        // Unconfigured ids inherit: global title-fill default is off.
        assert!(!effective_title_fill(&store, 99, 98));
    }
}
