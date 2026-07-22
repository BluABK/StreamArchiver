//! Finalizing captures: promotion out of the cache, companion moves,
//! orphan reconcile, renames/reorganization, and live-capture watchers.

use super::*;

/// Reorganize the files for one recording according to `cfg`. When `reverse` is
/// true, moves files from subdirs back to the base output dir.
/// Returns the new `output_path` for the video file, or the original if unchanged.
pub async fn reorganize_recording_files(
    rec_id: i64,
    store: &std::sync::Arc<crate::store::Store>,
    cfg: &crate::models::SubdirConfig,
    reverse: bool,
) -> anyhow::Result<Option<String>> {
    let (mid, output_path) = match store.get_recording_paths(rec_id)? {
        Some(v) => v,
        None => return Ok(None),
    };
    if output_path.is_empty() {
        return Ok(None);
    }
    let current = PathBuf::from(&output_path);

    // Fetch base_dir unconditionally — we need it even when the video is gone,
    // to sort any companion files (chat logs, thumbnails) stranded in the root.
    let (output_dir, _) = match store.get_monitor_output_dir(mid)? {
        Some(v) => v,
        None => return Ok(None),
    };
    let base_dir = PathBuf::from(&output_dir);

    if !crate::iomon::fs::exists_sync(Cat::Promote, &current) {
        // Video file is gone (failed recording, external move, etc.).
        // Still try to sort companion files in the directory the video was supposed to land in.
        if cfg.enabled && !reverse {
            if let Some(s) = current.file_stem().and_then(|s| s.to_str()) {
                let from_dir = current.parent().map(PathBuf::from).unwrap_or_else(|| base_dir.clone());
                move_companions_to_subdirs(&from_dir, &base_dir, s, cfg).await;
            }
        }
        return Ok(None);
    }

    let ext = current.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let is_mkv = ext == "mkv" || ext == "mp4" || ext == "ts";

    // Extract stem early so both the "already in place" path and the move path can use it.
    let stem = match current.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(None),
    };

    let target_dir = if reverse {
        base_dir.clone()
    } else if is_mkv && cfg.enabled {
        base_dir.join(&cfg.videos)
    } else {
        return Ok(None); // nothing to move
    };

    if target_dir == current.parent().unwrap_or(&base_dir) {
        // Video is already in the right place. Still scan base_dir for any companion
        // files that were left behind by a previous run (e.g. because the extension
        // wasn't handled at the time).
        if cfg.enabled && !reverse {
            move_companions_to_subdirs(&base_dir, &base_dir, &stem, cfg).await;
        }
        return Ok(None);
    }

    if let Err(e) = crate::iomon::fs::create_dir_all(Cat::Promote, &target_dir).await {
        anyhow::bail!("create_dir_all {:?}: {e:#}", target_dir);
    }
    let file_name = match current.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return Ok(None),
    };
    let new_video_path = target_dir.join(&file_name);
    crate::iomon::fs::rename(Cat::Promote, &current, &new_video_path).await?;

    // Move companion files (subs, thumbnail, chat) to their own dirs.
    if cfg.enabled && !reverse {
        let from_dir = current.parent().unwrap_or(&base_dir);
        move_companions_to_subdirs(from_dir, &base_dir, &stem, cfg).await;
    } else if reverse {
        // Collapse all companions from sub-dirs back to base_dir.
        let dirs = [&cfg.videos, &cfg.subs, &cfg.chat, &cfg.thumbs, &cfg.logs];
        for sub in &dirs {
            let sub_dir = base_dir.join(sub);
            move_companions(&sub_dir, &base_dir, &stem).await;
            // Try to remove the empty sub-dir (best effort; only our dirs).
            let _ = crate::iomon::fs::remove_dir(Cat::Promote, &sub_dir).await;
        }
    } else {
        // Normal companion move: keep everything together with the video.
        if let Some(from_dir) = current.parent() {
            move_companions(from_dir, &target_dir, &stem).await;
        }
    }

    let new_path_str = new_video_path.to_string_lossy().into_owned();
    store.update_recording_output_path(rec_id, &new_path_str)?;
    Ok(Some(new_path_str))
}

/// Move companion files (subs, thumbnail, chat log) to the appropriate sub-dirs
/// based on `cfg`. Best-effort — skips files that can't be moved.
pub(super) async fn move_companions_to_subdirs(from_dir: &Path, base_dir: &Path, stem: &str, cfg: &crate::models::SubdirConfig) {
    let prefix = format!("{stem}.");
    let mut rd = match crate::iomon::fs::read_dir(Cat::Promote, from_dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        let rest = &name[prefix.len()..];
        let target_sub = if rest.ends_with("chat.jsonl") || rest.ends_with("live_chat.json") || rest.ends_with("chat.log") {
            Some(&cfg.chat)
        } else if rest.ends_with("thumbnail.jpg") || rest.ends_with("thumbnail.webp") {
            Some(&cfg.thumbs)
        } else {
            let lower = rest.to_ascii_lowercase();
            let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or(&lower);
            if SUBTITLE_EXTS.contains(&ext) {
                Some(&cfg.subs)
            } else if THUMBNAIL_EXTS.contains(&ext) {
                Some(&cfg.thumbs)
            } else {
                None
            }
        };
        if let Some(sub) = target_sub {
            let target_dir = base_dir.join(sub);
            let _ = crate::iomon::fs::create_dir_all(Cat::Promote, &target_dir).await;
            let dst = target_dir.join(&name);
            let _ = crate::iomon::fs::rename(Cat::Promote, entry.path(), dst).await;
        }
    }
}

/// Sweep every file directly in `dir` (non-recursive) and move companion files
/// (chat logs, thumbnails, subtitles) into their configured subdirectories.
/// This catches files that aren't linked to any recording in the database
/// (e.g. chat logs from recordings that ended with no output_path).
/// Video/part files are ignored — only known companion extensions are moved.
pub(crate) async fn sweep_companion_files(dir: &Path, cfg: &crate::models::SubdirConfig) {
    if !cfg.enabled {
        return;
    }
    let mut rd = match crate::iomon::fs::read_dir(Cat::Promote, dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(true) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        let target_sub = if lower.ends_with(".chat.log")
            || lower.ends_with(".chat.jsonl")
            || lower.ends_with(".live_chat.json")
        {
            Some(&cfg.chat)
        } else if lower.ends_with(".thumbnail.jpg") || lower.ends_with(".thumbnail.webp") {
            Some(&cfg.thumbs)
        } else {
            let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or("");
            if SUBTITLE_EXTS.contains(&ext) {
                Some(&cfg.subs)
            } else if THUMBNAIL_EXTS.contains(&ext) {
                Some(&cfg.thumbs)
            } else {
                None
            }
        };
        if let Some(sub) = target_sub {
            let target_dir = dir.join(sub);
            let _ = crate::iomon::fs::create_dir_all(Cat::Promote, &target_dir).await;
            let dst = target_dir.join(&name);
            let _ = crate::iomon::fs::rename(Cat::Promote, entry.path(), dst).await;
        }
    }
}

/// Rename a recording's output file (and its companions) to a new stem.
/// Updates `recording.output_path` in the database.
/// Returns the new path string, or None if the recording has no output path.
pub async fn rename_recording_files(
    rec_id: i64,
    store: &std::sync::Arc<crate::store::Store>,
    new_stem: &str,
) -> anyhow::Result<Option<String>> {
    let (_, output_path) = match store.get_recording_paths(rec_id)? {
        Some(v) => v,
        None => return Ok(None),
    };
    if output_path.is_empty() {
        return Ok(None);
    }
    let current = PathBuf::from(&output_path);
    if !crate::iomon::fs::exists_sync(Cat::Promote, &current) {
        anyhow::bail!("output file not found: {}", current.display());
    }
    let dir = match current.parent() {
        Some(d) => d.to_path_buf(),
        None => return Ok(None),
    };
    let ext = current.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
    let old_stem = match current.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(None),
    };

    // Sanitize the new stem (same as capture filenames).
    let new_stem_clean = sanitize_filename(new_stem);
    if new_stem_clean.is_empty() || new_stem_clean == old_stem {
        return Ok(None);
    }

    let new_file = dir.join(format!("{new_stem_clean}.{ext}"));
    crate::iomon::fs::rename(Cat::Promote, &current, &new_file).await?;

    // Rename companion files.
    let prefix_old = format!("{old_stem}.");
    let mut rd = match crate::iomon::fs::read_dir(Cat::Promote, &dir).await {
        Ok(rd) => rd,
        Err(_) => {
            let new_path = new_file.to_string_lossy().into_owned();
            store.update_recording_output_path(rec_id, &new_path)?;
            return Ok(Some(new_path));
        }
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == new_file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() {
            continue;
        }
        if let Some(rest) = name.strip_prefix(&prefix_old) {
            if is_companion_suffix(rest) {
                let new_name = format!("{new_stem_clean}.{rest}");
                let dst = dir.join(&new_name);
                let _ = crate::iomon::fs::rename(Cat::Promote, entry.path(), dst).await;
            }
        }
    }

    let new_path = new_file.to_string_lossy().into_owned();
    store.update_recording_output_path(rec_id, &new_path)?;
    Ok(Some(new_path))
}
/// Rename a finished capture to `new_stem` (keeping its extension), avoiding
/// collisions. Returns the resulting path (unchanged on no-op or failure).
pub(super) async fn rename_for_media(final_path: PathBuf, new_stem: &str) -> PathBuf {
    let Some(dir) = final_path.parent().map(Path::to_path_buf) else {
        return final_path;
    };
    let ext = final_path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mkv".into());
    // Ignore the file we're renaming when checking collisions, so a "Both"-mode
    // capture whose pre-probe name already matches (incl. a build-time collision
    // suffix) resolves back to its own name and we no-op below.
    let unique = unique_stem(&dir, new_stem, &ext, Some(&final_path));
    let new_path = dir.join(format!("{unique}.{ext}"));
    if new_path == final_path {
        return final_path; // already correctly named (e.g. no media, or "Both")
    }
    let old_stem = final_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned());
    match rename_or_shorten(&final_path, &dir, &unique, &ext).await {
        Ok(actual) => {
            // Move subtitle / chat sidecars (e.g. `{stem}.en.vtt`,
            // `{stem}.chat.jsonl`, `{stem}.live_chat.json`) so they stay matched
            // to the renamed video instead of orphaning under the old stem.
            // Follow with the video's ACTUAL stem (rename_or_shorten may have
            // had to shorten it further) so sidecars never mismatch it.
            if let Some(old) = old_stem {
                let actual_stem = actual
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| unique.clone());
                rename_companion_sidecars(&dir, &old, &actual_stem).await;
            }
            actual
        }
        Err(e) => {
            warn!("media rename failed, keeping {}: {e:#}", final_path.display());
            final_path
        }
    }
}

