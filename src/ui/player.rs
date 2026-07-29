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
}

/// A finished probe shipped back from the worker.
pub(super) enum ProbeResult {
    File(std::path::PathBuf, bool),
    Dir(std::path::PathBuf, bool),
    Len(std::path::PathBuf, u64),
    LiveLen(std::path::PathBuf, u64),
    Target(String, Option<StreamTarget>),
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
        StreamTarget::Finished(_) | StreamTarget::Growing(_) => true,
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
    }
    cmd
}

/// Log and spawn a player/downloader command built for "play new instance",
/// returning a status-bar error message on spawn failure.
pub(super) fn spawn_logged(mut cmd: std::process::Command, what: &str) -> Option<String> {
    let line = format!(
        "{} {}",
        cmd.get_program().to_string_lossy(),
        cmd.get_args().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ")
    );
    tracing::info!(%line, "play-new-instance: spawning {what}");
    match cmd.spawn() {
        Ok(_) => None,
        Err(e) => {
            warn!(%line, "play-new-instance: failed to spawn {what}: {e}");
            Some(format!("Failed to launch {what}: {e}"))
        }
    }
}

// ----- Live-edge player title (configurable, optionally auto-updating) -----

/// Substitute the tokens available in the "Live-edge player title" setting:
/// `{channel}`, `{game}`, `{title_trimmed}`, `{pos}` (current playback
/// position). `{pos}` only ever ticks live for the launch paths this app
/// spawns mpv directly for (YouTube/Kick/ffmpeg-source, via
/// [`mpv_live_title_arg`]) — mpv's own `--title` flag supports property
/// expansion and keeps refreshing the window title on its own, unlike
/// `--force-media-title` (verified against mpv issue trackers: the latter
/// does not expand `${...}` properties, `--title` does). Streamlink (Twitch)
/// resolves its own `--title` template once at launch and forwards it to mpv
/// as `--force-media-title`, so the title it sets can never tick or update —
/// which is why that path asks Streamlink for an mpv IPC socket (see
/// [`streamlink_player_args`]) and replaces the whole title over IPC the
/// moment mpv is up, with a real `${time-pos}` in it.
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

