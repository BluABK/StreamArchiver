//! ffmpeg/ffprobe passes: remux TS→MKV, concat, thumbnail/subtitle
//! embedding, media probes, stall sampling.

use super::*;

/// Resolve a video/stream's real title, channel/uploader, and id via yt-dlp (no
/// download). Works for YouTube, Twitch VODs, Kick, and many sites; returns
/// `(title, channel, id)` with empty strings on any failure (caller falls back).
/// Each is truncated to keep filenames sane.
pub(super) async fn resolve_meta(video: &Video, auth: &AuthSource) -> (String, String, String) {
    // Three `--print` templates -> three output lines, in order.
    let mut args: Vec<String> = vec![
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--skip-download".into(),
        "--print".into(),
        "%(title)s".into(),
        "--print".into(),
        "%(channel,uploader)s".into(),
        "--print".into(),
        "%(id)s".into(),
    ];
    match auth {
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
    args.push(video.url.clone());

    let mut cmd = Command::new("yt-dlp");
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let out = match cmd.output().await {
        Ok(o) if o.status.success() => o,
        _ => return (String::new(), String::new(), String::new()),
    };
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut lines = raw.lines();
    let clean = |s: &str| -> String {
        let s = s.trim();
        if s.is_empty() || s == "NA" {
            String::new()
        } else {
            s.chars().take(120).collect()
        }
    };
    let title = clean(lines.next().unwrap_or(""));
    let channel = clean(lines.next().unwrap_or(""));
    let id = clean(lines.next().unwrap_or(""));
    (title, channel, id)
}
/// Parse a Twitch rendition name (`"720p60"`, `"1080p"`, `"480p30"`) into
/// `(height, fps)`; fps defaults to 30 when unstated. Aliases (`best`,
/// `worst`, `audio_only`) don't parse and return `None`.
pub(super) fn parse_rendition(name: &str) -> Option<(i64, i64)> {
    let (h, rest) = name.split_once('p')?;
    let h: i64 = h.parse().ok()?;
    let fps: i64 = if rest.is_empty() { 30 } else { rest.parse().ok()? };
    Some((h, fps))
}

/// The best rendition Twitch currently lists for `url`, via
/// `streamlink --json` (metadata only, nothing downloaded):
/// `(height, fps, name)`. `None` on any failure — callers treat that as
/// "nothing better known".
pub(super) async fn best_available_rendition(url: &str) -> Option<(i64, i64, String)> {
    let mut cmd = Command::new("streamlink");
    cmd.arg("--json")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .ok()?
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let streams = v.get("streams")?.as_object()?;
    let mut best: Option<(i64, i64, String)> = None;
    for name in streams.keys() {
        if let Some((h, fps)) = parse_rendition(name)
            && best.as_ref().map(|&(bh, bf, _)| (h, fps) > (bh, bf)).unwrap_or(true)
        {
            best = Some((h, fps, name.clone()));
        }
    }
    best
}

/// Probe available formats/qualities for a URL with the given tool, returning
/// combined stdout+stderr for the Videos-tab "List formats" window. yt-dlp gets
/// `--list-formats`, streamlink lists its stream qualities, ffmpeg uses ffprobe.
pub async fn probe_formats(tool: Tool, url: &str, auth: &AuthSource) -> Result<String, String> {
    let (program, mut args): (&str, Vec<String>) = match tool {
        Tool::YtDlp => (
            "yt-dlp",
            vec!["--list-formats".into(), "--no-playlist".into()],
        ),
        Tool::Streamlink => ("streamlink", Vec::new()),
        Tool::Ffmpeg => ("ffprobe", vec!["-hide_banner".into()]),
    };
    if let Tool::YtDlp = tool {
        match auth {
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
    }
    args.push(url.to_string());

    let mut cmd = Command::new(program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let out = match tokio::time::timeout(Duration::from_secs(45), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("failed to run {program}: {e}")),
        Err(_) => return Err(format!("{program} timed out")),
    };
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !s.trim().is_empty() {
            s.push('\n');
        }
        s.push_str(&err);
    }
    let s = s.trim().to_string();
    if s.is_empty() {
        Ok(format!("(no output; exit {:?})", out.status.code()))
    } else {
        Ok(s)
    }
}

/// Losslessly concatenate two same-encode MKVs (`head` then `tail`) into
/// `dst` — the exact 2-entry, no-trim case of [`concat_mkvs_n`].
pub(super) async fn concat_mkvs(
    list_dir: &Path,
    head: &Path,
    tail: &Path,
    dst: &Path,
) -> anyhow::Result<()> {
    concat_mkvs_n(
        list_dir,
        &[ConcatEntry::whole(head), ConcatEntry::whole(tail)],
        dst,
    )
    .await
}

/// One ffconcat entry: a source file, optionally trimmed to `[inpoint,
/// outpoint)` (broadcast-relative seconds within THAT file, `None` = play
/// from the start / to the end). The same path may appear more than once in
/// one list with different in/out points — ffmpeg's concat demuxer supports
/// this natively, so gap-splice never needs to physically pre-cut the base
/// file into pieces; one entry per untouched span plus one per patch.
#[derive(Clone, Copy)]
pub(super) struct ConcatEntry<'a> {
    pub(super) path: &'a Path,
    pub(super) inpoint: Option<f64>,
    pub(super) outpoint: Option<f64>,
}

