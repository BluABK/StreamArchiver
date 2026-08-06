//! Tiling layouts for "Play all collab instances": a few built-in presets
//! (equal grid, one main + rest tiled) plus user-saved custom layouts built
//! in the drag-canvas editor (`crate::ui::layout_editor`). Persistence
//! mirrors `crate::saved_views` almost exactly — a named `Vec<Preset>` in one
//! JSON setting, name is the identity — generalized here from a grid view to
//! a window-placement preset.
//!
//! This module is pure rect math + storage; it knows nothing about players,
//! processes, or windows — see `crate::window_placement` for applying a
//! resolved [`PixelRect`] to an actual OS window.

use serde::{Deserialize, Serialize};

use crate::display::{resolve_monitor, PhysicalMonitor, PixelRect};
use crate::models::K_LAYOUT_PRESETS;
use crate::store::Store;

/// One instance's placement within a [`CustomLayout`]: which monitor, and
/// where on it as fractions of that monitor's work area (resolution-
/// independent, so a saved layout still lines up after a resolution change).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayoutSlot {
    /// Resolved leniently against a live [`crate::display::enumerate_monitors`]
    /// call — see [`crate::display::resolve_monitor`].
    pub monitor_index: usize,
    /// `(x, y, w, h)` as fractions (0.0-1.0) of the resolved monitor's
    /// `work_rect`.
    pub rect_frac: (f32, f32, f32, f32),
}

impl LayoutSlot {
    fn resolve(&self, monitors: &[PhysicalMonitor]) -> PixelRect {
        let m = resolve_monitor(monitors, self.monitor_index);
        frac_to_pixels(m.work_rect, self.rect_frac)
    }
}

/// A user-named, saved tiling layout — one [`LayoutSlot`] per instance, in
/// the order instances were requested to play. Identity is `name` (unique,
/// enforced by the caller), same "name is the key" convention as
/// [`crate::saved_views::SavedView`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CustomLayout {
    pub name: String,
    pub slots: Vec<LayoutSlot>,
}

/// Built-in quick-tile presets. Each targets exactly one monitor (the caller
/// picks which, default primary) — spanning multiple monitors is only
/// available through a saved [`CustomLayout`] built in the drag-canvas
/// editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinPreset {
    TileEqually,
    MainPlusTiledRest,
    MainPlusRow,
}

impl BuiltinPreset {
    pub const ALL: [BuiltinPreset; 3] =
        [BuiltinPreset::TileEqually, BuiltinPreset::MainPlusTiledRest, BuiltinPreset::MainPlusRow];

    pub fn label(self) -> &'static str {
        match self {
            BuiltinPreset::TileEqually => "Tile Equally",
            BuiltinPreset::MainPlusTiledRest => "Main + Tiled Rest",
            BuiltinPreset::MainPlusRow => "Main + Row",
        }
    }
}

fn frac_to_pixels(base: PixelRect, frac: (f32, f32, f32, f32)) -> PixelRect {
    PixelRect {
        x: base.x + (frac.0 * base.w as f32).round() as i32,
        y: base.y + (frac.1 * base.h as f32).round() as i32,
        w: (frac.2 * base.w as f32).round() as i32,
        h: (frac.3 * base.h as f32).round() as i32,
    }
}

