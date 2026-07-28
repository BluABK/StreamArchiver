//! Rolling backups of the SQLite database (`VACUUM INTO`) — a same-day,
//! internally-consistent snapshot to fall back on, in case a mistake against
//! the live database (or a corruption, disk failure, etc.) needs undoing.
//!
//! Runs on its own **read-only** [`Connection`] to the same file rather than
//! the shared [`Store`] connection: `VACUUM INTO` only ever reads the source,
//! and a dedicated connection means a backup (potentially multi-second, for a
//! database in the hundreds of MB) never holds `Store`'s single shared
//! connection mutex — so it can't stall the scheduler tick or a recording's
//! in-flight metadata writes. WAL mode (which this app always runs in) lets
//! this reader proceed concurrently with the app's own writer regardless.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use tracing::{info, warn};

use crate::store::Store;

/// Settings key: rolling backups on/off. Default on — this is a safety net,
/// not an opt-in feature.
pub const K_ENABLED: &str = "db_backup_enabled";
/// Settings key: hours between backups.
pub const K_INTERVAL_HOURS: &str = "db_backup_interval_hours";
/// Settings key: how many rolling snapshots to keep.
pub const K_RETENTION_COUNT: &str = "db_backup_retention_count";
/// Settings key: last time a backup was attempted (unix secs) — throttles
/// [`maybe_run_backup`], same shape as `app_paths::K_LOGS_PRUNE_LAST`.
pub const K_LAST_RUN: &str = "db_backup_last_run";

pub const DEFAULT_INTERVAL_HOURS: i64 = 24;
pub const DEFAULT_RETENTION_COUNT: i64 = 14;

/// Filename prefix for backup files, e.g. `streamarchiver-1782844618.sqlite3`.
const FILE_PREFIX: &str = "streamarchiver-";

pub fn enabled(store: &Store) -> bool {
    store.get_setting(K_ENABLED).ok().flatten().is_none_or(|v| v != "0")
}

pub fn interval_hours(store: &Store) -> i64 {
    store
        .get_setting(K_INTERVAL_HOURS)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_INTERVAL_HOURS)
}

pub fn retention_count(store: &Store) -> i64 {
    store
        .get_setting(K_RETENTION_COUNT)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_RETENTION_COUNT)
}

pub fn last_run(store: &Store) -> i64 {
    store.get_setting(K_LAST_RUN).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Copy the live database into `backups_dir()` as a self-contained, VACUUMed
/// snapshot, then prune down to `keep`. Safe to call at any time while the
/// app is fully live (see the module doc for why).
pub fn run_backup_now(now: i64, keep: i64) -> Result<PathBuf> {
    backup_into(&crate::app_paths::db_path(), &crate::app_paths::backups_dir(), now, keep)
}

/// Does the actual copy + prune against explicit paths, so tests can point it
/// at a sandboxed directory instead of going through the global, env-var
/// overridable [`crate::app_paths::db_path`]/[`crate::app_paths::backups_dir`]
/// (which [`run_backup_now`] uses) and risking a real backups directory.
fn backup_into(src_path: &Path, dest_dir: &Path, now: i64, keep: i64) -> Result<PathBuf> {
    let dest_path = dest_dir.join(format!("{FILE_PREFIX}{now}.sqlite3"));

    let src = Connection::open_with_flags(src_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only for backup", src_path.display()))?;
    // Bound parameters aren't accepted for the destination filename by
    // sqlite's VACUUM INTO grammar in older builds; the path comes from our
    // own timestamped join above, never from user input, so inlining it
    // (single-quoted, with any embedded quote escaped) is safe.
    let escaped = dest_path.to_string_lossy().replace('\'', "''");
    src.execute_batch(&format!("VACUUM INTO '{escaped}'"))
        .with_context(|| format!("VACUUM INTO {}", dest_path.display()))?;
    prune_backups(dest_dir, keep.max(1) as usize);
    Ok(dest_path)
}

/// Delete the oldest backups beyond `keep` (ordered by the timestamp embedded
/// in the filename, newest kept).
fn prune_backups(dir: &Path, keep: usize) {
    let Ok(entries) = crate::iomon::fs::read_dir_sync(crate::iomon::Cat::Db, dir) else { return };
    let mut files: Vec<(i64, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_str()?.to_string();
            let ts: i64 = stem.strip_prefix(FILE_PREFIX)?.parse().ok()?;
            Some((ts, path))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(ts, _)| *ts);
    for (_, path) in &files[..files.len() - keep] {
        if let Err(e) = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::Db, path) {
            warn!("db_backup: failed to prune old backup {}: {e:#}", path.display());
        }
    }
}

/// List existing backups, newest first, as `(unix timestamp, path, size in
/// bytes)`. Used by the Settings UI; best-effort (an unreadable directory or
/// file just doesn't appear rather than erroring the whole page).
pub fn list_backups() -> Vec<(i64, PathBuf, u64)> {
    let dir = crate::app_paths::backups_dir();
    let Ok(entries) = crate::iomon::fs::read_dir_sync(crate::iomon::Cat::Db, &dir) else {
        return Vec::new();
    };
    let mut files: Vec<(i64, PathBuf, u64)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_str()?.to_string();
            let ts: i64 = stem.strip_prefix(FILE_PREFIX)?.parse().ok()?;
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            Some((ts, path, size))
        })
        .collect();
    files.sort_by_key(|(ts, _, _)| std::cmp::Reverse(*ts));
    files
}