impl<'a> ConcatEntry<'a> {
    pub(super) fn whole(path: &'a Path) -> Self {
        Self { path, inpoint: None, outpoint: None }
    }
    pub(super) fn trimmed(path: &'a Path, inpoint: Option<f64>, outpoint: Option<f64>) -> Self {
        Self { path, inpoint, outpoint }
    }
}

/// Losslessly concatenate an ordered list of (possibly-trimmed, possibly
/// repeated) source files into `dst` via ffmpeg's concat demuxer (`-c
/// copy`). `-fflags +genpts` regenerates timestamps across every seam and
/// `-avoid_negative_ts make_zero` keeps the joined timeline monotonic. The
/// list file lives in `list_dir` (a `.cache\` dir) and is removed
/// afterwards. Caller is responsible for verifying codec compatibility
/// first — mismatched parameters produce a broken file, not an ffmpeg
/// error. `-c copy` seeking via `inpoint`/`outpoint` is keyframe-bound, not
/// frame-exact — callers relying on a precise cut position (e.g.
/// gap-splice) must independently verify where each seam actually landed;
/// this function only builds and runs the concat.
pub(super) async fn concat_mkvs_n(
    list_dir: &Path,
    entries: &[ConcatEntry<'_>],
    dst: &Path,
) -> anyhow::Result<()> {
    use std::process::Stdio;

    // ffconcat quoting: single-quote each path, escaping embedded quotes.
    fn quote(p: &Path) -> String {
        p.to_string_lossy().replace('\'', r"'\''")
    }
    let stem = dst
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "concat".into());
    let list_path = list_dir.join(format!("{stem}.concat.txt"));
    let mut list = String::from("ffconcat version 1.0\n");
    for e in entries {
        list.push_str(&format!("file '{}'\n", quote(e.path)));
        // Order matters to ffmpeg: inpoint before outpoint, both after `file`.
        if let Some(i) = e.inpoint {
            list.push_str(&format!("inpoint {i:.3}\n"));
        }
        if let Some(o) = e.outpoint {
            list.push_str(&format!("outpoint {o:.3}\n"));
        }
    }
    crate::iomon::fs::write(Cat::ConcatList, &list_path, list).await?;

    // One full-file pass at a time on the recordings drive (see io_gate).
    let gate = crate::io_gate::local_pass(&crate::io_gate::gate_label("concat", dst), dst).await;
    let out = loop {
        let readrate = crate::io_gate::readrate_for(dst);
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-y")
            .arg("-fflags")
            .arg("+genpts");
        if let Some(rate) = readrate {
            cmd.arg("-readrate").arg(format!("{rate}"));
        }
        cmd.arg("-f")
            .arg("concat")
            .arg("-safe")
            .arg("0")
            .arg("-i")
            .arg(&list_path)
            .arg("-map")
            .arg("0:v?")
            .arg("-map")
            .arg("0:a?")
            .arg("-c")
            .arg("copy")
            .arg("-avoid_negative_ts")
            .arg("make_zero")
            .arg(dst)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        // spawn + wait_with_output (≡ output()) so the PID is sampleable.
        let out = match cmd.spawn() {
            Ok(child) => {
                let _io_guard = crate::iomon::track_tool(child.id(), "ffmpeg", "concat", dst);
                if let Some(pid) = child.id() {
                    gate.set_pid(pid);
                }
                child.wait_with_output().await
            }
            Err(e) => Err(e),
        };
        if let Ok(out) = &out
            && !out.status.success()
            && readrate.is_some()
            && crate::io_gate::is_readrate_error(&String::from_utf8_lossy(&out.stderr))
        {
            crate::io_gate::mark_readrate_unsupported();
            continue; // retry once without the throttle (latch is process-wide)
        }
        break out;
    };
    let _ = crate::iomon::fs::remove_file(Cat::ConcatList, &list_path).await;
    let out = out?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail_lines: Vec<&str> = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(3)
            .collect();
        let tail_msg: String = tail_lines.into_iter().rev().collect::<Vec<_>>().join(" | ");
        anyhow::bail!(
            "ffmpeg concat failed (exit {:?}): {tail_msg}",
            out.status.code()
        )
    }
}

/// When `progress_tx` is `Some((tx, task_id))`, ffmpeg progress is streamed as
/// `AppEvent::BackgroundTaskProgress` events on `tx`.
pub async fn remux_ts_to_mkv(
    src: &Path,
    dst: &Path,
    progress_tx: Option<(EventTx, u64)>,
    opts: &crate::models::RemuxOpts,
) -> anyhow::Result<()> {
    remux_ts_to_mkv_impl(src, dst, progress_tx, opts, true).await
}

/// `allow_readrate: false` = the pacing-collapse retry (see below): the
/// throttle is skipped for THIS file only, unlike the process-wide
/// unsupported-flag latch.
pub(super) async fn remux_ts_to_mkv_impl(
    src: &Path,
    dst: &Path,
    progress_tx: Option<(EventTx, u64)>,
    opts: &crate::models::RemuxOpts,
    allow_readrate: bool,
) -> anyhow::Result<()> {
    // One full-file pass at a time on the recordings drive (see io_gate).
    // With a task attached, the queue wait is reported as live progress info
    // (what holds the gate + queue depth) so a queued remux never looks stale.
    let label = crate::io_gate::gate_label("remux", dst);
    let gate = match &progress_tx {
        Some((tx, id)) => {
            let (tx, id) = (tx.clone(), *id);
            crate::io_gate::local_pass_with_progress(&label, dst, move |waited, holders, waiting, paused| {
                let info = if paused {
                    crate::io_gate::paused_wait_info(waited)
                } else {
                    crate::io_gate::wait_info(waited, holders, waiting)
                };
                let _ = tx.send(AppEvent::BackgroundTaskProgress { id, progress: None, info });
            })
            .await
        }
        None => crate::io_gate::local_pass(&label, dst).await,
    };
    remux_ts_to_mkv_gated(src, dst, progress_tx, opts, allow_readrate, gate).await
}

