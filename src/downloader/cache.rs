//! Capture working-dir (`.sa-cache`) layout: central roots, per-dir and
//! legacy candidates, reverse mapping, hiding, and the cache sweeper.

use super::*;

/// Name of the hidden working dir for in-progress captures. Deliberately
/// app-unique (".sa-cache", NOT ".cache") so backup tools that only support
/// global folder-NAME exclusions (e.g. Backblaze — no per-drive dir rules)
/// can exclude it without touching unrelated ".cache" dirs elsewhere.
pub const CACHE_DIR_NAME: &str = ".sa-cache";

/// The pre-2026-07-11 working-dir name. Never used for NEW captures; every
/// lookup (stranded captures, split parts, SABR resume state, sidecars, the
/// startup sweep) keeps checking it until the old dirs drain empty and the
/// sweep removes them.
pub const LEGACY_CACHE_DIR_NAME: &str = ".cache";

/// Settings key for the central capture-cache location (empty = per-output-dir
/// `.sa-cache\` subfolders, the pre-setting behavior).
pub const K_CACHE_ROOT: &str = "capture_cache_root";

/// The configured central cache roots (each normalized to end in
/// [`CACHE_DIR_NAME`]), at most one per drive letter — output dirs pick the
/// root on THEIR drive. Empty = per-output-dir layout everywhere.
pub(super) static CACHE_ROOTS: parking_lot::RwLock<Vec<PathBuf>> = parking_lot::RwLock::new(Vec::new());

/// Apply the central cache-root setting (startup + live on settings save).
/// Accepts SEVERAL roots separated by `;` (or newlines) — recordings can span
/// drives (`A:\streams\…` and `G:\streams\…` instances), and each drive needs
/// its own root since promotion must stay a same-volume rename. Each value is
/// normalized to end in a `.sa-cache` component — that name is what every
/// cache-membership check (string `contains`, SQL `LIKE`) keys on, and it's
/// the folder to exclude in backup tools. The first root listed for a drive
/// wins.
pub fn set_cache_root(raw: &str) {
    let mut roots: Vec<PathBuf> = Vec::new();
    for part in raw.split([';', '\n']) {
        let trimmed = part.trim().trim_end_matches(['\\', '/']);
        if trimmed.is_empty() {
            continue;
        }
        let p = PathBuf::from(trimmed);
        let p = if p.file_name().is_some_and(is_cache_dir_name) {
            p
        } else {
            p.join(CACHE_DIR_NAME)
        };
        // One root per drive — a second root on the same letter is ignored.
        if drive_of(&p).is_some_and(|d| roots.iter().any(|r| drive_of(r) == Some(d))) {
            warn!("capture cache root ignored (drive already has one): {}", p.display());
            continue;
        }
        roots.push(p);
    }
    for r in &roots {
        info!("capture cache root: {}", r.display());
    }
    *CACHE_ROOTS.write() = roots;
}

