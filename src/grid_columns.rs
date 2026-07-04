//! Persisted, user-customizable grid columns: visibility, order, and sort for
//! every data table (Streams, Videos, Background Active/Recent, Processes,
//! Issues). Column widths are deliberately NOT covered here — egui_extras's
//! width cache has no `serde` feature enabled in this build, so it's already
//! volatile (session-only) for every table today; there is nothing to persist
//! or reset for it beyond the existing per-table "⇔ Fit columns" mechanism.
//!
//! Mirrors [`crate::schedule_source`]'s `SourceEntry` / `load_source_order` /
//! `save_source_order` / `merge_order` pattern almost exactly, generalized
//! from one ordered list to one-per-table, keyed by [`GridTableId`]. The one
//! deliberate divergence: a newly-added *column* id defaults to **visible**
//! (unlike a newly-added schedule *source*, which defaults disabled for ToS
//! risk) — a silently vanished column reads as a bug, not a safety feature.

use std::collections::HashMap;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::models::{K_GRID_COLUMNS, K_GRID_SORT};
use crate::store::Store;

/// One column descriptor: a stable `id` (never reused/changed once shipped —
/// it's the persistence key), display title/tooltip, sizing, and whether it
/// takes part in sort/filter. Generalizes the old per-table `StreamCol`.
pub struct GridCol {
    pub id: &'static str,
    pub title: &'static str,
    pub tooltip: &'static str,
    pub min_width: f32,
    /// Starting (and clipped) width for content-capped columns whose value can
    /// be long. `0.0` = auto-size to content.
    pub initial: f32,
    pub sortable: bool,
    /// Use `Column::remainder()` (fills leftover width) instead of auto/initial
    /// — the trailing column in a few tables (Videos Actions, Background
    /// Detail, Processes Name, Issues File).
    pub stretch: bool,
}

/// One entry in a table's persisted column list: a stable id + visibility.
/// Vec order is display order (mirrors `SourceEntry`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnEntry {
    pub id: String,
    pub visible: bool,
}

/// A table's persisted sort, by stable column id rather than runtime index, so
/// it survives a `GridCol` array being reordered/extended across builds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PersistedSort {
    pub col: Option<String>,
    pub ascending: bool,
}

/// Which of the six grid tables an operation applies to; also the JSON-map key
/// (`key()`) and the `TableBuilder::id_salt` for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GridTableId {
    Streams,
    Videos,
    BgActive,
    BgRecent,
    Processes,
    Issues,
}

impl GridTableId {
    pub const ALL: [GridTableId; 6] = [
        GridTableId::Streams,
        GridTableId::Videos,
        GridTableId::BgActive,
        GridTableId::BgRecent,
        GridTableId::Processes,
        GridTableId::Issues,
    ];

    /// Stable string key: used as the map key inside `K_GRID_COLUMNS` /
    /// `K_GRID_SORT` and as the table's `id_salt` — keep both usages in sync.
    pub fn key(self) -> &'static str {
        match self {
            GridTableId::Streams => "streams_table",
            GridTableId::Videos => "videos_table",
            GridTableId::BgActive => "bg_active_table",
            GridTableId::BgRecent => "bg_recent_table",
            GridTableId::Processes => "processes_table",
            GridTableId::Issues => "issues_table",
        }
    }
}

/// Per-table runtime state: the persisted column entries plus the previous
/// frame's effective order. `note_order` detects a pure reorder (column count
/// unchanged) so the caller can force a `TableBuilder` width-cache reset —
/// egui_extras's cache is keyed by column *position*, so a reorder with no
/// count change would otherwise silently reapply a stale width to whatever
/// column now sits in that slot.
pub struct GridState {
    pub entries: Vec<ColumnEntry>,
    last_order: Vec<usize>,
}

impl GridState {
    pub fn load(store: &Store, table: GridTableId, columns: &[GridCol]) -> Self {
        GridState {
            entries: load_columns(store, table, columns),
            last_order: Vec::new(),
        }
    }

    /// Call once per frame with this frame's freshly-computed `effective_order()`
    /// result. Returns true when the order differs from last frame's (including
    /// the very first call) — the caller should force a `tb.reset()` when true.
    pub fn note_order(&mut self, order: &[usize]) -> bool {
        let changed = self.last_order != order;
        if changed {
            self.last_order = order.to_vec();
        }
        changed
    }
}

/// The default column list for a descriptor array: declaration order, all visible.
fn default_columns(columns: &[GridCol]) -> Vec<ColumnEntry> {
    columns
        .iter()
        .map(|c| ColumnEntry { id: c.id.to_string(), visible: true })
        .collect()
}