/// The gated body of [`remux_ts_to_mkv_impl`]. The disk-gate permit is an
/// argument so retries (pacing collapse, readrate-unsupported) KEEP this
/// file's turn. Releasing and re-acquiring used to send every retry to the
/// back of the queue — with a backlog of remuxes, each file burned a
/// 3-minute throttled attempt per gate turn, got killed by the pacing
/// watchdog, and rejoined the back: the whole queue carouseled for hours
/// without a single file finishing (observed 2026-07-12, ~10 queued passes
/// cycling across app restarts).
async fn remux_ts_to_mkv_gated(
    src: &Path,
    dst: &Path,
    progress_tx: Option<(EventTx, u64)>,
    opts: &crate::models::RemuxOpts,
    allow_readrate: bool,
    gate: crate::io_gate::LocalPass,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let readrate = if allow_readrate { crate::io_gate::readrate_for(dst) } else { None };

    // Get total duration so we can compute a percentage from ffmpeg's output.
    let total_us: Option<i64> = if progress_tx.is_some() {
        media_duration_secs(src).await.map(|s| s * 1_000_000)
    } else {
        None
    };

    // Look for a thumbnail sidecar sitting next to the source TS (either our
    // HTTP fetch → `{stem}.thumbnail.jpg`, or yt-dlp's `--write-thumbnail` →
    // `{stem}.webp`/`.jpg`/`.png`). If found and embedding is enabled, attach
    // it as MKV cover art so media players (mpv, VLC, …) pick it up automatically.
    let thumbnail = if opts.embed_thumbnail { find_thumbnail_for(src) } else { None };

    // Collect subtitle sidecars if embedding is enabled.
    let subs: Vec<PathBuf> = if opts.embed_subs {
        collect_subtitle_sidecars(src)
    } else {
        Vec::new()
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y");
    // Throttle how fast ffmpeg reads (and therefore writes) so this pass
    // can't saturate the drive live captures are writing to.
    if let Some(rate) = readrate {
        cmd.arg("-readrate").arg(format!("{rate}"));
    }
    cmd.arg("-i").arg(src);

    // Add subtitle sidecar inputs before -map flags so we can reference them.
    for sub in &subs {
        cmd.arg("-i").arg(sub);
    }

    cmd
        // Keep EVERY video/audio/subtitle stream, not just ffmpeg's default
        // one-per-type — otherwise the extra audio tracks captured via
        // `--hls-audio-select=*` would be dropped here. Map by type (each `?`
        // optional) rather than `-map 0` so TS data streams (e.g. timed-ID3),
        // which MKV can't hold, don't fail the remux.
        .arg("-map").arg("0:v?")
        .arg("-map").arg("0:a?")
        .arg("-map").arg("0:s?");

    // Map each subtitle sidecar input stream.
    for i in 1..=subs.len() {
        cmd.arg("-map").arg(format!("{i}:s?"));
    }

    cmd.arg("-c").arg("copy");

    // Title metadata tag (template expanded — never raw `{braces}`).
    if opts.embed_title && !opts.title_template.trim().is_empty() {
        cmd.arg("-metadata").arg(format!("title={}", expand_title_tag(opts, dst)));
    }

    if let Some(ref thumb) = thumbnail {
        let ext = thumb
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jpg")
            .to_ascii_lowercase();
        let mime = match ext.as_str() {
            "png"  => "image/png",
            "webp" => "image/webp",
            _      => "image/jpeg",
        };
        let cover_name = format!("cover.{ext}");
        cmd.arg("-attach").arg(thumb)
            .arg("-metadata:s:t").arg(format!("mimetype={mime}"))
            .arg("-metadata:s:t").arg(format!("filename={cover_name}"));
    }

    cmd
        // Write structured key=value progress lines to stdout.
        .arg("-progress").arg("pipe:1")
        // Suppress the default per-frame stats line that goes to stderr.
        .arg("-nostats")
        .arg(dst)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn()?;
    let _io_guard = crate::iomon::track_tool(child.id(), "ffmpeg", "remux", dst);
    if let Some(pid) = child.id() {
        gate.set_pid(pid);
    }
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Collect stderr in background so we can report it on failure.
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut lines: Vec<String> = Vec::new();
        while let Ok(Some(line)) = reader.next_line().await {
            lines.push(line);
        }
        lines
    });

    // `-readrate` paces reading against the INPUT'S OWN timestamps. A TS with
    // timestamp discontinuities (streamlink's ad-break cuts, PTS wraps) can
    // make ffmpeg believe it's hours "ahead of schedule" and sleep — observed
    // crawling at 0.6× realtime instead of 30×, wedging the 1-permit gate for
    // hours and leaving the take looking stuck "recording". If the media
    // position falls hopelessly behind wall clock, kill and retry this one
    // file without the throttle (still serialized by the gate).
    let pace_started = std::time::Instant::now();
    let mut pacing_broken = false;

    // Read stdout for progress events.  ffmpeg's -progress writes one key=value
    // per line; a block ends with `progress=continue` (or `progress=end`).
    // `out_time_ms` is microseconds despite the name (historical ffmpeg quirk).
    {
        let mut reader = BufReader::new(stdout).lines();
        // Per-block accumulators.
        let mut blk_frame = String::new();
        let mut blk_fps   = String::new();
        let mut blk_speed = String::new();
        let mut blk_pos   = String::new(); // out_time  HH:MM:SS.μs
        let mut blk_us: Option<i64> = None; // out_time_ms in microseconds

        while let Ok(Some(line)) = reader.next_line().await {
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                match k {
                    "frame"       => blk_frame = v.to_string(),
                    "fps"         => blk_fps   = v.to_string(),
                    "speed"       => blk_speed = v.to_string(),
                    "out_time"    => blk_pos   = v.to_string(),
                    "out_time_ms" => blk_us    = v.parse::<i64>().ok(),
                    "progress"    => {
                        // End of block — fire one event.
                        if let Some((ref tx, task_id)) = progress_tx {
                            let progress = blk_us.and_then(|us| {
                                total_us.filter(|&t| t > 0).map(|t| {
                                    (us as f64 / t as f64).clamp(0.0, 1.0) as f32
                                })
                            });
                            // Trim subsecond noise from out_time (keep HH:MM:SS).
                            let pos_short = blk_pos.split('.').next().unwrap_or(&blk_pos);
                            let info = format!(
                                "frame={} fps={} speed={} pos={}",
                                blk_frame, blk_fps, blk_speed, pos_short,
                            );
                            let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                id: task_id,
                                progress,
                                info,
                            });
                        }
                        // Pacing watchdog: with a working -readrate the media
                        // position advances at ~readrate× wall clock (disk
                        // pressure might drag it lower, but never below a few
                        // × realtime). Under 2× after 3 minutes = the pacing
                        // math is broken for this file.
                        if readrate.is_some() && !pacing_broken {
                            let elapsed = pace_started.elapsed().as_secs_f64();
                            let media_s = blk_us.unwrap_or(0) as f64 / 1e6;
                            if elapsed > 180.0 && media_s < elapsed * 2.0 {
                                pacing_broken = true;
                                let _ = child.start_kill();
                            }
                        }
                        // Reset for next block.
                        blk_frame.clear(); blk_fps.clear();
                        blk_speed.clear(); blk_pos.clear(); blk_us = None;
                    }
                    _ => {}
                }
            }
        }
    }

    let status = child.wait().await?;
    let stderr_lines = stderr_task.await.unwrap_or_default();

    if pacing_broken {
        warn!(
            "remux: -readrate pacing collapsed (media time far behind wall clock — \
             disk contention or timestamp discontinuities in the source, e.g. \
             ad-break cuts); retrying without the throttle, keeping this file's \
             disk-gate turn: {}",
            src.display()
        );
        return Box::pin(remux_ts_to_mkv_gated(src, dst, progress_tx, opts, false, gate)).await;
    }

    if status.success() {
        Ok(())
    } else {
        // Older ffmpeg (< 5.0) rejects -readrate outright: latch that
        // process-wide and retry this pass once without the throttle,
        // keeping our disk-gate turn.
        if readrate.is_some() && crate::io_gate::is_readrate_error(&stderr_lines.join("\n")) {
            crate::io_gate::mark_readrate_unsupported();
            return Box::pin(remux_ts_to_mkv_gated(src, dst, progress_tx, opts, true, gate)).await;
        }
        let code = status.code().unwrap_or(-1);
        // Grab the last few non-empty lines of stderr — ffmpeg prints the
        // relevant error at the end (e.g. "Invalid data found when processing input").
        let tail: String = stderr_lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(3)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        if tail.is_empty() {
            anyhow::bail!("ffmpeg remux failed (exit {})", code)
        } else {
            anyhow::bail!("ffmpeg remux failed (exit {}): {}", code, tail)
        }
    }
}

