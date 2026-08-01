//! One-time historical import for the Trash view (`disposal_record`):
//! reconstructs best-effort entries for disposals that happened before the
//! Trash view existed (schema < v73), from whatever DB traces and naming
//! conventions survive them. See `Store::gap_splice_patch_candidates` /
//! `vod_replace_candidates` / `post_join_head_disposal_candidates` for what
//! evidence each disposal kind leaves behind.
//!
//! Deliberately NOT attempted: "superseded old head" (a newer take's fresh
//! head displacing an older take's). Nothing distinguishes "this recording's
//! head was superseded" from "this recording never had a head backfilled at
//! all" — the vast majority of recordings — so guessing here would flood the
//! view with false positives rather than a few honest low-confidence rows.
//!
//! Every imported row is `DisposalRecordState::Permanent` with
//! `DisposalMethod::Unknown` and no real timestamp (a proxy: the take's
//! `ended_at`/`started_at`) — we don't know the method, the exact time, or
//! (for a "guess"-tier row) even the exact path with certainty, so no
//! Restore/Permanently-delete action is offered on these; they're read-only
//! history. A candidate is only imported after confirming its file no longer
//! exists on disk AND its drive is currently reachable (an unreachable drive
//! would make every file on it look "gone" whether it actually is or not).

use std::path::{Path, PathBuf};

use crate::disposal::{DisposalConfidence, DisposalMethod, DisposalRecordRow, DisposalRecordState};
use crate::iomon::Cat;
use crate::store::Store;

/// The deterministic rename `vod::` gives a displaced live capture before
/// disposing it (`vod.rs`: `let backup = live.with_extension("pre-vod.bak");`).
pub fn vod_backup_path(output_path: &Path) -> PathBuf {
    output_path.with_extension("pre-vod.bak")
}

/// `full_path`'s stem with its trailing `.full` stripped — `"X"` from
/// `"X.full.mkv"`. `None` if `full_path` doesn't have the expected shape
/// (defensive; every `full_path` the app itself writes does).
fn original_stem(full_path: &Path) -> Option<String> {
    full_path.file_stem()?.to_str()?.strip_suffix(".full").map(str::to_string)
}

/// `{stem}.head.mkv` naming guess for the head file a post-join cleanup
/// disposed — same stem `backfill.rs` used to create both `{stem}.head.mkv`
/// and `{stem}.full.mkv` in the first place.
pub fn head_guess_path(full_path: &Path) -> Option<PathBuf> {
    Some(full_path.with_file_name(format!("{}.head.mkv", original_stem(full_path)?)))
}

/// `{stem}.mkv` naming guess for the live-capture file a "Both"-mode
/// post-join cleanup disposed (only relevant when `output_path == full_path`
/// — the take's pointer was re-pointed at the full file).
pub fn live_capture_guess_path(full_path: &Path) -> Option<PathBuf> {
    Some(full_path.with_file_name(format!("{}.mkv", original_stem(full_path)?)))
}

/// Tally of what one `run_historical_backfill` pass did — shown to the user
/// so a sparse result reads as "nothing more to find" rather than "broken".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillReport {
    pub imported_exact: usize,
    pub imported_guess: usize,
    /// The candidate's path still exists — nothing to import (either it was
    /// never actually disposed, e.g. cleanup is set to Keep, or it's a
    /// Trash-method disposal still sitting in its trash folder — which this
    /// scan can't identify as such without knowing where every trash root
    /// ever configured was, so it's conservatively left alone).
    pub skipped_still_exists: usize,
    /// The candidate's drive isn't currently reachable — importing would
    /// risk mistaking "drive unplugged" for "file disposed".
    pub skipped_drive_unreachable: usize,
    /// Already logged (a live entry, or a previous backfill run) — reruns
    /// are idempotent.
    pub skipped_already_imported: usize,
}

impl BackfillReport {
    pub fn imported_total(&self) -> usize {
        self.imported_exact + self.imported_guess
    }

    /// One-line status-bar summary.
    pub fn summarize(&self) -> String {
        if self.imported_total() == 0 {
            return "Historical import: nothing new to add.".to_string();
        }
        format!(
            "Historical import: added {} ({} exact, {} inferred); skipped {} still-present, \
             {} on an unreachable drive, {} already logged.",
            self.imported_total(),
            self.imported_exact,
            self.imported_guess,
            self.skipped_still_exists,
            self.skipped_drive_unreachable,
            self.skipped_already_imported
        )
    }
}

/// Runs the whole scan. Synchronous and blocking (many small `stat` calls on
/// top of the DB reads) — callers on the UI thread must run this inside
/// `tokio::task::spawn_blocking`, same convention as any other bulk `Store`
/// access documented in `store.rs`'s module doc.
pub fn run_historical_backfill(store: &Store) -> BackfillReport {
    let existing = store.disposal_record_keys().unwrap_or_default();
    let mut report = BackfillReport::default();

    for (rec_id, patch, proxy_at) in store.gap_splice_patch_candidates().unwrap_or_default() {
        import_candidate(
            store,
            &mut report,
            &existing,
            rec_id,
            "gap splice cleanup: consumed patch",
            DisposalConfidence::HistoricalExact,
            Path::new(&patch.out_path),
            proxy_at,
        );
    }

    for (rec_id, output_path, proxy_at) in store.vod_replace_candidates().unwrap_or_default() {
        let path = vod_backup_path(Path::new(&output_path));
        import_candidate(
            store,
            &mut report,
            &existing,
            rec_id,
            "VOD replace: displaced live capture",
            DisposalConfidence::HistoricalExact,
            &path,
            proxy_at,
        );
    }

    for (rec_id, full_path, output_path, proxy_at) in
        store.post_join_head_disposal_candidates().unwrap_or_default()
    {
        let full_p = Path::new(&full_path);
        if let Some(head_path) = head_guess_path(full_p) {
            import_candidate(
                store,
                &mut report,
                &existing,
                rec_id,
                "post-join cleanup: head",
                DisposalConfidence::HistoricalGuess,
                &head_path,
                proxy_at,
            );
        }
        if output_path == full_path
            && let Some(live_path) = live_capture_guess_path(full_p)
        {
            import_candidate(
                store,
                &mut report,
                &existing,
                rec_id,
                "post-join cleanup: live capture",
                DisposalConfidence::HistoricalGuess,
                &live_path,
                proxy_at,
            );
        }
    }

    report
}