/// Normalize a persisted entry list against the current descriptor array: drop
/// ids no longer present (a stale entry from a newer/older build) and append
/// any descriptor ids missing from the list at the end (visible).
fn merge_columns(columns: &[GridCol], mut entries: Vec<ColumnEntry>) -> Vec<ColumnEntry> {
    entries.retain(|e| columns.iter().any(|c| c.id == e.id));
    for c in columns {
        if !entries.iter().any(|e| e.id == c.id) {
            entries.push(ColumnEntry { id: c.id.to_string(), visible: true });
        }
    }
    entries
}

fn all_columns_map(store: &Store) -> HashMap<String, Vec<ColumnEntry>> {
    store
        .get_setting(K_GRID_COLUMNS)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_columns_map(store: &Store, map: &HashMap<String, Vec<ColumnEntry>>) {
    if let Ok(json) = serde_json::to_string(map) {
        let _ = store.set_setting(K_GRID_COLUMNS, &json);
    }
}

/// Load a table's persisted column order/visibility, merged against `columns`
/// (its current descriptor array) so unknown ids are dropped and new ones are
/// appended (visible). Falls back to declaration order (all visible) when
/// nothing is persisted yet.
pub fn load_columns(store: &Store, table: GridTableId, columns: &[GridCol]) -> Vec<ColumnEntry> {
    match all_columns_map(store).remove(table.key()) {
        Some(entries) => merge_columns(columns, entries),
        None => default_columns(columns),
    }
}

/// Persist one table's column order/visibility (read-modify-write the shared map).
pub fn save_columns(store: &Store, table: GridTableId, entries: &[ColumnEntry]) {
    let mut map = all_columns_map(store);
    map.insert(table.key().to_string(), entries.to_vec());
    save_columns_map(store, &map);
}

fn all_sort_map(store: &Store) -> HashMap<String, PersistedSort> {
    store
        .get_setting(K_GRID_SORT)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_sort_map(store: &Store, map: &HashMap<String, PersistedSort>) {
    if let Ok(json) = serde_json::to_string(map) {
        let _ = store.set_setting(K_GRID_SORT, &json);
    }
}

/// Load a table's persisted sort state (default/unsorted when nothing saved).
pub fn load_sort(store: &Store, table: GridTableId) -> PersistedSort {
    all_sort_map(store).remove(table.key()).unwrap_or_default()
}

/// Persist one table's sort state.
pub fn save_sort(store: &Store, table: GridTableId, sort: &PersistedSort) {
    let mut map = all_sort_map(store);
    map.insert(table.key().to_string(), sort.clone());
    save_sort_map(store, &map);
}

/// `PersistedSort` (id-based) -> `(index, ascending)` against `columns`, for
/// feeding into a table's runtime `SortState`. `None` index when nothing is
/// persisted or the saved id no longer resolves (same as today's "unsorted"
/// default) — the caller assembles its own `SortState { col, ascending }`.
pub fn resolve_sort(columns: &[GridCol], persisted: &PersistedSort) -> (Option<usize>, bool) {
    let idx = persisted
        .col
        .as_ref()
        .and_then(|id| columns.iter().position(|c| c.id == id.as_str()));
    (idx, persisted.ascending)
}

/// `(index, ascending)` -> `PersistedSort` (id-based), for saving a table's
/// runtime `SortState` back.
pub fn unresolve_sort(columns: &[GridCol], col: Option<usize>, ascending: bool) -> PersistedSort {
    PersistedSort {
        col: col.and_then(|i| columns.get(i)).map(|c| c.id.to_string()),
        ascending,
    }
}

/// The ordered, visibility-filtered static indices to render this frame — the
/// ONE function the column-builder loop, header loop, and every row shape's
/// dispatch loop must all call, so they can never drift apart. `extra_gate`
/// folds in a table-specific visibility override (e.g. Streams/Videos' existing
/// `show_actions` toggle for the `"actions"` id) so there's one source of
/// truth, not a second independent hide flag.
pub fn effective_order(
    columns: &[GridCol],
    entries: &[ColumnEntry],
    extra_gate: impl Fn(&str) -> bool,
) -> Vec<usize> {
    entries
        .iter()
        .filter(|e| e.visible && extra_gate(&e.id))
        .filter_map(|e| columns.iter().position(|c| c.id == e.id))
        .collect()
}

/// Hide/show one column in place by id. No-op if the id isn't present.
pub fn set_visible(entries: &mut [ColumnEntry], id: &str, visible: bool) {
    if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
        e.visible = visible;
    }
}

/// Column-chooser body: one row per persisted entry (checkbox for visibility +
/// ▲/▼ to reorder), adapted from `source_list_inline_editor` (the ui.rs
/// schedule-source priority editor) for grid columns. `locked` marks ids whose
/// visibility is controlled elsewhere (the Streams/Videos "actions" id, gated
/// by the existing Show Actions setting) — these get a disabled, always-on
/// checkbox instead of an independent hide toggle, so there's one source of
/// truth for their visibility. Returns true if `entries` changed (caller is
/// responsible for persisting).
pub fn column_chooser_editor(
    ui: &mut egui::Ui,
    entries: &mut [ColumnEntry],
    columns: &[GridCol],
    locked: impl Fn(&str) -> bool,
) -> bool {
    let mut changed = false;
    let mut move_up: Option<usize> = None;
    let mut move_down: Option<usize> = None;
    let n = entries.len();
    for (i, entry) in entries.iter_mut().enumerate() {
        let Some(col) = columns.iter().find(|c| c.id == entry.id) else {
            continue;
        };
        let is_locked = locked(&entry.id);
        ui.horizontal(|ui| {
            if is_locked {
                let mut always_on = true;
                ui.add_enabled(false, egui::Checkbox::new(&mut always_on, ""))
                    .on_hover_text("Visibility controlled by Settings → Display");
            } else if ui.checkbox(&mut entry.visible, "").changed() {
                changed = true;
            }
            if ui
                .add_enabled(i > 0, egui::Button::new("▲").small())
                .on_hover_text("Move up")
                .clicked()
            {
                move_up = Some(i);
            }
            if ui
                .add_enabled(i + 1 < n, egui::Button::new("▼").small())
                .on_hover_text("Move down")
                .clicked()
            {
                move_down = Some(i);
            }
            let mut label = egui::RichText::new(col.title);
            if !entry.visible {
                label = label.weak();
            }
            let resp = ui.label(label);
            if !col.tooltip.is_empty() {
                resp.on_hover_text(col.tooltip);
            }
        });
    }
    if let Some(i) = move_up {
        entries.swap(i, i - 1);
        changed = true;
    }
    if let Some(i) = move_down {
        entries.swap(i, i + 1);
        changed = true;
    }
    changed
}

/// "Reset all columns": every listed table's entries -> default order, all
/// visible. Leaves any table NOT included in `tables` untouched.
pub fn reset_all_columns(store: &Store, tables: &[(GridTableId, &[GridCol])]) {
    let mut map = all_columns_map(store);
    for &(table, columns) in tables {
        map.insert(table.key().to_string(), default_columns(columns));
    }
    save_columns_map(store, &map);
}

/// "Reset column sort": every listed table's persisted sort -> unsorted.
pub fn reset_all_sort(store: &Store, tables: &[GridTableId]) {
    let mut map = all_sort_map(store);
    for &table in tables {
        map.insert(table.key().to_string(), PersistedSort::default());
    }
    save_sort_map(store, &map);
}

/// "Reset all column positions": every listed table's entries -> default
/// *order* only, preserving each id's current `visible` flag (unlike
/// `reset_all_columns`, which also clears hide/show choices).
pub fn reset_all_positions(store: &Store, tables: &[(GridTableId, &[GridCol])]) {
    let mut map = all_columns_map(store);
    for &(table, columns) in tables {
        let currently_visible = |id: &str| -> bool {
            map.get(table.key())
                .and_then(|entries| entries.iter().find(|e| e.id == id))
                .map(|e| e.visible)
                .unwrap_or(true)
        };
        let reset: Vec<ColumnEntry> = columns
            .iter()
            .map(|c| ColumnEntry {
                id: c.id.to_string(),
                visible: currently_visible(c.id),
            })
            .collect();
        map.insert(table.key().to_string(), reset);
    }
    save_columns_map(store, &map);
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: [GridCol; 3] = [
        GridCol { id: "a", title: "A", tooltip: "", min_width: 10.0, initial: 0.0, sortable: true, stretch: false },
        GridCol { id: "b", title: "B", tooltip: "", min_width: 10.0, initial: 0.0, sortable: true, stretch: false },
        GridCol { id: "c", title: "C", tooltip: "", min_width: 10.0, initial: 0.0, sortable: false, stretch: false },
    ];

    #[test]
    fn load_defaults_to_declaration_order_all_visible() {
        let store = Store::open_in_memory().unwrap();
        let entries = load_columns(&store, GridTableId::Streams, &COLS);
        assert_eq!(entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(entries.iter().all(|e| e.visible));
    }

    #[test]
    fn merge_drops_unknown_and_appends_missing_visible() {
        let store = Store::open_in_memory().unwrap();
        let mut map = HashMap::new();
        map.insert(
            GridTableId::Streams.key().to_string(),
            vec![
                ColumnEntry { id: "c".into(), visible: false },
                ColumnEntry { id: "bogus".into(), visible: true },
            ],
        );
        save_columns_map(&store, &map);
        let entries = load_columns(&store, GridTableId::Streams, &COLS);
        // Unknown id dropped; "c" kept (order + visibility preserved); "a"/"b"
        // (missing from the saved list) appended at the end, visible.
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "c");
        assert!(!entries[0].visible);
        assert!(entries.iter().find(|e| e.id == "a").unwrap().visible);
        assert!(entries.iter().find(|e| e.id == "b").unwrap().visible);
    }

    #[test]
    fn save_load_roundtrip_is_per_table() {
        let store = Store::open_in_memory().unwrap();
        let mut streams_entries = load_columns(&store, GridTableId::Streams, &COLS);
        streams_entries.swap(0, 2);
        set_visible(&mut streams_entries, "b", false);
        save_columns(&store, GridTableId::Streams, &streams_entries);

        let reloaded = load_columns(&store, GridTableId::Streams, &COLS);
        assert_eq!(reloaded[0].id, "c");
        assert!(!reloaded.iter().find(|e| e.id == "b").unwrap().visible);
        // A different table is unaffected.
        let videos_entries = load_columns(&store, GridTableId::Videos, &COLS);
        assert_eq!(videos_entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[test]
    fn effective_order_filters_hidden_and_gated() {
        let entries = vec![
            ColumnEntry { id: "a".into(), visible: true },
            ColumnEntry { id: "b".into(), visible: false },
            ColumnEntry { id: "c".into(), visible: true },
        ];
        // "c" additionally gated off by extra_gate (e.g. show_actions == false).
        let order = effective_order(&COLS, &entries, |id| id != "c");
        assert_eq!(order, vec![0]); // only "a" survives both gates
    }

    #[test]
    fn sort_resolve_unresolve_roundtrip() {
        let persisted = PersistedSort { col: Some("b".into()), ascending: false };
        let (idx, asc) = resolve_sort(&COLS, &persisted);
        assert_eq!(idx, Some(1));
        assert!(!asc);
        let back = unresolve_sort(&COLS, idx, asc);
        assert_eq!(back, persisted);
        // An unknown/stale id resolves to "unsorted".
        let stale = PersistedSort { col: Some("zzz".into()), ascending: true };
        assert_eq!(resolve_sort(&COLS, &stale).0, None);
    }

    #[test]
    fn sort_save_load_roundtrip_and_reset() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(load_sort(&store, GridTableId::Issues), PersistedSort::default());
        let s = PersistedSort { col: Some("a".into()), ascending: true };
        save_sort(&store, GridTableId::Issues, &s);
        assert_eq!(load_sort(&store, GridTableId::Issues), s);
        reset_all_sort(&store, &[GridTableId::Issues]);
        assert_eq!(load_sort(&store, GridTableId::Issues), PersistedSort::default());
    }

    #[test]
    fn reset_all_columns_restores_default_order_and_visibility() {
        let store = Store::open_in_memory().unwrap();
        let mut entries = load_columns(&store, GridTableId::Streams, &COLS);
        entries.swap(0, 2);
        set_visible(&mut entries, "a", false);
        save_columns(&store, GridTableId::Streams, &entries);

        reset_all_columns(&store, &[(GridTableId::Streams, &COLS)]);
        let reset = load_columns(&store, GridTableId::Streams, &COLS);
        assert_eq!(reset.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(reset.iter().all(|e| e.visible));
    }

    #[test]
    fn reset_all_positions_keeps_visibility_but_not_order() {
        let store = Store::open_in_memory().unwrap();
        let mut entries = load_columns(&store, GridTableId::Streams, &COLS);
        entries.swap(0, 2);
        set_visible(&mut entries, "a", false);
        save_columns(&store, GridTableId::Streams, &entries);

        reset_all_positions(&store, &[(GridTableId::Streams, &COLS)]);
        let reset = load_columns(&store, GridTableId::Streams, &COLS);
        assert_eq!(reset.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(!reset.iter().find(|e| e.id == "a").unwrap().visible);
        assert!(reset.iter().find(|e| e.id == "b").unwrap().visible);
    }
}
