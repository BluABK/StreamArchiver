//! Persisted, user-defined "saved views" for a grid table: a named snapshot
//! of its sort, per-column filters, and (Streams-specific today) the
//! channel-group visual-clustering toggle and Group/Recording-group filter
//! selections. Lets a user build up e.g. one view that shows channels
//! grouped and sorted by name, another flat and sorted by last-added,
//! without the app needing to hardcode a fixed set of "modes" — see
//! [`crate::grid_columns`], whose `HashMap<String, T>`-in-one-setting-key
//! shape this mirrors almost exactly, generalized from column config to a
//! whole named preset.
//!
//! A view's identity IS its name (unique within a table, enforced by the
//! caller) — there's no separate synthetic id, so a rename is just editing
//! the one field that also serves as the lookup key.
//! `channel_group_id`/`recording_group_id` reference the *actual* channel/
//! recording groups by id, with the same tolerant-to-staleness handling the
//! rest of the app already uses for ids that may since have been deleted
//! (e.g. [`crate::grid_columns::resolve_sort`] silently dropping unknown
//! column ids): applying a view whose referenced group no longer exists just
//! leaves that filter unset instead of erroring.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::grid_columns::{GridCol, GridTableId, PersistedSort};
use crate::models::K_SAVED_VIEWS;
use crate::store::Store;

fn default_true() -> bool {
    true
}

/// One saved view: a named snapshot of a table's sort/grouping/filters. See
/// the module docs for the "name is the id" design.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedView {
    pub name: String,
    #[serde(default)]
    pub sort: PersistedSort,
    /// Streams-only today (channel-group header clustering) — unused but
    /// harmless for any other table.
    #[serde(default = "default_true")]
    pub group_visually: bool,
    /// Column id -> filter text; only non-empty filters are stored.
    #[serde(default)]
    pub filters: HashMap<String, String>,
    #[serde(default)]
    pub channel_group_id: Option<i64>,
    #[serde(default)]
    pub recording_group_id: Option<i64>,
}

