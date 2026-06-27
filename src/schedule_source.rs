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

use crate::models::{K_CHANNEL_SOURCE_CFG, K_DISCORD_SCHEDULE, K_SCHEDULE_SOURCES, Platform};
use crate::store::Store;

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

/// Load the user's ordered source list. When nothing is persisted, returns the
/// seeded default order. When something is persisted, keeps the user's order +
/// flags for known ids and appends any kinds added in a newer build at the end
/// (disabled, so a new source never silently activates).
pub fn load_source_order(store: &Store) -> Vec<SourceEntry> {
    let raw = store.get_setting(K_SCHEDULE_SOURCES).ok().flatten();
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return default_order(store);
    };
    let mut entries: Vec<SourceEntry> = match serde_json::from_str::<Vec<SourceEntry>>(&raw) {
        Ok(v) => v.into_iter().filter(|e| e.kind().is_some()).collect(),
        Err(_) => return default_order(store),
    };
    // Append any kinds not present in the persisted list (forward-compat).
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
}
