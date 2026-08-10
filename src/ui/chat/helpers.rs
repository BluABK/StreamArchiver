//! Chat asset lookups and segment building: cached name colours, emote/
//! badge directories, the code -> file emote map, and the Twitch `emotes`
//! tag -> [`ChatSegment`] conversion.

use super::*;

/// The Twitch broadcaster's chosen chat name colour for `name`'s `account`, if
/// the asset fetch cached one (`…/{name}/twitch/{account}/name_color.txt`, e.g.
/// `#9146FF`; legacy pre-account dir as fallback). `None` when the streamer set
/// no colour or assets haven't been fetched.
pub(in crate::ui) fn load_twitch_name_color(name: &str, account: &str) -> Option<egui::Color32> {
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
pub(in crate::ui) fn twitch_emotes_dir(name: &str, account: &str) -> std::path::PathBuf {
    let [primary, legacy] = crate::assets::asset_read_dirs(name, Platform::Twitch, account);
    let primary = primary.join("emotes");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &primary) {
        return primary;
    }
    let legacy = legacy.join("emotes");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &legacy) { legacy } else { primary }
}

/// Where a Twitch channel's chat badge icons live: a per-channel dir (sub/
/// VIP/broadcaster badges — earned relative to the channel being watched, so
/// always resolved against it, not a fallback index like third-party
/// emotes) plus the shared global dir (mod/staff/bits/etc, downloaded once
/// for all channels). Built ONCE per popup-open — see [`resolve_twitch_badge_icon`]'s
/// doc for why resolution happens at parse time, not render time.
pub(crate) struct TwitchBadgeDirs {
    pub(crate) channel: Option<std::path::PathBuf>,
    pub(crate) global: std::path::PathBuf,
}

/// The Twitch `badges/` dir for (channel, account), mirroring [`twitch_emotes_dir`]'s
/// account-dir-with-legacy-fallback resolution.
pub(in crate::ui) fn twitch_badge_dir(name: &str, account: &str) -> std::path::PathBuf {
    let [primary, legacy] = crate::assets::asset_read_dirs(name, Platform::Twitch, account);
    let primary = primary.join("badges");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &primary) {
        return primary;
    }
    let legacy = legacy.join("badges");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &legacy) { legacy } else { primary }
}

/// The shared global Twitch badge dir (mod/staff/turbo/bits/etc — not tied to
/// any one channel), matching `assets::fetch_twitch_badges`'s own path.
pub(crate) fn twitch_global_badge_dir() -> std::path::PathBuf {
    crate::app_paths::platform_assets_dir().join("twitch").join("global_badges")
}

/// Resolve a raw IRC badge entry (`"subscriber/12"`) to its cached icon file,
/// checking the channel-specific dir first (sub/VIP/broadcaster badges are
/// earned per-channel) then the shared global dir (mod/staff/bits/etc).
/// `None` when the set/version isn't cached (never fetched, still
/// downloading, or a YouTube badge — this is Twitch-only) — the caller falls
/// back to a text glyph. Resolved ONCE per message at parse time (like
/// first-party emotes in [`build_twitch_segments`]) rather than per frame, to
/// keep filesystem stats out of the render loop.
pub(in crate::ui) fn resolve_twitch_badge_icon(
    raw: &str,
    dirs: &TwitchBadgeDirs,
) -> Option<std::path::PathBuf> {
    let (set_id, version) = raw.split_once('/')?;
    for base in dirs.channel.iter().chain(std::iter::once(&dirs.global)) {
        let p = base.join(set_id).join(version).join("2x.png");
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &p) {
            return Some(p);
        }
    }
    None
}

