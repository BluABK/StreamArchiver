//! Writing a message: the emote picker and `:code` autocomplete.
//!
//! Both read the one [`build_emote_catalog`] built when the window opened, so
//! what you can pick is exactly what the replay can render — with the single
//! documented exception of Twitch's first-party emotes, which the picker
//! offers (you can type them) but the replay resolves from Twitch's own tags
//! rather than by word-matching the code.

use super::*;

/// Shortest `:query` that opens the autocomplete.
///
/// Two, so the common one-character emoticons people actually type — `:)`,
/// `:D`, `:P`, `:3` — never pop a list over the chat they're replying to.
pub(in crate::ui) const MIN_COMPLETE_CHARS: usize = 2;

/// How many suggestions to offer. Twitch shows a handful; a long list is
/// slower to scan than typing another character.
pub(in crate::ui) const MAX_COMPLETIONS: usize = 8;

/// The `:partial` emote token immediately before the caret.
///
/// Returns the **character** range to replace (including the `:`) and the
/// query. `None` when there is no such token — which is the common case on
/// every keystroke, so this stays allocation-free until it actually matches.
///
/// The colon must start a word: `10:30` and a URL's `https://` must not open
/// an emote list, and neither should the second colon of a completed `:code:`.
pub(in crate::ui) fn emote_token(
    text: &str,
    caret: usize,
) -> Option<(std::ops::Range<usize>, String)> {
    let chars: Vec<char> = text.chars().collect();
    let caret = caret.min(chars.len());
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = caret;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    // Need a ':' immediately before the word, itself at a word start.
    let colon = start.checked_sub(1)?;
    if chars[colon] != ':' {
        return None;
    }
    if colon > 0 && (is_word(chars[colon - 1]) || chars[colon - 1] == ':') {
        return None;
    }
    if caret - start < MIN_COMPLETE_CHARS {
        return None;
    }
    Some((colon..caret, chars[start..caret].iter().collect()))
}

/// Replace a character range of `text` with `code` plus a trailing space, and
/// return the new caret position (in characters).
///
/// The trailing space is what makes the completion usable mid-sentence — and
/// it's also what Twitch does. One is added even if the next character is
/// already a space, because the caret then sits between the two rather than
/// before an existing one, which reads the same and keeps this reversible
/// with a single backspace.
pub(in crate::ui) fn apply_completion(
    text: &str,
    range: std::ops::Range<usize>,
    code: &str,
) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let head: String = chars[..range.start].iter().collect();
    let tail: String = chars[range.end.min(chars.len())..].iter().collect();
    let caret = range.start + code.chars().count() + 1;
    (format!("{head}{code} {tail}"), caret)
}

/// Put the message box's caret at `at` (characters) and focus it.
///
/// Needed after every programmatic edit of the draft. egui keeps the caret in
/// the widget's own stored state, so text inserted underneath it leaves the
/// caret where it was — type after picking an emote and the next character
/// lands in the middle of the code you just inserted.
pub(in crate::ui) fn set_draft_caret(ctx: &egui::Context, id: egui::Id, at: usize) {
    if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
        st.cursor
            .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(at))));
        st.store(ctx, id);
    }
    ctx.memory_mut(|m| m.request_focus(id));
}

/// Emotes matching `query`, best first, deduplicated by code.
///
/// Prefix matches rank above substring ones (typing `spin` wants
/// `spinCat` before `laynaSpinFast`), and shorter codes above longer within
/// each tier — the shorter one is more likely to be the exact thing meant.
/// Ties fall back to the catalogue's own order, which is channel sets before
/// globals, so a channel's own emote wins a straight tie.
pub(in crate::ui) fn rank_emote_matches<'a>(
    query: &str,
    catalog: &'a [CatalogEmote],
    limit: usize,
) -> Vec<&'a CatalogEmote> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    let mut hits: Vec<(bool, usize, usize, &CatalogEmote)> = Vec::new();
    for (i, e) in catalog.iter().enumerate() {
        let code = e.code.to_lowercase();
        let prefix = code.starts_with(&q);
        if prefix || code.contains(&q) {
            hits.push((!prefix, e.code.chars().count(), i, e));
        }
    }
    hits.sort_by_key(|&(substring, len, idx, _)| (substring, len, idx));
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    hits.into_iter()
        .filter(|(.., e)| seen.insert(e.code.as_str()))
        .map(|(.., e)| e)
        .take(limit)
        .collect()
}

