//! Filename templates, sanitization, UTF-16/NTFS length budgets, and
//! stem construction for monitors and videos.

use super::*;

/// Actual media properties of a capture, for the filename `{resolution}`/
/// `{height}`/`{width}`/`{fps}`/`{vcodec}` variables. Empty fields render empty.
#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub resolution: String, // "1920x1080"
    pub width: String,
    pub height: String,
    pub fps: String,    // rounded whole number, e.g. "60"
    pub vcodec: String, // e.g. "h264"
    pub acodec: String, // e.g. "aac", "opus"
    /// Container duration in whole seconds, when ffprobe reported one — lets
    /// callers that already probed avoid a second ffprobe pass just for length.
    pub duration_secs: Option<i64>,
}

/// True if `template` uses any media-info variable (so we only probe when needed).
pub(super) fn template_wants_media(template: &str) -> bool {
    ["{resolution}", "{height}", "{width}", "{fps}", "{vcodec}", "{acodec}"]
        .iter()
        .any(|k| template.contains(k))
}

/// True if `template` uses `{games}` (only known after the stream ends, so it
/// triggers a post-capture rename even when media probing is off).
pub(super) fn template_wants_games(template: &str) -> bool {
    template.contains("{games}")
}

/// True if `template` uses `{title}` or `{title_trimmed}` (may not be known at
/// recording start for all platforms, so it also triggers a post-capture
/// rename to fill the real value).
pub(super) fn template_wants_title(template: &str) -> bool {
    template.contains("{title}") || template.contains("{title_trimmed}")
}

/// True if `template` uses `{went_live_date}` or `{went_live_time}` (only known
/// at the end of the recording, so it triggers a post-capture rename).
pub(super) fn template_wants_went_live(template: &str) -> bool {
    template.contains("{went_live_date}") || template.contains("{went_live_time}")
}

/// The stream title for a finished recording: the first `title` change logged
/// by the meta-watcher (which is the baseline/initial value, i.e. the title at
/// recording start). Returns empty when no title was polled (generic URLs, etc.).
pub(super) fn title_for_recording(store: &Store, rec_id: i64) -> String {
    store
        .meta_changes_for_recording(rec_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.kind == "title")
        .map(|c| c.new_value.clone())
        .next()
        .unwrap_or_default()
}

/// Max length of the expanded `{games}` value, to keep paths sane.
pub(super) const GAMES_MAX_LEN: usize = 100;

/// NTFS enforces a 255 **UTF-16 code-unit** limit per path component — Windows
/// counts a surrogate pair (any character outside the Basic Multilingual
/// Plane, e.g. most emoji) as 2 units, not 1. Exceeding it fails file
/// operations with `ERROR_INVALID_NAME` ("The filename, directory name, or
/// volume label syntax is incorrect", os error 123) rather than a
/// length-specific error. See [`MAX_STEM_UTF16_LEN`].
pub(super) const NTFS_MAX_COMPONENT_UTF16: usize = 255;

/// The shared filename stem is combined with several different suffixes: the
/// main recording (`.ts`/`.mkv`) and, later, companion sidecars
/// (`rename_companion_sidecars`/`move_companions`) — subtitle `.<lang>.vtt`,
/// thumbnail `.thumbnail.jpg`, chat `.chat.jsonl`/`.live_chat.json`.
/// `.live_chat.json` is the longest, so it sets the reservable budget.
pub(super) const LONGEST_COMPANION_SUFFIX_LEN: usize = ".live_chat.json".len(); // 16, ASCII

/// Reserve for `unique_stem`'s collision suffix (` (2)`, ` (3)`, …).
pub(super) const COLLISION_SUFFIX_RESERVE: usize = 10;

/// Hard cap on an expanded filename stem, in UTF-16 code units, so the stem
/// plus ANY companion suffix plus a collision suffix always stays under
/// [`NTFS_MAX_COMPONENT_UTF16`]. A long stream title combined with several
/// logged categories (`{games}`) is the realistic way to hit this: a
/// 2026-07-04 incident had a 251-unit stem that fit fine with `.mkv` (4 units)
/// but its `.chat.jsonl` sidecar (11 units) pushed it to 258 and the
/// post-capture rename silently failed, orphaning the sidecar under the old
/// `title-tba`/`games-tba` name.
pub(super) const MAX_STEM_UTF16_LEN: usize =
    NTFS_MAX_COMPONENT_UTF16 - LONGEST_COMPANION_SUFFIX_LEN - COLLISION_SUFFIX_RESERVE; // 229

/// Total-path budget for tool CHILD processes. yt-dlp/streamlink are Python
/// without a long-path manifest, so the WHOLE path (not just each component)
/// must stay under Windows' MAX_PATH of 260 WCHARs — our own Rust I/O is
/// exempt (std uses `\\?\` verbatim paths), which is why the per-component
/// cap alone was not enough: a 2026-07-06 VOD-archive download died in 3 s
/// because `.cache\{232-unit stem}.vod.mp4.ytdl` came to 263 units.
pub(super) const MAX_CHILD_PATH_UTF16: usize = 240; // 260 minus drive/NUL/safety headroom

/// Longest working-file suffix a child appends under `.cache\`:
/// yt-dlp `.fNNN.webm.part` (15) beats `.mp4.ytdl` / `.temp.mkv` (9). Round up.
pub(super) const LONGEST_CHILD_SUFFIX_UTF16: usize = 16;

/// Cap `stem` so `{dir}\.cache\{stem}{worst child suffix}{collision suffix}`
/// stays under [`MAX_CHILD_PATH_UTF16`] for the tool child process — a
/// total-path cap layered on top of `expand_template`'s per-component cap.
pub(super) fn stem_capped_for_child_path(dir: &Path, stem: &str) -> String {
    let cache_units = cache_dir(dir).to_string_lossy().encode_utf16().count();
    let budget = MAX_CHILD_PATH_UTF16
        .saturating_sub(cache_units + 1 + LONGEST_CHILD_SUFFIX_UTF16 + COLLISION_SUFFIX_RESERVE)
        .max(20); // pathological deep dirs still get a usable name
    stem_fitting_budget(stem, budget.min(MAX_STEM_UTF16_LEN))
}

/// Build the `{games}` value from the categories played: distinct names in order
/// of first appearance (case-insensitive dedup), joined with `, ` and capped to
/// [`GAMES_MAX_LEN`] characters. Illegal filename characters are handled later by
/// `sanitize_filename` in `expand_template`.
pub(super) fn format_games(categories: &[String]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for c in categories {
        let c = c.trim();
        if !c.is_empty() && !seen.iter().any(|s| s.eq_ignore_ascii_case(c)) {
            seen.push(c);
        }
    }
    let joined = seen.join(", ");
    if joined.chars().count() <= GAMES_MAX_LEN {
        joined
    } else {
        joined.chars().take(GAMES_MAX_LEN).collect()
    }
}

/// The `{games}` value for a finished recording: every distinct category logged
/// to `stream_meta_change` for it (empty when none was logged — e.g. a generic
/// URL, which has no metadata source).
/// `store.remux_opts()` with the title-tag template vars filled from a
/// recording (title/games from its meta changes, channel from its monitor,
/// start time from the row). Recording finalize/re-remux paths should pass
/// THIS to `promote_capture`/`remux_ts_to_mkv` so the embedded title tag
/// carries real values instead of file-stem fallbacks.
pub(super) fn remux_opts_for_recording(store: &Store, rec_id: i64) -> crate::models::RemuxOpts {
    let mut opts = store.remux_opts();
    if opts.embed_title
        && let Ok(Some(rec)) = store.get_recording(rec_id)
    {
        let channel = store
            .get_monitor_with_channel(rec.monitor_id)
            .ok()
            .flatten()
            .map(|r| r.channel.name)
            .unwrap_or_default();
        opts.title_vars = Some(crate::models::TitleVars {
            title: title_for_recording(store, rec_id),
            channel,
            games: games_for_recording(store, rec_id),
            started_at: rec.started_at,
        });
    }
    opts
}

