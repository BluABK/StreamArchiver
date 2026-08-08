//! Font stack: the user's chosen UI and chat faces, plus OS fonts that cover
//! non-Latin glyphs (CJK, Hangul, fullwidth `【】`, emoji, historic scripts
//! like Egyptian Hieroglyphs, etc.) as *fallbacks* behind them.
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

/// The named egui family the chat replay renders in, so the chat can use a
/// different face from the rest of the app without either one losing the
/// non-Latin fallbacks.
pub const CHAT_FAMILY: &str = "chat";

/// Which system fonts the user picked, by display name (`""` = the egui
/// default). Kept as names rather than paths so the setting survives a font
/// being reinstalled to a different file.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct FontChoice {
    /// App-wide UI font.
    pub app: String,
    /// Chat replay font — see [`CHAT_FAMILY`].
    pub chat: String,
}

impl FontChoice {
    pub fn is_default(&self) -> bool {
        self.app.is_empty() && self.chat.is_empty()
    }
}

/// One installed font: what to show in the picker, and where its file is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemFont {
    pub display: String,
    pub path: std::path::PathBuf,
}

/// Strip the registry's type suffix from a font's registered name:
/// `"Segoe UI Semibold (TrueType)"` → `"Segoe UI Semibold"`.
fn clean_font_name(raw: &str) -> String {
    raw.rsplit_once(" (").map(|(name, _)| name).unwrap_or(raw).trim().to_string()
}

/// Fonts installed on this machine, for the pickers in Settings.
///
/// Reads the registry rather than enumerating with GDI, because
/// `FontData::from_owned` needs the font's **bytes from a file** —
/// `EnumFontFamiliesExW` yields family names with no path, and getting the
/// tables out of a selected `HFONT` via `GetFontData` is several times the
/// code for the same result. `HKLM` covers machine-wide installs, `HKCU`
/// per-user ones; a bare value is relative to the system font directory.
///
/// Enumerate once and cache — this is ~400 registry values plus a
/// `Path::exists` each, which is cheap but not free enough to run per frame.
#[cfg(windows)]
pub fn enumerate_system_fonts() -> Vec<SystemFont> {
    const SUBKEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts";
    let sysdir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("Fonts");
    let mut out: Vec<SystemFont> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for root in [windows_registry::LOCAL_MACHINE, windows_registry::CURRENT_USER] {
        let Ok(key) = root.open(SUBKEY) else { continue };
        let Ok(values) = key.values() else { continue };
        for (name, value) in values {
            // REG_SZ / REG_EXPAND_SZ only; anything else isn't a filename.
            let Ok(file) = String::try_from(value) else { continue };
            // Only what egui's font backend can actually parse.
            let ext = std::path::Path::new(&file)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !matches!(ext.as_str(), "ttf" | "ttc" | "otf") {
                continue;
            }
            let path = if std::path::Path::new(&file).is_absolute() {
                std::path::PathBuf::from(&file)
            } else {
                sysdir.join(&file)
            };
            let display = clean_font_name(&name);
            if display.is_empty() || !seen.insert(display.to_lowercase()) {
                continue;
            }
            if !crate::iomon::fs::exists_sync(crate::iomon::Cat::Startup, &path) {
                seen.remove(&display.to_lowercase());
                continue;
            }
            out.push(SystemFont { display, path });
        }
    }
    out.sort_by_key(|f| f.display.to_lowercase());
    out
}

#[cfg(not(windows))]
pub fn enumerate_system_fonts() -> Vec<SystemFont> {
    // No picker off Windows yet — the fallback list below still applies.
    Vec::new()
}

/// Install the fallback fonts, plus whichever faces the user picked.
///
/// Safe to call again at runtime: `ctx.set_fonts` rebuilds the atlas and
/// invalidates every cached galley, which is exactly what a font change
/// needs. It is NOT cheap, so the caller must only call it when the choice
/// actually changed — see `StreamArchiverApp::apply_font_settings`.
pub fn install_fonts(ctx: &egui::Context, choice: &FontChoice) {
    let mut fonts = egui::FontDefinitions::default();
    let mut added: Vec<String> = Vec::new();

    for group in FONT_GROUPS {
        for path in *group {
            match crate::iomon::fs::read_sync(crate::iomon::Cat::Startup, path) {
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

    // Resolve the user's picks to loaded font keys. A name that no longer
    // resolves (font uninstalled) silently falls back to the default rather
    // than leaving the app unusable.
    let installed = (!choice.is_default())
        .then(enumerate_system_fonts)
        .unwrap_or_default();
    let mut load_pick = |name: &str| -> Option<String> {
        let font = installed.iter().find(|f| f.display.eq_ignore_ascii_case(name))?;
        let key = format!("user:{}", font.path.display());
        if !fonts.font_data.contains_key(&key) {
            let bytes = crate::iomon::fs::read_sync(crate::iomon::Cat::Startup, &font.path)
                .map_err(|e| tracing::warn!("font {name:?}: {e}"))
                .ok()?;
            fonts.font_data.insert(key.clone(), Arc::new(FontData::from_owned(bytes)));
        }
        Some(key)
    };
    let app_pick = (!choice.app.is_empty()).then(|| load_pick(&choice.app)).flatten();
    let chat_pick = (!choice.chat.is_empty()).then(|| load_pick(&choice.chat)).flatten();

    // The chat family is the user's chat face (if any), then everything the
    // proportional family has — so a chat font with no CJK still renders a
    // Japanese name, and the UI icon glyphs egui bundles stay reachable.
    let mut chat_list: Vec<String> = Vec::new();
    if let Some(k) = &chat_pick {
        chat_list.push(k.clone());
    }
    chat_list.extend(
        fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default(),
    );

    // The user's app font goes in FRONT of egui's default rather than
    // replacing it: the default carries the UI icon glyphs still used outside
    // the chat window, so dropping it would leave tofu all over Settings.
    if let Some(k) = &app_pick {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, k.clone());
    }

    // Fallbacks: keep the primary font primary, try these only for missing glyphs.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for key in &added {
            list.push(key.clone());
        }
    }
    chat_list.extend(added.iter().cloned());
    chat_list.dedup();
    fonts.families.insert(FontFamily::Name(CHAT_FAMILY.into()), chat_list);

    ctx.set_fonts(fonts);
    tracing::info!(
        app = %choice.app,
        chat = %choice.chat,
        "installed fonts ({} non-Latin fallback(s))",
        added.len()
    );
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The registry stores a font's name with its type in parentheses. The
    /// picker shows and stores the cleaned name, so this is what a saved
    /// setting is matched against on the next launch.
    #[test]
    fn font_names_lose_their_registry_type_suffix() {
        assert_eq!(clean_font_name("Segoe UI (TrueType)"), "Segoe UI");
        assert_eq!(clean_font_name("Yu Gothic Medium & Yu Gothic UI (TrueType)"),
                   "Yu Gothic Medium & Yu Gothic UI");
        assert_eq!(clean_font_name("Cambria & Cambria Math (TrueType)"), "Cambria & Cambria Math");
        // A name with no suffix passes through untouched.
        assert_eq!(clean_font_name("Arial"), "Arial");
        // …including one that happens to contain a parenthesis mid-name.
        assert_eq!(clean_font_name("Foo(Bar)"), "Foo(Bar)");
    }

    #[test]
    fn a_default_choice_is_recognised_as_such() {
        assert!(FontChoice::default().is_default());
        assert!(!FontChoice { app: "Arial".into(), chat: String::new() }.is_default());
        assert!(!FontChoice { app: String::new(), chat: "Arial".into() }.is_default());
    }
}
