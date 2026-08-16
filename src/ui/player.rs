//! Playback/preview: filesystem probes, stream targets, player command
//! building, live preview spawning.

use super::*;

/// Compact `1920x1080@60 h264` of a file's first video stream, for the Issues
/// mismatch explainer. Blocking ffprobe — background threads only.
pub(super) fn probe_dims_sync(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.args([
        "-v", "error", "-select_streams", "v:0", "-show_entries",
        "stream=codec_name,width,height,r_frame_rate", "-of", "csv=p=0", path,
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(out) = cmd.output() else { return String::new() };
    // csv=p=0 → "h264,1280,720,60/1"
    let line = String::from_utf8_lossy(&out.stdout);
    let mut it = line.trim().split(',');
    let (codec, w, h, rate) = (
        it.next().unwrap_or(""),
        it.next().unwrap_or(""),
        it.next().unwrap_or(""),
        it.next().unwrap_or(""),
    );
    if codec.is_empty() || w.is_empty() {
        return String::new();
    }
    let fps = match rate.split_once('/') {
        Some((n, d)) => {
            let (n, d) = (n.parse::<f64>().unwrap_or(0.0), d.parse::<f64>().unwrap_or(1.0));
            if d > 0.0 { (n / d).round() as i64 } else { 0 }
        }
        None => rate.parse::<f64>().unwrap_or(0.0).round() as i64,
    };
    format!("{w}x{h}@{fps} {codec}")
}
/// Open a path (file or directory) with the default associated application.
pub(super) fn open_path(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// What "Play local recording (start)" should open for a recording, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StreamTarget {
    /// A completed output file — open plainly (works with any player).
    Finished(std::path::PathBuf),
    /// A single still-growing capture file (`.ts` / `.mkv` / `.mkv.mp4` /
    /// `.dash.ts`). mpv follows growth via `appending://`; other players open
    /// it plainly (playable, but they stop at the current end).
    Growing(std::path::PathBuf),
    /// Mid-SABR capture: separate still-growing per-format files (video +
    /// audio), largest first. Playable only in mpv: the largest (video) file
    /// opens as `appending://` main file, the rest attach via
    /// `--audio-file=appending://…`. (An `edl://` merge also loads both
    /// tracks but keeps zero seconds of demuxer readahead and bakes the
    /// total duration at open — video freezes and growth is not followed.)
    SplitAv(Vec<std::path::PathBuf>),
    /// A subscriber-only broadcast's CDN parts, oldest first.
    ///
    /// Nothing was captured from the live edge — Twitch refused it — so these
    /// numbered files *are* the archive until the session ends and joins them
    /// into the take's real file. Opened as a playlist, which every player
    /// takes: each part is complete before it's moved into place, so none of
    /// them needs growth-following.
    Sequence(Vec<std::path::PathBuf>),
}

/// SABR growing per-format files only need moderately-sized init data before
/// they're openable; also filters out the tiny `.state` sidecars (~50 B).
pub(super) const SPLIT_AV_MIN_BYTES: u64 = 64 * 1024;

/// How long a delivered [`FsProbes`] result stays fresh; after this the next
/// access queues a background refresh (the stale value keeps being returned
/// meanwhile — the UI thread never waits for the disk).
pub(super) const FS_PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// Entries not accessed for this long are dropped on `logic()`'s slow tick,
/// so paths for deleted rows don't accumulate.
pub(super) const FS_PROBE_EVICT: std::time::Duration = std::time::Duration::from_secs(120);

/// A probe request shipped to the `fs-probes` worker thread.
pub(super) enum ProbeJob {
    File(std::path::PathBuf),
    Dir(std::path::PathBuf),
    Len(std::path::PathBuf),
    /// Handle-based size via [`live_file_len`] — unlike `Len`, current even
    /// while another process still holds the file open for writing. For the
    /// Streams grid's take-row/stream-row size display on an active take.
    LiveLen(std::path::PathBuf),
    Target(String),
    /// `(archive folder, broadcast id)` — a subscriber-only broadcast's CDN
    /// parts, which is a `read_dir` of a folder holding a whole channel's
    /// archive and so belongs on the worker like every other scan.
    CdnParts(CdnPartsKey),
}

/// What identifies one broadcast's CDN parts: the folder to scan and the
/// broadcast id its takes' filenames all carry.
pub(super) type CdnPartsKey = (std::path::PathBuf, String);

/// A finished probe shipped back from the worker.
pub(super) enum ProbeResult {
    File(std::path::PathBuf, bool),
    Dir(std::path::PathBuf, bool),
    Len(std::path::PathBuf, u64),
    LiveLen(std::path::PathBuf, u64),
    Target(String, Option<StreamTarget>),
    CdnParts(CdnPartsKey, Vec<std::path::PathBuf>),
}

pub(super) struct ProbeSlot<V> {
    /// When the value was last actually probed (`None` = placeholder still
    /// awaiting its first worker result).
    pub(super) at: Option<std::time::Instant>,
    /// Last render-path access — drives eviction on the slow tick.
    pub(super) used: std::time::Instant,
    /// A refresh for this key is queued or in-flight (dedups requests, and
    /// bounds the worker queue to one entry per key even while the disk
    /// stalls for minutes).
    pub(super) pending: bool,
    pub(super) v: V,
}

/// Never-blocking cache for the per-row filesystem probes the tables re-run
/// every frame (in-progress capture scans, Open file/folder button
/// enablement). All I/O happens on a single `fs-probes` worker thread;
/// accessors return the last-known value immediately (a pessimistic
/// placeholder — `false`/`0`/`None` — on first sight) and queue a background
/// refresh once the entry is older than [`FS_PROBE_TTL`].
///
/// The single worker is deliberate: it serializes probe I/O, so when a disk
/// stalls only the worker blocks — values go stale but the UI keeps painting.
/// The old synchronous TTL design froze the whole UI for as long as one stat
/// took: recordings live on a USB HDD here, and under sustained capture
/// writes a single `File::open`/`read_dir` against it was observed blocking
/// for 60+ seconds (the 2026-07-09 "UI frozen" watchdog reports during the
/// GDQ marathon recording).
pub(super) struct FsProbes {
    pub(super) files: HashMap<std::path::PathBuf, ProbeSlot<bool>>,
    pub(super) dirs: HashMap<std::path::PathBuf, ProbeSlot<bool>>,
    pub(super) sizes: HashMap<std::path::PathBuf, ProbeSlot<u64>>,
    pub(super) live_sizes: HashMap<std::path::PathBuf, ProbeSlot<u64>>,
    pub(super) targets: HashMap<String, ProbeSlot<Option<StreamTarget>>>,
    pub(super) cdn_parts: HashMap<CdnPartsKey, ProbeSlot<Vec<std::path::PathBuf>>>,
    pub(super) tx: std::sync::mpsc::Sender<ProbeJob>,
    pub(super) rx: std::sync::mpsc::Receiver<ProbeResult>,
}

/// Return the slot's value, queueing a background refresh when it's a fresh
/// key or older than [`FS_PROBE_TTL`]. Shared by all four [`FsProbes`] maps.
pub(super) fn probe_lookup<K, Q, V>(
    tx: &std::sync::mpsc::Sender<ProbeJob>,
    map: &mut HashMap<K, ProbeSlot<V>>,
    key: &Q,
    placeholder: V,
    job: impl Fn(K) -> ProbeJob,
) -> V
where
    K: std::borrow::Borrow<Q> + std::hash::Hash + Eq,
    Q: std::hash::Hash + Eq + ToOwned<Owned = K> + ?Sized,
    V: Clone,
{
    let now = std::time::Instant::now();
    if let Some(slot) = map.get_mut(key) {
        slot.used = now;
        if !slot.pending && slot.at.is_none_or(|at| now.duration_since(at) >= FS_PROBE_TTL) {
            slot.pending = true;
            let _ = tx.send(job(key.to_owned()));
        }
        return slot.v.clone();
    }
    map.insert(
        key.to_owned(),
        ProbeSlot { at: None, used: now, pending: true, v: placeholder.clone() },
    );
    let _ = tx.send(job(key.to_owned()));
    placeholder
}

impl FsProbes {
    pub(super) fn new(ctx: egui::Context) -> FsProbes {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<ProbeJob>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<ProbeResult>();
        std::thread::Builder::new()
            .name("fs-probes".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    use crate::iomon::Cat;
                    let res = match job {
                        ProbeJob::File(p) => {
                            let v = crate::iomon::fs::metadata_sync(Cat::FsProbe, &p)
                                .map(|m| m.is_file())
                                .unwrap_or(false);
                            ProbeResult::File(p, v)
                        }
                        ProbeJob::Dir(p) => {
                            let v = crate::iomon::fs::metadata_sync(Cat::FsProbe, &p)
                                .map(|m| m.is_dir())
                                .unwrap_or(false);
                            ProbeResult::Dir(p, v)
                        }
                        // Directory-entry size (fine for finished files).
                        ProbeJob::Len(p) => {
                            let v = crate::iomon::fs::metadata_sync(Cat::FsProbe, &p)
                                .map(|m| m.len())
                                .unwrap_or(0);
                            ProbeResult::Len(p, v)
                        }
                        ProbeJob::LiveLen(p) => {
                            let v = live_file_len(&p).unwrap_or(0);
                            ProbeResult::LiveLen(p, v)
                        }
                        // stream_target_for_active's own read_dir/open calls
                        // are accounted individually (Cat::FsProbe) inside.
                        ProbeJob::Target(s) => {
                            let t = stream_target_for_active(&s);
                            ProbeResult::Target(s, t)
                        }
                        ProbeJob::CdnParts(key) => {
                            let parts = cdn_parts_for_broadcast(&key.0, &key.1);
                            ProbeResult::CdnParts(key, parts)
                        }
                    };
                    if res_tx.send(res).is_err() {
                        break; // FsProbes dropped — app shutting down
                    }
                    // Paint the fresh value promptly instead of waiting out
                    // the ≥1 fps idle repaint floor.
                    ctx.request_repaint();
                }
            })
            .expect("spawn fs-probes thread");
        FsProbes {
            files: HashMap::new(),
            dirs: HashMap::new(),
            sizes: HashMap::new(),
            live_sizes: HashMap::new(),
            targets: HashMap::new(),
            cdn_parts: HashMap::new(),
            tx: job_tx,
            rx: res_rx,
        }
    }

    /// Last-known `is_file` (false until the first probe lands).
    pub(super) fn is_file(&mut self, p: &std::path::Path) -> bool {
        probe_lookup(&self.tx, &mut self.files, p, false, ProbeJob::File)
    }

    /// Last-known `is_dir` (false until the first probe lands).
    pub(super) fn is_dir(&mut self, p: &std::path::Path) -> bool {
        probe_lookup(&self.tx, &mut self.dirs, p, false, ProbeJob::Dir)
    }

    /// Last-known directory-entry size (0 while missing/unprobed).
    pub(super) fn len(&mut self, p: &std::path::Path) -> u64 {
        probe_lookup(&self.tx, &mut self.sizes, p, 0, ProbeJob::Len)
    }

    /// Last-known [`live_file_len`] (0 while missing/unprobed) — current size
    /// of a still-growing capture, unlike [`FsProbes::len`]'s directory-entry
    /// read. For an active take's size in the Streams grid.
    pub(super) fn live_len(&mut self, p: &std::path::Path) -> u64 {
        probe_lookup(&self.tx, &mut self.live_sizes, p, 0, ProbeJob::LiveLen)
    }

    /// Last-known [`stream_target_for_active`] (a `.cache` dir scan plus a
    /// `File::open` per candidate — by far the heaviest per-row probe).
    pub(super) fn target(&mut self, output_path: &str) -> Option<StreamTarget> {
        probe_lookup(&self.tx, &mut self.targets, output_path, None, ProbeJob::Target)
    }

    /// Last-known CDN parts for one subscriber-only broadcast (empty until the
    /// first probe lands) — see [`cdn_parts_for_broadcast`].
    pub(super) fn cdn_parts(
        &mut self,
        dir: &std::path::Path,
        stream_id: &str,
    ) -> Vec<std::path::PathBuf> {
        let key = (dir.to_path_buf(), stream_id.to_string());
        probe_lookup(&self.tx, &mut self.cdn_parts, &key, Vec::new(), ProbeJob::CdnParts)
    }

    /// Install finished worker results. Called once per frame from `logic()`;
    /// results for keys evicted in the meantime are simply dropped.
    pub(super) fn drain_results(&mut self) {
        while let Ok(res) = self.rx.try_recv() {
            let now = std::time::Instant::now();
            fn install<K: std::hash::Hash + Eq, V>(
                map: &mut HashMap<K, ProbeSlot<V>>,
                key: &K,
                v: V,
                now: std::time::Instant,
            ) {
                if let Some(slot) = map.get_mut(key) {
                    slot.v = v;
                    slot.at = Some(now);
                    slot.pending = false;
                }
            }
            match res {
                ProbeResult::File(p, v) => install(&mut self.files, &p, v, now),
                ProbeResult::Dir(p, v) => install(&mut self.dirs, &p, v, now),
                ProbeResult::Len(p, v) => install(&mut self.sizes, &p, v, now),
                ProbeResult::LiveLen(p, v) => install(&mut self.live_sizes, &p, v, now),
                ProbeResult::Target(s, t) => install(&mut self.targets, &s, t, now),
                ProbeResult::CdnParts(k, v) => install(&mut self.cdn_parts, &k, v, now),
            }
        }
    }

    /// Drop entries no render path has touched for [`FS_PROBE_EVICT`]
    /// (deleted rows stop being rendered, stop being accessed, and age out).
    pub(super) fn evict_unused(&mut self) {
        let now = std::time::Instant::now();
        self.files.retain(|_, s| now.duration_since(s.used) < FS_PROBE_EVICT);
        self.dirs.retain(|_, s| now.duration_since(s.used) < FS_PROBE_EVICT);
        self.sizes.retain(|_, s| now.duration_since(s.used) < FS_PROBE_EVICT);
        self.live_sizes.retain(|_, s| now.duration_since(s.used) < FS_PROBE_EVICT);
        self.targets.retain(|_, s| now.duration_since(s.used) < FS_PROBE_EVICT);
        self.cdn_parts.retain(|_, s| now.duration_since(s.used) < FS_PROBE_EVICT);
    }
}