pub(super) fn games_for_recording(store: &Store, rec_id: i64) -> String {
    let cats: Vec<String> = store
        .meta_changes_for_recording(rec_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.kind == "category")
        .map(|c| c.new_value)
        .collect();
    format_games(&cats)
}

/// Round an ffprobe `r_frame_rate` ("60/1", "30000/1001") to a whole-number fps
/// string; empty on parse failure.
pub(super) fn fmt_fps(rate: &str) -> String {
    let (n, d) = match rate.split_once('/') {
        Some((n, d)) => (n.trim().parse::<f64>().ok(), d.trim().parse::<f64>().ok()),
        None => (rate.trim().parse::<f64>().ok(), Some(1.0)),
    };
    match (n, d) {
        (Some(n), Some(d)) if d > 0.0 && n > 0.0 => (n / d).round().to_string(),
        _ => String::new(),
    }
}
/// The configured quality selector with the `best` default applied.
pub(crate) fn resolved_quality(q: &str) -> String {
    if q.trim().is_empty() {
        "best".to_string()
    } else {
        q.trim().to_string()
    }
}

/// Read the global filename media-probe mode from settings.
pub(super) fn media_info_mode(store: &Store) -> MediaInfoMode {
    MediaInfoMode::parse(
        &store
            .get_setting(K_FILENAME_MEDIA)
            .ok()
            .flatten()
            .unwrap_or_default(),
    )
}

/// Build a monitor recording's filename stem (no extension, no collision suffix).
/// Shared by [`build_plan`] and the post-capture rename so they agree.
#[allow(clippy::too_many_arguments)]
pub(super) fn monitor_stem(
    m: &Monitor,
    ch_name: &str,
    started_at: i64,
    stream_id: Option<&str>,
    stream_title: &str,
    recording_count: i64,
    quality: &str,
    media: Option<&MediaInfo>,
    games: &str,
    tool: &str,
    mode: &str,
    platform: &str,
    went_live: i64,
) -> String {
    let take = (recording_count + 1).to_string();
    let mi = media.cloned().unwrap_or_default();
    // Use token-labelled placeholders for title/games not yet known at recording
    // start, filled in at the post-recording rename.
    let title_val = if stream_title.is_empty() && template_wants_title(&m.filename_template) {
        "title-tba"
    } else {
        stream_title
    };
    let games_val = if games.is_empty() && template_wants_games(&m.filename_template) {
        "games-tba"
    } else {
        games
    };
    expand_template(
        &m.filename_template,
        &TemplateVars {
            name: ch_name,
            title: title_val,
            video_id: stream_id.unwrap_or(""),
            quality,
            take: &take,
            games: games_val,
            resolution: &mi.resolution,
            height: &mi.height,
            width: &mi.width,
            fps: &mi.fps,
            vcodec: &mi.vcodec,
            acodec: &mi.acodec,
            tool,
            mode,
            platform,
            secs: started_at,
            went_live,
            ..Default::default()
        },
    )
}

/// The `{name}` value for an on-demand video: the user's Name, else the resolved
/// title, else a generic fallback.
pub(super) fn video_name<'a>(v: &'a Video, resolved_title: &'a str) -> &'a str {
    let name_field = v.title.trim();
    if !name_field.is_empty() {
        name_field
    } else if !resolved_title.is_empty() {
        resolved_title
    } else {
        "video"
    }
}

/// Build an on-demand video's filename stem (no extension, no collision suffix).
/// Shared by [`build_video_plan`] and the post-capture rename.
#[allow(clippy::too_many_arguments)]
pub(super) fn video_stem(
    v: &Video,
    started_at: i64,
    title: &str,
    channel: &str,
    video_id: &str,
    quality: &str,
    media: Option<&MediaInfo>,
    tool: &str,
    platform: &str,
) -> String {
    let resolved = title.trim();
    let mi = media.cloned().unwrap_or_default();
    expand_template(
        &v.filename_template,
        &TemplateVars {
            name: video_name(v, resolved),
            title: resolved,
            channel: channel.trim(),
            video_id: video_id.trim(),
            quality,
            resolution: &mi.resolution,
            height: &mi.height,
            width: &mi.width,
            fps: &mi.fps,
            vcodec: &mi.vcodec,
            acodec: &mi.acodec,
            tool,
            mode: "vod",
            platform,
            secs: started_at,
            ..Default::default()
        },
    )
}
/// Inputs to [`expand_template`]. Each field maps to a `{…}` variable; empty
/// fields render empty. `title`/`channel`/`video_id` are resolved metadata (empty
/// for live recordings or id-less methods); `resolution`/`height`/`width`/`fps`/
/// `vcodec` are actual media info (filled only when probing is enabled).
#[derive(Default)]
pub struct TemplateVars<'a> {
    pub name: &'a str,
    pub title: &'a str,
    pub channel: &'a str,
    pub video_id: &'a str,
    /// Configured quality selector (e.g. `1080p60`, `best`).
    pub quality: &'a str,
    pub resolution: &'a str,
    pub height: &'a str,
    pub width: &'a str,
    pub fps: &'a str,
    pub vcodec: &'a str,
    /// Audio codec, e.g. "aac", "opus" (empty when not probed or unknown).
    pub acodec: &'a str,
    /// Attempt number (per-monitor take count); empty for on-demand videos.
    pub take: &'a str,
    /// Distinct game/category names played during the recording, joined + length-
    /// capped. Only known after the stream ends, so it's filled at the post-rename
    /// (empty for the initial capture name and for on-demand videos).
    pub games: &'a str,
    /// Capture tool: "streamlink", "yt-dlp", "ffmpeg" (empty renders empty).
    pub tool: &'a str,
    /// Download mode: "live", "sabr", "dash", "hybrid", "hybrid-dash", "direct", "vod", "chat".
    pub mode: &'a str,
    /// Stream platform: "twitch", "youtube", "kick", "generic" (empty renders empty).
    pub platform: &'a str,
    /// Capture-start time (unix secs) for `{date}`/`{time}`/`{timestamp}`.
    pub secs: i64,
    /// When the broadcast went live (unix secs); 0 means unknown → {went_live_date}/{went_live_time} render empty.
    pub went_live: i64,
    /// Casing/override style for machine-value tokens (`{vcodec}` `{acodec}`
    /// `{platform}` `{platform_short}` `{tool}` `{mode}`). `None` = plain.
    pub style: Option<&'a TokenStyle>,
}

/// Preview a filename template with the given variable set. Extension not included.
/// Sanitizes and guarantees a non-empty result (falls back to `{name}_{date}_{time}`).
pub fn preview_filename(template: &str, vars: &TemplateVars<'_>) -> String {
    expand_template(template, vars)
}

/// Settings key: casing style for machine-value tokens (`{vcodec}` `{acodec}`
/// `{platform}` `{platform_short}` `{tool}` `{mode}`). `"branded"` = proper
/// trademark/spec casing (AAC, H.264, YouTube); anything else = as reported
/// (lowercase, today's behavior).
pub const K_TOKEN_STYLE: &str = "filename_token_style";
/// Settings key: user overrides for token values, one per line —
/// `value=Text` (any token) or `kind:value=Text` (one token kind, e.g.
/// `platform_short:youtube=YT2`). Overrides win over the branded map.
pub const K_TOKEN_OVERRIDES: &str = "filename_token_overrides";

/// How machine-value template tokens are rendered. [`Default`] = as-reported
/// lowercase with no overrides (exactly the pre-feature output).
#[derive(Clone, Default)]
pub struct TokenStyle {
    pub branded: bool,
    /// `(kind, lowercase value, replacement)`; empty kind = any token kind.
    pub overrides: Vec<(String, String, String)>,
}