/// Find a thumbnail sidecar that lives alongside `src` in the same directory.
///
/// Checked in priority order:
/// 1. `{stem}.thumbnail.jpg` — written by our HTTP fetch
/// 2. `{stem}.webp` / `{stem}.jpg` / `{stem}.png` — written by yt-dlp `--write-thumbnail`
///
/// Returns the first match found, or `None` if no thumbnail exists yet.
pub(super) fn find_thumbnail_for(src: &Path) -> Option<PathBuf> {
    let dir = src.parent()?;
    let stem = src.file_stem()?.to_string_lossy();
    for suffix in &["thumbnail.jpg", "webp", "jpg", "png", "jpeg"] {
        let candidate = dir.join(format!("{stem}.{suffix}"));
        if crate::iomon::fs::exists_sync(Cat::Thumbnail, &candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Bare per-format media files (`{stem}.….fN.….{mp4,m4a,webm,mkv}`) left in
/// `.cache\` for a recording — the SABR/DASH downloader writes per-format
/// files and merges them at the very end; if it dies first, the media
/// survives as finished parts while the predicted capture path never exists
/// (so finalize records 0 bytes and the take reads as "gone"). Mirrors the
/// naming rules of the UI's active-capture split scan, but accepts only
/// FINISHED parts (no `.part`/`.temp.`/`.ytdl`/`.state`/`.log`).
///
/// `capture` may point into a working dir (a never-promoted take) or at the
/// promoted location — the scan runs in the working dir(s) next to either
/// (current name and legacy `.cache\`; pre-rename parts live in the latter).
pub fn find_split_media(capture: &Path) -> Vec<PathBuf> {
    let Some(dir) = capture.parent() else { return Vec::new() };
    let Some(stem) = capture.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    let caches: Vec<PathBuf> = if strip_cache_component(capture).is_some() {
        vec![dir.to_path_buf()] // already inside a working dir — scan right there
    } else {
        cache_dir_candidates(dir)
    };
    let prefix = format!("{stem}.");
    let is_fmt_seg = |s: &str| {
        s.strip_prefix('f')
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut out = Vec::new();
    for cache in &caches {
        let Ok(rd) = crate::iomon::fs::read_dir_sync(Cat::FsProbe, cache) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix(&prefix) else { continue };
            if rest.contains(".part")
                || rest.contains(".temp.")
                || rest.ends_with(".ytdl")
                || rest.ends_with(".state")
                || rest.ends_with(".log")
            {
                continue;
            }
            let segs: Vec<&str> = rest.split('.').collect();
            let Some(fpos) = segs.iter().position(|s| is_fmt_seg(s)) else { continue };
            // Everything before the f<id> segment must be container decoration
            // ("mkv.f140.mp4"), never title text — otherwise a sibling recording
            // whose stem extends this one after a dot would leak in.
            if !segs[..fpos].iter().all(|s| matches!(*s, "mkv" | "mp4" | "webm" | "m4a" | "ts")) {
                continue;
            }
            if matches!(segs.last().copied(), Some("mp4" | "m4a" | "webm" | "mkv")) {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

/// Like [`find_split_media`], but for a take whose tool DIED mid-write: also
/// accept the unfinished `.part` working files (`{stem}.f303.mkv.sq0.part`,
/// `{stem}.mkv.f140.mp4.sq62.part` — both dev-build name orderings occur).
/// SABR restarts can leave several `sq<N>` sequences of the SAME format;
/// concatenating them would repeat or skip content, so only the LARGEST file
/// per format id is returned. Only meaningful for dead takes — a live
/// capture's parts are still growing.
pub fn find_split_parts(capture: &Path) -> Vec<PathBuf> {
    let Some(dir) = capture.parent() else { return Vec::new() };
    let Some(stem) = capture.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    let caches: Vec<PathBuf> = if strip_cache_component(capture).is_some() {
        vec![dir.to_path_buf()]
    } else {
        cache_dir_candidates(dir)
    };
    let is_fmt_seg = |s: &str| {
        s.strip_prefix('f')
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    };
    let is_sq_seg = |s: &str| {
        s.strip_prefix("sq")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    };
    let prefix = format!("{stem}.");
    // format id -> (bytes, path): keep the largest sequence per format.
    let mut best: std::collections::HashMap<String, (u64, PathBuf)> =
        std::collections::HashMap::new();
    for cache in &caches {
        let Ok(rd) = crate::iomon::fs::read_dir_sync(Cat::FsProbe, cache) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix(&prefix) else { continue };
            if rest.contains(".temp.") {
                continue;
            }
            let segs: Vec<&str> = rest.split('.').collect();
            // Shape: [container decoration…] f<id> <container> [sq<N>] part
            if segs.last().copied() != Some("part") {
                continue;
            }
            let Some(fpos) = segs.iter().position(|s| is_fmt_seg(s)) else { continue };
            if !segs[..fpos].iter().all(|s| matches!(*s, "mkv" | "mp4" | "webm" | "m4a" | "ts")) {
                continue;
            }
            let mid = &segs[fpos + 1..segs.len() - 1];
            let ok = match mid {
                [ext] => matches!(*ext, "mkv" | "mp4" | "webm" | "m4a" | "ts"),
                [ext, sq] => {
                    matches!(*ext, "mkv" | "mp4" | "webm" | "m4a" | "ts") && is_sq_seg(sq)
                }
                _ => false,
            };
            if !ok {
                continue;
            }
            let bytes = crate::iomon::fs::metadata_sync(Cat::FsProbe, entry.path())
                .map(|m| m.len())
                .unwrap_or(0);
            if bytes == 0 {
                continue;
            }
            let key = segs[fpos].to_string();
            match best.get(&key) {
                Some((b, _)) if *b >= bytes => {}
                _ => {
                    best.insert(key, (bytes, entry.path()));
                }
            }
        }
    }
    let mut out: Vec<PathBuf> = best.into_values().map(|(_, p)| p).collect();
    out.sort();
    out
}

/// Newest handle-true mtime (unix secs) across a recording's output file and
/// every capture-stem working file in its cache dir(s). Handle-based because
/// NTFS directory entries can be stale for files that are open for writing —
/// the file must be opened for the truth. Sync and it stats the recordings
/// drive: call it off the UI thread only (the Issues scan thread).
pub fn latest_capture_activity(output_path: &str) -> Option<i64> {
    fn open_mtime(p: &Path) -> Option<i64> {
        let md = crate::iomon::fs::open_sync(Cat::FsProbe, p).ok()?.metadata().ok()?;
        if !md.is_file() {
            return None;
        }
        md.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
    }
    let p = Path::new(output_path);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned())?;
    let mut newest = open_mtime(p);
    // Working dir(s): the file's own dir when it already points into a cache,
    // otherwise every cache-layout candidate next to the output dir.
    let dirs: Vec<PathBuf> = if strip_cache_component(p).is_some() {
        p.parent().map(|d| vec![d.to_path_buf()]).unwrap_or_default()
    } else {
        p.parent().map(cache_dir_candidates).unwrap_or_default()
    };
    let prefix = format!("{stem}.");
    for dir in &dirs {
        let Ok(rd) = crate::iomon::fs::read_dir_sync(Cat::FsProbe, dir) else { continue };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) {
                continue;
            }
            if let Some(mt) = open_mtime(&entry.path())
                && newest.is_none_or(|n| mt > n)
            {
                newest = Some(mt);
            }
        }
    }
    newest
}

/// Collect subtitle sidecar files adjacent to `src` (e.g. `{stem}.en.srt`,
/// `{stem}.vtt`, `{stem}.ass`). Used by `remux_ts_to_mkv` when `embed_subs` is on.
pub(super) fn collect_subtitle_sidecars(src: &Path) -> Vec<PathBuf> {
    let dir = match src.parent() { Some(d) => d, None => return Vec::new() };
    let stem = match src.file_stem() { Some(s) => s.to_string_lossy().into_owned(), None => return Vec::new() };
    let prefix = format!("{stem}.");
    let mut subs = Vec::new();
    if let Ok(rd) = crate::iomon::fs::read_dir_sync(Cat::Thumbnail, dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) {
                continue;
            }
            let rest = &name[prefix.len()..];
            let lower = rest.to_ascii_lowercase();
            let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or(&lower);
            if SUBTITLE_EXTS.contains(&ext) {
                subs.push(entry.path());
            }
        }
    }
    subs.sort();
    subs
}

/// Subtitle-sidecar extensions, for companion-file moves when the video is
/// renamed (so external subs stay associated with their recording).
pub(super) const SUBTITLE_EXTS: [&str; 6] = ["vtt", "srt", "ass", "ssa", "sub", "lrc"];

/// Thumbnail extensions, so a `{stem}.thumbnail.jpg` (our HTTP fetch) or a
/// `{stem}.webp`/`.jpg`/`.png` (yt-dlp `--write-thumbnail`) is promoted/renamed
/// alongside the recording instead of being orphaned.
pub(super) const THUMBNAIL_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// True if `rest` (the part of a sibling filename after `{old_stem}.`) is a
/// recognized companion: a subtitle sidecar, a thumbnail, or a chat log
/// (`.chat.jsonl` from the Twitch logger, `.live_chat.json` from yt-dlp).
pub(super) fn is_companion_suffix(rest: &str) -> bool {
    if rest.ends_with("chat.jsonl") || rest.ends_with("live_chat.json") {
        return true;
    }
    let lower = rest.to_ascii_lowercase();
    // `rest` may be a bare extension (yt-dlp's `{stem}.webp`) or a multi-part suffix
    // (`en.vtt`, `thumbnail.jpg`) — accept either: the final extension, else `rest`.
    let ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| lower.clone());
    SUBTITLE_EXTS.contains(&ext.as_str()) || THUMBNAIL_EXTS.contains(&ext.as_str())
}