/// Human-readable label for a raw badge entry's set id, used as hover text —
/// e.g. `"subscriber/12"` → `"Subscriber"`. Falls back to the raw set id
/// title-cased for anything not explicitly named.
pub(in crate::ui) fn badge_label(raw: &str) -> String {
    let set_id = raw.split('/').next().unwrap_or(raw);
    match set_id {
        "broadcaster" => "Broadcaster".to_string(),
        "moderator" => "Moderator".to_string(),
        "vip" => "VIP".to_string(),
        "subscriber" => "Subscriber".to_string(),
        "founder" => "Founder".to_string(),
        "bits" | "bits-leader" => "Bits".to_string(),
        "premium" => "Prime Gaming".to_string(),
        "partner" => "Verified Partner".to_string(),
        "staff" => "Twitch Staff".to_string(),
        "admin" => "Twitch Admin".to_string(),
        "turbo" => "Turbo".to_string(),
        _ => {
            let mut c = set_id.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Who supplies an emote. Ordering is display order in the picker.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(in crate::ui) enum EmoteSource {
    /// Twitch's own — the channel's sub emotes and your unlocked ones.
    Twitch,
    SevenTv,
    Bttv,
    Ffz,
}

impl EmoteSource {
    pub(in crate::ui) fn label(self) -> &'static str {
        match self {
            EmoteSource::Twitch => "Twitch",
            EmoteSource::SevenTv => "7TV",
            EmoteSource::Bttv => "BTTV",
            EmoteSource::Ffz => "FFZ",
        }
    }

    /// Each provider's own brand colour, for the small source tag in the
    /// `:code` suggestion popup — Twitch's own autocomplete colours these
    /// the same way rather than leaving every provider looking the same
    /// muted grey.
    pub(in crate::ui) fn color(self) -> egui::Color32 {
        match self {
            EmoteSource::Twitch => egui::Color32::from_rgb(0x91, 0x47, 0xff),
            EmoteSource::SevenTv => egui::Color32::from_rgb(0x4f, 0xc3, 0xf7),
            EmoteSource::Bttv => egui::Color32::from_rgb(0xf2, 0xa9, 0x22),
            EmoteSource::Ffz => egui::Color32::from_rgb(0x5c, 0xb8, 0x5c),
        }
    }
}

/// Which set an emote came from. Derived `Ord` sorts channel sets before
/// global ones, and within each, by provider — which is both the picker's
/// section order and [`emote_map_from_catalog`]'s precedence order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(in crate::ui) struct EmoteGroup {
    pub(in crate::ui) global: bool,
    pub(in crate::ui) source: EmoteSource,
}

impl EmoteGroup {
    /// Section heading in the picker.
    pub(in crate::ui) fn title(self, channel: &str) -> String {
        if self.global {
            format!("{} global emotes", self.source.label())
        } else if self.source == EmoteSource::Twitch {
            format!("{channel} — Twitch emotes")
        } else {
            format!("{channel} — {}", self.source.label())
        }
    }
}

/// One emote a chatter can type in this channel.
#[derive(Clone, Debug)]
pub(in crate::ui) struct CatalogEmote {
    pub(in crate::ui) code: String,
    pub(in crate::ui) path: std::path::PathBuf,
    pub(in crate::ui) group: EmoteGroup,
}

/// Every emote cached for this channel, grouped and in display order.
///
/// One pass over the manifests feeding both readers: the picker/autocomplete
/// need the grouping, and [`emote_map_from_catalog`] needs the flattened
/// code → image lookup. Building them separately would let the two drift on
/// which emotes exist.
pub(in crate::ui) fn build_emote_catalog(name: &str, account: &str) -> Vec<CatalogEmote> {
    use crate::assets::EmoteManifestEntry;
    let emotes_dir = twitch_emotes_dir(name, account);
    let plat = crate::app_paths::platform_assets_dir();
    let mut out: Vec<CatalogEmote> = Vec::new();

    let load = |path: std::path::PathBuf| -> Vec<EmoteManifestEntry> {
        crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    };
    let mut push = |entries: Vec<EmoteManifestEntry>,
                    group: EmoteGroup,
                    base_dir: &dyn Fn(&EmoteManifestEntry) -> std::path::PathBuf| {
        for e in entries {
            // Skip empty/whitespace-only codes (old name-less manifests, or odd
            // provider data) — they could never match a chat token anyway.
            if e.name.trim().is_empty() {
                continue;
            }
            // Resolves the current `{id}_{name}.{ext}` filename fetchers write,
            // falling back to the pre-rename `{id}.{ext}` form — see
            // `resolve_emote_path`'s doc for why this must stay in sync with
            // the fetchers instead of hardcoding one scheme here.
            let path = crate::assets::resolve_emote_path(&base_dir(&e), &e);
            if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &path) {
                out.push(CatalogEmote { code: e.name, path, group });
            }
        }
    };

    let chan = |source| EmoteGroup { global: false, source };
    let glob = |source| EmoteGroup { global: true, source };

    // Twitch first-party: the channel's own sub emotes, by code. Picker-only
    // — see `emote_map_from_catalog` for why these must NOT word-match here.
    let twitch_dir = emotes_dir.join("twitch");
    push(load(emotes_dir.join("twitch.json")), chan(EmoteSource::Twitch), &|_| {
        twitch_dir.clone()
    });
    // 7TV: always in the shared cache.
    push(load(emotes_dir.join("7tv.json")), chan(EmoteSource::SevenTv), &|_| {
        plat.join("7tv").join("emotes")
    });
    // BTTV: per-channel for channel emotes, shared for the rest.
    let bttv_channel = emotes_dir.join("bttv");
    let bttv_shared = plat.join("bttv").join("emotes");
    push(load(emotes_dir.join("bttv.json")), chan(EmoteSource::Bttv), &|e| {
        if e.shared { bttv_shared.clone() } else { bttv_channel.clone() }
    });
    // FFZ: always in the shared cache.
    push(load(emotes_dir.join("ffz.json")), chan(EmoteSource::Ffz), &|_| {
        plat.join("ffz").join("emotes")
    });

    // Each provider's GLOBAL set — the emotes every channel gets for free.
    // After the channel sets on purpose: `emote_map_from_catalog` is
    // first-wins, so a channel that aliases a global's code to its own emote
    // keeps its own, exactly as Twitch shows it. Images live in the same
    // shared per-provider cache the channel sets resolve into, so this only
    // adds names, never a second copy on disk.
    for (provider, source) in
        [("7tv", EmoteSource::SevenTv), ("bttv", EmoteSource::Bttv), ("ffz", EmoteSource::Ffz)]
    {
        let dir = plat.join(provider).join("emotes");
        push(load(crate::assets::global_emote_manifest(&plat, provider)), glob(source), &|_| {
            dir.clone()
        });
    }
    out
}