/// Parse the overrides setting: one `value=Text` / `kind:value=Text` per line;
/// blank lines and lines without `=` are ignored.
pub fn parse_token_overrides(raw: &str) -> Vec<(String, String, String)> {
    raw.lines()
        .filter_map(|line| {
            let (key, text) = line.split_once('=')?;
            let (key, text) = (key.trim(), text.trim());
            if key.is_empty() || text.is_empty() {
                return None;
            }
            let (kind, value) = match key.split_once(':') {
                Some((k, v)) => (k.trim().to_lowercase(), v.trim().to_lowercase()),
                None => (String::new(), key.to_lowercase()),
            };
            Some((kind, value, text.to_string()))
        })
        .collect()
}

/// Load the configured [`TokenStyle`] from settings.
pub fn load_token_style(store: &Store) -> TokenStyle {
    TokenStyle {
        branded: store.get_setting(K_TOKEN_STYLE).ok().flatten().as_deref() == Some("branded"),
        overrides: parse_token_overrides(
            &store.get_setting(K_TOKEN_OVERRIDES).ok().flatten().unwrap_or_default(),
        ),
    }
}

/// Process-wide token style, read by every template expansion that doesn't
/// override it explicitly (`TemplateVars::style`). Set at startup and on every
/// settings save — passing it through the dozens of plan/stem/preview call
/// chains would be pure plumbing. Tests never touch it (they pass an explicit
/// style), so the default-plain global keeps them deterministic.
static GLOBAL_TOKEN_STYLE: std::sync::OnceLock<std::sync::RwLock<TokenStyle>> =
    std::sync::OnceLock::new();

fn global_style_lock() -> &'static std::sync::RwLock<TokenStyle> {
    GLOBAL_TOKEN_STYLE.get_or_init(|| std::sync::RwLock::new(TokenStyle::default()))
}

/// Install the current settings' token style (startup + settings save).
pub fn set_global_token_style(style: TokenStyle) {
    *global_style_lock().write().unwrap() = style;
}

fn global_token_style() -> TokenStyle {
    global_style_lock().read().unwrap().clone()
}

/// Proper trademark/spec casing for a known machine value (`branded` style).
fn branded_value(kind: &str, value_lc: &str) -> Option<&'static str> {
    Some(match (kind, value_lc) {
        ("vcodec", "h264") => "H.264",
        ("vcodec", "h265") => "H.265",
        ("vcodec", "hevc") => "HEVC",
        ("vcodec", "av1") => "AV1",
        ("vcodec", "vp9") => "VP9",
        ("vcodec", "vp8") => "VP8",
        ("acodec", "aac") => "AAC",
        ("acodec", "opus") => "Opus",
        ("acodec", "mp3") => "MP3",
        ("acodec", "mp4a") => "MP4A",
        ("acodec", "vorbis") => "Vorbis",
        ("acodec", "flac") => "FLAC",
        ("acodec", "ac3") => "AC3",
        ("acodec", "eac3") => "EAC3",
        ("platform", "twitch") => "Twitch",
        ("platform", "youtube") => "YouTube",
        ("platform", "kick") => "Kick",
        ("platform", "nrk") => "NRK",
        ("platform", "nebula") => "Nebula",
        ("platform", "generic") => "Generic",
        ("platform_short", "twitch") => "TTV",
        ("platform_short", "youtube") => "YT",
        ("platform_short", "kick") => "Kick",
        ("platform_short", "nrk") => "NRK",
        ("platform_short", "nebula") => "Neb",
        ("platform_short", "generic") => "Gen",
        // yt-dlp's brand IS lowercase — only the others gain a capital.
        ("tool", "streamlink") => "Streamlink",
        ("tool", "ffmpeg") => "FFmpeg",
        ("mode", "live") => "Live",
        ("mode", "sabr") => "SABR",
        ("mode", "dash") => "DASH",
        ("mode", "hybrid") => "Hybrid",
        ("mode", "hybrid-dash") => "Hybrid-DASH",
        ("mode", "direct") => "Direct",
        ("mode", "vod") => "VOD",
        ("mode", "chat") => "Chat",
        _ => return None,
    })
}

/// Lowercase short form of a platform value (`{platform_short}`); unknown
/// platforms fall back to the full value.
fn short_platform(platform_lc: &str) -> &str {
    match platform_lc {
        "twitch" => "ttv",
        "youtube" => "yt",
        "kick" => "kick",
        "nrk" => "nrk",
        "nebula" => "neb",
        "generic" => "gen",
        other => other,
    }
}

/// Render one machine-value token: user override → branded map (when on) →
/// the raw value. `platform_short` shortens first, then styles.
fn styled_token(kind: &str, raw: &str, st: &TokenStyle) -> String {
    let value_lc = raw.to_lowercase();
    if let Some((.., text)) = st
        .overrides
        .iter()
        .find(|(k, v, _)| (k.is_empty() || k == kind) && *v == value_lc)
    {
        return text.clone();
    }
    if st.branded && let Some(b) = branded_value(kind, &value_lc) {
        return b.to_string();
    }
    if kind == "platform_short" {
        return short_platform(&value_lc).to_string();
    }
    raw.to_string()
}

/// Expand a filename template using our own (tool-agnostic) variables so the
/// output path is known in advance: `{name} {title} {channel} {video_id}
/// {quality} {resolution} {height} {width} {fps} {vcodec} {take} {games} {date}
/// {time} {timestamp}`.
pub(super) fn expand_template(template: &str, v: &TemplateVars) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix_utc(v.secs);
    let date = format!("{y:04}{mo:02}{d:02}");
    let time = format!("{h:02}{mi:02}{s:02}");
    let (wl_date, wl_time) = if v.went_live > 0 {
        let (wy, wmo, wd, wh, wmi, ws) = civil_from_unix_utc(v.went_live);
        (format!("{wy:04}{wmo:02}{wd:02}"), format!("{wh:02}{wmi:02}{ws:02}"))
    } else {
        (String::new(), String::new())
    };
    let tmpl = if template.trim().is_empty() {
        "{name}_{date}_{time}"
    } else {
        template
    };
    let global;
    let style = match v.style {
        Some(s) => s,
        None => {
            global = global_token_style();
            &global
        }
    };
    let expanded = tmpl
        .replace("{name}", v.name)
        .replace("{title_trimmed}", &trim_title_commands(v.title))
        .replace("{title}", v.title)
        .replace("{channel}", v.channel)
        .replace("{video_id}", v.video_id)
        .replace("{quality}", v.quality)
        .replace("{resolution}", v.resolution)
        .replace("{height}", v.height)
        .replace("{width}", v.width)
        .replace("{fps}", v.fps)
        .replace("{vcodec}", &styled_token("vcodec", v.vcodec, style))
        .replace("{acodec}", &styled_token("acodec", v.acodec, style))
        .replace("{tool}", &styled_token("tool", v.tool, style))
        .replace("{mode}", &styled_token("mode", v.mode, style))
        .replace("{platform_short}", &styled_token("platform_short", v.platform, style))
        .replace("{platform}", &styled_token("platform", v.platform, style))
        .replace("{take}", v.take)
        .replace("{games}", v.games)
        .replace("{date}", &date)
        .replace("{time}", &time)
        .replace("{timestamp}", &v.secs.to_string())
        .replace("{year}", &format!("{y:04}"))
        .replace("{month}", &format!("{mo:02}"))
        .replace("{day}", &format!("{d:02}"))
        .replace("{hour}", &format!("{h:02}"))
        .replace("{minute}", &format!("{mi:02}"))
        .replace("{second}", &format!("{s:02}"))
        .replace("{went_live_date}", &wl_date)
        .replace("{went_live_time}", &wl_time)
        // backward-compat aliases for tokens listed in the old tooltip
        .replace("{id}", v.video_id)
        .replace("{ts}", &v.secs.to_string())
        .replace("{category}", v.games);
    let cleaned = sanitize_filename(&expanded);
    // Bound the stem so it plus any companion suffix (the longest is
    // `.live_chat.json`) plus a collision suffix never exceeds NTFS's
    // per-component limit — see `MAX_STEM_UTF16_LEN`. Re-sanitize the trailing
    // edge: a cut can land on a space/period, which `sanitize_filename`'s trim
    // already handled for the untruncated end but not for a newly-created one.
    let cleaned = if cleaned.encode_utf16().count() > MAX_STEM_UTF16_LEN {
        truncate_utf16(&cleaned, MAX_STEM_UTF16_LEN)
            .trim_end_matches([' ', '.'])
            .to_string()
    } else {
        cleaned
    };
    if cleaned.is_empty() {
        format!("{}_{date}_{time}", sanitize_filename(v.name))
    } else {
        cleaned
    }
}

