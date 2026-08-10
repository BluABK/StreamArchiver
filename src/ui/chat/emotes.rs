//! Emote and emoji rendering: the path -> texture LRU cache and its
//! eviction, the CDN/clipboard helpers, the emote viewer grid, and emoji
//! segment splitting.

use super::*;

/// Soft cap on decoded emote-frame GPU memory; the cache is LRU-evicted past this.
pub(in crate::ui) const EMOTE_BUDGET_BYTES: usize = 192 * 1024 * 1024;

/// CDN URL for an emote given provider, id, and extension.
pub(in crate::ui) fn emote_cdn_url(provider: EmoteProvider, id: &str, ext: &str) -> String {
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
pub(in crate::ui) fn copy_emote_image_to_clipboard(path: &std::path::Path) {
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
pub(in crate::ui) fn emote_viewer_grid(
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
                        let r =
                            draw_cached_emote(ui, cache, &e.path, animate, IMG_H, None, now, misses, ctx);
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
                    if let Some((resp, _)) = img_resp.clone() {
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
                                    ui, cache, &epath, false, 160.0, None, now,
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

/// A decoded image at least this many times wider than tall is treated as a
/// "wide" emote (7TV's walk-cycle/banner-style emotes are commonly 2-4:1) —
/// see [`draw_cached_emote`]'s `wide` parameter for why that distinction
/// exists at all.
pub(in crate::ui) const WIDE_EMOTE_ASPECT_THRESHOLD: f32 = 1.5;

/// The on-screen size to draw a decoded emote at, given its native
/// (already-downscaled-to-≤56px) size. Pure and separate from
/// [`draw_cached_emote`] so the "a wide emote's HEIGHT gets crushed by the
/// width cap" fix is directly testable without a live `egui::Ui`.
///
/// Height ≤ the chosen target, width capped at the chosen max, aspect
/// preserved. Never upscales (`.min(1.0)`) — a small emote keeps its native
/// size, matching the prior loader behaviour.
fn emote_draw_size(native: egui::Vec2, emote_h: f32, wide: Option<(f32, f32)>) -> egui::Vec2 {
    let (target_h, max_w) = match (wide, native.x > native.y * WIDE_EMOTE_ASPECT_THRESHOLD) {
        (Some((wide_h, wide_max_w)), true) => (wide_h, wide_max_w),
        _ => (emote_h, 112.0),
    };
    let scale = (target_h / native.y.max(1.0)).min(max_w / native.x.max(1.0)).min(1.0);
    native * scale
}

/// Draw an emote from the decode cache. Returns the image `Response` when drawn, or
/// `None` (caller shows the text fallback) when the emote is still loading / failed.
/// Promotes a freshly-decoded entry to GPU textures (UI-thread upload), advances
/// the animation against the global clock `now`, and records `last_drawn` for LRU.
#[allow(clippy::too_many_arguments)]
pub(in crate::ui) fn draw_cached_emote(
    ui: &mut egui::Ui,
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    path: &Path,
    animate: bool,
    emote_h: f32,
    // `(target height, max width)` to use INSTEAD of `emote_h`/112px when
    // the decoded image clears `WIDE_EMOTE_ASPECT_THRESHOLD` — `None` for
    // every caller that doesn't distinguish (badges, the picker grid, the
    // usercard preview), which keeps their behaviour exactly as before.
    //
    // Why this exists: `emote_h` alone isn't enough for a genuinely wide
    // emote. The old single-cap formula was `min(emote_h / height,
    // 112px / width, 1.0)` — for a wide-aspect image that 112px width cap
    // routinely binds BEFORE the emote reaches `emote_h` tall at all, so a
    // 500×80px source at emote_h=24 came out ~112×18px: both dimensions
    // shrunk below the configured size, not just clipped narrower. A
    // separate, more generous cap for the wide case lets it actually reach
    // its own configured height instead.
    wide: Option<(f32, f32)>,
    now: f64,
    misses: &mut Vec<std::path::PathBuf>,
    ctx: &egui::Context,
) -> Option<(egui::Response, egui::TextureHandle)> {
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
            let size = emote_draw_size(s, emote_h, wide);
            if animate && anim.is_animated() {
                let (tex, remaining) = anim.frame_at(now);
                let tex = tex.clone();
                let resp = ui.add(
                    egui::Image::from_texture(&tex)
                        .fit_to_exact_size(size)
                        .sense(egui::Sense::click()),
                );
                // Only schedule the next frame for emotes actually on screen, so a
                // scrolled-away animation doesn't keep waking the UI.
                if ui.is_rect_visible(resp.rect) {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f32(
                        remaining.clamp(MIN_ANIM_REPAINT_SECS, 1.0),
                    ));
                }
                Some((resp, tex))
            } else {
                let (tex, _) = anim.frame_at(0.0);
                let tex = tex.clone();
                let resp = ui.add(
                    egui::Image::from_texture(&tex)
                        .fit_to_exact_size(size)
                        .sense(egui::Sense::click()),
                );
                Some((resp, tex))
            }
        }
    }
}

/// Floor on how often an animated emote may ask its viewport to repaint (30 fps).
/// Same hazard, and the same reasoning, as [`throttled_spinner`].
///
/// `emote_anim::MIN_DELAY` lets a frame's `remaining` come back as low
/// as 20 ms, and a popup asking for ~20 ms repaints saturates eframe's event
/// loop: it flips to `ControlFlow::Poll` and the **root** viewport stops being
/// serviced entirely — measured with `examples/vp_repaint_probe.rs` as root
/// 0 passes/s while the child ran at 165/s (its own request rate was ignored;
/// it just free-ran). That starves the main window's 1 Hz heartbeat, which is
/// also what the UI-freeze watchdog beats on, so a single animating emote could
/// both freeze the main window and trip a false "UI frozen" alarm.
///
/// The cliff is sharp — 25 ms already leaves the root alive — so 1/30 s is a
/// comfortable margin. It only rate-limits *rendering*: the animation is
/// sampled against wall-clock time, so it stays in sync and merely drops the
/// odd frame on emotes whose real delay is shorter than this (rare — Twitch/7TV
/// emotes are typically 30–100 ms per frame).
const MIN_ANIM_REPAINT_SECS: f32 = 1.0 / 30.0;

/// How recently an emote must have been drawn to be exempt from budget
/// eviction. See the comment at the check itself for why this is a window
/// rather than an exact "this frame" compare.
const RECENT_DRAW_SECS: f64 = 1.0;

/// Evict the least-recently-drawn ready emotes once the decoded-frame cache exceeds
/// [`EMOTE_BUDGET_BYTES`]. Emotes drawn in the last [`RECENT_DRAW_SECS`] are kept.
pub(in crate::ui) fn evict_emote_cache(
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
        // Keep anything drawn in the last second rather than requiring
        // `last_drawn >= now`: `last_drawn` is now stamped by the *popup*
        // viewport's clock while this sweep runs on the root's, so the two no
        // longer land on the identical value and an exact compare would evict
        // emotes that are on screen right now (forcing an immediate re-decode).
        if now - last_drawn < RECENT_DRAW_SECS {
            continue; // visible this frame — keep
        }
        g.remove(&k);
        cur -= bytes;
    }
}

