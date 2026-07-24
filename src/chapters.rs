//! Embed chapter markers into a finalized recording: one per title change,
//! one per category/game change (merged into one chapter when both change
//! together within a short window), one per "real" raid (a configurable
//! minimum viewer count, so 1-2-viewer raids don't spam the list), and a
//! bracketing "Recovered segment start"/"Recovered segment end" pair (plus,
//! independently, "Muted segment start"/"Muted segment end") around any
//! lost-segment patch [`crate::downloader::gap_splice`] spliced back in —
//! useful for spot-checking a recovery fix later. All four/five kinds are
//! independently toggleable; the master on/off follows the same
//! global→channel→instance override chain as [`crate::disposal`] (`Option<bool>`
//! scope-maps in `app_settings`, no schema migration for the scope itself).
//!
//! This is **purely additive metadata** — unlike gap-splice's PTS-exact
//! splice math, a chapter landing a few seconds off its real position is a
//! minor cosmetic miss, not data loss. The timeline math below is
//! deliberately approximate (wall-clock/offset arithmetic, not a PTS
//! anchor): see [`rebase_to_final_secs`].
//!
//! Orchestration (spawning the embed job, trigger wiring) lives in
//! `downloader::chapters` since it needs `Supervisor` internals; this module
//! holds the settings/scope chain plus every pure, unit-testable piece —
//! event coalescing, timeline rebasing, and ffmetadata construction.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::store::Store;

// ---------- settings keys ----------

/// Global default: embed chapters at all (default **on**).
pub const K_CHAPTERS_ENABLED: &str = "chapters_enabled";
/// Per-channel scope-config map (`{channel_id -> ChaptersScope}`).
pub const K_CHANNEL_CHAPTERS_SCOPE: &str = "channel_chapters_scope";
/// Per-monitor scope-config map (`{monitor_id -> ChaptersScope}`).
pub const K_MONITOR_CHAPTERS_SCOPE: &str = "monitor_chapters_scope";

/// Flat, global-only toggles for which event kinds produce chapters (the
/// `remux_embed_thumbnail` shape — no per-channel/instance override, nobody
/// asked for that granularity, only for the master on/off to be scoped).
pub const K_CHAPTERS_TITLE: &str = "chapters_title_changes";
pub const K_CHAPTERS_CATEGORY: &str = "chapters_category_changes";
pub const K_CHAPTERS_RAID: &str = "chapters_raids";
pub const K_CHAPTERS_RECOVERED: &str = "chapters_recovered_segments";
pub const K_CHAPTERS_MUTED: &str = "chapters_muted_segments";
/// Minimum raid party size to get its own chapter (string-encoded i64).
pub const K_CHAPTERS_RAID_MIN_VIEWERS: &str = "chapters_raid_min_viewers";
const DEFAULT_RAID_MIN_VIEWERS: i64 = 50;

// ---------- three-level scope config (clone of HeadBackfillScope, one field) ----------

/// A channel- or monitor-level override of the chapters master toggle.
/// `None` means "inherit the level above"; `Some(true/false)` forces it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChaptersScope {
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl ChaptersScope {
    /// True when this scope overrides nothing — persisted as a removal so
    /// the map only holds real overrides.
    pub fn is_inherit(&self) -> bool {
        self.enabled.is_none()
    }
}