/// Expand the remux title-tag template. `{name}` is always the destination
/// file stem; without recording context (`opts.title_vars` = None) `{title}`
/// falls back to the stem too and the remaining tokens render empty — the
/// literal template must never become the MKV title (which is exactly what
/// happened until 2026-07-13: the raw `{channel}: {year}-…` string was
/// embedded verbatim into every finalized MKV with the title tag enabled).
pub(super) fn expand_title_tag(opts: &crate::models::RemuxOpts, dst: &Path) -> String {
    let name = dst
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let v = opts.title_vars.clone().unwrap_or_default();
    let title = if v.title.trim().is_empty() { name.clone() } else { v.title };
    let (date, year, month, day) = if v.started_at > 0 {
        // Same UTC civil time the filename templates use.
        let (y, mo, d, ..) = civil_from_unix_utc(v.started_at);
        (
            format!("{y:04}-{mo:02}-{d:02}"),
            format!("{y:04}"),
            format!("{mo:02}"),
            format!("{d:02}"),
        )
    } else {
        Default::default()
    };
    opts.title_template
        .replace("{name}", &name)
        .replace("{title_trimmed}", &super::trim_title_commands(&title))
        .replace("{title}", &title)
        .replace("{channel}", &v.channel)
        .replace("{games}", &v.games)
        .replace("{date}", &date)
        .replace("{year}", &year)
        .replace("{month}", &month)
        .replace("{day}", &day)
        .trim()
        .to_string()
}