/// True current size of a possibly-being-written file, or `None` if it can't
/// be opened / isn't a file. The size that `fs::metadata` / `read_dir`
/// metadata report comes from the DIRECTORY ENTRY, which NTFS only updates
/// lazily while another process holds the file open for writing — a capture
/// started seconds ago reads as 0 bytes there even with megabytes written
/// (verified against a live download: dir entry 0, handle size 5 MB).
/// Opening the file queries the handle, which is always current.
pub(super) fn live_file_len(p: &std::path::Path) -> Option<u64> {
    let md = crate::iomon::fs::open_sync(crate::iomon::Cat::FsProbe, p)
        .ok()?
        .metadata()
        .ok()?;
    md.is_file().then(|| md.len())
}

/// Find what an active recording's in-progress capture can be played from, by
/// probing `.cache\` next to its final output path.
///
/// 1. The single-file captures streamlink/yt-dlp produce (`{stem}.ts`,
///    `{stem}.mkv`, `{stem}.mkv.mp4`) → [`StreamTarget::Growing`]. This also
///    covers the DASH companion (its own output path is `{stem}.dash.mkv`, so
///    its capture probe hits `{stem}.dash.ts`) and the brief post-merge window
///    of a SABR capture.
/// 2. SABR mid-download: the per-format growing files. Naming drifts between
///    dev-build versions — both `{stem}.mkv.f140.mp4[.sq0.part]` and
///    `{stem}.f303.mkv[.sq0.part]` orderings are seen in the wild — so this
///    scans for `{stem}.`-prefixed names containing an `f<digits>` dot-segment
///    with a media extension or an in-flight `.sq<N>.part` suffix. During the
///    active download the growing media file IS the `.sq<N>.part` file; the
///    bare-named twin only appears once the stream ends and the merge starts.
///    Two or more formats → [`StreamTarget::SplitAv`]; one → `Growing`.
pub(super) fn stream_target_for_active(output_path: &str) -> Option<StreamTarget> {
    let final_path = std::path::Path::new(output_path);
    let stem = final_path.file_stem()?.to_string_lossy().into_owned();
    // Current layout first (central root when configured), then the per-dir
    // and legacy dirs — takes started under an older build (incl. re-attached
    // ones) still write to those.
    let cache_dirs = crate::downloader::cache_dir_candidates(final_path.parent()?);
    for cache_dir in &cache_dirs {
        for ext in [".ts", ".mkv", ".mkv.mp4"] {
            let candidate = cache_dir.join(format!("{stem}{ext}"));
            if live_file_len(&candidate).unwrap_or(0) > 0 {
                return Some(StreamTarget::Growing(candidate));
            }
        }
    }
    // SABR split scan: group candidates by format id, keep the best file per
    // format (bare beats .part; higher .sq<N> beats lower — a resume starts a
    // new sequence file and the highest is the one currently growing).
    // (format_id, sequence: None = bare/merge-phase file) → (path, size)
    let mut best: std::collections::HashMap<u64, (Option<u64>, std::path::PathBuf, u64)> =
        std::collections::HashMap::new();
    let prefix = format!("{stem}.");
    for entry in cache_dirs.iter().flat_map(|d| {
        crate::iomon::fs::read_dir_sync(crate::iomon::Cat::FsProbe, d)
            .into_iter()
            .flatten()
            .flatten()
    }) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix) else { continue };
        if rest.ends_with(".state") || rest.ends_with(".log") || rest.ends_with(".ytdl") {
            continue;
        }
        if rest.contains(".temp.") {
            continue; // in-flight ffmpeg merge output
        }
        let segs: Vec<&str> = rest.split('.').collect();
        let parse_fmt = |s: &str| -> Option<u64> {
            let d = s.strip_prefix('f')?;
            if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            d.parse().ok()
        };
        let Some(fpos) = segs.iter().position(|s| parse_fmt(s).is_some()) else { continue };
        // Everything before the f<id> segment must be container decoration
        // (variant A has the template ext there: `{stem}.mkv.f140.mp4…`),
        // never arbitrary title text — otherwise a sibling recording whose
        // stem merely extends this stem after a dot ("Chan" vs
        // "Chan. Part 2.f303….part" → rest " Part 2.f303….part") leaks in.
        if !segs[..fpos].iter().all(|s| matches!(*s, "mkv" | "mp4" | "webm" | "m4a" | "ts")) {
            continue;
        }
        let format_id = parse_fmt(segs[fpos]).unwrap_or(0);
        // Growing in-flight file `….sq<N>.part`, or a bare media file
        // (merge phase / finished-writing).
        let seq: Option<u64> = match segs.as_slice() {
            [.., sq, "part"] => match sq.strip_prefix("sq").and_then(|d| d.parse().ok()) {
                Some(n) => Some(n),
                None => continue, // other .part files aren't playable media
            },
            [.., ext] if matches!(*ext, "mp4" | "m4a" | "webm" | "mkv") => None,
            _ => continue,
        };
        let Some(len) = live_file_len(&entry.path()) else { continue };
        if len < SPLIT_AV_MIN_BYTES {
            continue;
        }
        let candidate = (seq, entry.path(), len);
        match best.entry(format_id) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(candidate);
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                // Bare (None) outranks any .part; among .parts, higher sq wins.
                let better = match (&o.get().0, &candidate.0) {
                    (Some(_), None) => true,
                    (Some(cur), Some(new)) => new > cur,
                    _ => false,
                };
                if better {
                    o.insert(candidate);
                }
            }
        }
    }
    let mut parts: Vec<(std::path::PathBuf, u64)> =
        best.into_values().map(|(_, p, len)| (p, len)).collect();
    match parts.len() {
        0 => None,
        1 => Some(StreamTarget::Growing(parts.remove(0).0)),
        _ => {
            parts.sort_by_key(|p| std::cmp::Reverse(p.1)); // video (largest) first
            Some(StreamTarget::SplitAv(parts.into_iter().map(|(p, _)| p).collect()))
        }
    }
}

/// One subscriber-only broadcast's CDN parts in `dir`, in play order.
///
/// The archive folder, not the capture cache: a refused take never captured
/// anything, so there is nothing in the cache to find — the parts are written
/// straight into the output folder as finished files (see
/// `downloader::sub_only`). Without this, "Play local recording" is greyed out
/// for precisely the broadcasts where the app *is* archiving something, just
/// not from the live edge.
///
/// Matched by **broadcast id, not by take**, exactly as
/// `sub_only::adopt_sub_only_parts` matches when it resumes. One broadcast can
/// end up with parts under several takes' names — every time a doomed capture
/// spawns, the next part is written under that take's stem — and the index
/// continues across them. Scoping this to one take's stem would silently play
/// a fraction of the broadcast and call it the recording.
pub(super) fn cdn_parts_for_broadcast(
    dir: &std::path::Path,
    stream_id: &str,
) -> Vec<std::path::PathBuf> {
    if stream_id.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<(usize, std::path::PathBuf)> =
        crate::iomon::fs::read_dir_sync(crate::iomon::Cat::FsProbe, dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.contains(stream_id) {
                    return None;
                }
                crate::downloader::sub_only::part_index(&name).map(|i| (i, e.path()))
            })
            .collect();
    // By parsed index, not by name: the numbering is per broadcast and
    // continues across takes and restarts, so lexical order only coincidentally
    // agrees. Dedup because a part can exist in both the archive folder and an
    // older layout's cache.
    parts.sort_by_key(|(i, _)| *i);
    parts.dedup_by_key(|(i, _)| *i);
    parts.into_iter().map(|(_, p)| p).collect()
}

/// True when the configured player binary is mpv (or an mpv front-end like
/// mpv.net) — the only player that supports `appending://` and `edl://`.
pub(super) fn player_is_mpv(player_path: &str) -> bool {
    std::path::Path::new(player_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase().starts_with("mpv"))
        .unwrap_or(false)
}

/// Whether the configured player can play this target. Split SABR captures
/// need mpv; everything else is a plain file any player opens.
pub(super) fn playable_with(t: &StreamTarget, player: &str) -> bool {
    match t {
        // A `Sequence` is just several complete files handed over in order —
        // mpv, VLC and MPC all treat extra arguments as a playlist.
        StreamTarget::Finished(_) | StreamTarget::Growing(_) | StreamTarget::Sequence(_) => true,
        StreamTarget::SplitAv(_) => player_is_mpv(player),
    }
}

/// Flags for watching a still-growing capture in mpv: don't quit on a
/// momentary EOF/stall, and allow seeking within what's been read.
pub(super) const MPV_LIVE_FLAGS: &[&str] = &["--keep-open=yes", "--cache=yes", "--force-seekable=yes"];

/// Forward-slash form of a path — the safe spelling inside `appending://` /
/// `edl://` URL arguments on Windows.
pub(super) fn fwd_slashes(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Build the player invocation for a stream target. mpv gets live-view flags
/// and `appending://` URLs for growing files; other players get plain paths.
pub(super) fn build_player_command(player: &str, t: &StreamTarget) -> std::process::Command {
    let mut cmd = std::process::Command::new(player);
    let mpv = player_is_mpv(player);
    match t {
        StreamTarget::Finished(p) => {
            cmd.arg(p);
        }
        StreamTarget::Growing(p) => {
            if mpv {
                cmd.args(MPV_LIVE_FLAGS);
                cmd.arg(format!("appending://{}", fwd_slashes(p)));
            } else {
                cmd.arg(p);
            }
        }
        StreamTarget::SplitAv(parts) => {
            // Gated to mpv by playable_with; build the mpv form regardless.
            // Largest file (video) is the main file; the rest join as
            // external audio tracks. Each source is its own appending://
            // demuxer, so readahead and growth-following work normally
            // (unlike an edl:// merge, which starves the video stream).
            cmd.args(MPV_LIVE_FLAGS);
            let mut parts = parts.iter();
            if let Some(main) = parts.next() {
                cmd.arg(format!("appending://{}", fwd_slashes(main)));
            }
            for p in parts {
                cmd.arg(format!("--audio-file=appending://{}", fwd_slashes(p)));
            }
        }
        StreamTarget::Sequence(parts) => {
            // Plain paths in order: a playlist, not a merge. Each part is
            // already complete, so no `appending://` and no live flags — this
            // is finished material that simply hasn't been concatenated yet.
            for p in parts {
                cmd.arg(p);
            }
        }
    }
    cmd
}

/// Per-launch stderr sink for a live-edge tune-in's tools, under
/// `logs\player\{channel} - {time} - {tool}.log`.
///
/// These tools' stderr used to go straight to `Stdio::null()`, which meant a
/// live-edge mpv that froze or died left **no evidence anywhere** of why: the
/// yt-dlp feeding its pipe could have been killed, throttled, or refused a PO
/// token, and from outside all three look identical — a stopped picture (hit
/// 2026-07-31). The capture path has had per-tool logs for exactly this reason
/// all along; the player path never did.
///
/// Playback never depends on this: any failure to open the file degrades to
/// discarding the output, precisely the old behaviour.
fn player_log(channel: &str, tool: &str) -> std::process::Stdio {
    use std::process::Stdio;
    let safe: String = channel
        .chars()
        .map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { '_' })
        .collect();
    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    let dir = crate::app_paths::logs_dir().join("player");
    if crate::iomon::fs::create_dir_all_sync(crate::iomon::Cat::ToolLog, &dir).is_err() {
        return Stdio::null();
    }
    let path = dir.join(format!("{} - {stamp} - {tool}.log", safe.trim()));
    match crate::iomon::fs::open_with_sync(crate::iomon::Cat::ToolLog, &path, |o| {
        o.create(true).append(true);
    }) {
        Ok(f) => {
            tracing::debug!("play-new-instance: {tool} stderr -> {}", path.display());
            Stdio::from(f)
        }
        Err(e) => {
            warn!("play-new-instance: could not open a {tool} log ({e}) — discarding its output");
            Stdio::null()
        }
    }
}

// ----- Open-player presence (which instances are on screen right now) -----

/// monitor_id -> (players currently open, unix time the last one closed).
/// Fed by every live tune-in path that spawns a player for a TRACKED row
/// (streamlink, yt-dlp pipe, ffmpeg-source, the live-edge preview); read by
/// Follow-raid auto-play's "only when watching" gate via
/// [`monitor_watched_recently`]. In-memory only — a restart forgets, which
/// errs toward NOT popping players, the safe direction.
static OPEN_PLAYERS: std::sync::LazyLock<std::sync::Mutex<HashMap<i64, (u32, i64)>>> =
    std::sync::LazyLock::new(Default::default);