fn load_scope_map(store: &Store, key: &str) -> HashMap<String, ChaptersScope> {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_scope(store: &Store, key: &str, id: i64, cfg: &ChaptersScope) -> anyhow::Result<()> {
    let mut map = load_scope_map(store, key);
    if cfg.is_inherit() {
        map.remove(&id.to_string());
    } else {
        map.insert(id.to_string(), cfg.clone());
    }
    store.set_setting(key, &serde_json::to_string(&map)?)?;
    Ok(())
}

pub fn load_channel_chapters_scope(store: &Store, channel_id: i64) -> ChaptersScope {
    load_scope_map(store, K_CHANNEL_CHAPTERS_SCOPE)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

pub fn save_channel_chapters_scope(
    store: &Store,
    channel_id: i64,
    cfg: &ChaptersScope,
) -> anyhow::Result<()> {
    save_scope(store, K_CHANNEL_CHAPTERS_SCOPE, channel_id, cfg)
}

pub fn load_monitor_chapters_scope(store: &Store, monitor_id: i64) -> ChaptersScope {
    load_scope_map(store, K_MONITOR_CHAPTERS_SCOPE)
        .remove(&monitor_id.to_string())
        .unwrap_or_default()
}

pub fn save_monitor_chapters_scope(
    store: &Store,
    monitor_id: i64,
    cfg: &ChaptersScope,
) -> anyhow::Result<()> {
    save_scope(store, K_MONITOR_CHAPTERS_SCOPE, monitor_id, cfg)
}

/// Read a global boolean setting that defaults **on** — missing key ⇒ `true`,
/// anything but `"0"` ⇒ `true` (the `remux_embed_thumbnail`/`head_backfill`
/// idiom).
fn global_bool_default_true(store: &Store, key: &str) -> bool {
    store.get_setting(key).ok().flatten().is_none_or(|v| v != "0")
}

pub fn global_chapters_enabled(store: &Store) -> bool {
    global_bool_default_true(store, K_CHAPTERS_ENABLED)
}

/// Monitor override over channel override over the global default.
pub fn effective_chapters_enabled_from(
    global: bool,
    channel_scope: Option<&ChaptersScope>,
    monitor_scope: Option<&ChaptersScope>,
) -> bool {
    if let Some(v) = monitor_scope.and_then(|s| s.enabled) {
        return v;
    }
    if let Some(v) = channel_scope.and_then(|s| s.enabled) {
        return v;
    }
    global
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_chapters_enabled(store: &Store, channel_id: i64, monitor_id: i64) -> bool {
    let ch = load_channel_chapters_scope(store, channel_id);
    let mon = load_monitor_chapters_scope(store, monitor_id);
    effective_chapters_enabled_from(global_chapters_enabled(store), Some(&ch), Some(&mon))
}

// ---------- flat "which kinds" settings (Remux-section shape, global only) ----------

/// Which event kinds currently produce chapters, resolved from settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChapterKinds {
    pub title: bool,
    pub category: bool,
    pub raid: bool,
    pub recovered_segments: bool,
    pub muted_segments: bool,
    pub raid_min_viewers: i64,
}

pub fn chapter_kinds(store: &Store) -> ChapterKinds {
    ChapterKinds {
        title: global_bool_default_true(store, K_CHAPTERS_TITLE),
        category: global_bool_default_true(store, K_CHAPTERS_CATEGORY),
        raid: global_bool_default_true(store, K_CHAPTERS_RAID),
        recovered_segments: global_bool_default_true(store, K_CHAPTERS_RECOVERED),
        muted_segments: global_bool_default_true(store, K_CHAPTERS_MUTED),
        raid_min_viewers: store
            .get_setting(K_CHAPTERS_RAID_MIN_VIEWERS)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RAID_MIN_VIEWERS),
    }
}

// ---------- event collection (pure) ----------

/// How close together (in `at_secs`) a title change and a category change
/// have to land to be merged into one "{category} — {title}" chapter instead
/// of two separate ones.
const CHAPTER_COALESCE_SECS: i64 = 30;

/// Merge `stream_meta_change` rows (already loaded for one take) into
/// `(at_secs, label)` chapter events — `at_secs` relative to the take's
/// `started_at`, same frame [`rebase_to_final_secs`] expects. Baseline rows
/// (`old_value` empty — the "this is what it started as" row every take
/// gets) are dropped, matching `monitor_change_lines`' existing filter.
/// Title+category changes landing within [`CHAPTER_COALESCE_SECS`] of each
/// other merge into one chapter, preferring the more informative combined
/// label over picking just one.
pub fn coalesce_meta_events(changes: &[crate::models::StreamMetaChange]) -> Vec<(f64, String)> {
    let mut relevant: Vec<&crate::models::StreamMetaChange> = changes
        .iter()
        .filter(|c| (c.kind == "title" || c.kind == "category") && !c.old_value.is_empty())
        .collect();
    relevant.sort_by_key(|c| c.at_secs);

    let mut out = Vec::new();
    let mut i = 0;
    while i < relevant.len() {
        let group_start = relevant[i].at_secs;
        let mut title = None;
        let mut category = None;
        let mut j = i;
        while j < relevant.len() && relevant[j].at_secs - group_start <= CHAPTER_COALESCE_SECS {
            match relevant[j].kind.as_str() {
                "title" => title = Some(relevant[j].new_value.clone()),
                "category" => category = Some(relevant[j].new_value.clone()),
                _ => {}
            }
            j += 1;
        }
        let label = match (&category, &title) {
            (Some(g), Some(t)) => format!("{g} — {t}"),
            (Some(g), None) => g.clone(),
            (None, Some(t)) => t.clone(),
            (None, None) => unreachable!("group is never empty"),
        };
        out.push((group_start as f64, label));
        i = j;
    }
    out
}

/// Raid-in events for one take as `(at_secs, label)` chapter events,
/// filtered to raids with at least `min_viewers` — `stream_event.at` is
/// wall-clock, converted to the same "seconds since `started_at`" frame
/// every other event here uses.
pub fn raid_chapter_events(
    events: &[crate::models::StreamEventRow],
    started_at: i64,
    min_viewers: i64,
) -> Vec<(f64, String)> {
    events
        .iter()
        .filter(|e| e.kind == "raid_in" && e.amount >= min_viewers)
        .map(|e| {
            let at_secs = (e.at - started_at) as f64;
            (at_secs, format!("Raid: {} ({} viewers)", e.actor, e.amount))
        })
        .collect()
}

/// One recovered/spliced gap, in the two coordinate systems the chapters
/// engine needs: `local_start`/`local_end` are already final-file-relative
/// seconds — `gap_splice_job`'s own PTS-anchored output, reused as-is and
/// never re-derived here. `orig_start`/`orig_end` are the pre-splice gap
/// bounds, converted by the caller into the same "seconds since
/// `Recording.started_at`" frame [`rebase_to_final_secs`] expects for its
/// other argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplicedGap {
    pub local_start: f64,
    pub local_end: f64,
    pub orig_start: f64,
    pub orig_end: f64,
    pub muted_segs: i64,
}

