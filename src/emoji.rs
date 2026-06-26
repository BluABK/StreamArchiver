//! Unicode-emoji detection + Twemoji asset mapping for the chat replay.
//!
//! egui's renderer draws glyph outlines only (no colour fonts), so colour emoji
//! must be rendered as images. We segment chat text into emoji vs non-emoji runs,
//! map each emoji grapheme cluster to a [Twemoji](https://github.com/jdecked/twemoji)
//! PNG filename, and the UI downloads/caches those on demand (CC-BY 4.0 — attributed
//! in Settings). Detection is deliberately *liberal*: a false positive just triggers
//! a Twemoji fetch that 404s and falls back to the Unicode glyph, while a false
//! negative leaves the glyph as today — both degrade gracefully.

/// Pinned Twemoji asset version (the maintained jdecked fork; Twitter's original
/// repo is archived). Stable URLs for offline-cacheable downloads.
const TWEMOJI_VER: &str = "15.1.0";

const ZWJ: u32 = 0x200D;
const VS16: u32 = 0xFE0F;

fn is_skin_tone(u: u32) -> bool {
    (0x1F3FB..=0x1F3FF).contains(&u)
}

fn is_modifier(u: u32) -> bool {
    u == VS16 || is_skin_tone(u) // VS16 or skin-tone modifier (extend, never start)
}

fn is_regional(u: u32) -> bool {
    (0x1F1E6..=0x1F1FF).contains(&u)
}

/// Whether `c` can *begin* an emoji cluster. Liberal over the emoji planes/blocks
/// but excludes the joiners/modifiers/regional indicators (those only ever *extend*
/// or *pair*, never start) so a lone skin-tone swatch or half a flag isn't coloured.
fn is_emoji_base(c: char) -> bool {
    let u = c as u32;
    if is_modifier(u) || is_regional(u) || u == ZWJ {
        return false;
    }
    matches!(u,
        0x1F000..=0x1FAFF        // main emoji planes (faces, hands, hearts, …)
        | 0x2600..=0x27BF        // misc symbols + dingbats (☀ ★ ✈ ✊ ✨ ❤ …)
        | 0x2B00..=0x2BFF        // misc symbols & arrows (⭐ ⬛ …)
        | 0x2300..=0x23FF        // misc technical (⌚ ⏰ ⏳ …)
        | 0x2122 | 0x2139        // ™ ℹ
        | 0x203C | 0x2049        // ‼ ⁉
        | 0x24C2                 // Ⓜ
        | 0x3030 | 0x303D | 0x3297 | 0x3299
        | 0x00A9 | 0x00AE        // © ®
        | 0x2934 | 0x2935
    )
}

/// Split `text` into `(slice, is_emoji)` runs, coalescing each emoji grapheme
/// cluster (base + VS16/skin-tone modifiers + ZWJ-joined continuations, or a
/// regional-indicator pair = flag) into ONE emoji slice. Non-emoji text accumulates
/// between them. Never panics: all slicing uses recorded char-boundary byte offsets.
pub fn segment(text: &str) -> Vec<(&str, bool)> {
    let mut out: Vec<(&str, bool)> = Vec::new();
    let cs: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0usize;
    let mut text_start = 0usize; // byte index where the current non-emoji run began
    while i < cs.len() {
        let (byte_i, c) = cs[i];
        let u = c as u32;
        // A regional indicator only starts a cluster as a complete pair (flag).
        let regional_pair =
            is_regional(u) && cs.get(i + 1).is_some_and(|&(_, n)| is_regional(n as u32));
        if !is_emoji_base(c) && !regional_pair {
            i += 1;
            continue;
        }
        // Flush any pending non-emoji text before this cluster.
        if byte_i > text_start {
            out.push((&text[text_start..byte_i], false));
        }
        let cluster_start = byte_i;
        let mut j = i + 1;
        if regional_pair {
            j = i + 2; // exactly two regional indicators
        } else {
            loop {
                let Some(&(_, nc)) = cs.get(j) else { break };
                let nu = nc as u32;
                if is_modifier(nu) {
                    j += 1;
                } else if nu == ZWJ {
                    // Join the next pictographic (+ its modifiers) if present.
                    if cs.get(j + 1).is_some_and(|&(_, a)| is_emoji_base(a)) {
                        j += 2;
                        continue;
                    }
                    j += 1; // dangling joiner: absorb it so it doesn't leak into text
                    break;
                } else {
                    break;
                }
            }
        }
        let cluster_end = cs.get(j).map(|&(b, _)| b).unwrap_or(text.len());
        out.push((&text[cluster_start..cluster_end], true));
        i = j;
        text_start = cluster_end;
    }
    if text_start < text.len() {
        out.push((&text[text_start..], false));
    }
    out
}