fn note_player_opened(monitor_id: i64) {
    if monitor_id <= 0 {
        return; // synthetic rows (follow-raid/collab partners) have no instance
    }
    OPEN_PLAYERS.lock().unwrap().entry(monitor_id).or_insert((0, 0)).0 += 1;
}

fn note_player_closed(monitor_id: i64) {
    if monitor_id <= 0 {
        return;
    }
    if let Some(e) = OPEN_PLAYERS.lock().unwrap().get_mut(&monitor_id) {
        e.0 = e.0.saturating_sub(1);
        e.1 = crate::models::now_unix();
    }
}

/// Whether a player for this instance is open right now, or closed within
/// `within_secs`. The grace window matters because a raid fires as the
/// source broadcast winds down — mpv often hits end-of-stream and closes
/// moments before the EventSub raid event arrives, and "I was literally
/// just watching" must still count as watching.
pub(crate) fn monitor_watched_recently(monitor_id: i64, within_secs: i64) -> bool {
    OPEN_PLAYERS
        .lock()
        .unwrap()
        .get(&monitor_id)
        .is_some_and(|&(open, closed_at)| {
            open > 0 || crate::models::now_unix() - closed_at <= within_secs
        })
}

/// Log and spawn a player/downloader command built for "play new instance",
/// returning a status-bar error message on spawn failure. `watch_monitor`
/// registers the spawned process in [`OPEN_PLAYERS`] as an open player for
/// that instance (a reaper thread notes the close) — pass it for live
/// tune-ins of a tracked row, `None` for everything else. `tile_rect`, if
/// set, moves/resizes the spawned window (or a descendant's — see
/// [`crate::window_placement::place_window_for_pid_tree`]) to that rect once
/// it appears; pass `None` when no tiling was requested, or when the command
/// already got mpv `--geometry` flags baked in via [`apply_tile_or_geometry`]
/// (no Win32 fallback needed in that case).
pub(super) fn spawn_logged(
    mut cmd: std::process::Command,
    what: &str,
    watch_monitor: Option<i64>,
    tile_rect: Option<crate::display::PixelRect>,
) -> Option<String> {
    let line = format!(
        "{} {}",
        cmd.get_program().to_string_lossy(),
        cmd.get_args().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ")
    );
    tracing::info!(%line, "play-new-instance: spawning {what}");
    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(rect) = tile_rect {
                crate::window_placement::place_window_for_pid_tree(
                    child.id(),
                    rect,
                    crate::window_placement::PLACEMENT_TIMEOUT,
                );
            }
            if let Some(mid) = watch_monitor.filter(|&m| m > 0) {
                note_player_opened(mid);
                // The streamlink path's child is streamlink itself, which
                // exits when the player it owns closes — waiting on either
                // process shape means "the window is gone".
                std::thread::spawn(move || {
                    let _ = child.wait();
                    note_player_closed(mid);
                });
            }
            None
        }
        Err(e) => {
            warn!(%line, "play-new-instance: failed to spawn {what}: {e}");
            Some(format!("Failed to launch {what}: {e}"))
        }
    }
}

/// Resolve a requested tile rect against the player: mpv gets the rect baked
/// in as `--geometry` flags at command-build time (no window-matching race),
/// everything else gets the rect back so the caller can pass it through to
/// [`spawn_logged`]'s Win32 poll-and-move fallback instead.
pub(super) fn apply_tile_or_geometry(
    cmd: &mut std::process::Command,
    player: &str,
    rect: Option<crate::display::PixelRect>,
) -> Option<crate::display::PixelRect> {
    let rect = rect?;
    if player_is_mpv(player) {
        cmd.args(crate::window_placement::mpv_geometry_args(rect));
        None
    } else {
        Some(rect)
    }
}

// ----- Live-edge player title (configurable, optionally auto-updating) -----

/// Substitute the tokens available in the "Live-edge player title" setting:
/// `{channel}`, `{game}`, `{title_trimmed}`, `{pos}` (current playback
/// position). `{pos}` only ever ticks live for the launch paths this app
/// spawns mpv directly for (YouTube/Kick/ffmpeg-source, via
/// [`mpv_live_title_value`]) — mpv's own `--title` flag supports property
/// expansion and keeps refreshing the window title on its own, unlike
/// `--force-media-title` (verified against mpv issue trackers: the latter
/// does not expand `${...}` properties, `--title` does). Streamlink (Twitch)
/// resolves its own `--title` template once at launch and forwards it to mpv
/// as `--force-media-title`, so the title it sets can never tick or update —
/// which is why that path asks Streamlink for an mpv IPC socket (see
/// [`streamlink_player_args`]) and replaces the whole title over IPC the
/// moment mpv is up, with a real `${time-pos}` in it. Every IPC push updates
/// BOTH title surfaces — the window `title` (ticking) and
/// `force-media-title` (static), the latter feeding the OSC/stats
/// `media-title` display (see [`send_mpv_title`]).
pub(super) fn render_live_title(template: &str, channel: &str, game: &str, title_trimmed: &str, pos: &str) -> String {
    template
        .replace("{channel}", channel)
        .replace("{game}", game)
        .replace("{title_trimmed}", title_trimmed)
        .replace("{pos}", pos)
}

/// The mpv `--title` VALUE for a live-edge spawn this app owns directly —
/// `{pos}` becomes mpv's own `${time-pos}` property-expansion token (left as
/// literal text, not a resolved value) so the title keeps ticking with
/// actual playback position with no polling on our side. Used both as the
/// initial `--title=` spawn argument and as the value pushed over IPC by
/// [`run_live_title_updater`] on a later change.
fn mpv_live_title_value(template: &str, channel: &str, last_title: &str, last_game: &str) -> String {
    let trimmed = crate::downloader::trim_title_commands(last_title);
    render_live_title(template, channel, last_game, &trimmed, "${time-pos}")
}

/// The static (non-ticking) render of the live title: `{pos}` is a fixed
/// `00:00:00` because neither consumer can expand properties. Used for two
/// things that must agree:
/// - the Streamlink `--title` VALUE at launch (not an mpv flag — Streamlink
///   resolves it once and forwards it to mpv as `--force-media-title`),
///   resolved from this app's own known metadata (not Streamlink's scrape)
///   so the same template means the same thing on every launch path;
/// - the `force-media-title` value pushed over IPC on every title update
///   (see [`send_mpv_title`]) — the OSC/stats "media title" surface, which
///   would otherwise stay frozen at the launch value forever.
fn static_live_title(template: &str, channel: &str, last_title: &str, last_game: &str) -> String {
    let trimmed = crate::downloader::trim_title_commands(last_title);
    render_live_title(template, channel, last_game, &trimmed, "00:00:00")
}

/// How often [`run_live_title_updater`] re-checks the monitor's current
/// title/game for a change worth pushing.
const LIVE_TITLE_POLL: std::time::Duration = std::time::Duration::from_secs(20);

/// How long to wait for mpv to create its IPC pipe before giving up on
/// auto-updating the title — this never blocks or fails the play action
/// itself, it just silently stays at the title set at launch. Used by the
/// paths where this app spawns mpv itself, so the pipe appears within
/// milliseconds.
const LIVE_TITLE_IPC_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The same wait for the Streamlink path, where *Streamlink* spawns mpv only
/// after resolving the stream and opening the HLS session — and, with
/// `--retry-streams 3 --retry-max 5`, possibly after ~15 s of retries first.
/// Generous because the cost of waiting is one thread polling a pipe open
/// every 200 ms, while the cost of being too short is a silently frozen title.
const LIVE_TITLE_IPC_CONNECT_TIMEOUT_STREAMLINK: std::time::Duration =
    std::time::Duration::from_secs(60);

/// A unique `\\.\pipe\...` path for one mpv instance's `--input-ipc-server`.
///
/// The process-wide counter is what makes it unique: a timestamp alone repeats
/// within the same second, and the collab "play every angle" action spawns
/// three or four players in one go. Two mpv instances handed the same pipe
/// name fight over it — the loser silently gets no IPC server at all, and
/// title pushes meant for it land on the winner's window instead.
fn new_mpv_ipc_pipe_path() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        r"\\.\pipe\streamarchiver-mpv-{}-{}-{seq}",
        std::process::id(),
        crate::models::now_unix()
    )
}

/// Write one command line to an already-open mpv JSON-IPC pipe
/// (newline-delimited JSON, mpv's documented IPC protocol), then drain any
/// queued replies. mpv answers *every* command, and this client never
/// otherwise reads — without the drain, each write would leave one more reply
/// rotting in the pipe's outbound buffer for the life of the window.
fn send_mpv_line(pipe: &mut std::fs::File, cmd: serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    pipe.write_all(cmd.to_string().as_bytes())?;
    pipe.write_all(b"\n")?;
    drain_mpv_replies(pipe);
    Ok(())
}

/// Push both of mpv's title surfaces — they are SEPARATE properties and
/// updating only one leaves the other visibly stale (2026-07-31: a Twitch
/// category change updated the win32 title bar but the OSC seekbar and the
/// stats overlay kept showing the launch-time title):
/// - `title` — the OS window title. Supports `${...}` property expansion, so
///   it gets the ticking `${time-pos}` render.
/// - `force-media-title` — feeds the `media-title` property that the OSC,
///   the stats overlay, and playlist labels display. No property expansion,
///   so it gets the static render (`{pos}` as `00:00:00`), same as the
///   Streamlink launch value it replaces.
///
/// mpv treats setting a property to its current value as a no-op, so
/// re-sending unchanged titles is a safe way to double as a liveness probe.
fn send_mpv_title(pipe: &mut std::fs::File, title: &str, media_title: &str) -> std::io::Result<()> {
    send_mpv_line(pipe, serde_json::json!({"command": ["set_property", "title", title]}))?;
    send_mpv_line(
        pipe,
        serde_json::json!({"command": ["set_property", "force-media-title", media_title]}),
    )
}

/// A liveness probe that changes nothing: any complete command line gets
/// answered (and the answer drained by [`send_mpv_line`]), and a failed write
/// is the only "window closed" signal a client that pushes titles ever gets —
/// once mpv exits, its end of the pipe is gone and the write errors.
fn poke_mpv(pipe: &mut std::fs::File) -> std::io::Result<()> {
    send_mpv_line(pipe, serde_json::json!({"command": ["get_property", "pid"]}))
}

/// Read-and-discard whatever replies mpv has queued on the pipe, without
/// blocking (`PeekNamedPipe` first, then exact-size reads). Replies arrive
/// asynchronously, so the drain right after a write usually collects the
/// *previous* write's reply — which is the point: the buffer stays at most
/// one reply deep instead of growing for the life of the window.
fn drain_mpv_replies(pipe: &mut std::fs::File) {
    use std::io::Read;
    let mut scratch = [0u8; 512];
    loop {
        let avail = crate::platform::pipe_bytes_available(pipe) as usize;
        if avail == 0 {
            return;
        }
        // Reading exactly what Peek reported can never block.
        let take = avail.min(scratch.len());
        if pipe.read_exact(&mut scratch[..take]).is_err() {
            return;
        }
    }
}

/// Poll for mpv's `--input-ipc-server` pipe until it exists, and open it
/// read+write — write for the commands, read so [`drain_mpv_replies`] can
/// keep the reply buffer empty. `None` = it never appeared within
/// `connect_timeout` (mpv never came up, the launch failed, or the player
/// isn't mpv after all), which is always best-effort: the window simply keeps
/// the title it launched with.
fn connect_mpv_ipc(
    pipe_path: &str,
    connect_timeout: std::time::Duration,
) -> Option<std::fs::File> {
    let deadline = std::time::Instant::now() + connect_timeout;
    loop {
        match crate::iomon::fs::open_with_sync(crate::iomon::Cat::Preview, pipe_path, |o| {
            o.read(true).write(true);
        }) {
            Ok(f) => return Some(f),
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => {
                warn!("live-title: mpv IPC pipe never came up, auto-update disabled: {e}");
                return None;
            }
        }
    }
}