/// An even grid of `n` cells inside `area` — `cols = ceil(sqrt(n))`,
/// `rows = ceil(n/cols)`; a short last row is left-aligned rather than
/// stretched to fill the width.
fn grid_cells(area: PixelRect, n: usize) -> Vec<PixelRect> {
    if n == 0 || area.w <= 0 || area.h <= 0 {
        return Vec::new();
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let cell_w = area.w / cols as i32;
    let cell_h = area.h / rows as i32;
    (0..n)
        .map(|i| {
            let (col, row) = (i % cols, i / cols);
            PixelRect {
                x: area.x + col as i32 * cell_w,
                y: area.y + row as i32 * cell_h,
                w: cell_w,
                h: cell_h,
            }
        })
        .collect()
}

/// Pure math, no I/O: resolve a built-in preset against one monitor and an
/// instance count into absolute pixel rects, one per instance, in order.
pub fn resolve_builtin(preset: BuiltinPreset, monitor: &PhysicalMonitor, n: usize) -> Vec<PixelRect> {
    let area = monitor.work_rect;
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![area];
    }
    match preset {
        BuiltinPreset::TileEqually => grid_cells(area, n),
        BuiltinPreset::MainPlusTiledRest => {
            let main_w = (area.w as f32 * 0.65).round() as i32;
            let main = PixelRect { x: area.x, y: area.y, w: main_w, h: area.h };
            let rest_area =
                PixelRect { x: area.x + main_w, y: area.y, w: area.w - main_w, h: area.h };
            let mut out = vec![main];
            out.extend(grid_cells(rest_area, n - 1));
            out
        }
        BuiltinPreset::MainPlusRow => {
            let main_h = (area.h as f32 * 0.70).round() as i32;
            let main = PixelRect { x: area.x, y: area.y, w: area.w, h: main_h };
            let rest_area =
                PixelRect { x: area.x, y: area.y + main_h, w: area.w, h: area.h - main_h };
            let mut out = vec![main];
            let rest_cols = n - 1;
            let cell_w = rest_area.w / rest_cols as i32;
            out.extend((0..rest_cols).map(|i| PixelRect {
                x: rest_area.x + i as i32 * cell_w,
                y: rest_area.y,
                w: cell_w,
                h: rest_area.h,
            }));
            out
        }
    }
}

/// Resolve a saved [`CustomLayout`] against a live monitor list and an
/// instance count. Lenient to a mismatch between `layout.slots.len()` and
/// `n` (the layout may have been saved for a different collab size): extra
/// instances beyond the saved slots are tiled with [`BuiltinPreset::TileEqually`]
/// on the primary monitor; unused saved slots are simply not applied to
/// anything.
pub fn resolve_custom(layout: &CustomLayout, monitors: &[PhysicalMonitor], n: usize) -> Vec<PixelRect> {
    let mut out: Vec<PixelRect> = layout.slots.iter().take(n).map(|s| s.resolve(monitors)).collect();
    if out.len() < n {
        let primary = crate::display::primary_or_fallback(monitors);
        let extra = n - out.len();
        out.extend(resolve_builtin(BuiltinPreset::TileEqually, &primary, extra));
    }
    out
}

/// What a "Play all collab instances" menu click asked for: one of the 3
/// built-in quick presets, a previously-saved [`CustomLayout`] by name, or a
/// one-off arrangement built in the editor and applied without saving.
#[derive(Clone, Debug)]
pub enum LayoutChoice {
    Builtin(BuiltinPreset),
    Saved(String),
    Custom(Vec<LayoutSlot>),
}

/// Resolve a menu choice to `n` absolute pixel rects, in request order.
/// Builtins target the primary monitor; a `Saved` name that no longer exists
/// (deleted between menu-open and click, a narrow race) falls back to
/// `TileEqually` on the primary monitor, same tolerance as everywhere else
/// in this module.
pub fn resolve_choice(
    choice: &LayoutChoice,
    store: &Store,
    monitors: &[PhysicalMonitor],
    n: usize,
) -> Vec<PixelRect> {
    match choice {
        LayoutChoice::Builtin(preset) => {
            resolve_builtin(*preset, &crate::display::primary_or_fallback(monitors), n)
        }
        LayoutChoice::Saved(name) => match list_layouts(store).into_iter().find(|l| &l.name == name) {
            Some(layout) => resolve_custom(&layout, monitors, n),
            None => resolve_builtin(BuiltinPreset::TileEqually, &crate::display::primary_or_fallback(monitors), n),
        },
        LayoutChoice::Custom(slots) => {
            resolve_custom(&CustomLayout { name: String::new(), slots: slots.clone() }, monitors, n)
        }
    }
}

