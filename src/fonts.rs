//! Install OS fonts that cover non-Latin glyphs (CJK, Hangul, fullwidth `【】`,
//! etc.) as *fallbacks* behind egui's bundled default.
//!
//! egui's default font is Latin-only, so channel names like Japanese VTuber names
//! (or `Nimi Nightmare【Phase Connect】`) otherwise render as tofu boxes. We read a
//! few fonts already present on the system and append them after the defaults, so
//! Latin text + the UI icon glyphs keep the default look and only the missing
//! glyphs fall through to these. Nothing is bundled into the binary (keeps it lean);
//! if none of the candidates exist we simply leave the defaults untouched.

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
];

#[cfg(target_os = "macos")]
const FONT_GROUPS: &[&[&str]] = &[
    &[
        "/System/Library/Fonts/ヒラギノ角ゴシック W4.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ],
    &["/System/Library/Fonts/AppleSDGothicNeo.ttc"],
    &["/Library/Fonts/Arial Unicode.ttf"],
];

#[cfg(all(unix, not(target_os = "macos")))]
const FONT_GROUPS: &[&[&str]] = &[&[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
]];

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