/// The Streamlink `--title` VALUE (not an mpv flag — Streamlink translates
/// it internally) for a live-edge tune-in. Resolved once from this app's own
/// known metadata (not Streamlink's own scrape) so the same template means
/// the same thing on every launch path; `{pos}` is a fixed `00:00:00` because
/// Streamlink's title can't tick. It only shows for the moment before mpv's
/// IPC socket comes up, after which [`run_live_title_updater`]'s first push
/// replaces it with the ticking form — and with a blank template, or
/// auto-update off, it stays the whole time.
fn streamlink_live_title(template: &str, channel: &str, last_title: &str, last_game: &str) -> String {
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

/// Send one `set_property title <value>` command over an already-open mpv
/// JSON-IPC pipe (newline-delimited JSON, mpv's documented IPC protocol).
fn send_mpv_title(pipe: &mut std::fs::File, title: &str) -> std::io::Result<()> {
    use std::io::Write;
    let line = serde_json::json!({"command": ["set_property", "title", title]}).to_string();
    pipe.write_all(line.as_bytes())?;
    pipe.write_all(b"\n")
}

/// Background loop started by [`apply_live_title_and_spawn_updater`] (and by
/// the Streamlink branch of [`spawn_play_new_instance`]): connect to
/// `pipe_path`, push the current title once the pipe is up, then poll
/// `monitor_id`'s title/game every [`LIVE_TITLE_POLL`] and push a freshly
/// rendered title whenever either changed since the last push. Exits silently
/// once the pipe write fails (mpv closed its IPC server, meaning the player
/// quit) or `connect_timeout` elapses (mpv never came up, or doesn't support
/// `--input-ipc-server`).
///
/// The push-on-connect isn't redundant: on the Streamlink path our `--title`
/// never reaches mpv at all (Streamlink resolves its own `--title` once and
/// hands mpv `--force-media-title`, verified against Streamlink 8's
/// `PlayerArgsMPV::get_title`), so this first push is what puts the configured
/// template on the window — and starts `{pos}` ticking. On the paths that do
/// pass `--title=` themselves it re-sends the value already showing, which
/// costs one pipe write.
fn run_live_title_updater(
    pipe_path: String,
    store: Arc<crate::store::Store>,
    monitor_id: i64,
    template: String,
    mut last_title: String,
    mut last_game: String,
    connect_timeout: std::time::Duration,
) {
    let deadline = std::time::Instant::now() + connect_timeout;
    let mut pipe = loop {
        match crate::iomon::fs::open_with_sync(crate::iomon::Cat::Preview, &pipe_path, |o| {
            o.write(true);
        }) {
            Ok(f) => break f,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => {
                warn!("live-title: mpv IPC pipe never came up, auto-update disabled: {e}");
                return;
            }
        }
    };
    tracing::debug!(monitor_id, "live-title: mpv IPC connected, pushing title");
    let mut force = true;
    loop {
        if let Ok(Some(row)) = store.get_monitor_with_channel(monitor_id)
            && (force || row.last_title != last_title || row.last_game != last_game)
        {
            force = false;
            last_title = row.last_title.clone();
            last_game = row.last_game.clone();
            let value = mpv_live_title_value(&template, &row.channel.name, &last_title, &last_game);
            if send_mpv_title(&mut pipe, &value).is_err() {
                return; // player closed
            }
        }
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
    monitor_id: i64,
    channel: &str,
    last_title: &str,
    last_game: &str,
    template: &str,
    auto_update: bool,
    store: &Arc<crate::store::Store>,
) {
    if !player_is_mpv(player) || template.is_empty() {
        return;
    }
    cmd.arg(format!("--title={}", mpv_live_title_value(template, channel, last_title, last_game)));
    if !auto_update || monitor_id == 0 {
        return; // 0 = a synthetic follow-raid row with no real monitor to poll
    }
    let pipe_path = new_mpv_ipc_pipe_path();
    cmd.arg(format!("--input-ipc-server={pipe_path}"));
    spawn_live_title_updater(
        pipe_path, store, monitor_id, template, last_title, last_game,
        LIVE_TITLE_IPC_CONNECT_TIMEOUT,
    );
}

/// Start [`run_live_title_updater`] on its own thread. Shared by the paths
/// that spawn mpv directly (via [`apply_live_title_and_spawn_updater`]) and
/// the Streamlink path, which can only reach mpv through `--player-args`.
fn spawn_live_title_updater(
    pipe_path: String,
    store: &Arc<crate::store::Store>,
    monitor_id: i64,
    template: &str,
    last_title: &str,
    last_game: &str,
    connect_timeout: std::time::Duration,
) {
    let store = Arc::clone(store);
    let (template, last_title, last_game) =
        (template.to_string(), last_title.to_string(), last_game.to_string());
    std::thread::spawn(move || {
        run_live_title_updater(
            pipe_path, store, monitor_id, template, last_title, last_game, connect_timeout,
        );
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
fn streamlink_player_args(ipc_pipe: Option<&str>, mute: bool) -> Option<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(pipe) = ipc_pipe {
        args.push(format!("--input-ipc-server='{pipe}'"));
    }
    if mute {
        args.push("--mute".into());
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
) -> Option<String> {
    use crate::downloader::{load_ytdlp_bins, resolve_auth, sabr_preview_args, split_args, youtube_live_url, AuthSource};
    use crate::models::Platform;

    let m = &row.monitor;
    sweep_stale_previews();

    let bins = load_ytdlp_bins(store);
    let use_sabr = m.platform() == Platform::YouTube && bins.sabr.usable();
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
    let auth = resolve_auth(row, &settings.download_auth_method, &cookies);
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
    let monitor_id = m.id;
    let last_title = row.last_title.clone();
    let last_game = row.last_game.clone();
    let title_template = title_template_override.unwrap_or(settings.live_title_template.trim()).to_string();
    let title_auto_update = settings.live_title_auto_update;
    let title_store = Arc::clone(store);
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
                            &mut cmd, &player, monitor_id, &channel, &last_title, &last_game,
                            &title_template, title_auto_update, &title_store,
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
                    &mut cmd, &player, monitor_id, &channel, &last_title, &last_game,
                    &title_template, title_auto_update, &title_store,
                );
                match cmd.spawn() {
                    Ok(p) => Some((p, None)),
                    Err(e) => {
                        warn!(%channel, "live-preview: failed to launch player: {e}");
                        None
                    }
                }
            })();
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
                tracing::info!(%channel, "live-preview: player closed, stopping preview download");
            }
            Some((mut p, None)) => {
                let _ = p.wait();
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
pub(super) fn spawn_play_new_instance(
    row: &crate::models::MonitorWithChannel,
    player: &str,
    settings: &SettingsForm,
    store: &Arc<crate::store::Store>,
    mute: bool,
    title_template_override: Option<&str>,
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
    // there's nothing to mark (correctly a no-op).
    if let Ok(Some(rec)) = store.current_recording_for_monitor(m.id) {
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
    let auth = resolve_auth(row, &settings.download_auth_method, &cookies);
    let extra: Vec<String> = split_args(&m.extra_args);

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
                args.push(streamlink_live_title(
                    title_template, &row.channel.name, &row.last_title, &row.last_game,
                ));
            }
            args.push("--player".into());
            args.push(player.to_string());
            // Streamlink owns the player process here, so mpv's IPC socket —
            // the only way to update the title after launch, since Streamlink
            // resolves its own --title once and never revisits it — has to be
            // requested through --player-args. Skipped for a synthetic row
            // (id 0: follow-raid/collab partner), which has no monitor to poll.
            let ipc_pipe = (!title_template.is_empty()
                && settings.live_title_auto_update
                && player_is_mpv(player)
                && m.id != 0)
                .then(new_mpv_ipc_pipe_path);
            // A separate --player-args flag, NOT embedded in --player's own
            // value: Streamlink re-splits `--player` as its own command line,
            // and on Windows a value handed to it as "<path> --mute" can come
            // back mis-split into a single executable name — reported live as
            // `error: Failed to start player: C:\...\mpv.exe --mute (Player
            // executable not found)`. --player-args isn't re-split against the
            // player path, so it can't suffer the same failure.
            if let Some(player_args) =
                streamlink_player_args(ipc_pipe.as_deref(), mute && player_is_mpv(player))
            {
                args.push("--player-args".into());
                args.push(player_args);
            }
            args.push(m.url.clone());
            args.push(resolved_quality(&m.quality));
            let mut cmd = std::process::Command::new("streamlink");
            cmd.args(&args);
            let status = spawn_logged(cmd, "streamlink");
            // Only if Streamlink actually started — otherwise the updater would
            // sit on a pipe that can never appear and log a spurious warning.
            if let Some(pipe) = ipc_pipe.filter(|_| status.is_none()) {
                spawn_live_title_updater(
                    pipe, store, m.id, title_template, &row.last_title, &row.last_game,
                    LIVE_TITLE_IPC_CONNECT_TIMEOUT_STREAMLINK,
                );
            }
            status
        }
        Tool::YtDlp if m.platform() == Platform::YouTube => {
            spawn_live_preview(row, player, settings, store, mute, title_template_override)
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
            match std::process::Command::new(&ytdlp_bin)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let pipe = child.stdout.take()?;
                    let mut cmd = std::process::Command::new(player);
                    cmd.arg("-").stdin(Stdio::from(pipe));
                    apply_live_title_and_spawn_updater(
                        &mut cmd, player, m.id, &row.channel.name, &row.last_title, &row.last_game,
                        title_template_override.unwrap_or(settings.live_title_template.trim()),
                        settings.live_title_auto_update, store,
                    );
                    if mute && player_is_mpv(player) {
                        cmd.arg("--mute");
                    }
                    spawn_logged(cmd, "media player")
                }
                Err(e) => {
                    warn!(%line, "play-new-instance: failed to spawn yt-dlp: {e}");
                    Some(format!("Failed to launch yt-dlp: {e}"))
                }
            }
        }
        Tool::Ffmpeg => {
            let mut cmd = std::process::Command::new(player);
            apply_live_title_and_spawn_updater(
                &mut cmd, player, m.id, &row.channel.name, &row.last_title, &row.last_game,
                title_template_override.unwrap_or(settings.live_title_template.trim()),
                settings.live_title_auto_update, store,
            );
            if mute && player_is_mpv(player) {
                cmd.arg("--mute");
            }
            cmd.arg(&m.url);
            spawn_logged(cmd, "media player")
        }
    }
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
    // name, not its current title/game, so those are left blank rather than
    // wrong.
    row.channel.name = to_display_name.to_string();
    row.last_title.clear();
    row.last_game.clear();
    spawn_play_new_instance(&row, player, settings, store, false, None)
}

