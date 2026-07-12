//! Chat popup: log parsing (Twitch/YouTube), segments/emotes, colors,
//! emoji handling.

use super::*;

/// Source platform of a captured chat message (drives username colouring).
#[derive(Clone)]
pub(super) enum ChatPlatform {
    YouTube,
    Twitch,
}

/// One renderable piece of a chat message body. Built once at parse time;
/// [`render_chat_message`] just walks it. `file: None` means "no local image"
/// (offline / not downloaded / undecodable / unknown id) → the renderer falls back
/// to drawing `fallback_text` (the emoji glyph) if set, else `name` (the emote
/// code). A `Text` segment may contain spaces.
#[derive(Clone)]
pub(super) enum ChatSegment {
    Text(String),
    Emote {
        name: String,
        file: Option<std::path::PathBuf>,
        /// Where a queued image download will land when `file` is `None` —
        /// [`upgrade_pending_emotes`] promotes it to `file` once it exists on
        /// disk, replacing the old "re-parse the whole file after the emoji
        /// download" upgrade pass.
        pending: Option<std::path::PathBuf>,
        /// For emoji: the Unicode glyph to show when there's no image, so it
        /// degrades to a (mono) glyph rather than a code. `None` for code emotes.
        fallback_text: Option<String>,
    },
}

/// A single parsed chat message (YouTube `.live_chat.json` or Twitch `.chat.jsonl`).
#[derive(Clone)]
pub(super) struct ChatMessage {
    /// Seconds from stream start (negative = chat arrived before we started recording).
    pub(super) timestamp_secs: f64,
    pub(super) author: String,
    /// Verbatim message body with emote codes left inline. KEPT (never replaced by
    /// rendered names) so the popup search filter still matches an emote by its
    /// code/shortcut even when it renders as an image.
    pub(super) text: String,
    /// Render plan: text runs interleaved with emote references. Always built; the
    /// "render emotes" toggle is applied at draw time, not here.
    pub(super) segments: Vec<ChatSegment>,
    /// Twitch: raw IRC badge segment per entry, e.g. `"subscriber/12"`.
    /// YouTube: badge tooltip text, e.g. `"Member"`.
    pub(super) badges: Vec<String>,
    /// Explicit hex colour from Twitch USERCOLOR; `None` when unset or YouTube.
    pub(super) color_override: Option<egui::Color32>,
    pub(super) platform: ChatPlatform,
}

/// Height estimate for a chat row that hasn't been drawn yet (≈ one line).
pub(super) const CHAT_ROW_EST: f32 = 20.0;

/// A loaded chat log plus the state the incremental loaders and the
/// virtualized renderer need. Lives behind `ChatPopup::load_state`'s mutex;
/// the UI renders straight from the guard (no per-frame clone) while the
/// background tasks append/prepend under the same lock.
pub(super) struct ChatLog {
    pub(super) messages: Vec<ChatMessage>,
    /// Measured row heights, parallel to `messages` (estimates until a row has
    /// actually been drawn once). Drives the virtualized scroll offsets.
    pub(super) row_heights: Vec<f32>,
    /// The width `row_heights` was measured at — a resize changes wrapping, so
    /// the cache resets to estimates.
    pub(super) measured_width: f32,
    /// Byte offset just past the last fully-parsed line of the chat file; the
    /// live tail reload resumes here instead of re-parsing the whole file.
    pub(super) parsed_to: u64,
    /// True while the pre-tail (older) part of the file is still parsing in
    /// the background — the newest messages are already shown.
    pub(super) loading_older: bool,
}

pub(super) enum ChatLoadState {
    Loading,
    Loaded(ChatLog),
    NoFile,
    Error(String),
}

pub(super) struct ChatPopup {
    /// Monitor this window belongs to — keys the viewport id, so each channel
    /// gets its OWN chat window (opening another channel's chat no longer
    /// replaces the one already open).
    pub(super) monitor_id: i64,
    pub(super) monitor_name: String,
    /// Currently-viewed recording (`None` = monitor has no recordings at all).
    pub(super) recording: Option<Recording>,
    pub(super) all_recordings: Vec<Recording>,
    pub(super) load_state: Arc<Mutex<ChatLoadState>>,
    pub(super) search: String,
    /// When `true`: show the entire log from the top (no cap, stick-to-bottom off).
    /// When `false` (default): show the last 500 msgs and stick to bottom.
    pub(super) full_view: bool,
    /// When the popup last triggered a background re-read of the chat file.
    /// Used to tail a live recording: the file is re-parsed every few seconds
    /// while `recording.ended_at` is `None`.
    pub(super) last_reload: std::time::Instant,
    /// Third-party emote code → resolved on-disk image path, built ONCE on
    /// popup-open from the channel's BTTV/FFZ/7TV manifests (case-sensitive keys).
    /// Empty for YouTube / when chat assets aren't fetched. `Arc` so the
    /// background (re)parse tasks share it without rebuilding per tick.
    pub(super) emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    /// `…/{channel}/twitch/emotes/twitch/` — Twitch first-party emotes are
    /// id-keyed (resolved as `{id}.png` at parse time). `None` for YouTube.
    pub(super) twitch_emote_dir: Option<std::path::PathBuf>,
    /// True while a background emoji-download pass is running, so the 3s tail-reload
    /// doesn't pile up overlapping download passes for the same chat.
    pub(super) loading: Arc<AtomicBool>,
    /// Consecutive tail-reloads spent in `ChatLoadState::Error`. An errored
    /// reload retries with a FULL re-read of the sidecar (potentially hundreds
    /// of MB on the recordings drive), so the retry interval backs off
    /// exponentially instead of re-reading every 3 seconds forever. Reset on a
    /// successful load.
    pub(super) error_retries: u32,
    /// Cached search-filter result: (lowercased query, message count when
    /// computed, matching message indices). Recomputed only when the query or
    /// the message count changes — the filter used to lowercase every message
    /// every frame.
    pub(super) filter_cache: Option<(String, usize, Vec<u32>)>,
}
/// The Twitch broadcaster's chosen chat name colour for `name`'s `account`, if
/// the asset fetch cached one (`…/{name}/twitch/{account}/name_color.txt`, e.g.
/// `#9146FF`; legacy pre-account dir as fallback). `None` when the streamer set
/// no colour or assets haven't been fetched.
pub(super) fn load_twitch_name_color(name: &str, account: &str) -> Option<egui::Color32> {
    for dir in crate::assets::asset_read_dirs(name, Platform::Twitch, account) {
        if let Ok(s) = crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, dir.join("name_color.txt")) {
            return parse_chat_hex_color(s.trim());
        }
    }
    None
}

/// Build a Twitch channel's third-party emote lookup: case-sensitive emote code →
/// resolved on-disk image path. Reads the per-channel BTTV/FFZ/7TV manifests once
/// (called on popup-open, not per message) and resolves each entry to its file in
/// the per-channel or shared-global cache, keeping only those that exist.
///
/// Precedence on a code defined by multiple providers: **7TV > BTTV > FFZ** — we
/// insert in that order with `or_insert`, so the first (highest-priority) provider
/// to define a code wins and later duplicates don't clobber it.
/// The Twitch `emotes/` dir for (channel, account): the account dir when it has
/// content, else the legacy pre-account per-platform dir (read fallback).
pub(super) fn twitch_emotes_dir(name: &str, account: &str) -> std::path::PathBuf {
    let [primary, legacy] = crate::assets::asset_read_dirs(name, Platform::Twitch, account);
    let primary = primary.join("emotes");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &primary) {
        return primary;
    }
    let legacy = legacy.join("emotes");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &legacy) { legacy } else { primary }
}

pub(super) fn build_emote_map(name: &str, account: &str) -> HashMap<String, std::path::PathBuf> {
    use crate::assets::EmoteManifestEntry;
    let emotes_dir = twitch_emotes_dir(name, account);
    let plat = crate::app_paths::platform_assets_dir();
    let mut map: HashMap<String, std::path::PathBuf> = HashMap::new();

    let load = |file: &str| -> Vec<EmoteManifestEntry> {
        crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, emotes_dir.join(file))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    };
    let mut insert = |entries: Vec<EmoteManifestEntry>, resolve: &dyn Fn(&EmoteManifestEntry) -> std::path::PathBuf| {
        for e in entries {
            // Skip empty/whitespace-only codes (old name-less manifests, or odd
            // provider data) — they could never match a chat token anyway.
            if e.name.trim().is_empty() {
                continue;
            }
            let path = resolve(&e);
            if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &path) {
                map.entry(e.name).or_insert(path);
            }
        }
    };

    // 7TV: always in the shared global cache, `{id}.webp`.
    insert(load("7tv.json"), &|e| {
        plat.join("7tv").join("emotes").join(format!("{}.{}", e.id, e.ext))
    });
    // BTTV: per-channel for channel emotes, shared global for shared emotes.
    let bttv_channel = emotes_dir.join("bttv");
    let bttv_shared = plat.join("bttv").join("emotes");
    insert(load("bttv.json"), &|e| {
        let base = if e.shared { &bttv_shared } else { &bttv_channel };
        base.join(format!("{}.{}", e.id, e.ext))
    });
    // FFZ: always in the shared global cache.
    insert(load("ffz.json"), &|e| {
        plat.join("ffz").join("emotes").join(format!("{}.{}", e.id, e.ext))
    });
    map
}
/// Truncate a label to at most `max` characters, appending `…` when shortened.
/// Char-aware so it never splits a multi-byte UTF-8 emote code mid-codepoint.
pub(super) fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Twitch `/me` actions arrive over IRC wrapped as `\x01ACTION <body>\x01` (CTCP).
/// The `emotes` tag's offsets index `<body>`, and the wrapper is protocol noise, so
/// it must be unwrapped before slicing / searching / display (otherwise emote
/// offsets are shifted and the raw control chars show in the replay). Returns the
/// inner body when both the prefix and the trailing `\x01` are present, else the
/// input unchanged. The raw `.chat.jsonl` keeps the wrapper for archival fidelity.
pub(super) fn strip_ctcp_action(text: &str) -> &str {
    text.strip_prefix("\u{1}ACTION ")
        .and_then(|s| s.strip_suffix('\u{1}'))
        .unwrap_or(text)
}

/// Slice `text` into [`ChatSegment`]s for a Twitch message: first-party emotes are
/// placed by the IRC `emotes` tag (id + inclusive **code-point** ranges), then any
/// remaining plain-text gaps are word-matched against the third-party `emote_map`.
///
/// Offsets are Unicode code points (Rust `char` index), not bytes and not UTF-16.
/// Every slice goes through `text.get(..)`, and any malformed/overlapping/out-of-
/// range tag aborts first-party substitution for the whole message (→ one Text
/// segment, still word-matched), so this never panics on hostile input.
pub(super) fn build_twitch_segments(
    text: &str,
    emotes_tag: &str,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
) -> Vec<ChatSegment> {
    let spans = parse_first_party_spans(text, emotes_tag);
    if spans.is_empty() {
        return word_match_segments(text, emote_map);
    }
    // Emit text gaps (word-matched) and first-party emote images in order.
    let mut out: Vec<ChatSegment> = Vec::new();
    let mut cursor = 0usize;
    for (b0, b1, id) in spans {
        if b0 > cursor {
            if let Some(gap) = text.get(cursor..b0) {
                out.extend(word_match_segments(gap, emote_map));
            }
        }
        let name = text.get(b0..b1).unwrap_or("").to_string();
        // First-party files are `{id}.png` (static) or `{id}.gif` (animated — we
        // render its first frame). Probe both so animated channel emotes show too.
        let file = twitch_dir.and_then(|d| find_emote_file(d, &id));
        out.push(ChatSegment::Emote { name, file, fallback_text: None, pending: None });
        cursor = b1;
    }
    if cursor < text.len() {
        if let Some(rest) = text.get(cursor..) {
            out.extend(word_match_segments(rest, emote_map));
        }
    }
    out
}

