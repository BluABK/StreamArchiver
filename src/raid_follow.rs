//! "Follow raid" settings + resolvers: when a monitored Twitch channel raids
//! out to another channel, optionally auto-record and/or auto-play the raid
//! target for as long as it's live. Record and play are two fully
//! independent behaviors, each with its own 3-level (global -> channel ->
//! monitor) source-side override, same inheritance shape as
//! [`crate::disposal`]:
//!
//! - **Source side, record** (`K_*_RAID_FOLLOW_SCOPE`): "when *this* channel
//!   of mine raids out, should I auto-record the target?" —
//!   `effective_raid_follow_record`.
//! - **Source side, play** (`K_*_RAID_FOLLOW_PLAY_SCOPE`): "when *this*
//!   channel of mine raids out, should I auto-open a live-edge player for
//!   the target (no recording)?" — `effective_raid_follow_play`. This is the
//!   automatic equivalent of the manual "▷🏃 Follow raid" button.
//! - **Destination side, record** (`K_*_RAID_TARGET_SCOPE`): "should *this*
//!   channel ever be auto-RECORDED when it's a raid target?" — an explicit
//!   `Some(bool)` here overrides everything (including the disabled-check
//!   fallback below); `None` inherits down to [`should_record_raid_target`]'s
//!   disabled-skip default.
//! - **Destination side, play** (`K_*_RAID_PLAY_EXCLUDE_SCOPE`): "should
//!   *this* channel ever be auto-PLAYED when it's a raid target?" — a
//!   purpose-built opt-out (`is_excluded_from_auto_play`), NOT tied to the
//!   disabled-check at all: opening a player touches nothing about the
//!   target's own recording/disk configuration, so (unlike record) auto-play
//!   is never gated by the target being disabled — only by this explicit
//!   exclusion, same as the manual Follow raid button already ignores the
//!   target's disabled state unconditionally.
//!
//! Actual detection/orchestration lives in `src/downloader/raid_follow.rs`
//! (needs `Supervisor` internals); this module is the pure settings/resolver
//! layer, usable from UI code without pulling in downloader internals.

use std::collections::HashMap;

use crate::models::MonitorWithChannel;
use crate::store::Store;

// ---------- settings keys ----------

/// Global default: does raiding out ever trigger a follow-RECORD at all?
/// Default OFF — unlike most toggles in this codebase, this creates new
/// recordings of channels the user didn't curate, so it's opt-in.
pub const K_RAID_FOLLOW_RECORD: &str = "raid_follow_record";
/// Per-channel/monitor bool scope-config maps (`{id -> bool}`, absent = inherit).
pub const K_CHANNEL_RAID_FOLLOW_SCOPE: &str = "channel_raid_follow_scope";
pub const K_MONITOR_RAID_FOLLOW_SCOPE: &str = "monitor_raid_follow_scope";
/// Global default: does raiding out ever trigger a follow-PLAY (auto-open a
/// live-edge player, no recording) at all? Default OFF, same reasoning as
/// `K_RAID_FOLLOW_RECORD` — independent of it, either/both/neither can be on.
pub const K_RAID_FOLLOW_PLAY: &str = "raid_follow_play";
pub const K_CHANNEL_RAID_FOLLOW_PLAY_SCOPE: &str = "channel_raid_follow_play_scope";
pub const K_MONITOR_RAID_FOLLOW_PLAY_SCOPE: &str = "monitor_raid_follow_play_scope";
/// Global: gate follow-PLAY on the raiding instance actually being *watched*
/// — a player this app opened for it is still open, or closed within
/// [`RAID_PLAY_WATCHED_GRACE_SECS`]. Default ON: without it, any instance
/// with auto-play enabled pops a player for a raid the user never saw coming
/// (a stream ends while nobody's looking → an unexplained mpv window). "0"
/// disables the gate (auto-play fires whether or not you were watching).
pub const K_RAID_FOLLOW_PLAY_ONLY_WATCHED: &str = "raid_follow_play_only_watched";
/// How recently a player for the raiding instance must have been open to
/// still count as "watching". Raids fire as the source broadcast winds down,
/// and mpv often hits end-of-stream and closes moments before the EventSub
/// raid event arrives — "I was literally just watching" must still count.
pub const RAID_PLAY_WATCHED_GRACE_SECS: i64 = 600;
/// Global: output directory for an untracked (not one of our channels) raid
/// target's ad-hoc capture. Supports the `{name}` token.
pub const K_RAID_FOLLOW_OUTPUT_DIR: &str = "raid_follow_output_dir";
/// Per-channel/monitor bool scope-config maps for the destination side (record).
pub const K_CHANNEL_RAID_TARGET_SCOPE: &str = "channel_raid_target_scope";
pub const K_MONITOR_RAID_TARGET_SCOPE: &str = "monitor_raid_target_scope";
/// Global: skip auto-recording a tracked raid target that's currently
/// disabled (master switch off, at either channel or monitor level).
/// Default ON — the safe/conservative default; a channel can override this
/// per-channel/instance via `K_*_RAID_TARGET_SCOPE`.
pub const K_RAID_SKIP_DISABLED_TARGETS: &str = "raid_skip_disabled_targets";
/// Per-channel/monitor bool scope-config maps for the destination side
/// (play): an explicit `true` excludes this channel from ever being
/// auto-played as a raid target, `false`/absent allows it (the default).
/// Unlike the record-side target scope, there's no disabled-check fallback
/// and no global default — auto-play is allowed unless explicitly excluded.
pub const K_CHANNEL_RAID_PLAY_EXCLUDE_SCOPE: &str = "channel_raid_play_exclude_scope";
pub const K_MONITOR_RAID_PLAY_EXCLUDE_SCOPE: &str = "monitor_raid_play_exclude_scope";

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