/// Run the rolling-backup sweep if `interval_hours` has elapsed since the
/// last run (or it has never run) and backups are enabled — a cheap no-op
/// (two settings reads) the rest of the time. Called from the scheduler
/// tick, mirroring `app_paths::maybe_prune_old_logs`.
pub fn maybe_run_backup(store: &Store, now: i64) {
    if !enabled(store) {
        return;
    }
    let interval_secs = interval_hours(store).saturating_mul(3600);
    if now - last_run(store) < interval_secs {
        return;
    }
    // Record the attempt regardless of outcome, so a persistently failing
    // backup (e.g. disk full) doesn't retry every single tick.
    let _ = store.set_setting(K_LAST_RUN, &now.to_string());
    match run_backup_now(now, retention_count(store)) {
        Ok(path) => info!("db_backup: wrote {}", path.display()),
        Err(e) => warn!("db_backup: backup failed: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    // Test-only: throwaway tempdirs that iomon has no need to classify/attribute.
    #![allow(clippy::disallowed_methods)]
    use super::*;

    #[test]
    fn prune_backups_keeps_only_the_newest_n() {
        let dir = std::env::temp_dir().join(format!(
            "sa_db_backup_prune_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for ts in [100, 200, 300, 400, 500] {
            std::fs::write(dir.join(format!("{FILE_PREFIX}{ts}.sqlite3")), b"x").unwrap();
        }
        // An unrelated file must never be touched by the prefix-gated parse.
        std::fs::write(dir.join("unrelated.txt"), b"x").unwrap();

        prune_backups(&dir, 2);

        let mut remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            vec![
                "streamarchiver-400.sqlite3".to_string(),
                "streamarchiver-500.sqlite3".to_string(),
                "unrelated.txt".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_into_produces_a_readable_snapshot_and_prunes() {
        let root = std::env::temp_dir().join(format!(
            "sa_db_backup_src_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let dest_dir = root.join("backups");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let src_path = root.join("streamarchiver.sqlite3");

        let store = Store::open(&src_path).unwrap();
        store.set_setting("probe_key", "probe_value").unwrap();
        drop(store);

        let now = 1_000_000;
        let path = backup_into(&src_path, &dest_dir, now, 1).unwrap();
        assert!(path.exists());

        let check = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let value: String = check
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'probe_key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "probe_value");
        drop(check); // Windows won't let the prune below delete an open file.

        // A second backup at a later timestamp, with keep=1, must prune the first.
        let path2 = backup_into(&src_path, &dest_dir, now + 1, 1).unwrap();
        assert!(path2.exists());
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