/// A stem (filename without extension) that doesn't collide with an existing
/// `<stem>.<ext>` in `dir`: returns `stem`, else `stem (2)`, `stem (3)`, … —
/// matching the file-manager convention. A missing `dir` can't collide. `ignore`
/// (the file being renamed, if any) is treated as free so a post-rename to the
/// same/own name isn't pushed to a new suffix.
pub(crate) fn unique_stem(dir: &Path, stem: &str, ext: &str, ignore: Option<&Path>) -> String {
    let taken = |s: &str| {
        let p = dir.join(format!("{s}.{ext}"));
        Some(p.as_path()) != ignore && crate::iomon::fs::exists_sync(Cat::FsProbe, &p)
    };
    if !taken(stem) {
        return stem.to_string();
    }
    for n in 2..10_000 {
        let cand = format!("{stem} ({n})");
        if !taken(&cand) {
            return cand;
        }
    }
    // Pathological fallback (10k same-named files): stamp it so we never clobber.
    format!("{stem} ({})", now_unix())
}

/// Separator characters that delimit title segments and command clusters
/// (`A | !gg !tts`, `!WCC | !impulse | …`, `on !gamemasters - !schedule`).
/// Deliberately EXCLUDES `+` (part of `18+`) and `:` (subtitles).
const TITLE_SEP_CHARS: &[char] =
    &['|', '‖', '—', '–', '-', '~', '•', '·', '>', '/', ',', ';', '&', '→'];

/// Collapse whitespace runs to single spaces and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `{title_trimmed}`: the stream title with Twitch chat-command plugs
/// (`!gg !stoneforged !tangia`, `||🩸!youtube 💬!discord`) and `#ad` /
/// `#sponsored` disclosure tags removed, plus the separators/emoji they leave
/// orphaned. Rules were derived from a 630-title corpus (2026-07-22):
///
/// - A command is `!` at a non-alphanumeric boundary followed by ≥ 2
///   `[A-Za-z0-9_]` — so trailing exclamations (`YAHOO!!!!`, `Karaoke !`)
///   survive, while `!gg`, `!24`, `!FF` and emoji-glued `🩸!youtube` don't.
/// - Cleanup runs ONLY when something was removed: a command-free title is
///   returned verbatim.
/// - Head/tail segments left without any alphanumeric content (`||🩸 💬`,
///   a lone `|` after `!gsupps !store |`) are dropped along with their
///   separator; interior text is never touched (`… | 18+` keeps its `+`,
///   `A --> B` keeps its arrow). Emptied bracket pairs (`(!rules)` → `()`)
///   are removed.
/// - A title that consists ONLY of commands falls back to the original.
pub(crate) fn trim_title_commands(title: &str) -> String {
    let chars: Vec<char> = title.chars().collect();
    let mut out = String::with_capacity(title.len());
    let mut removed = false;
    // The previous SIGNIFICANT char: the one before `i` in the ORIGINAL text,
    // except a removed token counts as its `!`/`#` — so glued runs like
    // `!gg!tts` still match after the first removal.
    let mut prev: Option<char> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '!' && !prev.map(char::is_alphanumeric).unwrap_or(false) {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j - i >= 3 {
                removed = true;
                prev = Some('!');
                i = j;
                continue;
            }
        }
        if c == '#'
            && let Some(tag_len) = ["ad", "sponsored"].iter().find_map(|tag| {
                let end = i + 1 + tag.len();
                (chars.len() >= end
                    && chars[i + 1..end].iter().collect::<String>().eq_ignore_ascii_case(tag)
                    && chars.get(end).map(|c| !c.is_ascii_alphanumeric()).unwrap_or(true))
                .then_some(tag.len())
            })
        {
            removed = true;
            prev = Some('#');
            i += 1 + tag_len;
            continue;
        }
        out.push(c);
        prev = Some(c);
        i += 1;
    }
    if !removed {
        return title.to_string();
    }

    let is_sep = |c: &char| TITLE_SEP_CHARS.contains(c);
    let mut t = collapse_ws(&out);
    for _ in 0..20 {
        // Emptied bracket pairs first — `(!rules)` became `()`.
        for pair in ["( )", "()", "[ ]", "[]", "{ }", "{}", "【】", "（）"] {
            t = t.replace(pair, "");
        }
        t = collapse_ws(&t);
        let before = t.clone();
        let mut cs: Vec<char> = t.chars().collect();
        // Tail: bare separator/whitespace runs, then a final segment with no
        // alphanumeric content at all (command-decoration emoji, `||🩸 💬`).
        while matches!(cs.last(), Some(c) if is_sep(c) || c.is_whitespace()) {
            cs.pop();
        }
        if let Some(idx) = cs.iter().rposition(is_sep)
            && idx > 0
            && !cs[idx + 1..].iter().any(|c| c.is_alphanumeric())
        {
            cs.truncate(idx);
        }
        // Head mirror: junk before the first separator.
        while matches!(cs.first(), Some(c) if is_sep(c) || c.is_whitespace()) {
            cs.remove(0);
        }
        if let Some(idx) = cs.iter().position(is_sep)
            && idx > 0
            && !cs[..idx].iter().any(|c| c.is_alphanumeric())
        {
            cs.drain(..=idx);
        }
        t = cs.into_iter().collect::<String>().trim().to_string();
        if t == before {
            break;
        }
    }
    if t.is_empty() { title.trim().to_string() } else { t }
}

pub(crate) fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Windows also forbids a path component ending in a space or a period
    // (e.g. a stream title like "Chatting late tonight." would otherwise
    // produce a filename Windows refuses with the same ERROR_INVALID_NAME as
    // the length overflow above) — trim repeatedly, since removing one can
    // reveal another underneath.
    cleaned.trim().trim_end_matches([' ', '.']).to_string()
}

/// Truncate `s` to at most `max_units` **UTF-16 code units** — what NTFS and
/// Win32 file APIs actually count for a path component's length (a character
/// outside the Basic Multilingual Plane, e.g. most emoji, is a surrogate pair
/// = 2 units) — without splitting a character. A plain byte- or
/// char-count-based truncation would systematically undercount any title/
/// category containing emoji and could still overflow the true NTFS limit.
pub(super) fn truncate_utf16(s: &str, max_units: usize) -> &str {
    let mut acc = 0usize;
    for (i, ch) in s.char_indices() {
        acc += ch.len_utf16();
        if acc > max_units {
            return &s[..i];
        }
    }
    s
}