/// Drive letter of a path's prefix component (e.g. 'A' for `A:\x`), uppercased.
pub(super) fn drive_of(path: &Path) -> Option<char> {
    match path.components().next()? {
        std::path::Component::Prefix(p) => match p.kind() {
            std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) => {
                Some((d as char).to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

/// The hidden working directory for in-progress captures. Default layout: a
/// `.sa-cache\` subfolder of the output dir. With a central cache root
/// configured (`K_CACHE_ROOT`, e.g. `A:\streams\.sa-cache`), the layout is
/// `{root}\{output-dir leaf}\` instead — one excludable subtree per drive for
/// backup tools whose exclusions are path-based (no wildcards). The central
/// root only applies to output dirs on the SAME drive (promotion must stay a
/// same-volume rename, never a multi-GB cross-drive copy); others fall back
/// to the per-dir layout. The `.`-prefix hides it on Unix;
/// [`crate::platform::set_hidden`] adds the Windows hidden attribute when the
/// dir is created.
pub(crate) fn cache_dir(output_dir: &Path) -> PathBuf {
    if let Some(out_drive) = drive_of(output_dir)
        && let Some(root) = CACHE_ROOTS
            .read()
            .iter()
            .find(|r| drive_of(r) == Some(out_drive))
        && let Some(leaf) = output_dir.file_name()
        // An output dir inside the root itself would recurse the cache into
        // itself — keep those on the per-dir layout.
        && !output_dir.starts_with(root)
    {
        return root.join(leaf);
    }
    output_dir.join(CACHE_DIR_NAME)
}

/// Every working dir a recording's files might live in, current layout first:
/// the configured layout ([`cache_dir`]), the per-dir `.sa-cache\`, and the
/// legacy per-dir `.cache\`. Lookups of files that may PRE-DATE the central
/// root or the rename go through this; producers of new files use
/// [`cache_dir`] directly.
pub(crate) fn cache_dir_candidates(output_dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        cache_dir(output_dir),
        output_dir.join(CACHE_DIR_NAME),
        output_dir.join(LEGACY_CACHE_DIR_NAME),
    ];
    v.dedup();
    v
}

/// True if `name` is a capture working-dir name (current or legacy).
pub fn is_cache_dir_name(name: &std::ffi::OsStr) -> bool {
    name == CACHE_DIR_NAME || name == LEGACY_CACHE_DIR_NAME
}

/// Where a cache-resident file belongs once promoted: the same path with the
/// `.sa-cache`/`.cache` component removed. Works for every layout —
/// `A:\s\ch\.sa-cache\x.ts` → `A:\s\ch\x.ts` (per-dir),
/// `A:\s\.sa-cache\ch\x.ts` → `A:\s\ch\x.ts` (central root). `None` when the
/// path has no cache component (already promoted).
pub fn strip_cache_component(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut found = false;
    for c in path.components() {
        if !found
            && let std::path::Component::Normal(n) = c
            && is_cache_dir_name(n)
        {
            found = true;
            continue;
        }
        out.push(c.as_os_str());
    }
    found.then_some(out)
}

/// Distinct directories PAST recordings live in, derived from every stored
/// recording path (cache-resident ones mapped to their promoted parent). An
/// instance retargeted to another drive leaves its history behind — these
/// dirs keep old drives visible to the I/O monitor, the startup cache sweep,
/// and the Files view, even when no current instance points there.
pub fn historical_recording_dirs(store: &crate::store::Store) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = store
        .recording_paths_with_bytes()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(p, _)| {
            let path = PathBuf::from(&p);
            strip_cache_component(&path)
                .unwrap_or(path)
                .parent()
                .map(Path::to_path_buf)
        })
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs
}

/// Mark the working dir hidden on Windows — the `.sa-cache`/`.cache` ANCESTOR
/// component, not the leaf (under a central root, `{root}\{channel}` is a
/// plain subfolder; hiding the root hides the whole subtree).
pub(super) fn set_cache_hidden(cache: &Path) {
    let mut p = cache;
    loop {
        if p.file_name().is_some_and(is_cache_dir_name) {
            crate::platform::set_hidden(p);
            return;
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => {
                crate::platform::set_hidden(cache);
                return;
            }
        }
    }
}

/// True if a stored path string points into a capture working dir (current or
/// legacy name) — the string-level counterpart of [`is_cache_dir_name`] for
/// DB `output_path` values.
pub fn path_in_cache(path: &str) -> bool {
    path.contains(CACHE_DIR_NAME) || path.contains(LEGACY_CACHE_DIR_NAME)
}

/// Best-effort growing working-dir file candidates for a still-recording take,
/// keyed off its predicted final path's stem (mirrors how `build_plan`
/// derives `capture_path`, without needing to re-derive SABR/container state
/// — the caller just probes each candidate and uses whichever exists). Used
/// by `Supervisor::manual_head_backfill` for a take that's still active.
pub(super) fn live_capture_candidates(final_path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) =
        (final_path.parent(), final_path.file_stem().map(|s| s.to_string_lossy().into_owned()))
    else {
        return Vec::new();
    };
    cache_dir_candidates(dir)
        .iter()
        .flat_map(|cache| {
            [cache.join(format!("{stem}.ts")), cache.join(format!("{stem}.mkv"))]
        })
        .collect()
}
/// Stale `.cache\` working files are swept after this age on startup.
pub(super) const CACHE_MAX_AGE_SECS: u64 = 24 * 3600;