/// Parse the IRC `emotes` tag into a sorted list of `(byte_start, byte_end, id)`
/// spans over `text`, converting inclusive code-point offsets to validated byte
/// ranges in ONE walk. Returns empty when the tag is empty OR anything is
/// malformed/overlapping/out-of-range (caller then renders the text verbatim).
pub(super) fn parse_first_party_spans(text: &str, emotes_tag: &str) -> Vec<(usize, usize, String)> {
    if emotes_tag.is_empty() {
        return Vec::new();
    }
    // Collect (cp_start, cp_end_inclusive, id), dropping any malformed entry.
    let mut ranges: Vec<(usize, usize, String)> = Vec::new();
    for group in emotes_tag.split('/') {
        let Some((id, positions)) = group.split_once(':') else {
            return Vec::new();
        };
        if id.is_empty() {
            return Vec::new();
        }
        for pair in positions.split(',') {
            let Some((s, e)) = pair.split_once('-') else {
                return Vec::new();
            };
            let (Ok(s), Ok(e)) = (s.parse::<usize>(), e.parse::<usize>()) else {
                return Vec::new();
            };
            if e < s {
                return Vec::new();
            }
            ranges.push((s, e, id.to_string()));
        }
    }
    ranges.sort_by_key(|r| r.0);
    // Resolve code-point offsets → byte offsets in one pass. b0 = byte index of the
    // first char with cp_idx >= start; b1 = first char with cp_idx >= end+1 (or
    // text.len() when the span reaches end-of-string).
    let total_cps = text.chars().count();
    let mut spans: Vec<(usize, usize, String)> = Vec::with_capacity(ranges.len());
    let mut cursor_cp = 0usize; // overlap guard: next allowed start (code points)
    for (s, e, id) in ranges {
        if e >= total_cps || s < cursor_cp {
            // Out of range, or overlaps/touches a previous span → bail entirely.
            return Vec::new();
        }
        let mut b0: Option<usize> = None;
        let mut b1: Option<usize> = None;
        for (cp_idx, (byte_idx, _ch)) in text.char_indices().enumerate() {
            if b0.is_none() && cp_idx >= s {
                b0 = Some(byte_idx);
            }
            if cp_idx >= e + 1 {
                b1 = Some(byte_idx);
                break;
            }
        }
        let b0 = match b0 {
            Some(b) => b,
            None => return Vec::new(),
        };
        let b1 = b1.unwrap_or(text.len()); // span ended at end-of-string
        if b1 <= b0 || text.get(b0..b1).is_none() {
            return Vec::new();
        }
        spans.push((b0, b1, id));
        cursor_cp = e + 1;
    }
    spans
}

/// Split `text` on Unicode whitespace, emitting an `Emote` segment for each
/// whitespace-delimited token that exactly (case-sensitively) matches the
/// third-party `emote_map`, and `Text` for everything else (whitespace runs and
/// non-matching words), preserving the original spacing. Used both for whole
/// messages without first-party emotes and for the text gaps between them.
pub(super) fn word_match_segments(
    text: &str,
    emote_map: &HashMap<String, std::path::PathBuf>,
) -> Vec<ChatSegment> {
    if text.is_empty() {
        return Vec::new();
    }
    if emote_map.is_empty() {
        return vec![ChatSegment::Text(text.to_string())];
    }
    let mut out: Vec<ChatSegment> = Vec::new();
    let mut pending = String::new(); // accumulates Text (whitespace + non-emote words)
    // Walk maximal non-whitespace runs (candidate emote codes) and the whitespace
    // between them, so tabs / NBSP / multiple spaces are preserved verbatim.
    let mut rest = text;
    while !rest.is_empty() {
        // Leading whitespace run.
        let ws_end = rest
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        if ws_end > 0 {
            pending.push_str(&rest[..ws_end]);
            rest = &rest[ws_end..];
            continue;
        }
        // Non-whitespace token run.
        let tok_end = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let token = &rest[..tok_end];
        if let Some(path) = emote_map.get(token) {
            if !pending.is_empty() {
                out.push(ChatSegment::Text(std::mem::take(&mut pending)));
            }
            out.push(ChatSegment::Emote {
                name: token.to_string(),
                file: Some(path.clone()),
                fallback_text: None,
                pending: None,
            });
        } else {
            pending.push_str(token);
        }
        rest = &rest[tok_end..];
    }
    if !pending.is_empty() {
        out.push(ChatSegment::Text(pending));
    }
    out
}
// ── Chat viewer helpers ──────────────────────────────────────────────────────

/// Derive the chat sidecar path from a recording's output path.
/// Locate a recording's chat sidecar. yt-dlp's `live_chat` writer **appends** to the
/// `-o` value (keeping the video extension), so the YouTube sidecar is
/// `<output_path>.live_chat.json` (e.g. `clip.mkv.live_chat.json`) — not a simple
/// extension swap. The Twitch native logger instead **replaces** the extension
/// (`clip.chat.jsonl`). We try both forms, plus the legacy pre-`.cache` YouTube name
/// (`clip.ts.live_chat.json`).
pub(super) fn chat_file_for_recording(rec: &Recording) -> Option<std::path::PathBuf> {
    chat_file_candidates(rec).into_iter().find(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::ChatSidecar, p))
}

/// The candidate sidecar paths [`chat_file_for_recording`] probes, in order.
pub(super) fn chat_file_candidates(rec: &Recording) -> [std::path::PathBuf; 4] {
    let base = Path::new(&rec.output_path);
    [
        // YouTube (yt-dlp append form): `<output_path>.live_chat.json`.
        std::path::PathBuf::from(format!("{}.live_chat.json", rec.output_path)),
        // Twitch native logger (extension replace): `<stem>.chat.jsonl`.
        base.with_extension("chat.jsonl"),
        // Extension-replace live_chat form, just in case.
        base.with_extension("live_chat.json"),
        // Legacy pre-`.cache` YouTube name: `<stem>.ts.live_chat.json`.
        base.with_extension("ts.live_chat.json"),
    ]
}

/// [`chat_file_for_recording`] for render paths: existence via the non-blocking
/// [`FsProbes`] cache, so per-frame callers (the chat popup's recording picker,
/// the Streams-grid context menus) never stat the recordings drive themselves.
/// Answers can lag a probe round-trip (~a frame) behind the direct version.
pub(super) fn chat_file_for_recording_cached(
    fs: &mut FsProbes,
    rec: &Recording,
) -> Option<std::path::PathBuf> {
    chat_file_candidates(rec).into_iter().find(|p| fs.is_file(p))
}

pub(super) fn fmt_recording_label(rec: &Recording) -> String {
    let dt = chrono::DateTime::from_timestamp(rec.started_at, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| rec.started_at.to_string());
    format!("{dt} ({})", rec.status)
}

pub(super) fn fmt_chat_ts(secs: f64) -> String {
    if secs < 0.0 {
        return format!("-{}", fmt_chat_ts(-secs));
    }
    let s = secs as u64;
    format!("[{:02}:{:02}:{:02}]", s / 3600, (s % 3600) / 60, s % 60)
}

/// Soft cap on decoded emote-frame GPU memory; the cache is LRU-evicted past this.
pub(super) const EMOTE_BUDGET_BYTES: usize = 192 * 1024 * 1024;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_chat_message(
    ui: &mut egui::Ui,
    msg: &ChatMessage,
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    render_emotes: bool,
    animate: bool,
    now: f64,
    misses: &mut Vec<std::path::PathBuf>,
    ctx: &egui::Context,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        // Timestamp — muted monospace
        ui.label(
            egui::RichText::new(fmt_chat_ts(msg.timestamp_secs))
                .monospace()
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        // Badges
        for badge in &msg.badges {
            let (sym, color) = badge_display(badge, &msg.platform);
            ui.label(egui::RichText::new(sym).small().color(color));
        }
        // Username — bold, platform/user colour, adjusted for contrast on the
        // chat panel's background so dark colours stay legible.
        let name_color = chat_username_color(msg, ui.visuals().panel_fill);
        ui.label(
            egui::RichText::new(format!("{}:", msg.author))
                .strong()
                .color(name_color),
        );
        // Message body — text runs and (when enabled & on disk) inline emote images.
        let emote_h = (ui.text_style_height(&egui::TextStyle::Body) * 1.5).min(28.0);
        for seg in &msg.segments {
            match seg {
                ChatSegment::Text(t) => {
                    // One label per run: egui wraps a multi-word galley at word
                    // boundaries inside horizontal_wrapped while preserving the run's
                    // internal/leading/trailing whitespace verbatim.
                    ui.label(t);
                }
                ChatSegment::Emote { name, file, fallback_text, .. } => {
                    let drawn = render_emotes
                        && file.as_ref().is_some_and(|f| {
                            match draw_cached_emote(ui, cache, f, animate, emote_h, now, misses, ctx)
                            {
                                Some(resp) => {
                                    resp.on_hover_text(name);
                                    true
                                }
                                None => false,
                            }
                        });
                    if !drawn {
                        // No image (off / loading / not on disk / undecodable): show
                        // the emoji glyph if we have one, else the emote code.
                        ui.label(fallback_text.as_deref().unwrap_or(name));
                    }
                }
            }
        }
    });
}

/// CDN URL for an emote given provider, id, and extension.
pub(super) fn emote_cdn_url(provider: EmoteProvider, id: &str, ext: &str) -> String {
    match provider {
        EmoteProvider::Twitch => {
            if ext == "gif" {
                format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/animated/dark/3.0")
            } else {
                format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/static/dark/3.0")
            }
        }
        EmoteProvider::SevenTv => format!("https://cdn.7tv.app/emote/{id}/4x.{ext}"),
        EmoteProvider::Bttv => format!("https://cdn.betterttv.net/emote/{id}/3x.{ext}"),
        EmoteProvider::Ffz => format!("https://cdn.frankerfacez.com/emoticon/{id}/4"),
    }
}
/// Copy an image file's raw bytes to the Windows clipboard under the `PNG` format.
/// Most apps (Discord, browsers, image editors) accept `CF_PNG` for paste.
pub(super) fn copy_emote_image_to_clipboard(path: &std::path::Path) {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    let Ok(bytes) = crate::iomon::fs::read_sync(crate::iomon::Cat::AssetCache, path) else { return };

    let fmt_name: Vec<u16> = "PNG\0".encode_utf16().collect();
    let fmt = unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(fmt_name.as_ptr())) };
    if fmt == 0 {
        return;
    }

    unsafe {
        let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) else { return };
        let ptr = GlobalLock(hmem);
        if ptr.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(hmem);

        if OpenClipboard(None).is_ok() {
            let _ = EmptyClipboard();
            // SetClipboardData takes ownership of hmem on success; do not free it.
            let _ = SetClipboardData(
                fmt,
                Some(windows::Win32::Foundation::HANDLE(hmem.0 as *mut std::ffi::c_void)),
            );
            let _ = CloseClipboard();
        }
    }
}