/// Background loop started by [`apply_live_title_and_spawn_updater`] (and by
/// the Streamlink branch of [`spawn_play_new_instance`]): connect to
/// `pipe_path`, then re-render `monitor_id`'s title from its row and push it
/// every [`LIVE_TITLE_POLL`]. Exits once a pipe write fails (mpv closed its
/// IPC server, meaning the player quit) or `connect_timeout` elapses (mpv
/// never came up, or doesn't support `--input-ipc-server`).
///
/// The push-on-connect isn't redundant: on the Streamlink path our `--title`
/// never reaches mpv at all (Streamlink resolves its own `--title` once and
/// hands mpv `--force-media-title`, verified against Streamlink 8's
/// `PlayerArgsMPV::get_title`), so this first push is what puts the configured
/// template on the window — and starts `{pos}` ticking.
///
/// The push is deliberately unconditional, not gated on a title/game change:
/// the write doubles as the liveness probe. A change-gated loop only ever
/// noticed the player closing when the channel next retitled, so a stable
/// 12-hour stream left this thread polling the DB behind a long-closed window
/// for the rest of the session. Re-sending an unchanged title is an mpv-side
/// no-op, and [`send_mpv_line`]'s reply drain keeps the writes from
/// accumulating anything.
fn run_live_title_updater(
    pipe_path: String,
    store: Arc<crate::store::Store>,
    monitor_id: i64,
    template: String,
    connect_timeout: std::time::Duration,
) {
    let Some(mut pipe) = connect_mpv_ipc(&pipe_path, connect_timeout) else { return };
    tracing::debug!(monitor_id, "live-title: mpv IPC connected, pushing title");
    let mut pushed: Option<(String, String)> = None;
    loop {
        // Fresh render while the monitor row is readable; the last pushed
        // values once it isn't (monitor deleted mid-play) — something must go
        // down the pipe every round or the close goes unnoticed again.
        let value = match store.get_monitor_with_channel(monitor_id) {
            Ok(Some(row)) => (
                mpv_live_title_value(&template, &row.channel.name, &row.last_title, &row.last_game),
                static_live_title(&template, &row.channel.name, &row.last_title, &row.last_game),
            ),
            _ => match pushed.clone() {
                Some(v) => v,
                // Row unreadable before anything was ever pushed: no channel
                // name to render with, and nothing to probe with either.
                None => return,
            },
        };
        if send_mpv_title(&mut pipe, &value.0, &value.1).is_err() {
            return; // player closed
        }
        pushed = Some(value);
        std::thread::sleep(LIVE_TITLE_POLL);
    }
}

/// Add the configured live-edge title to an mpv `Command` this app spawns
/// directly, and — when enabled — start [`run_live_title_updater`] once the
/// player is running. Best-effort only: never blocks or fails the play
/// action, and is a no-op for anything other than mpv (only mpv understands
/// `--title` this way) or a blank template (old default-title behavior).
#[allow(clippy::too_many_arguments)]
fn apply_live_title_and_spawn_updater(
    cmd: &mut std::process::Command,
    player: &str,
    row: &crate::models::MonitorWithChannel,
    template: &str,
    auto_update: bool,
    store: &Arc<crate::store::Store>,
    meta: Option<&LiveMetaCtx>,
) {
    let (monitor_id, channel) = (row.monitor.id, row.channel.name.as_str());
    let (last_title, last_game) = (row.last_title.as_str(), row.last_game.as_str());
    if !player_is_mpv(player) || template.is_empty() {
        return;
    }
    cmd.arg(format!("--title={}", mpv_live_title_value(template, channel, last_title, last_game)));
    if !auto_update {
        return;
    }
    // A synthetic row (id 0: follow-raid/collab partner) has no monitor to
    // poll, so it gets the deferred Helix fetch instead of the DB updater.
    if monitor_id == 0 {
        if let Some(plan) = plan_untracked_title(meta, row, player, template) {
            cmd.arg(format!("--input-ipc-server={}", plan.pipe_path));
            plan.start(LIVE_TITLE_IPC_CONNECT_TIMEOUT);
        }
        return;
    }
    let pipe_path = new_mpv_ipc_pipe_path();
    cmd.arg(format!("--input-ipc-server={pipe_path}"));
    spawn_live_title_updater(pipe_path, store, monitor_id, template, LIVE_TITLE_IPC_CONNECT_TIMEOUT);
}

// ----- Untracked-row (collab partner / raid target) title, fetched after launch -----

/// How often [`run_untracked_title_updater`] re-queries Helix for a partner's
/// current title/game. Deliberately far slower than [`LIVE_TITLE_POLL`]: a
/// tracked monitor's updater reads a row this app already keeps fresh, while
/// every tick here is a real Helix call against the shared quota.
const UNTRACKED_TITLE_POLL: std::time::Duration = std::time::Duration::from_secs(120);

/// What a deferred (post-launch) title fetch needs: the app's shared detection
/// context — its HTTP client *and* its cached Twitch tokens — plus a runtime
/// handle to drive the request on.
///
/// Threaded through the play actions as an `Option`, so every path degrades to
/// exactly the old behavior (launch title only) when the app core hasn't
/// published a context yet.
#[derive(Clone)]
pub(crate) struct LiveMetaCtx {
    ctx: Arc<crate::detectors::DetectContext>,
    rt: tokio::runtime::Handle,
}

impl LiveMetaCtx {
    /// Build one from the app core, or `None` before [`crate::app_core::AppCore::start`]
    /// has run.
    pub(crate) fn from_core(core: &crate::app_core::AppCore) -> Option<Self> {
        core.detect_ctx().map(|ctx| LiveMetaCtx { ctx, rt: core.rt.clone() })
    }

    /// Build one inside an async task that already holds a context (the
    /// auto-play side of Follow raid).
    pub(crate) fn from_ctx(ctx: &Arc<crate::detectors::DetectContext>) -> Self {
        LiveMetaCtx { ctx: Arc::clone(ctx), rt: tokio::runtime::Handle::current() }
    }
}

/// Fill in the window title of a player opened for a channel this app doesn't
/// track — a collab partner or a raid target — *after* it has launched.
///
/// These rows are synthetic (`monitor.id == 0`, see [`spawn_play_collab_partner`]
/// and [`spawn_follow_raid`]): there is no monitor row, so no stored title or
/// game, so the configured template renders with `{game}`/`{title_trimmed}`
/// empty and the window says little more than the channel name. The metadata
/// exists — it's one Helix `GET /streams` away — but blocking the play action
/// on a network round-trip to get it would be the wrong trade: tuning in must
/// stay instant.
///
/// So the fetch happens here instead, on a background thread, and the result
/// is pushed into the already-running player over mpv's IPC socket.
///
/// **The first fetch runs before the pipe connect on purpose.** Connecting
/// polls for mpv's pipe for up to a minute on the Streamlink path (Streamlink
/// resolves the stream and *then* spawns the player), so the Helix round-trip
/// hides inside a wait that was happening anyway — and the very first push
/// then already carries real metadata, instead of writing a blank template
/// and correcting it a moment later.
///
/// **Poke first, fetch second.** Each refresh round probes the pipe
/// ([`poke_mpv`]) before touching Helix, so a window that has been closed
/// costs exactly zero further API calls — the poke fails and the thread
/// exits. That ordering is what makes an API-backed refresh loop safe to run
/// at all; it still ticks at the deliberately slow [`UNTRACKED_TITLE_POLL`]
/// (vs. [`LIVE_TITLE_POLL`]) because a tracked row's refresh is a local DB
/// read while every round here is a real Helix call, and "Play all collab
/// instances" can open four of these windows in one click.
fn run_untracked_title_updater(
    meta: LiveMetaCtx,
    pipe_path: String,
    url: String,
    channel: String,
    template: String,
    connect_timeout: std::time::Duration,
) {
    use crate::detectors::MetaFetch;

    let mut fetched = meta.rt.block_on(meta.ctx.twitch_stream_meta(&url));
    let Some(mut pipe) = connect_mpv_ipc(&pipe_path, connect_timeout) else { return };
    let mut pushed = String::new();
    loop {
        // `Offline`/`Failed` leave the launch title alone — an untracked
        // partner that already ended, or a Helix hiccup, must not blank a
        // window title that at least still names the channel. The next round
        // retries, so a hiccup at tune-in still resolves.
        if let MetaFetch::Live(m) = &fetched {
            let value = mpv_live_title_value(&template, &channel, &m.title, &m.game);
            if value != pushed {
                let media = static_live_title(&template, &channel, &m.title, &m.game);
                if send_mpv_title(&mut pipe, &value, &media).is_err() {
                    return; // player closed
                }
                tracing::debug!(%channel, "live-title: pushed fetched title for untracked row");
                pushed = value;
            }
        }
        std::thread::sleep(UNTRACKED_TITLE_POLL);
        if poke_mpv(&mut pipe).is_err() {
            return; // player closed — this round costs no API call
        }
        fetched = meta.rt.block_on(meta.ctx.twitch_stream_meta(&url));
    }
}

/// A decided-but-not-yet-started deferred title fetch: the IPC pipe path to
/// hand the player, plus everything [`run_untracked_title_updater`] will need.
///
/// Deciding and starting are separate steps because the Streamlink path can't
/// do both at once — it has to put `--input-ipc-server` on the command line
/// *before* launching, but must not start the updater until it knows the
/// launch succeeded (otherwise a failed spawn leaves a thread polling for a
/// pipe that can never appear, ending in a misleading warning).
struct UntrackedTitlePlan {
    pipe_path: String,
    meta: LiveMetaCtx,
    url: String,
    channel: String,
    template: String,
}

/// Whether a deferred title fetch is worth doing for this row.
///
/// All four conditions are load-bearing:
/// - `monitor.id == 0` — a *tracked* row already has stored title/game and its
///   own updater polling the DB for free; fetching would duplicate that at
///   Helix's expense.
/// - non-empty template — with no template there is no title to render.
/// - mpv — the IPC socket that carries the result is mpv-only.
/// - Twitch — the fetch is Helix `GET /streams`. Collab partners are
///   Twitch-only by construction, and so are raid targets, but a synthetic
///   row inherits its tool from whichever instance it was launched from, so
///   this is checked rather than assumed.
fn row_wants_deferred_title(
    row: &crate::models::MonitorWithChannel,
    player: &str,
    template: &str,
) -> bool {
    row.monitor.id == 0
        && !template.is_empty()
        && player_is_mpv(player)
        && row.monitor.platform() == crate::models::Platform::Twitch
}

/// Decide whether this row can use a deferred title fetch (see
/// [`row_wants_deferred_title`]) and the app core has published a detection
/// context to do it with. Allocates the pipe path but starts nothing; call
/// [`UntrackedTitlePlan::start`] for that.
fn plan_untracked_title(
    meta: Option<&LiveMetaCtx>,
    row: &crate::models::MonitorWithChannel,
    player: &str,
    template: &str,
) -> Option<UntrackedTitlePlan> {
    let meta = meta?;
    if !row_wants_deferred_title(row, player, template) {
        return None;
    }
    Some(UntrackedTitlePlan {
        pipe_path: new_mpv_ipc_pipe_path(),
        meta: meta.clone(),
        url: row.monitor.url.clone(),
        channel: row.channel.name.clone(),
        template: template.to_string(),
    })
}

impl UntrackedTitlePlan {
    /// Run [`run_untracked_title_updater`] on its own thread.
    fn start(self, connect_timeout: std::time::Duration) {
        std::thread::spawn(move || {
            run_untracked_title_updater(
                self.meta, self.pipe_path, self.url, self.channel, self.template, connect_timeout,
            );
        });
    }
}

/// Start [`run_live_title_updater`] on its own thread. Shared by the paths
/// that spawn mpv directly (via [`apply_live_title_and_spawn_updater`]) and
/// the Streamlink path, which can only reach mpv through `--player-args`.
fn spawn_live_title_updater(
    pipe_path: String,
    store: &Arc<crate::store::Store>,
    monitor_id: i64,
    template: &str,
    connect_timeout: std::time::Duration,
) {
    let store = Arc::clone(store);
    let template = template.to_string();
    std::thread::spawn(move || {
        run_live_title_updater(pipe_path, store, monitor_id, template, connect_timeout);
    });
}

/// The mpv flags this app needs Streamlink to pass on its behalf, as a
/// `--player-args` VALUE — Streamlink spawns the player there, so anything mpv
/// needs has to travel through this one string.
///
/// Two quoting rules, both verified against Streamlink 8.1.0's
/// `PlayerArgs.build()`:
///
/// - Values are `shlex.split` in **POSIX mode**, on Windows too, so a bare
///   `\\.\pipe\name` comes out the far end as `.pipename` — every backslash
///   eaten as an escape. Single-quoting the value makes shlex pass it through
///   byte-for-byte.
/// - The stream input placeholder is `{playerinput}`. `{filename}` was
///   Streamlink ≤5 spelling and is now an *unknown* variable, which its
///   formatter leaves as literal text — mpv then gets `{filename}` as a
///   playlist entry (a file that doesn't exist) *and* the real input appended
///   after it, since Streamlink adds the input itself when the placeholder is
///   missing.
fn streamlink_player_args(ipc_pipe: Option<&str>, mute: bool, geometry: Option<&str>) -> Option<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(pipe) = ipc_pipe {
        args.push(format!("--input-ipc-server='{pipe}'"));
    }
    if mute {
        args.push("--mute".into());
    }
    if let Some(g) = geometry {
        args.push(g.to_string());
    }
    (!args.is_empty()).then(|| format!("{} {{playerinput}}", args.join(" ")))
}

/// Root for throwaway live-edge preview downloads (see [`spawn_live_preview`]).
pub(super) fn preview_root() -> std::path::PathBuf {
    std::env::temp_dir().join("streamarchiver-preview")
}

/// Best-effort sweep of preview dirs older than a day (leftovers from previews
/// orphaned by an app exit — their downloader dies when the stream ends, but
/// the files linger until this runs on the next preview).
pub(super) fn sweep_stale_previews() {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    let Ok(rd) = crate::iomon::fs::read_dir_sync(crate::iomon::Cat::Preview, preview_root()) else { return };
    for entry in rd.flatten() {
        if entry.metadata().and_then(|m| m.modified()).map(|t| t < cutoff).unwrap_or(false) {
            let _ = crate::iomon::fs::remove_dir_all_sync(crate::iomon::Cat::Preview, entry.path());
        }
    }
}

