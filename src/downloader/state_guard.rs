//! SABR `.state` guard: while a from-start SABR capture runs, hold deny-read
//! guard handles on its `.state` checkpoint files so backup/AV scanners can't
//! acquire the read handles that make yt-dlp's atomic `tmp → .state` replace
//! die with `[WinError 5] Access is denied` (the Maid Mint / Octopimp
//! incidents — see `sabr_resumable_failure`'s in-flight retry, which stays as
//! the backstop for what this can't cover: the millisecond-lived rename
//! *source* tmp files, and the small re-acquire window after each replace).
//!
//! Windows sharing semantics make this work: our guard holds READ access with
//! a share mode of only `FILE_SHARE_DELETE` —
//! - a scanner's read-open **fails** (we don't share read/write), so it can
//!   never hold the non-share-delete handle that blocks the replace;
//! - yt-dlp's replace **succeeds** (it needs DELETE on the destination, which
//!   we do share), as does its success-path `.state` cleanup;
//! - after a replace our handle refers to the superseded file object, so the
//!   next tick's re-open by path lands on the NEW file and succeeds — while a
//!   re-open of a file we still guard fails with a sharing violation. That
//!   asymmetry is the staleness detector: open-succeeded ⇒ swap the guard,
//!   sharing-violation while we hold one ⇒ still current.
//!
//! Guards exist only while the capture's child process runs: they're spawned
//! next to it and dropped (handles closed) before the in-flight retry
//! relaunches the tool — yt-dlp must READ the `.state` to resume, and a
//! lingering deny-read guard would cause the very failure this prevents. For
//! the same reason the first acquire waits [`GUARD_START_DELAY_SECS`] after
//! spawn: a resuming attempt reads its state during startup.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, warn};

use super::*;

/// Setting key: hold deny-read guards on SABR `.state` files (default on;
/// `"0"` disables — the killswitch if a future yt-dlp build starts reading
/// its state mid-run).
pub const K_SABR_STATE_GUARD: &str = "sabr_state_guard";

/// Grace before the first acquire: a resuming yt-dlp reads its `.state`
/// during startup (playlist resolution, PO tokens, deep rewind can stretch
/// this), and guarding before that read would deny it.
const GUARD_START_DELAY_SECS: u64 = 120;
/// Re-acquire cadence. Each tick is one `read_dir` plus one cheap
/// `CreateFile` per state file; a freshly-replaced `.state` sits unguarded
/// for at most this long.
const GUARD_TICK: Duration = Duration::from_millis(2000);

pub(super) fn state_guard_enabled(store: &Store) -> bool {
    store.get_setting(K_SABR_STATE_GUARD).ok().flatten().is_none_or(|v| v != "0")
}

/// True for a SABR checkpoint of this capture: `{capture_file_name}.…​.state`
/// (e.g. `X.mkv` → `X.mkv.f299.mp4.state`).
fn is_state_file_for(name: &str, capture_file_name: &str) -> bool {
    name.strip_prefix(capture_file_name)
        .is_some_and(|rest| rest.starts_with('.') && rest.ends_with(".state"))
}

/// Open a guard handle: READ access, share **only** DELETE. Read access (not
/// just attributes — attribute-only opens are exempt from sharing checks and
/// would neither block nor be blocked) makes the handle participate in
/// sharing, denying everyone else read/write while yt-dlp's replace/delete
/// (DELETE access) still passes.
#[cfg(windows)]
fn open_guard(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    crate::iomon::fs::open_with_sync(Cat::FsProbe, path, |o| {
        o.read(true).share_mode(FILE_SHARE_DELETE);
    })
}

#[cfg(not(windows))]
fn open_guard(_path: &std::path::Path) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::other("state guard is Windows-only (sharing semantics)"))
}

/// ERROR_SHARING_VIOLATION — either our own still-current guard (expected) or
/// a foreign process already holding the file (reported once).
fn is_sharing_violation(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(32)
}