/// Chat's code → image lookup: what a bare word in a message renders as.
///
/// First-wins over [`build_emote_catalog`]'s order, so a channel's own emote
/// beats a global of the same code.
///
/// **Twitch first-party emotes are excluded on purpose.** Twitch tells us
/// exactly which ranges of a message are its own emotes, via the IRC `emotes`
/// tag, and those are rendered from that. Word-matching them by code as well
/// would render `Kappa` as a picture in messages where Twitch showed the
/// literal word — anyone can type another channel's sub-emote code without
/// being able to use it.
pub(in crate::ui) fn emote_map_from_catalog(
    catalog: &[CatalogEmote],
) -> HashMap<String, std::path::PathBuf> {
    let mut map: HashMap<String, std::path::PathBuf> = HashMap::new();
    for e in catalog {
        if e.group.source == EmoteSource::Twitch {
            continue;
        }
        map.entry(e.code.clone()).or_insert_with(|| e.path.clone());
    }
    map
}

/// Truncate a label to at most `max` characters, appending `…` when shortened.
/// Char-aware so it never splits a multi-byte UTF-8 emote code mid-codepoint.
pub(in crate::ui) fn truncate_label(s: &str, max: usize) -> String {
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
pub(in crate::ui) fn strip_ctcp_action(text: &str) -> &str {
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
pub(in crate::ui) fn build_twitch_segments(
    text: &str,
    emotes_tag: &str,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
    twitch_fallback_index: &HashMap<String, std::path::PathBuf>,
    fetch_unknown_emotes: bool,
    fetches: &mut Vec<EmojiFetch>,
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
        // Twitch lets any subscriber use their sub emotes in ANY channel's chat,
        // not just the one they subscribed to — an id missing from the open
        // channel's own dir may still be cached under a DIFFERENT channel's
        // (e.g. this app also archives that other streamer), so fall back to
        // every other cached channel's Twitch emote dir before giving up — via
        // a precomputed stem index (`twitch_fallback_index`), not a fresh
        // filesystem probe per occurrence (see its doc for why that mattered).
        let file = twitch_dir
            .and_then(|d| find_emote_file(d, &id, &name))
            .or_else(|| find_emote_fallback(twitch_fallback_index, &id, &name));
        // Still nothing local — the poster's home channel may not be
        // monitored/archived here at all. When enabled, queue an on-demand
        // fetch straight from Twitch's CDN by id (see `twitch_emote_cdn_fetch`)
        // and mark it pending; `upgrade_pending_emotes` promotes it to `file`
        // once the background download lands, same as a Unicode emoji glyph.
        let (file, pending) = if file.is_some() {
            (file, None)
        } else if fetch_unknown_emotes {
            let (dest, urls) = crate::assets::twitch_emote_cdn_fetch(&id, &name);
            if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &dest) {
                (Some(dest), None)
            } else if !crate::iomon::fs::exists_sync(
                crate::iomon::Cat::AssetCache,
                dest.with_extension("404"),
            ) {
                fetches.push(EmojiFetch { dest: dest.clone(), urls });
                (None, Some(dest))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        out.push(ChatSegment::Emote { name, file, fallback_text: None, pending });
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
pub(in crate::ui) fn parse_first_party_spans(text: &str, emotes_tag: &str) -> Vec<(usize, usize, String)> {
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
pub(in crate::ui) fn word_match_segments(
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
pub(in crate::ui) fn chat_file_for_recording(rec: &Recording) -> Option<std::path::PathBuf> {
    chat_file_candidates(rec).into_iter().find(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::ChatSidecar, p))
}

/// The candidate sidecar paths [`chat_file_for_recording`] probes, in order.
///
/// An explicit [`Recording::chat_path`] always comes first and, when set, is
/// the only candidate — persisted at spawn for every producer since the
/// dedicated chat-root feature. The derived fallbacks (legacy takes, plus
/// chat-root mirrors) live in [`crate::chat::chat_file_candidates`], shared
/// with the migration sweep.
pub(in crate::ui) fn chat_file_candidates(rec: &Recording) -> Vec<std::path::PathBuf> {
    crate::chat::chat_file_candidates(&rec.chat_path, &rec.output_path)
}

/// [`chat_file_for_recording`] for render paths: existence via the non-blocking
/// [`FsProbes`] cache, so per-frame callers (the chat popup's recording picker,
/// the Streams-grid context menus) never stat the recordings drive themselves.
/// Answers can lag a probe round-trip (~a frame) behind the direct version.
pub(in crate::ui) fn chat_file_for_recording_cached(
    fs: &mut FsProbes,
    rec: &Recording,
) -> Option<std::path::PathBuf> {
    chat_file_candidates(rec).into_iter().find(|p| fs.is_file(p))
}

pub(in crate::ui) fn fmt_recording_label(rec: &Recording) -> String {
    let dt = chrono::DateTime::from_timestamp(rec.started_at, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| rec.started_at.to_string());
    format!("{dt} ({})", rec.status)
}

pub(in crate::ui) fn fmt_chat_ts(secs: f64) -> String {
    if secs < 0.0 {
        return format!("-{}", fmt_chat_ts(-secs));
    }
    let s = secs as u64;
    format!("[{:02}:{:02}:{:02}]", s / 3600, (s % 3600) / 60, s % 60)
}