/// First existing first-party Twitch emote image for `id` in `dir`, trying the
/// formats Twitch uses (static `.png`, animated `.gif`) plus `.webp`, and —
/// per extension — the current `{id}_{name}.{ext}` filename the fetcher
/// writes before falling back to the pre-rename `{id}.{ext}` form. `None`
/// when none exist.
pub(in crate::ui) fn find_emote_file(dir: &Path, id: &str, name: &str) -> Option<std::path::PathBuf> {
    let sanitized = crate::assets::sanitize_emote_name(name);
    ["png", "gif", "webp"].iter().find_map(|ext| {
        let new_path = dir.join(format!("{id}_{sanitized}.{ext}"));
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &new_path) {
            return Some(new_path);
        }
        let old_path = dir.join(format!("{id}.{ext}"));
        crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &old_path).then_some(old_path)
    })
}

/// Look up `id`/`name` in a precomputed cross-channel stem index (see
/// `assets::index_emote_stems`) — an O(1) hashmap probe of both filename
/// forms instead of a filesystem stat, since this runs once per first-party
/// emote OCCURRENCE (a chat log routinely repeats the same emote hundreds of
/// times) times however many other channels are archived.
pub(in crate::ui) fn find_emote_fallback(
    index: &HashMap<String, std::path::PathBuf>,
    id: &str,
    name: &str,
) -> Option<std::path::PathBuf> {
    let sanitized = crate::assets::sanitize_emote_name(name);
    index
        .get(&format!("{id}_{sanitized}"))
        .or_else(|| index.get(id))
        .cloned()
}

/// An emoji image not yet on disk that the renderer would otherwise show as a
/// glyph. Collected during parse; the popup tries each `url` in order (Twemoji's
/// FE0F naming is irregular) and writes the first that succeeds to `dest`.
/// `pub(crate)`: also built/consumed by the "Fetch missing chat emotes"
/// maintenance sweep (`downloader::supervisor::cmd_fetch_missing_chat_emotes`),
/// which reuses this same struct + `download_emoji_images` rather than a
/// parallel download mechanism.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EmojiFetch {
    pub(crate) dest: std::path::PathBuf,
    pub(crate) urls: Vec<String>,
}

/// One parsed slice of a chat file: the messages, the emoji images to fetch,
/// and the byte offset just past the last complete line — the resume point for
/// the next incremental pass. `pub(crate)` for the same reason as `EmojiFetch`.
pub(crate) struct ChatChunk {
    // `ChatMessage`/`MarkerAt` stay UI-internal (`pub(super)`) — the
    // maintenance sweep this struct is also shared with only ever reads
    // `fetches`/`parsed_to`, never these.
    pub(in crate::ui) messages: Vec<ChatMessage>,
    pub(crate) fetches: Vec<EmojiFetch>,
    pub(crate) parsed_to: u64,
    /// Moderation markers found in this byte range — Twitch marker lines
    /// written live by our own logger, or YouTube's own deletion actions.
    pub(in crate::ui) markers: Vec<MarkerAt>,
}