/// Where a download tool's combined stdout+stderr log lives: under the app-data
/// logs dir on the system drive, NOT next to the capture. The tool appends to
/// it continuously and the app tail-polls it 4×/s per active download — pure
/// seek churn when it sat in `.cache\` on the recordings HDD, interleaved with
/// the capture writes themselves. The detached-process registry stores this
/// absolute path, so re-attach after a restart works for both new rows and
/// pre-relocation rows still pointing into `.cache\`. Pruned after 7 days at
/// startup alongside the app logs (previously the log was purged at finalize —
/// keeping it a week is a debugging improvement, not a regression).
pub(super) fn capture_log_path(capture_path: &Path, suffix: &str) -> PathBuf {
    let dir = crate::app_paths::logs_dir().join("captures");
    let _ = crate::iomon::fs::create_dir_all_sync(Cat::ToolLog, &dir);
    // Capture file names carry channel + timestamp (+ .dash infix for the
    // companion), so appending the suffix keeps sibling legs distinct.
    let name = capture_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "capture".into());
    dir.join(format!("{name}.{suffix}"))
}

/// Persist the raw capture's first MPEG-TS PTS onto its recording row before
/// the promote remux erases it (MKV timestamps restart at ~0). It's the head
/// backfill's exact-splice anchor (`pts_capture_offset`): a manual "Backfill
/// head" on the finished take can only splice exactly if a raw-`.ts`-era value
/// survives. No-op for non-`.ts` captures (no broadcast timeline) and for rows
/// that already have a value (the in-recording head job writes it first).
pub(super) async fn persist_capture_start_pts(store: &Store, rec_id: i64, capture_path: &Path) {
    if rec_id <= 0
        || !capture_path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ts"))
        || store.recording_capture_start_pts(rec_id).ok().flatten().is_some()
    {
        return;
    }
    if let Some(pts) = media_start_time_secs(capture_path).await {
        let _ = store.set_recording_capture_start_pts(rec_id, pts);
    }
}

/// Promote a finished capture from the `.cache\` working dir up to its final path
/// in the output dir: remux (TS→MKV) or move (already-final container), deleting the
/// cache source on success. A 0-byte/failed capture is left in `.cache\` (returns
/// the capture path so the caller can tell promotion didn't happen). `opts` governs
/// the remux branch's thumbnail/title/subtitle embedding (see [`crate::models::RemuxOpts`]) —
/// callers should pass `store.remux_opts()`, not a default, or the user's Settings
/// toggles silently do nothing for this (the automatic) path.
/// `task` = `(events, task_id)`: when set, the remux branch announces itself
/// as a Background job (kind `Remux`) with live ffmpeg progress — pass the
/// recording id as `task_id` for takes so grid rows can match it. Without it
/// a finalize can sit invisibly for a long time waiting for its disk-gate
/// turn while the row still reads "recording" (the rec-652 stuck-finalize
/// incident, 2026-07-12). The quick move branch never announces.
pub(super) async fn promote_capture(
    plan: &DownloadPlan,
    opts: &crate::models::RemuxOpts,
    task: Option<(EventTx, u64)>,
) -> PathBuf {
    // yt-dlp SABR dev-build sometimes appends a container extension to the
    // output path even when the template already specifies one — e.g. writing
    // `stem.mkv.mp4` instead of `stem.mkv` because the merged container is MP4.
    // Detect this and use the `.mp4` variant as the effective capture file.
    let effective = if !plan.remux_to_mkv && file_len(&plan.capture_path).await == 0 {
        let mut os = plan.capture_path.as_os_str().to_owned();
        os.push(".mp4");
        let mp4 = PathBuf::from(os);
        if file_len(&mp4).await > 0 { mp4 } else { plan.capture_path.clone() }
    } else {
        plan.capture_path.clone()
    };

    if file_len(&effective).await == 0 {
        return plan.capture_path.clone(); // failed: leave the partial for the sweep
    }
    if plan.remux_to_mkv {
        // ffmpeg writes the destination directly — there's no OS rename to
        // react to on a name-too-long failure, so shorten proactively before
        // the write instead of reactively after it (see path_with_safe_stem).
        let dest = path_with_safe_stem(&plan.final_path);
        if let Some((tx, id)) = &task {
            let _ = tx.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
                id: *id,
                kind: crate::events::BackgroundTaskKind::Remux,
                label: effective
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                detail: "finalize: remux to MKV (may wait for the disk gate)".into(),
                started_at: now_unix(),
                progress: None,
                progress_info: None,
            }));
        }
        let res = remux_ts_to_mkv(&effective, &dest, task.clone(), opts).await;
        if let Some((tx, id)) = &task {
            let outcome = match &res {
                Ok(()) => crate::events::TaskOutcome::Completed,
                Err(e) => crate::events::TaskOutcome::Failed(e.to_string()),
            };
            let _ = tx.send(AppEvent::BackgroundTaskFinished { id: *id, outcome });
        }
        match res {
            Ok(()) => {
                let _ = crate::iomon::fs::remove_file(Cat::Promote, &effective).await;
                dest
            }
            Err(e) => {
                warn!("remux failed, keeping {}: {e:#}", effective.display());
                effective
            }
        }
    } else {
        // Already the final container — move it up to the output dir. The
        // finished recording landing here matters more than a fully-
        // descriptive name: on a too-long name, rename_or_shorten falls back
        // to a shortened one rather than leaving a completed capture stuck
        // (and, after 24h, swept as stale) in the hidden `.cache\`.
        if let Some(parent) = plan.final_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(Cat::Promote, parent).await;
        }
        let dir = plan.final_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = plan
            .final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = plan
            .final_path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        match rename_or_shorten(&effective, dir, &stem, &ext).await {
            Ok(actual) => actual,
            Err(e) => {
                warn!("promote move failed, keeping {}: {e:#}", effective.display());
                effective
            }
        }
    }
}

