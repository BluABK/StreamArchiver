//! Install OS fonts that cover non-Latin glyphs (CJK, Hangul, fullwidth `【】`,
//! emoji, historic scripts like Egyptian Hieroglyphs, etc.) as *fallbacks*
//! behind egui's bundled default.
//!
//! egui's default font is Latin-only, so channel names like Japanese VTuber names
//! (or `Nimi Nightmare【Phase Connect】`) — and the emoji chat viewers spam — otherwise
//! render as tofu boxes. We read a few fonts already present on the system and append
//! them after the defaults, so Latin text + the UI icon glyphs keep the default look
//! and only the missing glyphs fall through to these. Nothing is bundled into the
//! binary (keeps it lean); if none of the candidates exist we leave the defaults
//! untouched.
//!
//! Emoji caveat: egui's renderer rasterizes glyph *outlines* only — it ignores the
//! colour tables (COLR/CPAL, sbix, CBDT) in colour-emoji fonts. So emoji render
//! **monochrome** (the base outline) where the chosen font provides one, and stay
//! tofu where it only has a colour bitmap. Segoe UI Emoji (Windows) and Noto Emoji
//! (Linux) carry outlines and render mono; Apple Color Emoji does not, so macOS
//! falls back to the symbol fonts (partial coverage).

use std::sync::Arc;

use eframe::egui::{self, FontData, FontFamily};

/// Fallback font groups, in priority order. For each group we load the **first**
/// file that exists (the entries within a group are equivalent alternatives), so
/// we don't load several overlapping Japanese fonts. CJK collection files (`.ttc`)
/// load face 0 (the regular weight), which is what `FontData::from_owned` selects.
#[cfg(windows)]
const FONT_GROUPS: &[&[&str]] = &[
    // Japanese (kana + kanji + CJK punctuation) — primary for VTuber names.
    &[
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
    ],
    // Korean (Hangul).
    &[r"C:\Windows\Fonts\malgun.ttf"],
    // Simplified Chinese (Han).
    &[r"C:\Windows\Fonts\msyh.ttc"],
    // Emoji — Segoe UI Emoji's base glyphs are monochrome outlines the renderer can
    // rasterize (the COLR colour layers are ignored), so modern emoji show as B&W
    // silhouettes instead of tofu.
    &[r"C:\Windows\Fonts\seguiemj.ttf"],
    // Older emoji + dingbats/symbols Segoe UI Emoji may not cover — also
    // covers Braille Patterns (the U+2800 "blank" spacer trick) and Enclosed
    // Alphanumerics (①②③, Ⓐ Ⓑ), so those need no dedicated group.
    &[r"C:\Windows\Fonts\seguisym.ttf"],
    // Historic scripts (Egyptian Hieroglyphs, Cuneiform, Anatolian Hieroglyphs,
    // Old Italic, Old Persian, Ugaritic, ...) — stream titles occasionally use
    // these decoratively (e.g. 𓋼𓍊 𓆏 𓍊𓋼) and would otherwise render as tofu.
    &[r"C:\Windows\Fonts\seguihis.ttf"],
    // Mathematical Alphanumeric Symbols (U+1D400-U+1D7FF) — the "fancy text
    // generator" style people copy-paste into titles/usernames (𝓯𝓪𝓷𝓬𝔂,
    // 𝕓𝕠𝕝𝕕, 𝔤𝔬𝔱𝔥𝔦𝔠, 𝘪𝘵𝘢𝘭𝘪𝘤, 𝚖𝚘𝚗𝚘𝚜𝚙𝚊𝚌𝚎, ...). `cambria.ttc`'s face 0 (the plain
    // "Cambria" face this loader always uses) fully covers the block —
    // verified by inspecting its cmap; the separate "Cambria Math" face
    // (index 1) isn't needed.
    &[r"C:\Windows\Fonts\cambria.ttc"],
];

#[cfg(target_os = "macos")]
const FONT_GROUPS: &[&[&str]] = &[
    &[
        "/System/Library/Fonts/ヒラギノ角ゴシック W4.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ],
    &["/System/Library/Fonts/AppleSDGothicNeo.ttc"],
    &["/Library/Fonts/Arial Unicode.ttf"],
    // Emoji/symbols. Apple Color Emoji is an sbix colour bitmap with no outlines, so
    // the renderer draws nothing from it — use the outline symbol fonts instead
    // (monochrome, partial emoji coverage; better than tofu).
    &["/System/Library/Fonts/Apple Symbols.ttf"],
    &["/System/Library/Fonts/ZapfDingbats.ttf"],
];

#[cfg(all(unix, not(target_os = "macos")))]
const FONT_GROUPS: &[&[&str]] = &[
    &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    ],
    // Emoji — Noto Color Emoji is a CBDT colour bitmap (renders blank), so prefer the
    // monochrome-outline Noto Emoji, which the renderer rasterizes.
    &[
        "/usr/share/fonts/truetype/noto/NotoEmoji-Regular.ttf",
        "/usr/share/fonts/noto/NotoEmoji-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoEmoji-Regular.ttf",
    ],
    // Extra symbol/dingbat blocks.
    &[
        "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
    ],
    // Historic scripts (Egyptian Hieroglyphs etc.) — see the Windows group above.
    &[
        "/usr/share/fonts/truetype/noto/NotoSansEgyptianHieroglyphs-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansEgyptianHieroglyphs-Regular.ttf",
    ],
    // Mathematical Alphanumeric Symbols ("fancy text generator" styles) — see
    // the Windows Cambria group above. GNU FreeFont's FreeSerif has broad
    // coverage of this block (unverified on this platform, unlike the Windows
    // Cambria path — best effort, same as the rest of this Linux list).
    &[
        "/usr/share/fonts/truetype/freefont/FreeSerif.ttf",
        "/usr/share/fonts/gnu-free/FreeSerif.ttf",
    ],
];

/// Append available system CJK/Unicode fonts as fallbacks. No-op (keeps the egui
/// defaults) when none of the candidates are present.
pub fn install_unicode_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut added: Vec<String> = Vec::new();

    for group in FONT_GROUPS {
        for path in *group {
            match std::fs::read(path) {
                Ok(bytes) => {
                    let key = format!("sys:{path}");
                    fonts
                        .font_data
                        .insert(key.clone(), Arc::new(FontData::from_owned(bytes)));
                    added.push(key);
                    break; // first match in the group wins
                }
                Err(_) => continue,
            }
        }
    }

    if added.is_empty() {
        return;
    }

    // Fallbacks: keep the default font primary, try these only for missing glyphs.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for key in &added {
            list.push(key.clone());
        }
    }

    ctx.set_fonts(fonts);
    tracing::info!("installed {} fallback font(s) for non-Latin glyphs", added.len());
}