/// OS-level "the filename itself is invalid/too long" errors — the reactive
/// backstop behind the proactive [`MAX_STEM_UTF16_LEN`] cap, for anything that
/// slips past it regardless (a filesystem with a tighter limit, an unusually
/// long companion suffix, `unique_stem`'s collision suffix tipping it over).
/// Distinguished from a transient problem (sharing violation, missing
/// directory, permissions) that retrying with a *different name* wouldn't fix:
/// - Windows: `ERROR_INVALID_NAME` (123 — NTFS's 255-unit-per-component limit,
///   or an illegal trailing `.`/` ` that slipped past sanitization) and
///   `ERROR_FILENAME_EXCED_RANGE` (206).
/// - Unix: `ENAMETOOLONG` (36 on Linux, 63 on macOS/BSD).
pub(super) fn is_name_too_long(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(123 | 206 | 36 | 63))
}

/// Replace the trailing `remove_units` **UTF-16 code units** of `s` with
/// `"..."` — a visible marker that the OS rejected the full name and it was
/// shortened to fit — without splitting a character (e.g. an emoji surrogate
/// pair). `remove_units` is how much to strip *before* the 3-unit marker is
/// appended, not a target length — see [`stem_fitting_budget`], which does
/// that arithmetic.
pub(super) fn ellipsize_utf16(s: &str, remove_units: usize) -> String {
    let total = s.encode_utf16().count();
    if remove_units >= total {
        return "...".to_string();
    }
    format!("{}...", truncate_utf16(s, total - remove_units))
}

/// Shorten `stem` (marking the cut with `"..."`, see [`ellipsize_utf16`]) so
/// its UTF-16 length is at most `budget` — a no-op if it already fits.
/// Deterministic: the same `(stem, budget)` always produces the same result.
pub(super) fn stem_fitting_budget(stem: &str, budget: usize) -> String {
    let stem_units = stem.encode_utf16().count();
    if stem_units <= budget {
        return stem.to_string();
    }
    // ellipsize_utf16 removes `remove_units` THEN appends "..." (3 units), so
    // ask for exactly enough removal that the total after appending lands at
    // `budget`, not `budget - 3`.
    ellipsize_utf16(stem, stem_units + 3 - budget)
}

/// Stem-length budgets tried when a name overflows, deterministic and
/// **independent of which specific suffix** (`.mkv`, `.chat.jsonl`,
/// `.en.vtt`, …) triggered the retry. That independence is the whole point:
/// a take's video and every one of its companions each call
/// [`rename_or_shorten`] starting from the SAME original stem, so whichever
/// budget rung first makes it fit is the SAME rung for all of them — they
/// converge on one identical shortened stem instead of each laddering to a
/// length sized only for its own suffix (which would desync sibling files
/// that must share a prefix — the original design's mistake). The first
/// budget is [`MAX_STEM_UTF16_LEN`], the proactive cap that already leaves
/// room for the longest companion suffix, so it succeeds on the first try in
/// the overwhelming majority of real overflows; the smaller ones are a
/// backstop for a total-path-length problem the per-component math alone
/// can't see (e.g. a deeply nested output directory).
pub(super) const STEM_SHORTEN_BUDGETS: [usize; 3] = [MAX_STEM_UTF16_LEN, 120, 40];

/// Attempt `tokio::fs::rename(from, dir.join(format!("{stem}.{suffix}")))`.
/// If the OS rejects the destination name (see [`is_name_too_long`]) —
/// overwhelmingly NTFS's 255-UTF-16-unit-per-component limit — deterministically
/// shorten `stem` (see [`stem_fitting_budget`]/[`STEM_SHORTEN_BUDGETS`]) and
/// retry. `suffix` (e.g. `"mkv"`, `"chat.jsonl"`, `"en.vtt"`) is never
/// touched — it's what identifies the file's role, and companion-matching
/// logic elsewhere depends on it staying intact.
///
/// A recording actually landing on disk matters more than a fully-descriptive
/// name, so this is the *last-resort* backstop behind the proactive stem cap
/// in `expand_template` (`MAX_STEM_UTF16_LEN`), which should make this rare in
/// practice — it exists for whatever that cap didn't anticipate.
///
/// Returns the path the file actually ended up at (`dir/{stem}.{suffix}`, or
/// a shortened sibling), or the original error if even the shortest attempt
/// still fails for an unrelated reason (e.g. a missing directory), or if a
/// shortened candidate would collide with an unrelated existing file (rather
/// than risk clobbering it, or a numeric disambiguator that would desync this
/// file from siblings independently computing the same candidate).
pub(super) async fn rename_or_shorten(
    from: &Path,
    dir: &Path,
    stem: &str,
    suffix: &str,
) -> std::io::Result<PathBuf> {
    let to = dir.join(format!("{stem}.{suffix}"));
    match crate::iomon::fs::rename(Cat::Promote, from, &to).await {
        Ok(()) => Ok(to),
        Err(e) if !is_name_too_long(&e) => Err(e),
        Err(first_err) => {
            for budget in STEM_SHORTEN_BUDGETS {
                let short_stem = stem_fitting_budget(stem, budget);
                if short_stem == stem {
                    continue; // this budget doesn't shorten it any further
                }
                let candidate = dir.join(format!("{short_stem}.{suffix}"));
                if crate::iomon::fs::exists_sync(Cat::Promote, &candidate) {
                    // Don't clobber it, and don't disambiguate with a numeric
                    // suffix here — that would itself desync this file from
                    // siblings that independently compute the same candidate.
                    return Err(first_err);
                }
                match crate::iomon::fs::rename(Cat::Promote, from, &candidate).await {
                    Ok(()) => {
                        warn!(
                            "shortened an over-long filename to fit the filesystem: {} -> {}",
                            to.display(),
                            candidate.display()
                        );
                        return Ok(candidate);
                    }
                    Err(e) if is_name_too_long(&e) => continue, // still too long, shrink further
                    Err(e) => return Err(e),
                }
            }
            Err(first_err)
        }
    }
}

/// Proactively shorten `path`'s file stem (see [`stem_fitting_budget`]) if
/// it's long enough to risk NTFS's per-component limit, returning the
/// original path unchanged if it's already safe. Unlike [`rename_or_shorten`],
/// this never touches the filesystem — it's for callers that WRITE to their
/// destination directly (ffmpeg-based remux) rather than renaming an existing
/// file, so there's no OS error to react to; the only option is to make sure
/// the destination is safe *before* the write is attempted.
pub(super) fn path_with_safe_stem(path: &Path) -> PathBuf {
    let (Some(dir), Some(stem)) = (
        path.parent(),
        path.file_stem().map(|s| s.to_string_lossy().into_owned()),
    ) else {
        return path.to_path_buf();
    };
    let safe_stem = stem_fitting_budget(&stem, MAX_STEM_UTF16_LEN);
    if safe_stem == stem {
        return path.to_path_buf();
    }
    match path.extension().map(|e| e.to_string_lossy().into_owned()) {
        Some(ext) if !ext.is_empty() => dir.join(format!("{safe_stem}.{ext}")),
        _ => dir.join(safe_stem),
    }
}

