//! Lock-culprit attribution for SABR capture deaths: when yt-dlp dies to
//! `[WinError 5] Access is denied` renaming its checkpoint tmp onto a
//! `.state` file, ask the Windows Restart Manager who holds the named files
//! and log it — scanners hold locks for seconds, so querying right at death
//! usually names the process whose exclusion list needs fixing.
//!
//! ## Why there is no `.state` *guard* here (2026-07-20 postmortem)
//!
//! This module used to also hold deny-read guard handles (READ access, share
//! only DELETE) on the capture's `.state` files, on the theory that a scanner
//! could then never acquire the read handle that kills yt-dlp's atomic
//! `tmp → .state` replace, while the replace itself (DELETE access) still
//! passed — a premise validated by a unit test doing `std::fs::rename` over a
//! guarded file on real NTFS. The field data said otherwise: every guarded
//! Maid Mint attempt died with WinError 5 on the FIRST checkpoint write after
//! the guard's 120s start grace, take after take. Root cause, reproduced
//! empirically: Rust's rename goes through `MoveFileExW`, which tolerates a
//! share-DELETE handle on the destination — but CPython 3.13's `os.replace`
//! (what the SABR fork calls) renames via `SetFileInformationByHandle` /
//! `FILE_RENAME_INFO` without POSIX semantics, which fails with
//! `ERROR_ACCESS_DENIED` when the destination has ANY open handle, even one
//! sharing read+write+delete. So the guard deterministically killed the very
//! write it existed to protect, and no handle-holding scheme can work: any
//! open handle at replace time — ours or a scanner's, however politely shared
//! — kills the replace. The durable fix lives in the yt-dlp-dev fork instead
//! (`_state.py` retries its `os.replace` on `PermissionError`); this module
//! keeps only the attribution half, plus the in-flight retry as backstop.

use std::path::PathBuf;

use tracing::{debug, warn};

use super::*;

/// True for a SABR checkpoint of this capture: `{capture_file_name}.…​.state`
/// (e.g. `X.mkv` → `X.mkv.f299.mp4.state`).
fn is_state_file_for(name: &str, capture_file_name: &str) -> bool {
    name.strip_prefix(capture_file_name)
        .is_some_and(|rest| rest.starts_with('.') && rest.ends_with(".state"))
}

/// Paths named in a tool's access-denied death line — the quoted operands of
/// e.g. `[WinError 5] Access is denied: 'A:\\x\\tmp123' -> "A:\\x\\Don't
/// forget.state"`. Python quotes each operand with `'…'` normally but
/// switches to `"…"` when the path itself contains an apostrophe (stream
/// titles do — the Maid Mint "Don't forget your coffee!" take hid its
/// `.state` path from the first version of this parser that way). Windows
/// filenames can never contain `"`, so scanning for a matching same-quote
/// terminator is unambiguous for both forms. Python-repr doubled backslashes
/// are normalized; only absolute drive paths are kept.
///
/// NB: paths that reach the log through a Python tool's stderr can be mangled
/// by its console encoding (non-ASCII chars dropped or replaced with U+FFFD),
/// in which case the parsed path names no real file — the caller falls back
/// to the capture's on-disk `.state` siblings for exactly that case.
pub(super) fn parse_locked_paths(line: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find(['\'', '"']) {
        let quote = rest.as_bytes()[start] as char;
        let after = &rest[start + 1..];
        let Some(len) = after.find(quote) else { break };
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
/// culprit machinery cares about.
pub(super) fn access_denied_failure(reason: &str) -> bool {
    reason.contains("WinError 5")
        || reason.contains("Access is denied")
        || reason.contains("PermissionError")
}

impl Supervisor {
    /// After an access-denied capture death, ask the Restart Manager who
    /// holds the files named in the error and log it. Best-effort: the
    /// rename *source* tmp file is deleted by yt-dlp's own cleanup before we
    /// run, so missing files are skipped — and because the error line's
    /// paths can be encoding-mangled by the tool's stderr (see
    /// [`parse_locked_paths`]), the capture's surviving on-disk `.state`
    /// siblings are always added to the query set as ground truth.
    pub(super) async fn log_lock_culprits(
        &self,
        tool_log: &str,
        capture_path: &Path,
        monitor_id: i64,
    ) {
        let reason = log_death_reason(tool_log);
        if !access_denied_failure(&reason) {
            return;
        }
        let mut paths: Vec<PathBuf> = parse_locked_paths(&reason)
            .into_iter()
            .filter(|p| crate::iomon::fs::is_file_sync(Cat::FsProbe, p))
            .collect();
        for p in state_file_siblings(capture_path).await {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
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

/// The capture's `.state` checkpoint files that exist on disk right now —
/// the rename destinations an access-denied death actually fought over,
/// discovered by directory listing so stderr encoding mangling can't hide
/// them.
async fn state_file_siblings(capture_path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(file_name)) = (
        capture_path.parent(),
        capture_path.file_name().map(|n| n.to_string_lossy().into_owned()),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(mut rd) = crate::iomon::fs::read_dir(Cat::FsProbe, dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_state_file_for(&name, &file_name) {
                out.push(entry.path());
            }
        }
    }
    out
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

    #[tokio::test]
    async fn state_file_siblings_finds_only_this_captures_states() {
        let dir = std::env::temp_dir().join(format!("sa-culprit-test-{}", std::process::id()));
        #[allow(clippy::disallowed_methods)] // test fixture I/O, not app I/O
        {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("cap.mkv.f140.mp4.state"), b"x").unwrap();
            std::fs::write(dir.join("cap.mkv.f399.mp4.state"), b"x").unwrap();
            std::fs::write(dir.join("other.mkv.f140.mp4.state"), b"x").unwrap();
            std::fs::write(dir.join("cap.mkv"), b"x").unwrap();
        }
        let mut found = state_file_siblings(&dir.join("cap.mkv")).await;
        found.sort();
        assert_eq!(
            found,
            vec![dir.join("cap.mkv.f140.mp4.state"), dir.join("cap.mkv.f399.mp4.state")]
        );
        #[allow(clippy::disallowed_methods)]
        std::fs::remove_dir_all(&dir).ok();
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

        // The 2026-07-20 Maid Mint shape: the destination contains an
        // apostrophe ("Don't"), so Python double-quotes THAT operand — the
        // single-quote-only first version of this parser missed it entirely
        // and the culprit query never saw the surviving .state file.
        let mixed = r#"UnavailableVideoError: Unable to download video: [WinError 5] Access is denied: 'A:\\streams\\.sa-cache\\Maid Mint\\tmpmbfi87e8' -> "A:\\streams\\.sa-cache\\Maid Mint\\Maid Mint - Don't forget your coffee!.mkv.f140.mp4.state""#;
        let paths = parse_locked_paths(mixed);
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths[0].to_string_lossy().ends_with("tmpmbfi87e8"));
        assert_eq!(
            paths[1],
            PathBuf::from(
                r"A:\streams\.sa-cache\Maid Mint\Maid Mint - Don't forget your coffee!.mkv.f140.mp4.state"
            )
        );

        // Quoted non-path operands are ignored; unrelated errors don't match.
        assert!(parse_locked_paths("ERROR: option 'foo' is unknown").is_empty());
        assert!(!access_denied_failure("ERROR: fragment not found"));
    }
}
