//! Manual "Delete file from disk (keep row)" action for a take row — a
//! right-click context-menu item (no hotkey, deliberately) that disposes of
//! a take's `output_path` per the same disposal-method resolution automatic
//! cleanup already uses ([`crate::disposal::dispose_media`] — trash folder /
//! Recycle Bin / permanent delete, whichever the instance/channel/global
//! chain resolves to), then clears the recording row's `output_path` so the
//! row itself stays (title, stats, chapters, chat log, notes, etc. are all
//! untouched) — every "open file"/"play" action on it just goes inert, the
//! same as any other take whose file happens to be missing.
//!
//! Deleting a captured file is high-consequence and sits in a context menu
//! otherwise full of harmless read-only actions, so three independent gates
//! must ALL be on before the menu item is even enabled:
//! - the Streams view's own "Allow deletion" toolbar checkbox (session-wide
//!   master switch, persisted, off by default)
//! - this take's CHANNEL's "Allow delete" setting (off by default)
//! - this take's INSTANCE's "Allow delete" setting (off by default)
//!
//! None of the three is an "inherit" chain the way most scoped settings in
//! this codebase are (disposal method, chapters, follow-raid, …) — there's
//! no adjustable global default underneath the channel/instance settings,
//! only the master switch above, so each is its own independent
//! off-by-default switch and all three must be explicitly turned on. A
//! confirmation dialog naming the resolved disposal method is still shown on
//! top of that (see `confirm_delete_file_window` in `ui/dialogs.rs`).

use crate::raid_follow::load_bool_scope;
use crate::store::Store;

/// `(rec_id, outcome)` for a finished manual delete — posted cross-thread by
/// the `core.rt.spawn`'d disposal task, drained by the UI once per frame. A
/// named alias (not inlined at the field) so `StreamArchiverApp`'s
/// `Arc<Mutex<Vec<_>>>` field doesn't trip clippy's `type_complexity` lint —
/// same shape as `ui::trash::TrashActionOutcome`.
pub type ManualDeleteOutcome = (i64, Result<String, String>);

/// `app_settings` key for the Streams view's "Allow deletion" master-switch
/// checkbox — `"1"`/`"0"`, defaults to off. Saved immediately on toggle, same
/// shape as `K_STREAMS_ONLY_RECORDED`.
pub const K_STREAMS_ALLOW_DELETE: &str = "streams_allow_delete";
/// Per-channel/monitor bool scope-config maps (`{id -> bool}`, absent =
/// false/blocked) — reuses `raid_follow`'s generic bool scope-map helpers.
pub const K_CHANNEL_ALLOW_DELETE: &str = "channel_allow_delete";
pub const K_MONITOR_ALLOW_DELETE: &str = "monitor_allow_delete";

/// Whether "Delete file from disk" should even be enabled for a take
/// belonging to `channel_id`/`monitor_id` — all three gates must be on.
pub fn deletion_allowed(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    master_switch_on(store)
        && load_bool_scope(store, K_CHANNEL_ALLOW_DELETE, channel_id).unwrap_or(false)
        && load_bool_scope(store, K_MONITOR_ALLOW_DELETE, monitor_id).unwrap_or(false)
}

/// Just the master-switch gate, for the toolbar checkbox and disabled-hover
/// text that needs to say specifically which gate is missing.
pub fn master_switch_on(store: &Store) -> bool {
    store.get_setting(K_STREAMS_ALLOW_DELETE).ok().flatten().as_deref() == Some("1")
}

/// Channel-level gate alone, for the same disabled-hover breakdown.
pub fn channel_gate_on(store: &Store, channel_id: i64) -> bool {
    load_bool_scope(store, K_CHANNEL_ALLOW_DELETE, channel_id).unwrap_or(false)
}

/// Instance-level gate alone, for the same disabled-hover breakdown.
pub fn monitor_gate_on(store: &Store, monitor_id: i64) -> bool {
    load_bool_scope(store, K_MONITOR_ALLOW_DELETE, monitor_id).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raid_follow::save_bool_scope;

    #[test]
    fn all_three_gates_default_off_and_are_independently_required() {
        let store = Store::open_in_memory().unwrap();
        assert!(!deletion_allowed(&store, 1, 1), "nothing set — must default to blocked");

        store.set_setting(K_STREAMS_ALLOW_DELETE, "1").unwrap();
        assert!(!deletion_allowed(&store, 1, 1), "master switch alone isn't enough");

        save_bool_scope(&store, K_CHANNEL_ALLOW_DELETE, 1, Some(true)).unwrap();
        assert!(!deletion_allowed(&store, 1, 1), "master + channel still missing the instance gate");

        save_bool_scope(&store, K_MONITOR_ALLOW_DELETE, 1, Some(true)).unwrap();
        assert!(deletion_allowed(&store, 1, 1), "all three on — now allowed");

        // Flipping the master switch back off blocks it again even though
        // both scoped settings are still on — it's a true AND gate, not an
        // inherit chain any one level can satisfy alone.
        store.set_setting(K_STREAMS_ALLOW_DELETE, "0").unwrap();
        assert!(!deletion_allowed(&store, 1, 1));
    }

    #[test]
    fn gates_are_scoped_per_channel_and_monitor() {
        let store = Store::open_in_memory().unwrap();
        store.set_setting(K_STREAMS_ALLOW_DELETE, "1").unwrap();
        save_bool_scope(&store, K_CHANNEL_ALLOW_DELETE, 1, Some(true)).unwrap();
        save_bool_scope(&store, K_MONITOR_ALLOW_DELETE, 1, Some(true)).unwrap();

        assert!(deletion_allowed(&store, 1, 1));
        // A different channel/monitor pair never inherits another one's
        // allowance — each id needs its own explicit opt-in.
        assert!(!deletion_allowed(&store, 2, 1));
        assert!(!deletion_allowed(&store, 1, 2));
    }
}