/// Convert a unix timestamp to a UTC civil date/time (Howard Hinnant's algorithm).
pub(super) fn civil_from_unix_utc(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hh, mm, ss)
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
    fn unique_stem_avoids_existing_files() {
        let dir = std::env::temp_dir()
            .join(format!("sa_unique_{}_{}", std::process::id(), now_unix()));
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing there yet -> stem unchanged.
        assert_eq!(unique_stem(&dir, "Layna", "mkv", None), "Layna");
        std::fs::write(dir.join("Layna.mkv"), b"x").unwrap();
        assert_eq!(unique_stem(&dir, "Layna", "mkv", None), "Layna (2)");
        std::fs::write(dir.join("Layna (2).mkv"), b"x").unwrap();
        assert_eq!(unique_stem(&dir, "Layna", "mkv", None), "Layna (3)");
        // A different extension doesn't collide.
        assert_eq!(unique_stem(&dir, "Layna", "ts", None), "Layna");
        // A missing directory can't collide.
        assert_eq!(unique_stem(&dir.join("nope"), "Layna", "mkv", None), "Layna");
        // The file being renamed is ignored, so its own name is treated as free
        // (the post-rename no-op case): "Layna (2).mkv" exists but is `ignore`.
        let own = dir.join("Layna (2).mkv");
        assert_eq!(unique_stem(&dir, "Layna", "mkv", Some(&own)), "Layna (2)");

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn is_name_too_long_matches_known_codes_only() {
        let too_long = |code: i32| std::io::Error::from_raw_os_error(code);
        assert!(is_name_too_long(&too_long(123))); // Windows ERROR_INVALID_NAME
        assert!(is_name_too_long(&too_long(206))); // Windows ERROR_FILENAME_EXCED_RANGE
        assert!(is_name_too_long(&too_long(36))); // Linux ENAMETOOLONG
        assert!(is_name_too_long(&too_long(63))); // macOS/BSD ENAMETOOLONG
        // A transient/unrelated error must NOT trigger the shortening path —
        // retrying with a different name wouldn't fix a sharing violation,
        // a missing directory, or a permissions problem.
        assert!(!is_name_too_long(&too_long(32))); // ERROR_SHARING_VIOLATION
        assert!(!is_name_too_long(&too_long(5))); // ERROR_ACCESS_DENIED
        assert!(!is_name_too_long(&std::io::Error::new(std::io::ErrorKind::Other, "x")));
    }

    #[test]
    fn ellipsize_utf16_marks_truncation_without_splitting_chars() {
        assert_eq!(ellipsize_utf16("abcdef", 3), "abc...");
        // Removing more than the whole string still yields a valid marker.
        assert_eq!(ellipsize_utf16("ab", 50), "...");
        // A surrogate pair ('🥹', 2 units) must never be split: removing
        // exactly 2 units lands right at its boundary (kept whole); removing
        // 3 would split it mid-pair, so the whole emoji is dropped instead of
        // producing a corrupt lone surrogate (constructing the `String` at
        // all is proof the result stayed valid UTF-8).
        let s = "ab🥹cd"; // a(1) b(1) 🥹(2) c(1) d(1) = 6 units total
        assert_eq!(ellipsize_utf16(s, 2), "ab🥹...");
        assert_eq!(ellipsize_utf16(s, 3), "ab...");
    }

    #[tokio::test]
    async fn rename_or_shorten_falls_back_on_overflow_and_preserves_content() {
        let dir = std::env::temp_dir()
            .join(format!("sa_shorten_{}_{}", std::process::id(), now_unix()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let src = dir.join("source.tmp");
        tokio::fs::write(&src, b"payload").await.unwrap();

        // Deliberately overflow NTFS's 255-UTF-16-unit-per-component limit:
        // 300 'x's + ".mkv" (4) = 304 units.
        let huge_stem = "x".repeat(300);
        let result = rename_or_shorten(&src, &dir, &huge_stem, "mkv").await;

        let actual = result.expect("must fall back to a shortened name, not fail outright");
        assert!(actual.is_file(), "the file must actually exist at the returned path");
        assert_eq!(tokio::fs::read(&actual).await.unwrap(), b"payload");
        assert!(!src.exists(), "the source must be gone (this was a rename, not a copy)");

        let name = actual.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.contains("..."), "shortened name must visibly mark the cut: {name}");
        assert!(name.ends_with(".mkv"), "the suffix must never be touched: {name}");
        assert!(
            name.encode_utf16().count() <= NTFS_MAX_COMPONENT_UTF16,
            "shortened name must actually fit: {} units",
            name.encode_utf16().count()
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn rename_or_shorten_passes_through_unrelated_errors() {
        // A source that doesn't exist fails with NotFound, not a
        // name-too-long condition — must propagate as-is, no retry loop.
        let dir = std::env::temp_dir()
            .join(format!("sa_shorten_missing_{}_{}", std::process::id(), now_unix()));
        let missing_src = dir.join("does_not_exist.tmp");
        let err = rename_or_shorten(&missing_src, &dir, "stem", "mkv")
            .await
            .expect_err("a missing source must fail, not silently succeed");
        assert!(!is_name_too_long(&err));
    }

    #[test]
    fn path_with_safe_stem_is_a_noop_when_already_safe() {
        let short = std::path::Path::new(r"C:\rec\short.mkv");
        assert_eq!(path_with_safe_stem(short), short);
    }

    #[test]
    fn path_with_safe_stem_shortens_an_overlong_stem_proactively() {
        // Used to pre-shorten a ffmpeg REMUX destination before the write is
        // even attempted (ffmpeg writes directly — there's no OS rename error
        // to react to afterward the way rename_or_shorten reacts to one).
        let long_stem = "z".repeat(260);
        let path = std::path::PathBuf::from(format!(r"C:\rec\{long_stem}.mkv"));
        let safe = path_with_safe_stem(&path);
        assert_ne!(safe, path);
        assert_eq!(safe.parent(), path.parent());
        assert_eq!(safe.extension().unwrap(), "mkv");
        let name = safe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.contains("..."), "must mark the cut: {name}");
        assert!(name.encode_utf16().count() <= NTFS_MAX_COMPONENT_UTF16);
    }
    #[tokio::test]
    async fn shortened_stem_is_deterministic_across_different_suffix_lengths() {
        // The exact regression an adversarial review caught in an earlier
        // version of this fix: shortening must be a PURE function of the
        // stem alone, independent of which suffix triggered it — otherwise
        // the video (short ".mkv" suffix) and its companions (longer
        // ".chat.jsonl"/".live_chat.json" suffixes) can each independently
        // choose a DIFFERENT shortened stem and end up mismatched, which is
        // exactly the "sidecar orphaned under a name that no longer matches
        // the video" failure this whole feature exists to prevent.
        let dir = std::env::temp_dir()
            .join(format!("sa_shorten_converge_{}_{}", std::process::id(), now_unix()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Long enough that EVERY suffix below needs shortening (so we're
        // actually exercising the fallback for all of them, not just the
        // longest), independent of what triggered it first — even combined
        // with the shortest suffix here ("mkv", 4 units incl. the dot),
        // 260 + 4 = 264 > 255.
        let stem = "y".repeat(260);

        let mut shortened_stems = Vec::new();
        for suffix in ["mkv", "en.vtt", "chat.jsonl", "live_chat.json"] {
            let src = dir.join(format!("src_{suffix}.tmp"));
            tokio::fs::write(&src, b"x").await.unwrap();
            let actual = rename_or_shorten(&src, &dir, &stem, suffix)
                .await
                .unwrap_or_else(|e| panic!("rename for suffix {suffix} must succeed: {e:#}"));
            let actual_stem = actual.file_stem().unwrap().to_string_lossy().into_owned();
            // .en.vtt / .chat.jsonl further split on their own internal dots
            // via Path::file_stem() (it only strips the LAST component), so
            // recover the true shared stem by stripping the known suffix
            // from the full file name instead.
            let full_name = actual.file_name().unwrap().to_string_lossy().into_owned();
            let true_stem = full_name.strip_suffix(&format!(".{suffix}")).unwrap_or(&actual_stem).to_string();
            shortened_stems.push((suffix, true_stem));
        }

        let first = &shortened_stems[0].1;
        for (suffix, s) in &shortened_stems {
            assert_eq!(
                s, first,
                "suffix {suffix} converged on a DIFFERENT stem than {}: {s} vs {first}",
                shortened_stems[0].0
            );
        }
        assert!(first.contains("..."), "convergence check is only meaningful if shortening actually happened");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn fmt_fps_rounds() {
        assert_eq!(fmt_fps("60/1"), "60");
        assert_eq!(fmt_fps("30000/1001"), "30"); // 29.97 -> 30
        assert_eq!(fmt_fps("60000/1001"), "60"); // 59.94 -> 60
        assert_eq!(fmt_fps("50"), "50");
        assert_eq!(fmt_fps("0/0"), "");
        assert_eq!(fmt_fps("N/A"), "");
    }

    #[test]
    fn template_wants_media_detects_vars() {
        assert!(template_wants_media("{name}_{resolution}"));
        assert!(template_wants_media("{fps}"));
        assert!(template_wants_media("{vcodec}-{height}"));
        assert!(!template_wants_media("{name}_{date}_{quality}"));
        assert!(!template_wants_media("{name}_{video_id}"));
    }

    #[test]
    fn template_expands_games() {
        let out = expand_template(
            "{name}_{games}",
            &TemplateVars {
                name: "Layna",
                games: "Just Chatting, Valorant",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out, "Layna_Just Chatting, Valorant");
        assert!(template_wants_games("{name}_{games}"));
        assert!(!template_wants_games("{name}_{date}"));
    }

    #[test]
    fn format_games_dedups_orders_and_truncates() {
        // Case-insensitive dedup, blanks skipped, order of first appearance kept.
        let cats = vec![
            "Just Chatting".to_string(),
            "Valorant".to_string(),
            "just chatting".to_string(),
            String::new(),
            " Valorant ".to_string(),
        ];
        assert_eq!(format_games(&cats), "Just Chatting, Valorant");
        // Capped to GAMES_MAX_LEN characters.
        let many: Vec<String> = (0..50).map(|i| format!("Game{i}")).collect();
        assert!(format_games(&many).chars().count() <= GAMES_MAX_LEN);
        assert_eq!(format_games(&[]), "");
    }

    #[test]
    fn civil_date_known_value() {
        // 1700000000 = 2023-11-14 22:13:20 UTC
        assert_eq!(
            civil_from_unix_utc(1_700_000_000),
            (2023, 11, 14, 22, 13, 20)
        );
    }

    #[test]
    fn template_expands_and_sanitizes() {
        let name = expand_template(
            "{name}_{date}",
            &TemplateVars {
                name: "Bad/Name?",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(name, "Bad_Name__20231114");
    }

    #[test]
    fn trim_title_commands_on_real_corpus_shapes() {
        // All inputs below are REAL titles from the 2026-07-22 DB corpus.
        let cases = [
            // Trailing pipe + command cluster (the most common shape).
            (
                "COWGIRL NUMI DEBUT!! SUBATHON TIME! [DAY 2] | !gg !stoneforged !tangia",
                "COWGIRL NUMI DEBUT!! SUBATHON TIME! [DAY 2]",
            ),
            // Pipe-separated command list, and `18+` must keep its plus.
            (
                "🎮 Collab Time! 🎨 Body Painter Arc - No One Will Spot Me | 18+ | !linktree | !discord | !patreon | !merch | !youtooz",
                "🎮 Collab Time! 🎨 Body Painter Arc - No One Will Spot Me | 18+",
            ),
            // Emoji glued to commands + `||` — orphaned decorations go too.
            (
                "My OBS Crashed :) Guess IM cooked  ||🩸!youtube 💬!discord !gamersupps 🎮!games",
                "My OBS Crashed :) Guess IM cooked",
            ),
            // …but a legit emoji ENDING of the title itself survives.
            (
                "VIRGIN EXPERIENCE: THE AMAZING DIGITAL CIRCUS🤡🎪 ||🩸!youtube 💬!discord",
                "VIRGIN EXPERIENCE: THE AMAZING DIGITAL CIRCUS🤡🎪",
            ),
            // Leading command block: the head junk goes, the title stays.
            ("!gsupps !store | Hanging Out :) | Gears 4 Later", "Hanging Out :) | Gears 4 Later"),
            // A bracket pair emptied by the removal disappears.
            (
                "What happens if Twitch Chat runs 4 countries at once? (!rules)",
                "What happens if Twitch Chat runs 4 countries at once?",
            ),
            // #ad / #sponsored disclosure tags are junk too.
            ("Checking out Altered Alma at 11:30! !alma #ad", "Checking out Altered Alma at 11:30!"),
            // Digit commands (`!24`) and mid-title `> ` shapes.
            (
                "🚨[HUGE EVENT]🚨12 HOUR PT.2 [Card Opening> UNHINGED] ||🩸!youtube 💬!discord 🎮!games !tcg #ad",
                "🚨[HUGE EVENT]🚨12 HOUR PT.2 [Card Opening> UNHINGED]",
            ),
            // Command as the object of a dash-separated tail.
            (
                "Octopath Travveler speedrun ft. @MochaJones10 on !gamemasters - !schedule",
                "Octopath Travveler speedrun ft. @MochaJones10 on",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(trim_title_commands(input), want, "input: {input}");
        }

        // False-positive guards: exclamations are not commands; a command-free
        // title comes back VERBATIM (no cleanup pass at all).
        for verbatim in [
            "CHIBI PLAYS MARIO YAHOO!!!!!!!",
            "Beach Day ~ 🏖️ More Karaoke !",
            "AoE2 climb to 2k all week!  //  Mostly Walking Mon",
            "Hamtaro Ham-Hams Unite!",
            "【Karaoke】Let me sing for you!!!",
        ] {
            assert_eq!(trim_title_commands(verbatim), verbatim);
        }

        // A title that is ONLY commands falls back to the original.
        assert_eq!(trim_title_commands("!gg !tts"), "!gg !tts");
    }

    #[test]
    fn token_style_brands_shortens_and_overrides() {
        // Plain (default) — exactly the pre-feature output, plus the new
        // {platform_short} in lowercase.
        let plain = expand_template(
            "{platform} {platform_short} {vcodec} {acodec} {mode} {tool}",
            &TemplateVars {
                platform: "youtube",
                vcodec: "h264",
                acodec: "aac",
                mode: "sabr",
                tool: "streamlink",
                ..Default::default()
            },
        );
        assert_eq!(plain, "youtube yt h264 aac sabr streamlink");

        // Branded: trademark/spec casing; yt-dlp stays lowercase (its brand).
        let branded = TokenStyle { branded: true, overrides: Vec::new() };
        let out = expand_template(
            "{platform} {platform_short} {vcodec} {acodec} {mode} {tool}",
            &TemplateVars {
                platform: "twitch",
                vcodec: "h264",
                acodec: "aac",
                mode: "vod",
                tool: "yt-dlp",
                style: Some(&branded),
                ..Default::default()
            },
        );
        assert_eq!(out, "Twitch TTV H.264 AAC VOD yt-dlp");

        // Overrides beat the branded map; kind-scoped keys hit one token only.
        let styled = TokenStyle {
            branded: true,
            overrides: parse_token_overrides(
                "h264=x264\nplatform_short:youtube=YT2\nignored line\n=empty\n",
            ),
        };
        let out = expand_template(
            "{platform} {platform_short} {vcodec}",
            &TemplateVars {
                platform: "youtube",
                vcodec: "H264", // matching is case-insensitive on the value
                style: Some(&styled),
                ..Default::default()
            },
        );
        assert_eq!(out, "YouTube YT2 x264");

        // Unknown values pass through untouched in every style.
        let out = expand_template(
            "{vcodec} {platform_short}",
            &TemplateVars {
                platform: "weirdtv",
                vcodec: "prores",
                style: Some(&branded),
                ..Default::default()
            },
        );
        assert_eq!(out, "prores weirdtv");
    }

    #[test]
    fn title_trimmed_token_expands_and_triggers_rename() {
        // The token flows through expand_template…
        let stem = expand_template(
            "{name} - {title_trimmed}",
            &TemplateVars {
                name: "Chan",
                title: "It's a collab day innit | !froot !frusic",
                ..Default::default()
            },
        );
        assert_eq!(stem, "Chan - It's a collab day innit");
        // …and counts as "wants title" so the post-capture rename fires and
        // the pre-rename stem gets the title-tba placeholder.
        assert!(template_wants_title("{name}_{title_trimmed}"));
        assert!(!template_wants_title("{name}_{date}"));
    }

    #[test]
    fn sanitize_filename_strips_trailing_dots_and_spaces() {
        // Windows forbids a path component ending in '.' or ' ' — a stream
        // title ending in a period (very plausible) must not silently produce
        // an unrenameable/uncreatable filename.
        assert_eq!(sanitize_filename("Chatting late tonight."), "Chatting late tonight");
        assert_eq!(sanitize_filename("Trailing space "), "Trailing space");
        // Repeated trailing dots/spaces are all stripped, not just one layer.
        assert_eq!(sanitize_filename("Ellipsis... "), "Ellipsis");
        // Illegal chars still map to '_' as before.
        assert_eq!(sanitize_filename("a:b"), "a_b");
    }

    #[test]
    fn truncate_utf16_counts_surrogate_pairs_not_chars() {
        // 'a' (1 unit) + '🥹' (U+1F979, outside the BMP -> 2 units).
        let s = "a🥹b";
        assert_eq!(truncate_utf16(s, 1), "a");
        // Cutting after the emoji's first unit must not split the surrogate
        // pair (and thus must not panic / produce invalid UTF-8) — the whole
        // emoji is dropped instead.
        assert_eq!(truncate_utf16(s, 2), "a");
        assert_eq!(truncate_utf16(s, 3), "a🥹");
        assert_eq!(truncate_utf16(s, 100), "a🥹b");
    }

    #[test]
    fn expand_template_caps_stem_for_the_longest_companion_suffix() {
        // Reproduces the 2026-07-04 CottontailVA incident: a long, emoji-laden
        // title plus several logged categories produced a stem that fit under
        // `.mkv` but overflowed NTFS's 255-UTF-16-unit limit once combined
        // with `.chat.jsonl` — the companion sidecar rename then failed with
        // os error 123 and was silently left behind under the old name.
        let categories: Vec<String> =
            ["Just Chatting", "Golf With Your Friends", "Super Battle Golf", "Left 4 Dead 2", "Overwatch"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let games = format_games(&categories);
        let title = "🥹 MOMMAS BEEN DEPRESSED BUT SHES BACK WITH MILKIES !!!💋_ !spotify !gg !soap";
        let out = expand_template(
            "{name} - {date} {time} - {title} [{games}] ({quality} {mode} {vcodec} {acodec}) - [{platform} {video_id}]",
            &TemplateVars {
                name: "CottontailVA",
                title,
                games: &games,
                quality: "1080p60",
                mode: "live",
                vcodec: "h264",
                acodec: "aac",
                platform: "twitch",
                video_id: "318342459223",
                secs: 1_751_663_194,
                ..Default::default()
            },
        );
        let out_units = out.encode_utf16().count();
        assert!(out_units <= MAX_STEM_UTF16_LEN, "stem itself must respect the cap: {out_units}");
        // The property that actually matters: EVERY companion suffix, applied
        // on top of this stem, must still fit under NTFS's per-component limit.
        for suffix in [".ts", ".mkv", ".chat.jsonl", ".live_chat.json", ".en.vtt", ".thumbnail.jpg"] {
            let full = format!("{out}{suffix}");
            let units = full.encode_utf16().count();
            assert!(
                units <= NTFS_MAX_COMPONENT_UTF16,
                "{suffix} pushes the filename to {units} UTF-16 units (limit {NTFS_MAX_COMPONENT_UTF16})"
            );
        }
        // Must not have been chopped mid-surrogate-pair (would be invalid
        // UTF-8 / panic on construction) and must not end in a bare '.'/' '.
        assert!(!out.ends_with('.') && !out.ends_with(' '));
    }

    #[test]
    fn expand_template_leaves_short_stems_untouched() {
        // Non-regression: ordinary templates/values must not be truncated at all.
        let out = expand_template(
            "{name}_{date}_{time}",
            &TemplateVars { name: "Layna", secs: 1_700_000_000, ..Default::default() },
        );
        assert_eq!(out, "Layna_20231114_221320");
    }

    #[test]
    fn template_expands_video_id_quality_take() {
        let out = expand_template(
            "{name}_{video_id}_{quality}_take{take}",
            &TemplateVars {
                name: "Stream",
                video_id: "abc123",
                quality: "1080p60",
                take: "3",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out, "Stream_abc123_1080p60_take3");
        // Empty id (id-less detection) leaves the slot blank.
        let out = expand_template(
            "{name}-{video_id}",
            &TemplateVars {
                name: "Stream",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out, "Stream-");
    }
    #[test]
    fn template_expands_title_and_channel() {
        let out = expand_template(
            "{title}_{date}",
            &TemplateVars {
                name: "ignored",
                title: "My Stream!",
                channel: "Streamer",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out, "My Stream!_20231114");
        // name / title / channel stay distinct in expand_template itself.
        let out2 = expand_template(
            "{channel}-{name}-{title}",
            &TemplateVars {
                name: "Nm",
                title: "Ttl",
                channel: "Chan",
                secs: 1_700_000_000,
                ..Default::default()
            },
        );
        assert_eq!(out2, "Chan-Nm-Ttl");
    }
    #[test]
    fn live_capture_stem_capped_for_child_path() {
        // Regression (2026-07-08): build_plan (live monitor capture) never
        // applied stem_capped_for_child_path, unlike build_video_plan — a long
        // title + a deep output dir could produce a `.cache\{stem}.ts` path
        // over Windows' MAX_PATH for the streamlink/yt-dlp child process,
        // which fails to open the output file (looks like ENOENT, not a
        // length-specific error).
        let mut r = row(Tool::Streamlink, Container::Mkv, Platform::Twitch);
        r.monitor.output_dir = r"A:\streams\ProjektMelody".into();
        r.monitor.filename_template =
            "{name} - {date} {time} - {title} [{games}] ({quality} {mode} {vcodec} {acodec}) - \
             [{platform} {video_id}]"
                .into();
        let long_title = "x".repeat(200);
        let plan = build_plan(
            &r,
            1_700_000_000,
            &AuthSource::None,
            &[],
            Some("31577148080"),
            &long_title,
            None,
            0,
            &YtDlpBins::default(),
        );
        let total = plan.capture_path.to_string_lossy().encode_utf16().count();
        assert!(
            total < 260,
            "capture path {total} WCHARs (over Windows MAX_PATH): {}",
            plan.capture_path.display()
        );
    }

    #[test]
    fn stem_capped_for_child_path_keeps_cache_path_under_budget() {
        // The 2026-07-06 incident: a 232-unit stem under A:\streams\Zentreya —
        // `.cache\{stem}.vod.mp4.ytdl` came to 263 WCHARs and killed yt-dlp.
        let dir = Path::new(r"A:\streams\Zentreya");
        let long_stem = format!("Zentreya - 2026-07-06 22-08-19 - {}.vod", "x".repeat(200));
        let capped = stem_capped_for_child_path(dir, &long_stem);
        let cache_units = cache_dir(dir).to_string_lossy().encode_utf16().count();
        let total = cache_units
            + 1
            + capped.encode_utf16().count()
            + LONGEST_CHILD_SUFFIX_UTF16
            + COLLISION_SUFFIX_RESERVE;
        assert!(total <= MAX_CHILD_PATH_UTF16, "total {total}");
        // A short stem is untouched.
        assert_eq!(stem_capped_for_child_path(dir, "short.vod"), "short.vod");
        // Deterministic.
        assert_eq!(capped, stem_capped_for_child_path(dir, &long_stem));
    }
}