/// Bracketing chapter-marker pairs for spliced gaps — independent of
/// [`rebase_to_final_secs`], since `local_start`/`local_end` are already
/// final-file-relative. `include_recovered` brackets every gap regardless of
/// mute status (for inspecting where a recovery fix landed);
/// `include_muted` additionally/independently brackets only the gaps whose
/// patch needed Twitch's muted-fallback copy — the two can coexist on the
/// same gap (both markers land at the same instant; [`merge_close_events`]
/// combines them into one chapter).
pub fn gap_marker_events(
    gaps: &[SplicedGap],
    include_recovered: bool,
    include_muted: bool,
) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    for g in gaps {
        if include_recovered {
            out.push((g.local_start, "Recovered segment start".to_string()));
            out.push((g.local_end, "Recovered segment end".to_string()));
        }
        if include_muted && g.muted_segs > 0 {
            out.push((g.local_start, "Muted segment start".to_string()));
            out.push((g.local_end, "Muted segment end".to_string()));
        }
    }
    out
}

// ---------- timeline rebasing (approximate, not PTS-exact) ----------

/// Convert `at_secs` (seconds since `Recording.started_at` — the frame
/// `stream_meta_change`/raid events are naturally in) into a position in the
/// CURRENT final file, applying two corrections:
///
/// - `head_shift_secs`: how much earlier the file's own t=0 sits relative to
///   `started_at` once head-backfill has prepended the missed intro (`0.0`
///   when no head was ever backfilled — the file's t=0 IS `started_at`).
/// - `gaps`: each completed gap-splice patch shifts every later position by
///   `(local_end - local_start) - (orig_end - orig_start)` — the patch may be
///   longer or shorter than the lost span it fills. `gaps` need not be
///   pre-sorted (sorted internally by `orig_start`).
///
/// If `at_secs` falls **inside** a gap's original `[orig_start, orig_end)`
/// (e.g. a raid landed exactly during a lost-segment window), clamps to that
/// gap's `local_start` rather than guessing a position inside content that
/// didn't originally exist.
pub fn rebase_to_final_secs(at_secs: f64, head_shift_secs: f64, gaps: &[SplicedGap]) -> f64 {
    let mut sorted: Vec<&SplicedGap> = gaps.iter().collect();
    sorted.sort_by(|a, b| a.orig_start.partial_cmp(&b.orig_start).unwrap_or(std::cmp::Ordering::Equal));

    let mut shift = head_shift_secs;
    for g in sorted {
        if at_secs >= g.orig_end {
            shift += (g.local_end - g.local_start) - (g.orig_end - g.orig_start);
        } else if at_secs > g.orig_start {
            return g.local_start.max(0.0);
        } else {
            break;
        }
    }
    (shift + at_secs).max(0.0)
}