/// One row of the emote picker's virtualized list.
enum PickerRow {
    /// A section heading, e.g. "7TV global emotes".
    Header(String),
    /// Indices into the filtered list, one row's worth.
    Emotes(std::ops::Range<usize>),
}

/// Lay the filtered catalogue out into fixed-height rows.
///
/// Precomputed rather than drawn straight, because the picker has to be
/// virtualized: a channel with 7TV plus every provider's globals is well over
/// a thousand emotes, and each one drawn is a texture upload and an LRU touch.
/// Rows let `ScrollArea::show_rows` draw only what's on screen.
fn picker_rows(items: &[&CatalogEmote], cols: usize, channel: &str) -> Vec<PickerRow> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let group = items[i].group;
        rows.push(PickerRow::Header(group.title(channel)));
        let start = i;
        while i < items.len() && items[i].group == group {
            i += 1;
        }
        let mut at = start;
        while at < i {
            let end = (at + cols).min(i);
            rows.push(PickerRow::Emotes(at..end));
            at = end;
        }
    }
    rows
}

/// The emote picker panel. Returns a code to insert, if one was clicked.
#[allow(clippy::too_many_arguments)]
pub(in crate::ui) fn emote_picker(
    ui: &mut egui::Ui,
    bar: &mut SendBar,
    catalog: &[CatalogEmote],
    channel: &str,
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    misses: &mut Vec<std::path::PathBuf>,
    animate: bool,
    now: f64,
    ctx: &egui::Context,
) -> Option<String> {
    const CELL: f32 = 34.0;
    let mut picked = None;

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut bar.picker_filter)
                .hint_text("Search emotes")
                .desired_width(ui.available_width() - 60.0),
        )
        .on_hover_text("Filter by code, case-insensitive.");
        if ui.button("✕").on_hover_text("Close the emote picker.").clicked() {
            bar.picker_open = false;
        }
    });

    let f = bar.picker_filter.trim().to_lowercase();
    let items: Vec<&CatalogEmote> = catalog
        .iter()
        .filter(|e| f.is_empty() || e.code.to_lowercase().contains(&f))
        .collect();
    if items.is_empty() {
        ui.weak(if catalog.is_empty() {
            "No emotes cached for this channel yet — they arrive with the next asset fetch \
             (channel Properties → ⟳ Refetch)."
        } else {
            "No emote matches that."
        });
        return picked;
    }

    let cols = ((ui.available_width() / CELL).floor() as usize).max(1);
    let rows = picker_rows(&items, cols, channel);
    egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, false]).show_rows(
        ui,
        CELL,
        rows.len(),
        |ui, range| {
            for row in &rows[range] {
                // EVERY row is allocated exactly CELL high, headers included.
                // `show_rows` positions the visible window by index × row
                // height, so a row that measured shorter would slide all the
                // rows below it out of step with the scrollbar.
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), CELL),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| match row {
                        PickerRow::Header(title) => {
                            ui.label(egui::RichText::new(title).small().strong());
                        }
                        PickerRow::Emotes(r) => {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            for e in &items[r.clone()] {
                                let drawn = draw_cached_emote(
                                    ui,
                                    cache,
                                    &e.path,
                                    animate,
                                    CELL - 6.0,
                                    now,
                                    misses,
                                    ctx,
                                );
                                let resp = match drawn {
                                    Some((resp, _)) => resp,
                                    // Still decoding: hold the slot so the
                                    // grid doesn't reflow under the pointer
                                    // as images land.
                                    None => ui.add_sized(
                                        egui::vec2(CELL - 6.0, CELL - 6.0),
                                        egui::Label::new("").sense(egui::Sense::click()),
                                    ),
                                };
                                if resp
                                    .on_hover_text(format!(
                                        "{}\n{}",
                                        e.code,
                                        e.group.title(channel)
                                    ))
                                    .clicked()
                                {
                                    picked = Some(e.code.clone());
                                }
                            }
                        }
                    },
                );
            }
        },
    );
    picked
}