/// Lay out a provider's emotes as a wrapping grid of fixed-width cells: the emote
/// image above its code. `deprecated` cells skip the image entirely (the file is
/// gone) — they show a 🚫 placeholder and strike through the code. Loading cells
/// show a `…` until the off-thread decode lands.
#[allow(clippy::too_many_arguments)]
pub(super) fn emote_viewer_grid(
    ui: &mut egui::Ui,
    emotes: &[ViewerEmote],
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    animate: bool,
    now: f64,
    misses: &mut Vec<std::path::PathBuf>,
    ctx: &egui::Context,
    deprecated: bool,
    provider: EmoteProvider,
    pending_properties: &mut Option<ViewerEmote>,
) {
    const CELL_W: f32 = 92.0;
    const IMG_H: f32 = 44.0;
    ui.horizontal_wrapped(|ui| {
        for e in emotes {
            let cell = ui.allocate_ui(egui::vec2(CELL_W, IMG_H + 22.0), |ui| {
                // Virtualize: only decode/upload/draw emotes whose cell is on screen.
                // `draw_cached_emote` stamps `last_drawn = now` on every Ready entry it
                // touches, which pins it against `evict_emote_cache` (it keeps anything
                // with `last_drawn >= now`). Drawing every emote each frame would pin the
                // entire provider — hundreds of animated emotes — past EMOTE_BUDGET_BYTES
                // and the LRU could never reclaim it. Off-screen cells reserve the same
                // band height (so wrap points / scroll extent stay put) but skip the cache
                // entirely, letting scrolled-away emotes age out and be evicted.
                let visible = ui.is_rect_visible(ui.max_rect());
                ui.vertical_centered(|ui| {
                    let img_resp = if deprecated {
                        ui.add_space((IMG_H - 18.0) / 2.0);
                        ui.label(egui::RichText::new("🚫").size(18.0).weak());
                        ui.add_space((IMG_H - 18.0) / 2.0);
                        None
                    } else if !visible {
                        ui.add_space(IMG_H);
                        None
                    } else {
                        let r = draw_cached_emote(ui, cache, &e.path, animate, IMG_H, now, misses, ctx);
                        if r.is_none() {
                            ui.add_space(IMG_H / 2.0 - 6.0);
                            ui.weak("…");
                            ui.add_space(IMG_H / 2.0 - 6.0);
                        }
                        r
                    };

                    // Alt-hover: show enlarged image + emote info as a tooltip.
                    // on_hover_ui_at_pointer takes self; clone the response so
                    // img_resp stays usable for the label below.
                    if let Some(resp) = img_resp.clone() {
                        if resp.hovered() && ctx.input(|i| i.modifiers.alt) {
                            let (epath, ename, eid, eext) = (
                                e.path.clone(),
                                e.name.clone(),
                                e.id.clone(),
                                e.ext.clone(),
                            );
                            resp.on_hover_ui_at_pointer(|ui| {
                                ui.set_max_width(280.0);
                                // Render cached texture at 3-4× cell size.
                                // The cache caps decode at 56 px so no re-upload.
                                draw_cached_emote(
                                    ui, cache, &epath, false, 160.0, now,
                                    &mut Vec::new(), ctx,
                                );
                                ui.separator();
                                let url = emote_cdn_url(provider, &eid, &eext);
                                egui::Grid::new(
                                    egui::Id::new("alt_emote_tip").with(&eid),
                                )
                                .num_columns(2)
                                .show(ui, |ui| {
                                    ui.label("Name:");
                                    ui.label(&ename);
                                    ui.end_row();
                                    ui.label("ID:");
                                    ui.label(&eid);
                                    ui.end_row();
                                    ui.label("URL:");
                                    ui.label(&url);
                                    ui.end_row();
                                });
                            });
                        }
                    }

                    let mut rt = egui::RichText::new(truncate_label(&e.name, 12)).small();
                    if deprecated {
                        rt = rt.strikethrough().weak();
                    }
                    ui.label(rt).on_hover_text(&e.name);
                });
            });

            // Right-click context menu on the entire cell.
            // allocate_ui returns Sense::hover(), which makes secondary_clicked()
            // always false and context_menu never fires. Re-interact with Sense::click()
            // on the same rect so the right-click is detected properly.
            let ctx_resp = ui.interact(
                cell.response.rect,
                egui::Id::new("emote_ctx").with(&e.id),
                egui::Sense::click(),
            );
            ctx_resp.context_menu(|ui| {
                if ui.button("Copy Image").clicked() {
                    copy_emote_image_to_clipboard(&e.path);
                    ui.close();
                }
                if ui.button("Open File").clicked() {
                    open_path(&e.path);
                    ui.close();
                }
                if ui.button("Open Folder").clicked() {
                    if let Some(dir) = e.path.parent() {
                        open_path(dir);
                    }
                    ui.close();
                }
                if ui.button("Copy URL").clicked() {
                    ui.ctx().copy_text(emote_cdn_url(provider, &e.id, &e.ext));
                    ui.close();
                }
                ui.separator();
                if ui.button("Properties").clicked() {
                    *pending_properties = Some(ViewerEmote {
                        name: e.name.clone(),
                        id: e.id.clone(),
                        ext: e.ext.clone(),
                        path: e.path.clone(),
                        exists: e.exists,
                    });
                    ui.close();
                }
            });
        }
    });
}

/// Draw an emote from the decode cache. Returns the image `Response` when drawn, or
/// `None` (caller shows the text fallback) when the emote is still loading / failed.
/// Promotes a freshly-decoded entry to GPU textures (UI-thread upload), advances
/// the animation against the global clock `now`, and records `last_drawn` for LRU.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_cached_emote(
    ui: &mut egui::Ui,
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    path: &Path,
    animate: bool,
    emote_h: f32,
    now: f64,
    misses: &mut Vec<std::path::PathBuf>,
    ctx: &egui::Context,
) -> Option<egui::Response> {
    use crate::emote_anim::EmoteLoad;
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    // Promote Decoded → Ready by uploading the frames to GPU textures here (must be
    // on the UI thread / with a live `ctx`).
    if matches!(g.get(path), Some(EmoteLoad::Decoded(..))) {
        if let Some(EmoteLoad::Decoded(imgs, delays)) = g.remove(path) {
            let anim = crate::emote_anim::upload(imgs, delays, ctx, &path.to_string_lossy());
            g.insert(path.to_path_buf(), EmoteLoad::Ready(anim));
        }
    }
    match g.get_mut(path) {
        None => {
            g.insert(path.to_path_buf(), EmoteLoad::Loading);
            misses.push(path.to_path_buf());
            None
        }
        Some(EmoteLoad::Loading) | Some(EmoteLoad::Failed) | Some(EmoteLoad::Decoded(..)) => None,
        Some(EmoteLoad::Ready(anim)) => {
            anim.last_drawn = now;
            let s = anim.size();
            // Height ≤ emote_h, width capped at 112, aspect preserved. Never upscale
            // (`.min(1.0)`) — a small emote keeps its native size, matching the prior
            // loader behaviour. `s` is already downscaled to ≤56px at decode time.
            let scale = (emote_h / s.y.max(1.0)).min(112.0 / s.x.max(1.0)).min(1.0);
            let size = egui::vec2(s.x * scale, s.y * scale);
            if animate && anim.is_animated() {
                let (tex, remaining) = anim.frame_at(now);
                let resp = ui.add(egui::Image::from_texture(tex).fit_to_exact_size(size));
                // Only schedule the next frame for emotes actually on screen, so a
                // scrolled-away animation doesn't keep waking the UI.
                if ui.is_rect_visible(resp.rect) {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f32(
                        remaining.min(1.0),
                    ));
                }
                Some(resp)
            } else {
                let (tex, _) = anim.frame_at(0.0);
                Some(ui.add(egui::Image::from_texture(tex).fit_to_exact_size(size)))
            }
        }
    }
}

/// Evict the least-recently-drawn ready emotes once the decoded-frame cache exceeds
/// [`EMOTE_BUDGET_BYTES`]. Emotes drawn this frame (`last_drawn == now`) are kept.
pub(super) fn evict_emote_cache(
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    now: f64,
) {
    use crate::emote_anim::EmoteLoad;
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    let total: usize = g
        .values()
        .map(|v| if let EmoteLoad::Ready(a) = v { a.bytes } else { 0 })
        .sum();
    if total <= EMOTE_BUDGET_BYTES {
        return;
    }
    let mut ready: Vec<(std::path::PathBuf, f64, usize)> = g
        .iter()
        .filter_map(|(k, v)| match v {
            EmoteLoad::Ready(a) => Some((k.clone(), a.last_drawn, a.bytes)),
            _ => None,
        })
        .collect();
    ready.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut cur = total;
    for (k, last_drawn, bytes) in ready {
        if cur <= EMOTE_BUDGET_BYTES {
            break;
        }
        if last_drawn >= now {
            continue; // visible this frame — keep
        }
        g.remove(&k);
        cur -= bytes;
    }
}

pub(super) fn badge_display(badge: &str, platform: &ChatPlatform) -> (&'static str, egui::Color32) {
    match platform {
        ChatPlatform::Twitch => {
            let name = badge.split('/').next().unwrap_or(badge);
            match name {
                "broadcaster" => ("📡", egui::Color32::from_rgb(0xe9, 0x1e, 0x63)),
                "moderator" | "mod" => ("⚔", egui::Color32::from_rgb(0x00, 0xad, 0x03)),
                "subscriber" => ("★", egui::Color32::from_rgb(0x96, 0x4b, 0xff)),
                "bits" => ("💎", egui::Color32::from_rgb(0x00, 0xc7, 0xac)),
                "premium" => ("👑", egui::Color32::from_rgb(0xff, 0xd7, 0x00)),
                "partner" => ("✓", egui::Color32::from_rgb(0x97, 0x45, 0xff)),
                _ => ("•", egui::Color32::GRAY),
            }
        }
        ChatPlatform::YouTube => {
            let lower = badge.to_lowercase();
            if lower.contains("member") {
                ("⭐", egui::Color32::from_rgb(0xff, 0xd7, 0x00))
            } else if lower.contains("moderator") {
                ("⚔", egui::Color32::from_rgb(0x00, 0xad, 0x03))
            } else if lower.contains("verified") || lower.contains("owner") {
                ("✓", egui::Color32::from_rgb(0x4a, 0xc2, 0xff))
            } else {
                ("•", egui::Color32::GRAY)
            }
        }
    }
}

/// The display colour for a chat author's name, adjusted to stay legible on the
/// chat panel's background `bg`. The base colour mirrors each platform: a Twitch
/// user's chosen USERCOLOR (or their deterministic default from Twitch's 15-colour
/// palette), and YouTube's role-based name colours (mod/member/owner/regular).
pub(super) fn chat_username_color(msg: &ChatMessage, bg: egui::Color32) -> egui::Color32 {
    let base = match (msg.color_override, &msg.platform) {
        // Twitch USERCOLOR (IRC `color` tag), used as-is by both platforms when set.
        (Some(c), _) => c,
        (None, ChatPlatform::Twitch) => twitch_username_color(&msg.author),
        (None, ChatPlatform::YouTube) => youtube_username_color(&msg.badges),
    };
    readable_color(base, bg)
}

