//! "Follow raid" settings + resolvers: when a monitored Twitch channel raids
//! out to another channel, optionally auto-record the raid target for as
//! long as it's live. Two independent 3-level (global -> channel -> monitor)
//! overrides, same inheritance shape as [`crate::disposal`]:
//!
//! - **Source side** (`K_*_RAID_FOLLOW_SCOPE`): "when *this* channel of mine
//!   raids out, should I follow it?" — `effective_raid_follow_record`.
//! - **Destination side** (`K_*_RAID_TARGET_SCOPE`): "should *this* channel
//!   ever be auto-recorded when it's a raid target?" — an explicit
//!   `Some(bool)` here overrides everything (including the disabled-check
//!   fallback below); `None` inherits down to [`should_record_raid_target`]'s
//!   disabled-skip default.
//!
//! Actual detection/orchestration lives in `src/downloader/raid_follow.rs`
//! (needs `Supervisor` internals); this module is the pure settings/resolver
//! layer, usable from UI code without pulling in downloader internals.

use std::collections::HashMap;

use crate::models::MonitorWithChannel;
use crate::store::Store;

// ---------- settings keys ----------

/// Global default: does raiding out ever trigger a follow-record at all?
/// Default OFF — unlike most toggles in this codebase, this creates new
/// recordings of channels the user didn't curate, so it's opt-in.
pub const K_RAID_FOLLOW_RECORD: &str = "raid_follow_record";
/// Per-channel/monitor bool scope-config maps (`{id -> bool}`, absent = inherit).
pub const K_CHANNEL_RAID_FOLLOW_SCOPE: &str = "channel_raid_follow_scope";
pub const K_MONITOR_RAID_FOLLOW_SCOPE: &str = "monitor_raid_follow_scope";
/// Global: output directory for an untracked (not one of our channels) raid
/// target's ad-hoc capture. Supports the `{name}` token.
pub const K_RAID_FOLLOW_OUTPUT_DIR: &str = "raid_follow_output_dir";
/// Per-channel/monitor bool scope-config maps for the destination side.
pub const K_CHANNEL_RAID_TARGET_SCOPE: &str = "channel_raid_target_scope";
pub const K_MONITOR_RAID_TARGET_SCOPE: &str = "monitor_raid_target_scope";
/// Global: skip auto-recording a tracked raid target that's currently
/// disabled (master switch or Auto-record off, at either channel or
/// monitor level). Default ON — the safe/conservative default; a channel
/// can override this per-channel/instance via `K_*_RAID_TARGET_SCOPE`.
pub const K_RAID_SKIP_DISABLED_TARGETS: &str = "raid_skip_disabled_targets";

// ---------- generic bool scope-map helpers ----------

fn load_bool_map(store: &Store, key: &str) -> HashMap<String, bool> {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn load_bool_scope(store: &Store, key: &str, id: i64) -> Option<bool> {
    load_bool_map(store, key).get(&id.to_string()).copied()
}

/// `None` removes the override (inherit); `Some(v)` sets it.
pub fn save_bool_scope(store: &Store, key: &str, id: i64, value: Option<bool>) -> anyhow::Result<()> {
    let mut map = load_bool_map(store, key);
    match value {
        Some(v) => {
            map.insert(id.to_string(), v);
        }
        None => {
            map.remove(&id.to_string());
        }
    }
    store.set_setting(key, &serde_json::to_string(&map)?)?;
    Ok(())
}

// ---------- source side: "follow my raids" ----------

pub fn global_raid_follow_record(store: &Store) -> bool {
    store
        .get_setting(K_RAID_FOLLOW_RECORD)
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Monitor override over channel override over the global default.
pub fn effective_raid_follow_record_from(
    global: bool,
    channel_scope: Option<bool>,
    monitor_scope: Option<bool>,
) -> bool {
    monitor_scope.or(channel_scope).unwrap_or(global)
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_raid_follow_record(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    let ch = load_bool_scope(store, K_CHANNEL_RAID_FOLLOW_SCOPE, channel_id);
    let mon = load_bool_scope(store, K_MONITOR_RAID_FOLLOW_SCOPE, monitor_id);
    effective_raid_follow_record_from(global_raid_follow_record(store), ch, mon)
}

pub fn raid_follow_output_dir(store: &Store) -> String {
    store.get_setting(K_RAID_FOLLOW_OUTPUT_DIR).ok().flatten().unwrap_or_default()
}

// ---------- destination side: "record me when I'm a raid target" ----------

pub fn raid_skip_disabled_targets_enabled(store: &Store) -> bool {
    store
        .get_setting(K_RAID_SKIP_DISABLED_TARGETS)
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Monitor override over channel override — `None` when neither level has an
/// explicit preference (falls through to the disabled-check default in
/// [`should_record_raid_target`]).
pub fn effective_raid_target_record_scope(
    store: &Store,
    channel_id: i64,
    monitor_id: i64,
) -> Option<bool> {
    load_bool_scope(store, K_MONITOR_RAID_TARGET_SCOPE, monitor_id)
        .or_else(|| load_bool_scope(store, K_CHANNEL_RAID_TARGET_SCOPE, channel_id))
}

/// Either the master switch or Auto-record being off, at either channel or
/// monitor level — the broadest reading of "disabled".
pub fn target_is_disabled(row: &MonitorWithChannel) -> bool {
    !row.channel.automation_enabled
        || !row.monitor.automation_enabled
        || !row.channel.enabled
        || !row.monitor.enabled
}

/// Pure core of [`should_record_raid_target`]: an explicit channel/instance
/// override always wins (even over a disabled target); otherwise record
/// unless the target is disabled AND the skip-disabled default is on.
pub fn should_record_raid_target_from(
    explicit_scope: Option<bool>,
    is_disabled: bool,
    skip_disabled_targets: bool,
) -> bool {
    match explicit_scope {
        Some(v) => v,
        None => !(is_disabled && skip_disabled_targets),
    }
}

/// Whether a TRACKED raid target should be auto-recorded via follow-raid.
/// Not meaningful for untracked/ad-hoc targets (they have no channel/monitor
/// row to scope against, and are always eligible subject only to the source
/// side's `effective_raid_follow_record`).
pub fn should_record_raid_target(store: &Store, row: &MonitorWithChannel) -> bool {
    should_record_raid_target_from(
        effective_raid_target_record_scope(store, row.channel.id, row.monitor.id),
        target_is_disabled(row),
        raid_skip_disabled_targets_enabled(store),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raid_follow_record_precedence() {
        assert!(!effective_raid_follow_record_from(false, None, None));
        assert!(effective_raid_follow_record_from(true, None, None));
        assert!(effective_raid_follow_record_from(false, Some(true), None));
        assert!(!effective_raid_follow_record_from(true, Some(false), None));
        // Monitor wins over channel.
        assert!(effective_raid_follow_record_from(false, Some(false), Some(true)));
        assert!(!effective_raid_follow_record_from(true, Some(true), Some(false)));
    }

    #[test]
    fn should_record_raid_target_table() {
        // Explicit override always wins, disabled or not.
        assert!(should_record_raid_target_from(Some(true), true, true));
        assert!(!should_record_raid_target_from(Some(false), false, false));
        // No override: skip only when disabled AND the global default skips.
        assert!(should_record_raid_target_from(None, false, true));
        assert!(should_record_raid_target_from(None, true, false));
        assert!(!should_record_raid_target_from(None, true, true));
        assert!(should_record_raid_target_from(None, false, false));
    }
}