/// Spawn a throwaway live-edge download into a temp dir and open its growing
/// capture in the player once it buffers — "tune in now" for YouTube streams.
///
/// This is the only viable live-edge path for SABR-only streams: they can't be
/// piped to stdout (yt-dlp PR #13515), stock yt-dlp sees no formats for a
/// player's URL handler, and seeking to the end of the main recording's
/// growing cue-less MKV means a multi-GB linear scan. A fresh live-edge
/// download's files BEGIN at the edge, so the player just plays from 0.
///
/// A watcher thread polls the temp dir, launches the player when a playable
/// target appears, waits for the player to exit, then kills the downloader
/// tree and deletes the temp dir. If the app exits first the downloader is
/// orphaned but self-limiting (it dies when the stream ends); leftovers are
/// swept by [`sweep_stale_previews`].
pub(super) fn spawn_live_preview(
    row: &crate::models::MonitorWithChannel,
    player: &str,
    settings: &SettingsForm,
    store: &Arc<crate::store::Store>,
    mute: bool,
    title_template_override: Option<&str>,
    meta: Option<&LiveMetaCtx>,
) -> Option<String> {
    use crate::downloader::{load_ytdlp_bins, resolve_auth, sabr_preview_args, split_args, youtube_live_url, AuthSource};
    use crate::models::Platform;

    let m = &row.monitor;
    sweep_stale_previews();

    let mut bins = load_ytdlp_bins(store);
    let use_sabr = m.platform() == Platform::YouTube && bins.sabr.usable();
    // Same client policy as captures: public broadcasts preview via the
    // no-PO-token primary client (default tv) so an attestation wave can't
    // kill the preview downloader mid-watch; members-only stays on web.
    if use_sabr {
        crate::downloader::apply_yt_client_policy(store, m.id, &mut bins.sabr);
    }
    if use_sabr && !player_is_mpv(player) {
        return Some("Live-edge preview of SABR streams requires mpv as the media player".into());
    }

    let tmp = preview_root().join(format!("{}-{}", m.id, crate::models::now_unix()));
    let cache = tmp.join(crate::downloader::CACHE_DIR_NAME);
    if let Err(e) = crate::iomon::fs::create_dir_all_sync(crate::iomon::Cat::Preview, &cache) {
        return Some(format!("Failed to create preview dir: {e}"));
    }

    // The settings form splits browser and profile; downloads need the
    // composed "browser:profile" form (a bare "firefox" would hit the
    // default profile, not the one holding the YouTube login).
    let cookies = compose_browser_profile(&settings.cookies_browser, &settings.cookies_profile);
    let mut auth = resolve_auth(row, &settings.download_auth_method, &cookies);
    // Anonymous public YouTube (same rule as captures): account cookies only
    // for members-only monitors.
    if m.platform() == Platform::YouTube
        && crate::downloader::yt_public_auth(store, m.id, &mut auth)
    {
        tracing::info!(monitor_id = m.id, "live-preview: running anonymously (public broadcast)");
    }
    let extra = split_args(&m.extra_args);
    let global_args = split_args(&settings.ytdlp_default_args);
    // Downloader writes into <tmp>\.cache\preview.*, matching the app's capture
    // convention so stream_target_for_active(<tmp>\preview.mkv) finds it.
    let probe_path = tmp.join("preview.mkv");
    let (program, args) = if use_sabr {
        (
            bins.sabr.binary.clone(),
            sabr_preview_args(&cache.join("preview.mkv"), &auth, &global_args, &bins.sabr, &extra, &m.url),
        )
    } else {
        let mut args = vec![
            "--no-part".to_string(),
            "--hls-use-mpegts".into(),
            "-o".into(),
            cache.join("preview.ts").to_string_lossy().into_owned(),
            "--no-live-from-start".into(),
        ];
        match &auth {
            AuthSource::CookiesBrowser(b) => {
                args.push("--cookies-from-browser".into());
                args.push(b.clone());
            }
            AuthSource::CookiesFile(p) => {
                args.push("--cookies".into());
                args.push(p.clone());
            }
            _ => {}
        }
        args.extend(global_args);
        if m.platform() == Platform::YouTube && !bins.sabr.pot_args.is_empty() {
            args.push("--extractor-args".into());
            args.push(bins.sabr.pot_args.clone());
        }
        args.extend(extra);
        args.push(if m.platform() == Platform::YouTube {
            youtube_live_url(&m.url)
        } else {
            m.url.clone()
        });
        (bins.system_program(), args)
    };

    let log_path = tmp.join("preview.log");
    let (log_out, log_err) = match crate::iomon::fs::create_sync(crate::iomon::Cat::Preview, &log_path)
        .and_then(|f| Ok((f.try_clone()?, f)))
    {
        Ok(pair) => pair,
        Err(e) => {
            let _ = crate::iomon::fs::remove_dir_all_sync(crate::iomon::Cat::Preview, &tmp);
            return Some(format!("Failed to create preview log: {e}"));
        }
    };
    let line = format!("{program} {}", args.join(" "));
    tracing::info!(%line, "live-preview: spawning downloader");
    let mut dl = match std::process::Command::new(&program)
        .args(&args)
        .stdout(log_out)
        .stderr(log_err)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = crate::iomon::fs::remove_dir_all_sync(crate::iomon::Cat::Preview, &tmp);
            warn!(%line, "live-preview: failed to spawn downloader: {e}");
            return Some(format!("Failed to launch downloader for live preview: {e}"));
        }
    };

    let msg = format!(
        "Starting live-edge preview of {} — the player opens once the stream buffers (~10-30 s)",
        row.channel.name
    );
    let player = player.to_string();
    let channel = row.channel.name.clone();
    // The whole row travels into the thread (rather than the four title fields
    // separately) so the launch-time title helper can also decide whether this
    // is an untracked row needing a deferred metadata fetch.
    let title_row = row.clone();
    let title_template = title_template_override.unwrap_or(settings.live_title_template.trim()).to_string();
    let title_auto_update = settings.live_title_auto_update;
    let title_store = Arc::clone(store);
    let title_meta = meta.cloned();
    std::thread::spawn(move || {
        let cleanup = |dl: &mut std::process::Child, tmp: &std::path::Path| {
            let pid = dl.id().to_string();
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = dl.kill(); // fallback; no-op if taskkill got it
            let _ = dl.wait();
            for _ in 0..10 {
                if crate::iomon::fs::remove_dir_all_sync(crate::iomon::Cat::Preview, tmp).is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        };
        // Lossy read: yt-dlp's console output isn't guaranteed UTF-8.
        let log_tail = |tmp: &std::path::Path| -> String {
            let bytes = crate::iomon::fs::read_sync(crate::iomon::Cat::Preview, tmp.join("preview.log")).unwrap_or_default();
            let s = String::from_utf8_lossy(&bytes);
            let cut = s.char_indices().rev().nth(599).map(|(i, _)| i).unwrap_or(0);
            s[cut..].to_string()
        };
        // What the downloader has produced so far (true handle sizes — the
        // dir-entry sizes are stale for open files), for timeout diagnostics.
        let cache_listing = |tmp: &std::path::Path| -> String {
            let Ok(rd) = crate::iomon::fs::read_dir_sync(crate::iomon::Cat::Preview, tmp.join(crate::downloader::CACHE_DIR_NAME)) else { return String::new() };
            rd.flatten()
                .map(|e| {
                    let len = live_file_len(&e.path()).unwrap_or(0);
                    format!("{} ({len} B)", e.file_name().to_string_lossy())
                })
                .collect::<Vec<_>>()
                .join(", ")
        };

        // Wait for a playable target: SABR needs both A/V parts (SplitAv), but
        // settle for a single growing file if no second part shows up shortly.
        let mut growing_since: Option<std::time::Instant> = None;
        let mut target: Option<StreamTarget> = None;
        let probe = probe_path.to_string_lossy().into_owned();
        for _ in 0..240 {
            if let Ok(Some(status)) = dl.try_wait() {
                warn!(
                    %channel,
                    %status,
                    tail = %log_tail(&tmp),
                    "live-preview: downloader exited before producing a playable stream"
                );
                cleanup(&mut dl, &tmp);
                return;
            }
            match stream_target_for_active(&probe).filter(|t| playable_with(t, &player)) {
                Some(t @ StreamTarget::SplitAv(_)) => {
                    target = Some(t);
                    break;
                }
                Some(t) => {
                    let since = *growing_since.get_or_insert_with(std::time::Instant::now);
                    if !use_sabr || since.elapsed() >= std::time::Duration::from_secs(4) {
                        target = Some(t);
                        break;
                    }
                }
                None => growing_since = None,
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let Some(target) = target else {
            warn!(
                %channel,
                preview_temp_files = %cache_listing(&tmp),
                tail = %log_tail(&tmp),
                "live-preview: no playable stream within 2 minutes"
            );
            cleanup(&mut dl, &tmp);
            return;
        };
        tracing::info!(%channel, ?target, "live-preview: buffered, launching player");
        // Split SABR previews are served through a generated live HLS playlist
        // — the only transport that follows the growing files at the live edge
        // indefinitely (appending:// latches EOF after one lost race against
        // the segment cadence). Non-ISOBMFF variants and single files fall
        // back to direct appending:// playback.
        let launched: Option<(std::process::Child, Option<crate::hls_preview::HlsPreview>)> =
            (|| {
                if let StreamTarget::SplitAv(parts) = &target
                    && parts.len() >= 2
                    && let Some(mut hp) =
                        crate::hls_preview::HlsPreview::open(&parts[0], &parts[1], &tmp)
                {
                    // Playlists need ≥2 coalesced segments per track
                    // (~10 s of media) before there's anything to play.
                    let mut ready = hp.tick(false);
                    for _ in 0..60 {
                        if ready {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        ready = hp.tick(false);
                    }
                    if ready {
                        tracing::info!(%channel, "live-preview: serving live HLS playlist");
                        let mut cmd = std::process::Command::new(&player);
                        cmd.args(MPV_LIVE_FLAGS);
                        // Segments end in ".part", which lavf's HLS demuxer
                        // blocks by default for local files.
                        cmd.arg("--demuxer-lavf-o=allowed_extensions=ALL");
                        cmd.arg(fwd_slashes(&hp.master_path()));
                        if mute {
                            cmd.arg("--mute");
                        }
                        apply_live_title_and_spawn_updater(
                            &mut cmd, &player, &title_row, &title_template, title_auto_update,
                            &title_store, title_meta.as_ref(),
                        );
                        match cmd.spawn() {
                            Ok(p) => return Some((p, Some(hp))),
                            Err(e) => {
                                warn!(%channel, "live-preview: failed to launch player: {e}");
                                return None;
                            }
                        }
                    }
                    warn!(%channel, "live-preview: HLS playlists not ready in time, falling back to appending://");
                }
                let mut cmd = build_player_command(&player, &target);
                if mute && player_is_mpv(&player) {
                    cmd.arg("--mute");
                }
                apply_live_title_and_spawn_updater(
                    &mut cmd, &player, &title_row, &title_template, title_auto_update,
                    &title_store, title_meta.as_ref(),
                );
                match cmd.spawn() {
                    Ok(p) => Some((p, None)),
                    Err(e) => {
                        warn!(%channel, "live-preview: failed to launch player: {e}");
                        None
                    }
                }
            })();
        // This thread owns the player Child, so open/close presence for the
        // watched-instance registry is tracked inline (see OPEN_PLAYERS).
        let watch_mid = title_row.monitor.id;
        if launched.is_some() {
            note_player_opened(watch_mid);
        }
        match launched {
            Some((mut p, Some(mut hp))) => {
                // Keep the playlists fresh while the player runs. When the
                // preview download ends (stream over / killed), write final
                // playlists with ENDLIST so the player finishes cleanly
                // instead of polling forever.
                let mut dl_ended = false;
                loop {
                    if !matches!(p.try_wait(), Ok(None)) {
                        break;
                    }
                    if !dl_ended && !matches!(dl.try_wait(), Ok(None)) {
                        dl_ended = true;
                        hp.tick(true);
                    } else if !dl_ended {
                        hp.tick(false);
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                note_player_closed(watch_mid);
                tracing::info!(%channel, "live-preview: player closed, stopping preview download");
            }
            Some((mut p, None)) => {
                let _ = p.wait();
                note_player_closed(watch_mid);
                tracing::info!(%channel, "live-preview: player closed, stopping preview download");
            }
            None => {}
        }
        cleanup(&mut dl, &tmp);
    });

    Some(msg)
}

/// Spawn a "play stream (live edge)" command — tunes into the stream at the
/// LIVE EDGE in the configured media player, without recording. (⏵ "Play
/// local recording (start)" is the from-start counterpart: it opens the
/// in-progress capture.) Returns a status-bar message to show the user, or
/// `None`.
///
/// - Streamlink: `--player <path>` routes output to the player (live edge).
///   The live-edge title (see `render_live_title`) is passed via Streamlink's
///   own `--title`, since Streamlink — not this app — owns the player process
///   here. With mpv + auto-update on, this path additionally asks Streamlink
///   to hand mpv an `--input-ipc-server` socket (see
///   [`streamlink_player_args`]) and drives the title over that, which is how
///   it keeps up with title/game changes despite not owning the process.
/// - yt-dlp + YouTube: throwaway live-edge preview download, see
///   [`spawn_live_preview`] — SABR-only streams can't be piped or URL-played.
/// - yt-dlp + other platforms (Kick): pipes `-o -` stdout to the player's
///   stdin (from the live edge — from-start capture can't pipe). This app
///   spawns the player directly, so the title can auto-update (mpv only).
/// - ffmpeg source: passes the URL directly. Same direct-spawn title support
///   as the yt-dlp pipe case.
///
/// `mute` (mpv only) and `title_template_override` are for the collab-instance
/// play actions: muting every OTHER angle so they don't all play audio at
/// once, and a separate template for a synthetic (untracked-partner) row
/// that has no real title/game to fill the normal template's tokens with.
/// Both are no-ops (`false`/`None`) for a normal single-instance play.
///
/// `is_vod`, set by [`spawn_play_vod`], skips everything here that only
/// makes sense for a LIVE tune-in: marking a currently-recording broadcast
/// as "started" (a VOD play isn't watching whatever's live right now),
/// YouTube's SABR live-preview workaround (VODs aren't SABR-restricted, so
/// the plain yt-dlp pipe branch handles them fine), and the live-title
/// auto-update IPC dance (there's nothing "live" left to track — the title
/// is set once at launch and never touched again).
pub(super) fn spawn_play_new_instance(
    row: &crate::models::MonitorWithChannel,
    player: &str,
    settings: &SettingsForm,
    store: &Arc<crate::store::Store>,
    mute: bool,
    title_template_override: Option<&str>,
    meta: Option<&LiveMetaCtx>,
    is_vod: bool,
    // Requested "Play all collab instances" tile placement, if any — `None`
    // everywhere else. Not threaded into the `Tool::YtDlp` YouTube live-edge
    // preview branch ([`spawn_live_preview`]): collab is a Twitch-only
    // feature (`models::CollabPartner`), so that branch is unreachable from
    // any tiled call site.
    tile_rect: Option<crate::display::PixelRect>,
) -> Option<String> {
    use crate::downloader::{
        push_track_args, resolve_auth, resolved_quality, split_args, AuthSource,
    };
    use crate::models::{Platform, Tool};

    let m = &row.monitor;
    // Watching at the live edge counts as "started" for whichever broadcast
    // is currently recording for this monitor, if any — mirrors the
    // finished-file playback hook in `ui/streams.rs`. A live-only tune-in
    // with nothing actively recording has no broadcast identity yet, so
    // there's nothing to mark (correctly a no-op). Skipped for a VOD play:
    // an old broadcast being watched has no bearing on whatever's currently
    // recording for this monitor, if anything.
    if !is_vod && let Ok(Some(rec)) = store.current_recording_for_monitor(m.id) {
        let key = crate::models::stream_key(&rec);
        let cur = store.stream_watch_state(&key).ok().flatten().map(|(s, _)| s);
        if history::should_advance_to_started(cur.as_deref()) {
            let _ = store.set_stream_watch_state(&key, m.id, "started");
        }
    }
    // The settings form splits browser and profile; downloads need the
    // composed "browser:profile" form (a bare "firefox" would hit the
    // default profile, not the one holding the YouTube login).
    let cookies = compose_browser_profile(&settings.cookies_browser, &settings.cookies_profile);
    let mut auth = resolve_auth(row, &settings.download_auth_method, &cookies);
    // Anonymous public YouTube (same rule as captures): account cookies only
    // for members-only monitors.
    if m.platform() == Platform::YouTube
        && crate::downloader::yt_public_auth(store, m.id, &mut auth)
    {
        tracing::info!(monitor_id = m.id, "play: running anonymously (public YouTube)");
    }
    let extra: Vec<String> = split_args(&m.extra_args);
    // The global auto-update setting would otherwise poll this monitor's
    // CURRENT (possibly still-live) title/game into a VOD player's title —
    // wrong for something that isn't live anymore.
    let auto_update = settings.live_title_auto_update && !is_vod;

    match m.tool {
        Tool::Streamlink => {
            let mut args: Vec<String> = Vec::new();
            if m.platform() == Platform::Twitch {
                args.push("--twitch-supported-codecs=h264,h265,av1".into());
                if let AuthSource::Token(ref t) = auth {
                    args.push(format!("--twitch-api-header=Authorization=OAuth {t}"));
                }
            }
            // No --hls-live-restart even for from-start monitors: ▷ means
            // "tune in at the live edge"; ⏵ covers from-start viewing.
            args.push("--retry-streams".into());
            args.push("3".into());
            args.push("--retry-max".into());
            args.push("5".into());
            push_track_args(&mut args, Tool::Streamlink, &m.audio_tracks, &m.subtitle_tracks, false);
            args.extend(extra);
            let title_template = title_template_override.unwrap_or(settings.live_title_template.trim());
            if !title_template.is_empty() {
                args.push("--title".into());
                args.push(static_live_title(
                    title_template, &row.channel.name, &row.last_title, &row.last_game,
                ));
            }
            args.push("--player".into());
            args.push(player.to_string());
            // Streamlink owns the player process here, so mpv's IPC socket —
            // the only way to update the title after launch, since Streamlink
            // resolves its own --title once and never revisits it — has to be
            // requested through --player-args.
            //
            // A synthetic row (id 0: follow-raid/collab partner) has no monitor
            // to poll, so it takes the deferred-fetch updater instead: same
            // socket, metadata from Helix rather than from the DB.
            let untracked = auto_update
                .then(|| plan_untracked_title(meta, row, player, title_template))
                .flatten();
            let ipc_pipe = match &untracked {
                Some(plan) => Some(plan.pipe_path.clone()),
                None => (!title_template.is_empty()
                    && auto_update
                    && player_is_mpv(player)
                    && m.id != 0)
                    .then(new_mpv_ipc_pipe_path),
            };
            // A separate --player-args flag, NOT embedded in --player's own
            // value: Streamlink re-splits `--player` as its own command line,
            // and on Windows a value handed to it as "<path> --mute" can come
            // back mis-split into a single executable name — reported live as
            // `error: Failed to start player: C:\...\mpv.exe --mute (Player
            // executable not found)`. --player-args isn't re-split against the
            // player path, so it can't suffer the same failure.
            // Streamlink itself doesn't understand --geometry — an mpv tile
            // request has to ride inside --player-args like --mute/the IPC
            // socket above; a non-mpv player instead falls through to
            // spawn_logged's Win32 fallback below (streamlink spawns it as a
            // child, so the pid tree walk still finds its window).
            let mpv_geometry =
                tile_rect.filter(|_| player_is_mpv(player)).map(|r| r.geometry_arg());
            if let Some(player_args) = streamlink_player_args(
                ipc_pipe.as_deref(),
                mute && player_is_mpv(player),
                mpv_geometry.as_deref(),
            ) {
                args.push("--player-args".into());
                args.push(player_args);
            }
            args.push(m.url.clone());
            args.push(resolved_quality(&m.quality));
            let mut cmd = std::process::Command::new("streamlink");
            // Streamlink spawns mpv itself here, so its stderr is the only
            // window into BOTH processes for the Twitch path.
            cmd.args(&args).stderr(player_log(&row.channel.name, "streamlink"));
            let win32_tile_rect = if mpv_geometry.is_some() { None } else { tile_rect };
            let status = spawn_logged(cmd, "streamlink", Some(m.id), win32_tile_rect);
            // Both updaters start only if Streamlink actually started —
            // otherwise one would sit on a pipe that can never appear and log
            // a spurious warning a minute later.
            if status.is_none() {
                match untracked {
                    Some(plan) => plan.start(LIVE_TITLE_IPC_CONNECT_TIMEOUT_STREAMLINK),
                    None => {
                        if let Some(pipe) = ipc_pipe {
                            spawn_live_title_updater(
                                pipe, store, m.id, title_template,
                                LIVE_TITLE_IPC_CONNECT_TIMEOUT_STREAMLINK,
                            );
                        }
                    }
                }
            }
            status
        }
        Tool::YtDlp if !is_vod && m.platform() == Platform::YouTube => {
            spawn_live_preview(row, player, settings, store, mute, title_template_override, meta)
        }
        Tool::YtDlp => {
            let ytdlp_bin = if settings.ytdlp_binary_path.trim().is_empty() {
                "yt-dlp".to_string()
            } else {
                settings.ytdlp_binary_path.trim().to_string()
            };
            let global_args = split_args(&settings.ytdlp_default_args);
            let mut args = vec![
                "--no-part".to_string(),
                "--hls-use-mpegts".into(),
                "-o".into(),
                "-".into(),
                // From-start needs fragment merging, which can't pipe — a new
                // player instance always starts at the live edge.
                "--no-live-from-start".into(),
            ];
            match &auth {
                AuthSource::CookiesBrowser(b) => {
                    args.push("--cookies-from-browser".into());
                    args.push(b.clone());
                }
                AuthSource::CookiesFile(p) => {
                    args.push("--cookies".into());
                    args.push(p.clone());
                }
                _ => {}
            }
            args.extend(global_args);
            push_track_args(&mut args, Tool::YtDlp, &m.audio_tracks, &m.subtitle_tracks, false);
            args.extend(extra);
            args.push(m.url.clone());
            use std::process::Stdio;
            let line = format!("{ytdlp_bin} {}", args.join(" "));
            tracing::info!(%line, "play-new-instance: spawning yt-dlp pipe");
            // stdout stays piped (it IS the video going into the player);
            // stderr is the only diagnostic this path has, so keep it.
            match std::process::Command::new(&ytdlp_bin)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(player_log(&row.channel.name, "yt-dlp"))
                .spawn()
            {
                Ok(mut child) => {
                    let pipe = child.stdout.take()?;
                    let mut cmd = std::process::Command::new(player);
                    cmd.arg("-")
                        .stdin(Stdio::from(pipe))
                        .stderr(player_log(&row.channel.name, "player"));
                    apply_live_title_and_spawn_updater(
                        &mut cmd, player, row,
                        title_template_override.unwrap_or(settings.live_title_template.trim()),
                        auto_update, store, meta,
                    );
                    if mute && player_is_mpv(player) {
                        cmd.arg("--mute");
                    }
                    let win32_tile_rect = apply_tile_or_geometry(&mut cmd, player, tile_rect);
                    spawn_logged(cmd, "media player", Some(m.id), win32_tile_rect)
                }
                Err(e) => {
                    warn!(%line, "play-new-instance: failed to spawn yt-dlp: {e}");
                    Some(format!("Failed to launch yt-dlp: {e}"))
                }
            }
        }
        Tool::Ffmpeg => {
            let mut cmd = std::process::Command::new(player);
            cmd.stderr(player_log(&row.channel.name, "player"));
            apply_live_title_and_spawn_updater(
                &mut cmd, player, row,
                title_template_override.unwrap_or(settings.live_title_template.trim()),
                auto_update, store, meta,
            );
            if mute && player_is_mpv(player) {
                cmd.arg("--mute");
            }
            cmd.arg(&m.url);
            let win32_tile_rect = apply_tile_or_geometry(&mut cmd, player, tile_rect);
            spawn_logged(cmd, "media player", Some(m.id), win32_tile_rect)
        }
    }
}

/// Play a resolved VOD URL in the configured media player — reuses
/// [`spawn_play_new_instance`]'s per-platform/per-tool dispatch (same auth,
/// cookies, quality, and tool preference the channel's own live plays use)
/// via a row clone with `monitor.url` swapped to the VOD URL, the same
/// substitute-URL pattern [`spawn_follow_raid`]/`spawn_play_collab_partner`
/// use. `is_vod = true` is passed through so nothing live-only (SABR
/// live-preview, live-title auto-update, watch-state marking) kicks in.
/// `pub(crate)` so `downloader::supervisor`'s manual-command handling — the
/// only caller, since resolving `vod_url` needs an async Helix/CDN lookup
/// this UI-thread function can't do itself — can reach it directly.
pub(crate) fn spawn_play_vod(
    source_row: &crate::models::MonitorWithChannel,
    vod_url: &str,
    vod_title: &str,
    player: &str,
    settings: &SettingsForm,
    store: &Arc<crate::store::Store>,
) -> Option<String> {
    let mut row = source_row.clone();
    row.monitor.url = vod_url.to_string();
    row.last_title = vod_title.to_string();
    row.last_game.clear();
    spawn_play_new_instance(
        &row, player, settings, store, false,
        Some("🎬  VOD: {channel} - {title_trimmed}"), None, true, None,
    )
}

/// "Follow raid": tune into a raid target at the live edge, no recording —
/// built from a synthetic, never-persisted `MonitorWithChannel` (id 0, url =
/// `twitch.tv/<to_login>`, tool/quality/extra_args copied from the raiding
/// monitor's own settings) handed to the existing [`spawn_play_new_instance`]
/// unmodified, the same way the collab-instance play actions reuse it for
/// partner rows that aren't the clicked-on row. `pub(crate)` (not just
/// `pub(super)`) so the auto-play side of Follow raid
/// (`downloader::raid_follow`) can call it directly — it's pure
/// `std::process` + `&SettingsForm`/`&Arc<Store>`, no egui/ctx dependency.
pub(crate) fn spawn_follow_raid(
    source_row: &crate::models::MonitorWithChannel,
    to_login: &str,
    to_display_name: &str,
    player: &str,
    settings: &SettingsForm,
    store: &Arc<crate::store::Store>,
    meta: Option<&LiveMetaCtx>,
) -> Option<String> {
    if to_login.is_empty() {
        return Some(format!(
            "Raid target login unknown for {to_display_name} — Twitch didn't report it"
        ));
    }
    let mut row = source_row.clone();
    row.monitor.id = 0;
    row.monitor.url = format!("https://twitch.tv/{to_login}");
    // The title fields belong to the SOURCE (raiding) channel, not the raid
    // target — carrying them over would mislabel the live-edge title with
    // the wrong channel's stale metadata. We only know the target's display
    // name at this point, so those are left blank rather than wrong; the
    // deferred fetch (see `run_untracked_title_updater`) fills them in over
    // mpv's IPC socket once Helix answers, without delaying the tune-in.
    row.channel.name = to_display_name.to_string();
    row.last_title.clear();
    row.last_game.clear();
    spawn_play_new_instance(&row, player, settings, store, false, None, meta, false, None)
}

/// Tune into a verified-but-untracked collab partner at the live edge, no
/// recording — same synthetic-row fallback as [`spawn_follow_raid`] (id 0,
/// url = `twitch.tv/<login>`, tool/quality/extra_args copied from
/// `source_row`, the instance whose collab menu this partner came from).
/// Collab partners are Twitch-only (see `models::CollabPartner`), so the
/// URL is always a plain `twitch.tv` one, no platform dispatch needed.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_play_collab_partner(
    source_row: &crate::models::MonitorWithChannel,
    partner: &crate::ui::grid::UntrackedCollabPartner,
    player: &str,
    settings: &SettingsForm,
    store: &Arc<crate::store::Store>,
    mute: bool,
    title_template_override: Option<&str>,
    meta: Option<&LiveMetaCtx>,
    tile_rect: Option<crate::display::PixelRect>,
) -> Option<String> {
    let mut row = source_row.clone();
    row.monitor.id = 0;
    row.monitor.url = format!("https://twitch.tv/{}", partner.login);
    row.channel.name = partner.name.clone();
    // Untracked, so there is no stored title/game — `meta` is what turns the
    // template's {game}/{title_trimmed} into real text, fetched from Helix
    // after launch rather than before it (`run_untracked_title_updater`).
    row.last_title.clear();
    row.last_game.clear();
    spawn_play_new_instance(
        &row, player, settings, store, mute, title_template_override, meta, false, tile_rect,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use std::path::PathBuf;

    // ----- "Play local recording (start)" target probing (incl. mid-SABR captures) -----

    /// A fresh scratch dir mimicking a channel output dir with a `.cache\` inside.
    fn probe_dir(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sa_probe_{tag}_{}_{}",
            std::process::id(),
            crate::models::now_unix()
        ));
        let cache = dir.join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        (dir, cache)
    }

    /// Write `name` in `cache` with `len` filler bytes.
    fn put(cache: &PathBuf, name: &str, len: usize) {
        std::fs::write(cache.join(name), vec![0u8; len]).unwrap();
    }

    const BIG: usize = 64 * 1024; // SPLIT_AV_MIN_BYTES

    #[test]
    fn probe_finds_single_growing_ts() {
        let (dir, cache) = probe_dir("ts");
        put(&cache, "Chan - 2026.ts", BIG);
        let out = dir.join("Chan - 2026.mkv");
        assert_eq!(
            stream_target_for_active(&out.to_string_lossy()),
            Some(StreamTarget::Growing(cache.join("Chan - 2026.ts")))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_finds_dash_companion_ts() {
        let (dir, cache) = probe_dir("dash");
        put(&cache, "Chan - 2026.dash.ts", BIG);
        // The companion recording's own output path carries the .dash infix.
        let out = dir.join("Chan - 2026.dash.mkv");
        assert_eq!(
            stream_target_for_active(&out.to_string_lossy()),
            Some(StreamTarget::Growing(cache.join("Chan - 2026.dash.ts")))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CDN parts belong to a BROADCAST, not to the take that happened to write
    /// them.
    ///
    /// Every time a doomed capture spawns, the next part is written under that
    /// new take's name while the index keeps counting for the broadcast — so
    /// one stream really does end up with `…01-05-45…cdnpart-001` and
    /// `…02-11-57…cdnpart-007` side by side (observed 2026-08-09). Matching on
    /// one take's stem would offer a fraction of the stream and present it as
    /// the recording; this matches the broadcast id, exactly as the capture
    /// side matches when it resumes.
    #[test]
    fn cdn_parts_are_collected_per_broadcast_across_takes() {
        let (dir, _cache) = probe_dir("cdn");
        // Two takes of ONE broadcast, indices continuing across them, written
        // out of order to prove the sort isn't accidental.
        for (stem, i) in [
            ("Chan - 01-05-45 - [Twitch 320826528858]", 7usize),
            ("Chan - 02-11-57 - [Twitch 320826528858]", 2),
            ("Chan - 01-05-45 - [Twitch 320826528858]", 1),
        ] {
            std::fs::write(dir.join(format!("{stem}.cdnpart-{i:03}.mkv")), b"x").unwrap();
        }
        // A different broadcast in the same folder must not leak in.
        std::fs::write(dir.join("Chan - 00-00-00 - [Twitch 999].cdnpart-001.mkv"), b"x").unwrap();
        // Neither must a take's ordinary head backfill, which is not a part.
        std::fs::write(dir.join("Chan - 01-05-45 - [Twitch 320826528858].head.mkv"), b"x").unwrap();

        let parts = cdn_parts_for_broadcast(&dir, "320826528858");
        let names: Vec<String> =
            parts.iter().map(|p| p.file_name().unwrap().to_string_lossy().into()).collect();
        assert_eq!(
            names,
            [
                "Chan - 01-05-45 - [Twitch 320826528858].cdnpart-001.mkv",
                "Chan - 02-11-57 - [Twitch 320826528858].cdnpart-002.mkv",
                "Chan - 01-05-45 - [Twitch 320826528858].cdnpart-007.mkv",
            ],
            "parts play in broadcast index order, whichever take wrote them"
        );
        // No broadcast id to key on: offer nothing rather than everything.
        assert!(cdn_parts_for_broadcast(&dir, "").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The parts are a playlist for ANY player — unlike a split SABR capture,
    /// which genuinely needs mpv. Getting this wrong would grey the button out
    /// again for everyone not using mpv.
    #[test]
    fn cdn_part_sequences_play_in_any_player_and_open_plainly() {
        let t = StreamTarget::Sequence(vec![
            std::path::PathBuf::from("a.mkv"),
            std::path::PathBuf::from("b.mkv"),
        ]);
        assert!(playable_with(&t, "vlc.exe"));
        assert!(playable_with(&t, "mpv.exe"));

        // Complete files: plain paths in order, and none of the growth-following
        // flags a live capture needs.
        for player in ["mpv.exe", "vlc.exe"] {
            let cmd = build_player_command(player, &t);
            let args: Vec<String> =
                cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
            assert_eq!(args, ["a.mkv", "b.mkv"], "{player}");
        }
    }

    #[test]
    fn probe_ignores_empty_single_file() {
        let (dir, cache) = probe_dir("empty");
        put(&cache, "Chan - 2026.ts", 0);
        let out = dir.join("Chan - 2026.mkv");
        assert_eq!(stream_target_for_active(&out.to_string_lossy()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_sabr_split_format_id_after_template_ext() {
        // Naming variant A (seen 2026-07-01): {stem}.mkv.f<id>.mp4.sq0.part
        let (dir, cache) = probe_dir("sabr_a");
        put(&cache, "Chan - 2026.mkv.f400.mp4.sq0.part", 4 * BIG); // video (bigger)
        put(&cache, "Chan - 2026.mkv.f140.mp4.sq0.part", BIG); // audio
        put(&cache, "Chan - 2026.mkv.f400.mp4.state", 52); // resume sidecars: excluded
        put(&cache, "Chan - 2026.mkv.f140.mp4.state", 51);
        put(&cache, "Chan - 2026.log", 9999); // tool log: excluded
        put(&cache, "Chan - 2026.thumbnail.jpg", 9999); // no f<id> segment: excluded
        let out = dir.join("Chan - 2026.mkv");
        assert_eq!(
            stream_target_for_active(&out.to_string_lossy()),
            Some(StreamTarget::SplitAv(vec![
                cache.join("Chan - 2026.mkv.f400.mp4.sq0.part"), // largest first
                cache.join("Chan - 2026.mkv.f140.mp4.sq0.part"),
            ]))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_sabr_split_format_id_before_container_ext() {
        // Naming variant B (seen 2026-06-30): {stem}.f<id>.mkv.sq0.part
        let (dir, cache) = probe_dir("sabr_b");
        put(&cache, "Chan - 2026.f303.mkv.sq0.part", 4 * BIG);
        put(&cache, "Chan - 2026.f140.mkv.sq0.part", BIG);
        let out = dir.join("Chan - 2026.mkv");
        assert_eq!(
            stream_target_for_active(&out.to_string_lossy()),
            Some(StreamTarget::SplitAv(vec![
                cache.join("Chan - 2026.f303.mkv.sq0.part"),
                cache.join("Chan - 2026.f140.mkv.sq0.part"),
            ]))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_sabr_prefers_bare_over_part_and_higher_sequence() {
        let (dir, cache) = probe_dir("sabr_pref");
        // f303: bare (merge phase) outranks the leftover .part.
        put(&cache, "Chan - 2026.f303.mkv", 4 * BIG);
        put(&cache, "Chan - 2026.f303.mkv.sq0.part", 4 * BIG);
        // f140: sq1 (post-resume sequence) outranks sq0.
        put(&cache, "Chan - 2026.f140.mkv.sq0.part", BIG);
        put(&cache, "Chan - 2026.f140.mkv.sq1.part", BIG);
        // In-flight ffmpeg merge output must never be picked.
        put(&cache, "Chan - 2026.mkv.temp.mp4", 8 * BIG);
        let out = dir.join("Chan - 2026.mkv");
        assert_eq!(
            stream_target_for_active(&out.to_string_lossy()),
            Some(StreamTarget::SplitAv(vec![
                cache.join("Chan - 2026.f303.mkv"),
                cache.join("Chan - 2026.f140.mkv.sq1.part"),
            ]))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_sabr_single_format_is_growing() {
        // Audio-only (or video-only) SABR capture: one growing file → Growing.
        let (dir, cache) = probe_dir("sabr_one");
        put(&cache, "Chan - 2026.f140.mkv.sq0.part", BIG);
        let out = dir.join("Chan - 2026.mkv");
        assert_eq!(
            stream_target_for_active(&out.to_string_lossy()),
            Some(StreamTarget::Growing(cache.join("Chan - 2026.f140.mkv.sq0.part")))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_sabr_ignores_tiny_files() {
        // First seconds of a capture: files below the init-segment floor.
        let (dir, cache) = probe_dir("sabr_tiny");
        put(&cache, "Chan - 2026.f303.mkv.sq0.part", 1024);
        put(&cache, "Chan - 2026.f140.mkv.sq0.part", 512);
        let out = dir.join("Chan - 2026.mkv");
        assert_eq!(stream_target_for_active(&out.to_string_lossy()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_does_not_leak_across_stems() {
        // Another recording's SABR files in the same .cache must not match.
        let (dir, cache) = probe_dir("stems");
        put(&cache, "Other - 2025.f303.mkv.sq0.part", 4 * BIG);
        put(&cache, "Other - 2025.f140.mkv.sq0.part", BIG);
        let out = dir.join("Chan - 2026.mkv");
        assert_eq!(stream_target_for_active(&out.to_string_lossy()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_does_not_leak_into_prefix_stems() {
        // A sibling recording whose stem EXTENDS this stem after a dot
        // ("Chan - Movie Night" vs "Chan - Movie Night. Part 2") must not
        // leak into the shorter stem's probe: the segments before the f<id>
        // token would be title text, not container decoration.
        let (dir, cache) = probe_dir("prefix");
        put(&cache, "Chan - Movie Night. Part 2.f303.mkv.sq0.part", 4 * BIG);
        put(&cache, "Chan - Movie Night. Part 2.f140.mkv.sq0.part", BIG);
        let out = dir.join("Chan - Movie Night.mkv");
        assert_eq!(stream_target_for_active(&out.to_string_lossy()), None);
        // With the shorter stem's own files present, only they are picked.
        put(&cache, "Chan - Movie Night.f303.mkv.sq0.part", 4 * BIG);
        put(&cache, "Chan - Movie Night.f140.mkv.sq0.part", BIG);
        assert_eq!(
            stream_target_for_active(&out.to_string_lossy()),
            Some(StreamTarget::SplitAv(vec![
                cache.join("Chan - Movie Night.f303.mkv.sq0.part"),
                cache.join("Chan - Movie Night.f140.mkv.sq0.part"),
            ]))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----- FsProbes (async stale-while-revalidate cache) -----

    /// Poll `drain + is_file` until it reports `want` or a deadline passes —
    /// the worker answers on its own thread, so results land asynchronously.
    fn probes_wait_file(fp: &mut FsProbes, p: &std::path::Path, want: bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            fp.drain_results();
            if fp.is_file(p) == want {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn fs_probes_serve_placeholder_then_worker_result_then_stale_value() {
        let (dir, cache) = probe_dir("fsprobes");
        let file = cache.join("real.ts");
        std::fs::write(&file, b"x").unwrap();
        let mut fp = FsProbes::new(egui::Context::default());
        // First sight: pessimistic placeholder — the calling (UI) thread
        // must never touch the disk itself.
        assert!(!fp.is_file(&file));
        // The worker's answer lands shortly after.
        assert!(probes_wait_file(&mut fp, &file, true), "worker result never arrived");
        // Stale-while-revalidate: after deleting the file the cached true is
        // still served immediately (no blocking re-probe)…
        std::fs::remove_file(&file).unwrap();
        assert!(fp.is_file(&file));
        // …and flips to false once the TTL expires and the refresh lands.
        assert!(probes_wait_file(&mut fp, &file, false), "refresh never arrived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_probes_evict_only_entries_not_accessed_recently() {
        let (dir, cache) = probe_dir("fsevict");
        let file = cache.join("real.ts");
        std::fs::write(&file, b"x").unwrap();
        let mut fp = FsProbes::new(egui::Context::default());
        assert!(probes_wait_file(&mut fp, &file, true));
        // Recently accessed → survives the slow-tick eviction.
        fp.evict_unused();
        assert!(fp.files.contains_key(&file));
        // Backdate the access stamp (skipped when machine uptime is shorter
        // than the eviction window — Instant can't represent that past).
        if let Some(old) = std::time::Instant::now()
            .checked_sub(FS_PROBE_EVICT + std::time::Duration::from_secs(1))
        {
            fp.files.get_mut(&file).unwrap().used = old;
            fp.evict_unused();
            assert!(!fp.files.contains_key(&file));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_av_player_command_uses_appending_and_audio_file() {
        let parts = vec![
            PathBuf::from(r"A:\streams\Nitya Ch. 【Phase】\.cache\a b.f303.mkv.sq0.part"),
            PathBuf::from(r"A:\streams\Nitya Ch. 【Phase】\.cache\a b.f140.mkv.sq0.part"),
        ];
        let cmd = super::build_player_command(
            r"C:\Progs\mpv\mpv.exe",
            &StreamTarget::SplitAv(parts.clone()),
        );
        assert_eq!(cmd.get_program().to_string_lossy(), r"C:\Progs\mpv\mpv.exe");
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        // Live-view flags, then the largest (video) file as the appending://
        // main file, then the audio as an external appending:// track.
        // Backslashes are converted to forward slashes inside the URLs.
        assert!(args.contains(&"--keep-open=yes".to_string()));
        let fwd = |p: &PathBuf| p.to_string_lossy().replace('\\', "/");
        assert_eq!(args.last().unwrap(), &format!("--audio-file=appending://{}", fwd(&parts[1])));
        assert!(args.contains(&format!("appending://{}", fwd(&parts[0]))));

        // Growing single file under mpv also uses appending://; other players
        // and finished files get the plain path.
        let g = StreamTarget::Growing(PathBuf::from(r"A:\x\.cache\y.ts"));
        let cmd = super::build_player_command(r"C:\Progs\mpv\mpv.exe", &g);
        assert!(cmd.get_args().any(|a| a.to_string_lossy() == "appending://A:/x/.cache/y.ts"));
        let cmd = super::build_player_command(r"C:\VLC\vlc.exe", &g);
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec![r"A:\x\.cache\y.ts".to_string()]);
    }

    #[test]
    fn player_kind_sniffing() {
        assert!(player_is_mpv(r"C:\Progs\mpv\mpv.exe"));
        assert!(player_is_mpv(r"C:\Apps\mpv.net\mpvnet.exe"));
        assert!(player_is_mpv("mpv"));
        assert!(!player_is_mpv(r"C:\Program Files\VideoLAN\VLC\vlc.exe"));
        assert!(!player_is_mpv(""));

        let split = StreamTarget::SplitAv(vec![PathBuf::from("v"), PathBuf::from("a")]);
        assert!(playable_with(&split, r"C:\Progs\mpv\mpv.exe"));
        assert!(!playable_with(&split, r"C:\VLC\vlc.exe"));
        let single = StreamTarget::Growing(PathBuf::from("x.ts"));
        assert!(playable_with(&single, r"C:\VLC\vlc.exe"));
        assert!(playable_with(&StreamTarget::Finished(PathBuf::from("x.mkv")), ""));
    }

    // ----- Live-edge player title -----

    #[test]
    fn render_live_title_substitutes_every_token() {
        assert_eq!(
            render_live_title("{channel}: 【{game}】- {title_trimmed}", "Layna", "Just Chatting", "12 Hours.", "00:00:00"),
            "Layna: 【Just Chatting】- 12 Hours."
        );
        // A repeated token gets every occurrence replaced.
        assert_eq!(render_live_title("{channel}/{channel}", "X", "", "", ""), "X/X");
        // Tokens absent from the template are simply never looked at.
        assert_eq!(render_live_title("static text", "X", "Y", "Z", "W"), "static text");
        // {pos} is a real token, distinct from the other three.
        assert_eq!(render_live_title("{pos}", "", "", "", "00:12:34"), "00:12:34");
    }

    #[test]
    fn mpv_live_title_value_embeds_the_live_time_pos_property_and_trims_commands() {
        let v = mpv_live_title_value(
            "{channel}: 【{game}】- {title_trimmed} [{pos}]",
            "Layna",
            "12 Hours. !gg !tts",
            "Just Chatting",
        );
        // `{pos}` becomes mpv's own live property-expansion token, not a
        // resolved value — this is what makes the position keep ticking
        // without any polling on our side.
        assert_eq!(v, "Layna: 【Just Chatting】- 12 Hours. [${time-pos}]");
    }

    #[test]
    fn static_live_title_fixes_pos_at_zero() {
        let v = static_live_title("{channel} [{pos}]", "Layna", "Hello !gg", "Just Chatting");
        assert_eq!(v, "Layna [00:00:00]");
    }

    #[test]
    fn open_players_presence_open_then_grace_then_stale() {
        // Distinct ids so this test can't collide with any other user of the
        // process-wide registry.
        let (a, b) = (9_000_001, 9_000_002);
        assert!(!monitor_watched_recently(a, 600), "never opened");
        note_player_opened(a);
        assert!(monitor_watched_recently(a, 0), "open now counts at any grace");
        note_player_closed(a);
        assert!(monitor_watched_recently(a, 600), "just closed, inside grace");
        // Aged-out close: rewrite the entry as if it closed an hour ago.
        OPEN_PLAYERS.lock().unwrap().insert(a, (0, crate::models::now_unix() - 3600));
        assert!(!monitor_watched_recently(a, 600), "closed too long ago");
        // Synthetic rows (id 0) are never tracked.
        note_player_opened(0);
        assert!(!monitor_watched_recently(0, 600));
        // Two players open, one closes — still watching (count bookkeeping,
        // not the grace window: the close stamp is aged out below).
        note_player_opened(b);
        note_player_opened(b);
        note_player_closed(b);
        OPEN_PLAYERS.lock().unwrap().get_mut(&b).unwrap().1 = crate::models::now_unix() - 3600;
        assert!(monitor_watched_recently(b, 0), "one of two players still open");
    }

    #[test]
    fn deferred_title_fetch_only_for_untracked_twitch_rows_in_mpv() {
        use crate::downloader::test_util::row;
        use crate::models::{Container, Platform, Tool};

        let tracked = row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        assert_ne!(tracked.monitor.id, 0);
        // A tracked row already has stored title/game and an updater polling
        // the DB for free — fetching would spend Helix quota on a duplicate.
        assert!(!row_wants_deferred_title(&tracked, "mpv.exe", "{channel} {game}"));

        // The synthetic shape both `spawn_play_collab_partner` and
        // `spawn_follow_raid` build: id 0, no stored title/game.
        let mut untracked = tracked.clone();
        untracked.monitor.id = 0;
        assert!(row_wants_deferred_title(&untracked, "mpv.exe", "{channel} {game}"));

        // No template = nothing to render into; no mpv = no IPC socket to
        // push the result over.
        assert!(!row_wants_deferred_title(&untracked, "mpv.exe", ""));
        assert!(!row_wants_deferred_title(&untracked, "vlc.exe", "{channel} {game}"));

        // The fetch is Helix `GET /streams`. A synthetic row inherits its
        // tool/url from whichever instance launched it, so a non-Twitch one
        // is reachable and must be declined rather than mis-queried.
        let mut yt = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        yt.monitor.id = 0;
        assert!(!row_wants_deferred_title(&yt, "mpv.exe", "{channel} {game}"));
    }

    /// The reply drain must empty everything queued (including more than its
    /// scratch buffer holds) and return immediately on an empty pipe instead
    /// of blocking — exercised against a real Win32 pipe, since PeekNamedPipe
    /// is the whole mechanism.
    #[cfg(windows)]
    #[test]
    fn drain_mpv_replies_empties_the_pipe_without_blocking() {
        use std::io::Write;
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Pipes::CreatePipe;

        let (mut read, mut write) = unsafe {
            let mut r = HANDLE::default();
            let mut w = HANDLE::default();
            CreatePipe(&mut r, &mut w, None, 0).unwrap();
            (
                std::fs::File::from_raw_handle(r.0 as _),
                std::fs::File::from_raw_handle(w.0 as _),
            )
        };
        assert_eq!(crate::platform::pipe_bytes_available(&read), 0);
        // Simulate a backlog of mpv replies bigger than the drain's 512-byte
        // scratch, so the multi-pass path is covered too.
        write.write_all(&vec![b'x'; 700]).unwrap();
        write.write_all(b"\n").unwrap();
        assert!(crate::platform::pipe_bytes_available(&read) > 0);
        drain_mpv_replies(&mut read);
        assert_eq!(crate::platform::pipe_bytes_available(&read), 0, "backlog fully drained");
        // Empty pipe: returns without ever issuing a (blocking) read.
        drain_mpv_replies(&mut read);
    }

    /// A non-pipe handle must read as "nothing available" — that failed-peek
    /// contract is what keeps [`drain_mpv_replies`] from ever reaching a
    /// blocking read on a handle it doesn't understand.
    #[cfg(windows)]
    #[test]
    fn pipe_bytes_available_is_zero_for_non_pipe_handles() {
        let dir = std::env::temp_dir().join(format!(
            "sa_peek_{}_{}",
            std::process::id(),
            crate::models::now_unix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-a-pipe.txt");
        std::fs::write(&path, b"plenty of bytes in here").unwrap();
        let f = std::fs::File::open(&path).unwrap();
        assert_eq!(crate::platform::pipe_bytes_available(&f), 0);
        drop(f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both readers of the auto-update setting must agree, including on the
    /// unset default (ON) — they used to decode it differently (`== "1"` vs
    /// `!= "0"`), leaving the feature off in the UI but on for auto-played
    /// raid windows of the same fresh install.
    #[test]
    fn live_title_auto_update_defaults_on_and_readers_agree() {
        let store = crate::store::Store::open_in_memory().unwrap();
        for (value, expect) in [(None, true), (Some("0"), false), (Some("1"), true)] {
            if let Some(v) = value {
                store.set_setting(crate::ui::K_LIVE_TITLE_AUTO_UPDATE, v).unwrap();
            }
            assert_eq!(crate::ui::live_title_auto_update_setting(&store), expect, "{value:?}");
            assert_eq!(
                crate::ui::SettingsForm::for_auto_play(&store).live_title_auto_update,
                expect,
                "for_auto_play must match the settings form for {value:?}"
            );
        }
    }

    #[test]
    fn streamlink_player_args_quote_the_pipe_and_use_the_current_input_token() {
        let pipe = r"\\.\pipe\streamarchiver-mpv-1234-99";
        let v = streamlink_player_args(Some(pipe), false, None).unwrap();
        // The pipe path MUST be single-quoted: Streamlink shlex-splits this
        // value in POSIX mode even on Windows, so an unquoted `\\.\pipe\x`
        // arrives as `.pipex` with every backslash eaten as an escape.
        assert_eq!(v, format!("--input-ipc-server='{pipe}' {{playerinput}}"));
        // `{playerinput}`, never the removed `{filename}` — an unknown
        // variable survives as literal text and mpv treats it as a file to
        // play (Streamlink then appends the real input after it anyway).
        assert!(!v.contains("{filename}"));

        // Mute composes into the SAME value: a second --player-args would
        // just override the first.
        let both = streamlink_player_args(Some(pipe), true, None).unwrap();
        assert_eq!(both, format!("--input-ipc-server='{pipe}' --mute {{playerinput}}"));
        assert_eq!(both.matches("{playerinput}").count(), 1);

        // Mute alone still works, and nothing at all means no flag.
        assert_eq!(streamlink_player_args(None, true, None).unwrap(), "--mute {playerinput}");
        assert_eq!(streamlink_player_args(None, false, None), None);

        // Geometry composes alongside the others too.
        let geo = streamlink_player_args(Some(pipe), true, Some("--geometry=800x600+0+0")).unwrap();
        assert_eq!(
            geo,
            format!("--input-ipc-server='{pipe}' --mute --geometry=800x600+0+0 {{playerinput}}")
        );
    }

    #[test]
    fn mpv_ipc_pipe_paths_are_unique_per_instance() {
        // Two players opened in the same second must not share a socket, or a
        // title push for one lands on the other (collab "play every angle"
        // spawns several at once).
        let paths: std::collections::HashSet<String> =
            (0..8).map(|_| new_mpv_ipc_pipe_path()).collect();
        assert_eq!(paths.len(), 8, "pipe paths collided: {paths:?}");
        for p in &paths {
            assert!(p.starts_with(r"\\.\pipe\streamarchiver-mpv-"));
            assert!(!p.contains(' '), "a space here would break --player-args splitting");
        }
    }
}