/// What the autocomplete decided this frame.
pub(in crate::ui) enum Completion {
    /// Nothing to offer — the caller sends on Enter as usual.
    None,
    /// A list is open; the caller must NOT treat Enter/Tab as "send".
    Open,
    /// Accept this code over this character range.
    Accept(std::ops::Range<usize>, String),
}

/// The `:code` autocomplete list, drawn above the message box.
///
/// Keyboard first — that's how it's used mid-sentence: ↑/↓ move, Tab or Enter
/// accepts, Esc dismisses. Enter is deliberately shared with "send": while a
/// list is open it completes instead, because a half-typed `:spin` is never
/// what someone meant to say.
#[allow(clippy::too_many_arguments)]
pub(in crate::ui) fn emote_autocomplete(
    ui: &mut egui::Ui,
    bar: &mut SendBar,
    edit: &egui::Response,
    caret: Option<usize>,
    catalog: &[CatalogEmote],
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    misses: &mut Vec<std::path::PathBuf>,
    now: f64,
    ctx: &egui::Context,
) -> Completion {
    let Some((range, query)) = caret.and_then(|c| emote_token(&bar.draft, c)) else {
        bar.complete_sel = 0;
        bar.complete_dismissed.clear();
        return Completion::None;
    };
    // Esc dismisses THIS token, not autocomplete as a whole: typing on should
    // bring the list back, but re-showing it for the same word you just
    // dismissed would make Esc useless.
    if bar.complete_dismissed == query {
        return Completion::None;
    }
    let matches = rank_emote_matches(&query, catalog, MAX_COMPLETIONS);
    if matches.is_empty() {
        bar.complete_sel = 0;
        return Completion::None;
    }
    bar.complete_sel = bar.complete_sel.min(matches.len() - 1);

    let (up, down, accept, dismiss) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::Tab) || i.key_pressed(egui::Key::Enter),
            i.key_pressed(egui::Key::Escape),
        )
    });
    if dismiss {
        bar.complete_dismissed = query;
        return Completion::None;
    }
    if up {
        bar.complete_sel = bar.complete_sel.checked_sub(1).unwrap_or(matches.len() - 1);
    }
    if down {
        bar.complete_sel = (bar.complete_sel + 1) % matches.len();
    }

    let mut clicked: Option<String> = None;
    egui::Popup::from_response(edit)
        .id(egui::Id::new(("chat_emote_complete", edit.id)))
        .align(egui::RectAlign::TOP_START)
        .open(true)
        // The list must not vanish on the click that picks from it, and
        // clicking away is already handled by the token check above.
        .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
        .show(|ui| {
            ui.set_min_width(220.0);
            for (i, e) in matches.iter().enumerate() {
                let selected = i == bar.complete_sel;
                let resp = ui
                    .scope_builder(
                        egui::UiBuilder::new().sense(egui::Sense::click()),
                        |ui| {
                            if selected {
                                let r = ui.available_rect_before_wrap();
                                ui.painter().rect_filled(
                                    r,
                                    3.0,
                                    ui.visuals().selection.bg_fill.gamma_multiply(0.5),
                                );
                            }
                            ui.horizontal(|ui| {
                                draw_cached_emote(
                                    ui, cache, &e.path, false, 22.0, now, misses, ctx,
                                );
                                ui.label(&e.code);
                                ui.weak(
                                    egui::RichText::new(e.group.source.label()).small(),
                                );
                            });
                        },
                    )
                    .response;
                if resp.clicked() {
                    clicked = Some(e.code.clone());
                }
            }
        });

    if let Some(code) = clicked {
        return Completion::Accept(range, code);
    }
    if accept {
        return Completion::Accept(range, matches[bar.complete_sel].code.clone());
    }
    Completion::Open
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(codes: &[(&str, bool)]) -> Vec<CatalogEmote> {
        codes
            .iter()
            .map(|(c, global)| CatalogEmote {
                code: (*c).to_string(),
                path: std::path::PathBuf::from(format!("{c}.webp")),
                group: EmoteGroup { global: *global, source: EmoteSource::SevenTv },
            })
            .collect()
    }

    /// The token is what decides whether a list appears at all, so every way
    /// of NOT being one matters as much as the happy path.
    #[test]
    fn emote_token_finds_a_colon_word_before_the_caret() {
        assert_eq!(
            emote_token("hello :spin", 11),
            Some((6..11, "spin".to_string()))
        );
        // Mid-string: only the token the caret is inside counts.
        assert_eq!(emote_token(":spin ok", 5), Some((0..5, "spin".to_string())));

        // One character is an emoticon, not a query — `:)` and `:D` must not
        // pop a list over the chat.
        assert_eq!(emote_token("hi :D", 5), None);
        assert_eq!(emote_token("hi :", 4), None);

        // A colon that doesn't start a word: a time, a URL, a closed `:code:`.
        assert_eq!(emote_token("at 10:30", 8), None);
        assert_eq!(emote_token("https://x", 9), None);
        assert_eq!(emote_token("::spin", 6), None);

        // No colon at all, and a caret that isn't at the token's end.
        assert_eq!(emote_token("spin", 4), None);
        assert_eq!(emote_token(":spin here", 10), None);
    }

    /// Non-ASCII must not panic or slice mid-codepoint — chat is full of it,
    /// and this indexes by character throughout for exactly that reason.
    #[test]
    fn emote_token_and_completion_are_character_indexed() {
        assert_eq!(emote_token("떡볶이 :spin", 9), Some((4..9, "spin".to_string())));
        let (text, caret) = apply_completion("떡볶이 :spin", 4..9, "laynaSpin");
        assert_eq!(text, "떡볶이 laynaSpin ");
        assert_eq!(caret, 14);
    }

    #[test]
    fn completion_replaces_the_token_and_leaves_a_trailing_space() {
        let (text, caret) = apply_completion("hello :spin", 6..11, "laynaSpinFast");
        assert_eq!(text, "hello laynaSpinFast ");
        assert_eq!(caret, 20);
        // Mid-sentence: the tail survives and the caret lands before it.
        let (text, caret) = apply_completion("a :spin b", 2..7, "Kappa");
        assert_eq!(text, "a Kappa  b");
        assert_eq!(caret, 8);
    }

    /// Prefix beats substring, shorter beats longer, and a channel emote beats
    /// a global one of the same code.
    #[test]
    fn matches_rank_prefix_then_length_then_channel_over_global() {
        let c = cat(&[
            ("laynaSpinFast", false),
            ("spinCat", false),
            ("spin", true),
            ("cupidFingerspin", false),
        ]);
        let got: Vec<&str> =
            rank_emote_matches("spin", &c, 10).iter().map(|e| e.code.as_str()).collect();
        // Prefix tier first (`spin`, `spinCat`), then the substring tier by
        // length — `laynaSpinFast` is 13 characters, `cupidFingerspin` is 15.
        assert_eq!(got, ["spin", "spinCat", "laynaSpinFast", "cupidFingerspin"]);

        // Case-insensitive, and the limit is honoured.
        assert_eq!(rank_emote_matches("SPIN", &c, 2).len(), 2);
        assert!(rank_emote_matches("nothinglikethis", &c, 10).is_empty());
        assert!(rank_emote_matches("", &c, 10).is_empty(), "an empty query offers nothing");
    }

    /// A code present in both a channel set and a global one is one
    /// suggestion, not two — and it's the channel's, which is what would
    /// actually render.
    #[test]
    fn duplicate_codes_collapse_to_the_channel_one() {
        let c = cat(&[("xdx", false), ("xdx", true)]);
        let got = rank_emote_matches("xdx", &c, 10);
        assert_eq!(got.len(), 1);
        assert!(!got[0].group.global);
    }

    /// Sections come out in catalogue order with their emotes packed into
    /// rows, so `show_rows` can draw only what's visible.
    #[test]
    fn picker_rows_group_by_section_and_wrap_at_the_column_count() {
        let c = cat(&[("a", false), ("b", false), ("c", false), ("d", true)]);
        let items: Vec<&CatalogEmote> = c.iter().collect();
        let rows = picker_rows(&items, 2, "Blu");
        let shape: Vec<String> = rows
            .iter()
            .map(|r| match r {
                PickerRow::Header(t) => format!("H:{t}"),
                PickerRow::Emotes(r) => format!("E:{}..{}", r.start, r.end),
            })
            .collect();
        assert_eq!(
            shape,
            ["H:Blu — 7TV", "E:0..2", "E:2..3", "H:7TV global emotes", "E:3..4"]
        );
    }
}
