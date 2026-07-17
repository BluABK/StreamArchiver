//! Preferred platform when a channel has more than one instance simultaneously
//! live — decides which instance's Title/Game/Viewers/Went-Live/Started-On/
//! Duration drive the rolled-up channel row (`channel_primary_preferred` in
//! `src/ui/grid.rs`), instead of the plain "earliest live instance wins"
//! default (`channel_primary`).
//!
//! Three-level inheritance — **instance pin < channel override < global
//! default** — mirroring [`crate::vod_archive`]'s scope-map pattern exactly:
//! per-channel overrides are a `{channel_id -> Platform}` JSON map in
//! `app_settings` (no schema migration), and the instance level is a
//! `{monitor_id -> true}` pin map rather than a platform (an instance is
//! already one specific platform, so "prefer this instance" is a stronger,
//! more specific signal than "prefer this platform").

use std::collections::{HashMap, HashSet};

use crate::models::Platform;
use crate::store::Store;

/// Global default preferred platform. Empty/absent = no preference (falls
/// back to `channel_primary`'s plain earliest-live-wins behavior).
pub const K_PRIMARY_PLATFORM_PREF: &str = "primary_platform_pref";
/// Per-channel scope-config map (`{channel_id -> Platform}`); a channel absent
/// from the map inherits the global default.
pub const K_CHANNEL_PRIMARY_PLATFORM_SCOPE: &str = "channel_primary_platform_scope";
/// Per-monitor pin map (`{monitor_id -> true}`) — only `true` entries are ever
/// stored (unpinning removes the entry, same as `vod_archive`'s scope maps
/// only storing real overrides).
pub const K_MONITOR_PRIMARY_PIN: &str = "monitor_primary_pin";

/// The global default preferred platform, or `None` if unset/unparseable.
pub fn global_primary_platform(store: &Store) -> Option<Platform> {
    store
        .get_setting(K_PRIMARY_PLATFORM_PREF)
        .ok()
        .flatten()
        .and_then(|s| Platform::parse_opt(&s))
}

pub fn set_global_primary_platform(store: &Store, platform: Option<Platform>) -> anyhow::Result<()> {
    store.set_setting(K_PRIMARY_PLATFORM_PREF, platform.map(Platform::as_str).unwrap_or(""))?;
    Ok(())
}

/// Load the per-channel platform-preference map.
pub fn load_channel_platform_scope_map(store: &Store) -> HashMap<String, Platform> {
    store
        .get_setting(K_CHANNEL_PRIMARY_PLATFORM_SCOPE)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// One channel's platform-preference override (`None` = inherit the global
/// default).
pub fn channel_primary_platform(store: &Store, channel_id: i64) -> Option<Platform> {
    load_channel_platform_scope_map(store).remove(&channel_id.to_string())
}

/// Save (or clear, when `platform` is `None`) one channel's override.
pub fn save_channel_primary_platform(
    store: &Store,
    channel_id: i64,
    platform: Option<Platform>,
) -> anyhow::Result<()> {
    let mut map = load_channel_platform_scope_map(store);
    match platform {
        Some(p) => {
            map.insert(channel_id.to_string(), p);
        }
        None => {
            map.remove(&channel_id.to_string());
        }
    }
    store.set_setting(K_CHANNEL_PRIMARY_PLATFORM_SCOPE, &serde_json::to_string(&map)?)?;
    Ok(())
}

/// Load the set of pinned monitor ids ("always show this instance when live").
pub fn load_monitor_pin_map(store: &Store) -> HashSet<i64> {
    store
        .get_setting(K_MONITOR_PRIMARY_PIN)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str::<HashMap<String, bool>>(&s).ok())
        .map(|m| m.into_iter().filter(|&(_, v)| v).filter_map(|(k, _)| k.parse().ok()).collect())
        .unwrap_or_default()
}

pub fn monitor_is_pinned(store: &Store, monitor_id: i64) -> bool {
    load_monitor_pin_map(store).contains(&monitor_id)
}