fn all_layouts(store: &Store) -> Vec<CustomLayout> {
    store
        .get_setting(K_LAYOUT_PRESETS)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_layouts(store: &Store, layouts: &[CustomLayout]) {
    if let Ok(json) = serde_json::to_string(layouts) {
        let _ = store.set_setting(K_LAYOUT_PRESETS, &json);
    }
}

/// Every saved custom layout, in creation order.
pub fn list_layouts(store: &Store) -> Vec<CustomLayout> {
    all_layouts(store)
}

/// Insert or overwrite (by name) one layout.
pub fn upsert_layout(store: &Store, layout: CustomLayout) {
    let mut layouts = all_layouts(store);
    match layouts.iter_mut().find(|l| l.name == layout.name) {
        Some(existing) => *existing = layout,
        None => layouts.push(layout),
    }
    save_layouts(store, &layouts);
}

/// Delete a layout by name; no-op if absent.
pub fn delete_layout(store: &Store, name: &str) {
    let mut layouts = all_layouts(store);
    layouts.retain(|l| l.name != name);
    save_layouts(store, &layouts);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: i32, y: i32, w: i32, h: i32) -> PhysicalMonitor {
        PhysicalMonitor {
            index: 0,
            rect: PixelRect { x, y, w, h },
            work_rect: PixelRect { x, y, w, h },
            is_primary: true,
            name: "test".into(),
        }
    }

    fn area(rects: &[PixelRect]) -> i64 {
        rects.iter().map(|r| r.w as i64 * r.h as i64).sum()
    }

    fn overlaps(a: PixelRect, b: PixelRect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    fn assert_no_overlap(rects: &[PixelRect]) {
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(!overlaps(rects[i], rects[j]), "{:?} overlaps {:?}", rects[i], rects[j]);
            }
        }
    }

    #[test]
    fn tile_equally_covers_area_without_overlap_for_various_counts() {
        let m = monitor(0, 0, 1920, 1080);
        for n in 1..=6 {
            let rects = resolve_builtin(BuiltinPreset::TileEqually, &m, n);
            assert_eq!(rects.len(), n);
            assert_no_overlap(&rects);
            for r in &rects {
                assert!(r.x >= m.work_rect.x && r.y >= m.work_rect.y);
                assert!(r.x + r.w <= m.work_rect.x + m.work_rect.w);
                assert!(r.y + r.h <= m.work_rect.y + m.work_rect.h);
            }
        }
    }

    #[test]
    fn tile_equally_single_instance_fills_monitor() {
        let m = monitor(100, 50, 1920, 1080);
        let rects = resolve_builtin(BuiltinPreset::TileEqually, &m, 1);
        assert_eq!(rects, vec![m.work_rect]);
    }

    #[test]
    fn main_plus_tiled_rest_gives_main_the_larger_share_and_no_overlap() {
        let m = monitor(0, 0, 1920, 1080);
        for n in 2..=5 {
            let rects = resolve_builtin(BuiltinPreset::MainPlusTiledRest, &m, n);
            assert_eq!(rects.len(), n);
            assert_no_overlap(&rects);
            let main = rects[0];
            assert!(main.w as f32 > m.work_rect.w as f32 * 0.5, "main should be the bigger tile");
            assert!(main.h == m.work_rect.h, "main spans full height");
        }
    }

    #[test]
    fn main_plus_row_gives_main_the_top_and_rest_a_bottom_row() {
        let m = monitor(0, 0, 1920, 1080);
        for n in 2..=5 {
            let rects = resolve_builtin(BuiltinPreset::MainPlusRow, &m, n);
            assert_eq!(rects.len(), n);
            assert_no_overlap(&rects);
            let main = rects[0];
            assert!(main.w == m.work_rect.w, "main spans full width");
            assert!(main.h as f32 > m.work_rect.h as f32 * 0.5, "main should be the bigger tile");
            for rest in &rects[1..] {
                assert!(rest.y >= main.y + main.h, "rest sits below main");
            }
        }
    }

    #[test]
    fn resolve_custom_falls_back_to_tile_equally_for_extra_instances() {
        let monitors = vec![monitor(0, 0, 1920, 1080)];
        let layout = CustomLayout {
            name: "one-slot".into(),
            slots: vec![LayoutSlot { monitor_index: 0, rect_frac: (0.0, 0.0, 0.5, 1.0) }],
        };
        let rects = resolve_custom(&layout, &monitors, 3);
        assert_eq!(rects.len(), 3, "2 unresolved instances still get placed via fallback tiling");
        assert_no_overlap(&rects[1..]);
    }

    #[test]
    fn resolve_custom_stale_monitor_index_falls_back_to_primary() {
        let monitors = vec![monitor(0, 0, 1920, 1080)];
        let layout = CustomLayout {
            name: "stale".into(),
            slots: vec![LayoutSlot { monitor_index: 7, rect_frac: (0.0, 0.0, 1.0, 1.0) }],
        };
        let rects = resolve_custom(&layout, &monitors, 1);
        assert_eq!(rects[0], monitors[0].work_rect);
    }

    #[test]
    fn resolve_choice_saved_falls_back_to_tile_equally_when_deleted() {
        let store = Store::open_in_memory().unwrap();
        let monitors = vec![monitor(0, 0, 1920, 1080)];
        // "gone" was never saved — simulates the narrow race where a saved
        // layout is deleted between menu-open and click.
        let rects = resolve_choice(&LayoutChoice::Saved("gone".into()), &store, &monitors, 2);
        assert_eq!(rects.len(), 2);
        assert_no_overlap(&rects);
    }

    #[test]
    fn resolve_choice_saved_resolves_the_matching_layout() {
        let store = Store::open_in_memory().unwrap();
        let slot = LayoutSlot { monitor_index: 0, rect_frac: (0.0, 0.0, 0.5, 1.0) };
        upsert_layout(&store, CustomLayout { name: "Half".into(), slots: vec![slot] });
        let monitors = vec![monitor(0, 0, 1920, 1080)];
        let rects = resolve_choice(&LayoutChoice::Saved("Half".into()), &store, &monitors, 1);
        assert_eq!(rects, vec![PixelRect { x: 0, y: 0, w: 960, h: 1080 }]);
    }

    #[test]
    fn upsert_inserts_then_overwrites_by_name() {
        let store = Store::open_in_memory().unwrap();
        upsert_layout(&store, CustomLayout { name: "A".into(), slots: vec![] });
        assert_eq!(list_layouts(&store).len(), 1);
        let slot = LayoutSlot { monitor_index: 0, rect_frac: (0.0, 0.0, 1.0, 1.0) };
        upsert_layout(&store, CustomLayout { name: "A".into(), slots: vec![slot] });
        let layouts = list_layouts(&store);
        assert_eq!(layouts.len(), 1, "same name overwrites in place, doesn't duplicate");
        assert_eq!(layouts[0].slots.len(), 1);
    }

    #[test]
    fn delete_removes_by_name_leaves_others() {
        let store = Store::open_in_memory().unwrap();
        upsert_layout(&store, CustomLayout { name: "A".into(), slots: vec![] });
        upsert_layout(&store, CustomLayout { name: "B".into(), slots: vec![] });
        delete_layout(&store, "A");
        let layouts = list_layouts(&store);
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].name, "B");
    }

    #[test]
    fn area_helper_sanity() {
        // Guards the test helper itself: a 2x1 grid inside a 1000x1000
        // monitor should account for ~all the area (integer division can
        // legitimately drop a few pixels at the seam).
        let m = monitor(0, 0, 1000, 1000);
        let rects = resolve_builtin(BuiltinPreset::TileEqually, &m, 2);
        assert!(area(&rects) > 999_000);
    }
}