/// Twitch's 15 default name colours, assigned to users who never picked one.
/// Twitch keys this off the name (first + last char), so the same user is always
/// the same colour — we reproduce that exactly for ASCII names.
pub(super) fn twitch_username_color(name: &str) -> egui::Color32 {
    const DEFAULTS: [egui::Color32; 15] = [
        egui::Color32::from_rgb(0xFF, 0x00, 0x00), // Red
        egui::Color32::from_rgb(0x00, 0x00, 0xFF), // Blue
        egui::Color32::from_rgb(0x00, 0x80, 0x00), // Green
        egui::Color32::from_rgb(0xB2, 0x22, 0x22), // FireBrick
        egui::Color32::from_rgb(0xFF, 0x7F, 0x50), // Coral
        egui::Color32::from_rgb(0x9A, 0xCD, 0x32), // YellowGreen
        egui::Color32::from_rgb(0xFF, 0x45, 0x00), // OrangeRed
        egui::Color32::from_rgb(0x2E, 0x8B, 0x57), // SeaGreen
        egui::Color32::from_rgb(0xDA, 0xA5, 0x20), // GoldenRod
        egui::Color32::from_rgb(0xD2, 0x69, 0x1E), // Chocolate
        egui::Color32::from_rgb(0x5F, 0x9E, 0xA0), // CadetBlue
        egui::Color32::from_rgb(0x1E, 0x90, 0xFF), // DodgerBlue
        egui::Color32::from_rgb(0xFF, 0x69, 0xB4), // HotPink
        egui::Color32::from_rgb(0x8A, 0x2B, 0xE2), // BlueViolet
        egui::Color32::from_rgb(0x00, 0xFF, 0x7F), // SpringGreen
    ];
    let b = name.as_bytes();
    if b.is_empty() {
        return egui::Color32::GRAY;
    }
    let n = (b[0] as usize + b[b.len() - 1] as usize) % DEFAULTS.len();
    DEFAULTS[n]
}

/// YouTube live-chat name colours by role (derived from the author's badges):
/// moderator blue, member green, owner gold, and a neutral grey for everyone else
/// (YouTube doesn't per-user colour regular names). Readability is applied later.
pub(super) fn youtube_username_color(badges: &[String]) -> egui::Color32 {
    let has = |needle: &str| badges.iter().any(|b| b.to_lowercase().contains(needle));
    if has("owner") {
        egui::Color32::from_rgb(0xFF, 0xD6, 0x00) // channel owner — gold
    } else if has("moderator") {
        egui::Color32::from_rgb(0x5E, 0x84, 0xF1) // YouTube moderator blue
    } else if has("member") {
        egui::Color32::from_rgb(0x2B, 0xA6, 0x40) // YouTube member green
    } else {
        egui::Color32::from_rgb(0xB0, 0xB0, 0xB0) // regular — neutral grey
    }
}

/// WCAG relative luminance of a colour (sRGB → linear, then the standard weights).
pub(super) fn relative_luminance(c: egui::Color32) -> f32 {
    let lin = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio between two colours (1.0 = identical, 21.0 = black/white).
pub(super) fn contrast_ratio(a: egui::Color32, b: egui::Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Nudge `fg`'s lightness away from the background (lighter on a dark bg, darker on
/// a light bg) until it clears a contrast floor, preserving hue — the way Twitch
/// lightens dark name colours in dark mode so e.g. pure blue stays legible. Returns
/// `fg` unchanged when it's already comfortable.
pub(super) fn readable_color(fg: egui::Color32, bg: egui::Color32) -> egui::Color32 {
    // Slightly under WCAG AA (4.5): names are bold, and staying closer keeps the
    // colour vivid rather than washing it toward white/black.
    const TARGET: f32 = 4.0;
    if contrast_ratio(fg, bg) >= TARGET {
        return fg;
    }
    // Push toward whichever extreme can actually out-contrast the background, not a
    // flat luminance midpoint — for a mid-tone background, lightening toward white
    // may never reach the target while darkening toward black does (and vice-versa).
    let lighten = contrast_ratio(egui::Color32::WHITE, bg) >= contrast_ratio(egui::Color32::BLACK, bg);
    let (h, s, mut l) = rgb_to_hsl(fg);
    let mut out = fg;
    for _ in 0..50 {
        l = if lighten { (l + 0.02).min(1.0) } else { (l - 0.02).max(0.0) };
        out = hsl_to_rgb(h, s, l);
        if contrast_ratio(out, bg) >= TARGET {
            return out;
        }
        if l <= 0.0 || l >= 1.0 {
            break; // can't push further; return the best we reached
        }
    }
    out
}

/// sRGB → HSL (hue degrees 0–360, saturation/lightness 0–1).
pub(super) fn rgb_to_hsl(c: egui::Color32) -> (f32, f32, f32) {
    let (r, g, b) = (
        c.r() as f32 / 255.0,
        c.g() as f32 / 255.0,
        c.b() as f32 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-6 {
        return (0.0, 0.0, l); // achromatic (grey)
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    ((h * 60.0).rem_euclid(360.0), s, l)
}

/// HSL → sRGB (inverse of [`rgb_to_hsl`]).
pub(super) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> egui::Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| (((v + m) * 255.0).round()).clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgb(to(r1), to(g1), to(b1))
}

pub(super) fn parse_chat_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

/// First existing emote image for `{stem}` in `dir`, trying the formats Twitch
/// uses (static `.png`, animated `.gif`) plus `.webp`. `None` when none exist.
pub(super) fn find_emote_file(dir: &Path, stem: &str) -> Option<std::path::PathBuf> {
    ["png", "gif", "webp"]
        .iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, p))
}

/// An emoji image not yet on disk that the renderer would otherwise show as a
/// glyph. Collected during parse; the popup tries each `url` in order (Twemoji's
/// FE0F naming is irregular) and writes the first that succeeds to `dest`.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct EmojiFetch {
    pub(super) dest: std::path::PathBuf,
    pub(super) urls: Vec<String>,
}

/// One parsed slice of a chat file: the messages, the emoji images to fetch,
/// and the byte offset just past the last complete line — the resume point for
/// the next incremental pass.
pub(super) struct ChatChunk {
    pub(super) messages: Vec<ChatMessage>,
    pub(super) fetches: Vec<EmojiFetch>,
    pub(super) parsed_to: u64,
}

/// Split a text run into [`ChatSegment`]s, turning each Unicode-emoji cluster into
/// an `Emote` that resolves to a cached Twemoji image (with the glyph as fallback),
/// and recording any not-yet-downloaded image in `fetches`. Plain text passes
/// through unchanged (fast path).
pub(super) fn emoji_split(text: &str, fetches: &mut Vec<EmojiFetch>) -> Vec<ChatSegment> {
    let runs = crate::emoji::segment(text);
    if runs.iter().all(|(_, is_emoji)| !is_emoji) {
        return vec![ChatSegment::Text(text.to_string())];
    }
    let emoji_dir = emoji_cache_dir();
    let mut out = Vec::with_capacity(runs.len());
    for (slice, is_emoji) in runs {
        if is_emoji {
            let key = crate::emoji::cache_key(slice);
            let dest = emoji_dir.join(format!("{key}.png"));
            let file = crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &dest).then(|| dest.clone());
            // Skip re-fetching emoji we've already failed to download (a `.404`
            // marker), so a liberal false-positive / missing asset isn't re-requested
            // on every live tail-reload.
            if file.is_none() && !crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, emoji_dir.join(format!("{key}.404"))) {
                fetches.push(EmojiFetch {
                    dest: dest.clone(),
                    urls: crate::emoji::twemoji_url_candidates(slice),
                });
            }
            let pending = file.is_none().then_some(dest);
            out.push(ChatSegment::Emote {
                name: slice.to_string(),
                file,
                fallback_text: Some(slice.to_string()),
                pending,
            });
        } else if !slice.is_empty() {
            out.push(ChatSegment::Text(slice.to_string()));
        }
    }
    out
}

/// The shared emoji image cache directory (`asset-cache/emotes/emoji/`).
pub(super) fn emoji_cache_dir() -> std::path::PathBuf {
    crate::app_paths::asset_cache_dir()
        .join("emotes")
        .join("emoji")
}

/// Expand the `Text` segments of an already-built segment list, splitting out any
/// Unicode emoji into image segments. Emote segments are left untouched.
pub(super) fn expand_emoji(segments: Vec<ChatSegment>, fetches: &mut Vec<EmojiFetch>) -> Vec<ChatSegment> {
    let mut out = Vec::with_capacity(segments.len());
    for seg in segments {
        match seg {
            ChatSegment::Text(t) => out.extend(emoji_split(&t, fetches)),
            other => out.push(other),
        }
    }
    out
}

/// File extension to use for a downloaded image, from the URL path (png/gif/webp),
/// defaulting to `png`.
pub(super) fn url_ext(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .and_then(|p| p.rsplit('.').next())
        .filter(|e| matches!(*e, "png" | "gif" | "webp" | "jpg" | "jpeg"))
        .unwrap_or("png")
}

/// How much of the file's tail the phase-1 (instant) parse covers. Enough for
/// hundreds of Twitch lines / dozens of (much fatter) YouTube lines.
pub(super) const CHAT_TAIL_BYTES: u64 = 512 * 1024;

/// Parse the byte range `[from, to)` of a chat file (`to == None` reads to the
/// current EOF). Only complete (newline-terminated) lines are parsed; a
/// trailing partial line — the logger may be mid-write — is left for the next
/// pass via `parsed_to`, so incremental tail reads never split a message. Both
/// formats (Twitch `.chat.jsonl`, YouTube `.live_chat.json`) are line-delimited
/// JSON, so byte-offset resumption is exact.
pub(super) fn parse_chat_chunk(
    path: &Path,
    from: u64,
    to: Option<u64>,
    start_unix_secs: i64,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
) -> anyhow::Result<ChatChunk> {
    use std::io::{Read, Seek, SeekFrom};
    // Read window: bounds peak memory on huge logs — the previous whole-range
    // slurp held roughly 2x the file size in RAM for a marathon stream's
    // phase-2 parse.
    const WINDOW: usize = 8 * 1024 * 1024;
    let chat_region = crate::iomon::classify(path);
    let mut f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let len = f.metadata()?.len();
    let end = to.unwrap_or(len).min(len);
    if from >= end {
        return Ok(ChatChunk { messages: Vec::new(), fetches: Vec::new(), parsed_to: from });
    }
    f.seek(SeekFrom::Start(from))?;
    let is_yt = path.to_string_lossy().ends_with("live_chat.json");
    let start_ms = start_unix_secs as f64 * 1000.0;
    let mut messages = Vec::new();
    let mut fetches: Vec<EmojiFetch> = Vec::new();
    let mut parsed_to = from;
    let mut pos = from;
    // Carries a partial line across window boundaries.
    let mut buf: Vec<u8> = Vec::new();
    while pos < end {
        let take = WINDOW.min((end - pos) as usize);
        let old_len = buf.len();
        buf.resize(old_len + take, 0);
        let read_start = std::time::Instant::now();
        let read_res = f.read_exact(&mut buf[old_len..]);
        crate::iomon::record_region(
            crate::iomon::Cat::ChatSidecar,
            chat_region,
            crate::iomon::OpKind::Read,
            take as u64,
            read_start.elapsed(),
        );
        read_res?;
        pos += take as u64;
        let complete = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            // No boundary yet — a single line larger than the window; grow
            // the buffer with the next window.
            None if pos < end => continue,
            // A bounded chunk ends on a known line boundary; an unbounded
            // tail can end mid-line while the logger is writing.
            None if to.is_some() => buf.len(),
            None => 0,
        };
        {
            let text = String::from_utf8_lossy(&buf[..complete]);
            for line in text.lines() {
                if line.is_empty() {
                    continue;
                }
                if is_yt {
                    parse_yt_chat_line(line, &mut messages, &mut fetches);
                } else if let Some(m) =
                    parse_twitch_chat_line(line, start_ms, emote_map, twitch_dir, &mut fetches)
                {
                    messages.push(m);
                }
            }
        }
        buf.drain(..complete);
        parsed_to = pos - buf.len() as u64;
    }
    // De-duplicate so the same emoji isn't downloaded once per occurrence.
    fetches.sort_by(|a, b| a.dest.cmp(&b.dest));
    fetches.dedup();
    Ok(ChatChunk { messages, fetches, parsed_to })
}