fn all_views_map(store: &Store) -> HashMap<String, Vec<SavedView>> {
    store
        .get_setting(K_SAVED_VIEWS)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_views_map(store: &Store, map: &HashMap<String, Vec<SavedView>>) {
    if let Ok(json) = serde_json::to_string(map) {
        let _ = store.set_setting(K_SAVED_VIEWS, &json);
    }
}

/// A table's saved views, in creation order.
pub fn list_views(store: &Store, table: GridTableId) -> Vec<SavedView> {
    all_views_map(store).remove(table.key()).unwrap_or_default()
}

/// Persist one table's saved-view list (read-modify-write the shared map).
pub fn save_views(store: &Store, table: GridTableId, views: &[SavedView]) {
    let mut map = all_views_map(store);
    map.insert(table.key().to_string(), views.to_vec());
    save_views_map(store, &map);
}

/// Insert or overwrite (by name) one view in a table's saved list.
pub fn upsert_view(store: &Store, table: GridTableId, view: SavedView) {
    let mut views = list_views(store, table);
    match views.iter_mut().find(|v| v.name == view.name) {
        Some(existing) => *existing = view,
        None => views.push(view),
    }
    save_views(store, table, &views);
}

/// Rename a view in place, preserving its position; no-op (returns `false`)
/// if `old` isn't found. The caller is responsible for re-pointing anything
/// tracking the view by its old name (e.g. a "currently applied" pointer).
pub fn rename_view(store: &Store, table: GridTableId, old: &str, new: &str) -> bool {
    let mut views = list_views(store, table);
    let Some(v) = views.iter_mut().find(|v| v.name == old) else {
        return false;
    };
    v.name = new.to_string();
    save_views(store, table, &views);
    true
}

/// Delete a view by name; no-op if absent.
pub fn delete_view(store: &Store, table: GridTableId, name: &str) {
    let mut views = list_views(store, table);
    views.retain(|v| v.name != name);
    save_views(store, table, &views);
}

/// Column-id-keyed filter map (only non-empty filters) -> the runtime,
/// index-positioned `Vec<String>` a table's `ordered_rows` uses. Mirrors
/// [`crate::grid_columns::resolve_sort`]'s tolerance: an id no longer present
/// in `columns` is silently dropped.
pub fn resolve_filters(columns: &[GridCol], map: &HashMap<String, String>) -> Vec<String> {
    columns
        .iter()
        .map(|c| map.get(c.id).cloned().unwrap_or_default())
        .collect()
}

/// The reverse of [`resolve_filters`]: index-positioned filters -> a
/// column-id-keyed map, storing only non-empty entries.
pub fn unresolve_filters(columns: &[GridCol], filters: &[String]) -> HashMap<String, String> {
    columns
        .iter()
        .zip(filters)
        .filter(|(_, f)| !f.trim().is_empty())
        .map(|(c, f)| (c.id.to_string(), f.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: [GridCol; 2] = [
        GridCol { id: "a", title: "A", tooltip: "", min_width: 10.0, initial: 0.0, sortable: true, stretch: false },
        GridCol { id: "b", title: "B", tooltip: "", min_width: 10.0, initial: 0.0, sortable: true, stretch: false },
    ];

    #[test]
    fn upsert_inserts_then_overwrites_by_name() {
        let store = Store::open_in_memory().unwrap();
        upsert_view(&store, GridTableId::Streams, SavedView { name: "Grouped".into(), ..Default::default() });
        assert_eq!(list_views(&store, GridTableId::Streams).len(), 1);
        upsert_view(
            &store,
            GridTableId::Streams,
            SavedView { name: "Grouped".into(), group_visually: false, ..Default::default() },
        );
        let views = list_views(&store, GridTableId::Streams);
        assert_eq!(views.len(), 1, "same name overwrites in place, doesn't duplicate");
        assert!(!views[0].group_visually);
    }

    #[test]
    fn rename_updates_in_place_preserving_position() {
        let store = Store::open_in_memory().unwrap();
        upsert_view(&store, GridTableId::Streams, SavedView { name: "A".into(), ..Default::default() });
        upsert_view(&store, GridTableId::Streams, SavedView { name: "B".into(), ..Default::default() });
        assert!(rename_view(&store, GridTableId::Streams, "A", "A2"));
        let views = list_views(&store, GridTableId::Streams);
        assert_eq!(views.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["A2", "B"]);
        assert!(!rename_view(&store, GridTableId::Streams, "nope", "x"), "unknown name is a no-op");
    }

    #[test]
    fn delete_removes_by_name_leaves_others() {
        let store = Store::open_in_memory().unwrap();
        upsert_view(&store, GridTableId::Streams, SavedView { name: "A".into(), ..Default::default() });
        upsert_view(&store, GridTableId::Streams, SavedView { name: "B".into(), ..Default::default() });
        delete_view(&store, GridTableId::Streams, "A");
        let views = list_views(&store, GridTableId::Streams);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "B");
    }

    #[test]
    fn views_are_per_table() {
        let store = Store::open_in_memory().unwrap();
        upsert_view(&store, GridTableId::Streams, SavedView { name: "S".into(), ..Default::default() });
        upsert_view(&store, GridTableId::Videos, SavedView { name: "V".into(), ..Default::default() });
        assert_eq!(list_views(&store, GridTableId::Streams).len(), 1);
        assert_eq!(list_views(&store, GridTableId::Videos).len(), 1);
    }

    #[test]
    fn filters_roundtrip_drops_empty_and_unknown_ids() {
        let filters = vec!["foo".to_string(), String::new()];
        let map = unresolve_filters(&COLS, &filters);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("a"), Some(&"foo".to_string()));
        let back = resolve_filters(&COLS, &map);
        assert_eq!(back, vec!["foo".to_string(), String::new()]);

        // An id no longer present in `columns` is silently dropped, same
        // tolerance as grid_columns::resolve_sort.
        let mut stale = HashMap::new();
        stale.insert("zzz".to_string(), "x".to_string());
        stale.insert("b".to_string(), "y".to_string());
        assert_eq!(resolve_filters(&COLS, &stale), vec![String::new(), "y".to_string()]);
    }
}