/// Move every recognized companion (`{stem}.*` matched by [`is_companion_suffix`] —
/// subtitles, thumbnail, in-process chat) from `from_dir` up to `to_dir`.
/// Best-effort; never clobbers an existing target.
pub(super) async fn move_companions(from_dir: &Path, to_dir: &Path, stem: &str) {
    let prefix = format!("{stem}.");
    let mut rd = match crate::iomon::fs::read_dir(Cat::Promote, from_dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        if !is_companion_suffix(rest) {
            continue;
        }
        let to = to_dir.join(&name);
        if crate::iomon::fs::exists_sync(Cat::Promote, &to) {
            continue;
        }
        match crate::iomon::fs::rename(Cat::Promote, entry.path(), &to).await {
            Ok(()) => {}
            Err(e) if is_name_too_long(&e) => {
                if let Err(e) = rename_or_shorten(&entry.path(), to_dir, stem, rest).await {
                    warn!("companion promote failed for {name}: {e:#}");
                }
            }
            Err(e) => warn!("companion promote failed for {name}: {e:#}"),
        }
    }
}

/// After promotion, delete the recording's remaining working files (`{stem}.*`) from
/// `cache` (SABR `.sq0.part`/`.state`, leftover `.ts`, chat fragments), then remove
/// the cache dir if it is now empty. Best-effort.
pub(super) async fn purge_cache(cache: &Path, stem: &str) {
    let prefix = format!("{stem}.");
    if let Ok(mut rd) = crate::iomon::fs::read_dir(Cat::CacheSweep, cache).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                let _ = crate::iomon::fs::remove_file(Cat::CacheSweep, entry.path()).await;
            }
        }
    }
    let _ = crate::iomon::fs::remove_dir(Cat::CacheSweep, cache).await; // only if empty
}