/// The first line boundary within the file's last [`CHAT_TAIL_BYTES`] — where
/// the phase-1 tail parse starts. 0 for small files (just parse everything).
pub(super) fn chat_tail_start(path: &Path) -> anyhow::Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let len = f.metadata()?.len();
    if len <= CHAT_TAIL_BYTES {
        return Ok(0);
    }
    f.seek(SeekFrom::Start(len - CHAT_TAIL_BYTES))?;
    let mut buf = vec![0u8; CHAT_TAIL_BYTES as usize];
    let read_start = std::time::Instant::now();
    let read_res = f.read_exact(&mut buf);
    crate::iomon::record(
        crate::iomon::Cat::ChatSidecar,
        path,
        crate::iomon::OpKind::Read,
        CHAT_TAIL_BYTES,
        read_start.elapsed(),
    );
    read_res?;
    Ok(match buf.iter().position(|&b| b == b'\n') {
        Some(i) => len - CHAT_TAIL_BYTES + i as u64 + 1,
        None => 0, // no boundary in the tail (one giant line) — parse it all
    })
}

/// Parse a chunk of a chat file off the UI thread, mapping both join and parse
/// errors to a string for [`ChatLoadState::Error`].
pub(super) async fn parse_chunk_blocking(
    path: std::path::PathBuf,
    from: u64,
    to: Option<u64>,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
) -> Result<ChatChunk, String> {
    tokio::task::spawn_blocking(move || {
        parse_chat_chunk(&path, from, to, start_ts, &emote_map, twitch_dir.as_deref())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// Promote pending emote images that have landed on disk since the parse.
/// The existence checks run OUTSIDE the chat mutex and off the async threads
/// (`spawn_blocking`) — stat-ing thousands of segments while holding the lock
/// the renderer takes every frame froze the whole app.
pub(super) async fn upgrade_pending_emotes(state: &Arc<Mutex<ChatLoadState>>) {
    let pending: Vec<std::path::PathBuf> = {
        let st = state.lock().unwrap();
        let ChatLoadState::Loaded(log) = &*st else { return };
        let mut set: HashSet<std::path::PathBuf> = HashSet::new();
        for m in &log.messages {
            for seg in &m.segments {
                if let ChatSegment::Emote { file: None, pending: Some(p), .. } = seg {
                    set.insert(p.clone());
                }
            }
        }
        set.into_iter().collect()
    };
    if pending.is_empty() {
        return;
    }
    let on_disk: HashSet<std::path::PathBuf> = tokio::task::spawn_blocking(move || {
        pending.into_iter().filter(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, p)).collect()
    })
    .await
    .unwrap_or_default();
    if on_disk.is_empty() {
        return;
    }
    if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
        for m in &mut log.messages {
            for seg in &mut m.segments {
                if let ChatSegment::Emote { file, pending, .. } = seg
                    && file.is_none()
                    && pending.as_ref().is_some_and(|p| on_disk.contains(p))
                {
                    *file = pending.take();
                }
            }
        }
    }
}

/// Download missing emoji/emoji-emote images (sequential, best-effort; 404s for a
/// liberally-detected non-emoji just leave the glyph). Capped so a pathological
/// message can't trigger thousands of requests.
pub(super) async fn download_emoji_images(fetches: &[EmojiFetch]) {
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return;
    };
    for f in fetches.iter().take(300) {
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &f.dest) {
            continue;
        }
        let mut got = false;
        let mut network_error = false; // a transient failure → don't negative-cache
        for url in &f.urls {
            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(bytes) = resp.bytes().await {
                        if let Some(parent) = f.dest.parent() {
                            let _ = crate::iomon::fs::create_dir_all(crate::iomon::Cat::AssetCache, parent).await;
                        }
                        if crate::iomon::fs::write(crate::iomon::Cat::AssetCache, &f.dest, &bytes).await.is_ok() {
                            got = true;
                            break;
                        }
                    }
                }
                // 404 = this candidate name doesn't exist → try the next candidate.
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {}
                // Any other HTTP status or a transport error is transient-ish.
                Ok(_) | Err(_) => network_error = true,
            }
        }
        // Negative-cache ONLY a definitive miss (every candidate 404'd, no network
        // error), so a transient offline failure can't permanently block a real
        // emoji. `dest` is `{key}.png` → marker `{key}.404`.
        if !got && !network_error {
            let marker = f.dest.with_extension("404");
            if let Some(parent) = marker.parent() {
                let _ = crate::iomon::fs::create_dir_all(crate::iomon::Cat::AssetCache, parent).await;
            }
            let _ = crate::iomon::fs::write(crate::iomon::Cat::AssetCache, &marker, b"").await;
        }
    }
}

/// Load a chat file into `state`, tail-first: the newest [`CHAT_TAIL_BYTES`]
/// parse and display immediately, then the rest of the file parses in the
/// background and is spliced in front (`loading_older` marks the gap). Then —
/// when `fetch_emoji` — missing emoji images download once and upgrade the
/// in-memory segments in place. Runs entirely off the UI thread.
pub(super) async fn load_chat(
    state: Arc<Mutex<ChatLoadState>>,
    loading: Arc<AtomicBool>,
    path: Option<std::path::PathBuf>,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
    fetch_emoji: bool,
    ctx: egui::Context,
) {
    let Some(path) = path else {
        *state.lock().unwrap() = ChatLoadState::NoFile;
        ctx.request_repaint();
        return;
    };
    // Phase 1: the file's tail — the newest messages show instantly instead of
    // waiting for a full-file parse.
    let head_end = {
        let p = path.clone();
        match tokio::task::spawn_blocking(move || chat_tail_start(&p)).await {
            Ok(Ok(off)) => off,
            Ok(Err(e)) => {
                *state.lock().unwrap() = ChatLoadState::Error(e.to_string());
                ctx.request_repaint();
                return;
            }
            Err(e) => {
                *state.lock().unwrap() = ChatLoadState::Error(e.to_string());
                ctx.request_repaint();
                return;
            }
        }
    };
    let mut fetches = match parse_chunk_blocking(
        path.clone(),
        head_end,
        None,
        start_ts,
        emote_map.clone(),
        twitch_dir.clone(),
    )
    .await
    {
        Ok(chunk) => {
            *state.lock().unwrap() = ChatLoadState::Loaded(ChatLog {
                messages: chunk.messages,
                row_heights: Vec::new(),
                measured_width: 0.0,
                parsed_to: chunk.parsed_to,
                loading_older: head_end > 0,
            });
            ctx.request_repaint();
            chunk.fetches
        }
        Err(e) => {
            *state.lock().unwrap() = ChatLoadState::Error(e);
            ctx.request_repaint();
            return;
        }
    };
    // Phase 2: everything before the tail, spliced in front when ready.
    if head_end > 0 {
        match parse_chunk_blocking(
            path.clone(),
            0,
            Some(head_end),
            start_ts,
            emote_map.clone(),
            twitch_dir.clone(),
        )
        .await
        {
            Ok(older) => {
                fetches.extend(older.fetches);
                if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
                    // Heights are index-parallel to messages: prepend matching
                    // estimates, or every measured tail height would be
                    // re-attributed to the oldest rows and the virtualized
                    // offsets/scrollbar would scramble.
                    if !log.row_heights.is_empty() {
                        let n = older.messages.len();
                        log.row_heights
                            .splice(0..0, std::iter::repeat_n(CHAT_ROW_EST, n));
                    }
                    log.messages.splice(0..0, older.messages);
                    log.loading_older = false;
                }
            }
            Err(_) => {
                if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
                    log.loading_older = false;
                }
            }
        }
        ctx.request_repaint();
    }
    // Phase 3: emoji downloads + in-place upgrade. Only one download pass runs
    // at a time (a concurrent tail reload just skips kicking off another).
    fetches.sort_by(|a, b| a.dest.cmp(&b.dest));
    fetches.dedup();
    if fetch_emoji && !fetches.is_empty() && !loading.swap(true, Ordering::SeqCst) {
        download_emoji_images(&fetches).await;
        loading.store(false, Ordering::SeqCst);
        upgrade_pending_emotes(&state).await;
        ctx.request_repaint();
    }
}

/// Incremental tail reload for a live recording: parse only the bytes appended
/// since the last pass and push them onto the existing log. (The previous
/// implementation re-parsed the entire file every few seconds.)
#[allow(clippy::too_many_arguments)]
pub(super) async fn tail_chat(
    state: Arc<Mutex<ChatLoadState>>,
    loading: Arc<AtomicBool>,
    path: std::path::PathBuf,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
    fetch_emoji: bool,
    ctx: egui::Context,
) {
    let from = {
        match &*state.lock().unwrap() {
            ChatLoadState::Loaded(log) => Some(log.parsed_to),
            // Initial load still in flight — let it finish first.
            ChatLoadState::Loading => return,
            // The sidecar may have appeared since (opened seconds after the
            // recording started) or a transient read error cleared — retry the
            // full tail-first load instead of staying broken forever.
            ChatLoadState::NoFile | ChatLoadState::Error(_) => None,
        }
    };
    let Some(from) = from else {
        load_chat(state, loading, Some(path), start_ts, emote_map, twitch_dir, fetch_emoji, ctx)
            .await;
        return;
    };
    let Ok(chunk) = parse_chunk_blocking(path, from, None, start_ts, emote_map, twitch_dir).await
    else {
        return;
    };
    // Always advance past parsed complete lines — a chunk of non-message lines
    // (tickers, moderation events) must not freeze the resume offset, or every
    // 3s pass re-reads an ever-growing suffix.
    if chunk.parsed_to > from || !chunk.messages.is_empty() {
        if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
            // Overlapping reloads on a slow read could double-append; only the
            // pass that still matches the resume offset lands.
            if log.parsed_to == from {
                log.messages.extend(chunk.messages);
                log.parsed_to = chunk.parsed_to;
            }
        }
        ctx.request_repaint();
    }
    if fetch_emoji && !chunk.fetches.is_empty() && !loading.swap(true, Ordering::SeqCst) {
        download_emoji_images(&chunk.fetches).await;
        loading.store(false, Ordering::SeqCst);
        upgrade_pending_emotes(&state).await;
        ctx.request_repaint();
    }
}

/// Parse one line of a YouTube `.live_chat.json` file (a line can carry several
/// messages in the VOD-replay format), appending to `out`.
pub(super) fn parse_yt_chat_line(line: &str, out: &mut Vec<ChatMessage>, fetches: &mut Vec<EmojiFetch>) {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    if let Some(replay) = v.get("replayChatItemAction") {
        // VOD replay format: replayChatItemAction.{videoOffsetTimeMsec, actions[]}
        let offset_ms = replay
            .get("videoOffsetTimeMsec")
            .and_then(|x| x.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| x.as_i64()));
        if let Some(actions) = replay.get("actions").and_then(|a| a.as_array()) {
            for action in actions {
                if let Some(msg) = yt_action_to_msg(action, offset_ms, fetches) {
                    out.push(msg);
                }
            }
        }
    } else if let Some(msg) = yt_action_to_msg(&v, None, fetches) {
        // Live format: addChatItemAction directly at the top level of each line.
        out.push(msg);
    }
}