#[allow(clippy::too_many_arguments)]
fn import_candidate(
    store: &Store,
    report: &mut BackfillReport,
    existing: &std::collections::HashSet<(i64, String)>,
    rec_id: i64,
    reason: &str,
    confidence: DisposalConfidence,
    path: &Path,
    proxy_at: i64,
) {
    if existing.contains(&(rec_id, reason.to_string())) {
        report.skipped_already_imported += 1;
        return;
    }
    let Some(drive) = crate::downloader::drive_of(path) else {
        return;
    };
    if !crate::iomon::fs::exists_sync(Cat::CacheSweep, PathBuf::from(format!("{drive}:\\"))) {
        report.skipped_drive_unreachable += 1;
        return;
    }
    if crate::iomon::fs::exists_sync(Cat::CacheSweep, path) {
        report.skipped_still_exists += 1;
        return;
    }
    let row = DisposalRecordRow {
        id: 0,
        rec_id,
        reason: reason.to_string(),
        method: DisposalMethod::Unknown,
        original_path: path.to_string_lossy().into_owned(),
        trash_path: String::new(),
        state: DisposalRecordState::Permanent,
        disposed_at: proxy_at,
        updated_at: proxy_at,
        confidence,
        // Unknowable in principle: this scan only ever imports a candidate
        // whose file is already confirmed gone from disk (see the `exists_sync`
        // guard above), so there's nothing left to stat.
        bytes: None,
    };
    if store.insert_disposal_record(&row).is_ok() {
        match confidence {
            DisposalConfidence::HistoricalExact => report.imported_exact += 1,
            DisposalConfidence::HistoricalGuess => report.imported_guess += 1,
            DisposalConfidence::Live => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vod_backup_path_swaps_the_extension() {
        assert_eq!(
            vod_backup_path(Path::new(r"A:\streams\Ch\x.mkv")),
            PathBuf::from(r"A:\streams\Ch\x.pre-vod.bak")
        );
    }

    #[test]
    fn head_and_live_guess_paths_share_the_original_stem() {
        let full = Path::new(r"A:\streams\Ch\x.full.mkv");
        assert_eq!(head_guess_path(full), Some(PathBuf::from(r"A:\streams\Ch\x.head.mkv")));
        assert_eq!(live_capture_guess_path(full), Some(PathBuf::from(r"A:\streams\Ch\x.mkv")));
    }

    #[test]
    fn guess_paths_are_none_for_an_unexpected_shape() {
        // Doesn't end in ".full.mkv" — nothing to strip, don't guess wrong.
        let odd = Path::new(r"A:\streams\Ch\x.mkv");
        assert_eq!(head_guess_path(odd), None);
        assert_eq!(live_capture_guess_path(odd), None);
    }

    #[test]
    fn report_summarize_distinguishes_empty_from_populated() {
        assert_eq!(BackfillReport::default().summarize(), "Historical import: nothing new to add.");
        let report = BackfillReport {
            imported_exact: 2,
            imported_guess: 1,
            skipped_still_exists: 3,
            skipped_drive_unreachable: 1,
            skipped_already_imported: 5,
        };
        let s = report.summarize();
        assert!(s.contains("added 3"), "{s}");
        assert!(s.contains("2 exact"), "{s}");
        assert!(s.contains("1 inferred"), "{s}");
    }

    #[test]
    fn run_historical_backfill_imports_and_is_idempotent_on_rerun() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.upsert_channel("A", "https://twitch.tv/a", crate::models::Platform::Twitch).unwrap();
        let mid = store.insert_monitor(&crate::store::test_util::sample_monitor(cid)).unwrap();

        // A gap-splice patch whose file no longer exists on the (currently
        // reachable) system drive — should import as HistoricalExact.
        let rec = store
            .insert_recording(mid, 100, "C:/tmp/x.mkv", Some(50), false, Some("s1"), None, "", "")
            .unwrap();
        store.finish_recording(rec, 200, 500, Some(0), "completed", "C:/tmp/x.mkv", "").unwrap();
        store.replace_pending_gap_ranges(rec, &[(10.0, 20.0)]).unwrap();
        let pending = store.gap_ranges_in_state(rec, "pending").unwrap();
        // A path that (almost certainly) doesn't exist, on the reachable C: drive.
        store
            .set_gap_range_state(
                pending[0].id,
                "done",
                r"C:\__streamarchiver_test_nonexistent__\x.gap10.mkv",
                0,
            )
            .unwrap();

        let report = run_historical_backfill(&store);
        assert_eq!(report.imported_exact, 1);
        assert_eq!(report.imported_guess, 0);

        let records = store.list_disposal_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].row.confidence, DisposalConfidence::HistoricalExact);
        assert_eq!(records[0].row.state, DisposalRecordState::Permanent);
        assert_eq!(records[0].row.method, DisposalMethod::Unknown);

        // Re-running must not duplicate it.
        let report2 = run_historical_backfill(&store);
        assert_eq!(report2.imported_exact, 0);
        assert_eq!(report2.skipped_already_imported, 1);
        assert_eq!(store.list_disposal_records().unwrap().len(), 1);
    }
}
