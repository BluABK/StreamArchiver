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
/// `pub(crate)`: also used by `disposal::pick_trash_root`'s same-drive matching.
pub(crate) fn drive_of(path: &Path) -> Option<char> {
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

/// True if `name` is a recognized transient working-file pattern — safe for
/// [`Supervisor::sweep_caches`] to delete by age alone. An **allowlist**, not
/// a denylist: a genuine capture (`.ts`, `.mkv`, a bare per-format `.mp4`/
/// `.webm` download) never matches, however long it's sat there. A 2026-07
/// incident lost ~7.7h of a recording this way: a stale-but-unreferenced
/// `.sa-cache\` `.ts` (left behind by a botched/interrupted promotion that
/// silently produced a short final file) was swept as if it were leftover
/// working-file litter, when it was actually the only complete copy. Recognize
/// specific tool byproducts by name instead of assuming "any old file in a
/// cache dir is disposable".
pub(super) fn is_sweepable_cache_litter(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const TRANSIENT_SUFFIXES: &[&str] = &[
        ".tmp.mkv",          // interrupted embed pass (chapters/thumbnail/subs)
        ".chapters.ffmeta.txt",
        ".ffmeta.txt",
        ".progress.log",     // ffmpeg_job restart-survival progress file
        ".thumbnail.jpg",
        ".part",             // yt-dlp SABR per-format piece, e.g. `.sq0.part`/`.sgN.part`
        ".state",            // yt-dlp SABR resume state
        ".ytdl",             // yt-dlp resume metadata
    ];
    TRANSIENT_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

/// Best-effort growing working-dir file candidates for a still-recording take,
/// keyed off its predicted final path's stem (mirrors how `build_plan`
/// derives `capture_path`, without needing to re-derive SABR/container state
/// — the caller just probes each candidate and uses whichever exists). Used
/// by `Supervisor::manual_head_backfill` for a take that's still active.
/// Every byte this take currently has sitting in the capture cache.
///
/// [`live_capture_candidates`] only names `{stem}.ts` and `{stem}.mkv`,
/// because those are what a take is *promoted* from. That is the wrong shape
/// for accounting: a yt-dlp SABR capture that dies mid-stream leaves
/// `{stem}.f303.mkv.sq0.part` (video) and `{stem}.f140.mkv` (audio), which
/// match neither name — so a take that wrote 5 GB was recorded as 0 bytes and
/// its disk usage was invisible everywhere in the app. One real archive had
/// **826 GB** of capture cache accounted for nowhere.
///
/// So this globs the cache directory for anything sharing the take's stem —
/// media, partials, thumbnails, `.state`, playlists. Everything the take left
/// behind counts, because all of it is occupying the disk, which is the
/// question the number exists to answer.
pub(super) async fn cache_leftover_bytes(final_path: &Path) -> u64 {
    let (Some(dir), Some(stem)) =
        (final_path.parent(), final_path.file_stem().map(|s| s.to_string_lossy().into_owned()))
    else {
        return 0;
    };
    let mut total = 0u64;
    for cache in cache_dir_candidates(dir) {
        let Ok(mut entries) = crate::iomon::fs::read_dir(Cat::FsProbe, &cache).await else {
            continue;
        };
        while let Ok(Some(e)) = entries.next_entry().await {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // `starts_with` on the stem, so `<stem>.f303.mkv.sq0.part` counts
            // while a DIFFERENT take that merely shares a title prefix does
            // not — stems carry the take's own timestamp.
            if !name.starts_with(&stem) {
                continue;
            }
            if let Ok(md) = crate::iomon::fs::metadata(Cat::FsProbe, &e.path()).await {
                total += md.len();
            }
        }
    }
    total
}

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

/// Settings key: `"0"` disables dropping a leftover working-dir capture whose
/// finished archive copy has been verified (default on). The kill switch
/// exists because this is the ONE sweep rule allowed to remove a real capture
/// — see [`is_sweepable_cache_litter`] for the incident that made everything
/// else name-allowlisted.
pub const K_CACHE_DROP_REDUNDANT: &str = "cache_drop_redundant_captures";

pub(super) fn cache_drop_redundant_enabled(store: &Store) -> bool {
    store.get_setting(K_CACHE_DROP_REDUNDANT).ok().flatten().as_deref() != Some("0")
}

/// A cache capture's stem maps to the take that superseded it. Built from both
/// `output_path` and `full_path` because a joined take's `output_path` may
/// have been re-pointed at `{stem}.full.mkv`, whose file stem (`{stem}.full`)
/// no longer matches the `{stem}.ts` still sitting in the working dir.
fn final_paths_by_capture_stem(store: &Store) -> HashMap<String, (i64, PathBuf)> {
    let mut map: HashMap<String, (i64, PathBuf)> = HashMap::new();
    for (rec_id, output_path, full_path) in store.finished_takes_final_paths().unwrap_or_default() {
        // The take's current best copy: after a `Both` cleanup that IS the
        // full, and `output_path` has already been re-pointed at it.
        let final_p = PathBuf::from(&output_path);
        for p in [&output_path, &full_path] {
            let Some(stem) = Path::new(p).file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            for key in [stem, stem.trim_end_matches(".full")] {
                if !key.is_empty() {
                    map.entry(key.to_string()).or_insert_with(|| (rec_id, final_p.clone()));
                }
            }
        }
    }
    map
}

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
    /// [`CACHE_MAX_AGE_SECS`]), skipping any stem currently being resumed, PLUS
    /// any orphaned `{stem}.tmp.mkv`/`{stem}.chapters.ffmeta.txt` left directly
    /// in the output dir itself by an embed-thumbnail/embed-subs/embed-chapters
    /// pass killed mid-flight (app crash, forced quit, power loss) — those
    /// never get renamed over the real file (only on success) and live
    /// OUTSIDE `.cache\`, so nothing else ever notices or removes them.
    ///
    /// Only deletes names matching [`is_sweepable_cache_litter`]'s allowlist of
    /// known tool byproducts — a real capture (`.ts`/`.mkv`/a bare per-format
    /// download) is never touched by age alone, however stale-looking. This
    /// used to be an unconditional "any file, any age" sweep, which destroyed
    /// the only complete copy of a recording once (see that function's doc
    /// comment).
    /// Removes a `.cache\` dir that ends up empty. Best-effort; runs once at
    /// startup.
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
        let mut orphan_tmp_removed = 0u32;
        // Lookup for the one rule allowed to remove a real capture (below):
        // "this take finished and its archive copy is verifiably at least as
        // long". Built once — it's a single query plus string work.
        let drop_redundant = cache_drop_redundant_enabled(&self.store);
        let finals =
            if drop_redundant { final_paths_by_capture_stem(&self.store) } else { HashMap::new() };
        for d in dirs {
            let out_dir = Path::new(&d);
            if let Ok(mut rd) = crate::iomon::fs::read_dir(Cat::CacheSweep, out_dir).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !(name.ends_with(".tmp.mkv") || name.ends_with(".chapters.ffmeta.txt")) {
                        continue;
                    }
                    let Ok(meta) = entry.metadata().await else { continue };
                    let stale = meta
                        .modified()
                        .ok()
                        .and_then(|m| now.duration_since(m).ok())
                        .map(|age| age.as_secs() >= CACHE_MAX_AGE_SECS)
                        .unwrap_or(false);
                    if stale
                        && meta.is_file()
                        && crate::iomon::fs::remove_file(Cat::CacheSweep, entry.path()).await.is_ok()
                    {
                        orphan_tmp_removed += 1;
                    }
                }
            }

            // Both the current working dir and the legacy `.cache\` — the
            // legacy one only drains (nothing writes there anymore) and its
            // empty husk is removed below, ending backup-tool churn on it.
            for cache in cache_dir_candidates(out_dir) {
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
                    if !stale || !meta.is_file() {
                        continue;
                    }
                    if !is_sweepable_cache_litter(&name) {
                        // Not a recognized transient pattern — a real capture.
                        // Age alone must never remove one (that mistake cost
                        // 7.7h of footage once); the only way out is a
                        // verified archive copy.
                        if drop_redundant {
                            self.drop_if_superseded(&entry.path(), &name, &finals).await;
                        }
                        continue;
                    }
                    if crate::iomon::fs::remove_file(Cat::CacheSweep, entry.path()).await.is_ok() {
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
        if orphan_tmp_removed > 0 {
            info!(
                "capture-cache sweep: deleted {orphan_tmp_removed} orphaned embed-pass temp \
                 file(s) from output dirs (interrupted thumbnail/subtitle/chapters embed, \
                 older than {}h; the real recording is never touched by these — only its own \
                 rename-on-success sidecar)",
                CACHE_MAX_AGE_SECS / 3600,
            );
        }
    }

    /// Drop a stale working-dir capture **only** once its finished archive copy
    /// is proven to hold at least as much footage.
    ///
    /// This is the single exception to "the sweep never removes a real
    /// capture". Promotion normally moves the capture out of the working dir,
    /// but a crash, a failed remux, or a re-attach can leave the original
    /// behind — after which nothing ever cleans it up, because age alone isn't
    /// evidence (a stale-looking `.ts` was once the only complete copy of a
    /// recording, which is why [`is_sweepable_cache_litter`] is an allowlist).
    /// 95 GB accumulated that way on one drive before this existed.
    ///
    /// The proof required is deliberately narrow:
    /// 1. the stem maps to a take that has **finished** (in-flight captures
    ///    aren't in the map at all), and
    /// 2. that take's final file exists, and
    /// 3. ffprobe says it is **at least as long** as the cache copy.
    ///
    /// Anything unprovable — no matching take, missing final, either probe
    /// failing — leaves the file exactly where it is. Removal goes through
    /// [`crate::disposal::dispose_media`] rather than a plain delete: this is
    /// real footage, so it stays recoverable from the Trash view.
    async fn drop_if_superseded(
        &self,
        path: &Path,
        name: &str,
        finals: &HashMap<String, (i64, PathBuf)>,
    ) {
        // `{stem}.ts` / `{stem}.mkv` only — a per-format SABR piece
        // (`{stem}.f303.mkv`) is a fragment, not a playable capture, and its
        // duration says nothing about completeness.
        let lower = name.to_ascii_lowercase();
        if !(lower.ends_with(".ts") || lower.ends_with(".mkv")) {
            return;
        }
        let Some(stem) = Path::new(name).file_stem().and_then(|s| s.to_str()) else {
            return;
        };
        if stem.rsplit('.').next().is_some_and(|last| {
            last.starts_with('f') && last[1..].chars().all(|c| c.is_ascii_digit()) && last.len() > 1
        }) {
            return; // `{stem}.f303.mkv` and friends
        }
        let Some((rec_id, final_p)) = finals.get(stem) else {
            return; // no finished take claims this capture — leave it alone
        };
        if crate::iomon::fs::metadata(Cat::CacheSweep, final_p).await.is_err() {
            return; // the archive copy isn't there; this may be all that's left
        }
        let (Some(final_d), Some(cache_d)) =
            (media_duration_secs(final_p).await, media_duration_secs(path).await)
        else {
            debug!(rec_id = *rec_id, "cache sweep: could not probe both copies of {name} — kept");
            return;
        };
        // The archive copy may legitimately be LONGER (a head+live join), never
        // meaningfully shorter. A truncated final is exactly the botched
        // promotion this gate exists to catch.
        if final_d <= 0 || cache_d <= 0 || final_d + 5 < cache_d {
            warn!(
                rec_id = *rec_id,
                final_d,
                cache_d,
                "cache sweep: {name}'s archive copy is SHORTER than the working-dir capture — \
                 keeping the capture (the promoted file may be truncated)"
            );
            return;
        }
        let scope = self
            .store
            .get_recording(*rec_id)
            .ok()
            .flatten()
            .and_then(|r| self.store.get_monitor_with_channel(r.monitor_id).ok().flatten())
            .map(|mw| (mw.channel.id, mw.monitor.id));
        let Some((channel_id, monitor_id)) = scope else {
            return;
        };
        match crate::disposal::dispose_media(
            &self.store,
            channel_id,
            monitor_id,
            path,
            *rec_id,
            "cache sweep: superseded working-dir capture",
        )
        .await
        {
            Ok(d) => info!(
                rec_id = *rec_id,
                "cache sweep: working-dir capture {name} ({cache_d}s) {} — the archive copy \
                 ({final_d}s) supersedes it",
                d.describe()
            ),
            Err(e) => warn!(rec_id = *rec_id, "cache sweep: disposing {name} failed: {e:#} (kept)"),
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

    /// A take that dies mid-capture leaves yt-dlp's SABR halves behind, and
    /// they match neither `{stem}.ts` nor `{stem}.mkv` — which is exactly why
    /// their gigabytes were recorded as zero. The stem prefix has to catch
    /// them, without catching a different take that merely shares a title.
    #[tokio::test]
    async fn cache_leftovers_count_sabr_partials_but_not_a_neighbouring_take() {
        #![allow(clippy::disallowed_methods)]
        let dir = std::env::temp_dir().join(format!("sa_leftover_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = dir.join(CACHE_DIR_NAME);
        std::fs::create_dir_all(&cache).unwrap();

        let stem = "chan - 2026-08-04 18-52-08 - Title [YouTube AAA]";
        // What a dead SABR capture actually leaves.
        std::fs::write(cache.join(format!("{stem}.f303.mkv.sq0.part")), vec![0u8; 5000]).unwrap();
        std::fs::write(cache.join(format!("{stem}.f140.mkv")), vec![0u8; 700]).unwrap();
        std::fs::write(cache.join(format!("{stem}.thumbnail.jpg")), vec![0u8; 30]).unwrap();
        // A DIFFERENT take of the same stream: same title, different timestamp.
        // Stems carry the take's own time, so this must not be counted.
        std::fs::write(
            cache.join("chan - 2026-08-04 19-30-00 - Title [YouTube AAA].f303.mkv.sq0.part"),
            vec![0u8; 900_000],
        )
        .unwrap();

        let final_path = dir.join(format!("{stem}.mkv"));
        let got = cache_leftover_bytes(&final_path).await;
        assert_eq!(got, 5000 + 700 + 30, "the take's own leftovers, and only those");

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn sweepable_cache_litter_never_matches_a_real_capture() {
        // Recognized tool byproducts: safe to age-sweep.
        for name in [
            "Show.tmp.mkv",
            "Show.chapters.ffmeta.txt",
            "Show.ffmeta.txt",
            "Show.chapters.progress.log",
            "Show.thumbnail.jpg",
            "Show.mkv.f140.mp4.sq0.part",
            "Show.mkv.f140.mp4.state",
            "Show.mkv.f140.mp4.ytdl",
        ] {
            assert!(is_sweepable_cache_litter(name), "expected {name} to be sweepable");
        }
        // A real capture, however it's named — including a bare per-format
        // download whose merge never finished — must never match, however
        // stale-looking: the exact class of file a 2026-07 incident lost
        // ~7.7h of footage by deleting.
        for name in [
            "Show - 2026-07-17 19-26-54 - title-tba [games-tba] (1080p60 live h264 aac) - [twitch 319550633312].ts",
            "Show.mkv",
            "Show.mkv.f140.mp4",
            "Show.webm",
        ] {
            assert!(!is_sweepable_cache_litter(name), "expected {name} to be protected");
        }
    }

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