pub(super) fn yt_action_to_msg(
    action: &serde_json::Value,
    offset_ms: Option<i64>,
    fetches: &mut Vec<EmojiFetch>,
) -> Option<ChatMessage> {
    let r = action.pointer("/addChatItemAction/item/liveChatTextMessageRenderer")?;
    let ts_secs = if let Some(ms) = offset_ms {
        ms as f64 / 1000.0
    } else {
        r["timestampUsec"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1_000_000.0
    };
    let author = r
        .pointer("/authorName/simpleText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // YouTube pre-tokenizes the body as `message.runs[]`: text runs are literal,
    // emoji runs carry either a standard unicode char (`emojiId`) or a custom
    // channel emoji (image-only). Build the display `segments` and the verbatim
    // search `text` in one pass.
    let mut text = String::new();
    let mut segments: Vec<ChatSegment> = Vec::new();
    if let Some(runs) = r["message"]["runs"].as_array() {
        for run in runs {
            if let Some(t) = run["text"].as_str() {
                text.push_str(t);
                // Text runs can themselves contain literal unicode emoji.
                segments.extend(emoji_split(t, fetches));
            } else if let Some(emoji) = run.get("emoji") {
                let shortcut = emoji["shortcuts"]
                    .as_array()
                    .and_then(|s| s.first())
                    .and_then(|e| e.as_str());
                let emoji_id = emoji["emojiId"].as_str();
                let label = shortcut.or(emoji_id).unwrap_or("[emoji]");
                text.push_str(label);
                if emoji["isCustomEmoji"].as_bool() == Some(true) {
                    // Custom channel emoji: image-only. Download YouTube's own PNG
                    // (largest thumbnail) into the cache; until present, fall back to
                    // the shortcut text.
                    let url = emoji
                        .pointer("/image/thumbnails")
                        .and_then(|t| t.as_array())
                        .and_then(|a| a.last())
                        .and_then(|t| t["url"].as_str());
                    let mut pending = None;
                    let file = emoji_id.zip(url).and_then(|(id, url)| {
                        let dest = crate::app_paths::asset_cache_dir()
                            .join("emotes")
                            .join("youtube")
                            .join(format!(
                                "{}.{}",
                                crate::downloader::sanitize_filename(id),
                                url_ext(url)
                            ));
                        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &dest) {
                            Some(dest)
                        } else {
                            fetches.push(EmojiFetch {
                                dest: dest.clone(),
                                urls: vec![url.to_string()],
                            });
                            pending = Some(dest);
                            None
                        }
                    });
                    segments.push(ChatSegment::Emote {
                        name: label.to_string(),
                        file,
                        fallback_text: None,
                        pending,
                    });
                } else {
                    // Standard unicode emoji: `emojiId` is the actual char(s) → route
                    // through the shared Twemoji emoji pipeline for colour.
                    let glyph = emoji_id.or(shortcut).unwrap_or("[emoji]");
                    segments.extend(emoji_split(glyph, fetches));
                }
            }
        }
    }
    let badges: Vec<String> = r["authorBadges"]
        .as_array()
        .map(|bs| {
            bs.iter()
                .filter_map(|b| {
                    b.pointer("/liveChatAuthorBadgeRenderer/tooltip")
                        .and_then(|t| t.as_str())
                        .map(|t| t.split('(').next().unwrap_or(t).trim().to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ChatMessage {
        timestamp_secs: ts_secs,
        author,
        text,
        segments,
        badges,
        color_override: None,
        platform: ChatPlatform::YouTube,
    })
}

/// Parse one line of a Twitch `.chat.jsonl` file. `start_ms` is the stream
/// start in unix milliseconds (timestamps become offsets from it).
pub(super) fn parse_twitch_chat_line(
    line: &str,
    start_ms: f64,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
    fetches: &mut Vec<EmojiFetch>,
) -> Option<ChatMessage> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ts_ms = v["ts"].as_f64().unwrap_or(0.0);
    let author = v["name"]
        .as_str()
        .or_else(|| v["login"].as_str())
        .unwrap_or("")
        .to_string();
    // Unwrap `/me` CTCP actions so the emote offsets (which index the inner
    // body) align and the raw control chars don't show in the replay/search.
    let text = strip_ctcp_action(v["text"].as_str().unwrap_or("")).to_string();
    let color_override = v["color"].as_str().and_then(parse_chat_hex_color);
    // Split raw badge tag "subscriber/12,moderator/1" into one entry per badge.
    let badges = v["badges"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_default();
    // `emotes` tag is absent on pre-feature logs → empty → first-party emotes
    // simply don't render (third-party word-matching still applies).
    let emotes_tag = v["emotes"].as_str().unwrap_or("");
    let segments = build_twitch_segments(&text, emotes_tag, emote_map, twitch_dir);
    // Split literal unicode emoji out of the text segments into colour images.
    let segments = expand_emoji(segments, fetches);
    Some(ChatMessage {
        timestamp_secs: (ts_ms - start_ms) / 1000.0,
        author,
        text,
        segments,
        badges,
        color_override,
        platform: ChatPlatform::Twitch,
    })
}

impl StreamArchiverApp {
    // ── Chat log viewer ──────────────────────────────────────────────────────

    /// Open the chat popup for a monitor. `rec_id` picks a specific recording
    /// (a take/stream row's "View chat"); `None` falls back to the most recent
    /// recording that has a chat file.
    pub(super) fn open_chat_popup(&mut self, monitor_id: i64, rec_id: Option<i64>, ctx: &egui::Context) {
        let row = self.rows.iter().find(|r| r.monitor.id == monitor_id);
        let monitor_name = row.map(|r| r.channel.name.clone()).unwrap_or_default();
        let platform = row.map(|r| r.monitor.platform());
        // The emote/badge cache is per-ACCOUNT: this monitor's URL names which
        // account's assets to use (a channel can hold a main + alt Twitch).
        let account = row
            .map(|r| asset_account(&r.monitor.url, r.monitor.platform()))
            .unwrap_or_default();
        // Twitch: build the third-party emote map (BTTV/FFZ/7TV) once and point at
        // the first-party emote dir. YouTube/others: empty map, no dir (emotes come
        // inline in the runs / aren't word-matched).
        let (emote_map, twitch_emote_dir) = if platform == Some(Platform::Twitch) {
            let dir = twitch_emotes_dir(&monitor_name, &account).join("twitch");
            (Arc::new(build_emote_map(&monitor_name, &account)), Some(dir))
        } else {
            (Arc::new(HashMap::new()), None)
        };

        let recs = self
            .core
            .store
            .recordings_for_monitor(monitor_id)
            .unwrap_or_default();
        let rec = rec_id
            .and_then(|id| recs.iter().find(|r| r.id == id))
            .or_else(|| recs.iter().rev().find(|r| chat_file_for_recording(r).is_some()))
            .or_else(|| recs.last())
            .cloned();

        let state = Arc::new(Mutex::new(ChatLoadState::Loading));
        let loading = Arc::new(AtomicBool::new(false));
        if let Some(r) = &rec {
            self.core.rt.spawn(load_chat(
                state.clone(),
                loading.clone(),
                chat_file_for_recording(r),
                r.went_live_at.unwrap_or(r.started_at),
                emote_map.clone(),
                twitch_emote_dir.clone(),
                self.render_emotes,
                ctx.clone(),
            ));
        } else {
            *state.lock().unwrap() = ChatLoadState::NoFile;
        }
        let popup = ChatPopup {
            monitor_id,
            monitor_name,
            recording: rec,
            all_recordings: recs,
            load_state: state,
            search: String::new(),
            full_view: false,
            last_reload: std::time::Instant::now(),
            emote_map,
            twitch_emote_dir,
            loading,
            error_retries: 0,
            filter_cache: None,
        };
        // One chat window per monitor: re-targeting an already-open window
        // (e.g. "View chat" on another take) replaces its content in place;
        // a different monitor gets its own window.
        match self.chat_popups.iter_mut().find(|p| p.monitor_id == monitor_id) {
            Some(slot) => *slot = popup,
            None => self.chat_popups.push(popup),
        }
    }

    #[allow(deprecated)]
    /// Render every open chat window (one OS viewport per monitor).
    pub(super) fn chat_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for idx in 0..self.chat_popups.len() {
            if self.chat_popup_window(ctx, idx) {
                closed.push(self.chat_popups[idx].monitor_id);
            }
        }
        if !closed.is_empty() {
            self.chat_popups.retain(|p| !closed.contains(&p.monitor_id));
            if self.chat_popups.is_empty() {
                // Free all decoded emote frame textures once the last chat
                // window is gone.
                self.clear_emote_cache();
            }
        }
    }

    /// Render one chat window; returns true when the user closed it.
    #[allow(deprecated)]
    pub(super) fn chat_popup_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        const CHAT_RELOAD_SECS: u64 = 3;
        let popup = &mut self.chat_popups[idx];
        // Watchdog: name this phase so a freeze dialog points at the chat popup.
        self.heartbeat.set_context(format!("Chat: {}", popup.monitor_name));
        self.heartbeat.set_activity(crate::watchdog::Activity::Chat);
        let mut open = true;
        let title = format!("💬  Chat — {}", popup.monitor_name);
        let vp_id = egui::ViewportId::from_hash_of(("chat_popup_vp", popup.monitor_id));

        // Whether the selected recording is still in progress (chat file is growing).
        let rec_active = popup.recording.as_ref().map_or(false, |r| r.ended_at.is_none());
        // An errored load retries with a FULL sidecar re-read — back that off
        // exponentially (3s → 6 → … → capped ~3min) instead of hammering the
        // recordings drive every tick. Loaded resets the ladder; NoFile stays
        // on the fast tick (retrying a missing file is one cheap stat, and the
        // sidecar usually appears seconds into a recording).
        let errored = matches!(&*popup.load_state.lock().unwrap(), ChatLoadState::Error(_));
        if !errored && matches!(&*popup.load_state.lock().unwrap(), ChatLoadState::Loaded(_)) {
            popup.error_retries = 0;
        }
        let reload_after = if errored {
            std::time::Duration::from_secs((CHAT_RELOAD_SECS << popup.error_retries.min(6)).min(180))
        } else {
            std::time::Duration::from_secs(CHAT_RELOAD_SECS)
        };
        // Collect everything needed for a tail-reload before the `show` closure
        // borrows `popup` so we can act on it cleanly afterwards.
        type ReloadInfo = (
            std::path::PathBuf,
            i64,
            Arc<Mutex<ChatLoadState>>,
            Arc<HashMap<String, std::path::PathBuf>>,
            Option<std::path::PathBuf>,
            Arc<AtomicBool>,
        );
        let reload_info: Option<ReloadInfo> =
            if rec_active && popup.last_reload.elapsed() >= reload_after {
                // Sidecar located via the probe cache: this runs on the UI
                // thread every 3s per live popup, and a direct stat against
                // the recordings drive can block the frame for seconds.
                let fs = &mut self.fs_probes;
                popup.recording.as_ref().and_then(|r| {
                    chat_file_for_recording_cached(fs, r).map(|path| {
                        (
                            path,
                            r.went_live_at.unwrap_or(r.started_at),
                            popup.load_state.clone(),
                            popup.emote_map.clone(),
                            popup.twitch_emote_dir.clone(),
                            popup.loading.clone(),
                        )
                    })
                })
            } else {
                None
            };

        // The emote cache is shared (Arc<Mutex>), so the closure can use a clone
        // without borrowing `self`. Copy the render toggles out too. `now` is the
        // global animation clock — all instances of an emote animate in lockstep.
        let anim_cache = self.emote_anim.clone();
        let render_emotes = self.render_emotes;
        let animate_emotes = self.animate_emotes;
        let now = ctx.input(|i| i.time);
        let mut decode_misses: Vec<std::path::PathBuf> = Vec::new();

        ctx.show_viewport_immediate(
            vp_id,
            egui::ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size([480.0, 600.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    // ── Toolbar ──────────────────────────────────────────────
                    ui.horizontal(|ui| {
                        // Recording picker: only if >1 recording has a chat file.
                        // Probe-cache lookups: this filter re-runs EVERY FRAME
                        // over the monitor's whole take history (4 candidate
                        // paths each) — direct stats here were measured in the
                        // thousands per second against the recordings drive.
                        let fs = &mut self.fs_probes;
                        let recs_with_chat: Vec<_> = popup
                            .all_recordings
                            .iter()
                            .filter(|r| chat_file_for_recording_cached(fs, r).is_some())
                            .collect();
                        if recs_with_chat.len() > 1 {
                            let cur_label = popup
                                .recording
                                .as_ref()
                                .map(fmt_recording_label)
                                .unwrap_or_default();
                            egui::ComboBox::from_id_salt("chat_rec_pick")
                                .selected_text(cur_label)
                                .show_ui(ui, |ui| {
                                    for rec in &recs_with_chat {
                                        let label = fmt_recording_label(rec);
                                        let selected = popup
                                            .recording
                                            .as_ref()
                                            .map(|r| r.id == rec.id)
                                            .unwrap_or(false);
                                        if ui.selectable_label(selected, &label).clicked() {
                                            let new_rec = (*rec).clone();
                                            let state = Arc::new(Mutex::new(ChatLoadState::Loading));
                                            let path = chat_file_for_recording(&new_rec);
                                            let start_ts =
                                                new_rec.went_live_at.unwrap_or(new_rec.started_at);
                                            let emap = popup.emote_map.clone();
                                            let tdir = popup.twitch_emote_dir.clone();
                                            popup.load_state = state.clone();
                                            popup.recording = Some(new_rec);
                                            popup.last_reload = std::time::Instant::now();
                                            // Keyed on (query, count) only — a
                                            // different log with the same count
                                            // would reuse stale match indices.
                                            popup.filter_cache = None;
                                            self.core.rt.spawn(load_chat(
                                                state,
                                                popup.loading.clone(),
                                                path,
                                                start_ts,
                                                emap,
                                                tdir,
                                                render_emotes,
                                                ctx.clone(),
                                            ));
                                        }
                                    }
                                });
                            ui.separator();
                        }

                        // Search filter
                        ui.label("🔍");
                        ui.add(
                            egui::TextEdit::singleline(&mut popup.search)
                                .hint_text("Filter…")
                                .desired_width(150.0),
                        );
                        if !popup.search.is_empty() && ui.small_button("✕").clicked() {
                            popup.search.clear();
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.toggle_value(&mut popup.full_view, "View full");
                        });
                    });
                    ui.separator();

                    // ── Content ──────────────────────────────────────────────
                    // Render straight from the mutex guard — the old code
                    // cloned the entire parsed log (every message + segments)
                    // every single frame.
                    let mut guard = popup.load_state.lock().unwrap();
                    match &mut *guard {
                        ChatLoadState::Loading => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Loading chat…");
                            });
                            ctx.request_repaint();
                        }
                        ChatLoadState::NoFile => {
                            ui.add_space(8.0);
                            ui.label("No chat file found for this recording.");
                            ui.weak("Chat logging must be enabled and a recording must exist.");
                        }
                        ChatLoadState::Error(e) => {
                            ui.colored_label(egui::Color32::RED, format!("Failed to load: {e}"));
                        }
                        ChatLoadState::Loaded(log) => {
                            // Keep the height cache aligned with the message
                            // list: tail appends get estimates at the end; a
                            // shrink (recording switch) resets everything.
                            let n = log.messages.len();
                            if log.row_heights.len() > n {
                                log.row_heights.clear();
                            }
                            log.row_heights.resize(n, CHAT_ROW_EST);

                            // Search filter, recomputed only when the query or
                            // the message count changes — not every frame.
                            let q = popup.search.to_lowercase();
                            if q.is_empty() {
                                popup.filter_cache = None;
                            } else {
                                let stale = popup
                                    .filter_cache
                                    .as_ref()
                                    .is_none_or(|(cq, cn, _)| *cq != q || *cn != n);
                                if stale {
                                    let idx: Vec<u32> = log
                                        .messages
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, m)| {
                                            m.text.to_lowercase().contains(&q)
                                                || m.author.to_lowercase().contains(&q)
                                        })
                                        .map(|(i, _)| i as u32)
                                        .collect();
                                    popup.filter_cache = Some((q.clone(), n, idx));
                                }
                            }
                            let filtered: Option<&[u32]> =
                                popup.filter_cache.as_ref().map(|(_, _, v)| v.as_slice());
                            let count = filtered.map_or(n, |v| v.len());

                            ui.horizontal(|ui| {
                                ui.weak(format!("{count} messages"));
                                if log.loading_older {
                                    ui.spinner();
                                    ui.weak("loading older messages…");
                                }
                            });

                            let stick = q.is_empty() && !popup.full_view;
                            const GAP: f32 = 2.0;
                            const OVERSCAN: f32 = 300.0;
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .stick_to_bottom(stick)
                                .show_viewport(ui, |ui, viewport| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    // Wrapping depends on width — a resize
                                    // re-measures everything.
                                    let w = ui.available_width();
                                    if (w - log.measured_width).abs() > 0.5 {
                                        log.measured_width = w;
                                        for h in &mut log.row_heights {
                                            *h = CHAT_ROW_EST;
                                        }
                                    }
                                    // One cheap pass over the cached heights
                                    // finds the on-screen window; only rows
                                    // within the viewport (± overscan) are
                                    // laid out — everything else is two
                                    // spacers, so a 6-hour log renders a few
                                    // dozen rows per frame, not all of them.
                                    // f64 accumulation: an f32 running sum
                                    // drifts past ~2M px (100k+ rows), which
                                    // desyncs offsets from rendered heights
                                    // and can retrigger repaints forever.
                                    let top = f64::from(viewport.min.y - OVERSCAN);
                                    let bottom = f64::from(viewport.max.y + OVERSCAN);
                                    let mut y = 0.0f64;
                                    let mut first = count;
                                    let mut offset = 0.0f64;
                                    let mut last = count;
                                    let mut last_y = 0.0f64;
                                    for di in 0..count {
                                        let mi = filtered.map_or(di, |v| v[di] as usize);
                                        let h = f64::from(log.row_heights[mi] + GAP);
                                        if first == count && y + h > top {
                                            first = di;
                                            offset = y;
                                        }
                                        if last == count && y > bottom {
                                            last = di;
                                            last_y = y;
                                        }
                                        y += h;
                                    }
                                    if last == count {
                                        last_y = y;
                                    }
                                    let total = y;
                                    ui.add_space(offset as f32);
                                    let mut mismeasured = false;
                                    for di in first..last {
                                        let mi = filtered.map_or(di, |v| v[di] as usize);
                                        let r = ui.scope(|ui| {
                                            render_chat_message(
                                                ui,
                                                &log.messages[mi],
                                                &anim_cache,
                                                render_emotes,
                                                animate_emotes,
                                                now,
                                                &mut decode_misses,
                                                ctx,
                                            );
                                        });
                                        let h = r.response.rect.height();
                                        if (h - log.row_heights[mi]).abs() > 0.5 {
                                            log.row_heights[mi] = h;
                                            mismeasured = true;
                                        }
                                        ui.add_space(GAP);
                                    }
                                    // Reserve the space of everything below
                                    // the rendered window so the scrollbar
                                    // spans the whole log.
                                    if total > last_y {
                                        ui.add_space((total - last_y) as f32);
                                    }
                                    if mismeasured {
                                        // Offsets were computed from estimates
                                        // — redo with real heights next frame.
                                        ctx.request_repaint();
                                    }
                                });
                        }
                    }
                });
                draw_alt_image_preview(ctx);
            },
        );
        // Decode any newly-seen emotes off the UI thread, then LRU-evict the cache.
        self.pump_emote_decodes(decode_misses, now, ctx);

        // Tail-reload: while the recording is live, parse only the bytes
        // appended since the last pass and push them onto the shown log —
        // the whole file is never re-read.
        if let Some((path, start_ts, state, emap, tdir, loading)) = reload_info {
            self.chat_popups[idx].last_reload = std::time::Instant::now();
            if errored {
                self.chat_popups[idx].error_retries =
                    self.chat_popups[idx].error_retries.saturating_add(1);
            }
            self.core.rt.spawn(tail_chat(
                state,
                loading,
                path,
                start_ts,
                emap,
                tdir,
                render_emotes,
                ctx.clone(),
            ));
        }
        // Keep the UI alive while a live recording is open so the next
        // interval check fires automatically.
        if rec_active {
            ctx.request_repaint_after(std::time::Duration::from_secs(CHAT_RELOAD_SECS));
        }

        !open
    }

    /// Drop all decoded emote frames and bump the epoch so any in-flight decode
    /// task skips its insert (poison-safe).
    pub(super) fn clear_emote_cache(&self) {
        self.emote_epoch.fetch_add(1, Ordering::SeqCst);
        self.emote_anim
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Decode newly-seen emotes off the UI thread, then enforce the LRU memory
    /// budget on the (now drawn) cache. Bounds how many decodes start per frame so
    /// opening a view with hundreds of distinct emotes doesn't spawn a blocking-
    /// thread storm; the over-cap ones revert to "unseen" and retry next frame. The
    /// epoch guard drops results whose cache was cleared (view closed / assets
    /// refetched) mid-decode. Shared by the chat replay popup and the emote viewer.
    pub(super) fn pump_emote_decodes(
        &self,
        mut decode_misses: Vec<std::path::PathBuf>,
        now: f64,
        ctx: &egui::Context,
    ) {
        // Watchdog: the decode/upload/evict sweep is the most texture-churning phase.
        self.heartbeat.set_activity(crate::watchdog::Activity::EmoteDecodePump);
        const MAX_DECODE_PER_FRAME: usize = 64;
        if decode_misses.len() > MAX_DECODE_PER_FRAME {
            let mut g = self.emote_anim.lock().unwrap_or_else(|e| e.into_inner());
            for path in &decode_misses[MAX_DECODE_PER_FRAME..] {
                g.remove(path);
            }
            decode_misses.truncate(MAX_DECODE_PER_FRAME);
        }
        let epoch = self.emote_epoch.load(Ordering::SeqCst);
        for path in decode_misses {
            let cache = self.emote_anim.clone();
            let epoch_at = self.emote_epoch.clone();
            let ctx2 = ctx.clone();
            self.core.rt.spawn_blocking(move || {
                let decoded = crate::iomon::fs::read_sync(crate::iomon::Cat::AssetCache, &path).ok().and_then(|b| crate::emote_anim::decode(&b));
                let entry = match decoded {
                    Some((imgs, delays)) => crate::emote_anim::EmoteLoad::Decoded(imgs, delays),
                    None => crate::emote_anim::EmoteLoad::Failed,
                };
                let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
                if epoch_at.load(Ordering::SeqCst) == epoch {
                    g.insert(path, entry);
                    drop(g);
                    ctx2.request_repaint();
                }
            });
        }
        evict_emote_cache(&self.emote_anim, now);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use std::path::PathBuf;

    // ----- first-party emote offset parsing (IRC `emotes` tag) -----

    #[test]
    fn first_party_spans_ascii_sorted_byte_ranges() {
        // "Kappa Keepo Kappa": cp ranges 0-4 / 6-10 / 12-16 == byte ranges (ASCII).
        let text = "Kappa Keepo Kappa";
        let spans = parse_first_party_spans(text, "25:0-4,12-16/1902:6-10");
        assert_eq!(
            spans,
            vec![
                (0, 5, "25".to_string()),
                (6, 11, "1902".to_string()),
                (12, 17, "25".to_string()),
            ]
        );
        for (b0, b1, _) in &spans {
            assert!(text.get(*b0..*b1).is_some());
        }
    }

    #[test]
    fn first_party_offsets_are_code_points_not_utf16_or_bytes() {
        // A leading astral emoji (😀 = 1 code point, 2 UTF-16 units, 4 bytes) before
        // the emote. Twitch counts code points, so "Kappa" is cp 2..=6.
        let text = "😀 Kappa";
        let spans = parse_first_party_spans(text, "25:2-6");
        assert_eq!(spans.len(), 1);
        let (b0, b1, id) = &spans[0];
        assert_eq!(id, "25");
        assert_eq!(text.get(*b0..*b1), Some("Kappa")); // not "appa"/"aKapp"/garbage
    }

    #[test]
    fn first_party_trailing_emote_reaches_end_of_string() {
        // Emote as the final token: end+1 is one-past-end → b1 must be text.len().
        let text = "gg Kappa";
        let spans = parse_first_party_spans(text, "25:3-7");
        assert_eq!(spans, vec![(3, 8, "25".to_string())]);
        assert_eq!(text.get(3..8), Some("Kappa"));
    }

    #[test]
    fn first_party_bails_on_overlap_reversed_or_oob() {
        // Overlapping spans → abort first-party entirely (empty).
        assert!(parse_first_party_spans("abcde", "25:0-2/1902:1-4").is_empty());
        // Reversed (end < start) → empty.
        assert!(parse_first_party_spans("abcde", "25:5-3").is_empty());
        // Out of range (end >= code-point count) → empty.
        assert!(parse_first_party_spans("hi", "25:0-5").is_empty());
        // Malformed → empty.
        assert!(parse_first_party_spans("hi", "garbage").is_empty());
        // Empty tag → empty.
        assert!(parse_first_party_spans("hi", "").is_empty());
    }

    // ----- third-party word matching -----

    fn emote(name: &str, file: &Option<PathBuf>) -> ChatSegment {
        ChatSegment::Emote { name: name.into(), file: file.clone(), fallback_text: None, pending: None }
    }

    #[test]
    fn word_match_is_case_sensitive_and_whole_token() {
        let mut map = HashMap::new();
        let p = PathBuf::from("/x/poggers.webp");
        map.insert("POGGERS".to_string(), p.clone());
        let segs = word_match_segments("hi POGGERS poggers POGGERSx", &map);
        // Only the exact, whole-token "POGGERS" matches; "poggers"/"POGGERSx" stay text.
        let emotes: Vec<_> = segs
            .iter()
            .filter(|s| matches!(s, ChatSegment::Emote { .. }))
            .collect();
        assert_eq!(emotes.len(), 1);
        assert!(matches!(&emotes[0], ChatSegment::Emote { name, .. } if name == "POGGERS"));
    }

    #[test]
    fn word_match_preserves_spacing_and_tabs() {
        let mut map = HashMap::new();
        map.insert("Kappa".to_string(), PathBuf::from("/x/k.png"));
        // Tab-separated tokens still match (Unicode-whitespace tokenization).
        let segs = word_match_segments("a\tKappa\tb", &map);
        // Reconstructing all text + emote names must round-trip the original.
        let mut rebuilt = String::new();
        for s in &segs {
            match s {
                ChatSegment::Text(t) => rebuilt.push_str(t),
                ChatSegment::Emote { name, .. } => rebuilt.push_str(name),
            }
        }
        assert_eq!(rebuilt, "a\tKappa\tb");
        assert!(segs.iter().any(|s| matches!(s, ChatSegment::Emote { name, .. } if name == "Kappa")));
    }

    #[test]
    fn empty_map_yields_single_text_segment() {
        let map: HashMap<String, PathBuf> = HashMap::new();
        let segs = word_match_segments("hello world", &map);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], ChatSegment::Text(t) if t == "hello world"));
    }

    #[test]
    fn strips_ctcp_action_wrapper() {
        // `/me` actions are unwrapped; the inner body is what emote offsets index.
        assert_eq!(strip_ctcp_action("\u{1}ACTION Kappa\u{1}"), "Kappa");
        assert_eq!(strip_ctcp_action("\u{1}ACTION \u{1}"), "");
        // Plain messages and malformed wrappers pass through untouched.
        assert_eq!(strip_ctcp_action("hello"), "hello");
        assert_eq!(strip_ctcp_action("\u{1}ACTION no-suffix"), "\u{1}ACTION no-suffix");
    }

    #[test]
    fn action_message_emote_offsets_align_after_strip() {
        // `/me Kappa` → stored `\x01ACTION Kappa\x01`; after stripping, offset 0-4
        // must land on "Kappa", not on the control-char-prefixed wrapper.
        let stripped = strip_ctcp_action("\u{1}ACTION Kappa\u{1}");
        let map = HashMap::new();
        let segs = build_twitch_segments(stripped, "25:0-4", &map, None);
        assert!(matches!(&segs[0], ChatSegment::Emote { name, .. } if name == "Kappa"));
    }

    // ----- username colour readability -----

    #[test]
    fn hsl_round_trips_within_one_step() {
        for c in [
            egui::Color32::from_rgb(0xFF, 0x00, 0x00),
            egui::Color32::from_rgb(0x00, 0x00, 0xFF),
            egui::Color32::from_rgb(0x8A, 0x2B, 0xE2),
            egui::Color32::from_rgb(0x12, 0x34, 0x56),
            egui::Color32::from_rgb(0x80, 0x80, 0x80),
        ] {
            let (h, s, l) = rgb_to_hsl(c);
            let back = hsl_to_rgb(h, s, l);
            // Allow ±1 per channel for rounding.
            assert!((c.r() as i32 - back.r() as i32).abs() <= 1);
            assert!((c.g() as i32 - back.g() as i32).abs() <= 1);
            assert!((c.b() as i32 - back.b() as i32).abs() <= 1);
        }
    }

    #[test]
    fn readable_color_lightens_dark_color_on_dark_bg() {
        let bg = egui::Color32::from_rgb(0x1e, 0x1e, 0x1e); // egui dark panel-ish
        let blue = egui::Color32::from_rgb(0x00, 0x00, 0xFF); // unreadable on dark
        assert!(contrast_ratio(blue, bg) < 4.0);
        let fixed = readable_color(blue, bg);
        assert!(contrast_ratio(fixed, bg) >= 4.0);
        // Hue stays blue-ish: blue channel remains the dominant one.
        assert!(fixed.b() > fixed.r() && fixed.b() > fixed.g());
    }

    #[test]
    fn readable_color_picks_reachable_direction_on_midtone_bg() {
        // On a mid-grey bg, lightening blue toward white never clears 4.0, but
        // darkening does — the direction must be chosen by reachable contrast.
        let bg = egui::Color32::from_gray(128);
        let blue = egui::Color32::from_rgb(0x00, 0x00, 0xFF);
        let fixed = readable_color(blue, bg);
        assert!(contrast_ratio(fixed, bg) >= 4.0);
    }

    #[test]
    fn readable_color_keeps_already_legible_color() {
        let bg = egui::Color32::from_rgb(0x1e, 0x1e, 0x1e);
        let coral = egui::Color32::from_rgb(0xFF, 0x7F, 0x50); // already high-contrast
        assert_eq!(readable_color(coral, bg), coral);
    }

    #[test]
    fn readable_color_darkens_pale_color_on_light_bg() {
        let bg = egui::Color32::WHITE;
        let pale = egui::Color32::from_rgb(0xFF, 0xFF, 0x00); // yellow: invisible on white
        assert!(contrast_ratio(pale, bg) < 4.0);
        let fixed = readable_color(pale, bg);
        assert!(contrast_ratio(fixed, bg) > contrast_ratio(pale, bg));
    }

    #[test]
    fn twitch_default_color_is_deterministic_per_name() {
        // Same name → same colour every time (Twitch's stable default assignment).
        assert_eq!(twitch_username_color("Kappa"), twitch_username_color("Kappa"));
    }
    #[test]
    fn build_twitch_combines_first_party_offset_and_thirdparty_word() {
        // First-party "Kappa" by offset (no file on disk → name fallback), and a
        // third-party "POGGERS" by word match in the trailing gap.
        let mut map = HashMap::new();
        let pog = PathBuf::from("/x/poggers.webp");
        map.insert("POGGERS".to_string(), pog.clone());
        // "Kappa POGGERS": Kappa at cp 0-4; POGGERS is the gap word.
        let segs = build_twitch_segments("Kappa POGGERS", "25:0-4", &map, None);
        // Expect: Emote(Kappa, None) then Text(" ") then Emote(POGGERS, Some).
        assert!(matches!(&segs[0], ChatSegment::Emote { name, file, .. } if name == "Kappa" && file.is_none()));
        assert!(segs.iter().any(|s| matches!(s, ChatSegment::Emote { name, file, .. } if name == "POGGERS" && file.as_ref() == Some(&pog))));
    }

    fn rec_with_output(path: &str) -> crate::models::Recording {
        crate::models::Recording {
            id: 1,
            monitor_id: 1,
            started_at: 0,
            ended_at: None,
            status: "recording".into(),
            bytes: 0,
            exit_code: None,
            output_path: path.into(),
            went_live_at: None,
            went_live_approx: false,
            lost_secs: None,
            stream_id: None,
            take_group: None,
            ad_count: 0,
            ad_secs: 0,
            meta_change_count: 0,
            title: String::new(),
            category: String::new(),
            log_excerpt: String::new(),
            notes: String::new(),
            vod_id: None,
            vod_state: None,
            vod_muted_secs: None,
            recovery_state: None,
            recovered_path: None,
            vod_dl_state: None,
            vod_dl_path: None,
            vod_dl_video_id: None,
            backfill_path: None,
            full_path: None,
            trigger_info: String::new(),
            head_backfill_state: String::new(),
            trigger_rule_json: String::new(),
        }
    }

    #[test]
    fn finds_youtube_live_chat_append_form() {
        // yt-dlp appends `.live_chat.json` to the -o value, so the sidecar keeps the
        // video extension: `<output_path>.live_chat.json`.
        let dir = std::env::temp_dir().join(format!("sa-chat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("clip.mkv");
        std::fs::write(format!("{}.live_chat.json", out.to_string_lossy()), "{}").unwrap();

        let found = chat_file_for_recording(&rec_with_output(&out.to_string_lossy()));
        assert_eq!(found.as_deref(), Some(out.with_extension("mkv.live_chat.json").as_path()));

        // Twitch native logger uses the extension-replace form.
        let tout = dir.join("vod.mkv");
        std::fs::write(tout.with_extension("chat.jsonl"), "{}").unwrap();
        let tfound = chat_file_for_recording(&rec_with_output(&tout.to_string_lossy()));
        assert_eq!(tfound.as_deref(), Some(tout.with_extension("chat.jsonl").as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
