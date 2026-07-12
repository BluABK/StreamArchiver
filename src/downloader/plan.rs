//! Capture/video/chat plan building: argument builders, SABR capture
//! args, `build_plan` and `build_video_plan`.

use super::*;

/// Build the command + file plan for a monitor.
///
/// All tools capture to a progressively-flushed `.ts` (so an abrupt
/// kill/crash leaves usable data) and remux losslessly to `.mkv` on clean
/// stop. If the user picked the TS container, the `.ts` is kept as-is.
/// Append audio/subtitle track-selection flags appropriate to `tool`.
///
/// `audio`/`subs` are the per-monitor selectors: empty = the tool's default, a
/// case-insensitive `all` (or `*`) = every track, otherwise a comma-separated
/// pass-through list. Audio selection is a streamlink feature
/// (`--hls-audio-select`); subtitle capture is a yt-dlp feature (`--sub-langs`,
/// written as sidecar files next to the recording). Each tool ignores the
/// selector it can't honor — streamlink can't mux subtitles, and ffmpeg has no
/// per-track selector (its capture maps all video+audio tracks regardless of the
/// value; see the `Tool::Ffmpeg` arm of `build_plan`). Pushed before the user's
/// `extra_args` so a power user can still override. Selector values use the
/// `--flag=value` form so a value can never be mis-parsed as a separate option.
///
/// `chat` requests yt-dlp's `live_chat` pseudo-subtitle (YouTube chat), folded
/// into the same `--sub-langs` list. Twitch chat is captured separately by a
/// native logger (see `chat::log_twitch_chat`), so callers pass `chat = false`
/// for Twitch yt-dlp monitors.
pub(crate) fn push_track_args(args: &mut Vec<String>, tool: Tool, audio: &str, subs: &str, chat: bool) {
    let audio = audio.trim();
    let subs = subs.trim();
    let is_all = |s: &str| s.eq_ignore_ascii_case("all") || s == "*";
    match tool {
        Tool::Streamlink => {
            if !audio.is_empty() {
                let sel = if is_all(audio) { "*" } else { audio };
                args.push(format!("--hls-audio-select={sel}"));
            }
        }
        Tool::YtDlp => {
            // Combine subtitle languages and (optionally) the live-chat pseudo-
            // track into one --sub-langs list; both are written as sidecar files
            // (`.vtt` / `.live_chat.json`) next to the capture — a lossless,
            // replayable archive, NOT embedded into the container. The media-rename
            // step moves these companions so they stay matched to the final file
            // (see `rename_companion_sidecars`). `all`/`*` mean every subtitle.
            let mut langs: Vec<&str> = Vec::new();
            if !subs.is_empty() {
                langs.push(if is_all(subs) { "all" } else { subs });
            }
            if chat {
                langs.push("live_chat");
            } else {
                // live_chat blocks video download on live streams: yt-dlp downloads
                // the chat stream indefinitely before starting the video. Exclude it
                // so `all` doesn't pull it in. The dedicated chat sidecar
                // (run_chat_download) handles it when chat_log is enabled.
                langs.retain(|l| *l != "live_chat");
                if langs.iter().any(|l| *l == "all") {
                    langs.push("-live_chat");
                }
            }
            if !langs.is_empty() {
                args.push(format!("--sub-langs={}", langs.join(",")));
                args.push("--write-subs".into());
            }
        }
        Tool::Ffmpeg => {}
    }
}

/// Build a yt-dlp `-f` format selector implementing on-demand `audio_tracks`
/// selection for the Video downloader — YouTube VODs can carry multiple audio
/// tracks (the original plus dubbed languages, or descriptive audio), and
/// yt-dlp's own default format selection only ever picks one. Returns `None`
/// when `audio` is empty (no override — the tool's/`quality` field's own
/// default wins). The second element is whether `--audio-multistreams` is
/// required (whenever more than one audio format could be muxed in, so
/// yt-dlp doesn't silently keep only the highest-priority one).
///
/// `[language^=<code>]` (prefix match, not `=` exact match) so a plain "en"
/// still matches a track tagged "en-US"/"en-GB" — YouTube's exact codes vary
/// and aren't predictable from the UI alone.
pub(super) fn yt_audio_format_selector(audio: &str) -> Option<(String, bool)> {
    let audio = audio.trim();
    if audio.is_empty() {
        return None;
    }
    if audio.eq_ignore_ascii_case("all") || audio == "*" {
        // Every audio-only format (every language) as separate muxed streams.
        return Some(("bv*+ba.*".to_string(), true));
    }
    let codes: Vec<&str> = audio.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if codes.is_empty() {
        return None;
    }
    let parts: Vec<String> = codes.iter().map(|c| format!("ba[language^={c}]")).collect();
    Some((format!("bv*+{}", parts.join("+")), codes.len() > 1))
}