/// Handle to a running guard task; [`StateGuard::stop`] must be awaited after
/// the capture's child process exits and BEFORE any relaunch of the tool.
pub(super) struct StateGuard {
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl StateGuard {
    fn disabled() -> StateGuard {
        StateGuard { stop: Arc::new(AtomicBool::new(true)), task: None }
    }

    /// Signal the task and wait for its guard handles to be closed.
    pub(super) async fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Supervisor {
    /// Spawn the `.state` guard task for one SABR capture attempt (no-op
    /// handle when `sabr` is false or the setting is off). The task discovers
    /// `{capture}.*.state` siblings in the working dir and keeps deny-read
    /// guards on them until stopped.
    pub(super) fn spawn_sabr_state_guard(&self, plan: &DownloadPlan, monitor_id: i64, sabr: bool) -> StateGuard {
        if !sabr || !state_guard_enabled(&self.store) {
            return StateGuard::disabled();
        }
        let (Some(dir), Some(file_name)) = (
            plan.capture_path.parent().map(Path::to_path_buf),
            plan.capture_path.file_name().map(|n| n.to_string_lossy().into_owned()),
        ) else {
            return StateGuard::disabled();
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let task = tokio::spawn(async move {
            crate::app_core::sleep_cancellable(
                Duration::from_secs(GUARD_START_DELAY_SECS),
                &stop2,
            )
            .await;
            let mut guards: HashMap<PathBuf, std::fs::File> = HashMap::new();
            let mut reported: HashSet<PathBuf> = HashSet::new();
            while !stop2.load(Ordering::SeqCst) {
                let mut listed: HashSet<PathBuf> = HashSet::new();
                if let Ok(mut rd) = crate::iomon::fs::read_dir(Cat::FsProbe, &dir).await {
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if !is_state_file_for(&name, &file_name) {
                            continue;
                        }
                        let path = entry.path();
                        listed.insert(path.clone());
                        match open_guard(&path) {
                            Ok(f) => {
                                // Success = the file is new (first sighting or
                                // freshly replaced) — swap the guard in; the
                                // superseded handle (if any) closes on drop.
                                if guards.insert(path.clone(), f).is_none() {
                                    debug!(monitor_id, "SABR state guard: holding {name}");
                                }
                            }
                            Err(e) if is_sharing_violation(&e) => {
                                // Holding it ourselves ⇒ still current, fine.
                                // NOT holding it ⇒ a foreign process (backup/
                                // AV) beat us to it — the exact situation
                                // that kills the replace. Name the culprit
                                // once so the exclusion list can be fixed.
                                if !guards.contains_key(&path) && reported.insert(path.clone()) {
                                    let p = path.clone();
                                    let holders = tokio::task::spawn_blocking(move || {
                                        crate::platform::file_lock_holders(&[p])
                                    })
                                    .await
                                    .unwrap_or_default();
                                    let held_by = if holders.is_empty() {
                                        "an unidentified process (lock released before it could be attributed)".to_string()
                                    } else {
                                        holders.join(", ")
                                    };
                                    warn!(
                                        monitor_id,
                                        "SABR state guard: {name} is already held by {held_by} — cannot guard it; if this is a backup/AV tool, exclude the capture cache dirs"
                                    );
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                guards.remove(&path);
                            }
                            Err(e) => {
                                debug!(monitor_id, "SABR state guard: open of {name} failed: {e}");
                            }
                        }
                    }
                }
                // A vanished state file (success-path cleanup) is no longer
                // listed — drop its guard so the delete-pending file closes.
                guards.retain(|p, _| listed.contains(p));
                crate::app_core::sleep_cancellable(GUARD_TICK, &stop2).await;
            }
            // Task end drops `guards` — every handle closes before stop()
            // returns, so a retry's resume-read is never denied by us.
        });
        StateGuard { stop, task: Some(task) }
    }
}

/// Paths named in a tool's access-denied death line — the single-quoted
/// operands of e.g. `[WinError 5] Access is denied: 'A:\\x\\tmp123' ->
/// 'A:\\x\\y.state'`. Python reprs double the backslashes; both forms are
/// normalized. Only absolute drive paths are kept (the quotes can hold
/// anything).
pub(super) fn parse_locked_paths(line: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('\'') {
        let after = &rest[start + 1..];
        let Some(len) = after.find('\'') else { break };
        let raw = &after[..len];
        let normalized = raw.replace("\\\\", "\\");
        let is_abs_drive = normalized.len() > 3
            && normalized.as_bytes()[1] == b':'
            && normalized.as_bytes()[2] == b'\\'
            && normalized.as_bytes()[0].is_ascii_alphabetic();
        if is_abs_drive {
            out.push(PathBuf::from(normalized));
        }
        rest = &after[len + 1..];
    }
    out
}

/// True when a tool death line is the transient local file-lock shape the
/// guard/culprit machinery cares about.
pub(super) fn access_denied_failure(reason: &str) -> bool {
    reason.contains("WinError 5")
        || reason.contains("Access is denied")
        || reason.contains("PermissionError")
}

impl Supervisor {
    /// After an access-denied capture death, ask the Restart Manager who
    /// holds the files named in the error and log it — scanners hold locks
    /// for seconds, so querying right at death usually names the culprit
    /// (the thing whose exclusion list needs fixing). Best-effort; the
    /// rename *source* tmp file is usually gone already, so missing files
    /// are skipped rather than queried.
    pub(super) async fn log_lock_culprits(&self, tool_log: &str, monitor_id: i64) {
        let reason = log_death_reason(tool_log);
        if !access_denied_failure(&reason) {
            return;
        }
        let paths: Vec<PathBuf> = parse_locked_paths(&reason)
            .into_iter()
            .filter(|p| crate::iomon::fs::is_file_sync(Cat::FsProbe, p))
            .collect();
        if paths.is_empty() {
            debug!(monitor_id, "lock culprit: no surviving file to query (tmp already gone)");
            return;
        }
        // NB: not named `display` — tracing macros import their own
        // `display` helper inside the expansion and would shadow the local.
        let shown: Vec<String> =
            paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let holders =
            tokio::task::spawn_blocking(move || crate::platform::file_lock_holders(&paths))
                .await
                .unwrap_or_default();
        if holders.is_empty() {
            debug!(
                monitor_id,
                "lock culprit: no current holder of {} (lock already released)",
                shown.join(", ")
            );
        } else {
            warn!(
                monitor_id,
                "lock culprit: {} held by {} — if this is a backup/AV tool, exclude the capture cache dirs",
                shown.join(", "),
                holders.join(", ")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_matching_is_prefix_and_suffix_exact() {
        let cap = "Octopimp - x [youtube dAIrgOgOHQg].mkv";
        assert!(is_state_file_for(
            "Octopimp - x [youtube dAIrgOgOHQg].mkv.f299.mp4.state",
            cap
        ));
        assert!(is_state_file_for("Octopimp - x [youtube dAIrgOgOHQg].mkv.f140.state", cap));
        // Other stems, non-state siblings, and the capture itself don't match.
        assert!(!is_state_file_for("Octopimp - y.mkv.f299.mp4.state", cap));
        assert!(!is_state_file_for("Octopimp - x [youtube dAIrgOgOHQg].mkv.f299.mp4.part", cap));
        assert!(!is_state_file_for(cap, cap));
        // A stem that merely starts with the capture name needs the dot.
        assert!(!is_state_file_for("Octopimp - x [youtube dAIrgOgOHQg].mkv2.state", cap));
    }

    /// Live validation of the entire sharing-semantics premise on real NTFS:
    /// a guarded `.state` (1) denies scanner-style read opens, (2) reports a
    /// sharing violation on re-acquire while current (the staleness
    /// detector), (3) still lets the atomic `tmp → .state` replace through,
    /// and (4) re-acquires cleanly on the post-replace NEW file.
    #[test]
    #[cfg(windows)]
    #[allow(clippy::disallowed_methods)] // test fixture I/O, not app I/O
    fn guard_denies_readers_but_allows_replace() {
        let dir = std::env::temp_dir();
        let state = dir.join(format!("sa-guard-selftest-{}.state", std::process::id()));
        let tmp = dir.join(format!("sa-guard-selftest-{}.tmp", std::process::id()));
        std::fs::write(&state, b"v1").unwrap();

        let guard = open_guard(&state).expect("guard acquires a free state file");
        // Scanner-style read open (default permissive share mode) → denied.
        let denied = std::fs::File::open(&state);
        assert!(is_sharing_violation(&denied.expect_err("read open must be denied")));
        // Re-acquire while we hold the current file → sharing violation.
        assert!(is_sharing_violation(&open_guard(&state).expect_err("self re-open must fail")));
        // yt-dlp's checkpoint replace (rename-over) must still succeed:
        // DELETE is the one thing the guard shares.
        std::fs::write(&tmp, b"v2").unwrap();
        std::fs::rename(&tmp, &state).expect("replace over a guarded state must succeed");
        // The path now names a NEW file object → the swap re-acquire works.
        let swapped = open_guard(&state).expect("guard swaps onto the replaced file");
        drop(swapped);
        drop(guard);
        let _ = std::fs::remove_file(&state);
    }

    #[test]
    fn parse_locked_paths_from_real_death_line() {
        // The exact 2026-07-20 Octopimp line (Python repr, doubled backslashes).
        let line = r"yt_dlp.utils.UnavailableVideoError: Unable to download video: [WinError 5] Access is denied: 'A:\\streams\\.sa-cache\\Octopimp\\tmpdsihyqqz' -> 'A:\\streams\\.sa-cache\\Octopimp\\Octopimp - 2026-07-19 23-39-03 - OctoVT Live Stream [games-tba] (p sabr  ) - [youtube dAIrgOgOHQg].mkv.f299.mp4.state'";
        let paths = parse_locked_paths(line);
        assert_eq!(paths.len(), 2);
        assert_eq!(
            paths[0],
            PathBuf::from(r"A:\streams\.sa-cache\Octopimp\tmpdsihyqqz")
        );
        assert!(paths[1].to_string_lossy().ends_with(".mkv.f299.mp4.state"));
        assert!(access_denied_failure(line));

        // Single-backslash form (non-repr contexts) parses identically.
        let single = r"PermissionError: [WinError 5] Access is denied: 'C:\x\y.state'";
        assert_eq!(parse_locked_paths(single), vec![PathBuf::from(r"C:\x\y.state")]);

        // Quoted non-path operands are ignored; unrelated errors don't match.
        assert!(parse_locked_paths("ERROR: option 'foo' is unknown").is_empty());
        assert!(!access_denied_failure("ERROR: fragment not found"));
    }
}