/// The working dir (current or legacy name) that still holds SABR resume
/// state (`.state` / `.sq0.part` / `.part`) for the recording's stem — i.e.
/// an interrupted SABR capture that can be continued, AND where its surviving
/// files actually live (a pre-rename capture resumes in the legacy dir so
/// yt-dlp's `-o` matches the original). Derived synchronously from the
/// recording's stored output path.
pub(super) fn sabr_state_dir(output_path: &str) -> Option<PathBuf> {
    let p = Path::new(output_path);
    let (dir, stem) = (p.parent()?, p.file_stem().map(|s| s.to_string_lossy().into_owned())?);
    let prefix = format!("{stem}.");
    for cache in cache_dir_candidates(dir) {
        let Ok(rd) = crate::iomon::fs::read_dir_sync(Cat::FsProbe, &cache) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix)
                && (name.ends_with(".state")
                    || name.ends_with(".sq0.part")
                    || name.ends_with(".part"))
            {
                return Some(cache);
            }
        }
    }
    None
}

/// True if a recording's working dir still holds SABR resume state — see
/// [`sabr_state_dir`].
pub(super) fn sabr_state_exists(output_path: &str) -> bool {
    sabr_state_dir(output_path).is_some()
}

impl Supervisor {
    /// Delete stale working files from every output dir's `.cache\` (older than
    /// [`CACHE_MAX_AGE_SECS`]), skipping any stem currently being resumed. Removes a
    /// `.cache\` dir that ends up empty. Best-effort; runs once at startup.
    pub async fn sweep_caches(&self, skip_stems: std::collections::HashSet<String>) {
        // Current instance output dirs PLUS every dir past recordings live in
        // — retargeting an instance to another drive must not strand stale
        // working files (or empty legacy dirs) on the old one forever.
        let mut dirs = self.store.all_output_dirs().unwrap_or_default();
        dirs.extend(
            historical_recording_dirs(&self.store)
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        dirs.sort_unstable();
        dirs.dedup();
        let now = std::time::SystemTime::now();
        for d in dirs {
            // Both the current working dir and the legacy `.cache\` — the
            // legacy one only drains (nothing writes there anymore) and its
            // empty husk is removed below, ending backup-tool churn on it.
            for cache in cache_dir_candidates(Path::new(&d)) {
                let Ok(mut rd) = crate::iomon::fs::read_dir(Cat::CacheSweep, &cache).await else {
                    continue;
                };
                let mut removed = 0u32;
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if skip_stems
                        .iter()
                        .any(|s| name.starts_with(&format!("{s}.")))
                    {
                        continue; // belongs to a recording being resumed
                    }
                    let Ok(meta) = entry.metadata().await else {
                        continue;
                    };
                    let stale = meta
                        .modified()
                        .ok()
                        .and_then(|m| now.duration_since(m).ok())
                        .map(|age| age.as_secs() >= CACHE_MAX_AGE_SECS)
                        .unwrap_or(false);
                    if stale && meta.is_file() && crate::iomon::fs::remove_file(Cat::CacheSweep, entry.path()).await.is_ok() {
                        removed += 1;
                    }
                }
                if removed > 0 {
                    info!(
                        "capture-cache sweep: deleted {removed} leftover transient working \
                         file(s) from the on-disk cache {} (abandoned mid-capture temp data \
                         older than {}h; finished archives are never swept)",
                        cache.display(),
                        CACHE_MAX_AGE_SECS / 3600,
                    );
                }
                let _ = crate::iomon::fs::remove_dir(Cat::CacheSweep, &cache).await; // only if now empty
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use crate::models::{Channel, Container, DetectionMethod, Monitor, Tool};
    #[allow(unused_imports)]
    use crate::downloader::test_util::*;

    #[test]
    #[cfg(windows)]
    fn central_cache_root_layout_and_reverse_mapping() {
        // Reverse mapping: the promoted location is the path minus its cache
        // component, for every layout generation.
        assert_eq!(
            strip_cache_component(Path::new(r"A:\s\ch\.sa-cache\x.ts")),
            Some(PathBuf::from(r"A:\s\ch\x.ts"))
        );
        assert_eq!(
            strip_cache_component(Path::new(r"A:\s\.sa-cache\ch\x.ts")),
            Some(PathBuf::from(r"A:\s\ch\x.ts"))
        );
        assert_eq!(
            strip_cache_component(Path::new(r"A:\s\ch\.cache\x.ts")),
            Some(PathBuf::from(r"A:\s\ch\x.ts"))
        );
        assert_eq!(strip_cache_component(Path::new(r"A:\s\ch\x.ts")), None);

        // Use a drive letter no other test touches — CACHE_ROOT is process-global.
        set_cache_root(r"Q:\streams"); // normalized to Q:\streams\.sa-cache
        // Same drive → central layout, one subfolder per output-dir leaf.
        assert_eq!(
            cache_dir(Path::new(r"Q:\streams\Chan")),
            PathBuf::from(r"Q:\streams\.sa-cache\Chan")
        );
        // Different drive → per-dir fallback (promotion must stay a rename).
        assert_eq!(
            cache_dir(Path::new(r"R:\out\Chan")),
            PathBuf::from(r"R:\out\Chan\.sa-cache")
        );
        // An output dir inside the root itself must not recurse the cache.
        assert_eq!(
            cache_dir(Path::new(r"Q:\streams\.sa-cache\Chan")),
            PathBuf::from(r"Q:\streams\.sa-cache\Chan\.sa-cache")
        );
        // Lookups cover all three layouts, current first.
        assert_eq!(
            cache_dir_candidates(Path::new(r"Q:\streams\Chan")),
            vec![
                PathBuf::from(r"Q:\streams\.sa-cache\Chan"),
                PathBuf::from(r"Q:\streams\Chan\.sa-cache"),
                PathBuf::from(r"Q:\streams\Chan\.cache"),
            ]
        );
        // Multiple roots (;-separated), one per drive — each output dir picks
        // the root on ITS drive.
        set_cache_root(r"Q:\streams ; S:\rec\.sa-cache");
        assert_eq!(
            cache_dir(Path::new(r"Q:\streams\Chan")),
            PathBuf::from(r"Q:\streams\.sa-cache\Chan")
        );
        assert_eq!(
            cache_dir(Path::new(r"S:\rec\Chan")),
            PathBuf::from(r"S:\rec\.sa-cache\Chan")
        );
        assert_eq!(
            cache_dir(Path::new(r"R:\out\Chan")),
            PathBuf::from(r"R:\out\Chan\.sa-cache")
        );
        set_cache_root("");
        assert_eq!(
            cache_dir(Path::new(r"Q:\streams\Chan")),
            PathBuf::from(r"Q:\streams\Chan\.sa-cache")
        );
    }

    #[test]
    fn cache_dir_rename_lookups_cover_both_names() {
        // Producers use the new, backup-excludable name…
        assert!(cache_dir(Path::new("C:/out")).ends_with(CACHE_DIR_NAME));
        // …while name checks accept both generations.
        assert!(is_cache_dir_name(std::ffi::OsStr::new(CACHE_DIR_NAME)));
        assert!(is_cache_dir_name(std::ffi::OsStr::new(LEGACY_CACHE_DIR_NAME)));
        assert!(!is_cache_dir_name(std::ffi::OsStr::new(".cachex")));
        assert!(path_in_cache(r"A:\s\c\.sa-cache\x.ts"));
        assert!(path_in_cache(r"A:\s\c\.cache\x.ts"));
        assert!(!path_in_cache(r"A:\s\c\x.ts"));

        // find_split_media scans the NEW dir from a final path too (the
        // legacy dir is covered by find_split_media_accepts_bare_parts_only).
        let dir = std::env::temp_dir().join(format!("sa-split-new-{}", std::process::id()));
        let cache = dir.join(CACHE_DIR_NAME);
        std::fs::create_dir_all(&cache).unwrap();
        let stem = "Chan - 2026-07-11 01-02-03 - title [youtube xyz]";
        std::fs::write(cache.join(format!("{stem}.mkv.f299.mp4")), b"v").unwrap();
        std::fs::write(cache.join(format!("{stem}.mkv.f140.mp4")), b"a").unwrap();
        let parts = find_split_media(&dir.join(format!("{stem}.mkv")));
        assert_eq!(parts.len(), 2);

        // …and live_capture_candidates probes both dirs, new name first.
        let cands = live_capture_candidates(&dir.join(format!("{stem}.mkv")));
        assert!(cands[0].to_string_lossy().contains(CACHE_DIR_NAME));
        assert!(cands.iter().any(|c| {
            let s = c.to_string_lossy().into_owned();
            s.contains(LEGACY_CACHE_DIR_NAME) && !s.contains(CACHE_DIR_NAME)
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