/// Lowercase-hex codepoints of a cluster joined by `-`, optionally dropping the
/// VS16 (`U+FE0F`) presentation selector.
fn codepoints(cluster: &str, drop_vs16: bool) -> String {
    cluster
        .chars()
        .map(|c| c as u32)
        .filter(|&u| !(drop_vs16 && u == VS16))
        .map(|u| format!("{u:x}"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Stable cache key (filename stem) for a cluster — the raw codepoints, so two
/// distinct clusters never collide and the key is independent of Twemoji's
/// irregular FE0F-dropping rules.
pub fn cache_key(cluster: &str) -> String {
    codepoints(cluster, false)
}

/// The Twemoji 72px PNG CDN URL for a codepoint filename stem.
pub fn twemoji_url(stem: &str) -> String {
    format!("https://cdn.jsdelivr.net/gh/jdecked/twemoji@{TWEMOJI_VER}/assets/72x72/{stem}.png")
}

/// Candidate Twemoji URLs to try for a cluster, in order. Twemoji's FE0F-drop rule
/// is irregular (depends on ZWJ presence), so we try the FE0F-kept name then the
/// FE0F-stripped name; the first that exists wins.
pub fn twemoji_url_candidates(cluster: &str) -> Vec<String> {
    let kept = codepoints(cluster, false);
    let stripped = codepoints(cluster, true);
    let mut v = vec![twemoji_url(&kept)];
    if stripped != kept {
        v.push(twemoji_url(&stripped));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_plain_text_as_one_run() {
        assert_eq!(segment("hello world"), vec![("hello world", false)]);
    }

    #[test]
    fn isolates_a_simple_emoji() {
        // 😀 = U+1F600 (one codepoint, 4 bytes).
        let segs = segment("hi 😀 yo");
        assert_eq!(segs, vec![("hi ", false), ("😀", true), (" yo", false)]);
    }

    #[test]
    fn coalesces_modifiers_zwj_and_flags() {
        // Waving hand + skin tone = one cluster.
        let wave = "\u{1F44B}\u{1F3FB}";
        assert_eq!(segment(wave), vec![(wave, true)]);
        // ZWJ sequence (man + ZWJ + computer) = one cluster.
        let zwj = "\u{1F468}\u{200D}\u{1F4BB}";
        assert_eq!(segment(zwj), vec![(zwj, true)]);
        // Flag = two regional indicators = one cluster.
        let flag = "\u{1F1FA}\u{1F1F8}";
        assert_eq!(segment(flag), vec![(flag, true)]);
    }

    #[test]
    fn url_candidates_try_fe0f_kept_then_stripped() {
        // ZWJ sequence with FE0F (rainbow flag): the FE0F-kept name is the real
        // Twemoji asset, so it must be tried first; stripped is the fallback.
        let c = twemoji_url_candidates("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}");
        assert!(c[0].ends_with("1f3f3-fe0f-200d-1f308.png"));
        assert!(c[1].ends_with("1f3f3-200d-1f308.png"));
        // Lone heart with VS16: both kept/stripped present as candidates.
        let h = twemoji_url_candidates("\u{2764}\u{FE0F}");
        assert!(h[0].ends_with("2764-fe0f.png"));
        assert!(h[1].ends_with("2764.png"));
        // Bare emoji: a single candidate.
        assert_eq!(twemoji_url_candidates("\u{1F600}").len(), 1);
    }

    #[test]
    fn cache_key_is_raw_codepoints() {
        assert_eq!(cache_key("\u{1F44B}\u{1F3FB}"), "1f44b-1f3fb");
        assert_eq!(cache_key("\u{2764}\u{FE0F}"), "2764-fe0f");
    }

    #[test]
    fn lone_modifier_or_half_flag_is_not_an_emoji_cluster() {
        // A bare skin-tone modifier is plain text, not its own emoji.
        assert_eq!(segment("\u{1F3FB}"), vec![("\u{1F3FB}", false)]);
        // A single trailing regional indicator after a flag stays text.
        let segs = segment("\u{1F1FA}\u{1F1F8}\u{1F1FA}");
        assert_eq!(segs[0], ("\u{1F1FA}\u{1F1F8}", true));
        assert_eq!(segs[1], ("\u{1F1FA}", false));
    }

    #[test]
    fn dangling_zwj_does_not_leak_into_text() {
        // man + ZWJ + non-emoji text: the ZWJ is absorbed into the emoji cluster.
        let segs = segment("\u{1F468}\u{200D}hi");
        assert_eq!(segs[0], ("\u{1F468}\u{200D}", true));
        assert_eq!(segs[1], ("hi", false));
    }
}
