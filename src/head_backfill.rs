//! Global<channel<instance settings for the Twitch capture-from-start head-
//! backfill feature (see `head_backfill_job`/`supersede_old_heads` in
//! `downloader.rs`): whether a later "take" (a reconnect mid-broadcast) should
//! also fetch a fresh head, and whether a verified-good fresh head should
//! replace older takes' now-redundant head files.
//!
//! Same three-level inheritance chain as [`crate::vod_archive::VodArchiveScope`]
//! (monitor override → channel override → global default), `Option<bool>`
//! overrides stored as JSON scope-maps in `app_settings` (no schema
//! migration). Unlike the VOD-archive toggles, both settings here default to
//! **on**, so the global reader defaults `true` on a missing key instead of
//! `false` (the `remux_embed_thumbnail` idiom, not `vod_archive::global_bool`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::store::Store;

// ---------- settings keys ----------

/// Global default: fetch a fresh head backfill for a later take (a retake),
/// not just the stream's first take.
pub const K_HEAD_BACKFILL_FETCH: &str = "head_backfill_fetch_new_take";
/// Global default: once a fresh head passes its integrity checks, delete
/// older takes' now-redundant head files for the same stream.
pub const K_HEAD_BACKFILL_REPLACE: &str = "head_backfill_replace_old";
/// Per-channel scope-config map (`{channel_id -> HeadBackfillScope}`).
pub const K_CHANNEL_HEAD_BACKFILL_SCOPE: &str = "channel_head_backfill_scope";
/// Per-monitor scope-config map (`{monitor_id -> HeadBackfillScope}`).
pub const K_MONITOR_HEAD_BACKFILL_SCOPE: &str = "monitor_head_backfill_scope";

// ---------- three-level scope config (clone of VodArchiveScope) ----------

/// A channel- or monitor-level override of the head-backfill toggles. `None`
/// on a field means "inherit the level above"; `Some(true/false)` forces it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadBackfillScope {
    #[serde(default)]
    pub fetch: Option<bool>,
    #[serde(default)]
    pub replace: Option<bool>,
}

impl HeadBackfillScope {
    /// True when this scope overrides nothing — persisted as a removal so the
    /// map only holds real overrides.
    pub fn is_inherit(&self) -> bool {
        self.fetch.is_none() && self.replace.is_none()
    }
}