pub(super) async fn file_len(path: &Path) -> u64 {
    crate::iomon::fs::metadata(Cat::FsProbe, path)
        .await
        .map(|m| m.len())
        .unwrap_or(0)
}

/// How long a download's output (log + capture files) may stay completely
/// unchanged before the stall watchdog kills its process tree. Generous on
/// purpose: a live capture writes continuously, reconnect attempts produce log
/// lines, and post-download merges grow stem-sibling temp files — 15 minutes of
/// total silence means a wedged tool, not a slow one.
pub(super) const STALL_KILL_SECS: u64 = 15 * 60;

/// How long the CAPTURE files alone may stay frozen while the log keeps
/// moving. A tool endlessly retry-logging against a dead stream never trips
/// the total-silence rule (its log grows forever), but a capture that already
/// has bytes and hasn't gained one in an hour is wedged. Metadata-only phases
/// (format probing, waiting) are exempt via `has_capture`.
pub(super) const CAPTURE_STALL_KILL_SECS: u64 = 60 * 60;

/// Handle-based (size, modified-unix-secs) — `fs::metadata` reads the
/// directory entry, which NTFS updates lazily while a writer holds the file
/// open; opening the file queries the handle, which is always current.
pub(super) async fn open_len_mtime(p: &Path) -> Option<(u64, i64)> {
    let f = crate::iomon::fs::open(Cat::FsProbe, p).await.ok()?;
    let md = f.metadata().await.ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some((md.len(), mtime))
}

/// One stall-watchdog sample over the log + every capture-stem file.
pub(super) struct StallSample {
    /// Log size (any write changes it).
    pub(super) log_sig: u64,
    /// Sum of sizes + names of capture-stem files (SABR `.part`s, merge temps,
    /// the bare capture) — any media write anywhere changes it.
    pub(super) capture_sig: u64,
    /// Newest handle-based modified time across all of the above (unix secs;
    /// 0 when nothing exists).
    pub(super) newest_mtime: i64,
    /// True when at least one capture-stem file has bytes.
    pub(super) has_capture: bool,
}

pub(super) async fn stall_sample(log_path: &Path, capture_path: &Path) -> StallSample {
    let (log_sig, mut newest_mtime) = open_len_mtime(log_path).await.unwrap_or((0, 0));
    let mut capture_sig = 0u64;
    let mut has_capture = false;
    if let (Some(dir), Some(stem)) = (capture_path.parent(), capture_path.file_stem()) {
        let stem = stem.to_string_lossy().into_owned();
        if let Ok(mut rd) = crate::iomon::fs::read_dir(Cat::FsProbe, dir).await {
            while let Ok(Some(e)) = rd.next_entry().await {
                let name = e.file_name().to_string_lossy().into_owned();
                // Only THIS capture's own outputs count: the rest after the
                // stem must be an extension chain (".f303.mp4.part"), which
                // excludes a retake's "{stem} (2).ts" — its growth must not
                // reset take 1's stall timers.
                let Some(rest) = name.strip_prefix(&stem) else {
                    continue;
                };
                if !rest.starts_with('.') {
                    continue;
                }
                // Files other processes write next to the capture must not
                // count as capture activity either: the tool's own log (X.ts →
                // X.log) and the chat/subtitle sidecars a separate process
                // appends — otherwise endless retry-logging / an active chat
                // keeps `capture_sig` moving and the frozen rules never fire.
                if e.path() == log_path
                    || rest.contains("live_chat")
                    || rest.ends_with(".chat.jsonl")
                    || rest.ends_with(".chat.log")
                    || rest.ends_with(".vtt")
                {
                    continue;
                }
                let (len, mtime) = open_len_mtime(&e.path()).await.unwrap_or((0, 0));
                capture_sig = capture_sig.wrapping_add(len).wrapping_add(name.len() as u64);
                has_capture |= len > 0;
                newest_mtime = newest_mtime.max(mtime);
            }
        }
    }
    StallSample { log_sig, capture_sig, newest_mtime, has_capture }
}

/// Media duration of `path` in whole seconds via `ffprobe`, or `None` if it
/// can't be determined (file missing/unreadable, ffprobe absent, or a container
/// — e.g. a still-growing `.ts` — that doesn't report a duration).
pub(super) async fn media_duration_secs(path: &Path) -> Option<i64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error", "-hide_banner", "-show_entries", "format=duration",
              "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if secs.is_finite() && secs >= 0.0 {
        Some(secs as i64)
    } else {
        None
    }
}

/// Per-seam landed-position check for gap-splice: the PTS (seconds) of the
/// first video packet at or after `near_secs` in an already-produced file.
/// `-c copy` seeking via ffconcat `inpoint`/`outpoint` is keyframe-bound,
/// not frame-exact (see `concat_mkvs_n`'s doc comment) — a snap in either
/// direction can leave a real seam defect (a gap, or duplicated content)
/// that an aggregate duration check alone would miss (a snap on one seam
/// can cancel a snap on another in the total). Comparing this against the
/// intended splice position is that independent check. `None` on any ffprobe
/// failure — callers must treat that as "can't verify," never as a pass.
pub(super) async fn packet_pts_near(path: &Path, near_secs: f64) -> Option<f64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v", "error",
        "-select_streams", "v:0",
        "-read_intervals", &format!("{}%+#1", near_secs.max(0.0)),
        "-show_entries", "packet=pts_time",
        "-of", "csv=p=0",
    ])
    .arg(path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse::<f64>().ok()
}