/// Split a text run into [`ChatSegment`]s, turning each Unicode-emoji cluster into
/// an `Emote` that resolves to a cached Twemoji image (with the glyph as fallback),
/// and recording any not-yet-downloaded image in `fetches`. Plain text passes
/// through unchanged (fast path).
pub(in crate::ui) fn emoji_split(text: &str, fetches: &mut Vec<EmojiFetch>) -> Vec<ChatSegment> {
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
pub(in crate::ui) fn emoji_cache_dir() -> std::path::PathBuf {
    crate::app_paths::asset_cache_dir()
        .join("emotes")
        .join("emoji")
}

/// Expand the `Text` segments of an already-built segment list, splitting out any
/// Unicode emoji into image segments. Emote segments are left untouched.
pub(in crate::ui) fn expand_emoji(segments: Vec<ChatSegment>, fetches: &mut Vec<EmojiFetch>) -> Vec<ChatSegment> {
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
pub(in crate::ui) fn url_ext(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .and_then(|p| p.rsplit('.').next())
        .filter(|e| matches!(*e, "png" | "gif" | "webp" | "jpg" | "jpeg"))
        .unwrap_or("png")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A roughly-square emote isn't affected by `wide` at all, `Some` or not
    /// — it only ever applies to a genuinely wide-aspect image.
    #[test]
    fn a_square_emote_ignores_the_wide_allowance() {
        let native = egui::vec2(56.0, 56.0);
        let plain = emote_draw_size(native, 24.0, None);
        let with_wide = emote_draw_size(native, 24.0, Some((48.0, 400.0)));
        assert_eq!(plain, with_wide);
        assert_eq!(plain, egui::vec2(24.0, 24.0));
    }

    /// The bug this was written to fix: a wide-aspect image, sized by the
    /// regular path (`wide: None`, or a caller that doesn't clear the
    /// threshold), gets its HEIGHT crushed well below the configured
    /// target because the flat 112px width cap binds first.
    #[test]
    fn without_the_wide_allowance_a_wide_emote_is_crushed_short() {
        let native = egui::vec2(500.0, 80.0); // 6.25:1
        let size = emote_draw_size(native, 24.0, None);
        // Naively scaling to 24pt tall would need 500 * (24/80) = 150px
        // wide, over the 112px cap — so the cap binds and the emote comes
        // out well under 24px tall too, not just narrower.
        assert!(size.y < 20.0, "expected the height to be crushed, got {size:?}");
        assert!(size.x <= 112.0);
    }

    /// The fix: given a wide-specific target + a correspondingly generous
    /// max width, a realistically-proportioned wide emote (7TV's
    /// walk-cycle/banner style — up to ~4:1) reaches its OWN configured
    /// height instead of being cut short by the regular 112px cap.
    #[test]
    fn the_wide_allowance_lets_a_wide_emote_reach_its_own_target_height() {
        let native = egui::vec2(320.0, 80.0); // 4:1
        let size = emote_draw_size(native, 24.0, Some((24.0, 24.0 * 6.0)));
        assert_eq!(size.y, 24.0, "reaches the wide target height exactly");
        assert_eq!(size.x, 320.0 * (24.0 / 80.0));
    }

    /// Even the generous wide max-width is still a cap, not a blank
    /// cheque — an extreme outlier aspect ratio still gets bounded rather
    /// than being allowed to stretch across the whole row.
    #[test]
    fn the_wide_max_width_still_caps_an_extreme_outlier() {
        let native = egui::vec2(500.0, 80.0); // 6.25:1
        let size = emote_draw_size(native, 24.0, Some((24.0, 24.0 * 6.0)));
        assert_eq!(size.x, 24.0 * 6.0, "width cap binds");
        assert!(size.y < 24.0, "so the height comes in a little under target");
    }

    /// A small emote is never upscaled, wide or not — matches the
    /// pre-existing (non-wide) behaviour.
    #[test]
    fn a_small_emote_is_never_upscaled() {
        let native = egui::vec2(200.0, 20.0); // 10:1, but tiny
        let size = emote_draw_size(native, 64.0, Some((64.0, 400.0)));
        assert_eq!(size, native, "already smaller than every target — kept as-is");
    }

    #[test]
    fn url_ext_reads_the_extension_and_ignores_query_strings() {
        assert_eq!(url_ext("https://cdn.example/e.webp?v=2"), "webp");
        assert_eq!(url_ext("https://cdn.example/e.gif#frag"), "gif");
        assert_eq!(url_ext("https://cdn.example/e.bogus"), "png");
        assert_eq!(url_ext("https://cdn.example/e"), "png");
    }
}
