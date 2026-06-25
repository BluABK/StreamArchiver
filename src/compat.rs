//! Per-build compatibility fixups applied when re-attaching to a detached
//! download started by a *different* app build.
//!
//! Every detached download stores the [`crate::version::build_id`] of the build
//! that spawned it (`detached_process.spawn_build`). When a newer build
//! re-attaches and the stored build differs from the running one, the startup
//! reconcile calls [`reattach_fixups`] first — the single seam where a code
//! change that would otherwise mis-handle an in-flight download (a renamed
//! capture-path scheme, a changed log format, altered tool args) registers a
//! migration keyed by the build that produced the row.
//!
//! Today this is a no-op: the current build is the only one that ever wrote the
//! registry. The `match` below is where future fixups land; each should log what
//! it did so re-attach decisions stay auditable.

use tracing::debug;

use crate::models::DetachedRow;

/// Apply any fixups needed to safely re-attach to `row`, which was spawned by
/// build `spawn_build` (already known to differ from the running build). Mutates
/// `row` in place (e.g. rewriting a path an old build used) and returns whether
/// any fixup was applied. Each fixup should `info!`-log what it did.
///
/// No fixups exist yet — the current build is the only one that has ever written
/// the registry. The `version` parse below is the seam: add migrations keyed by
/// the spawning build, e.g.
/// `if older_than(version, "0.2.0") { rewrite_capture_path(row); return true; }`.
pub fn reattach_fixups(spawn_build: &str, row: &mut DetachedRow) -> bool {
    let (version, _git) = split_build(spawn_build);
    let _ = (version, &mut *row);
    debug!(spawn_build, "no re-attach compat fixups for this build");
    false
}

/// Split a `build_id()` (`"<major>.<minor>.<patch>/<sha>[-dirty]"`) into its
/// version and git parts; either may be empty for an unrecognized string.
fn split_build(spawn_build: &str) -> (&str, &str) {
    match spawn_build.split_once('/') {
        Some((v, g)) => (v, g),
        None => (spawn_build, ""),
    }
}