/// When the main recording file is renamed, move its companion sidecars
/// (`{old_stem}.<lang>.vtt` subtitles, `{old_stem}.chat.jsonl` /
/// `{old_stem}.live_chat.json` chat logs) to follow `new_stem`, so they don't
/// become orphaned next to a renamed video. Best-effort: per-file failures are
/// logged, not fatal; existing targets are never clobbered.
pub(super) async fn rename_companion_sidecars(dir: &Path, old_stem: &str, new_stem: &str) {
    if old_stem == new_stem || old_stem.is_empty() {
        return;
    }
    let prefix = format!("{old_stem}.");
    let mut rd = match crate::iomon::fs::read_dir(Cat::Promote, dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        if !is_companion_suffix(rest) {
            continue;
        }
        let to = dir.join(format!("{new_stem}.{rest}"));
        if crate::iomon::fs::exists_sync(Cat::Promote, &to) {
            continue; // don't clobber an unrelated existing file
        }
        // Retry with exponential backoff: on Windows the chat downloader
        // (yt-dlp) may still have live_chat.json open when finalization runs,
        // producing os error 32 (ERROR_SHARING_VIOLATION). Give the process
        // time to flush and release the handle before giving up.
        let src = entry.path();
        let mut delay_ms = 500u64;
        let mut last_err: Option<std::io::Error> = None;
        let mut renamed = false;
        for attempt in 0u32..5 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms *= 2; // 500 → 1000 → 2000 → 4000 ms
            }
            match crate::iomon::fs::rename(Cat::Promote, &src, &to).await {
                Ok(()) => { last_err = None; renamed = true; break; }
                Err(e) if e.raw_os_error() == Some(32)  // Windows: SHARING_VIOLATION
                       || e.raw_os_error() == Some(16)  // Unix: EBUSY
                => { last_err = Some(e); }
                Err(e) => { last_err = Some(e); break; } // non-retryable error
            }
        }
        if renamed {
            continue;
        }
        // The name itself may be the problem (most commonly NTFS's
        // 255-UTF-16-unit-per-component limit — see `MAX_STEM_UTF16_LEN`).
        // Following the video's rename matters more than a fully-descriptive
        // sidecar name, so shorten `new_stem` (never touching `rest`, which
        // identifies the sidecar's role) and retry, rather than leaving this
        // companion permanently orphaned under its old name.
        if last_err.as_ref().is_some_and(is_name_too_long) {
            match rename_or_shorten(&src, dir, new_stem, rest).await {
                Ok(_) => continue,
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(e) = last_err {
            warn!("companion sidecar rename failed for {}: {e:#}", name);
        }
    }
}
/// Watch a from-start recording's growing capture and zero its "lost time" once
/// the captured media catches up to the live edge. Exits early when `done` is set
/// (recording ended) so finalize can compute the exact residual without a race.
#[allow(clippy::too_many_arguments)]
pub(super) async fn catch_up_watcher(
    store: Arc<Store>,
    events: EventTx,
    monitor_id: i64,
    platform: crate::models::Platform,
    rec_id: i64,
    capture_path: PathBuf,
    went_live: i64,
    done: Arc<AtomicBool>,
) {
    // Each probe is a fresh ffprobe over the growing multi-GB capture (header
    // read + tail seeks) on the recordings drive, so the cadence adapts: the
    // measured deficit bounds how soon catch-up could possibly happen — a
    // capture 2h behind can't catch up in the next 20s, so probing it every
    // 20s is pure wasted reads. Only the cosmetic lost-time flip is delayed.
    let mut interval_secs = CATCHUP_PROBE_INTERVAL_SECS as i64;
    const MAX_PROBE_INTERVAL_SECS: i64 = 180;
    loop {
        // Interruptible wait between probes (checks `done` every 250ms).
        for _ in 0..(interval_secs * 4) {
            if done.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if done.load(Ordering::SeqCst) {
            return;
        }
        if let Some(captured) = media_duration_secs(&capture_path).await {
            let elapsed = now_unix() - went_live;
            if captured + CATCHUP_TOLERANCE_SECS >= elapsed {
                let _ = store.set_recording_lost_secs(rec_id, 0);
                info!(
                    rec_id,
                    "from-start capture caught up with live {} (lost time = 0)",
                    platform.tag()
                );
                // Wake the UI so an already-expanded history tree refreshes the
                // Lost-time column from the new value.
                let _ = events.send(AppEvent::MonitorState {
                    monitor_id,
                    state: "recording".into(),
                });
                return;
            }
            let deficit = elapsed - captured;
            interval_secs =
                (deficit / 4).clamp(CATCHUP_PROBE_INTERVAL_SECS as i64, MAX_PROBE_INTERVAL_SECS);
        } else {
            // Growing TS refused a duration (documented ffprobe behavior early
            // on) — ease off gradually rather than re-paying the failed read
            // every base interval.
            interval_secs = (interval_secs * 2).min(MAX_PROBE_INTERVAL_SECS);
        }
    }
}

/// How often to poll a stream's title/category for changes during a recording
/// (Twitch Helix, Kick v2 JSON, or the YouTube `/live` page). Changes are
/// infrequent and not time-critical for an archive log, so a coarse interval
/// keeps the cost low — one request per active recording (the YouTube path
/// fetches the full watch page; the others a small JSON response).
pub(super) const META_POLL_INTERVAL_SECS: i64 = 60;

/// Poll a live channel's title + game/category for the duration of a recording,
/// logging each change to `stream_meta_change`. The metadata source is chosen by
/// `platform`: Twitch via Helix, Kick via its v2 channel JSON, YouTube by
/// scraping the `/live` page (its `game` is the broad content category, as
/// YouTube has no per-stream game field). The first observed value of each field
/// is logged as the baseline (empty `old_value`); later transitions record
/// `old -> new` (including a change to empty, e.g. a cleared category). Stops
/// when `done` (recording ended) or `shutdown` is set. No-ops gracefully when
/// the source answers offline or is unavailable (see [`MetaFetch`]
/// (crate::detectors::MetaFetch) — only `Failed` counts as a broken refresh).
///
/// Also refreshes `monitor.last_viewers` every cycle (Twitch/Kick exactly;
/// YouTube from the watch page's scraped "watching now"). This is the ONLY place that field
/// gets updated while a recording is active: `scheduler::tick` skips an
/// actively-recording monitor entirely (the supervisor owns its state until
/// the tool exits), so without this the Viewers column would freeze at
/// whatever it last read before the recording started (or stay unknown,
/// since a push-triggered start never had a poll outcome to read one from).
/// Unlike title/game, the count isn't logged as a "change" — it fluctuates
/// continuously, so every fetch just overwrites it directly.
/// Also enforces a trigger rule's "only recording while matching"
/// (`stop_rule`, `Some` only when the rule that started this recording has
/// `stop_on_unmatch` on): each cycle re-checks the rule against the freshly-
/// polled title/game and, once it's been unmatched for `end_delay_secs`,
/// sends `ManualCommand::Stop` — the same path the Stop button and scheduled-
/// recording auto-stop already use. The very first poll only seeds a
/// matching/non-matching baseline (mirrors the title/game baseline logic
/// below) rather than acting on it, so a slow/lagging metadata read can never
/// cause an immediate false-stop right after the trigger fires.
#[allow(clippy::too_many_arguments)]
pub(super) async fn meta_watcher(
    ctx: Arc<DetectContext>,
    store: Arc<Store>,
    events: EventTx,
    monitor_id: i64,
    rec_id: i64,
    started_at: i64,
    url: String,
    platform: Platform,
    done: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    manual_tx: mpsc::UnboundedSender<ManualCommand>,
    stop_rule: Option<crate::triggers::TriggerRule>,
    // Monitor-persisted title/game at take start — seeds the continuous,
    // all-time `monitor_stream_change` ledger (see below) separately from
    // `last_title`/`last_game`, which track this *take's* own baseline for
    // the per-recording `stream_meta_change` popup. Without this split, every
    // take start would log a spurious "changed from empty" event into the
    // continuous history even when the title genuinely hadn't changed.
    cont_title: String,
    cont_game: String,
    cont_tags: String,
) {
    let mut last_title: Option<String> = None;
    let mut last_game: Option<String> = None;
    let mut last_tags: Option<String> = None;
    let mut cont_title = cont_title;
    let mut cont_game = cont_game;
    let mut cont_tags = cont_tags;
    // "Stream Together" collab refresh inputs (Twitch only): the in-recording
    // half of the dual feed — the scheduler skips actively-recording monitors,
    // so without this the Collab column would freeze for the whole take.
    let collab_login =
        (platform == Platform::Twitch).then(|| crate::detectors::twitch_login(&url)).flatten();
    // Broadcast id for tagging collab sessions AND viewer-history samples
    // (any platform — recordings carry the id when detection had one).
    let sample_stream_id = store.recording_stream_id(rec_id).unwrap_or_default();
    let mut collab_bid: Option<String> = None;
    // Stop-on-unmatch state — see the doc comment above. `last_matched: None`
    // until the baseline poll; `unmatch_since: Some(t)` from the poll that
    // first observed a matching->non-matching transition, cleared the moment
    // it matches again.
    let mut last_matched: Option<bool> = None;
    let mut unmatch_since: Option<i64> = None;
    let mut stop_sent = false;
    // Whether the last fetch failed, so a run of failures during a take (e.g. a
    // mid-take Twitch token expiring) logs once instead of once per poll for
    // the rest of the recording — mirrors the batched-Helix-poll fix in
    // `detect_twitch`. `None` until the first fetch settles (no baseline noise).
    let mut fetch_failing: Option<bool> = None;
    // "Stream ended, capture still finishing" tracking (⏬ badge): consecutive
    // authoritative Offline answers while the capture runs. Two in a row set
    // the flag (one could be a platform blip at the exact poll moment);
    // a Live answer clears it. Reset at watcher start so a stale flag from a
    // previous take can't mislabel this one.
    let mut offline_streak = 0u32;
    let mut offline_flagged = false;
    if let Err(e) = store.set_monitor_capture_offline(monitor_id, false) {
        warn!("clear capture-offline flag failed: {e:#}");
    }
    loop {
        if done.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
            return;
        }
        use crate::detectors::MetaFetch;
        let fetched = match platform {
            Platform::Twitch => ctx.twitch_stream_meta(&url).await,
            Platform::Kick => ctx.kick_stream_meta(&url).await,
            Platform::YouTube => ctx.youtube_stream_meta(&url).await,
            // No metadata source for generic/branded-generic monitors.
            Platform::Nrk | Platform::Nebula | Platform::Generic => MetaFetch::Offline,
        };
        if platform.has_stream_meta() {
            // Only a genuine fetch failure counts toward the warning streak:
            // `Offline` is an authoritative answer (the stream ended while the
            // capture drains its tail) — frozen fields are expected then, and
            // warning would fire spuriously at every normal stream end.
            let failing_now = matches!(fetched, MetaFetch::Failed);
            if failing_now && fetch_failing != Some(true) {
                warn!(
                    monitor_id, rec_id,
                    "live title/game/viewer refresh started failing for {} — \
                     title/game/viewer count will stay frozen until it recovers \
                     (enable debug logging for the specific reason)",
                    platform.tag()
                );
            } else if !failing_now && fetch_failing == Some(true) {
                info!(monitor_id, rec_id, "live title/game/viewer refresh recovered for {}", platform.tag());
            }
            fetch_failing = Some(failing_now);
            // ⏬ tracking: the platform authoritatively saying "offline" while
            // this capture still runs means the broadcast ended and the tool
            // is draining backlog/tail or muxing — flag it so the grid stops
            // implying "live". Two consecutive answers guard against a blip;
            // `Failed` (no answer) never changes the flag.
            match &fetched {
                MetaFetch::Offline => {
                    offline_streak += 1;
                    if offline_streak >= 2 && !offline_flagged {
                        offline_flagged = true;
                        info!(
                            monitor_id, rec_id,
                            "stream ended on {} — capture still finishing \
                             (backlog/tail download or final mux)",
                            platform.tag()
                        );
                        if let Err(e) = store.set_monitor_capture_offline(monitor_id, true) {
                            warn!("set capture-offline flag failed: {e:#}");
                        }
                    }
                }
                MetaFetch::Live(_) => {
                    offline_streak = 0;
                    if offline_flagged {
                        offline_flagged = false;
                        if let Err(e) = store.set_monitor_capture_offline(monitor_id, false) {
                            warn!("clear capture-offline flag failed: {e:#}");
                        }
                    }
                }
                MetaFetch::Failed => {}
            }
        }
        if let MetaFetch::Live(meta) = fetched {
            let at = (now_unix() - started_at).max(0);
            let mut changed = false;
            // Continuous, all-time history (spans recording and non-recording
            // time alike — see `monitor_stream_change`): compared against the
            // monitor's own persisted last-known value, not this take's
            // baseline, so a take starting mid-broadcast (title already known
            // from the live poll before recording began) doesn't log a false
            // "changed from empty" event.
            if meta.title != cont_title {
                if let Err(e) = store.insert_monitor_stream_change(
                    monitor_id, now_unix(), "title", &cont_title, &meta.title,
                ) {
                    warn!("insert monitor title change failed: {e:#}");
                }
                cont_title = meta.title.clone();
            }
            if meta.game != cont_game {
                if let Err(e) = store.insert_monitor_stream_change(
                    monitor_id, now_unix(), "category", &cont_game, &meta.game,
                ) {
                    warn!("insert monitor category change failed: {e:#}");
                }
                cont_game = meta.game.clone();
            }
            // Title: log the initial non-empty value, then every transition.
            if last_title.as_deref() != Some(meta.title.as_str()) {
                let baseline = last_title.is_none();
                let old = last_title.take().unwrap_or_default();
                last_title = Some(meta.title.clone());
                if !(baseline && meta.title.is_empty()) {
                    match store.insert_meta_change(rec_id, at, "title", &old, &meta.title) {
                        Ok(_) => changed = true,
                        Err(e) => warn!("insert title change failed: {e:#}"),
                    }
                }
            }
            // Category/game: same rule.
            if last_game.as_deref() != Some(meta.game.as_str()) {
                let baseline = last_game.is_none();
                let old = last_game.take().unwrap_or_default();
                last_game = Some(meta.game.clone());
                if !(baseline && meta.game.is_empty()) {
                    match store.insert_meta_change(rec_id, at, "category", &old, &meta.game) {
                        Ok(_) => changed = true,
                        Err(e) => warn!("insert category change failed: {e:#}"),
                    }
                }
            }
            // Tags: same rule again, in both ledgers. An empty value from a
            // tagless source (YouTube's scrape has no tag list) only ever
            // compares against empty, so it can never log a fake "cleared".
            if meta.tags != cont_tags {
                if let Err(e) = store.insert_monitor_stream_change(
                    monitor_id, now_unix(), "tags", &cont_tags, &meta.tags,
                ) {
                    warn!("insert monitor tags change failed: {e:#}");
                }
                cont_tags = meta.tags.clone();
                if let Err(e) = store.set_monitor_tags(monitor_id, &meta.tags) {
                    warn!("update tags failed: {e:#}");
                }
            }
            if last_tags.as_deref() != Some(meta.tags.as_str()) {
                let baseline = last_tags.is_none();
                let old = last_tags.take().unwrap_or_default();
                last_tags = Some(meta.tags.clone());
                if !(baseline && meta.tags.is_empty()) {
                    match store.insert_meta_change(rec_id, at, "tags", &old, &meta.tags) {
                        Ok(_) => changed = true,
                        Err(e) => warn!("insert tags change failed: {e:#}"),
                    }
                }
            }
            if let Some(v) = meta.viewers {
                if let Err(e) = store.set_monitor_viewers(monitor_id, v) {
                    warn!("update viewers failed: {e:#}");
                }
                // The in-recording half of the viewer-history dual feed —
                // the scheduler never samples an actively-recording monitor.
                if let Err(e) = store.record_viewer_samples(
                    now_unix(),
                    &[(monitor_id, v, meta.followers, sample_stream_id.as_str())],
                ) {
                    warn!("record viewer history failed: {e:#}");
                }
            }
            if let Some(login) = &collab_login {
                if collab_bid.is_none() {
                    collab_bid = ctx.twitch_id_for_login(login).await;
                }
                ctx.refresh_twitch_collab(
                    monitor_id,
                    login,
                    collab_bid.clone(),
                    &sample_stream_id,
                    &meta.title,
                )
                .await;
            }
            if let Some(rule) = &stop_rule
                && !stop_sent
            {
                let matches_now = crate::triggers::first_match(
                    std::slice::from_ref(rule),
                    Some(meta.title.as_str()),
                    Some(meta.game.as_str()),
                )
                .is_some();
                match last_matched {
                    None => last_matched = Some(matches_now), // baseline only, don't act
                    Some(prev) => {
                        if matches_now {
                            unmatch_since = None; // re-matched: cancel any pending stop
                        } else if prev {
                            unmatch_since = Some(now_unix());
                            info!(
                                monitor_id, rec_id, end_delay_secs = rule.end_delay_secs,
                                "trigger stop-on-unmatch: no longer matches"
                            );
                        }
                        last_matched = Some(matches_now);
                    }
                }
                if let Some(since) = unmatch_since
                    && now_unix() - since >= rule.end_delay_secs
                {
                    info!(monitor_id, rec_id, "trigger stop-on-unmatch: grace elapsed, stopping");
                    let _ = manual_tx.send(ManualCommand::Stop(monitor_id));
                    stop_sent = true;
                }
            }
            if changed {
                // Wake the UI so the Changes column / popup refreshes live.
                let _ = events.send(AppEvent::MonitorState {
                    monitor_id,
                    state: "recording".into(),
                });
            }
        }

        // Interruptible wait until the next poll (checks the flags every 250ms).
        for _ in 0..(META_POLL_INTERVAL_SECS * 4) {
            if done.load(Ordering::SeqCst) || shutdown.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Find the actual output when the predicted path is missing: the largest file
/// in `predicted`'s directory whose name shares its stem (e.g. yt-dlp wrote
/// `<stem>.webm` instead of the predicted `<stem>.mkv`). Tool working/side
/// files sharing the stem (`.log`, `.ytdl`, `.part`, chat/subtitle sidecars)
/// are never candidates — a failed download whose only stem-mate was its own
/// log file used to get that log promoted as the "video" and classified
/// completed (2026-07-06 incident).
pub(super) async fn newest_with_stem(predicted: &Path) -> Option<PathBuf> {
    let dir = predicted.parent()?;
    let stem = predicted.file_stem()?.to_string_lossy().into_owned();
    let mut best: Option<(u64, PathBuf)> = None;
    let mut entries = crate::iomon::fs::read_dir(Cat::FsProbe, dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&stem) && plausible_media_output(&name[stem.len()..]) {
            let len = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
            if len > 0 && best.as_ref().map(|(b, _)| len > *b).unwrap_or(true) {
                best = Some((len, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// True when `rest` (a filename minus the predicted stem, e.g. `".mkv"` or
/// `" (2).webm"` or `".vod.log"`) names a plausible final media output rather
/// than a tool working/side file.
pub(super) fn plausible_media_output(rest: &str) -> bool {
    let lower = rest.to_ascii_lowercase();
    // Working/side files a tool leaves next to (or instead of) the video.
    const NEVER: [&str; 8] = [
        ".log", ".ytdl", ".part", ".json", ".jsonl", ".vtt", ".m3u8", ".txt",
    ];
    if NEVER.iter().any(|s| lower.ends_with(s)) || lower.contains(".temp.") {
        return false;
    }
    const MEDIA_EXTS: [&str; 8] = ["mkv", "mp4", "webm", "ts", "mov", "flv", "m4a", "opus"];
    match lower.rsplit_once('.') {
        Some((_, ext)) => MEDIA_EXTS.contains(&ext),
        None => false,
    }
}

/// True when a capture produced no footage because the stream wasn't actually
/// capturable — it had already ended, hadn't started yet, or exposed no live
/// video formats — rather than because of a real error. Detected from the tool's
/// stderr tail, so a concluded YouTube live (yt-dlp prints "Only images are
/// available …" once the live formats are gone) or an offline/ended Twitch
/// channel (streamlink: "No playable streams found") is classified as `ended`,
/// not `failed`.
pub(super) fn stream_ended_or_unavailable(log: &str) -> bool {
    const PATTERNS: [&str; 5] = [
        // yt-dlp: a live that has ended/not-started has no video formats, only
        // thumbnail/storyboard images.
        "Only images are available",
        "This live event has ended",
        "This live event will begin",
        "Premieres in",
        // streamlink: channel offline or stream ended.
        "No playable streams found",
    ];
    PATTERNS.iter().any(|p| log.contains(p))
}

/// True when SABR live-from-start failed because YouTube's DVR window (~4h)
/// no longer covers the beginning of a long-running stream. The tool downloads
/// a few dozen initial segments then the server goes silent and raises
/// `StreamStallError: … not near live head`. Retrying from-start will always
/// hit the same wall; the next attempt should capture the live edge instead.
pub(super) fn sabr_dvr_window_exceeded(log: &str) -> bool {
    log.contains("not near live head")
}

/// True when a from-start SABR capture is worth retrying **in the same take**
/// rather than finalizing as failed: it left resumable `.state`/`.part` files
/// behind (so yt-dlp can pick up where it died) and the failure wasn't the
/// stream itself ending or the DVR window closing — i.e. a transient local
/// hiccup like antivirus/backup briefly locking the checkpoint file mid-write
/// (2026-07-16: a 2h15m/1.75GB Maid Mint capture died this way to a Windows
/// `PermissionError` renaming its SABR `.state` file, and nothing recovered it
/// until the app was told about it). Mirrors the resumability gate
/// `resume_inflight` already uses for crash recovery at app startup, so this
/// is the same contract, just reachable immediately instead of only after a
/// restart.
pub(super) fn sabr_resumable_failure(
    is_youtube_sabr_from_start: bool,
    sabr_usable: bool,
    state_exists: bool,
    log: &str,
) -> bool {
    is_youtube_sabr_from_start
        && sabr_usable
        && state_exists
        && !stream_ended_or_unavailable(log)
        && !sabr_dvr_window_exceeded(log)
}

/// True when a capture died because yt-dlp couldn't get a GVS PO token — i.e.
/// the bgutil provider server was down/unreachable, not anything wrong with
/// the stream or the take. Matches the phrase shared by every shape the
/// failure takes in a real log (`ERROR:`, `yt_dlp...PoTokenError:`,
/// `DownloadError:`, and the `[sabr:stream] Got error: … Retrying (n/5)`
/// warnings that precede them), as observed in the 2026-07-18 girl_dm_
/// crash-loop. On a match the caller brings the managed server back up
/// ([`crate::pot_server`]) before retrying, because retrying against a dead
/// server fails identically every time.
pub(super) fn pot_token_failure(log: &str) -> bool {
    log.contains("requires a GVS PO Token") || log.contains("PoTokenError")
}

/// A one-line "why did the tool die" summary from a captured log tail, for
/// retry/failure log messages: the last line that looks like an error (yt-dlp
/// `ERROR:`, Python exceptions like `PermissionError:`, node `error:`), else
/// the last non-empty line. Needed because the tool's own log FILE is
/// truncated by the next attempt (`process.rs` creates it fresh per spawn),
/// so the main log's retry line is the only place the cause survives. Splits
/// on `\r` too — yt-dlp progress rewrites make lines carriage-return
/// separated. Capped so a pathological line can't flood the log.
pub(super) fn log_death_reason(log: &str) -> String {
    let lines: Vec<&str> =
        log.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty()).collect();
    let reason = lines
        .iter()
        .rev()
        .find(|l| {
            ["ERROR", "Error", "error:", "Exception", "Traceback"]
                .iter()
                .any(|m| l.contains(m))
        })
        .or_else(|| lines.last())
        .copied()
        .unwrap_or("(no tool output captured)");
    let mut reason = reason.to_string();
    if reason.chars().count() > 300 {
        reason = reason.chars().take(300).collect::<String>() + "…";
    }
    reason
}

/// The genuinely diagnostic lines (errors, exceptions, tool warnings) from a
/// tool-log tail, newline-joined — empty when the tail is only routine
/// progress/status noise. For "surface any tool diagnostics" logging at
/// teardown: a cleanly finished tool's tail is `\r`-rewritten progress
/// (`[download] 100% of 4.75MiB …`), and dumping that raw put noise in the
/// app log at WARN on every normal chat-download end. Splits on `\r` like
/// [`log_death_reason`], dedups consecutive repeats (yt-dlp retry warnings),
/// keeps the LAST `max_lines` (the terminal error outranks earlier retries).
pub(super) fn diagnostic_log_lines(log: &str, max_lines: usize) -> String {
    const MARKERS: [&str; 6] = ["ERROR", "Error", "error:", "Exception", "Traceback", "WARNING"];
    let mut lines: Vec<&str> = log
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|l| !l.is_empty() && MARKERS.iter().any(|m| l.contains(m)))
        .collect();
    lines.dedup();
    if lines.len() > max_lines {
        lines.drain(..lines.len() - max_lines);
    }
    lines.join("\n")
}

impl Supervisor {
    /// Disk-aware startup repair for takes whose DB row claims a final output
    /// file (crash orphans, plus rows a pre-2026-07-11 blind promotion already
    /// flipped to 'completed' with nothing on disk):
    ///
    /// - final file exists non-empty → promote to 'completed' with its real size
    ///   (the app died after the promote move but before the status update);
    /// - final file missing but the capture survived in `.cache\` (the app died
    ///   before/during the finalize remux) → retarget `output_path` to the
    ///   capture, so the row lands in the Issues panel's existing recovery
    ///   sections ("needs re-remux" for a `.ts`, "stuck in cache" for an
    ///   already-final container) instead of reading as a misleading "gone";
    /// - neither on disk → leave the row alone (genuinely gone; Issues says so).
    ///
    /// Never touches rows that are 'recording' (adopted or being finalized).
    pub async fn reconcile_orphan_outputs(&self) {
        let candidates = match self.store.orphan_repair_candidates() {
            Ok(c) => c,
            Err(e) => {
                warn!("orphan repair: failed to load candidates: {e:#}");
                return;
            }
        };
        let mut promoted = 0usize;
        let mut retargeted = 0usize;
        for (rec_id, status, out) in candidates {
            let final_path = PathBuf::from(&out);
            let final_len = crate::iomon::fs::metadata(Cat::Startup, &final_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            if final_len > 0 {
                if let Err(e) = self.store.promote_orphan_completed(rec_id, final_len as i64) {
                    warn!(rec_id, "orphan repair: promote failed: {e:#}");
                } else {
                    promoted += 1;
                    info!(rec_id, bytes = final_len, "orphan repair: output file intact, promoted to 'completed'");
                }
                continue;
            }
            let mut found = None;
            for c in live_capture_candidates(&final_path) {
                let len = crate::iomon::fs::metadata(Cat::Startup, &c)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                if len > 0 {
                    found = Some((c, len));
                    break;
                }
            }
            let Some((capture, cap_len)) = found else {
                continue; // nothing on disk — Issues lists it as missing
            };
            let cap_s = capture.to_string_lossy();
            if let Err(e) = self.store.update_recording_output_path(rec_id, &cap_s) {
                warn!(rec_id, "orphan repair: retarget failed: {e:#}");
                continue;
            }
            let is_ts = capture.extension().is_some_and(|e| e.eq_ignore_ascii_case("ts"));
            if is_ts {
                // A `.ts` in `.cache\` is exactly what the Issues panel's
                // "needs re-remux" section (and its Re-remux action) handles.
                info!(
                    rec_id, prior_status = %status, bytes = cap_len,
                    "orphan repair: final file missing but capture survived in .cache — \
                     retargeted; fix via Issues → Re-remux: {cap_s}"
                );
            } else {
                // Already-final container (e.g. a SABR `.mkv`) merely stuck in
                // `.cache\` — promote so the "stuck in cache" recovery applies.
                let _ = self.store.promote_orphan_completed(rec_id, cap_len as i64);
                info!(
                    rec_id, prior_status = %status, bytes = cap_len,
                    "orphan repair: capture stuck in .cache — retargeted; fix via Issues → Recover: {cap_s}"
                );
            }
            retargeted += 1;
        }
        if promoted + retargeted > 0 {
            info!(promoted, retargeted, "orphan repair: pass complete");
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

    #[tokio::test]
    async fn companion_sidecars_follow_rename() {
        let dir = std::env::temp_dir()
            .join(format!("sa_subs_{}_{}", std::process::id(), now_unix()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let old = "cap_20260620";
        let new = "Show_1080p";
        // The video has already been renamed; its companions have not.
        tokio::fs::write(dir.join(format!("{new}.mkv")), b"v").await.unwrap();
        tokio::fs::write(dir.join(format!("{old}.en.vtt")), b"s").await.unwrap();
        tokio::fs::write(dir.join(format!("{old}.chat.jsonl")), b"c").await.unwrap();
        tokio::fs::write(dir.join(format!("{old}.live_chat.json")), b"c").await.unwrap();
        // A same-stem non-companion file must be left alone.
        tokio::fs::write(dir.join(format!("{old}.notes.txt")), b"x").await.unwrap();

        rename_companion_sidecars(&dir, old, new).await;

        assert!(dir.join(format!("{new}.en.vtt")).exists());
        assert!(dir.join(format!("{new}.chat.jsonl")).exists());
        assert!(dir.join(format!("{new}.live_chat.json")).exists());
        assert!(!dir.join(format!("{old}.en.vtt")).exists());
        assert!(!dir.join(format!("{old}.chat.jsonl")).exists());
        assert!(dir.join(format!("{old}.notes.txt")).exists());
        assert!(!dir.join(format!("{new}.notes.txt")).exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    #[tokio::test]
    async fn companion_sidecar_rename_shortens_instead_of_orphaning() {
        // Reproduces the exact CottontailVA-class failure: the main video's
        // stem fits under its own extension but overflows once combined with
        // a companion's longer suffix. The sidecar must still follow the
        // rename (under a shortened, "..."-marked name) instead of being
        // silently left behind under `old_stem`.
        let dir = std::env::temp_dir()
            .join(format!("sa_shorten_sidecar_{}_{}", std::process::id(), now_unix()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let old_stem = "old";
        // 250 'x's: old="old" -> new_stem+".mkv" is fine (254 units), but
        // new_stem+".chat.jsonl" (261 units) overflows 255.
        let new_stem = "x".repeat(250);
        tokio::fs::write(dir.join(format!("{old_stem}.chat.jsonl")), b"chat-data").await.unwrap();

        rename_companion_sidecars(&dir, old_stem, &new_stem).await;

        assert!(
            !dir.join(format!("{old_stem}.chat.jsonl")).exists(),
            "must not still be sitting under the old placeholder name"
        );
        let mut rd = tokio::fs::read_dir(&dir).await.unwrap();
        let mut found = None;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".chat.jsonl") {
                found = Some(name);
            }
        }
        let name = found.expect("the sidecar must have followed the rename under some name");
        assert!(name.contains("..."), "the shortened name must mark the cut: {name}");
        assert!(name.encode_utf16().count() <= NTFS_MAX_COMPONENT_UTF16);
        assert_eq!(
            tokio::fs::read(dir.join(&name)).await.unwrap(),
            b"chat-data",
            "content must be preserved, not just the name"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    #[test]
    fn companion_suffix_recognizes_thumbnails_subs_and_chat() {
        assert!(is_companion_suffix("thumbnail.jpg"));
        assert!(is_companion_suffix("webp")); // yt-dlp --write-thumbnail
        assert!(is_companion_suffix("png"));
        assert!(is_companion_suffix("en.vtt"));
        assert!(is_companion_suffix("chat.jsonl"));
        assert!(is_companion_suffix("live_chat.json"));
        // The video itself and SABR working files are NOT companions.
        assert!(!is_companion_suffix("ts"));
        assert!(!is_companion_suffix("mkv"));
        assert!(!is_companion_suffix("f140.mkv.sq0.part"));
        assert!(!is_companion_suffix("f303.mkv.state"));
    }
    #[test]
    fn find_split_media_accepts_bare_parts_only() {
        let dir = std::env::temp_dir().join(format!("sa-split-{}", std::process::id()));
        let cache = dir.join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        let stem = "Chan - 2026-07-08 23-05-21 - title [youtube abc]";
        // Finished parts (video + audio) — accepted.
        std::fs::write(cache.join(format!("{stem}.mkv.f299.mp4")), b"v").unwrap();
        std::fs::write(cache.join(format!("{stem}.mkv.f140.mp4")), b"a").unwrap();
        // In-flight / working files — rejected.
        std::fs::write(cache.join(format!("{stem}.mkv.f299.mp4.sq0.part")), b"x").unwrap();
        std::fs::write(cache.join(format!("{stem}.mkv.temp.mp4")), b"x").unwrap();
        std::fs::write(cache.join(format!("{stem}.log")), b"x").unwrap();
        std::fs::write(cache.join(format!("{stem}.thumbnail.jpg")), b"x").unwrap();
        // A sibling recording whose stem extends this one — rejected.
        std::fs::write(cache.join(format!("{stem}. Part 2.f303.mp4")), b"x").unwrap();

        // Works from both the never-promoted (.cache) path and the final path.
        for capture in [
            cache.join(format!("{stem}.mkv")),
            dir.join(format!("{stem}.mkv")),
        ] {
            let parts = find_split_media(&capture);
            let names: Vec<String> = parts
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                names,
                vec![
                    format!("{stem}.mkv.f140.mp4"),
                    format!("{stem}.mkv.f299.mp4"),
                ],
                "capture={}",
                capture.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn find_split_parts_picks_largest_per_format() {
        let dir = std::env::temp_dir().join(format!("sa-split-parts-{}", std::process::id()));
        let cache = dir.join(".sa-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let stem = "Chan - 2026-07-12 00-03-31 - title [youtube abc]";
        // Ordering A: {stem}.f<id>.mkv.sq<N>.part (format id before container).
        std::fs::write(cache.join(format!("{stem}.f303.mkv.sq0.part")), vec![0u8; 10]).unwrap();
        std::fs::write(cache.join(format!("{stem}.f140.mkv.sq0.part")), vec![0u8; 4]).unwrap();
        // Ordering B: {stem}.mkv.f<id>.mp4.sq<N>.part — two sequences of the
        // SAME format: only the largest is returned.
        std::fs::write(cache.join(format!("{stem}.mkv.f299.mp4.sq1.part")), vec![0u8; 3]).unwrap();
        std::fs::write(cache.join(format!("{stem}.mkv.f299.mp4.sq62.part")), vec![0u8; 20]).unwrap();
        // Non-media / working files — rejected.
        std::fs::write(cache.join(format!("{stem}.f303.mkv.state")), b"x").unwrap();
        std::fs::write(cache.join(format!("{stem}.mkv.temp.mp4.sq0.part")), b"x").unwrap();
        std::fs::write(cache.join(format!("{stem}.log")), b"x").unwrap();
        // 0-byte part — rejected.
        std::fs::write(cache.join(format!("{stem}.f251.mkv.sq0.part")), b"").unwrap();
        // A sibling recording whose stem extends this one — rejected.
        std::fs::write(cache.join(format!("{stem}. Part 2.f303.mkv.sq0.part")), b"x").unwrap();

        let parts = find_split_parts(&cache.join(format!("{stem}.mkv")));
        let names: Vec<String> = parts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                format!("{stem}.f140.mkv.sq0.part"),
                format!("{stem}.f303.mkv.sq0.part"),
                format!("{stem}.mkv.f299.mp4.sq62.part"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn ended_stream_is_not_a_failure() {
        // A concluded/upcoming YouTube live: yt-dlp has only thumbnail images left
        // (the exact stderr from the Layna YouTube take 3 bug).
        assert!(stream_ended_or_unavailable(
            "ERROR: [youtube] aEhxflmEYGA: Requested format is not available\n\
             WARNING: Only images are available for download. use --list-formats to see them"
        ));
        assert!(stream_ended_or_unavailable("This live event has ended."));
        assert!(stream_ended_or_unavailable("This live event will begin in 2 hours."));
        // streamlink: offline / ended channel.
        assert!(stream_ended_or_unavailable(
            "error: No playable streams found on this URL: https://twitch.tv/x"
        ));
        // A *real* failure must stay a failure (e.g. the Layna take 1: bad cookies),
        // and a generic error too.
        assert!(!stream_ended_or_unavailable(
            "WARNING: [youtube] The provided YouTube account cookies are no longer valid."
        ));
        assert!(!stream_ended_or_unavailable(
            "ERROR: unable to download video data: HTTP Error 403: Forbidden"
        ));
        assert!(!stream_ended_or_unavailable(""));
    }
    #[test]
    fn sabr_resumable_failure_gates_correctly() {
        let permission_error = "PermissionError: [WinError 5] Access is denied: \
             'A:\\streams\\.sa-cache\\Maid Mint\\tmpzyk9b7fo' -> '...state'";
        // The Maid Mint incident: a from-start SABR take, usable binary, leftover
        // `.state`, and a failure that's neither the stream ending nor the DVR
        // window closing — should retry.
        assert!(sabr_resumable_failure(true, true, true, permission_error));
        assert!(!sabr_resumable_failure(false, true, true, permission_error), "not a YouTube SABR from-start take");
        assert!(!sabr_resumable_failure(true, false, true, permission_error), "no usable SABR binary");
        assert!(!sabr_resumable_failure(true, true, false, permission_error), "nothing resumable left behind");
        assert!(
            !sabr_resumable_failure(true, true, true, "Only images are available for download."),
            "stream genuinely ended — retrying would just hit the same wall"
        );
        assert!(
            !sabr_resumable_failure(true, true, true, "StreamStallError: not near live head"),
            "DVR window exceeded has its own recovery (fall back to live edge), not a same-take retry"
        );
        // The 2026-07-20 Maid Mint deaths: deep-rewind segment desync after a
        // connection reset. Resuming genuinely progresses (each retry grew the
        // downloaded ranges), so this must stay retryable — that take failed
        // only because the fixed retry budget ran out (now refunded after a
        // long-lived attempt).
        assert!(sabr_resumable_failure(
            true,
            true,
            true,
            "yt_dlp.utils.DownloadError: Segment sequence number mismatch for format \
             FormatId(itag=399, lmt=None, xtags=None): expected 648, received 9655"
        ));
        // "the streamer has disabled DVR" is an informational line, not the
        // DVR-WINDOW-exceeded stall — it must not block a retry.
        assert!(sabr_resumable_failure(
            true,
            true,
            true,
            "[download] Downloading from the live edge; the streamer has disabled DVR for this stream\nyt_dlp.utils.DownloadError: Segment sequence number mismatch"
        ));
    }
    #[test]
    fn pot_token_failure_matches_real_shapes() {
        // Every shape the 2026-07-18 girl_dm_ crash-loop actually logged.
        for line in [
            "ERROR: This stream requires a GVS PO Token to continue",
            "yt_dlp.extractor.youtube._streaming.sabr.exceptions.PoTokenError: \
             This stream requires a GVS PO Token to continue",
            "yt_dlp.utils.DownloadError: This stream requires a GVS PO Token to continue",
            "WARNING: [sabr:stream] Got error: This stream requires a GVS PO Token \
             to continue. Retrying (5/5)...",
        ] {
            assert!(pot_token_failure(line), "{line:?} not classified");
        }
        // Other failures must not trigger a server restart.
        assert!(!pot_token_failure(
            "PermissionError: [WinError 5] Access is denied: 'tmp' -> '.state'"
        ));
        assert!(!pot_token_failure("Only images are available for download."));
        assert!(!pot_token_failure(""));
    }
    #[test]
    fn diagnostic_log_lines_drops_progress_noise() {
        // The exact leak: a cleanly-finished chat download's tail is progress
        // rewrites only — nothing diagnostic, so nothing should be logged.
        assert_eq!(
            diagnostic_log_lines("[download]  99% of 4.75MiB\r[download] 100% of 4.75MiB in 04:14:48 at 326.08B/s\n", 8),
            ""
        );
        assert_eq!(diagnostic_log_lines("", 8), "");
        // Real diagnostics survive, progress interleaved via \r is dropped,
        // consecutive duplicate warnings collapse.
        let log = "[download] 12%\rWARNING: [youtube] Retrying (1/3)...\rWARNING: [youtube] Retrying (1/3)...\r[download] 13%\nERROR: fragment not found";
        assert_eq!(
            diagnostic_log_lines(log, 8),
            "WARNING: [youtube] Retrying (1/3)...\nERROR: fragment not found"
        );
        // Cap keeps the LAST lines — the terminal error, not the first retry.
        let many = (1..=9).map(|i| format!("ERROR: e{i}")).collect::<Vec<_>>().join("\n");
        let capped = diagnostic_log_lines(&many, 3);
        assert_eq!(capped, "ERROR: e7\nERROR: e8\nERROR: e9");
    }
    #[test]
    fn log_death_reason_picks_error_over_trailing_progress() {
        // yt-dlp interleaves \r progress rewrites; the Python exception is the
        // interesting line even when progress noise follows it.
        let log = "[download] 1.2MiB\rPermissionError: [WinError 5] Access is denied: 'tmp' -> '.state'\r[download] 1.3MiB\r[download] 1.4MiB";
        assert_eq!(
            log_death_reason(log),
            "PermissionError: [WinError 5] Access is denied: 'tmp' -> '.state'"
        );
        // No error marker anywhere -> last non-empty line, better than nothing.
        assert_eq!(log_death_reason("line one\nline two\n\n"), "line two");
        assert_eq!(log_death_reason(""), "(no tool output captured)");
        // A pathological line is capped, not dumped whole.
        let huge = format!("ERROR: {}", "x".repeat(1000));
        assert!(log_death_reason(&huge).chars().count() <= 301);
    }
    #[test]
    fn plausible_media_output_rejects_working_files() {
        // Tool working/side files must never be promoted as the video.
        for rest in [
            ".log", ".vod.log", ".ytdl", ".mp4.ytdl", ".part", ".f303.webm.part",
            ".chat.jsonl", ".live_chat.json", ".en.vtt", ".m3u8", ".temp.mkv", "",
        ] {
            assert!(!plausible_media_output(rest), "{rest:?} accepted");
        }
        // Real media outputs (incl. collision variants and differing exts).
        for rest in [".mkv", ".vod.mkv", ".webm", ".mp4", ".ts", " (2).mkv", ".m4a"] {
            assert!(plausible_media_output(rest), "{rest:?} rejected");
        }
    }
}