/// Load a scope-config map (`{id -> HeadBackfillScope}`) from the given setting key.
fn load_scope_map(store: &Store, key: &str) -> HashMap<String, HeadBackfillScope> {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save one id's scope into the map at `key`, removing inert entries.
fn save_scope(store: &Store, key: &str, id: i64, cfg: &HeadBackfillScope) -> anyhow::Result<()> {
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
pub fn load_channel_head_backfill_scope(store: &Store, channel_id: i64) -> HeadBackfillScope {
    load_scope_map(store, K_CHANNEL_HEAD_BACKFILL_SCOPE)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

/// Save one channel's scope config.
pub fn save_channel_head_backfill_scope(
    store: &Store,
    channel_id: i64,
    cfg: &HeadBackfillScope,
) -> anyhow::Result<()> {
    save_scope(store, K_CHANNEL_HEAD_BACKFILL_SCOPE, channel_id, cfg)
}

/// One monitor's scope config (default = inherit when unset).
pub fn load_monitor_head_backfill_scope(store: &Store, monitor_id: i64) -> HeadBackfillScope {
    load_scope_map(store, K_MONITOR_HEAD_BACKFILL_SCOPE)
        .remove(&monitor_id.to_string())
        .unwrap_or_default()
}

/// Save one monitor's scope config.
pub fn save_monitor_head_backfill_scope(
    store: &Store,
    monitor_id: i64,
    cfg: &HeadBackfillScope,
) -> anyhow::Result<()> {
    save_scope(store, K_MONITOR_HEAD_BACKFILL_SCOPE, monitor_id, cfg)
}

/// Read a global boolean setting that defaults **on** — missing key ⇒ `true`,
/// anything but `"0"` ⇒ `true` (the `remux_embed_thumbnail` idiom). Deliberately
/// NOT `vod_archive::global_bool`, which defaults `false` on a missing key —
/// wrong for a setting that ships on by default.
fn global_bool_default_true(store: &Store, key: &str) -> bool {
    store.get_setting(key).ok().flatten().is_none_or(|v| v != "0")
}

pub fn global_fetch_new_take(store: &Store) -> bool {
    global_bool_default_true(store, K_HEAD_BACKFILL_FETCH)
}

pub fn global_replace_old(store: &Store) -> bool {
    global_bool_default_true(store, K_HEAD_BACKFILL_REPLACE)
}

/// Resolve the effective "fetch a fresh head on a later take" toggle: monitor
/// override over channel override over the global default.
pub fn effective_fetch_new_take_from(
    global: bool,
    channel_scope: Option<&HeadBackfillScope>,
    monitor_scope: Option<&HeadBackfillScope>,
) -> bool {
    if let Some(v) = monitor_scope.and_then(|s| s.fetch) {
        return v;
    }
    if let Some(v) = channel_scope.and_then(|s| s.fetch) {
        return v;
    }
    global
}

/// Resolve the effective "replace older takes' heads" toggle.
pub fn effective_replace_old_from(
    global: bool,
    channel_scope: Option<&HeadBackfillScope>,
    monitor_scope: Option<&HeadBackfillScope>,
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
pub fn effective_fetch_new_take(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    let ch = load_channel_head_backfill_scope(store, channel_id);
    let mon = load_monitor_head_backfill_scope(store, monitor_id);
    effective_fetch_new_take_from(global_fetch_new_take(store), Some(&ch), Some(&mon))
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_replace_old(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    let ch = load_channel_head_backfill_scope(store, channel_id);
    let mon = load_monitor_head_backfill_scope(store, monitor_id);
    effective_replace_old_from(global_replace_old(store), Some(&ch), Some(&mon))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(fetch: Option<bool>, replace: Option<bool>) -> HeadBackfillScope {
        HeadBackfillScope { fetch, replace }
    }

    #[test]
    fn precedence_monitor_over_channel_over_global() {
        // monitor override wins
        assert!(effective_fetch_new_take_from(false, Some(&scope(Some(false), None)), Some(&scope(Some(true), None))));
        assert!(!effective_fetch_new_take_from(true, Some(&scope(Some(true), None)), Some(&scope(Some(false), None))));
        // channel override wins when monitor inherits
        assert!(effective_fetch_new_take_from(false, Some(&scope(Some(true), None)), Some(&scope(None, None))));
        // global when both inherit
        assert!(effective_fetch_new_take_from(true, Some(&scope(None, None)), Some(&scope(None, None))));
        assert!(!effective_fetch_new_take_from(false, None, None));
        // replace resolves independently
        assert!(effective_replace_old_from(false, None, Some(&scope(None, Some(true)))));
        assert!(!effective_replace_old_from(true, Some(&scope(None, Some(false))), Some(&scope(None, None))));
    }

    #[test]
    fn scope_json_roundtrip_and_inherit() {
        assert!(scope(None, None).is_inherit());
        assert!(!scope(Some(false), None).is_inherit());
        let s = scope(Some(true), Some(false));
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<HeadBackfillScope>(&json).unwrap(), s);
        // A legacy/empty object deserializes to inherit.
        assert!(serde_json::from_str::<HeadBackfillScope>("{}").unwrap().is_inherit());
    }

    #[test]
    fn global_defaults_on_when_unset() {
        let store = Store::open_in_memory().unwrap();
        assert!(global_fetch_new_take(&store), "missing key should default true");
        assert!(global_replace_old(&store), "missing key should default true");
        store.set_setting(K_HEAD_BACKFILL_FETCH, "0").unwrap();
        assert!(!global_fetch_new_take(&store));
        store.set_setting(K_HEAD_BACKFILL_REPLACE, "1").unwrap();
        assert!(global_replace_old(&store));
    }

    #[test]
    fn effective_reads_use_default_true_global() {
        let store = Store::open_in_memory().unwrap();
        // No overrides anywhere, no global key written -> defaults on for both.
        assert!(effective_fetch_new_take(&store, 1, 1));
        assert!(effective_replace_old(&store, 1, 1));
        // A monitor override still wins over the default-true global.
        save_monitor_head_backfill_scope(&store, 1, &scope(Some(false), None)).unwrap();
        assert!(!effective_fetch_new_take(&store, 1, 1));
        assert!(effective_replace_old(&store, 1, 1)); // untouched field still inherits
    }
}