// ---------- source side: "auto-play my raids" ----------

pub fn global_raid_follow_play(store: &Store) -> bool {
    store
        .get_setting(K_RAID_FOLLOW_PLAY)
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Store-hitting resolver for one channel+monitor pair (play side) — same
/// monitor-over-channel-over-global precedence as the record side, via the
/// shared [`effective_raid_follow_record_from`] precedence helper (the name
/// is record-specific only because it shipped first; the precedence logic
/// itself doesn't care whether the bool means "record" or "play").
pub fn effective_raid_follow_play(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    let ch = load_bool_scope(store, K_CHANNEL_RAID_FOLLOW_PLAY_SCOPE, channel_id);
    let mon = load_bool_scope(store, K_MONITOR_RAID_FOLLOW_PLAY_SCOPE, monitor_id);
    effective_raid_follow_record_from(global_raid_follow_play(store), ch, mon)
}

/// Whether follow-play additionally requires the raiding instance to have
/// been open in a player (see [`K_RAID_FOLLOW_PLAY_ONLY_WATCHED`]).
pub fn raid_follow_play_only_watched(store: &Store) -> bool {
    store
        .get_setting(K_RAID_FOLLOW_PLAY_ONLY_WATCHED)
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true)
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

/// The master switch being off, at either channel or monitor level. Auto-
/// record being off does NOT count — same distinction Trigger Words already
/// draw (`try_begin`'s `auto_off` bypass in `downloader/supervisor.rs`):
/// Auto-record is a disk-space control on unattended polling, not a "don't
/// ever force-start this" signal, and follow-raid is a forced trigger like
/// a trigger-word match, not a poll. A channel/instance the user has
/// deliberately left in manual-only mode (Auto off) should still record
/// when a followed raid lands on it; only the master switch means "leave
/// this alone entirely."
pub fn target_is_disabled(row: &MonitorWithChannel) -> bool {
    !row.channel.automation_enabled || !row.monitor.automation_enabled
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

// ---------- destination side: "exclude from auto-play" ----------

/// Whether a channel/instance has explicitly opted out of ever being
/// auto-played as a raid target. Monitor overrides channel; absent at both
/// levels means NOT excluded (auto-play's default is to allow, unlike
/// record's disabled-skip default — there's no disabled-check involved at
/// all here, see the module doc comment).
pub fn is_excluded_from_auto_play(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    load_bool_scope(store, K_MONITOR_RAID_PLAY_EXCLUDE_SCOPE, monitor_id)
        .or_else(|| load_bool_scope(store, K_CHANNEL_RAID_PLAY_EXCLUDE_SCOPE, channel_id))
        .unwrap_or(false)
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
    fn target_is_disabled_ignores_auto_record_only_master_switch_counts() {
        use crate::models::{Container, Platform, Tool};
        let row = crate::downloader::test_util::row(Tool::Streamlink, Container::Ts, Platform::Twitch);
        // Fully on: not disabled.
        assert!(!target_is_disabled(&row));
        // Auto-record off (either level) — NOT disabled, unlike the old behavior.
        let mut r = row.clone();
        r.monitor.enabled = false;
        assert!(!target_is_disabled(&r));
        let mut r = row.clone();
        r.channel.enabled = false;
        assert!(!target_is_disabled(&r));
        // Master switch off (either level) — disabled.
        let mut r = row.clone();
        r.monitor.automation_enabled = false;
        assert!(target_is_disabled(&r));
        let mut r = row.clone();
        r.channel.automation_enabled = false;
        assert!(target_is_disabled(&r));
    }

    #[test]
    fn raid_follow_play_and_exclude_scope_roundtrip() {
        let store = crate::store::Store::open_in_memory().unwrap();
        // Play: same precedence shape as record, independent setting/keys.
        assert!(!effective_raid_follow_play(&store, 1, 1));
        store.set_setting(K_RAID_FOLLOW_PLAY, "1").unwrap();
        assert!(effective_raid_follow_play(&store, 1, 1));
        save_bool_scope(&store, K_MONITOR_RAID_FOLLOW_PLAY_SCOPE, 1, Some(false)).unwrap();
        assert!(!effective_raid_follow_play(&store, 1, 1));
        // Record-side global is untouched by the play-side setting above.
        assert!(!effective_raid_follow_record(&store, 1, 1));

        // Exclude-from-auto-play: default allowed, monitor overrides channel.
        assert!(!is_excluded_from_auto_play(&store, 2, 2));
        save_bool_scope(&store, K_CHANNEL_RAID_PLAY_EXCLUDE_SCOPE, 2, Some(true)).unwrap();
        assert!(is_excluded_from_auto_play(&store, 2, 2));
        save_bool_scope(&store, K_MONITOR_RAID_PLAY_EXCLUDE_SCOPE, 2, Some(false)).unwrap();
        assert!(!is_excluded_from_auto_play(&store, 2, 2));
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
