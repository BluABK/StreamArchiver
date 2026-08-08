//! Username and badge colouring: Twitch's deterministic palette, the
//! YouTube badge palette, and the contrast lift that keeps either readable
//! against the panel background.

use super::*;

pub(in crate::ui) fn badge_display(badge: &str, platform: &ChatPlatform) -> (&'static str, egui::Color32) {
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
pub(in crate::ui) fn chat_username_color(msg: &ChatMessage, bg: egui::Color32) -> egui::Color32 {
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
pub(in crate::ui) fn twitch_username_color(name: &str) -> egui::Color32 {
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
pub(in crate::ui) fn youtube_username_color(badges: &[String]) -> egui::Color32 {
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
pub(in crate::ui) fn relative_luminance(c: egui::Color32) -> f32 {
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
pub(in crate::ui) fn contrast_ratio(a: egui::Color32, b: egui::Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Nudge `fg`'s lightness away from the background (lighter on a dark bg, darker on
/// a light bg) until it clears a contrast floor, preserving hue — the way Twitch
/// lightens dark name colours in dark mode so e.g. pure blue stays legible. Returns
/// `fg` unchanged when it's already comfortable.
pub(in crate::ui) fn readable_color(fg: egui::Color32, bg: egui::Color32) -> egui::Color32 {
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
pub(in crate::ui) fn rgb_to_hsl(c: egui::Color32) -> (f32, f32, f32) {
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
pub(in crate::ui) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> egui::Color32 {
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

pub(in crate::ui) fn parse_chat_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

/// `#RRGGBB` for `c`, ignoring alpha — inverse of [`parse_chat_hex_color`].
pub(in crate::ui) fn hex_color_string(c: egui::Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b())
}

/// Per-channel linear interpolation between two opaque colors, `t` in `0..=1`.
pub(in crate::ui) fn lerp_color32(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    egui::Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}