// ---------- chapter list construction + ffmetadata ----------

/// One embedded chapter marker: starts at `at_secs`, runs until the next
/// chapter's `at_secs` (or the file's end for the last one).
#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub at_secs: f64,
    pub title: String,
}

/// Events landing within this many seconds of each other collapse into one
/// chapter — e.g. a gap-splice patch that's both "recovered" and "muted"
/// emits start markers at the identical instant when both kinds are enabled.
const CHAPTER_MERGE_EPSILON_SECS: f64 = 0.5;

/// Sort `events` and merge near-duplicates (within
/// [`CHAPTER_MERGE_EPSILON_SECS`]) into single chapters with a combined
/// title, so two markers that land at the same instant don't produce a
/// zero-length chapter.
pub fn merge_close_events(mut events: Vec<(f64, String)>) -> Vec<Chapter> {
    events.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<Chapter> = Vec::new();
    for (at_secs, title) in events {
        if let Some(last) = out.last_mut()
            && (at_secs - last.at_secs).abs() <= CHAPTER_MERGE_EPSILON_SECS
        {
            last.title = format!("{} / {}", last.title, title);
            continue;
        }
        out.push(Chapter { at_secs: at_secs.max(0.0), title });
    }
    out
}

/// Backslash-escape ffmetadata's special characters (`=`, `;`, `#`, `\`,
/// newline) per the format's own escaping rule.
fn escape_ffmetadata(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build a `;FFMETADATA1` chapters file body from an already-sorted (by
/// [`merge_close_events`]) chapter list. Each chapter's `END` is the next
/// chapter's `START`, or `total_duration_secs` for the last one (a rough
/// `+1s` guess when the duration isn't known — better than a zero-length
/// final chapter).
pub fn build_ffmetadata(chapters: &[Chapter], total_duration_secs: Option<f64>) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    for (i, c) in chapters.iter().enumerate() {
        let start_ms = (c.at_secs.max(0.0) * 1000.0).round() as i64;
        let next_start_ms = chapters.get(i + 1).map(|n| (n.at_secs.max(0.0) * 1000.0).round() as i64);
        let end_ms = next_start_ms
            .or_else(|| total_duration_secs.map(|d| (d * 1000.0).round() as i64))
            .unwrap_or(start_ms + 1000)
            .max(start_ms + 1);
        out.push_str("[CHAPTER]\n");
        out.push_str("TIMEBASE=1/1000\n");
        out.push_str(&format!("START={start_ms}\n"));
        out.push_str(&format!("END={end_ms}\n"));
        out.push_str(&format!("title={}\n", escape_ffmetadata(&c.title)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{StreamEventRow, StreamMetaChange};

    fn meta(at_secs: i64, kind: &str, old: &str, new: &str) -> StreamMetaChange {
        StreamMetaChange {
            id: 0,
            recording_id: 1,
            at_secs,
            kind: kind.into(),
            old_value: old.into(),
            new_value: new.into(),
        }
    }

    #[test]
    fn scope_precedence_monitor_over_channel_over_global() {
        let ch = ChaptersScope { enabled: Some(false) };
        let mon = ChaptersScope { enabled: Some(true) };
        assert!(effective_chapters_enabled_from(false, Some(&ch), Some(&mon)));
        assert!(!effective_chapters_enabled_from(true, Some(&ch), Some(&ChaptersScope::default())));
        assert!(effective_chapters_enabled_from(true, None, None));
        assert!(ChaptersScope::default().is_inherit());
        assert!(!ch.is_inherit());
    }

    #[test]
    fn global_default_is_on() {
        let store = Store::open_in_memory().unwrap();
        assert!(global_chapters_enabled(&store));
        assert!(effective_chapters_enabled(&store, 1, 1));
        store.set_setting(K_CHAPTERS_ENABLED, "0").unwrap();
        assert!(!global_chapters_enabled(&store));
    }

    #[test]
    fn chapter_kinds_default_all_on_with_raid_threshold_50() {
        let store = Store::open_in_memory().unwrap();
        let k = chapter_kinds(&store);
        assert!(k.title && k.category && k.raid && k.recovered_segments && k.muted_segments);
        assert_eq!(k.raid_min_viewers, 50);
    }

    #[test]
    fn coalesce_merges_title_and_category_within_window() {
        let changes = vec![
            meta(100, "category", "Just Chatting", "Elden Ring"),
            meta(105, "title", "afk", "let's ring"),
        ];
        let events = coalesce_meta_events(&changes);
        assert_eq!(events, vec![(100.0, "Elden Ring — let's ring".to_string())]);
    }

    #[test]
    fn coalesce_keeps_title_only_and_category_only_separate_when_far_apart() {
        let changes = vec![
            meta(0, "category", "", "Just Chatting"), // baseline row, dropped
            meta(100, "category", "Just Chatting", "Elden Ring"),
            meta(500, "title", "let's ring", "goodnight"),
        ];
        let events = coalesce_meta_events(&changes);
        assert_eq!(
            events,
            vec![(100.0, "Elden Ring".to_string()), (500.0, "goodnight".to_string())]
        );
    }

    #[test]
    fn raid_filters_by_threshold_and_converts_to_take_relative() {
        let events = vec![
            StreamEventRow {
                id: 1,
                monitor_id: 1,
                at: 1_000_150,
                stream_id: String::new(),
                kind: "raid_in".into(),
                actor: "big_streamer".into(),
                target: String::new(),
                amount: 200,
                tier: String::new(),
                detail: String::new(),
            },
            StreamEventRow {
                id: 2,
                monitor_id: 1,
                at: 1_000_200,
                stream_id: String::new(),
                kind: "raid_in".into(),
                actor: "tiny_raider".into(),
                target: String::new(),
                amount: 2,
                tier: String::new(),
                detail: String::new(),
            },
            StreamEventRow {
                id: 3,
                monitor_id: 1,
                at: 1_000_300,
                stream_id: String::new(),
                kind: "raid_out".into(),
                actor: String::new(),
                target: "other".into(),
                amount: 200,
                tier: String::new(),
                detail: String::new(),
            },
        ];
        let out = raid_chapter_events(&events, 1_000_000, 50);
        assert_eq!(out, vec![(150.0, "Raid: big_streamer (200 viewers)".to_string())]);
    }

    #[test]
    fn rebase_no_gaps_is_pure_head_shift() {
        assert_eq!(rebase_to_final_secs(50.0, 0.0, &[]), 50.0);
        assert_eq!(rebase_to_final_secs(50.0, 120.0, &[]), 170.0);
    }

    #[test]
    fn rebase_single_gap_before_and_after() {
        // Patch is 5s longer than the original 2s gap it fills.
        let gap = SplicedGap { local_start: 100.0, local_end: 105.0, orig_start: 100.0, orig_end: 102.0, muted_segs: 0 };
        // Before the gap: untouched.
        assert_eq!(rebase_to_final_secs(50.0, 0.0, &[gap]), 50.0);
        // After the gap: shifted by the +3s the patch added.
        assert_eq!(rebase_to_final_secs(200.0, 0.0, &[gap]), 203.0);
    }

    #[test]
    fn rebase_landing_inside_a_gap_clamps_to_patch_start() {
        let gap = SplicedGap { local_start: 100.0, local_end: 105.0, orig_start: 100.0, orig_end: 102.0, muted_segs: 0 };
        assert_eq!(rebase_to_final_secs(101.0, 0.0, &[gap]), 100.0);
    }

    #[test]
    fn rebase_multiple_gaps_accumulate_and_order_independent_input() {
        let g1 = SplicedGap { local_start: 100.0, local_end: 103.0, orig_start: 100.0, orig_end: 102.0, muted_segs: 0 }; // +1s
        let g2 = SplicedGap { local_start: 300.0, local_end: 306.0, orig_start: 297.0, orig_end: 300.0, muted_segs: 1 }; // +3s
        // Pass in reverse order — function sorts internally.
        let gaps = [g2, g1];
        assert_eq!(rebase_to_final_secs(50.0, 0.0, &gaps), 50.0); // before both
        assert_eq!(rebase_to_final_secs(200.0, 0.0, &gaps), 201.0); // after g1 only: +1
        assert_eq!(rebase_to_final_secs(400.0, 0.0, &gaps), 404.0); // after both: +1+3
    }

    #[test]
    fn rebase_zero_head_shift_with_no_head_backfill() {
        // No head backfill (head_shift=0) vs. a completed one (head_shift=J).
        assert_eq!(rebase_to_final_secs(10.0, 0.0, &[]), 10.0);
        assert_eq!(rebase_to_final_secs(10.0, 45.0, &[]), 55.0);
    }

    #[test]
    fn gap_markers_recovered_covers_all_muted_covers_only_muted() {
        let clean = SplicedGap { local_start: 10.0, local_end: 12.0, orig_start: 10.0, orig_end: 11.0, muted_segs: 0 };
        let muted = SplicedGap { local_start: 50.0, local_end: 53.0, orig_start: 49.0, orig_end: 51.0, muted_segs: 2 };
        let gaps = [clean, muted];

        let recovered_only = gap_marker_events(&gaps, true, false);
        assert_eq!(
            recovered_only,
            vec![
                (10.0, "Recovered segment start".to_string()),
                (12.0, "Recovered segment end".to_string()),
                (50.0, "Recovered segment start".to_string()),
                (53.0, "Recovered segment end".to_string()),
            ]
        );

        let muted_only = gap_marker_events(&gaps, false, true);
        assert_eq!(
            muted_only,
            vec![
                (50.0, "Muted segment start".to_string()),
                (53.0, "Muted segment end".to_string()),
            ]
        );

        assert!(gap_marker_events(&gaps, false, false).is_empty());
    }

    #[test]
    fn merge_close_events_combines_coincident_markers_keeps_distant_ones_separate() {
        let events = vec![
            (50.0, "Recovered segment start".to_string()),
            (50.0, "Muted segment start".to_string()),
            (200.0, "goodnight".to_string()),
        ];
        let chapters = merge_close_events(events);
        assert_eq!(
            chapters,
            vec![
                Chapter { at_secs: 50.0, title: "Recovered segment start / Muted segment start".to_string() },
                Chapter { at_secs: 200.0, title: "goodnight".to_string() },
            ]
        );
    }

    #[test]
    fn merge_close_events_sorts_unordered_input() {
        let events = vec![(200.0, "b".to_string()), (10.0, "a".to_string())];
        let chapters = merge_close_events(events);
        assert_eq!(chapters[0].at_secs, 10.0);
        assert_eq!(chapters[1].at_secs, 200.0);
    }

    #[test]
    fn ffmetadata_last_chapter_uses_total_duration() {
        let chapters = vec![
            Chapter { at_secs: 0.0, title: "start".to_string() },
            Chapter { at_secs: 100.0, title: "second".to_string() },
        ];
        let text = build_ffmetadata(&chapters, Some(150.0));
        assert_eq!(
            text,
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=100000\ntitle=start\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=100000\nEND=150000\ntitle=second\n"
        );
    }

    #[test]
    fn ffmetadata_unknown_duration_falls_back_to_plus_one_second() {
        let chapters = vec![Chapter { at_secs: 10.0, title: "only".to_string() }];
        let text = build_ffmetadata(&chapters, None);
        assert!(text.contains("START=10000\nEND=11000\n"));
    }

    #[test]
    fn ffmetadata_escapes_special_characters_in_title() {
        let chapters = vec![Chapter { at_secs: 0.0, title: "a=b; c#d\\e".to_string() }];
        let text = build_ffmetadata(&chapters, Some(5.0));
        assert!(text.contains("title=a\\=b\\; c\\#d\\\\e\n"));
    }
}