/// Returns the URL yt-dlp should receive for a live YouTube recording.
/// Extract a YouTube video ID from a URL (`watch?v=`, `youtu.be/`, `/live/ID`).
/// Returns `None` for channel or handle URLs that don't embed a specific video ID.
pub(super) fn extract_yt_video_id(url: &str) -> Option<String> {
    for marker in &["?v=", "&v="] {
        if let Some(pos) = url.find(marker) {
            let rest = &url[pos + marker.len()..];
            let id: String = rest.chars().take_while(|c| *c != '&' && *c != '#').collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    if let Some(pos) = url.find("youtu.be/") {
        let rest = &url[pos + "youtu.be/".len()..];
        let id: String = rest.chars().take_while(|c| *c != '?' && *c != '#' && *c != '/').collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    if let Some(pos) = url.find("/live/") {
        let rest = &url[pos + "/live/".len()..];
        let id: String = rest.chars().take_while(|c| *c != '?' && *c != '#' && *c != '/').collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

/// Channel URLs (/@handle, /channel/UC…, /c/name, /user/name) are resolved to
/// their /live variant so yt-dlp goes straight to the active stream instead of
/// enumerating the whole channel. Specific-video URLs (watch?v=, youtu.be/,
/// `/live/<id>`) and already-suffixed /live URLs are left unchanged.
pub(crate) fn youtube_live_url(url: &str) -> String {
    let u = url.trim_end_matches('/');
    let is_specific = u.contains("/watch?")
        || u.contains("/live/")
        || u.contains("youtu.be/")
        || u.ends_with("/live");
    if is_specific {
        url.to_string()
    } else {
        format!("{u}/live")
    }
}

/// Append the Settings-level yt-dlp postprocessor args (`--postprocessor-args`
/// specs, `;;`-separated) to a yt-dlp invocation. This is how the user
/// throttles yt-dlp's INTERNAL ffmpeg passes — the post-stream SABR format
/// merge reads+writes the whole multi-GB capture at full disk speed on the
/// recordings drive, and the app's own gates/`-readrate` can't reach inside
/// the tool (e.g. `Merger+ffmpeg_i:-readrate 30`). Pushed BEFORE the global
/// default args so a per-target spec there can override it.
pub(super) fn push_ppa_args(args: &mut Vec<String>) {
    let specs = crate::io_gate::ytdlp_ppa();
    for spec in specs.split(";;") {
        let spec = spec.trim();
        if !spec.is_empty() {
            args.push("--postprocessor-args".into());
            args.push(spec.to_string());
        }
    }
}

/// Build a yt-dlp chat-only plan: `--skip-download --sub-langs=live_chat
/// --write-subs` with the same output path as the video so the `.live_chat.json`
/// sidecar lands next to it. Auth and global defaults are forwarded as-is so the
/// cookies / token work the same as they do for the video process.
pub fn build_chat_plan(
    row: &MonitorWithChannel,
    capture_path: &Path,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    system_program: &str,
) -> DownloadPlan {
    let mut args = vec!["--no-part".to_string()];
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
    // Global defaults first so our required args below can override them.
    args.extend_from_slice(ytdlp_global_args);
    args.push("--skip-download".into());
    args.push("--sub-langs=live_chat".into());
    args.push("--write-subs".into());
    // Stock yt-dlp finds ZERO formats on YouTube *live* streams these days
    // (its web clients are SABR-only there, and it drops SABR formats as
    // unsupported) and then aborts the whole extraction — killing this leg
    // ~3s after spawn even though it only wants the live_chat "subtitle",
    // which comes from the page data and needs no media format at all.
    args.push("--ignore-no-formats-error".into());
    args.push("-o".into());
    args.push(capture_path.to_string_lossy().into_owned());
    let url = if row.monitor.platform() == Platform::YouTube {
        youtube_live_url(&row.monitor.url)
    } else {
        row.monitor.url.clone()
    };
    args.push(url);
    DownloadPlan {
        program: system_program.to_string(),
        args,
        capture_path: capture_path.to_path_buf(),
        final_path: capture_path.to_path_buf(),
        remux_to_mkv: false,
        writes_own_thumbnail: false,
        mode: "chat".into(),
    }
}

/// Build the DASH companion plan for dual capture. Mirrors the system yt-dlp live
/// path (.ts → MKV remux) but forces the configured DASH format, captures from the
/// live edge (`--no-live-from-start` — the SABR primary owns capture-from-start),
/// and writes a sibling `{stem}.dash.{ts,mkv}` next to the primary so both files
/// belong to the same take.
pub(super) fn build_dash_companion_plan(
    primary_final: &Path,
    row: &MonitorWithChannel,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    system_program: &str,
    dash_format: &str,
    pot_args: &str,
) -> DownloadPlan {
    let dir = primary_final.parent().unwrap_or_else(|| Path::new("."));
    let stem = primary_final
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Capture into the hidden `.cache\` (promoted up on finish); final lands in dir.
    let ts_path = cache_dir(dir).join(format!("{stem}.dash.ts"));
    let mkv_path = dir.join(format!("{stem}.dash.mkv"));
    let mut args = vec![
        "--no-part".to_string(),
        "--hls-use-mpegts".into(),
        "-o".into(),
        ts_path.to_string_lossy().into_owned(),
        // DASH can't reliably rewind; the SABR primary owns capture-from-start.
        "--no-live-from-start".into(),
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
    push_ppa_args(&mut args);
    args.extend_from_slice(ytdlp_global_args);
    if !pot_args.is_empty() {
        args.push("--extractor-args".into());
        args.push(pot_args.to_string());
    }
    args.push("-f".into());
    args.push(dash_format.to_string());
    args.extend(split_args(&row.monitor.extra_args));
    args.push(youtube_live_url(&row.monitor.url));
    DownloadPlan {
        program: system_program.to_string(),
        args,
        capture_path: ts_path,
        final_path: mkv_path,
        remux_to_mkv: true,
        writes_own_thumbnail: false,
        mode: "hybrid-dash".into(),
    }
}
/// Build the yt-dlp SABR capture args, shared by [`build_plan`]'s SABR branch and
/// resume. Writes the final MKV directly to `out_mkv`; forces the SABR formats +
/// extractor-args (or the manual raw override); applies cookies, the global Settings
/// args, and the PO-token provider args. `from_start` selects `--live-from-start`
/// (rewind to the broadcast start) vs `--no-live-from-start` (join at the live
/// edge). `sort` is the resolved codec/quality `-S` value (`""` = none). All
/// inputs deterministic, so a resume re-runs byte-identically and yt-dlp
/// continues from the surviving `.state`.
#[allow(clippy::too_many_arguments)]
pub(super) fn sabr_capture_args(
    out_mkv: &Path,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    sabr: &SabrConfig,
    extra: &[String],
    url: &str,
    from_start: bool,
    sort: &str,
) -> Vec<String> {
    let mut args = vec![
        "--no-part".to_string(),
        "-o".into(),
        out_mkv.to_string_lossy().into_owned(),
        if from_start { "--live-from-start" } else { "--no-live-from-start" }.into(),
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
    // Postprocessor throttle first, then global Settings args (e.g.
    // --js-runtimes node) — later --postprocessor-args for the same target win.
    push_ppa_args(&mut args);
    args.extend_from_slice(ytdlp_global_args);
    // PO-token provider args (e.g. bgutil) — a separate --extractor-args entry,
    // applied regardless of the preset/raw choice below.
    if !sabr.pot_args.is_empty() {
        args.push("--extractor-args".into());
        args.push(sabr.pot_args.clone());
    }
    // Manual raw args override the format + extractor-args preset entirely.
    let raw = split_args(&sabr.raw_args);
    if raw.is_empty() {
        args.push("--extractor-args".into());
        args.push(sabr.extractor_args.clone());
        args.push("-f".into());
        args.push(sabr.format.clone());
    } else {
        args.extend(raw);
    }
    // Codec/quality preference: a `-S` sort layered on the `-f` selector, so it
    // only decides which format each `b*` selector resolves to. Before `extra`
    // so the dropdown wins over any user `-S` in the monitor's extra args.
    if !sort.is_empty() {
        args.push("-S".into());
        args.push(sort.to_string());
    }
    args.extend_from_slice(extra);
    args.push(youtube_live_url(url));
    args
}

/// Build the yt-dlp SABR args for a throwaway live-edge preview download
/// ("Play new instance"): identical to [`sabr_capture_args`] except it joins
/// at the live edge instead of rewinding to the start — the whole point is
/// that the preview files BEGIN at the edge, so the player needs no seeking —
/// and it prefers fMP4-compatible formats: the preview is served to the
/// player through a generated live HLS playlist of byteranges
/// ([`crate::hls_preview`]), which requires ISOBMFF per-format files (a VP9
/// pick lands in a Matroska container HLS can't address). Falls back to the
/// configured selector when no mp4+m4a pair exists (playback then degrades
/// to `appending://`, which stalls once caught up to the live edge).
pub(crate) fn sabr_preview_args(
    out_mkv: &Path,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    sabr: &SabrConfig,
    extra: &[String],
    url: &str,
) -> Vec<String> {
    // Live edge (`from_start=false`): the preview files BEGIN at the edge. No
    // codec `-S` here — the preview forces its own fMP4 `-f` for HLS-playlist
    // playback below, which a codec sort could fight.
    let mut args = sabr_capture_args(out_mkv, auth, ytdlp_global_args, sabr, extra, url, false, "");
    if let Some(pos) = args.iter().position(|a| a == "-f")
        && let Some(v) = args.get_mut(pos + 1)
    {
        *v = format!("bv[protocol=sabr][ext=mp4]+ba[protocol=sabr][ext=m4a]/{v}");
    }
    args
}

/// Whether a monitor's YouTube live capture goes through the SABR dev build.
///
/// SABR is used for **all** YouTube yt-dlp live captures (from-start AND live
/// edge), not just from-start: YouTube now serves live via SABR, and the system
/// build's default clients return "No video formats found" at the live edge.
/// `capture_from_start` controls only from-start-vs-edge (see `sabr_capture_args`),
/// not whether SABR is used. Requires the SABR dev build to be configured.
pub(super) fn sabr_selected(m: &Monitor, ytdlp: &YtDlpBins) -> bool {
    m.tool == Tool::YtDlp && m.platform() == Platform::YouTube && ytdlp.sabr.usable()
}

/// Compute the download mode string for a monitor recording.
pub(super) fn recording_mode(m: &Monitor, use_sabr: bool, secondary: bool) -> String {
    match m.tool {
        Tool::Streamlink => "live".into(),
        Tool::Ffmpeg => "direct".into(),
        Tool::YtDlp => {
            if use_sabr {
                if m.dual_capture {
                    if secondary { "hybrid-dash".into() } else { "hybrid".into() }
                } else {
                    "sabr".into()
                }
            } else {
                "dash".into()
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_plan(
    row: &MonitorWithChannel,
    started_at: i64,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    stream_id: Option<&str>,
    stream_title: &str,
    media: Option<&MediaInfo>,
    went_live_at: i64,
    ytdlp: &YtDlpBins,
) -> DownloadPlan {
    let m = &row.monitor;
    let ch = &row.channel;
    let dir = PathBuf::from(&m.output_dir);
    let quality = resolved_quality(&m.quality);
    let use_sabr = sabr_selected(m, ytdlp);
    let mode = recording_mode(m, use_sabr, false);
    let platform = m.platform().as_str().to_string();
    let tool_label = m.tool.label();
    // `{video_id}` (platform id when known), `{take}` (attempt number), and the
    // media vars (filled only when `media` is provided, i.e. pre-probe). Then
    // avoid clobbering an existing finished file of the same name.
    // `{games}` isn't known until the stream ends; it's filled at the post-rename.
    let stem = monitor_stem(
        m, &ch.name, started_at, stream_id, stream_title, row.recording_count, &quality, media, "",
        tool_label, &mode, &platform, went_live_at,
    );
    // Keep the child tool's working path under MAX_PATH (streamlink/yt-dlp are
    // Python, no long-path manifest) — the per-component cap already applied
    // inside `expand_template` alone isn't enough (see `MAX_CHILD_PATH_UTF16`);
    // `build_video_plan` already does this for on-demand downloads, live
    // captures need the same guard.
    let stem = stem_capped_for_child_path(&dir, &stem);
    let extra = split_args(&m.extra_args);
    // SABR (YouTube capture-from-start via the dev build) writes the final MKV
    // directly — it merges separate SABR audio+video through ffmpeg, which the
    // mpegts/.ts intermediate can't hold. Everything else captures to .ts first.
    let final_ext = if use_sabr {
        "mkv"
    } else {
        match m.container {
            Container::Mkv => "mkv",
            Container::Ts => "ts",
        }
    };
    let stem = unique_stem(&dir, &stem, final_ext, None);

    // Working files capture into the hidden `.cache\` subdir; the finished file is
    // promoted up to the output dir on a clean finalize (same-volume rename).
    let cache = cache_dir(&dir);
    let (capture_path, final_path, remux_to_mkv) = if use_sabr {
        // SABR writes the final MKV directly (no .ts intermediate); promoted via a
        // move on finish.
        (cache.join(format!("{stem}.mkv")), dir.join(format!("{stem}.mkv")), false)
    } else {
        match m.container {
            Container::Mkv => (
                cache.join(format!("{stem}.ts")),
                dir.join(format!("{stem}.mkv")),
                true,
            ),
            Container::Ts => (
                cache.join(format!("{stem}.ts")),
                dir.join(format!("{stem}.ts")),
                false,
            ),
        }
    };
    let cap_str = capture_path.to_string_lossy().into_owned();

    let mut writes_own_thumbnail = false;
    let (program, args) = match m.tool {
        Tool::Streamlink => {
            let mut args = Vec::new();
            if m.platform() == Platform::Twitch {
                // Reach 1440p/2K (HEVC) enhanced-broadcasting sources.
                args.push("--twitch-supported-codecs=h264,h265,av1".to_string());
                // Authenticated capture (sub-only / Turbo ad-free) via the token.
                if let AuthSource::Token(t) = auth {
                    args.push(format!("--twitch-api-header=Authorization=OAuth {t}"));
                }
            }
            if m.capture_from_start {
                // Rewind to the start of the DVR window (best-effort on Twitch).
                args.push("--hls-live-restart".into());
            }
            args.push("--retry-streams".into());
            args.push("3".into());
            args.push("--retry-max".into());
            args.push("5".into());
            push_track_args(&mut args, Tool::Streamlink, &m.audio_tracks, &m.subtitle_tracks, false);
            args.extend(extra);
            args.push("-o".into());
            args.push(cap_str);
            args.push(m.url.clone());
            args.push(quality);
            ("streamlink".to_string(), args)
        }
        Tool::YtDlp if use_sabr => {
            // SABR capture via the dev build: writes the final MKV directly (SABR
            // merges separate audio+video, so no mpegts/.ts). Chat, assets, and
            // thumbnails are handled off this process (the dev build is a stale fork
            // — keep its surface minimal): no --write-thumbnail / --sub-langs here.
            let args = sabr_capture_args(
                &capture_path, auth, ytdlp_global_args, &ytdlp.sabr, &extra, &m.url,
                m.capture_from_start, &resolve_sabr_sort(m, &ytdlp.sabr),
            );
            (ytdlp.sabr.binary.clone(), args)
        }
        Tool::YtDlp => {
            let mut args = vec![
                "--no-part".to_string(),
                "--hls-use-mpegts".into(), // progressive .ts output
                "-o".into(),
                cap_str,
            ];
            if m.capture_from_start {
                args.push("--live-from-start".into());
            } else {
                args.push("--no-live-from-start".into());
            }
            // Authenticated capture (members-only / sub-only) via cookies.
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
            // Postprocessor throttle, then the global defaults from Settings →
            // yt-dlp default arguments. Per-monitor extra_args extend after
            // and can override both.
            push_ppa_args(&mut args);
            args.extend_from_slice(ytdlp_global_args);
            // PO-token provider (bgutil HTTP) — YouTube requires a proof-of-origin
            // token for video formats regardless of whether SABR is in use.
            if m.platform() == Platform::YouTube && !ytdlp.sabr.pot_args.is_empty() {
                args.push("--extractor-args".into());
                args.push(ytdlp.sabr.pot_args.clone());
            }
            // Never request live_chat for live recordings: yt-dlp's live_chat
            // downloader runs until the stream ends and blocks video download in the
            // same process. YouTube chat replay can be downloaded after the stream
            // via build_video_plan (which does pass chat_log through). Twitch chat
            // uses the native WS logger regardless.
            push_track_args(&mut args, Tool::YtDlp, &m.audio_tracks, &m.subtitle_tracks, false);
            if m.fetch_thumbnail {
                args.push("--write-thumbnail".to_string());
                writes_own_thumbnail = true;
            }
            args.extend(extra);
            let url = if m.platform() == Platform::YouTube {
                youtube_live_url(&m.url)
            } else {
                m.url.clone()
            };
            args.push(url);
            (ytdlp.system_program(), args)
        }
        Tool::Ffmpeg => {
            let mut args = vec![
                "-y".to_string(),
                "-i".into(),
                m.url.clone(),
                // Keep all video + audio tracks (ffmpeg's default copies only one
                // per type). TS can't reliably hold text subtitles, so subs are
                // left to the MKV remux.
                "-map".into(),
                "0:v?".into(),
                "-map".into(),
                "0:a?".into(),
                "-c".into(),
                "copy".into(),
            ];
            args.extend(extra);
            args.push(cap_str);
            ("ffmpeg".to_string(), args)
        }
    };

    DownloadPlan {
        program,
        args,
        capture_path,
        final_path,
        remux_to_mkv,
        writes_own_thumbnail,
        mode,
    }
}

/// Build the command + file plan for an on-demand video/VOD download.
///
/// Output is always MKV: yt-dlp downloads the full video and remuxes to MKV
/// directly; streamlink/ffmpeg capture to `.ts` then remux losslessly. Unlike
/// [`build_plan`], there are no live-stream flags (`--live-from-start`,
/// `--retry-streams`).
pub fn build_video_plan(
    v: &Video,
    started_at: i64,
    title: &str,
    channel: &str,
    video_id: &str,
    auth: &AuthSource,
    ytdlp_global_args: &[String],
    media: Option<&MediaInfo>,
    ytdlp: &YtDlpBins,
) -> DownloadPlan {
    let dir = PathBuf::from(&v.output_dir);
    let quality = resolved_quality(&v.quality);
    let stem = video_stem(v, started_at, title, channel, video_id, &quality, media, v.tool.label(), Platform::detect(&v.url).as_str());
    let extra = split_args(&v.extra_args);
    let platform = Platform::detect(&v.url);
    // Keep the child tool's working paths under MAX_PATH (Python has no
    // long-path support; the per-component cap alone can't see the full path).
    let stem = stem_capped_for_child_path(&dir, &stem);
    // Don't clobber an existing finished file (all video tools end at .mkv).
    let stem = unique_stem(&dir, &stem, "mkv", None);
    let final_path = dir.join(format!("{stem}.mkv"));
    // Working files capture into the hidden `.cache\`; promoted up on finish.
    let cache = cache_dir(&dir);

    match v.tool {
        Tool::YtDlp => {
            // yt-dlp downloads the complete video and remuxes to MKV. `%(ext)s`
            // becomes `mkv` after the remux, so the cache file is predictable.
            let out_tmpl = cache
                .join(format!("{stem}.%(ext)s"))
                .to_string_lossy()
                .into_owned();
            let mut args = vec![
                "--no-part".to_string(),
                "--no-playlist".into(),
                "--merge-output-format".into(),
                "mkv".into(),
                "--remux-video".into(),
                "mkv".into(),
                // Emit a parseable percent + speed per line for the UI progress
                // bar and Speed column (`;;` separates the two fields).
                "--newline".into(),
                "--progress-template".into(),
                "download:DLPCT=%(progress._percent_str)s;;SPEED=%(progress.speed)s".into(),
                "-o".into(),
                out_tmpl,
            ];
            // Global VOD/video download rate limit (Settings → Downloads;
            // default off). yt-dlp-syntax value, placed before the global
            // defaults + per-video extra args so either can override it.
            // Deliberately never applied to live captures — throttling a
            // live-edge download just makes it fall behind and lose data —
            // and the other tools have no equivalent flag (streamlink has no
            // rate limiter; ffmpeg pulls are readrate-gated elsewhere).
            let rate = crate::io_gate::download_rate_limit();
            if !rate.is_empty() {
                args.push("--limit-rate".into());
                args.push(rate);
            }
            // `quality` is a full escape hatch (a user can type any yt-dlp format
            // string there) and always wins if set; audio-track selection only
            // synthesizes its own `-f` when `quality` is left at the default, since
            // yt-dlp's `-f` isn't cumulative — two of them would just have the last
            // one win, silently discarding whichever was meant to combine with it.
            let audio_sel = yt_audio_format_selector(&v.audio_tracks);
            if quality != "best" {
                args.push("-f".into());
                args.push(quality);
            } else if let Some((sel, multistream)) = &audio_sel {
                args.push("-f".into());
                args.push(sel.clone());
                if *multistream {
                    args.push("--audio-multistreams".into());
                }
            }
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
            // The stock build's default client mix is broken for YouTube VODs
            // (2026-07): tv_downgraded 403s even with a valid PO token, tv
            // serves DRM formats, web/web_safari are SABR-only (dropped as
            // unsupported), ios/android serve nothing — mweb + a GVS PO token
            // is the mix that downloads. First in the args so later
            // --extractor-args youtube:… entries (Settings global defaults or
            // per-video extra args) can override it if the landscape shifts.
            if platform == Platform::YouTube {
                args.push("--extractor-args".into());
                args.push("youtube:player_client=mweb".into());
            }
            // Postprocessor throttle (the VOD download's remux/merge is a
            // full-file ffmpeg pass inside yt-dlp), then the global defaults
            // from Settings → yt-dlp default arguments.
            push_ppa_args(&mut args);
            args.extend_from_slice(ytdlp_global_args);
            // PO-token provider (bgutil HTTP) — YouTube 403s googlevideo media
            // URLs fetched without a proof-of-origin token, so VOD downloads
            // need it exactly like live captures do (the extraction step alone
            // still succeeds, which made these failures easy to miss).
            if platform == Platform::YouTube && !ytdlp.sabr.pot_args.is_empty() {
                args.push("--extractor-args".into());
                args.push(ytdlp.sabr.pot_args.clone());
            }
            // Subtitle + chat (live_chat) sidecars, fetched the same way as a live
            // capture; unlike a live capture, subtitles then get embedded into the
            // file (below, once it's fully promoted) instead of staying beside it —
            // audio-track selection was already applied above via `-f`, before
            // `quality`/extractor-args, since it's format-selection, not a
            // sidecar-writing flag `push_track_args` handles.
            push_track_args(
                &mut args,
                Tool::YtDlp,
                &v.audio_tracks,
                &v.subtitle_tracks,
                v.chat_log,
            );
            args.extend(extra);
            args.push(v.url.clone());
            DownloadPlan {
                program: ytdlp.resolve_program(&v.tool_binary),
                args,
                // yt-dlp writes the final MKV into .cache; promoted up via a move.
                capture_path: cache.join(format!("{stem}.mkv")),
                final_path,
                remux_to_mkv: false,
                writes_own_thumbnail: false,
                mode: "vod".into(),
            }
        }
        Tool::Streamlink => {
            let ts_path = cache.join(format!("{stem}.ts"));
            let mut args = Vec::new();
            if platform == Platform::Twitch {
                args.push("--twitch-supported-codecs=h264,h265,av1".to_string());
                if let AuthSource::Token(t) = auth {
                    args.push(format!("--twitch-api-header=Authorization=OAuth {t}"));
                }
            }
            // Audio-track selection (streamlink can't mux subtitles/chat).
            push_track_args(
                &mut args,
                Tool::Streamlink,
                &v.audio_tracks,
                &v.subtitle_tracks,
                v.chat_log,
            );
            args.extend(extra);
            args.push("-o".into());
            args.push(ts_path.to_string_lossy().into_owned());
            args.push(v.url.clone());
            args.push(quality);
            DownloadPlan {
                program: "streamlink".to_string(),
                args,
                capture_path: ts_path,
                final_path,
                remux_to_mkv: true,
                writes_own_thumbnail: false,
                mode: "vod".into(),
            }
        }
        Tool::Ffmpeg => {
            let ts_path = cache.join(format!("{stem}.ts"));
            let mut args = vec![
                "-y".to_string(),
                "-i".into(),
                v.url.clone(),
                // Keep all video + audio tracks (ffmpeg's default copies only one
                // per type); the MKV remux below preserves them.
                "-map".into(),
                "0:v?".into(),
                "-map".into(),
                "0:a?".into(),
                "-c".into(),
                "copy".into(),
            ];
            args.extend(extra);
            args.push(ts_path.to_string_lossy().into_owned());
            DownloadPlan {
                program: "ffmpeg".to_string(),
                args,
                capture_path: ts_path,
                final_path,
                remux_to_mkv: true,
                writes_own_thumbnail: false,
                mode: "vod".into(),
            }
        }
    }
}
/// Minimal whitespace arg splitter (double-quoted segments kept together).
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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
    fn streamlink_mkv_records_ts_then_remuxes() {
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert_eq!(plan.program, "streamlink");
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.remux_to_mkv);
        assert!(
            plan.args
                .iter()
                .any(|a| a.contains("twitch-supported-codecs"))
        );
        assert!(plan.args.iter().any(|a| a == "best"));
    }

    #[test]
    fn ytdlp_mkv_records_ts_then_remuxes() {
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert_eq!(plan.program, "yt-dlp");
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.remux_to_mkv);
        assert!(plan.args.iter().any(|a| a == "--live-from-start"));
        assert!(plan.args.iter().any(|a| a == "--hls-use-mpegts"));
    }

    #[test]
    fn streamlink_token_adds_twitch_auth_header() {
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::Token("abc123".into()),
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(
            plan.args
                .iter()
                .any(|a| a == "--twitch-api-header=Authorization=OAuth abc123")
        );
    }

    #[test]
    fn ytdlp_cookies_added() {
        let browser = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::CookiesBrowser("firefox".into()),
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        let joined = browser.args.join(" ");
        assert!(joined.contains("--cookies-from-browser firefox"));

        let file = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::CookiesFile("C:/c.txt".into()),
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(file.args.join(" ").contains("--cookies C:/c.txt"));
    }

    #[test]
    fn audio_subtitle_track_selection() {
        // streamlink: "all"/"*" -> --hls-audio-select=*; a list passes through.
        let mut r = row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        r.monitor.audio_tracks = "all".into();
        r.monitor.subtitle_tracks = "all".into(); // ignored by streamlink
        let plan = build_plan(&r, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(plan.args.iter().any(|a| a == "--hls-audio-select=*"));
        assert!(!plan.args.iter().any(|a| a == "--sub-langs"));

        let mut r2 = row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        r2.monitor.audio_tracks = "en,de".into();
        let plan2 = build_plan(&r2, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(plan2.args.iter().any(|a| a == "--hls-audio-select=en,de"));

        // yt-dlp: "all" subs -> --sub-langs=all --write-subs; audio ignored. The
        // `--flag=value` form keeps a value from being mis-parsed as an option.
        let mut y = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y.monitor.subtitle_tracks = "all".into();
        y.monitor.audio_tracks = "all".into(); // ignored by yt-dlp
        let yplan = build_plan(&y, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        // live_chat is excluded from the main process via negation so "all"
        // doesn't pull in the chat stream and block the video download.
        assert!(yplan.args.iter().any(|a| a == "--sub-langs=all,-live_chat"));
        assert!(yplan.args.iter().any(|a| a == "--write-subs"));
        assert!(!yplan.args.iter().any(|a| a == "--hls-audio-select=*"));

        // "*" is normalized to "all,-live_chat" for subtitles too.
        let mut y2 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y2.monitor.subtitle_tracks = "*".into();
        let yplan2 = build_plan(&y2, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(yplan2.args.iter().any(|a| a == "--sub-langs=all,-live_chat"));

        // A language list passes through verbatim (joined form).
        let mut y3 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y3.monitor.subtitle_tracks = "en,de".into();
        let yplan3 = build_plan(&y3, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(yplan3.args.iter().any(|a| a == "--sub-langs=en,de"));

        // Empty (existing-monitor default) adds no track flags at all.
        let plain = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(
            !plain
                .args
                .iter()
                .any(|a| a.starts_with("--hls-audio-select"))
        );
    }

    #[test]
    fn chat_logging_ytdlp_live_chat() {
        // live_chat is NEVER requested by build_plan: the yt-dlp live_chat downloader
        // runs until the stream ends and blocks video download in the same process.
        // Chat replay is downloaded by build_video_plan (VOD) after the stream ends.
        let mut y = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y.monitor.chat_log = true;
        y.monitor.subtitle_tracks = String::new();
        let plan = build_plan(&y, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(!plan.args.iter().any(|a| a.contains("live_chat")));
        assert!(!plan.args.iter().any(|a| a == "--write-subs"));

        // Explicit subtitle selection still works (just no live_chat folded in).
        let mut y2 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        y2.monitor.chat_log = true;
        y2.monitor.subtitle_tracks = "en".into();
        let plan2 = build_plan(&y2, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(!plan2.args.iter().any(|a| a.contains("live_chat")));
        assert!(plan2.args.iter().any(|a| a == "--sub-langs=en"));

        // Twitch + yt-dlp + chat_log -> NO yt-dlp live_chat (the native Twitch
        // chat logger handles it instead).
        let mut t = row(Tool::YtDlp, Container::Mkv, Platform::Twitch);
        t.monitor.chat_log = true;
        t.monitor.subtitle_tracks = String::new();
        let plant = build_plan(&t, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &YtDlpBins::default());
        assert!(!plant.args.iter().any(|a| a.contains("live_chat")));
    }
    #[test]
    fn streamlink_ts_keeps_ts() {
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Ts, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(plan.final_path.to_string_lossy().ends_with(".ts"));
        assert!(!plan.remux_to_mkv);
    }

    fn sabr_bins() -> YtDlpBins {
        YtDlpBins {
            system: String::new(),
            sabr: SabrConfig {
                enabled: true,
                binary: "C:/git/yt-dlp-dev/dist/yt-dlp.exe".into(),
                format: SABR_DEFAULT_FORMAT.into(),
                extractor_args: SABR_DEFAULT_EXTRACTOR_ARGS.into(),
                raw_args: String::new(),
                pot_args: SABR_DEFAULT_POT_ARGS.into(),
                codec_pref: SabrCodecPref::Auto,
                codec_custom: String::new(),
            },
            custom: Vec::new(),
        }
    }

    #[test]
    fn sabr_preview_args_join_at_live_edge() {
        // The live-edge preview ("Play new instance") must be the capture
        // command with from-start swapped for live-edge and an fMP4-first
        // format selector (HLS-playlist playback needs ISOBMFF files) —
        // nothing else.
        let bins = sabr_bins();
        let out = PathBuf::from(r"C:\tmp\.cache\preview.mkv");
        let preview = sabr_preview_args(
            &out, &AuthSource::None, &[], &bins.sabr, &[], "https://www.youtube.com/@chan",
        );
        assert!(preview.iter().any(|a| a == "--no-live-from-start"));
        assert!(!preview.iter().any(|a| a == "--live-from-start"));
        let fpos = preview.iter().position(|a| a == "-f").unwrap();
        assert_eq!(
            preview[fpos + 1],
            format!("bv[protocol=sabr][ext=mp4]+ba[protocol=sabr][ext=m4a]/{SABR_DEFAULT_FORMAT}")
        );
        let capture = sabr_capture_args(
            &out, &AuthSource::None, &[], &bins.sabr, &[], "https://www.youtube.com/@chan", true, "",
        );
        let normalize = |v: &[String]| {
            v.iter()
                .filter(|a| !a.contains("live-from-start") && !a.contains("[protocol=sabr]"))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(normalize(&preview), normalize(&capture));
    }

    #[test]
    fn deep_rewind_toggle_appends_extractor_arg() {
        let store = Store::open_in_memory().unwrap();
        // Off by default: the extractor-args are the plain preset.
        let off = load_ytdlp_bins(&store).sabr.extractor_args;
        assert_eq!(off, SABR_DEFAULT_EXTRACTOR_ARGS);
        assert!(!off.contains("enable_live_deep_rewind"));

        // Enabled: the deep-rewind key is appended under the youtube: namespace.
        store.set_setting("ytdlp_sabr_deep_rewind", "1").unwrap();
        let on = load_ytdlp_bins(&store).sabr.extractor_args;
        assert_eq!(
            on,
            format!("{SABR_DEFAULT_EXTRACTOR_ARGS};enable_live_deep_rewind=true")
        );

        // Explicit "0" is off again.
        store.set_setting("ytdlp_sabr_deep_rewind", "0").unwrap();
        assert!(
            !load_ytdlp_bins(&store)
                .sabr
                .extractor_args
                .contains("enable_live_deep_rewind")
        );

        // No double-append when the user already put it in the args field by hand.
        store.set_setting("ytdlp_sabr_deep_rewind", "1").unwrap();
        store
            .set_setting(
                "ytdlp_sabr_extractor_args",
                "youtube:formats=duplicate;enable_live_deep_rewind=true",
            )
            .unwrap();
        let manual = load_ytdlp_bins(&store).sabr.extractor_args;
        assert_eq!(manual.matches("enable_live_deep_rewind").count(), 1);
    }

    #[test]
    fn youtube_capture_from_start_uses_sabr_binary_and_mkv() {
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &sabr_bins(),
        );
        assert_eq!(plan.program, "C:/git/yt-dlp-dev/dist/yt-dlp.exe");
        // SABR writes the final MKV directly (no .ts intermediate, no remux), but
        // into the hidden .cache\ working dir; finalize promotes it to the output dir.
        assert!(plan.capture_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.capture_path.parent().unwrap().ends_with(CACHE_DIR_NAME));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert_eq!(plan.final_path.parent().unwrap(), std::path::Path::new("C:/rec"));
        assert_ne!(plan.capture_path, plan.final_path);
        assert!(!plan.remux_to_mkv);
        assert!(!plan.writes_own_thumbnail);
        assert!(plan.args.iter().any(|a| a == "--live-from-start"));
        assert!(!plan.args.iter().any(|a| a == "--hls-use-mpegts"));
        assert!(plan.args.iter().any(|a| a == "-f"));
        assert!(plan.args.iter().any(|a| a == SABR_DEFAULT_FORMAT));
        assert!(plan.args.iter().any(|a| a == "--extractor-args"));
        assert!(plan.args.iter().any(|a| a == SABR_DEFAULT_EXTRACTOR_ARGS));
    }

    #[test]
    fn sabr_pot_args_added_as_separate_extractor_args() {
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &sabr_bins(),
        );
        // The PO-token provider args ride on their own --extractor-args entry,
        // distinct from the youtube: SABR args — so there are two of them.
        let xargs = plan.args.iter().filter(|a| *a == "--extractor-args").count();
        assert_eq!(xargs, 2);
        assert!(plan.args.iter().any(|a| a == SABR_DEFAULT_POT_ARGS));
        assert!(plan.args.iter().any(|a| a == SABR_DEFAULT_EXTRACTOR_ARGS));

        // Empty pot args ⇒ only the youtube: --extractor-args entry.
        let mut bins = sabr_bins();
        bins.sabr.pot_args = String::new();
        let plan2 = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert_eq!(plan2.args.iter().filter(|a| *a == "--extractor-args").count(), 1);
    }

    #[test]
    fn sabr_raw_args_override_replaces_preset() {
        let mut bins = sabr_bins();
        bins.sabr.raw_args = "-f custom+best --extractor-args youtube:foo".into();
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert!(plan.args.iter().any(|a| a == "custom+best"));
        assert!(plan.args.iter().any(|a| a == "youtube:foo"));
        // The preset format/extractor-args are NOT injected when raw args are set.
        assert!(!plan.args.iter().any(|a| a == SABR_DEFAULT_FORMAT));
        assert!(!plan.args.iter().any(|a| a == SABR_DEFAULT_EXTRACTOR_ARGS));
    }

    #[test]
    fn sabr_used_for_live_edge_when_enabled() {
        // Disabled SABR → normal system yt-dlp path (.ts + mpegts). This is the
        // only case that still uses the system build (no dev build configured);
        // YouTube live is unrecordable via the default clients, but that's the
        // pre-existing "SABR not set up" limitation.
        let mut bins = sabr_bins();
        bins.sabr.enabled = false;
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert_eq!(plan.program, "yt-dlp");
        assert!(plan.args.iter().any(|a| a == "--hls-use-mpegts"));

        // Enabled SABR + capture_from_start = false → SABR at the LIVE EDGE.
        // (YouTube live is SABR-only now; the old default-client "dash" path
        // returned "No video formats found" and crash-looped.)
        let mut r = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        r.monitor.capture_from_start = false;
        let plan2 = build_plan(
            &r, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &sabr_bins(),
        );
        assert_eq!(plan2.program, "C:/git/yt-dlp-dev/dist/yt-dlp.exe");
        assert!(plan2.args.iter().any(|a| a == "--no-live-from-start"));
        assert!(!plan2.args.iter().any(|a| a == "--live-from-start"));
        assert!(!plan2.args.iter().any(|a| a == "--hls-use-mpegts"));
        assert!(plan2.args.iter().any(|a| a == "-f"));
        assert!(plan2.args.iter().any(|a| a == SABR_DEFAULT_FORMAT));
        assert!(plan2.args.iter().any(|a| a == SABR_DEFAULT_EXTRACTOR_ARGS));
        // Direct-MKV (no .ts remux) at the edge, same as from-start SABR.
        assert!(plan2.capture_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan2.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(!plan2.remux_to_mkv);
    }
    #[test]
    fn captures_route_into_hidden_cache_subdir() {
        // streamlink (MKV container): capture .ts under .cache\, final .mkv in dir.
        let plan = build_plan(
            &row(Tool::Streamlink, Container::Mkv, Platform::Twitch),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &YtDlpBins::default(),
        );
        assert!(plan.capture_path.parent().unwrap().ends_with(CACHE_DIR_NAME));
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert_eq!(plan.final_path.parent().unwrap(), std::path::Path::new("C:/rec"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));

        // Video (yt-dlp) also captures into .cache\, final .mkv in the output dir.
        let vplan = build_video_plan(
            &video(Tool::YtDlp, "https://youtu.be/abc"),
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::None,
            &[],
            None,
            &YtDlpBins::default(),
        );
        assert!(vplan.capture_path.parent().unwrap().ends_with(CACHE_DIR_NAME));
        assert_eq!(vplan.final_path.parent().unwrap(), std::path::Path::new("C:/vids"));
    }

    #[test]
    fn sabr_args_match_between_build_and_resume() {
        // A resume rebuilds the args from the same capture path; they must be
        // byte-identical so yt-dlp continues from the surviving `.state`.
        let bins = sabr_bins();
        let r = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        let plan = build_plan(&r, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins);
        let resume_args = sabr_capture_args(
            &plan.capture_path,
            &AuthSource::None,
            &[],
            &bins.sabr,
            &[],
            &r.monitor.url,
            r.monitor.capture_from_start,
            &resolve_sabr_sort(&r.monitor, &bins.sabr),
        );
        assert_eq!(plan.args, resume_args);
    }

    /// The value of a `-S` arg (the token after `-S`), or `None` if absent.
    fn sort_of(args: &[String]) -> Option<String> {
        args.iter().position(|a| a == "-S").map(|i| args[i + 1].clone())
    }

    #[test]
    fn sabr_codec_pref_injects_format_sort() {
        let bins = sabr_bins();

        // Default (Inherit → global Auto) emits no -S, so existing captures are
        // byte-identical.
        let auto = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        let plan = build_plan(&auto, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins);
        assert_eq!(sort_of(&plan.args), None);

        // A per-instance H.264 preference injects `-S res,fps,vcodec:h264`.
        let mut h264 = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        h264.monitor.sabr_codec_pref = SabrCodecPref::H264;
        let plan = build_plan(&h264, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins);
        assert_eq!(sort_of(&plan.args).as_deref(), Some("res,fps,vcodec:h264"));

        // Inherit falls through to the global default — here Best quality → `br`.
        let mut inh = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        inh.monitor.sabr_codec_pref = SabrCodecPref::Inherit;
        let mut bins_best = sabr_bins();
        bins_best.sabr.codec_pref = SabrCodecPref::BestQuality;
        let plan = build_plan(&inh, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins_best);
        assert_eq!(sort_of(&plan.args).as_deref(), Some("res,fps,br"));

        // A per-instance override wins over the global default.
        let mut over = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        over.monitor.sabr_codec_pref = SabrCodecPref::Vp9;
        let plan = build_plan(&over, 1_700_000_000, &AuthSource::None, &[], None, "", None, 0, &bins_best);
        assert_eq!(sort_of(&plan.args).as_deref(), Some("res,fps,vcodec:vp9"));
    }
    #[test]
    fn ytdlp_video_outputs_mkv_directly() {
        let plan = build_video_plan(
            &video(Tool::YtDlp, "https://youtube.com/watch?v=abc"),
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::None,
            &[],
            None,
            &YtDlpBins::default(),
        );
        assert_eq!(plan.program, "yt-dlp");
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(!plan.remux_to_mkv); // yt-dlp produces the MKV itself
        // Not a live capture: no live-stream flags.
        assert!(!plan.args.iter().any(|a| a == "--live-from-start"));
        assert!(plan.args.iter().any(|a| a == "--remux-video"));
    }

    #[test]
    fn yt_audio_format_selector_builds_expected_selectors() {
        assert_eq!(yt_audio_format_selector(""), None);
        assert_eq!(yt_audio_format_selector("   "), None);
        assert_eq!(
            yt_audio_format_selector("all"),
            Some(("bv*+ba.*".to_string(), true))
        );
        assert_eq!(
            yt_audio_format_selector("*"),
            Some(("bv*+ba.*".to_string(), true))
        );
        // A single language: no --audio-multistreams needed.
        assert_eq!(
            yt_audio_format_selector("en"),
            Some(("bv*+ba[language^=en]".to_string(), false))
        );
        // Multiple languages: muxed together, needs --audio-multistreams.
        assert_eq!(
            yt_audio_format_selector("en, de"),
            Some(("bv*+ba[language^=en]+ba[language^=de]".to_string(), true))
        );
    }

    #[test]
    fn ytdlp_video_audio_tracks_select_format() {
        // A single language picks that track, no --audio-multistreams.
        let mut v = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v.audio_tracks = "en".into();
        let plan = build_video_plan(&v, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &YtDlpBins::default());
        assert!(plan.args.iter().any(|a| a == "-f"));
        assert!(plan.args.iter().any(|a| a == "bv*+ba[language^=en]"));
        assert!(!plan.args.iter().any(|a| a == "--audio-multistreams"));

        // Multiple languages need --audio-multistreams to keep them all.
        let mut v2 = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v2.audio_tracks = "en,de".into();
        let plan2 = build_video_plan(&v2, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &YtDlpBins::default());
        assert!(plan2.args.iter().any(|a| a == "bv*+ba[language^=en]+ba[language^=de]"));
        assert!(plan2.args.iter().any(|a| a == "--audio-multistreams"));

        // A custom `quality` format string always wins over audio_tracks — two
        // -f flags would just have the second one silently discard the first.
        let mut v3 = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v3.audio_tracks = "en".into();
        v3.quality = "bestvideo[height<=720]+bestaudio".into();
        let plan3 = build_video_plan(&v3, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &YtDlpBins::default());
        assert!(plan3.args.iter().any(|a| a == "bestvideo[height<=720]+bestaudio"));
        assert!(!plan3.args.iter().any(|a| a.contains("language^=")));

        // Empty audio_tracks: no -f override at all (tool's own default wins).
        let plain = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        let plan4 = build_video_plan(&plain, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &YtDlpBins::default());
        assert!(!plan4.args.iter().any(|a| a == "-f"));
    }
    #[test]
    fn ytdlp_video_uses_sabr_binary_when_selected() {
        let mut v = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v.tool_binary = TOOL_BINARY_SABR.into();
        let plan = build_video_plan(
            &v,
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::None,
            &[],
            None,
            &sabr_bins(),
        );
        assert_eq!(plan.program, "C:/git/yt-dlp-dev/dist/yt-dlp.exe");
    }

    #[test]
    fn ytdlp_video_uses_custom_tool_binary_when_selected() {
        let mut v = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v.tool_binary = "myfork".into();
        let mut bins = YtDlpBins::default();
        bins.custom.push(CustomTool { alias: "myfork".into(), path: "C:/tools/myfork.exe".into() });
        let plan = build_video_plan(&v, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &bins);
        assert_eq!(plan.program, "C:/tools/myfork.exe");
    }
    #[test]
    fn ytdlp_video_quality_and_cookies() {
        let mut v = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v.quality = "bv*+ba".into();
        let plan = build_video_plan(
            &v,
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::CookiesBrowser("edge".into()),
            &[],
            None,
            &sabr_bins(),
        );
        let joined = plan.args.join(" ");
        assert!(joined.contains("-f bv*+ba"));
        assert!(joined.contains("--cookies-from-browser edge"));
        // YouTube VOD media URLs 403 without a PO token — the provider args
        // must be present just like on live captures — and need a client mix
        // that still serves downloadable formats (mweb).
        assert!(joined.contains(SABR_DEFAULT_POT_ARGS));
        assert!(joined.contains("youtube:player_client=mweb"));
    }

    #[test]
    fn streamlink_vod_remuxes_to_mkv() {
        let plan = build_video_plan(
            &video(Tool::Streamlink, "https://twitch.tv/videos/123"),
            1_700_000_000,
            "",
            "",
            "",
            &AuthSource::None,
            &[],
            None,
            &YtDlpBins::default(),
        );
        assert_eq!(plan.program, "streamlink");
        assert!(plan.capture_path.to_string_lossy().ends_with(".ts"));
        assert!(plan.final_path.to_string_lossy().ends_with(".mkv"));
        assert!(plan.remux_to_mkv);
        // No live-only retry flags for a VOD.
        assert!(!plan.args.iter().any(|a| a == "--retry-streams"));
    }
    #[test]
    fn video_plan_track_and_chat_args() {
        // yt-dlp: subtitle + chat selection -> --sub-langs (incl. live_chat) + --write-subs.
        let mut v = video(Tool::YtDlp, "https://youtube.com/watch?v=abc");
        v.subtitle_tracks = "all".into();
        v.chat_log = true;
        let plan = build_video_plan(&v, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &YtDlpBins::default());
        let joined = plan.args.join(" ");
        assert!(joined.contains("--sub-langs=all,live_chat"), "{joined}");
        assert!(plan.args.iter().any(|a| a == "--write-subs"), "{joined}");
        // The URL stays last (track args were inserted before it, not after).
        assert_eq!(plan.args.last().map(String::as_str), Some(v.url.as_str()));

        // streamlink: audio-track selection -> --hls-audio-select (subtitles n/a).
        let mut s = video(Tool::Streamlink, "https://twitch.tv/videos/123");
        s.audio_tracks = "en,de".into();
        let plan = build_video_plan(&s, 1_700_000_000, "", "", "", &AuthSource::None, &[], None, &YtDlpBins::default());
        let joined = plan.args.join(" ");
        assert!(joined.contains("--hls-audio-select=en,de"), "{joined}");
    }
    #[test]
    fn youtube_live_url_appends_live_to_channel_urls() {
        // Channel forms — must get /live appended.
        assert_eq!(youtube_live_url("https://www.youtube.com/@YUY_IX"), "https://www.youtube.com/@YUY_IX/live");
        assert_eq!(youtube_live_url("https://www.youtube.com/@YUY_IX/"), "https://www.youtube.com/@YUY_IX/live");
        assert_eq!(youtube_live_url("https://youtube.com/channel/UCabc123"), "https://youtube.com/channel/UCabc123/live");
        assert_eq!(youtube_live_url("https://youtube.com/c/SomeName"), "https://youtube.com/c/SomeName/live");
        assert_eq!(youtube_live_url("https://youtube.com/user/SomeName"), "https://youtube.com/user/SomeName/live");
        // Already has /live — unchanged.
        assert_eq!(youtube_live_url("https://www.youtube.com/@YUY_IX/live"), "https://www.youtube.com/@YUY_IX/live");
        // Specific video URLs — unchanged.
        assert_eq!(youtube_live_url("https://www.youtube.com/watch?v=abc123"), "https://www.youtube.com/watch?v=abc123");
        assert_eq!(youtube_live_url("https://youtu.be/abc123"), "https://youtu.be/abc123");
        assert_eq!(youtube_live_url("https://www.youtube.com/live/abc123"), "https://www.youtube.com/live/abc123");
    }
}