/// First presentation timestamp of `path` in seconds via ffprobe
/// (`format=start_time`), or `None` if it can't be determined. For a raw
/// MPEG-TS capture (even a still-growing one — start_time comes from the
/// first packets) this is the broadcast's own PTS timeline position, which is
/// what the head backfill's exact-splice math needs; a remuxed MKV reports ~0
/// (timestamps reset), which the caller's sanity window rejects. Also accepts
/// a local `.m3u8` slice with absolute https segment URLs (the same
/// protocol-whitelist arrangement `recovery::mux_playlist_to_mkv` feeds
/// ffmpeg) — that's how the DVR playlist's segment-0 PTS is probed.
pub async fn media_start_time_secs(path: &Path) -> Option<f64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error", "-hide_banner",
              "-protocol_whitelist", "file,http,https,tcp,tls,crypto",
              "-show_entries", "format=start_time",
              "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if secs.is_finite() && secs >= 0.0 { Some(secs) } else { None }
}

/// True if the MKV at `path` already has at least one attachment stream (cover art).
/// Runs `ffprobe` synchronously — only call from a blocking context or `spawn_blocking`.
pub fn mkv_has_thumbnail(path: &Path) -> bool {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-select_streams", "t",
               "-show_entries", "stream=index", "-of", "csv=p=0"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// Embed `thumb` as a cover-art attachment into an existing MKV file in-place
/// (remux to a temp file, then atomically replace the original).
pub async fn embed_thumbnail_into_mkv(mkv: &Path, thumb: &Path) -> anyhow::Result<()> {
    let tmp = mkv.with_extension("tmp.mkv");
    let ext = thumb.extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png"  => "image/png",
        "webp" => "image/webp",
        _      => "image/jpeg",
    };
    let cover_name = format!("cover.{ext}");
    // One full-file pass at a time on the recordings drive (see io_gate).
    let gate =
        crate::io_gate::local_pass(&crate::io_gate::gate_label("embed-thumbnail", mkv), mkv).await;
    let out = loop {
        let readrate = crate::io_gate::readrate_for(mkv);
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-y");
        if let Some(rate) = readrate {
            cmd.arg("-readrate").arg(format!("{rate}"));
        }
        cmd.arg("-i").arg(mkv)
            .arg("-i").arg(thumb)
            .arg("-map").arg("0")
            .arg("-c").arg("copy")
            .arg("-attach").arg(thumb)
            .arg("-metadata:s:t").arg(format!("mimetype={mime}"))
            .arg("-metadata:s:t").arg(format!("filename={cover_name}"))
            .arg(&tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        // spawn + wait_with_output (≡ output()) so the PID is sampleable.
        let child = cmd.spawn()?;
        let _io_guard = crate::iomon::track_tool(child.id(), "ffmpeg", "embed-thumbnail", mkv);
        if let Some(pid) = child.id() {
            gate.set_pid(pid);
        }
        let out = child.wait_with_output().await?;
        if !out.status.success()
            && readrate.is_some()
            && crate::io_gate::is_readrate_error(&String::from_utf8_lossy(&out.stderr))
        {
            crate::io_gate::mark_readrate_unsupported();
            continue; // retry without the throttle
        }
        break out;
    };
    if !out.status.success() {
        let _ = crate::iomon::fs::remove_file(Cat::Thumbnail, &tmp).await;
        let tail = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ffmpeg embed-thumbnail failed: {}", tail.trim().lines().last().unwrap_or(""));
    }
    crate::iomon::fs::rename(Cat::Thumbnail, &tmp, mkv).await?;
    Ok(())
}

/// Embed every subtitle sidecar (`.vtt`/`.srt`/…, per [`collect_subtitle_sidecars`])
/// sitting next to `mkv` into the file in-place (remux to a temp file, then
/// atomically replace the original — same idiom as [`embed_thumbnail_into_mkv`]),
/// tagging each stream's `language` metadata from the sidecar's `{stem}.<lang>.ext`
/// filename when present. Deletes the sidecar files on success — for the on-demand
/// Video downloader specifically (all downloads land in one flat folder, unlike
/// live recordings' per-channel subdirs), a lingering `.en.vtt` next to the video
/// is just clutter, not useful. Returns `Ok(false)` (no-op) when there's nothing to
/// embed, so callers can call this unconditionally after every video download.
pub async fn embed_subtitles_into_mkv(mkv: &Path) -> anyhow::Result<bool> {
    let subs = collect_subtitle_sidecars(mkv);
    if subs.is_empty() {
        return Ok(false);
    }
    let tmp = mkv.with_extension("tmp.mkv");
    // One full-file pass at a time on the recordings drive (see io_gate).
    let gate =
        crate::io_gate::local_pass(&crate::io_gate::gate_label("embed-subs", mkv), mkv).await;
    let out = loop {
        let readrate = crate::io_gate::readrate_for(mkv);
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-y");
        if let Some(rate) = readrate {
            cmd.arg("-readrate").arg(format!("{rate}"));
        }
        cmd.arg("-i").arg(mkv);
        for sub in &subs {
            cmd.arg("-i").arg(sub);
        }
        cmd.arg("-map").arg("0:v?").arg("-map").arg("0:a?").arg("-map").arg("0:s?");
        for i in 1..=subs.len() {
            cmd.arg("-map").arg(format!("{i}:s?"));
        }
        cmd.arg("-c").arg("copy");
        for (i, sub) in subs.iter().enumerate() {
            if let Some(lang) = subtitle_lang_from_name(mkv, sub) {
                cmd.arg(format!("-metadata:s:s:{i}")).arg(format!("language={lang}"));
            }
        }
        cmd.arg(&tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        // spawn + wait_with_output (≡ output()) so the PID is sampleable.
        let child = cmd.spawn()?;
        let _io_guard = crate::iomon::track_tool(child.id(), "ffmpeg", "embed-subs", mkv);
        if let Some(pid) = child.id() {
            gate.set_pid(pid);
        }
        let out = child.wait_with_output().await?;
        if !out.status.success()
            && readrate.is_some()
            && crate::io_gate::is_readrate_error(&String::from_utf8_lossy(&out.stderr))
        {
            crate::io_gate::mark_readrate_unsupported();
            continue; // retry without the throttle
        }
        break out;
    };
    if !out.status.success() {
        let _ = crate::iomon::fs::remove_file(Cat::Thumbnail, &tmp).await;
        let tail = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ffmpeg embed-subs failed: {}", tail.trim().lines().last().unwrap_or(""));
    }
    crate::iomon::fs::rename(Cat::Thumbnail, &tmp, mkv).await?;
    for sub in &subs {
        let _ = crate::iomon::fs::remove_file(Cat::Thumbnail, sub).await;
    }
    Ok(true)
}

/// Embed chapter markers into an existing MKV file in-place (same temp-file,
/// atomic-replace idiom as [`embed_thumbnail_into_mkv`] and
/// [`embed_subtitles_into_mkv`]). `ffmetadata` is an already-built
/// `;FFMETADATA1` chapters body (see `crate::chapters::build_ffmetadata`).
/// `-map_metadata 0` keeps the container's own existing global metadata
/// (e.g. the title tag `promote_capture` already set) untouched — chapters
/// are pulled from the new input via `-map_chapters 1` only.
pub async fn embed_chapters_into_mkv(mkv: &Path, ffmetadata: &str) -> anyhow::Result<()> {
    let chapters_txt = mkv.with_extension("chapters.ffmeta.txt");
    crate::iomon::fs::write(Cat::ConcatList, &chapters_txt, ffmetadata).await?;
    let tmp = mkv.with_extension("tmp.mkv");
    // One full-file pass at a time on the recordings drive (see io_gate).
    let gate =
        crate::io_gate::local_pass(&crate::io_gate::gate_label("embed-chapters", mkv), mkv).await;
    let out = loop {
        let readrate = crate::io_gate::readrate_for(mkv);
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-y");
        if let Some(rate) = readrate {
            cmd.arg("-readrate").arg(format!("{rate}"));
        }
        cmd.arg("-i").arg(mkv)
            .arg("-i").arg(&chapters_txt)
            .arg("-map").arg("0")
            .arg("-map_metadata").arg("0")
            .arg("-map_chapters").arg("1")
            .arg("-c").arg("copy")
            .arg(&tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        // spawn + wait_with_output (≡ output()) so the PID is sampleable.
        let child = cmd.spawn()?;
        let _io_guard = crate::iomon::track_tool(child.id(), "ffmpeg", "embed-chapters", mkv);
        if let Some(pid) = child.id() {
            gate.set_pid(pid);
        }
        let out = child.wait_with_output().await?;
        if !out.status.success()
            && readrate.is_some()
            && crate::io_gate::is_readrate_error(&String::from_utf8_lossy(&out.stderr))
        {
            crate::io_gate::mark_readrate_unsupported();
            continue; // retry without the throttle
        }
        break out;
    };
    let _ = crate::iomon::fs::remove_file(Cat::ConcatList, &chapters_txt).await;
    if !out.status.success() {
        let _ = crate::iomon::fs::remove_file(Cat::Thumbnail, &tmp).await;
        let tail = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ffmpeg embed-chapters failed: {}", tail.trim().lines().last().unwrap_or(""));
    }
    crate::iomon::fs::rename(Cat::Thumbnail, &tmp, mkv).await?;
    Ok(())
}

/// Extract the language code from a subtitle sidecar's name relative to `mkv`'s
/// stem (`{stem}.en.vtt` -> `Some("en")`; `{stem}.vtt` -> `None`, no code present).
pub(super) fn subtitle_lang_from_name(mkv: &Path, sub: &Path) -> Option<String> {
    let stem = mkv.file_stem()?.to_string_lossy().into_owned();
    let name = sub.file_stem()?.to_string_lossy().into_owned(); // strips only the trailing ext, e.g. "{stem}.en"
    let prefix = format!("{stem}.");
    name.strip_prefix(prefix.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}
/// ffprobe all streams of a file path or stream URL into [`MediaInfo`].
/// Extracts video codec, dimensions, fps and audio codec.
/// `None` if ffprobe fails / there's no readable video stream.
pub(super) async fn probe_media(target: &str) -> Option<MediaInfo> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v", "error",
        "-show_entries", "stream=codec_type,codec_name,width,height,r_frame_rate:format=duration",
        "-of", "json",
    ])
    .arg(target)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let streams = json["streams"].as_array()?;
    let mut info = MediaInfo::default();
    for stream in streams {
        let codec_type = stream["codec_type"].as_str().unwrap_or("");
        let codec_name = stream["codec_name"].as_str().unwrap_or("").to_string();
        match codec_type {
            "video" if info.vcodec.is_empty() => {
                info.vcodec = codec_name;
                if let (Some(w), Some(h)) = (
                    stream["width"].as_u64(),
                    stream["height"].as_u64(),
                ) {
                    info.width = w.to_string();
                    info.height = h.to_string();
                }
                if let Some(rate) = stream["r_frame_rate"].as_str() {
                    info.fps = fmt_fps(rate);
                }
            }
            "audio" if info.acodec.is_empty() => {
                info.acodec = codec_name;
            }
            _ => {}
        }
    }
    info.duration_secs = json["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|d| d.round() as i64);
    // Require real pixel dimensions (ffprobe can report "N/A" for odd inputs).
    if info.width.parse::<u32>().is_err() || info.height.parse::<u32>().is_err() {
        return None;
    }
    info.resolution = format!("{}x{}", info.width, info.height);
    Some(info)
}

/// Resolve a playable media URL for a stream (for pre-probe), then ffprobe it.
/// Best-effort; `None` on any failure (caller then leaves the media vars empty).
pub(super) async fn preprobe_media(
    tool: Tool,
    url: &str,
    quality: &str,
    auth: &AuthSource,
) -> Option<MediaInfo> {
    let target = resolve_play_url(tool, url, quality, auth).await?;
    probe_media(&target).await
}

/// Resolve a stream's direct media URL via the capture tool (so ffprobe can read
/// it before recording). `None` on failure.
pub(super) async fn resolve_play_url(
    tool: Tool,
    url: &str,
    quality: &str,
    auth: &AuthSource,
) -> Option<String> {
    let quality = resolved_quality(quality);
    let (program, args): (&str, Vec<String>) = match tool {
        // ffmpeg reads the source URL directly.
        Tool::Ffmpeg => return Some(url.to_string()),
        Tool::Streamlink => {
            let mut a = Vec::new();
            if Platform::detect(url) == Platform::Twitch {
                a.push("--twitch-supported-codecs=h264,h265,av1".to_string());
                if let AuthSource::Token(t) = auth {
                    a.push(format!("--twitch-api-header=Authorization=OAuth {t}"));
                }
            }
            a.push("--stream-url".into());
            a.push(url.to_string());
            a.push(quality);
            ("streamlink", a)
        }
        Tool::YtDlp => {
            let mut a = vec!["-g".to_string(), "--no-warnings".into(), "--no-playlist".into()];
            if quality != "best" {
                a.push("-f".into());
                a.push(quality);
            }
            match auth {
                AuthSource::CookiesBrowser(b) => {
                    a.push("--cookies-from-browser".into());
                    a.push(b.clone());
                }
                AuthSource::CookiesFile(p) => {
                    a.push("--cookies".into());
                    a.push(p.clone());
                }
                _ => {}
            }
            a.push(url.to_string());
            ("yt-dlp", a)
        }
    };
    let mut cmd = Command::new(program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // yt-dlp may print separate video+audio URLs; the first is the video stream.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
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
    fn title_tag_expands_tokens_and_never_leaks_braces() {
        let dst = Path::new(r"A:\streams\Vienna\Vienna - 2026-07-11 - stream.mkv");
        let mut opts = crate::models::RemuxOpts {
            embed_title: true,
            title_template: "{channel}: {year}-{month}-{day} - {title} ({games})".into(),
            title_vars: Some(crate::models::TitleVars {
                title: "cozy stream".into(),
                channel: "Vienna".into(),
                games: "Just Chatting".into(),
                started_at: 1783811671, // 2026-07-11 UTC
            }),
            ..Default::default()
        };
        assert_eq!(
            expand_title_tag(&opts, dst),
            "Vienna: 2026-07-11 - cozy stream (Just Chatting)"
        );
        // No recording context: {title}/{name} fall back to the file stem,
        // everything else renders empty — never literal {braces}.
        opts.title_vars = None;
        let s = expand_title_tag(&opts, dst);
        assert!(!s.contains('{'), "raw template leaked: {s}");
        assert!(s.contains("Vienna - 2026-07-11 - stream"));
        // Default "{title}" template + no vars = the stem.
        opts.title_template = "{title}".into();
        assert_eq!(expand_title_tag(&opts, dst), "Vienna - 2026-07-11 - stream");
    }

    #[test]
    fn subtitle_lang_from_name_extracts_code() {
        let mkv = Path::new("C:/vids/My Video.mkv");
        assert_eq!(
            subtitle_lang_from_name(mkv, Path::new("C:/vids/My Video.en.vtt")),
            Some("en".to_string())
        );
        assert_eq!(
            subtitle_lang_from_name(mkv, Path::new("C:/vids/My Video.en-US.vtt")),
            Some("en-US".to_string())
        );
        // No language segment present (bare `{stem}.vtt`) -> no tag.
        assert_eq!(subtitle_lang_from_name(mkv, Path::new("C:/vids/My Video.vtt")), None);
    }
    #[test]
    fn parse_rendition_heights_fps_and_aliases() {
        assert_eq!(parse_rendition("720p60"), Some((720, 60)));
        assert_eq!(parse_rendition("1080p"), Some((1080, 30)));
        assert_eq!(parse_rendition("480p30"), Some((480, 30)));
        assert_eq!(parse_rendition("best"), None);
        assert_eq!(parse_rendition("worst"), None);
        assert_eq!(parse_rendition("audio_only"), None);
        assert_eq!(parse_rendition("chunked"), None);
    }
}