/// Tune into a verified-but-untracked collab partner at the live edge, no
/// recording — same synthetic-row fallback as [`spawn_follow_raid`] (id 0,
/// url = `twitch.tv/<login>`, tool/quality/extra_args copied from
/// `source_row`, the instance whose collab menu this partner came from).
/// Collab partners are Twitch-only (see `models::CollabPartner`), so the
/// URL is always a plain `twitch.tv` one, no platform dispatch needed.
pub(super) fn spawn_play_collab_partner(
    source_row: &crate::models::MonitorWithChannel,
    partner: &crate::ui::grid::UntrackedCollabPartner,
    player: &str,
    settings: &SettingsForm,
    store: &Arc<crate::store::Store>,
    mute: bool,
    title_template_override: Option<&str>,
) -> Option<String> {
    let mut row = source_row.clone();
    row.monitor.id = 0;
    row.monitor.url = format!("https://twitch.tv/{}", partner.login);
    row.channel.name = partner.name.clone();
    row.last_title.clear();
    row.last_game.clear();
    spawn_play_new_instance(&row, player, settings, store, mute, title_template_override)
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
    fn streamlink_live_title_fixes_pos_at_zero() {
        let v = streamlink_live_title("{channel} [{pos}]", "Layna", "Hello !gg", "Just Chatting");
        assert_eq!(v, "Layna [00:00:00]");
    }

    #[test]
    fn streamlink_player_args_quote_the_pipe_and_use_the_current_input_token() {
        let pipe = r"\\.\pipe\streamarchiver-mpv-1234-99";
        let v = streamlink_player_args(Some(pipe), false).unwrap();
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
        let both = streamlink_player_args(Some(pipe), true).unwrap();
        assert_eq!(both, format!("--input-ipc-server='{pipe}' --mute {{playerinput}}"));
        assert_eq!(both.matches("{playerinput}").count(), 1);

        // Mute alone still works, and nothing at all means no flag.
        assert_eq!(streamlink_player_args(None, true).unwrap(), "--mute {playerinput}");
        assert_eq!(streamlink_player_args(None, false), None);
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