/// Save (or clear) one monitor's pin.
pub fn save_monitor_pin(store: &Store, monitor_id: i64, pinned: bool) -> anyhow::Result<()> {
    let mut map = load_monitor_pin_map(store);
    if pinned {
        map.insert(monitor_id);
    } else {
        map.remove(&monitor_id);
    }
    // Persisted as `{id -> true}` (never `false`) so the map only ever holds
    // real pins, matching `vod_archive`'s "inert entries removed" convention.
    let json_map: HashMap<String, bool> = map.into_iter().map(|id| (id.to_string(), true)).collect();
    store.set_setting(K_MONITOR_PRIMARY_PIN, &serde_json::to_string(&json_map)?)?;
    Ok(())
}

/// Resolve the effective preferred platform for one channel: channel override
/// beats the global default. Pure — the instance-pin tier is handled
/// separately by `channel_primary_preferred` since it isn't a platform pick.
pub fn effective_primary_platform_from(
    global: Option<Platform>,
    channel_override: Option<Platform>,
) -> Option<Platform> {
    channel_override.or(global)
}

/// A one-shot snapshot of the whole preference config, loaded once (e.g. per
/// Streams-view cache rebuild, NOT per row/frame — `channel_row`/`channel_cells`
/// run for every channel, every frame, so hitting the store per call would
/// mean a `get_setting` + JSON parse per channel per repaint) and reused for
/// every channel's `effective()` lookup and every `channel_primary_preferred`
/// pin check.
#[derive(Default)]
pub struct PlatformPrefCtx {
    global: Option<Platform>,
    channel_overrides: HashMap<i64, Platform>,
    pub pins: HashSet<i64>,
}

impl PlatformPrefCtx {
    pub fn load(store: &Store) -> Self {
        let channel_overrides = load_channel_platform_scope_map(store)
            .into_iter()
            .filter_map(|(k, v)| k.parse().ok().map(|id| (id, v)))
            .collect();
        PlatformPrefCtx {
            global: global_primary_platform(store),
            channel_overrides,
            pins: load_monitor_pin_map(store),
        }
    }

    /// The effective preferred platform for one channel: channel override,
    /// else the global default.
    pub fn effective(&self, channel_id: i64) -> Option<Platform> {
        effective_primary_platform_from(self.global, self.channel_overrides.get(&channel_id).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_platform_channel_overrides_global() {
        assert_eq!(
            effective_primary_platform_from(Some(Platform::Twitch), Some(Platform::YouTube)),
            Some(Platform::YouTube),
        );
        assert_eq!(effective_primary_platform_from(Some(Platform::Twitch), None), Some(Platform::Twitch));
        assert_eq!(effective_primary_platform_from(None, Some(Platform::Kick)), Some(Platform::Kick));
        assert_eq!(effective_primary_platform_from(None, None), None);
    }

    #[test]
    fn channel_scope_roundtrip_and_clear() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(channel_primary_platform(&store, 5), None);
        save_channel_primary_platform(&store, 5, Some(Platform::YouTube)).unwrap();
        assert_eq!(channel_primary_platform(&store, 5), Some(Platform::YouTube));
        // A different channel is unaffected.
        assert_eq!(channel_primary_platform(&store, 6), None);
        // Clearing removes the entry entirely (not just sets it to a "none" value).
        save_channel_primary_platform(&store, 5, None).unwrap();
        assert_eq!(channel_primary_platform(&store, 5), None);
        assert!(load_channel_platform_scope_map(&store).is_empty());
    }

    #[test]
    fn monitor_pin_roundtrip_and_clear() {
        let store = Store::open_in_memory().unwrap();
        assert!(!monitor_is_pinned(&store, 42));
        save_monitor_pin(&store, 42, true).unwrap();
        assert!(monitor_is_pinned(&store, 42));
        assert!(!monitor_is_pinned(&store, 43));
        save_monitor_pin(&store, 42, false).unwrap();
        assert!(!monitor_is_pinned(&store, 42));
        assert!(load_monitor_pin_map(&store).is_empty());
    }

    #[test]
    fn global_platform_roundtrip_and_clear() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(global_primary_platform(&store), None);
        set_global_primary_platform(&store, Some(Platform::Twitch)).unwrap();
        assert_eq!(global_primary_platform(&store), Some(Platform::Twitch));
        set_global_primary_platform(&store, None).unwrap();
        assert_eq!(global_primary_platform(&store), None);
    }
}
