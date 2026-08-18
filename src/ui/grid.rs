//! Grid machinery shared by the table views: columns, sorting, cells,
//! badges, instance-row rendering.

use super::*;

/// Success/affirmative green, shared across the table (recording "completed",
/// video "completed", ad-free "Yes").
pub(super) const SUCCESS_GREEN: egui::Color32 = egui::Color32::from_rgb(0x57, 0xc7, 0x57);

/// Streams-row background tint while an ad is playing (amber) / after an error
/// (red). Recording + keyboard-selected rows reuse the theme's selection accent.
pub(super) const HL_AD: egui::Color32 = egui::Color32::from_rgb(0x7a, 0x5a, 0x12);
pub(super) const HL_ERROR: egui::Color32 = egui::Color32::from_rgb(0x6e, 0x2f, 0x2f);
/// Row tint for a sub-row that CONTAINS a deep-filter match (see
/// [`FilterHits`]) — a dim teal, deliberately distinct from the ad amber,
/// error red, and the theme-accent recording/selection tints it can sit
/// among. Lowest priority: any state tint wins over it.
pub(super) const HL_FILTER_HIT: egui::Color32 = egui::Color32::from_rgb(0x1c, 0x4f, 0x52);
/// Background painted behind the matched substring itself inside a text cell
/// (see [`highlight_text_label`]) — brighter than [`HL_FILTER_HIT`] so the
/// match pops even on an already-tinted row.
pub(super) const HL_FILTER_TEXT_BG: egui::Color32 = egui::Color32::from_rgb(0x2e, 0x86, 0x8c);
/// Readable red for inline error/validation *text* (the row tint [`HL_ERROR`] is
/// too dark to read as a foreground colour).
pub(super) const HL_ERROR_TEXT: egui::Color32 = egui::Color32::from_rgb(0xe0, 0x6c, 0x6c);
/// Readable amber for inline caveats — something the reader must weigh, but
/// which isn't an error. Matches the "aborted" badge's amber so one hue means
/// "qualified" throughout.
pub(super) const HL_WARN_TEXT: egui::Color32 = egui::Color32::from_rgb(0xe0, 0xa8, 0x50);

/// Paint a row-tint background for one table cell + apply the selected-row
/// text colour. Call at the TOP of a cell closure so widgets draw on top.
///
/// Replaces the pre-virtualization trick of mutating the body `Ui`'s
/// `selection.bg_fill` between `body.row()` calls: with the virtualized
/// `body.rows()` the body `Ui` isn't reachable per row, so each cell paints
/// its own background instead (the half-item-spacing expansion mirrors
/// egui_extras' gapless stripe/selection fill).
pub(super) fn tint_cell(ui: &mut egui::Ui, tint: Option<egui::Color32>) {
    let Some(c) = tint else { return };
    let rect = ui.max_rect().expand2(0.5 * ui.spacing().item_spacing);
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, c);
    // Same text treatment egui_extras applies to `set_selected` rows.
    let stroke = ui.style().visuals.selection.stroke.color;
    ui.style_mut().visuals.override_text_color = Some(stroke);
}

/// Background tint for a Streams row, by state (highest priority first): an ad is
/// playing > recording > last poll/recording errored > keyboard-selected.
/// `accent` is the theme's selection color (so recording/selected keep the
/// existing look). When `status_colors` is off, the status tints (ad / recording
/// / error) are suppressed but a keyboard-`selected` row is still highlighted.
/// `None` = no tint.
pub(super) fn row_tint(
    recording: bool,
    ad_running: bool,
    errored: bool,
    selected: bool,
    accent: egui::Color32,
    status_colors: bool,
) -> Option<egui::Color32> {
    if status_colors {
        if recording && ad_running {
            return Some(HL_AD);
        } else if recording {
            return Some(accent);
        } else if errored {
            return Some(HL_ERROR);
        }
    }
    selected.then_some(accent)
}

/// Background tint for a Videos row, by download status: in-flight = the theme
/// accent, failed = the error red. `None` (incl. when `status_colors` is off) =
/// no tint. Mirrors [`row_tint`] for the Streams table.
pub(super) fn video_row_tint(status: &str, accent: egui::Color32, status_colors: bool) -> Option<egui::Color32> {
    if !status_colors {
        return None;
    }
    match status {
        "downloading" | "queued" => Some(accent),
        "failed" => Some(HL_ERROR),
        _ => None,
    }
}

/// Whether a monitor is in an error/failure state right now. Only `last_state`
/// is checked — recording failures are visible via the ⚠ state icon on the
/// instance row, and a failed `last_recording_status` should not prevent
/// "Clear error" from dismissing the channel-row tint.
pub(super) fn monitor_errored(m: &MonitorWithChannel) -> bool {
    matches!(m.monitor.last_state.as_str(), "error" | "failed")
}

/// Ad-break count for a cell (blank when there are none, so empty rows stay clean).
pub(super) fn fmt_ad_count(n: i64) -> String {
    if n > 0 { n.to_string() } else { String::new() }
}

/// The 🔒 badge's colour — a muted lock, not an error red: a subscriber-only
/// stream isn't a fault, it's a broadcast we aren't entitled to.
pub(super) const SUB_ONLY_COLOR: egui::Color32 = egui::Color32::from_rgb(0xc0, 0x93, 0xe0);

/// The instance row's subscriber-only marker.
///
/// A word, not a bare glyph. The lone 🔒 that used to sit here was a
/// small-sized outline (egui rasterizes emoji monochrome — see `crate::fonts`)
/// in a muted purple, beside a bright state dot: it read as a speck. This is
/// the fact that explains why a live channel is being archived from the CDN
/// instead of the live edge, so it has to survive a glance.
pub(super) const SUB_ONLY_BADGE: &str = "🔒 subs";

/// Hover text for the ⭳ **CDN capture** badge on an instance row.
pub(super) const CDN_CAPTURE_HOVER: &str =
    "Being archived from Twitch's CDN. The live edge was refused (subscriber-only), so instead \
     of a capture tool this monitor runs a CDN session: every few minutes it fetches the video \
     published since the last pass and writes it as a numbered part beside the take's output \
     file.\n\n\
     This is a real capture — the parts are on disk now and \"Play local recording\" opens them \
     in order. They are joined into the take's single file when the broadcast ends. The archive \
     necessarily lags the live edge, because the CDN cannot serve video it hasn't segmented yet.";

/// Hover text for the 🔒 **subscriber-only** badge.
///
/// `covers_until` is how far the CDN head backfill has archived this broadcast
/// (its take's own start time), so the reader sees the lag instead of guessing
/// at it — this archive is assembled behind the live edge by definition, and
/// that gap is the one number worth acting on.
pub(super) fn sub_only_hover(platform: Platform, covers_until: Option<i64>, now: i64) -> String {
    // YouTube gates the manifest itself — there is no public CDN copy to fall
    // back on, so the wording must not imply one is being fetched.
    if platform != Platform::Twitch {
        return "🔒 Members-only stream — the credentials in use don't hold this channel's \
                membership, so it can't be captured. It's still recorded in the history as \
                seen-live. Point Settings → Accounts → Download authentication at a browser \
                profile signed in with the membership and it will capture normally."
            .to_string();
    }
    let mut out = "🔒 Subscriber-only stream — the connected account isn't entitled to it \
                   (UNAUTHORIZED_ENTITLEMENTS), so the live edge can't be captured directly."
        .to_string();
    match covers_until.filter(|t| *t > 0) {
        Some(t) => out.push_str(&format!(
            "\n\nIt is still being archived, from Twitch's CDN: the head backfill holds this \
             broadcast from its start up to {} — roughly {} behind live. That gap closes each \
             time the capture retries (every {} minutes), and the last minutes before the \
             stream ends may be missing.",
            fmt_datetime_short(t),
            crate::rolling::fmt_remaining((now - t).max(0)),
            crate::downloader::SUB_ONLY_COOLDOWN_SECS / 60,
        )),
        None => out.push_str(
            "\n\nCapture falls back to the CDN head backfill, which archives the broadcast from \
             its start and always lags the live edge.",
        ),
    }
    out.push_str(
        "\n\nSubscribing with the connected Twitch account would let this capture normally.",
    );
    out
}

/// Resolve an instance's ad-free status into a (label, tooltip) for display.
/// Manual flag wins; otherwise the auto Twitch-sub result (`Some(true)` = sub'd,
/// `Some(false)` = checked & not sub'd, `None` = unknown/not checked). Returns
/// `None` when there's nothing to show.
pub(super) fn ad_free_status(manual: bool, sub: Option<bool>) -> Option<(&'static str, &'static str)> {
    if manual {
        Some((
            "Yes",
            "Marked ad-free for your account (member/sub/Turbo) — captures won't have \
             ad-break hard cuts.",
        ))
    } else {
        match sub {
            Some(true) => Some((
                "Yes (sub)",
                "Your connected Twitch account is subscribed to this channel — \
                 subscriber captures have no ad breaks.",
            )),
            _ => None,
        }
    }
}

/// Channel-row ad-free summary (label + numeric sort key) from how many of the
/// channel's instances are ad-free.
pub(super) fn ad_free_summary(ad_free_count: usize, total: usize) -> (&'static str, f64) {
    if total == 0 || ad_free_count == 0 {
        ("", 0.0)
    } else if ad_free_count == total {
        ("Yes", 2.0)
    } else {
        ("some", 1.0)
    }
}

/// Human-readable lines describing where ad breaks cause hard cuts in the
/// finished file. `at_secs` is already the cut's position in the captured file
/// (ad segments are filtered out), so it's shown directly as a seek timestamp.
/// `breaks` must be ordered by offset.
pub(super) fn ad_cut_lines(breaks: &[AdBreak]) -> Vec<String> {
    breaks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "#{}  cut at {}  ({}s ad)",
                i + 1,
                fmt_duration(b.at_secs.max(0)),
                b.duration_secs
            )
        })
        .collect()
}

/// Count string for a Changes cell ("" when zero, so empty cells render nothing).
pub(super) fn fmt_meta_count(n: i64) -> String {
    if n > 0 { n.to_string() } else { String::new() }
}

/// Render a "Next stream" cell: blank when no upcoming stream is known, else the
/// scheduled start datetime. When `clickable`, a double-click returns true so the
/// caller can open the full-schedule popup; the hover shows the title.
pub(super) fn next_stream_cell(ui: &mut egui::Ui, at: Option<i64>, title: &str, clickable: bool) -> bool {
    let Some(at) = at.filter(|&a| a > 0) else {
        return false;
    };
    let compact = short_ts_on();
    let display = if compact { fmt_datetime_compact(at) } else { fmt_datetime_short(at) };
    let label = if clickable {
        egui::Label::new(&display).sense(egui::Sense::click())
    } else {
        egui::Label::new(&display)
    };
    let resp = ui.add(label).on_hover_ui(|ui| {
        if compact {
            ui.label(fmt_datetime_short(at));
        }
        if title.is_empty() {
            ui.label("Next scheduled stream.");
        } else {
            ui.label(format!("Next: {title}"));
        }
        if clickable {
            ui.label("Double-click for the full upcoming schedule.");
        }
    });
    clickable && resp.double_clicked()
}

/// One human-readable line per *actual* metadata change (offset + kind +
/// `old → new`). The initial value of each field (logged with an empty
/// `old_value`) is the starting state, not a change, so it's skipped — it still
/// shows as the `old` side of the first real change.
pub(super) fn meta_change_lines(changes: &[StreamMetaChange]) -> Vec<String> {
    changes
        .iter()
        .filter(|c| !c.old_value.is_empty() || c.kind == "collab")
        .map(|c| {
            let at = fmt_duration(c.at_secs.max(0));
            format!("{at}  {}", change_transition(&c.kind, &c.old_value, &c.new_value))
        })
        .collect()
}

/// `Kind: old → new` with kind-appropriate empty-value wording. Collab rows
/// keep their session-start events (empty `old` = the collab beginning, a
/// meaningful moment, unlike title/category baselines which are just the
/// first observation).
pub(super) fn change_transition(kind: &str, old: &str, new: &str) -> String {
    let label = match kind {
        "category" => "Category",
        "collab" => "Collab",
        "tags" => "Tags",
        _ => "Title",
    };
    let (none_old, none_new) = if kind == "collab" {
        ("(none)", "(ended)")
    } else {
        ("", "(cleared)")
    };
    let old = if old.is_empty() { none_old } else { old };
    let new = if new.is_empty() { none_new } else { new };
    format!("{label}: {old} → {new}")
}
/// One human-readable line per *actual* change in a monitor's all-time history
/// (absolute date/time, not an offset — there's no single take to be relative
/// to). Same "skip the baseline" rule as [`meta_change_lines`].
pub(super) fn monitor_change_lines(changes: &[MonitorStreamChange]) -> Vec<String> {
    changes
        .iter()
        .filter(|c| !c.old_value.is_empty() || c.kind == "collab")
        .map(|c| {
            let at = fmt_datetime_short(c.at_unix);
            format!("{at}  {}", change_transition(&c.kind, &c.old_value, &c.new_value))
        })
        .collect()
}

/// Merge a stream's takes into one chronological change list. Each take's offsets
/// (`at_secs`, relative to that take's start) are rebased onto the whole stream's
/// timeline (`take.started_at - stream_start + at_secs`); the rows are then sorted
/// and run through [`meta_change_lines`], which drops each take's initial value
/// (empty `old_value`) — so a take re-observing the value the previous take ended
/// on adds no duplicate line, while genuine changes are kept.
pub(super) fn aggregate_stream_changes(takes: &[(i64, Vec<StreamMetaChange>)]) -> Vec<StreamMetaChange> {
    let stream_start = takes.iter().map(|(s, _)| *s).min().unwrap_or(0);
    let mut all: Vec<StreamMetaChange> = Vec::new();
    for (started_at, rows) in takes {
        for r in rows {
            let mut adj = r.clone();
            adj.at_secs = (started_at - stream_start) + r.at_secs;
            all.push(adj);
        }
    }
    all.sort_by_key(|c| (c.at_secs, c.id));
    all
}

/// Multi-line tooltip body for a Changes cell: a heading plus the change list
/// (just the heading when the detail isn't loaded or there are no changes).
pub(super) fn meta_tooltip(count: i64, changes: Option<&Vec<StreamMetaChange>>) -> String {
    let mut s = format!("{count} title/category change(s) during this recording.");
    if let Some(lines) = changes.map(|c| meta_change_lines(c)).filter(|l| !l.is_empty()) {
        s.push('\n');
        s.push_str(&lines.join("\n"));
    }
    s
}

/// Render one Changes table cell: blank when the count is
/// zero, a lazily-built hover list, and (when `clickable`) a double-click to open
/// the change-log popup. Returns whether it was double-clicked so the caller can
/// open the right popup (a single take, or a whole stream's aggregated takes).
pub(super) fn meta_cell(
    ui: &mut egui::Ui,
    count: i64,
    detail: Option<&Vec<StreamMetaChange>>,
    clickable: bool,
) -> bool {
    let text = fmt_meta_count(count);
    if text.is_empty() {
        return false;
    }
    let label = if clickable {
        egui::Label::new(text).sense(egui::Sense::click())
    } else {
        egui::Label::new(text)
    };
    let resp = ui.add(label).on_hover_ui(|ui| {
        ui.label(meta_tooltip(count, detail));
    });
    clickable && resp.double_clicked()
}

/// Render the combined Ads column (📢): the ad break count as the cell text, with
/// a tooltip showing "Ads: N (total time)" + the per-break cut list if loaded.
/// The double-click behaviour mirrors [`combined_ads_cell`].
pub(super) fn combined_ads_cell(
    ui: &mut egui::Ui,
    count: i64,
    secs: i64,
    detail: Option<&Vec<AdBreak>>,
    clickable_rec: Option<i64>,
) -> Option<i64> {
    if count == 0 {
        return None;
    }
    let text = fmt_ad_count(count);
    let label = if clickable_rec.is_some() {
        egui::Label::new(text).sense(egui::Sense::click())
    } else {
        egui::Label::new(text)
    };
    let resp = ui.add(label).on_hover_ui(|ui| {
        ui.label(format!("Ads: {} ({})", count, fmt_duration(secs)));
        if let Some(b) = detail.filter(|b| !b.is_empty()) {
            ui.label(ad_cut_lines(b).join("\n"));
        }
    });
    match clickable_rec {
        Some(rec) if resp.double_clicked() => Some(rec),
        _ => None,
    }
}

/// Render a timestamp cell using the compact format when short-timestamps mode is
/// on; falls back to the normal format when off. When compact, the full timestamp
/// is shown in a tooltip.
pub(super) fn ts_label(ui: &mut egui::Ui, secs: i64) {
    if secs <= 0 {
        return;
    }
    let compact = short_ts_on();
    let display = if compact { fmt_datetime_compact(secs) } else { fmt_datetime_short(secs) };
    let resp = ui.label(display);
    if compact {
        resp.on_hover_text(fmt_datetime_short(secs));
    }
}

/// Like [`ts_label`] but appends `~` for approximate times (Went Live column).
pub(super) fn ts_went_live_label(ui: &mut egui::Ui, secs: i64, approx: bool) {
    if secs <= 0 {
        return;
    }
    let compact = short_ts_on();
    let display = {
        let s = if compact { fmt_datetime_compact(secs) } else { fmt_datetime_short(secs) };
        if approx { format!("{s}~") } else { s }
    };
    let resp = ui.label(display);
    if compact {
        let full = {
            let s = fmt_datetime_short(secs);
            if approx { format!("{s}~") } else { s }
        };
        resp.on_hover_text(full);
    }
}

/// Render a current-Title / current-Game cell: blank when empty, otherwise a
/// label truncated to the (width-capped) column. egui shows the full text on
/// hover automatically when the label is elided (`show_tooltip_when_elided`
/// defaults to true), so we add no explicit tooltip — a second one would just
/// stack a duplicate.
/// Tags cell: the truncated tag list, with the broadcast language appended to
/// the hover when known (it arrives from the same Helix response).
pub(super) fn tags_cell(ui: &mut egui::Ui, tags: &str, language: &str) {
    if tags.is_empty() && language.is_empty() {
        return;
    }
    let hover = match (tags.is_empty(), language.is_empty()) {
        (false, false) => format!("{tags}\nLanguage: {language}"),
        (false, true) => tags.to_string(),
        (true, false) => format!("Language: {language}"),
        (true, true) => unreachable!(),
    };
    ui.add(egui::Label::new(tags).truncate()).on_hover_text(hover);
}

pub(super) fn meta_value_cell(ui: &mut egui::Ui, value: &str, hl: Option<&str>) {
    if value.is_empty() {
        return;
    }
    // `hl` = the column's active filter needle — the matched substring gets
    // the highlight pill so a filtered grid shows WHY this row is here.
    highlight_text_label(ui, value, hl);
}

/// Render a name as a plain label, or — when `cid` is `Some` (it resolves to
/// a locally-tracked channel) — as `color`, underlined, with a click-to-open-
/// Properties hyperlink. Returns `Some(cid)` when clicked. Shared by every
/// place a tracked channel's name shows up as plain text outside its own
/// Streams-grid row (Collab column, name-suffix, Stats events), so the same
/// identity always reads as the same colour and is always a shortcut to its
/// Properties window.
/// What clicking a name in an event line asks for.
pub(super) enum NameClick {
    /// A tracked channel of ours — its own Properties window.
    Channel(i64),
    /// Anyone else (a chatter, a raider we don't monitor) — their user
    /// Properties, built from what this channel has recorded about them.
    User(String),
}

/// Render a name inside an event line: coloured, and clickable through to
/// whatever "who is this" window fits it.
///
/// A tracked channel keeps its own colour and opens channel Properties (see
/// [`tracked_name_label`]). Everyone else — the overwhelming majority of names
/// in an events table — used to render as flat grey text with nothing behind
/// it, even though the app knows plenty about them: what they've cheered,
/// gifted and raided, and every moderation action against them. They now get
/// the same deterministic colour chat gives them, and open user Properties.
pub(super) fn event_name_label(
    ui: &mut egui::Ui,
    name: &str,
    cid: Option<i64>,
    color: Option<egui::Color32>,
) -> Option<NameClick> {
    if cid.is_some() {
        return tracked_name_label(ui, name, cid, color).map(NameClick::Channel);
    }
    if name.trim().is_empty() {
        ui.label(name);
        return None;
    }
    // Same per-name hash chat uses, so one person reads the same colour in the
    // replay, the notifications feed and here; adjusted for the panel it lands
    // on so dark hashes stay legible.
    let color = readable_color(twitch_username_color(name), ui.visuals().panel_fill);
    let resp = ui.add(
        egui::Label::new(egui::RichText::new(name).color(color).underline())
            .sense(egui::Sense::click()),
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(
        "Click for this user's Properties — what this channel has recorded about them \
         (bits, gift subs, raids) and any moderation actions against them.",
    )
    .clicked()
    .then(|| NameClick::User(name.to_string()))
}

pub(super) fn tracked_name_label(
    ui: &mut egui::Ui,
    name: &str,
    cid: Option<i64>,
    color: Option<egui::Color32>,
) -> Option<i64> {
    let Some(cid) = cid else {
        ui.label(name);
        return None;
    };
    let text = match color {
        Some(c) => egui::RichText::new(name).color(c).underline(),
        None => egui::RichText::new(name).underline(),
    };
    let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text("Click to open this channel's Properties.").clicked().then_some(cid)
}

/// Resolve a collab partner login to its own tracked-channel colour, same
/// precedence/readability-adjustment as a channel's own Streams-grid name.
fn collab_partner_color(
    pcid: i64,
    channel_name_colors: &HashMap<i64, (egui::Color32, bool)>,
    tint: Option<egui::Color32>,
    ui: &egui::Ui,
) -> egui::Color32 {
    let (base, adjust) =
        channel_name_colors.get(&pcid).copied().unwrap_or_else(|| (channel_event_color(pcid, ""), false));
    if adjust { readable_color(base, tint.unwrap_or_else(|| ui.visuals().panel_fill)) } else { base }
}

/// Like [`tracked_name_label`], but for a collab partner specifically: when
/// the partner does NOT resolve to a tracked channel, right-clicking their
/// name offers "Add as new instance" — opens the Add-stream form pre-filled
/// with their Twitch login/display name (`MonitorForm::from_collab_partner`),
/// so following up on a real-life collaborator doesn't require retyping
/// their URL by hand. Confirmed partners only (`from_title == false`) — a
/// title `@mention` is an unverified guess, same reasoning
/// [`UntrackedCollabPartner`] already applies to auto-play. Returns the
/// clicked tracked-channel id (if any) plus an add-instance request (if any).
pub(super) fn collab_name_label(
    ui: &mut egui::Ui,
    p: &crate::models::CollabPartner,
    cid: Option<i64>,
    color: Option<egui::Color32>,
) -> (Option<i64>, Option<UntrackedCollabPartner>) {
    let text = p.display(p.from_title);
    if cid.is_some() {
        return (tracked_name_label(ui, &text, cid, color), None);
    }
    if p.from_title || p.login.is_empty() {
        ui.label(text);
        return (None, None);
    }
    let resp = ui
        .add(egui::Label::new(&text).sense(egui::Sense::click()))
        .on_hover_text(format!("Right-click to add {} as a new instance.", p.name));
    let mut add = None;
    resp.context_menu(|ui| {
        ui.set_min_width(180.0);
        if ui.button(format!("➕  Add {} as new instance", p.name)).clicked() {
            add = Some(UntrackedCollabPartner { login: p.login.clone(), name: p.name.clone() });
            ui.close();
        }
    });
    (None, add)
}

/// Render a comma-joined run of collab partner names — shared-chat partners
/// first, then title `@mentions` as `@name` — each coloured with its own
/// tracked channel's Streams-grid colour and linked to its Properties window
/// when `resolve` maps its login to one (see [`collab_name_label`]); an
/// untracked or unverified name stays plain (confirmed-but-untracked names
/// offer "Add as new instance" on right-click). Returns the clicked
/// channel's id (if any), an add-instance request (if any), plus the group's
/// response, so the caller can attach its own hover text (it needs the full
/// [`crate::models::CollabLive`] for host/since-when, not just the partners).
pub(super) fn collab_names_row(
    ui: &mut egui::Ui,
    partners: &[crate::models::CollabPartner],
    resolve: impl Fn(&str) -> Option<i64>,
    channel_name_colors: &HashMap<i64, (egui::Color32, bool)>,
    tint: Option<egui::Color32>,
) -> (Option<i64>, Option<UntrackedCollabPartner>, egui::Response) {
    let mut clicked = None;
    let mut add = None;
    let resp = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let mut first = true;
            for p in partners.iter().filter(|p| !p.from_title).chain(partners.iter().filter(|p| p.from_title)) {
                if !first {
                    ui.label(", ");
                }
                first = false;
                let pcid = resolve(&p.login);
                let color = pcid.map(|cid| collab_partner_color(cid, channel_name_colors, tint, ui));
                let (c, a) = collab_name_label(ui, p, pcid, color);
                if c.is_some() {
                    clicked = c;
                }
                if a.is_some() {
                    add = a;
                }
            }
        })
        .response;
    (clicked, add, resp)
}

/// Render a 🤝 Collab cell: comma-joined partner names, truncated to the
/// column, with a detail hover (who, host, since-when, source). Blank when
/// not collabing. See [`collab_names_row`] for the per-name colour/link/
/// add-instance behaviour. Returns the clicked channel's id, if any, plus an
/// add-instance request, if any.
pub(super) fn collab_cell(
    ui: &mut egui::Ui,
    collab: Option<&crate::models::CollabLive>,
    rows: &[MonitorWithChannel],
    login_to_mid: &HashMap<String, i64>,
    channel_name_colors: &HashMap<i64, (egui::Color32, bool)>,
    tint: Option<egui::Color32>,
) -> (Option<i64>, Option<UntrackedCollabPartner>) {
    let Some(c) = collab else { return (None, None) };
    if c.partners.is_empty() {
        return (None, None);
    }
    let hover = collab_hover(c);
    let resolve = |login: &str| {
        login_to_mid.get(login).and_then(|&mid| rows.iter().find(|r| r.monitor.id == mid).map(|r| r.channel.id))
    };
    let (clicked, add, resp) = collab_names_row(ui, &c.partners, resolve, channel_name_colors, tint);
    resp.on_hover_text(hover);
    (clicked, add)
}

/// The 🤝 hover text: shared-chat partners with the host called out, the
/// session start, and title-mention partners marked as the heuristic they are.
pub(super) fn collab_hover(c: &crate::models::CollabLive) -> String {
    let mut lines: Vec<String> = Vec::new();
    let shared: Vec<&crate::models::CollabPartner> =
        c.partners.iter().filter(|p| !p.from_title).collect();
    if !shared.is_empty() {
        let host = if c.host_id.is_empty() {
            String::new()
        } else if let Some(h) = shared.iter().find(|p| p.id == c.host_id) {
            format!(" (host: {})", h.name)
        } else {
            " (host: this channel)".to_string()
        };
        let names: Vec<String> = shared.iter().map(|p| p.display(false)).collect();
        lines.push(format!("Streaming together with {}{host}", names.join(", ")));
        if c.since_unix > 0 {
            lines.push(format!("Shared chat since {}", fmt_datetime_short(c.since_unix)));
        }
    }
    let mentions: Vec<String> = c
        .partners
        .iter()
        .filter(|p| p.from_title)
        .map(|p| p.display(true))
        .collect();
    if !mentions.is_empty() {
        lines.push(format!("@mentioned in the title (unconfirmed): {}", mentions.join(", ")));
    }
    lines.join("\n")
}

/// Parse the monitor id out of a [`StreamGroup`] key (`s<mid>:…` / `t<mid>:…`).
pub(super) fn stream_key_monitor(key: &str) -> Option<i64> {
    let rest = key.strip_prefix('s').or_else(|| key.strip_prefix('t'))?;
    rest.split(':').next()?.parse().ok()
}

/// Format a go-live time (`~`-suffixed when only our approximate time is known).
pub(super) fn fmt_went_live(at: Option<i64>, approx: bool) -> String {
    match at {
        Some(w) => {
            let s = fmt_datetime_short(w);
            if approx { format!("{s}~") } else { s }
        }
        None => String::new(),
    }
}

/// Compact live viewer count (`1234` → `1.2K`, `1_200_000` → `1.2M`); empty for
/// a negative (unknown) count.
pub(super) fn fmt_viewers(n: i64) -> String {
    if n < 0 {
        String::new()
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}K", n / 1000)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}
/// 👁 cell: the current count plus a tiny inline last-hour trend sparkline
/// (painter polyline — no per-row egui_plot cost). Empty for a negative
/// (unknown/offline) count. Returns true on double-click (open 📈 stats).
pub(super) fn viewers_cell(
    ui: &mut egui::Ui,
    viewers: i64,
    spark: Option<&Vec<(i64, i64)>>,
) -> bool {
    if viewers < 0 {
        return false;
    }
    let mut open_stats = false;
    let resp = ui
        .add(egui::Label::new(fmt_viewers(viewers)).truncate().sense(egui::Sense::click()))
        .on_hover_text(format!(
            "{viewers} viewers\nDouble-click for viewer history graphs (📈)"
        ));
    if resp.double_clicked() {
        open_stats = true;
    }
    if let Some(pts) = spark
        && pts.len() >= 2
        && ui.available_width() >= 30.0
    {
        let w = ui.available_width().min(48.0);
        let (rect, sresp) =
            ui.allocate_exact_size(egui::vec2(w, 12.0), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let (t0, t1) = (pts[0].0, pts[pts.len() - 1].0);
            let (lo, hi) = pts.iter().fold((i64::MAX, i64::MIN), |(lo, hi), (_, v)| {
                (lo.min(*v), hi.max(*v))
            });
            let dt = (t1 - t0).max(1) as f32;
            let dv = (hi - lo).max(1) as f32;
            let line: Vec<egui::Pos2> = pts
                .iter()
                .map(|(t, v)| {
                    egui::pos2(
                        rect.left() + (*t - t0) as f32 / dt * rect.width(),
                        rect.bottom() - (*v - lo) as f32 / dv * rect.height(),
                    )
                })
                .collect();
            let color = ui.visuals().weak_text_color();
            ui.painter().add(egui::Shape::line(line, egui::Stroke::new(1.0, color)));
        }
        let sresp = sresp.on_hover_text(format!(
            "Last hour: {} → {} (peak {})\nDouble-click for viewer history graphs (📈)",
            fmt_viewers(pts[0].1),
            fmt_viewers(pts[pts.len() - 1].1),
            fmt_viewers(pts.iter().map(|(_, v)| *v).max().unwrap_or(0)),
        ));
        if sresp.double_clicked() {
            open_stats = true;
        }
    }
    open_stats
}

/// Theme color for a video download status string.
pub(super) fn video_status_color(status: &str) -> egui::Color32 {
    use egui::Color32;
    match status {
        "downloading" => Color32::from_rgb(0x4d, 0x9b, 0xff),
        "completed" => SUCCESS_GREEN,
        "failed" => Color32::from_rgb(0xe0, 0x6c, 0x6c),
        _ => Color32::from_gray(0xa0), // queued / stopped / orphaned
    }
}
// ─── Sortable + filterable tables ───────────────────────────────────────────
//
// Both tables share a tiny model: each row is turned into a `Vec<Cell>` in header
// order. Videos excludes its trailing Actions column (`VIDEO_COLS` = 9); Streams
// keeps a (non-sortable, empty) Actions placeholder slot so the model indices line
// up with `STREAM_COLUMNS` (`STREAM_COLS`). The header renders a click-to-sort
// title + a per-column filter box; `ordered_rows` filters then sorts and returns
// the surviving original-row indices in display order. The data cells themselves
// are still drawn by the existing per-row code, indexed by those original indices.
// (The optional Actions column is skipped at render time, not in the model.)

/// The Streams columns, in DEFAULT display order (the user's persisted order
/// lives in `StreamArchiverApp.streams_grid`; see [`grid_columns::GridCol`]).
/// Actions and the platform-icon column sit just left of Name by default; the
/// current Game/Title sit just right of State. Widths are floors —
/// `Column::auto` shrinks tight columns to their content — except the
/// `initial`-width columns, which start narrow and truncate (full value on
/// hover). Each `id` is a stable persistence key: never reuse or change one
/// once shipped.
pub(super) const STREAM_COLUMNS: [GridCol; 27] = [
    GridCol { id: "enabled",     title: "On",         tooltip: "Master switch. Off = fully dormant: no detection, recording, or asset/about/posts/schedule fetch until you act manually (▶ Start, ⟳ Refetch). Independent from Auto (which only gates automatic recording). The channel checkbox and each instance checkbox are independent.", min_width: 30.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "auto",        title: "Auto",       tooltip: "Auto-record: automatically record to disk when the stream goes live (a disk-space control). It does NOT gate detection, metadata, posts, schedules or assets — those always run while the channel is On. Manual Start still records, and trigger words (Settings → Automation) can still start a recording while Auto is off. The channel checkbox and each instance checkbox are independent.", min_width: 36.0,  initial: 0.0,   sortable: true,  stretch: false },
    GridCol { id: "actions",     title: "Actions",    tooltip: "Per-row actions: start/stop recording, edit, add instance, open folder, delete.",            min_width: 126.0, initial: 0.0,   sortable: false, stretch: false },
    GridCol { id: "platform",    title: "Plat",       tooltip: "Source platform (icon): Twitch, YouTube, Kick, or a generic URL. A channel shows every platform among its instances.", min_width: 52.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "name",        title: "Name",       tooltip: "Channel (container) name. Expand it to see its instances and recording history.",            min_width: 130.0, initial: 0.0,   sortable: true,  stretch: false },
    GridCol { id: "tool",        title: "Tool",  tooltip: "Capture tool: SL = streamlink, yt-dlp, ff = ffmpeg. Hover a row for the full name.", min_width: 36.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "detection",   title: "⇄",    tooltip: "Detection method — how liveness is detected: ↺ = API poll, ⚡ = push event, ⌁ = scrape, ◉ = probe, C = CLI, ⛔ = disabled (manual only). Hover a row for the full method.", min_width: 24.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "scheduled_rec", title: "📅", tooltip: "Scheduled recordings: force-start at a specific time or on a weekly repeat, bypassing Auto. Hover for the next few occurrences. Hidden by default — enable it from the column header.", min_width: 32.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "polled",      title: "Polled", tooltip: "When this instance was last checked. Compact mode shows HH:MM only; hover for the full timestamp.", min_width: 64.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "state",       title: "●",    tooltip: "Current monitor state. ⏺ = recording, ● = live (not recording), ○ = idle, ⚠ = failed, ⚡ = aborted.", min_width: 26.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "next_stream", title: "Next stream",tooltip: "Next scheduled stream (Twitch schedule / YouTube upcoming). Hover for its title; double-click for the full schedule.", min_width: 96.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "game",        title: "Game",       tooltip: "Current game / category of the most recent recording. Truncated — hover for the full name.", min_width: 60.0,  initial: 96.0,  sortable: true, stretch: false },
    GridCol { id: "title",       title: "Title",      tooltip: "Current stream title of the most recent recording. Truncated — hover for the full title.",   min_width: 80.0,  initial: 170.0, sortable: true, stretch: false },
    GridCol { id: "collab",      title: "🤝 Collab",  tooltip: "Who this channel is streaming together with (Twitch \"Stream Together\" / Shared Chat, plus @mentions in the title shown as @name). Live rows show the current collab; stream/take rows show the collab recorded for that broadcast. Hover for host and details; right-click the channel for the full collab history.", min_width: 70.0, initial: 110.0, sortable: true, stretch: false },
    GridCol { id: "viewers",     title: "👁",         tooltip: "Live viewer count (Twitch / Kick; YouTube best-effort). Shown for a live channel even when not recording; blank when offline or unknown.", min_width: 44.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "changes",     title: "✏",          tooltip: "Title / game-category changes logged during the recording. Hover or double-click for the log.", min_width: 24.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "ads",         title: "📢",         tooltip: "Ad breaks detected (Twitch + streamlink); each is a hard cut. Hover for count + total time; double-click for the cut list.", min_width: 24.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "went_live",   title: "Went Live",  tooltip: "When the stream went live on the platform (a trailing \"~\" means it's our approximate time).", min_width: 96.0, initial: 0.0,  sortable: true, stretch: false },
    GridCol { id: "started_on",  title: "Started On", tooltip: "When recording started.",                                                                    min_width: 92.0,  initial: 0.0,   sortable: true, stretch: false },
    GridCol { id: "lost_time",   title: "Lost time",  tooltip: "How much of the start was missed. Drops to 0 once a from-start capture catches up to the live edge.", min_width: 52.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "duration",    title: "Duration",   tooltip: "How long we've recorded (ticks while live).",                                               min_width: 56.0,  initial: 0.0,   sortable: true, stretch: false },
    GridCol { id: "ad_free",     title: "Ad-free",    tooltip: "Marked or auto-detected ad-free (sub / Turbo / Premium) — captures have no ad-break cuts. A channel row shows one 🛡 per ad-free instance.", min_width: 54.0,  initial: 0.0,   sortable: true, stretch: false },
    GridCol { id: "added",       title: "Added",      tooltip: "When the channel was added.",                                                               min_width: 84.0,  initial: 0.0,   sortable: true, stretch: false },
    GridCol { id: "tags",        title: "Tags",       tooltip: "The stream's tags (Twitch; Kick when set) — persists through offline as the channel's usual tags, same as language/category id. Hover for the full list; changes are archived — see 📝 Title/category/tags history.", min_width: 0.0, initial: 120.0, sortable: true, stretch: false },
    GridCol { id: "rolling",     title: "🕰",         tooltip: "Rolling recordings: how long until the next file under this row is deleted automatically. Channel/instance rows show the soonest of everything beneath them; stream and take rows show their own. Sort by it to put whatever expires first at the top. Blank for rows with nothing counting down (📌 = kept, 🗑 = already expired). Hidden by default — enable it from the column header.", min_width: 62.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "drives",      title: "🖴",         tooltip: "Which drive letters this row's recordings are stored on, comma-separated (A:, G:). Channel and instance rows list every drive anything beneath them sits on; period, stream and take rows narrow it down to their own files. Read from the stored paths rather than confirmed against disk, so a file that vanished outside the app still counts until its row is disposed of. Blank for rows with no stored file, and for network (UNC) paths, which have no drive letter.", min_width: 54.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "disk_use",    title: "💾",         tooltip: "Disk space used by stored recordings. Period/stream/take rows confirm each file still exists before counting it; the channel/instance rollup is a stored total instead (refreshed when the grid reloads, not live) and can briefly overcount a take whose file has since gone missing — expand down to it for the exact figure.", min_width: 64.0, initial: 0.0, sortable: true, stretch: false },
];

/// Total Streams columns, including the non-sortable Actions slot.
pub(super) const STREAM_COLS: usize = STREAM_COLUMNS.len();

/// Effective `min_width` for a Streams-grid column. Went Live / Started On /
/// Next stream / Polled render via [`short_ts_on`]-aware formatters
/// (`ts_label`/`ts_went_live_label`/`next_stream_cell`/`fmt_polled`) whose
/// short-mode text ("12/07 02:00", or "02:00" for Polled) is much narrower
/// than their `GridCol::min_width` — calibrated for the longer full-format
/// text. Since `Column::auto()`'s min_width is only ever a FLOOR (full mode
/// still auto-grows past it as needed), shrinking it while short mode is on
/// is safe both ways; without this the column was stuck with permanent
/// trailing space in short mode (reported 2026-07-08). A column whose width
/// was already fit/persisted at the old, wider floor needs one manual
/// resize or "⇔ Fit columns" to actually shrink to the new floor — egui_extras
/// keeps a resizable column's stored width once set, it doesn't re-measure
/// every frame.
pub(super) fn streams_col_min_width(c: &GridCol) -> f32 {
    if !short_ts_on() {
        return c.min_width;
    }
    match c.id {
        "went_live" | "started_on" | "next_stream" => c.min_width.min(64.0),
        "polled" => c.min_width.min(40.0),
        _ => c.min_width,
    }
}

/// The Videos columns, in DEFAULT display order (mirrors `STREAM_COLUMNS`).
/// The trailing Actions column (index `VIDEO_COLS`, 9) is non-sortable and
/// gated by the existing Show Actions setting, same as Streams'.
pub(super) const VIDEO_COLUMNS: [GridCol; 10] = [
    GridCol { id: "video",    title: "Video",    tooltip: "The video's title (or the URL until detected). Hover a row for the full URL.", min_width: 180.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "channel",  title: "Channel",  tooltip: "Uploader / channel name (filled when Auto-detect is on).", min_width: 110.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "platform", title: "Platform", tooltip: "Source platform: YouTube, Twitch, Kick, or a generic URL.", min_width: 86.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "tool",     title: "Tool",     tooltip: "Download tool: yt-dlp, streamlink, or ffmpeg.", min_width: 72.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "status",   title: "Status",   tooltip: "queued / downloading / completed / failed / stopped. Hover a failed row to see why.", min_width: 96.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "speed",    title: "Speed",    tooltip: "Current download speed (shown while downloading).", min_width: 82.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "size",     title: "Size",     tooltip: "Size of the output file (grows while downloading).", min_width: 72.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "added",    title: "Added",    tooltip: "When the download was added.", min_width: 80.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "file",     title: "File",     tooltip: "Output file path once written. Hover for the full path.", min_width: 160.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "actions",  title: "Actions",  tooltip: "Per-row actions: stop / retry, open file, open folder, copy URL, delete.", min_width: 150.0, initial: 0.0, sortable: false, stretch: true },
];

/// Sortable/filterable Videos columns (Video..File; excludes Actions).
pub(super) const VIDEO_COLS: usize = 9;

/// The 🎞 Clips columns, in DEFAULT display order.
///
/// Two of these exist for reasons that are not obvious from the name. **Keys**
/// reports whether the clip still carries `video_id` + `vod_offset`: Twitch
/// nulls both when the parent VOD expires, and without them a deleted clip can
/// only be recovered from its own CDN object, never rebuilt from the broadcast.
/// It is the single best predictor of whether this row is recoverable, so it is
/// a column rather than a hover. **Offset** is where the clip sits inside that
/// VOD, which is what a rebuild actually cuts on.
///
/// Each `id` is a stable persistence key: never reuse or change one once shipped.
pub(super) const CLIP_COLUMNS: [GridCol; 14] = [
    GridCol { id: "platform", title: "",         tooltip: "Source platform.", min_width: 26.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "title",    title: "Clip",     tooltip: "The clip's title. Hover for the full text; click a row's Actions to open it.", min_width: 200.0, initial: 260.0, sortable: true, stretch: false },
    GridCol { id: "channel",  title: "Channel",  tooltip: "Broadcaster the clip is of. Blank means a clip of a channel you don't monitor (found in chat).", min_width: 110.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "clipper",  title: "Clipped by", tooltip: "Who made the clip.", min_width: 100.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "created",  title: "Created",  tooltip: "When the clip was made — not when the broadcast happened.", min_width: 88.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "length",   title: "Length",   tooltip: "Clip duration.", min_width: 62.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "views",    title: "Views",    tooltip: "View count at the last sweep.", min_width: 70.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "keys",     title: "🔑",        tooltip: "Whether this clip still carries its recovery keys (the parent VOD id + offset).\n\n🔑 = recoverable: if the clip is deleted it can be rebuilt from the broadcast.\n— = keys already expired: Twitch drops them when the parent VOD does, so only the clip's own CDN copy could ever be recovered.\n\nThey're captured within days of the broadcast or not at all.", min_width: 34.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "offset",   title: "At",       tooltip: "Where the clip starts inside the parent VOD — the point a rebuild cuts from.", min_width: 66.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "stream",   title: "Stream",   tooltip: "The local recording of the broadcast this clip came from, when we have one. That's what makes a lost clip cuttable without touching the network.", min_width: 100.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "state",    title: "State",    tooltip: "indexed / queued / downloading / archived / gone / failed. A 'gone' clip has vanished upstream.", min_width: 96.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "size",     title: "Size",     tooltip: "Size of the archived file.", min_width: 72.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "file",     title: "File",     tooltip: "Archived file path. Hover for the full path.", min_width: 150.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "actions",  title: "Actions",  tooltip: "Per-row actions: play, open the clip page, copy URL, download, recover, delete.", min_width: 150.0, initial: 0.0, sortable: false, stretch: true },
];

/// Sortable/filterable Clips columns (Platform..File; excludes Actions).
pub(super) const CLIP_COLS: usize = 13;

/// The 📥 Backlog columns, in DEFAULT display order — one row per *broadcast*
/// (a `StreamGroup`), flat and newest-first across every channel.
///
/// Deliberately not the Streams columns: Streams is a tree grouped under
/// channel containers and its rows are monitors, so its per-instance columns
/// (On/Auto/Tool/Detection/Polled/Next stream/Added) are meaningless here.
/// What Backlog needs is "what was this broadcast, and have I watched it" —
/// hence Watch + the recording-shaped columns, and nothing about live state.
/// Each `id` is a stable persistence key: never reuse or change one once
/// shipped.
pub(super) const BACKLOG_COLUMNS: [GridCol; 13] = [
    GridCol { id: "watch",     title: "Watch",   tooltip: "Watch state for this broadcast: ◻ Unwatched, ▶ Started, ⏭ Skipped, ✔ Watched. Playing a take (or tuning into the channel live) advances Unwatched/Skipped to Started automatically; it never downgrades one you already marked. State belongs to the BROADCAST, so a reconnect that produced several takes shares one.", min_width: 130.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "platform",  title: "Plat",    tooltip: "Source platform of the channel this broadcast came from.", min_width: 46.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "channel",   title: "Channel", tooltip: "Which channel broadcast this, with the capturing instance's profile picture (the channel's own when that account has none yet) — hold Alt while hovering a picture for a full-size preview. Click a row to select that channel in 📺 Streams.", min_width: 120.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "title",     title: "Title",   tooltip: "The broadcast's title (the newest take's logged title). Truncated — hover for the full text.", min_width: 100.0, initial: 260.0, sortable: true, stretch: false },
    GridCol { id: "game",      title: "Game",    tooltip: "The broadcast's game / category (the newest take's logged value). Truncated — hover for the full name.", min_width: 70.0, initial: 120.0, sortable: true, stretch: false },
    GridCol { id: "went_live", title: "Went Live", tooltip: "When the stream went live on the platform (a trailing \"~\" means it's our approximate time).", min_width: 96.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "started",   title: "Started", tooltip: "When recording started.", min_width: 92.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "duration",  title: "Duration", tooltip: "How much was captured, summed across every take of this broadcast.", min_width: 56.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "size",      title: "Size",    tooltip: "Total size on disk of this broadcast's takes. Blank once the files are gone (e.g. an expired rolling recording, or a manual delete) — the history row survives either way.", min_width: 62.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "chat",      title: "💬",      tooltip: "A chat log was captured for this broadcast. Click to open the chat replay.", min_width: 26.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "changes",   title: "✏",       tooltip: "Title / game-category changes logged during the broadcast.", min_width: 24.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "ads",       title: "📢",      tooltip: "Ad breaks detected during the broadcast; each is a hard cut. Hover for count + total time.", min_width: 24.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "status",    title: "●",       tooltip: "Rolled-up capture status for the broadcast: ⏺ recording, ✔ completed, ⚠ failed, ⚡ aborted.", min_width: 26.0, initial: 0.0, sortable: true, stretch: true },
];

/// Background "Active tasks" columns (no sort/filter — hide/reorder only).
pub(super) const BG_ACTIVE_COLUMNS: [GridCol; 5] = [
    GridCol { id: "channel", title: "Channel / Label", tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "rec_id",  title: "Rec ID",          tooltip: "The recording id this task is working on — cross-reference against the app log's `rec_id=…` fields. Blank for tasks not tied to one recording.", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "task",    title: "Task",            tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "detail",  title: "Detail",          tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: true },
    GridCol { id: "elapsed", title: "Elapsed",         tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
];

/// Background "Recent" columns (no sort/filter — hide/reorder only).
pub(super) const BG_RECENT_COLUMNS: [GridCol; 5] = [
    GridCol { id: "channel", title: "Channel / Label", tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "rec_id",  title: "Rec ID",          tooltip: "The recording id this task worked on — cross-reference against the app log's `rec_id=…` fields. Blank for tasks not tied to one recording.", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "task",    title: "Task",            tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "detail",  title: "Detail",          tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: true },
    GridCol { id: "outcome", title: "Outcome",         tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
];

/// Processes window columns (no sort/filter — hide/reorder only). `filename`
/// (the long capture/tmp-file name) is deliberately last and the only
/// stretch column — every other column stays compact and visible instead of
/// being squeezed off to the right by a long name in the middle.
pub(super) const PROCESSES_COLUMNS: [GridCol; 11] = [
    GridCol { id: "pid",      title: "PID",      tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "type",     title: "Type",     tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "name",     title: "Name",     tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "tool",     title: "Tool",     tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "drive",    title: "Drive",    tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "io",       title: "I/O",      tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "progress", title: "Progress", tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "status",   title: "Status",   tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "uptime",   title: "Uptime",   tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "actions",  title: "Actions",  tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "filename", title: "Filename", tooltip: "", min_width: 120.0, initial: 0.0, sortable: false, stretch: true },
];

/// Issues window columns (no sort/filter — hide/reorder only). Shared by all 5
/// row-rendering blocks (needs-remux, stuck-in-cache, missing, errors-no-file,
/// errors); the blank-titled `platform` column holds only an icon.
pub(super) const ISSUES_COLUMNS: [GridCol; 8] = [
    GridCol { id: "platform", title: "",        tooltip: "", min_width: 0.0,   initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "channel",  title: "Channel", tooltip: "", min_width: 100.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "started",  title: "Started", tooltip: "", min_width: 0.0,   initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "file",     title: "File",    tooltip: "", min_width: 160.0, initial: 0.0, sortable: false, stretch: true },
    GridCol { id: "size",     title: "Size",    tooltip: "", min_width: 60.0,  initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "type",     title: "Type",    tooltip: "", min_width: 80.0,  initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "status",   title: "Status",  tooltip: "", min_width: 130.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "actions",  title: "Actions", tooltip: "", min_width: 0.0,   initial: 0.0, sortable: false, stretch: false },
];

/// The static `GridCol` descriptor array for a given grid table — used by the
/// "⇕ Reorder columns…" window, which (unlike each table's own render code)
/// doesn't already have its column array in scope at the point it needs one.
pub(super) fn columns_for(table: GridTableId) -> &'static [GridCol] {
    match table {
        GridTableId::Streams => &STREAM_COLUMNS,
        GridTableId::Videos => &VIDEO_COLUMNS,
        GridTableId::BgActive => &BG_ACTIVE_COLUMNS,
        GridTableId::BgRecent => &BG_RECENT_COLUMNS,
        GridTableId::Processes => &PROCESSES_COLUMNS,
        GridTableId::Issues => &ISSUES_COLUMNS,
        GridTableId::Backlog => &BACKLOG_COLUMNS,
        GridTableId::Clips => &CLIP_COLUMNS,
    }
}

/// A human-readable name for a grid table's "⇕ Reorder columns…" window title
/// — `GridTableId::key()` is a settings-map key (`"streams_table"`), not
/// meant for display.
pub(super) fn table_display_name(table: GridTableId) -> &'static str {
    match table {
        GridTableId::Streams => "Streams",
        GridTableId::Videos => "Videos",
        GridTableId::BgActive => "Background (Active)",
        GridTableId::BgRecent => "Background (Recent)",
        GridTableId::Processes => "Processes",
        GridTableId::Issues => "Issues",
        GridTableId::Backlog => "Backlog",
        GridTableId::Clips => "Clips",
    }
}

/// Backing state for the "⇕ Reorder columns…" window: a working copy of one
/// table's persisted entries, edited freely (checkbox + ▲/▼) and only
/// written back — triggering exactly one save + one table reset — when the
/// user hits Apply. This exists specifically so dragging a column across
/// many positions doesn't force the live grid to reset on every intermediate
/// move, the way the inline header popup's immediate-apply ▲/▼ used to (see
/// [[grid-column-width-persistence]]).
pub(super) struct ReorderColumnsState {
    pub(super) table: GridTableId,
    pub(super) draft: Vec<ColumnEntry>,
    /// Set by the deferred closure on Apply/Cancel/close; read back by
    /// `reorder_columns_window` next call.
    pub(super) apply: bool,
    pub(super) cancel: bool,
}

/// One sort level: a column index (into the table's static `*_COLUMNS` array) +
/// direction. `SortState.keys` is the priority list, primary first.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct SortLevel {
    pub(super) col: usize,
    pub(super) ascending: bool,
}

/// A table's multi-level sort. An empty `keys` list keeps the natural (database)
/// order. Not `Copy` (holds a `Vec`); `PartialEq` drives the save-back
/// "changed?" check.
#[derive(Clone, Default, PartialEq)]
pub(super) struct SortState {
    pub(super) keys: Vec<SortLevel>,
}

impl SortState {
    /// Position of `col` in the priority list, if it's an active sort key.
    pub(super) fn level_of(&self, col: usize) -> Option<usize> {
        self.keys.iter().position(|l| l.col == col)
    }
    /// Plain header click: make `col` the sole key (ascending). If it's already
    /// the sole key, just flip its direction.
    pub(super) fn set_sole(&mut self, col: usize) {
        if self.keys.len() == 1 && self.keys[0].col == col {
            self.keys[0].ascending = !self.keys[0].ascending;
        } else {
            self.keys = vec![SortLevel { col, ascending: true }];
        }
    }
    /// Shift-click / "Add as secondary": flip direction if already a key, else
    /// append it as a new lowest-priority level (ascending).
    pub(super) fn toggle_or_append(&mut self, col: usize) {
        match self.level_of(col) {
            Some(p) => self.keys[p].ascending = !self.keys[p].ascending,
            None => self.keys.push(SortLevel { col, ascending: true }),
        }
    }
    /// Drop `col` from the priority list (no-op if absent).
    pub(super) fn remove_col(&mut self, col: usize) {
        self.keys.retain(|l| l.col != col);
    }
}

/// A cell's sort key: numeric columns sort numerically, text columns sort
/// case-insensitively — `Text` is stored PRE-LOWERCASED so the per-frame sort
/// compares without allocating. (Filtering always uses the displayed `text`.)
pub(super) enum SortKey {
    Num(f64),
    Text(String),
}

/// A precomputed cell: `text` is what's shown/filtered (case-insensitive
/// substring), `key` is what's sorted. `deep` extends the *filter* haystack
/// with what this row's collapsed descendants would show in the same column —
/// see [`Cell::push_deep`].
pub(super) struct Cell {
    pub(super) text: String,
    pub(super) key: SortKey,
    /// Additional lowercase filter haystack (never sorted or displayed):
    /// newline-joined values from the row's sub-rows, so filtering matches a
    /// value that lives on a collapsed instance/stream row instead of only
    /// the top-level rollup. Empty for flat tables (Videos).
    pub(super) deep: String,
}

impl Cell {
    /// A text cell — filter and sort both use the string.
    pub(super) fn text(s: impl Into<String>) -> Cell {
        let s = s.into();
        Cell {
            key: SortKey::Text(s.to_lowercase()),
            text: s,
            deep: String::new(),
        }
    }
    /// A numeric cell — sorts by `n`, filters/shows `display`.
    pub(super) fn num(n: f64, display: impl Into<String>) -> Cell {
        Cell {
            text: display.into(),
            key: SortKey::Num(n),
            deep: String::new(),
        }
    }
    /// Append a descendant's value to the filter haystack (lowercased,
    /// newline-separated so no substring can straddle two values). Empty
    /// strings are dropped rather than stacking blank lines.
    pub(super) fn push_deep(&mut self, s: &str) {
        let s = s.trim();
        if s.is_empty() {
            return;
        }
        if !self.deep.is_empty() {
            self.deep.push('\n');
        }
        self.deep.push_str(&s.to_lowercase());
    }
}

/// One Streams-table channel container + its instance-row indices into
/// `StreamArchiverApp::rows`.
pub(super) struct ChanEntry {
    pub(super) channel: Channel,
    pub(super) rows: Vec<usize>,
}

/// The Streams view's frame-invariant data, cached across repaints (see the
/// rebuild block in `channels_view`). `stamp` is (unix second — 0 while no
/// capture is active, so an idle grid never rebuilds on time alone; cache rev).
pub(super) struct StreamsViewCache {
    pub(super) stamp: (i64, u64),
    pub(super) chan_entries: Vec<ChanEntry>,
    pub(super) channel_avatars: HashMap<i64, egui::TextureHandle>,
    /// Per-instance (monitor-id-keyed) avatar: the icon of that instance's own account.
    pub(super) instance_avatars: HashMap<i64, egui::TextureHandle>,
    pub(super) channel_name_colors: HashMap<i64, (egui::Color32, bool)>,
    pub(super) groups: HashMap<i64, Vec<StreamGroup>>,
    /// Every currently-`"recording"` take, by monitor id — a cheap global
    /// query (unlike `groups`, which only holds data for expanded monitors)
    /// so a live capture's "Play local recording (start)"/"Backfill head" stay usable on
    /// a collapsed instance row instead of reading as "nothing to play".
    pub(super) active_recordings: HashMap<i64, Vec<crate::models::Recording>>,
    /// Lowercase Twitch login → monitor id, one entry per locally-tracked
    /// Twitch instance — lets a collab partner (known only by login) be
    /// resolved to a local monitor for "Play collab instance"/"Play all
    /// collab instances" without an O(partners × rows) scan per row.
    pub(super) twitch_login_to_mid: HashMap<String, i64>,
    /// Each monitor's most recent `raid_out` event, if any — powers the
    /// "Follow raid" play action's enabled state and target.
    pub(super) latest_raid_out: HashMap<i64, crate::models::StreamEventRow>,
    /// Takes still counting down towards rolling auto-deletion, by monitor id
    /// (absent = none), with the soonest deadline among them — the 🕰 rollup
    /// badge and column on instance and channel rows. A DB read, so it lives
    /// here rather than in the render path; see [`crate::rolling`].
    pub(super) rolling_rollups: HashMap<i64, crate::rolling::RollingRollup>,
    /// Drive letters each monitor's takes are stored on (absent = nothing
    /// stored) — the 🖴 column on channel and instance rows, which have no
    /// per-take data loaded to derive it themselves. A DB read, so it lives
    /// here rather than in the render path.
    pub(super) monitor_drives: HashMap<i64, Vec<char>>,
    /// Clip counts `(total, archived)` per parent VOD id — the 🎞 summary row
    /// under a broadcast, and whether a single-take broadcast is expandable at
    /// all. A DB read, so it lives here rather than in the render path.
    pub(super) clips_by_vod: HashMap<String, (i64, i64)>,
    /// Per-monitor sum of finished-take bytes — the "Disk use" column on
    /// channel/instance rows. See `Store::monitor_disk_usage`'s doc comment
    /// for why this is a coarser figure than what period/stream/take rows
    /// show (those confirm each file against disk; this doesn't).
    pub(super) monitor_disk_usage: HashMap<i64, i64>,
    pub(super) model: Vec<Vec<Cell>>,
    /// Snapshot of the preferred-platform-when-multiple-live config, loaded
    /// once per rebuild rather than per channel row per frame.
    pub(super) platform_pref: crate::platform_pref::PlatformPrefCtx,
    /// Live instances currently standing by because a sibling is recording
    /// this broadcast on the preferred platform, mapped to that platform's
    /// label — the ⇄ badge. Derived during the rebuild (it reads the simulcast
    /// settings), never per frame. See [`crate::simulcast`].
    pub(super) simulcast_standby: HashMap<i64, String>,
}

pub(super) fn cmp_sort_key(a: &SortKey, b: &SortKey) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (SortKey::Num(x), SortKey::Num(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        // Both sides pre-lowercased at construction — no per-comparison allocs.
        (SortKey::Text(x), SortKey::Text(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Filter then sort `rows`, returning surviving original indices in display
/// order. `filters[c]` is a case-insensitive substring filter for column `c`.
pub(super) fn ordered_rows(rows: &[Vec<Cell>], sort: &SortState, filters: &[String]) -> Vec<usize> {
    // Lowercase the filters ONCE (this used to run per row × per column, every
    // frame — thousands of allocations just to find out the filters are empty)
    // and only test the columns that actually have a filter.
    let active: Vec<(usize, String)> = filters
        .iter()
        .enumerate()
        .filter_map(|(c, f)| {
            let f = f.trim().to_lowercase();
            (!f.is_empty()).then_some((c, f))
        })
        .collect();
    let mut idx: Vec<usize> = if active.is_empty() {
        (0..rows.len()).collect()
    } else {
        (0..rows.len())
            .filter(|&i| {
                active.iter().all(|(c, f)| {
                    rows[i]
                        .get(*c)
                        .map(|cell| {
                            let own = match &cell.key {
                                // Text cells store a pre-lowercased sort key —
                                // match it instead of re-lowercasing the display
                                // text per row per frame.
                                SortKey::Text(k) => k.contains(f.as_str()),
                                SortKey::Num(_) => cell.text.to_lowercase().contains(f.as_str()),
                            };
                            // A value on a collapsed sub-row counts too (the
                            // `deep` haystack) — the top-level rollup often
                            // shows none of what its descendants do.
                            own || cell.deep.contains(f.as_str())
                        })
                        .unwrap_or(true)
                })
            })
            .collect()
    };
    if !sort.keys.is_empty() {
        // Fold the priority list into a short-circuiting comparator chain: each
        // level breaks ties left by the higher-priority ones. `sort_by` is
        // stable, so rows equal on every key keep their natural (DB) order — so
        // equal-primary rows cluster together with no divider rows needed.
        idx.sort_by(|&a, &b| {
            sort.keys.iter().fold(std::cmp::Ordering::Equal, |acc, level| {
                acc.then_with(|| {
                    let o = match (rows[a].get(level.col), rows[b].get(level.col)) {
                        (Some(x), Some(y)) => cmp_sort_key(&x.key, &y.key),
                        _ => std::cmp::Ordering::Equal,
                    };
                    if level.ascending { o } else { o.reverse() }
                })
            })
        });
    }
    idx
}

/// The Streams grid's active column filters, resolved to `(column id,
/// lowercase needle)` pairs — the render-side companion to the deep filter in
/// [`ordered_rows`]. Where that decides which channel rows *survive*, this
/// marks where the match actually *lives* once you look at the survivors:
/// which instance/stream/take rows contain it (row tint, [`HL_FILTER_HIT`])
/// and which substring matched (text highlight, [`highlight_text_label`]).
/// Without it, a channel kept visible by a collapsed YouTube instance's
/// stream title looks identical to one kept by its Twitch instance — exactly
/// the reported confusion.
pub(super) struct FilterHits {
    /// One entry per non-empty filter box.
    active: Vec<(&'static str, String)>,
}

/// `Some(matched)` when a column's filter has row-level data to test on this
/// row kind, `None` when the column only exists at channel level (neutral —
/// it can neither confirm nor rule out this row).
type ColMatch = Option<bool>;

impl FilterHits {
    /// Resolve the header filter boxes (indexed by `STREAM_COLUMNS` position)
    /// into active `(id, lowercase needle)` pairs. `None` when no filter is
    /// set — callers skip all hit work in the common case.
    pub(super) fn from_filters(filters: &[String]) -> Option<FilterHits> {
        let active: Vec<(&'static str, String)> = filters
            .iter()
            .enumerate()
            .filter_map(|(c, f)| {
                let f = f.trim().to_lowercase();
                (!f.is_empty()).then(|| (STREAM_COLUMNS.get(c).map(|col| col.id).unwrap_or(""), f))
            })
            .collect();
        (!active.is_empty()).then_some(FilterHits { active })
    }

    /// The lowercase needle filtering `col_id`, if any — for highlighting the
    /// matched substring inside that column's text cells.
    pub(super) fn needle(&self, col_id: &str) -> Option<&str> {
        self.active.iter().find(|(id, _)| *id == col_id).map(|(_, n)| n.as_str())
    }

    /// Fold one column's verdict into the row decision. The row is a hit when
    /// every active filter is matched-or-neutral AND at least one actually
    /// matched at this row's level — all-neutral means the filters were
    /// satisfied purely by channel-level values (e.g. the channel name), and
    /// tinting every sub-row for that would just be noise.
    fn resolve(verdicts: impl Iterator<Item = ColMatch>) -> bool {
        let mut any = false;
        for v in verdicts {
            match v {
                Some(true) => any = true,
                Some(false) => return false,
                None => {}
            }
        }
        any
    }

    /// Does THIS instance contain the filters' matches — its own values plus
    /// its logged title/category history (`rec_texts`, the same per-monitor
    /// haystacks the deep filter uses), so a hit inside a *collapsed* stream
    /// history still marks the instance row that holds it.
    pub(super) fn instance_hit(
        &self,
        m: &MonitorWithChannel,
        rec_texts: Option<&(String, String)>,
    ) -> bool {
        let has = |s: &str, f: &str| s.to_lowercase().contains(f);
        Self::resolve(self.active.iter().map(|(id, f)| -> ColMatch {
            match *id {
                "name" => Some(has(&m.monitor.url, f)),
                "platform" => Some(has(m.monitor.platform().label(), f)),
                "tool" => Some(has(m.monitor.tool.label(), f)),
                "detection" => Some(has(m.monitor.detection_method.label(), f)),
                "game" => Some(
                    has(&m.last_game, f)
                        || has(&m.last_recording_category, f)
                        || rec_texts.is_some_and(|(_, c)| c.contains(f.as_str())),
                ),
                "title" => Some(
                    has(&m.last_title, f)
                        || has(&m.last_recording_title, f)
                        || rec_texts.is_some_and(|(t, _)| t.contains(f.as_str())),
                ),
                "collab" => Some(m.live_collab.as_ref().is_some_and(|c| has(&c.names(), f))),
                "tags" => Some(has(&m.last_tags, f)),
                _ => None,
            }
        }))
    }

    /// Does this stream row (any of its takes) contain the matches. `collab`
    /// is the broadcast's stored collab names, when known.
    pub(super) fn stream_hit(&self, g: &crate::models::StreamGroup, collab: Option<&str>) -> bool {
        Self::resolve(self.active.iter().map(|(id, f)| -> ColMatch {
            match *id {
                "game" => Some(g.takes.iter().any(|t| t.category.to_lowercase().contains(f.as_str()))),
                "title" => Some(g.takes.iter().any(|t| t.title.to_lowercase().contains(f.as_str()))),
                "collab" => Some(collab.is_some_and(|c| c.to_lowercase().contains(f.as_str()))),
                _ => None,
            }
        }))
    }

    /// Does this single take contain the matches.
    pub(super) fn take_hit(&self, t: &crate::models::Recording) -> bool {
        Self::resolve(self.active.iter().map(|(id, f)| -> ColMatch {
            match *id {
                "game" => Some(t.category.to_lowercase().contains(f.as_str())),
                "title" => Some(t.title.to_lowercase().contains(f.as_str())),
                _ => None,
            }
        }))
    }
}

/// A truncating label that paints every case-insensitive occurrence of
/// `needle` in `text` on a [`HL_FILTER_TEXT_BG`] pill — how a filtered grid
/// shows *which part* of a cell matched. Falls back to a plain label when
/// there's no needle or no occurrence, so unfiltered rendering pays nothing.
pub(super) fn highlight_text_label(ui: &mut egui::Ui, text: &str, needle: Option<&str>) {
    let Some(needle) = needle.filter(|n| !n.is_empty()) else {
        ui.add(egui::Label::new(text).truncate());
        return;
    };
    let lower = text.to_lowercase();
    if !lower.contains(needle) {
        ui.add(egui::Label::new(text).truncate());
        return;
    }
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let color = ui
        .style()
        .visuals
        .override_text_color
        .unwrap_or_else(|| ui.visuals().text_color());
    let mut job = egui::text::LayoutJob::default();
    let mut pos = 0;
    // Walk the LOWERCASED text for match positions, slice the ORIGINAL text
    // at the same byte offsets — `to_lowercase` can change byte length for a
    // handful of scripts, but per-char lowercasing of the kinds of text here
    // (titles/categories) is length-stable; a mismatch would only ever skew
    // which chars get the pill, never panic, since `find` offsets are always
    // char boundaries of `lower` and `get` guards the original slice.
    while let Some(rel) = lower[pos..].find(needle) {
        let (start, end) = (pos + rel, pos + rel + needle.len());
        let (Some(before), Some(hit)) = (text.get(pos..start), text.get(start..end)) else {
            break; // boundary skew — bail to plain text for the rest
        };
        job.append(before, 0.0, egui::TextFormat::simple(font_id.clone(), color));
        job.append(
            hit,
            0.0,
            egui::TextFormat {
                font_id: font_id.clone(),
                color: egui::Color32::WHITE,
                background: HL_FILTER_TEXT_BG,
                ..Default::default()
            },
        );
        pos = end;
    }
    job.append(
        text.get(pos..).unwrap_or(""),
        0.0,
        egui::TextFormat::simple(font_id, color),
    );
    ui.add(egui::Label::new(job).truncate());
}

/// Render one sortable + optionally filterable header cell for column `idx`: a
/// click-to-sort title (with ▲/▼ when active, plus a plain-digit level ordinal
/// when the sort is multi-level) above a filter box. Plain click = sole key;
/// Shift-click = add/toggle an additional level (matching the context menu).
pub(super) fn sort_filter_header(
    ui: &mut egui::Ui,
    idx: usize,
    title: &str,
    tooltip: &str,
    filterable: bool,
    sort: &mut SortState,
    filter: &mut String,
) {
    ui.vertical(|ui| {
        // Arrow shows direction; when there are ≥2 keys, a plain digit shows this
        // column's 1-based priority ("▲1" primary, "▲2" secondary, …). Plain
        // digits (not superscripts) to avoid font/tofu risk for ordinals ≥4.
        let arrow = match sort.level_of(idx) {
            Some(p) => {
                let dir = if sort.keys[p].ascending { "▲" } else { "▼" };
                if sort.keys.len() >= 2 {
                    format!(" {dir}{}", p + 1)
                } else {
                    format!(" {dir}")
                }
            }
            None => String::new(),
        };
        let hover_base = if tooltip.is_empty() {
            String::new()
        } else {
            format!("{tooltip}\n\n")
        };
        let hover = format!(
            "{hover_base}Click to sort (again to reverse) · Shift-click to add a \
             sort level · Right-click for options."
        );
        let resp = ui
            .add(egui::Button::new(egui::RichText::new(format!("{title}{arrow}")).strong()).frame(false))
            .on_hover_text(hover);
        if resp.clicked() {
            if ui.input(|i| i.modifiers.shift) {
                sort.toggle_or_append(idx);
            } else {
                sort.set_sole(idx);
            }
        }
        if filterable {
            ui.add(
                egui::TextEdit::singleline(filter)
                    .hint_text("filter")
                    .desired_width(f32::INFINITY),
            )
            .on_hover_text(
                "Case-insensitive text filter for this column. In tables with \
                 sub-rows (Streams), it also matches values on collapsed \
                 sub-rows — an instance's URL/tool, and every title/category \
                 its stream history ever logged — keeping the channel row \
                 visible. The instance / stream / take rows that actually \
                 contain the match are tinted teal (expand to follow the \
                 trail), and the matched text itself is highlighted in the \
                 cells that show it.",
            );
        }
    });
}

/// Render one grid-table header cell: sortable columns get the existing
/// click-to-sort + filter box ([`sort_filter_header`]); non-sortable get a
/// plain strong label. Every cell also gets a right-click column-chooser
/// context menu shared by every grid table — a quick "Hide this column"
/// action (skipped for `locked` ids, whose visibility is controlled elsewhere;
/// see [`ColumnEntry`]/[`grid_columns::column_chooser_editor`]) followed by
/// the full show/hide + reorder list (adapted from `source_list_inline_editor`).
/// The whole-cell `ctx_resp`/`Sense::click()` interaction (emote-grid re-interact
/// pattern) is created FIRST so the sort button / filter box, added afterwards,
/// sit on top and win their own left-clicks (see the ordering note in the body);
/// the ctx_resp then catches right-clicks over the rest of the cell.
#[allow(clippy::too_many_arguments)]
pub(super) fn grid_header_cell(
    ui: &mut egui::Ui,
    table: GridTableId,
    idx: usize,
    col: &GridCol,
    filterable: bool,
    sort: &mut SortState,
    filter: &mut String,
    entries: &mut [ColumnEntry],
    columns: &[GridCol],
    locked: impl Fn(&str) -> bool,
) -> bool {
    // Register the whole-cell right-click interaction BEFORE rendering the sort
    // button / filter box, so those (added afterwards) sit ON TOP and win their
    // own clicks. egui's hit-test breaks overlap ties in favor of the
    // last-added widget (egui-0.34 `hit_test::find_closest_within`: "in case of
    // a tie, take the last one = the one on top"). If this ctx_resp were created
    // *after* the frameless sort button, it would swallow every left-click and
    // the header would never sort — while the right-click menu still worked.
    let ctx_resp = ui.interact(
        ui.max_rect(),
        egui::Id::new(("grid_col_ctx", table.key(), col.id)),
        egui::Sense::click(),
    );
    if col.sortable {
        sort_filter_header(ui, idx, col.title, col.tooltip, filterable, sort, filter);
    } else if !col.title.is_empty() {
        ui.strong(col.title).on_hover_text(col.tooltip);
    }
    let mut open_reorder = false;
    ctx_resp.context_menu(|ui| {
        ui.set_min_width(200.0);
        if col.sortable {
            if ui.button("⬍  Sort by this column").clicked() {
                sort.keys = vec![SortLevel { col: idx, ascending: true }];
                ui.close();
            }
            if ui.button("➕  Add as secondary sort").clicked() {
                sort.toggle_or_append(idx);
                ui.close();
            }
            if sort.level_of(idx).is_some() && ui.button("➖  Remove from sort").clicked() {
                sort.remove_col(idx);
                ui.close();
            }
            if !sort.keys.is_empty() && ui.button("✖  Clear sort").clicked() {
                sort.keys.clear();
                ui.close();
            }
            ui.separator();
        }
        if !locked(col.id) && !col.title.is_empty() && ui.button(format!("🚫  Hide '{}'", col.title)).clicked() {
            grid_columns::set_visible(entries, col.id, false);
            ui.close();
        }
        if ui.button("⇕  Reorder columns…").clicked() {
            open_reorder = true;
            ui.close();
        }
        ui.separator();
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            grid_columns::column_chooser_editor(ui, entries, columns, &locked, false);
        });
    });
    open_reorder
}

/// Simpler variant of [`grid_header_cell`] for tables with no sort/filter (the
/// 4 "simple" tables: Background Active/Recent, Processes, Issues) — just the
/// plain label plus the shared column-chooser context menu. Returns true when
/// "⇕ Reorder columns…" was clicked this frame (caller opens the dedicated
/// apply-once window — see [`grid_header_cell`]'s doc on why reordering isn't
/// inline here).
pub(super) fn grid_header_cell_plain(
    ui: &mut egui::Ui,
    table: GridTableId,
    col: &GridCol,
    entries: &mut [ColumnEntry],
    columns: &[GridCol],
) -> bool {
    if !col.title.is_empty() {
        ui.strong(col.title).on_hover_text(col.tooltip);
    }
    let ctx_resp = ui.interact(
        ui.max_rect(),
        egui::Id::new(("grid_col_ctx", table.key(), col.id)),
        egui::Sense::click(),
    );
    let mut open_reorder = false;
    ctx_resp.context_menu(|ui| {
        ui.set_min_width(200.0);
        if !col.title.is_empty() && ui.button(format!("🚫  Hide '{}'", col.title)).clicked() {
            grid_columns::set_visible(entries, col.id, false);
            ui.close();
        }
        if ui.button("⇕  Reorder columns…").clicked() {
            open_reorder = true;
            ui.close();
        }
        ui.separator();
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            grid_columns::column_chooser_editor(ui, entries, columns, |_| false, false);
        });
    });
    open_reorder
}

/// Sort/filter cells for one video row, in Videos-table column order:
/// Video, Channel, Platform, Tool, Status, Speed, Size, Added, File.
pub(super) fn video_cells(
    v: &Video,
    speed: &std::collections::HashMap<i64, f64>,
) -> Vec<Cell> {
    let label = if v.title.trim().is_empty() {
        v.url.clone()
    } else {
        v.title.clone()
    };
    // Speed is only meaningful while actively downloading.
    let spd = if v.status == "downloading" {
        speed.get(&v.id).copied().unwrap_or(0.0)
    } else {
        0.0
    };
    vec![
        Cell::text(label),
        Cell::text(v.channel.clone()),
        Cell::text(v.platform.label()),
        Cell::text(v.tool.label()),
        Cell::text(v.status.clone()),
        Cell::num(spd, fmt_speed(spd)),
        Cell::num(
            v.bytes as f64,
            if v.bytes > 0 { fmt_bytes(v.bytes) } else { String::new() },
        ),
        Cell::num(v.created_at as f64, fmt_date(v.created_at)),
        Cell::text(v.output_path.clone()),
    ]
}

/// Columns derived from a monitor's latest recording.
pub(super) struct RecordingCells {
    /// True while the take is still in progress (status == "recording").
    pub(super) active: bool,
    /// When *we* started recording (formatted for filter/sort; render via [`ts_label`]).
    pub(super) started_on: String,
    /// Raw unix seconds for "Started On" — used by [`ts_label`] for compact tooltips.
    pub(super) started_secs: i64,
    /// How long we've recorded (ticks while active; final length otherwise).
    pub(super) duration: String,
    /// Raw seconds behind `duration` — numeric sort key (0 when unknown).
    pub(super) duration_secs: i64,
    /// When the stream went live on the platform (`~`-prefixed if approximate, formatted).
    pub(super) went_live: String,
    /// Raw unix seconds for "Went Live" — used by [`ts_went_live_label`].
    pub(super) went_live_secs: i64,
    /// True when the went-live timestamp is our approximation, not the platform's.
    pub(super) went_live_approx: bool,
    /// How much of the beginning we missed.
    pub(super) lost: String,
    /// Raw seconds behind `lost` — numeric sort key (0 when unknown).
    pub(super) lost_secs: i64,
}

pub(super) fn recording_cells(row: &MonitorWithChannel, now: i64) -> RecordingCells {
    let active = row.last_recording_status.as_deref() == Some("recording");
    // Not recording (e.g. Auto off) but currently live: fall back to the
    // poll-detected go-live time instead of whatever (possibly old/unrelated)
    // recording happens to be "latest" for this instance, so Went Live/Started
    // On/Duration still show something for the CURRENT live session. There's no
    // separate "recording start" here, so Started On mirrors Went Live, and
    // Lost time doesn't apply (nothing is being captured).
    if !active && row.monitor.last_state == "live" && let Some(w) = row.monitor.last_live_since {
        let approx = row.monitor.last_live_since_approx;
        let went_live = {
            let s = fmt_datetime_short(w);
            if approx { format!("{s}~") } else { s }
        };
        let dur = (now - w).max(0);
        return RecordingCells {
            active: false,
            started_on: went_live.clone(),
            started_secs: w,
            duration: fmt_duration(dur),
            duration_secs: dur,
            went_live,
            went_live_secs: w,
            went_live_approx: approx,
            lost: String::new(),
            lost_secs: 0,
        };
    }
    if !active {
        // The instance/channel row represents PRESENT state, not history — a
        // finished take's Went Live/Started On/Duration/Lost time belong on
        // that take's own Stream/Take row in the expanded tree (see
        // `take_status_badges` and friends), not here. Neither active above
        // nor currently live (that returned already) means genuinely idle:
        // blank every time cell instead of resurfacing whatever recording
        // happens to be "latest" for this instance.
        return RecordingCells {
            active: false,
            started_on: String::new(),
            started_secs: 0,
            duration: String::new(),
            duration_secs: 0,
            went_live: String::new(),
            went_live_secs: 0,
            went_live_approx: false,
            lost: String::new(),
            lost_secs: 0,
        };
    }
    // Active: show the in-progress take's own live-ticking stats.
    let started = row.last_recording_started;
    let started_secs = started.unwrap_or(0);
    let dur = started.map(|s| now - s);
    let went_live_secs = row.last_recording_went_live.unwrap_or(0);
    let went_live_approx = row.last_recording_went_live_approx;
    let went_live = match row.last_recording_went_live {
        Some(w) => {
            let s = fmt_datetime_short(w);
            if went_live_approx {
                format!("{s}~")
            } else {
                s
            }
        }
        None => String::new(),
    };
    // Prefer the resolved lost time (0 once a from-start capture caught up, or
    // the exact residual) when known; else fall back to started - went_live.
    let lost_val: Option<i64> = match row.last_recording_lost_secs {
        Some(s) => Some(s.max(0)),
        None => match (started, row.last_recording_went_live) {
            (Some(s), Some(w)) => Some((s - w).max(0)),
            _ => None,
        },
    };
    RecordingCells {
        active,
        started_on: started.map(fmt_datetime_short).unwrap_or_default(),
        started_secs,
        duration: dur.map(fmt_duration).unwrap_or_default(),
        duration_secs: dur.unwrap_or(0).max(0),
        went_live,
        went_live_secs,
        went_live_approx,
        lost: lost_val.map(fmt_duration).unwrap_or_default(),
        lost_secs: lost_val.unwrap_or(0),
    }
}

/// Theme color for a recording / stream status string.
/// Short abbreviation for the Tool column — narrower than the full label.
pub(super) fn short_tool_label(tool: crate::models::Tool) -> &'static str {
    match tool {
        crate::models::Tool::Streamlink => "SL",
        crate::models::Tool::YtDlp => "yt-dlp",
        crate::models::Tool::Ffmpeg => "ff",
    }
}

/// Icon for the Detection column — one or two Unicode chars that convey the
/// detection mechanism. Tooltip shows the full label + explanation.
pub(super) fn detection_icon(m: crate::models::DetectionMethod) -> &'static str {
    use crate::models::DetectionMethod::*;
    match m {
        TwitchApi | YouTubeApi | KickApi => "↺",  // API polling
        Scrape => "⌁",                            // page scrape
        CliSelfPoll => "C",                       // CLI retry loop
        GenericProbe => "◉",                      // HTTP probe
        EventSub => "⚡",                          // pure push event
        EventSubHelix => "⚡↺",                   // push + poll fallback
        WebSub | WebSubOnly => "⚡",             // WebSub push
        Disabled => "⛔",                          // no auto-detection at all
    }
}

/// Hover text for the "finalizing" state (capture over, finalize pending).
pub(super) const FINALIZING_HOVER: &str =
    "Capture ended — finalizing: the remux/promote into the output dir is \
     running or queued at the disk gate (large backlogs can take hours). \
     Watch progress and the queue in the Background view.";

/// Hover for the ⏬ state badge: the channel is offline but its capture tool
/// hasn't exited yet.
pub(super) const CAPTURE_OFFLINE_HOVER: &str =
    "Stream ENDED — the channel is offline, but the capture is still \
     finishing: a live-from-start capture keeps downloading the stream's \
     recorded backlog until it reaches the end, and huge files mux for a \
     while afterwards. The recording completes on its own; nothing is being \
     missed. (⏹ still stops it early if you don't want the rest.)";

/// Icon + color for the State column. Returns `(icon, text_color)`.
pub(super) fn state_icon(state: &str) -> (&'static str, egui::Color32) {
    use egui::Color32;
    match state {
        "recording" => ("⏺", Color32::from_rgb(0x4d, 0x9b, 0xff)), // blue
        "finalizing" => ("⌛", Color32::from_rgb(0xd8, 0xb4, 0x54)), // amber — capture over, remux pending
        "live" => ("●", SUCCESS_GREEN),                              // green (live not yet recording)
        "failed" => ("⚠", HL_ERROR_TEXT),                           // red
        "stopped" => ("⏹", Color32::from_gray(0xa0)),               // gray
        "aborted" => ("⚡", Color32::from_rgb(0xe0, 0xa8, 0x50)),   // amber
        "ended" => ("✔", Color32::from_gray(0xa0)),                  // gray
        "not_recorded" => ("👁", Color32::from_gray(0xa0)),          // gray — seen live, Auto was off
        "completed" => ("✔", SUCCESS_GREEN),                         // green
        _ => ("○", Color32::from_gray(0x70)),                        // idle/unknown — dim
    }
}

/// [`state_icon`], but an acknowledged `failed` status (see
/// `Recording::err_ack`) keeps its ⚠ glyph — so the failure stays visible at
/// its own row/rollup — rendered muted gray instead of red, rather than the
/// normal alarming red. Everything else is unchanged.
pub(super) fn state_icon_ack(state: &str, err_ack: bool) -> (&'static str, egui::Color32) {
    if err_ack && state == "failed" {
        ("⚠", egui::Color32::from_gray(0x70))
    } else {
        state_icon(state)
    }
}

/// Trigger-word / VOD-state / live-DVR-backfill status badges for the
/// Streams-tree "state" cell. Shared by the Stream row (rolled up across all
/// its takes — the only place these are visible for the common single-take
/// case, since a lone take never gets its own `Vis::Take` sub-row) and each
/// individual Take row of a multi-take stream.
#[allow(clippy::too_many_arguments)]
pub(super) fn take_status_badges(
    ui: &mut egui::Ui,
    trigger_info: &str,
    vod_not_published: bool,
    vod_muted_secs: Option<i64>,
    full_backfilled: bool,
    head_backfilled: bool,
    backfill_running: bool,
    backfill_queued: bool,
    sabr_live_edge_fallback: bool,
    chapters_state: &str,
    gap_recover_running: bool,
    // Capture-alert rollup for this take (or summed over a stream's takes).
    alert: Option<&crate::store::RecAlertBadge>,
) -> bool {
    let mut open_warnings = false;
    if !trigger_info.is_empty() {
        ui.colored_label(
            egui::Color32::from_rgb(0xe8, 0xc5, 0x4a),
            egui::RichText::new("⚡").small(),
        )
        .on_hover_text(format!("Started by a trigger word: {trigger_info}"));
    }
    if vod_not_published {
        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), "⚠ no VOD")
            .on_hover_text("No VOD was published — this local recording may be the only surviving copy.");
    } else if let Some(secs) = vod_muted_secs.filter(|&s| s > 0) {
        ui.colored_label(egui::Color32::from_rgb(220, 160, 30), "✂ muted")
            .on_hover_text(format!(
                "VOD has {} of muted content (DMCA) — local recording is the authoritative archive.",
                fmt_duration(secs)
            ));
    }
    if full_backfilled {
        ui.colored_label(egui::Color32::from_rgb(70, 180, 90), "🧩 full")
            .on_hover_text("Missed start was backfilled from the live VOD and joined with the capture (see {stem}.full.mkv).");
    } else if head_backfilled {
        ui.colored_label(egui::Color32::from_rgb(80, 160, 220), "🧩 head")
            .on_hover_text("Missed start was backfilled from the live VOD ({stem}.head.mkv) — the joined file lands after the recording finishes.");
    } else if backfill_running {
        ui.colored_label(egui::Color32::from_rgb(80, 160, 220), "⏳ backfilling…")
            .on_hover_text("Fetching the missed start from the live VOD — check the Background tab for progress, or right-click this take → \"Abort backfill\" to stop it.");
    } else if backfill_queued {
        ui.colored_label(egui::Color32::from_gray(0xa0), "⏳ backfill queued")
            .on_hover_text(format!(
                "Will check for a missed start to backfill from the live VOD shortly \
                 (waiting ~{}s for the CDN/stream to settle).",
                crate::downloader::HEAD_BACKFILL_SETTLE_SECS
            ));
    } else if sabr_live_edge_fallback {
        ui.colored_label(egui::Color32::from_rgb(220, 175, 60), "🕘 live edge only")
            .on_hover_text(
                "This broadcast was already older than YouTube SABR's DVR rewind window \
                 (~4h) when this take started, so every attempt to capture from the true \
                 start stalled immediately — captured from the live edge instead. There is \
                 no missed-intro head for this take and there never will be; this isn't a \
                 failure, just a limitation of how far back SABR can rewind.",
            );
    }
    if chapters_state == "done" {
        ui.colored_label(egui::Color32::from_rgb(140, 160, 220), "📑 chapters")
            .on_hover_text(
                "Chapter markers (title/category changes, raids, recovered/muted gap-splice \
                 segments) are embedded in this file — open it in a player that shows \
                 chapters (e.g. mpv, VLC) to scrub through them.",
            );
    }
    if gap_recover_running {
        ui.colored_label(egui::Color32::from_rgb(220, 120, 60), "🩹 recovering gaps…")
            .on_hover_text(
                "This capture lost segments (see 🚨 Warnings) — re-fetching the lost time \
                 ranges from the VOD CDN into patch files next to the recording. Progress \
                 is in the Background tab.",
            );
    }
    // Persistent capture-alert state (all badges click through to 🚨 Warnings).
    if let Some(a) = alert {
        let clickable = |ui: &mut egui::Ui, color: egui::Color32, text: String| {
            ui.add(
                egui::Label::new(egui::RichText::new(text).color(color))
                    .sense(egui::Sense::click()),
            )
        };
        let healed = a.errors && a.ranges_total > 0 && a.recovered == a.ranges_total;
        if healed {
            let text =
                if a.muted > 0 { "🩹 recovered (muted)".to_string() } else { "🩹 recovered".to_string() };
            let resp = clickable(ui, egui::Color32::from_rgb(110, 200, 130), text)
                .on_hover_text(format!(
                    "This capture lost {} segments (~{}), but every lost range was re-fetched \
                     from the VOD into patch files next to the recording{}. Click for the \
                     🚨 Warnings details.",
                    crate::models::group_thousands(a.lost_segments),
                    fmt_duration(a.lost_segments * 2),
                    if a.muted > 0 {
                        format!(" ({} segments only survived as DMCA-muted copies)", a.muted)
                    } else {
                        String::new()
                    }
                ));
            open_warnings |= resp.clicked();
        } else if a.errors && !gap_recover_running {
            // "Lost data" only when segments actually went missing — error
            // alerts with no loss attached (a rejected PO token, a fatal tool
            // error that killed the take) get their own badge; "0 segments
            // (~00:00:00) missing" reads as nonsense.
            if a.lost_segments > 0 || a.ranges_total > 0 {
                let text = if a.ranges_total > 0 {
                    format!("🚨 lost data ({}/{} recovered)", a.recovered, a.ranges_total)
                } else {
                    "🚨 lost data".to_string()
                };
                let resp = clickable(ui, egui::Color32::from_rgb(230, 100, 100), text)
                    .on_hover_text(format!(
                        "The capture tool reported data loss: {} segments (~{}) missing.{} \
                         Click for the 🚨 Warnings details.",
                        crate::models::group_thousands(a.lost_segments),
                        fmt_duration(a.lost_segments * 2),
                        if a.ranges_total > 0 {
                            " VOD re-fetch of the lost ranges is queued/in progress."
                        } else {
                            ""
                        }
                    ));
                open_warnings |= resp.clicked();
            } else {
                let resp = clickable(
                    ui,
                    egui::Color32::from_rgb(230, 100, 100),
                    "⛔ capture error".to_string(),
                )
                .on_hover_text(
                    "The capture tool reported a fatal error (e.g. YouTube rejecting its \
                     PO token) and this take died — it may have ended earlier than the \
                     broadcast, but no segment loss was reported within what it did \
                     capture. Click for the 🚨 Warnings details.",
                );
                open_warnings |= resp.clicked();
            }
        } else if a.superseded {
            let resp = clickable(ui, egui::Color32::from_rgb(110, 200, 130), "🔁 superseded".into())
                .on_hover_text(
                    "This capture attempt died (see 🚨 Warnings), but a later take of the same \
                     broadcast completed — new takes re-fetch the full stream head (deep \
                     rewind / VOD backfill), so the completed take should cover this one's \
                     content. Click for the 🚨 Warnings details.",
                );
            open_warnings |= resp.clicked();
        } else if a.gated {
            // Ahead of the generic warning branch: "tool warnings" is true
            // here but useless. The take did not fail because the tool
            // misbehaved, it failed because the broadcast was not ours to
            // capture — which is a state, and should read as one.
            let resp = clickable(ui, SUB_ONLY_COLOR, "🔒 not entitled".into())
                .on_hover_text(
                    "This broadcast was subscriber-only or members-only and the \
                     credentials in use do not hold that entitlement, so this take \
                     captured nothing. That is a state of the broadcast, not a capture \
                     fault — retries are spaced out accordingly. Click for the 🚨 \
                     Warnings details.",
                );
            open_warnings |= resp.clicked();
        } else if !a.errors && a.warnings {
            let resp = clickable(ui, egui::Color32::from_rgb(220, 175, 60), "⚠ tool warnings".into())
                .on_hover_text(
                    "The capture tool logged non-fatal warnings for this take — no data loss \
                     detected. Click for the 🚨 Warnings details.",
                );
            open_warnings |= resp.clicked();
        }
    }
    open_warnings
}

/// Whether a live `HeadBackfill` background task is currently working on
/// `rec_id` (either the head-fetch or the head+live concat phase).
pub(super) fn head_backfill_running(tasks: &[crate::events::BackgroundTask], rec_id: i64) -> bool {
    tasks.iter().any(|t| {
        matches!(t.kind, crate::events::BackgroundTaskKind::HeadBackfill(rid) if rid == rec_id)
    })
}

/// Whether a chapters-embed task is currently working on `rec_id` (either
/// its own trigger or as part of a "Re-embed chapters (all)" bulk run).
pub(super) fn chapters_running(tasks: &[crate::events::BackgroundTask], rec_id: i64) -> bool {
    tasks.iter().any(|t| {
        matches!(t.kind, crate::events::BackgroundTaskKind::Chapters(rid) if rid == rec_id)
            || t.kind == crate::events::BackgroundTaskKind::ReembedChaptersAll
    })
}

/// Whether a lost-segment recovery task is currently working on `rec_id`.
pub(super) fn gap_recover_running(tasks: &[crate::events::BackgroundTask], rec_id: i64) -> bool {
    tasks.iter().any(|t| {
        matches!(t.kind, crate::events::BackgroundTaskKind::GapRecover(rid) if rid == rec_id)
    })
}

/// Render the Streams-tree Name cell: indent by `depth`, a clickable ▶/▼ when
/// `has_children`, an optional 18 px avatar, then `label`. Returns true if the
/// disclosure was clicked.
pub(super) fn tree_name(
    ui: &mut egui::Ui,
    depth: usize,
    has_children: bool,
    expanded: bool,
    avatar: Option<&egui::TextureHandle>,
    label: impl Into<egui::WidgetText>,
) -> bool {
    let mut clicked = false;
    ui.add_space(depth as f32 * 16.0);
    if has_children {
        let tri = if expanded { "▼" } else { "▶" };
        if ui
            .add(egui::Button::new(tri).small().frame(false))
            .on_hover_text("Expand / collapse")
            .clicked()
        {
            clicked = true;
        }
    } else {
        ui.add_space(16.0); // align with rows that have a triangle
    }
    if let Some(tex) = avatar {
        let resp = ui.add(
            egui::Image::from_texture(tex)
                .fit_to_exact_size(egui::vec2(18.0, 18.0))
                .corner_radius(egui::CornerRadius::same(3)),
        );
        queue_alt_image_preview(ui.ctx(), &resp, tex);
        ui.add_space(3.0);
    }
    ui.label(label);
    clicked
}

/// Compact, readable form of an instance's source URL for the Name cell (drops
/// the scheme and a leading `www.`).
pub(super) fn instance_label(url: &str) -> String {
    let s = url.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let s = s.strip_prefix("www.").unwrap_or(s);
    let s = s.trim_end_matches('/');
    if s.is_empty() { "(no URL)".to_string() } else { s.to_string() }
}

/// The countdown colour for a rolling deadline: **yellow** with the full TTL
/// still to run, ramping through orange to **red** as it runs out.
///
/// Deliberately never green — every one of these files is scheduled for
/// deletion, so the mildest state is still a warning, not an all-clear. The
/// ramp is against the take's own TTL (see [`crate::rolling::remaining_frac`])
/// rather than an absolute number of hours: "1 day left" is most of a 30 h
/// rolling window still to run, and the last scrap of a 30 d one.
pub(super) fn rolling_urgency_color(remaining: i64, ttl_secs: i64) -> egui::Color32 {
    // Ends: a readable warning-yellow and the same soft red the rest of the
    // grid uses for "this needs you now" (`HL_ERROR_TEXT`).
    const CALM: (f32, f32, f32) = (0xe8 as f32, 0xc5 as f32, 0x4a as f32);
    const URGENT: (f32, f32, f32) = (0xe0 as f32, 0x6c as f32, 0x6c as f32);
    let f = crate::rolling::remaining_frac(remaining, ttl_secs);
    let mix = |urgent: f32, calm: f32| (urgent + (calm - urgent) * f).round() as u8;
    egui::Color32::from_rgb(
        mix(URGENT.0, CALM.0),
        mix(URGENT.1, CALM.1),
        mix(URGENT.2, CALM.2),
    )
}

/// The 🕰 rolling-recording badge for one take: a countdown while its file is
/// still scheduled for auto-deletion, and a marker once it has been kept or
/// swept. Nothing at all for an ordinary take. See [`crate::rolling`].
pub(super) fn rolling_take_badge(ui: &mut egui::Ui, t: &crate::models::Recording, now: i64) {
    use crate::models::RollingState;
    match t.rolling.state(t.ended_at) {
        RollingState::None => {}
        RollingState::Rolling { deadline } => {
            let (text, hover) = match deadline {
                Some(d) => (
                    format!("🕰 {}", crate::rolling::fmt_remaining(d - now)),
                    format!(
                        "Rolling recording — this file is deleted automatically at {} ({} left \
                         of a {} window), unless you Keep it (📥 Backlog → Rolling recordings). \
                         The take's history row stays either way.",
                        fmt_datetime_short(d),
                        crate::rolling::fmt_remaining(d - now),
                        crate::rolling::fmt_remaining(t.rolling.ttl_secs),
                    ),
                ),
                None => (
                    "🕰".to_string(),
                    format!(
                        "Rolling recording — its {} countdown starts when this capture finishes.",
                        crate::rolling::fmt_remaining(t.rolling.ttl_secs),
                    ),
                ),
            };
            // Yellow with time in hand, red as the deadline closes in — the
            // colour IS the "decide now" signal, so it never renders weak.
            match deadline {
                Some(d) => ui.colored_label(rolling_urgency_color(d - now, t.rolling.ttl_secs), text),
                None => ui.weak(text),
            }
            .on_hover_text(hover);
        }
        RollingState::Kept { at } => {
            ui.weak("🕰📌").on_hover_text(format!(
                "Kept from a rolling recording on {} — it was scheduled for automatic deletion \
                 and you chose to keep it, so it's an ordinary archived stream now.",
                fmt_datetime_short(at)
            ));
        }
        RollingState::Expired { at } => {
            ui.weak("🕰🗑").on_hover_text(format!(
                "Rolling recording expired on {} — its time ran out and it wasn't kept, so \
                 the video file is gone (deleted then, or already gone by then). Everything \
                 else about the take (title, stats, chat log, chapters, notes) was preserved.",
                fmt_datetime_short(at)
            ));
        }
    }
}

/// The 🕰 rollup badge for an instance or channel row: how many takes beneath
/// it are still counting down towards auto-deletion, and how long the **first**
/// of them has left — `🕰 37 (2d 4h)`. Nothing when none are counting.
///
/// The count alone was the whole badge until it turned out to be the less
/// useful half: 37 rolling takes is fine if the nearest one goes next week and
/// urgent if it goes tonight, and a collapsed row can't tell you which. Same
/// countdown, same colour ramp as the take rows beneath it, so drilling down
/// confirms rather than surprises.
///
/// `scope` names what the rollup covers, for the hover text — `"channel"`,
/// `"instance"`, `"date range"`.
pub(super) fn rolling_rollup_badge(
    ui: &mut egui::Ui,
    r: &crate::rolling::RollingRollup,
    now: i64,
    scope: &str,
) {
    if r.count <= 0 {
        return;
    }
    match r.remaining(now) {
        Some(left) => {
            ui.colored_label(
                rolling_urgency_color(left, r.ttl_secs),
                format!("🕰{} ({})", r.count, crate::rolling::fmt_remaining(left)),
            )
            .on_hover_text(format!(
                "{} rolling recording(s) under this {scope} are counting down towards automatic \
                 deletion. The first goes at {} — {} from now, out of a {} window. Expand to \
                 find it, or open 📥 Backlog → Rolling recordings to Keep any you want to hold \
                 on to.",
                r.count,
                fmt_datetime_short(r.soonest.unwrap_or(0)),
                crate::rolling::fmt_remaining(left),
                crate::rolling::fmt_remaining(r.ttl_secs),
            ));
        }
        // Counting takes exist but none has a deadline yet — they're all still
        // recording, and the clock only starts when a capture ends.
        None => {
            ui.weak(format!("🕰{}", r.count)).on_hover_text(format!(
                "{} rolling recording(s) under this {scope}. None is counting down yet — the \
                 clock starts when each capture finishes.",
                r.count,
            ));
        }
    }
}

/// The 🕰 badge for a **broadcast** row: the same countdown its take rows show,
/// rolled up to the stream that owns them (soonest deadline wins). A broadcast
/// that reconnected has several takes, all under one TTL — without this the
/// countdown was only visible after expanding, which is exactly the row you'd
/// expand *because* you saw it.
pub(super) fn rolling_group_badge(
    ui: &mut egui::Ui,
    rolling: super::history::GroupRolling,
    now: i64,
) {
    use super::history::GroupRolling;
    match rolling {
        GroupRolling::None => {}
        GroupRolling::Rolling { deadline: Some(d), ttl_secs } => {
            ui.colored_label(
                rolling_urgency_color(d - now, ttl_secs),
                format!("🕰 {}", crate::rolling::fmt_remaining(d - now)),
            )
            .on_hover_text(format!(
                "Rolling recording — this broadcast's file(s) are deleted automatically at {} \
                 ({} left of a {} window), unless you Keep it (📥 Backlog → Rolling \
                 recordings). The history rows stay either way.",
                fmt_datetime_short(d),
                crate::rolling::fmt_remaining(d - now),
                crate::rolling::fmt_remaining(ttl_secs),
            ));
        }
        GroupRolling::Rolling { deadline: None, ttl_secs } => {
            ui.weak("🕰").on_hover_text(format!(
                "Rolling recording — its {} countdown starts when this capture finishes.",
                crate::rolling::fmt_remaining(ttl_secs),
            ));
        }
        GroupRolling::Kept => {
            ui.weak("🕰📌").on_hover_text(
                "Kept from a rolling recording — it was scheduled for automatic deletion and \
                 you chose to keep it, so it's an ordinary archived broadcast now.",
            );
        }
        GroupRolling::Expired => {
            ui.weak("🕰🗑").on_hover_text(
                "Rolling recording expired — the time ran out and it wasn't kept, so the \
                 video file(s) are gone. Everything else about the broadcast (title, stats, \
                 chat log, chapters, notes) was preserved.",
            );
        }
    }
}

/// A channel row's rolling rollup: every instance's merged into one, so the
/// badge and the 🕰 column both report the soonest deadline anywhere under the
/// channel. Shared with `channel_cells` so the sort key and the rendered cell
/// can't disagree.
pub(super) fn merged_rolling(
    monitors: &[&MonitorWithChannel],
    rollups: &HashMap<i64, crate::rolling::RollingRollup>,
) -> crate::rolling::RollingRollup {
    let mut agg = crate::rolling::RollingRollup::default();
    for m in monitors {
        if let Some(r) = rollups.get(&m.monitor.id) {
            agg.merge(r);
        }
    }
    agg
}

/// The distinct drives a set of takes is stored on, sorted — the 🖴 column for
/// the rows that already have their takes in memory (period, stream, take).
/// Channel and instance rows get the same answer straight from SQL
/// ([`crate::store::Store::drive_letters_by_monitor`]), since a collapsed row
/// has no take history loaded.
pub(super) fn drives_of_takes<'a>(
    takes: impl Iterator<Item = &'a crate::models::Recording>,
) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for t in takes {
        if t.output_path.is_empty() {
            continue;
        }
        if let Some(c) = crate::iomon::drive_letter(std::path::Path::new(&t.output_path))
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    out.sort_unstable();
    out
}

/// Merge several drive lists into one sorted, deduplicated list — a channel
/// row over its instances, or a group row over its channels.
pub(super) fn merge_drives(lists: impl Iterator<Item = impl AsRef<[char]>>) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for l in lists {
        for &c in l.as_ref() {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out.sort_unstable();
    out
}

/// `['A', 'G']` → `"A:, G:"` — the 🖴 cell's text, and the sort key behind it
/// (alphabetical, so rows sharing a drive sort together).
pub(super) fn fmt_drives(drives: &[char]) -> String {
    drives.iter().map(|c| format!("{c}:")).collect::<Vec<_>>().join(", ")
}

/// The 🖴 column cell. `scope` names what the list covers, for the hover text.
pub(super) fn drives_cell(ui: &mut egui::Ui, drives: &[char], scope: &str) {
    if drives.is_empty() {
        return;
    }
    let text = fmt_drives(drives);
    ui.label(&text).on_hover_text(format!(
        "This {scope}'s recordings are {} {text}. Read from the recorded paths, not confirmed \
         against disk — a file moved or deleted outside the app still counts here until its \
         row is disposed of.",
        if drives.len() == 1 { "all on" } else { "spread across" },
    ));
}

/// The rolling rollup over a set of takes — what a period row (week / month /
/// year) shows, summed from the broadcasts it groups. The channel and instance
/// rows get the same shape straight from SQL
/// ([`crate::store::Store::rolling_rollup_by_monitor`]); this is for the rows
/// that already have their takes in memory.
pub(super) fn rolling_of_takes<'a>(
    takes: impl Iterator<Item = &'a crate::models::Recording>,
) -> crate::rolling::RollingRollup {
    use crate::models::RollingState;
    let mut agg = crate::rolling::RollingRollup::default();
    for t in takes {
        if let RollingState::Rolling { deadline } = t.rolling.state(t.ended_at) {
            agg.merge(&crate::rolling::RollingRollup {
                count: 1,
                soonest: deadline,
                ttl_secs: if deadline.is_some() { t.rolling.ttl_secs } else { 0 },
            });
        }
    }
    agg
}

/// One take's rolling state in the broadcast-shaped form, so take rows and
/// stream rows can share [`rolling_group_cell`] instead of drifting apart.
pub(super) fn rolling_of_take(t: &crate::models::Recording) -> super::history::GroupRolling {
    use super::history::GroupRolling;
    use crate::models::RollingState;
    match t.rolling.state(t.ended_at) {
        RollingState::None => GroupRolling::None,
        RollingState::Rolling { deadline } => {
            GroupRolling::Rolling { deadline, ttl_secs: t.rolling.ttl_secs }
        }
        RollingState::Kept { .. } => GroupRolling::Kept,
        RollingState::Expired { .. } => GroupRolling::Expired,
    }
}

/// The 🕰 **column** cell for a rollup row (channel or instance): the time left
/// on the soonest rolling take beneath it, blank when nothing is counting.
/// Unlike the State-cell badge this drops the count — the column is here to be
/// sorted by, and the badge next door already carries the "how many".
pub(super) fn rolling_rollup_cell(
    ui: &mut egui::Ui,
    r: &crate::rolling::RollingRollup,
    now: i64,
    scope: &str,
) {
    if r.count <= 0 {
        return;
    }
    match r.remaining(now) {
        Some(left) => {
            ui.colored_label(
                rolling_urgency_color(left, r.ttl_secs),
                crate::rolling::fmt_remaining(left),
            )
            .on_hover_text(format!(
                "The soonest of {} rolling recording(s) under this {scope} is deleted \
                 automatically at {} — {} left of a {} window.",
                r.count,
                fmt_datetime_short(r.soonest.unwrap_or(0)),
                crate::rolling::fmt_remaining(left),
                crate::rolling::fmt_remaining(r.ttl_secs),
            ));
        }
        None => {
            ui.weak("recording").on_hover_text(format!(
                "{} rolling recording(s) under this {scope}, none counting down yet — the clock \
                 starts when each capture finishes.",
                r.count,
            ));
        }
    }
}

/// The 🕰 **column** cell for a broadcast or take row: its own countdown, or
/// the 📌 kept / 🗑 expired marker once it has stopped counting.
pub(super) fn rolling_group_cell(
    ui: &mut egui::Ui,
    rolling: super::history::GroupRolling,
    now: i64,
) {
    use super::history::GroupRolling;
    match rolling {
        GroupRolling::None => {}
        GroupRolling::Rolling { deadline: Some(d), ttl_secs } => {
            ui.colored_label(
                rolling_urgency_color(d - now, ttl_secs),
                crate::rolling::fmt_remaining(d - now),
            )
            .on_hover_text(format!(
                "Deleted automatically at {} — {} left of a {} window. 📥 Backlog → Rolling \
                 recordings to Keep it.",
                fmt_datetime_short(d),
                crate::rolling::fmt_remaining(d - now),
                crate::rolling::fmt_remaining(ttl_secs),
            ));
        }
        GroupRolling::Rolling { deadline: None, ttl_secs } => {
            ui.weak("recording").on_hover_text(format!(
                "Rolling recording — its {} countdown starts when this capture finishes.",
                crate::rolling::fmt_remaining(ttl_secs),
            ));
        }
        GroupRolling::Kept => {
            ui.weak("📌").on_hover_text("Kept — no longer counting down towards auto-deletion.");
        }
        GroupRolling::Expired => {
            ui.weak("🗑").on_hover_text(
                "Rolling recording expired — the file is gone; everything else about it was \
                 preserved.",
            );
        }
    }
}

/// Just the channel/user name, for places with no room for a URL — currently
/// the Custom layout editor's chips, where the other angles are collab
/// partners shown by their platform display name and this has to match.
///
/// [`instance_label`] can't be used there: it is deliberately a URL path
/// (`twitch.tv/camizolecorzette`) because the Name cell has to tell several
/// instances of one channel apart, which is more text than a chip can hold.
/// Falls back to the URL's last path segment for a container with no name of
/// its own.
pub(super) fn channel_only_label(row: &MonitorWithChannel) -> String {
    let name = row.channel.name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    let url = instance_label(&row.monitor.url);
    url.rsplit('/').find(|s| !s.is_empty()).unwrap_or(url.as_str()).to_string()
}

/// The platform shared by all of a channel's instances, or `None` if they differ
/// (or there are none) — drives the container row's badge.
pub(super) fn channel_platform(monitors: &[&MonitorWithChannel]) -> Option<Platform> {
    let mut it = monitors.iter().map(|m| m.monitor.platform());
    let first = it.next()?;
    if it.all(|p| p == first) { Some(first) } else { None }
}

/// The instance that represents the channel container row: the
/// earliest-started instance that is CURRENTLY live or recording (so the row
/// reflects what's happening right now, not a stale past session — and picks
/// the earliest when several instances are live at once); or, when nothing is
/// currently live/recording, the most-recent-past-recording instance (the
/// original history-browsing behavior). `None` only for an empty container.
/// Shared by the sort/filter model and the row render so they can't drift.
pub(super) fn channel_primary<'a>(
    monitors: &[&'a MonitorWithChannel],
    active: &HashSet<i64>,
    now: i64,
) -> Option<&'a MonitorWithChannel> {
    let mut best: Option<(&'a MonitorWithChannel, i64)> = None;
    for &m in monitors {
        let recording = active.contains(&m.monitor.id);
        let live = recording || m.monitor.last_state == "live";
        if !live {
            continue;
        }
        let went_live = recording_cells(m, now).went_live_secs;
        if went_live <= 0 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, b)) => went_live < b,
        };
        if better {
            best = Some((m, went_live));
        }
    }
    best.map(|(m, _)| m)
        .or_else(|| monitors.iter().copied().max_by_key(|m| m.last_recording_started.unwrap_or(0)))
}

/// Like [`channel_primary`], but layers the platform-preference feature
/// (`crate::platform_pref`) on top: prefer a currently-live PINNED instance
/// (the instance-level tier — a stronger, more specific signal than a
/// platform pick, since an instance already IS one platform) first, then a
/// currently-live instance matching the resolved preferred platform (channel
/// override, else global default — see `effective_primary_platform_from`),
/// and only then fall back to `channel_primary`'s plain earliest-live-wins.
/// Both preference tiers still resolve ties among their own qualifying
/// instances via `channel_primary`'s own earliest-live logic — a preference
/// narrows the candidate pool, it doesn't change how ties within that pool
/// are broken. Pre-filtering to `live_monitors` before applying either tier
/// matters: `channel_primary` has its own "nothing is live" fallback (most
/// recent past recording), which must never surface a stale, currently-
/// offline pinned/preferred instance while some OTHER instance is actually
/// live right now.
pub(super) fn channel_primary_preferred<'a>(
    monitors: &[&'a MonitorWithChannel],
    active: &HashSet<i64>,
    now: i64,
    pinned_ids: &HashSet<i64>,
    preferred_platform: Option<Platform>,
) -> Option<&'a MonitorWithChannel> {
    let live_monitors: Vec<&'a MonitorWithChannel> = monitors
        .iter()
        .copied()
        .filter(|m| active.contains(&m.monitor.id) || m.monitor.last_state == "live")
        .collect();
    if !pinned_ids.is_empty() {
        let pinned: Vec<&'a MonitorWithChannel> =
            live_monitors.iter().copied().filter(|m| pinned_ids.contains(&m.monitor.id)).collect();
        if let Some(m) = channel_primary(&pinned, active, now) {
            return Some(m);
        }
    }
    if let Some(p) = preferred_platform {
        let matching: Vec<&'a MonitorWithChannel> =
            live_monitors.iter().copied().filter(|m| m.monitor.platform() == p).collect();
        if let Some(m) = channel_primary(&matching, active, now) {
            return Some(m);
        }
    }
    channel_primary(monitors, active, now)
}

/// How many of the channel's instances are currently live (recording or not) —
/// drives the container row's bubbled-up live-count badge.
pub(super) fn channel_live_count(monitors: &[&MonitorWithChannel], active: &HashSet<i64>) -> usize {
    monitors
        .iter()
        .filter(|m| active.contains(&m.monitor.id) || m.monitor.last_state == "live")
        .count()
}

/// How many of the channel's instances are ad-free (manual flag or detected sub).
pub(super) fn channel_ad_free_count(monitors: &[&MonitorWithChannel]) -> usize {
    monitors
        .iter()
        .filter(|m| ad_free_status(m.monitor.ad_free, m.ad_free_sub).is_some())
        .count()
}

/// Sort/filter cells for a channel container's top-level row (matches the table
/// columns). `channel` is the container; `monitors` are its instances (possibly
/// none, for an empty container).
/// `active` is the live set of monitor ids with a capture process running —
/// the same source the row render uses for its state dot — so sorting by the
/// State column reorders the moment a recording starts/stops instead of
/// waiting for the next DB reload to land.
#[allow(clippy::too_many_arguments)]
pub(super) fn channel_cells(
    channel: &Channel,
    monitors: &[&MonitorWithChannel],
    active: &HashSet<i64>,
    now: i64,
    platform_pref: &crate::platform_pref::PlatformPrefCtx,
    // Per-monitor lowercase (titles, categories) of every value the monitor's
    // stream/take rows have ever logged (`Store::monitor_meta_filter_texts`)
    // — feeds the deep-filter haystacks below. Empty map = no deep history
    // matching (tests).
    rec_texts: &HashMap<i64, (String, String)>,
    // Per-monitor finished-take byte sum (`Store::monitor_disk_usage`) — the
    // channel row's "Disk use" cell sums every instance's entry.
    monitor_disk_usage: &HashMap<i64, i64>,
    // Per-monitor rolling rollup (`Store::rolling_rollup_by_monitor`) — the
    // channel row's 🕰 cell takes the soonest deadline across its instances.
    rolling_rollups: &HashMap<i64, crate::rolling::RollingRollup>,
    // Per-monitor stored drive letters (`Store::drive_letters_by_monitor`) —
    // the channel row's 🖴 cell merges every instance's list.
    monitor_drives: &HashMap<i64, Vec<char>>,
) -> Vec<Cell> {
    if monitors.is_empty() {
        // Empty container: just the name + "added"; everything else blank
        // (including disk use — nothing stored with no instances to store it).
        let mut cells: Vec<Cell> = (0..STREAM_COLS).map(|_| Cell::text(String::new())).collect();
        cells[0] = Cell::num(0.0, "off");
        let pos = |id: &str| STREAM_COLUMNS.iter().position(|c| c.id == id).unwrap();
        cells[pos("name")] = Cell::text(channel.name.clone());
        cells[pos("added")] = Cell::num(channel.created_at as f64, fmt_date(channel.created_at));
        return cells;
    }
    // Live process state, not the DB snapshot — matches the rendered state dot.
    let any_recording = monitors.iter().any(|m| active.contains(&m.monitor.id));
    let live_count = channel_live_count(monitors, active);
    // The earliest-live (or, if none live, most recent past recording) instance
    // drives the time columns — unless a pin/platform preference picks a
    // different currently-live instance instead (must match `channel_row`'s
    // own render exactly, or sorting and display would silently disagree).
    let primary = channel_primary_preferred(
        monitors, active, now, &platform_pref.pins, platform_pref.effective(channel.id),
    )
    .unwrap_or(monitors[0]);
    let rec = recording_cells(primary, now);
    let ninst = monitors.len();
    let tool = ninst.to_string();
    let last = monitors
        .iter()
        .filter_map(|m| m.monitor.last_checked_at)
        .max()
        .unwrap_or(0);
    // In STREAM_COLUMNS order: Enabled, Auto, Actions(empty), Plat, Name, Tool,
    // Detection, Scheduled rec, Polled, State, Next stream, Game, Title,
    // 🤝 Collab, Viewers, ✏ (Changes), 📢 (Ads), Went Live, Started On, Lost,
    // Duration, Ad-free, Added. MUST stay positionally 1:1 with STREAM_COLUMNS (every
    // column needs an entry here even if it's just a blank placeholder like
    // "actions"/"detection"/"scheduled_rec" below) — `ordered_rows` indexes
    // this vec by the column's STREAM_COLUMNS position, so a missing entry
    // silently shifts every later column's sort/filter onto the wrong data
    // instead of erroring (this exact bug: sorting by "state" was actually
    // sorting by "next_stream" because "scheduled_rec" had no cell here).
    let mut cells = vec![
        Cell::num(
            if channel.automation_enabled { 1.0 } else { 0.0 },
            if channel.automation_enabled { "on" } else { "off" },
        ),
        Cell::num(
            if channel.enabled { 1.0 } else { 0.0 },
            if channel.enabled { "on" } else { "off" },
        ),
        Cell::text(String::new()), // actions (not sortable/filterable)
        Cell::text(
            channel_platform(monitors)
                .map(|p| p.label().to_string())
                .unwrap_or_else(|| "mixed".into()),
        ),
        Cell::text(channel.name.clone()),
        Cell::text(tool),
        Cell::text(String::new()), // detection
        Cell::text(String::new()), // scheduled_rec (not aggregated at channel level)
        Cell::num(last as f64, fmt_datetime_short(last)), // polled (datetime only)
        // Mirrors the rendered state cell: recording > live > failed > blank
        // (offline/idle). A numeric priority (not `Cell::text`, whose sort key
        // is plain alphabetical — "failed" < "live" < "recording" only happens
        // to match by coincidence, and "" doesn't sort last in every locale)
        // so ascending/descending both order sensibly and stay correct if
        // another state label is ever added here.
        {
            let (priority, label) = if any_recording {
                (3.0, "recording")
            } else if live_count > 0 {
                (2.0, "live")
            } else if primary.last_recording_status.as_deref() == Some("failed")
                && !primary.last_recording_err_ack
            {
                (1.0, "failed")
            } else {
                (0.0, "")
            };
            Cell::num(priority, label)
        },
        {
            // Sort/show the channel's SOONEST upcoming stream across its instances.
            let next_at = monitors.iter().filter_map(|m| m.next_stream_at).min();
            Cell::num(
                next_at.unwrap_or(0) as f64,
                next_at.map(fmt_datetime_short).unwrap_or_default(),
            )
        },
        Cell::text(if rec.active { primary.last_recording_category.clone() } else { primary.last_game.clone() }),
        Cell::text(if rec.active { primary.last_recording_title.clone() } else { primary.last_title.clone() }),
        // 🤝 Collab — the primary live instance's current partners.
        Cell::text(
            primary.live_collab.as_ref().map(|c| c.names()).unwrap_or_default(),
        ),
        // Viewers — live count (blank when offline/unknown).
        Cell::num(
            primary.last_viewers.max(0) as f64,
            if primary.last_viewers >= 0 { fmt_viewers(primary.last_viewers) } else { String::new() },
        ),
        // ✏ Changes (index 11)
        Cell::num(
            primary.last_recording_meta_changes as f64,
            fmt_meta_count(primary.last_recording_meta_changes),
        ),
        // 📢 Ads combined (index 12) — sort by count; ad time surfaced via tooltip
        Cell::num(
            primary.last_recording_ad_count as f64,
            fmt_ad_count(primary.last_recording_ad_count),
        ),
        // Went Live (index 13)
        Cell::num(
            rec.went_live_secs as f64,
            rec.went_live.clone(),
        ),
        // Started On (index 14)
        Cell::num(
            rec.started_secs as f64,
            rec.started_on.clone(),
        ),
        Cell::num(rec.lost_secs as f64, rec.lost.clone()),
        Cell::num(rec.duration_secs as f64, rec.duration.clone()),
        {
            let (label, key) =
                ad_free_summary(channel_ad_free_count(monitors), monitors.len());
            Cell::num(key, label)
        },
        Cell::num(channel.created_at as f64, fmt_date(channel.created_at)),
        // Tags — the primary live instance's current tag list.
        Cell::text(primary.last_tags.clone()),
        // 🕰 Rolling — time left on the SOONEST rolling take anywhere under
        // this channel. Sorted ascending that puts whatever expires first at
        // the top, so "nothing counting down" has to sort last rather than as
        // "0 seconds left": hence `INFINITY` for an empty rollup.
        {
            let agg = merged_rolling(monitors, rolling_rollups);
            match agg.remaining(now) {
                Some(left) => Cell::num(left as f64, crate::rolling::fmt_remaining(left)),
                None if agg.count > 0 => {
                    // Counting, but still recording — no deadline yet, and
                    // nothing is at risk until the capture ends, so it sorts
                    // after every dated row.
                    Cell::num(f64::INFINITY, "recording")
                }
                None => Cell::num(f64::INFINITY, String::new()),
            }
        },
        // 🖴 Drives — every drive anything under this channel is stored on.
        Cell::text(fmt_drives(&merge_drives(
            monitors.iter().filter_map(|m| monitor_drives.get(&m.monitor.id)),
        ))),
        // Disk use — summed across every instance (index last).
        {
            let total: i64 =
                monitors.iter().map(|m| monitor_disk_usage.get(&m.monitor.id).copied().unwrap_or(0)).sum();
            Cell::num(total as f64, if total > 0 { fmt_bytes(total) } else { String::new() })
        },
    ];

    // ── Deep-filter haystacks ────────────────────────────────────────────
    // A column filter must match what the row's collapsed descendants show,
    // not only the channel rollup above — the rollup follows ONE primary
    // instance and (for Game/Title) only its current/live values, so e.g. a
    // finished stream's title was visible on its sub-row yet unfindable.
    // Indices below are STREAM_COLUMNS positions (same 1:1 contract as the
    // vec above). Deep values never affect sorting or display.
    let idx = |id: &str| {
        STREAM_COLUMNS
            .iter()
            .position(|c| c.id == id)
            .unwrap_or_else(|| unreachable!("unknown column id {id}"))
    };
    let (i_plat, i_name, i_tool, i_det) = (idx("platform"), idx("name"), idx("tool"), idx("detection"));
    let (i_game, i_title, i_collab, i_tags) = (idx("game"), idx("title"), idx("collab"), idx("tags"));
    for m in monitors {
        // Instance rows: URL under Name; tool/detection/platform as labels
        // (the cells render icons/abbreviations, hover shows these names).
        cells[i_plat].push_deep(m.monitor.platform().label());
        cells[i_name].push_deep(&m.monitor.url);
        cells[i_tool].push_deep(m.monitor.tool.label());
        cells[i_det].push_deep(m.monitor.detection_method.label());
        // Live + last-recording metadata of EVERY instance, not just the
        // rollup's primary — plus the full logged title/category history of
        // the instance's stream/take rows, expanded or not.
        cells[i_game].push_deep(&m.last_game);
        cells[i_game].push_deep(&m.last_recording_category);
        cells[i_title].push_deep(&m.last_title);
        cells[i_title].push_deep(&m.last_recording_title);
        if let Some((titles, categories)) = rec_texts.get(&m.monitor.id) {
            cells[i_title].push_deep(titles);
            cells[i_game].push_deep(categories);
        }
        if let Some(c) = &m.live_collab {
            cells[i_collab].push_deep(&c.names());
        }
        cells[i_tags].push_deep(&m.last_tags);
    }
    cells
}

/// Renders the "Stop recording" and "Stop (allow triggers)" submenus — each
/// offering the same three hold durations (until a new broadcast / 6 hours /
/// 12 hours) — as a self-contained block of context-menu items. Shared by
/// the instance, stream, and take row context menus so all three offer
/// identical Stop actions without repeating the six buttons inline (which,
/// added to everything else already in these menus, made them uncomfortably
/// tall).
pub(super) fn stop_recording_submenus(ui: &mut egui::Ui, mid: i64, a: &mut RowActions) {
    ui.menu_button("⏹  Stop recording", |ui| {
        if ui.button("Stop recording").clicked() {
            a.stop = Some((mid, None));
            ui.close();
        }
        if ui.button("Stop for 6 hours").clicked() {
            a.stop = Some((mid, Some(6)));
            ui.close();
        }
        if ui.button("Stop for 12 hours").clicked() {
            a.stop = Some((mid, Some(12)));
            ui.close();
        }
    })
    .response
    .on_hover_text(
        "Stops the take and holds EVERY automatic restart — polls, pushes, and \
         trigger-word matches alike — until this channel goes offline and starts \
         a NEW broadcast (or the chosen timer expires). ▶ Start clears the hold.",
    );
    ui.menu_button("⏹  Stop (allow triggers)", |ui| {
        if ui.button("Stop recording").clicked() {
            a.stop_allow_triggers = Some((mid, None));
            ui.close();
        }
        if ui.button("Stop for 6 hours").clicked() {
            a.stop_allow_triggers = Some((mid, Some(6)));
            ui.close();
        }
        if ui.button("Stop for 12 hours").clicked() {
            a.stop_allow_triggers = Some((mid, Some(12)));
            ui.close();
        }
    })
    .response
    .on_hover_text(
        "Like Stop, but a trigger-word match can still start a fresh recording \
         during the hold — e.g. you stop the main broadcast, but an impromptu \
         karaoke segment later still gets captured. Plain Auto-record (polls/\
         pushes) stays suppressed either way. ▶ Start clears the hold.",
    );
}

/// Self-mutating actions collected while rendering a capture-instance row.
#[derive(Default)]
pub(super) struct RowActions {
    pub(super) start: Option<i64>,                 // monitor id
    /// `(monitor id, hold hours)` — `None` hours = hold until a new broadcast.
    /// Blocks every automatic restart, including trigger-word matches.
    pub(super) stop: Option<(i64, Option<i64>)>,
    /// Same as `stop`, but the hold lets a trigger-word match still start a
    /// fresh recording during it (e.g. an impromptu karaoke segment) — only
    /// plain Auto-record (polls/pushes) stays suppressed.
    pub(super) stop_allow_triggers: Option<(i64, Option<i64>)>,
    pub(super) stop_chat: Option<i64>,             // monitor id
    pub(super) view_chat: Option<i64>,             // monitor id
    pub(super) edit: Option<i64>,                  // monitor id
    pub(super) add_instance: Option<i64>,          // channel id
    /// Monitor id to open the "Move to another channel" dialog for.
    pub(super) move_instance: Option<i64>,
    pub(super) delete: Option<(i64, String)>,      // (monitor id, channel name)
    pub(super) toggle_enabled: Option<(i64, bool)>,
    pub(super) toggle_automation: Option<(i64, bool)>,
    pub(super) select: Option<i64>,                // monitor id
    pub(super) open_schedule: Option<i64>,         // monitor id (open its Next stream popup)
    pub(super) open_collab_history: Option<i64>,   // channel id (open its 🤝 collab history)
    pub(super) open_viewer_stats: Option<i64>,     // channel id (open its 📈 viewer stats)
    /// Channel id (open its ℹ Properties — set by clicking a coloured/linked
    /// tracked-channel name, e.g. in the Collab column or a " × Partner" suffix).
    pub(super) open_channel_props: Option<i64>,
    pub(super) mark_hype: Option<i64>,             // channel id (open the 🚂 mark-train dialog)
    pub(super) properties: Option<i64>,            // monitor id
    pub(super) reorganize_monitor: Option<i64>,    // monitor id
    pub(super) reorganize_channel: Option<i64>,    // channel id
    /// Target to open in the configured media player (set by "Play local recording (start)").
    pub(super) stream_in_player: Option<StreamTarget>,
    /// Monitor id to open a live stream in the player without recording (set by "Play stream (live edge)").
    pub(super) play_new_instance: Option<i64>,
    /// Recording id to manually (re)trigger head backfill for (set by "Backfill head").
    pub(super) backfill_head: Option<i64>,
    /// Targets to open in the player — this instance plus every collab
    /// partner that resolves to a locally-tracked monitor with a currently-
    /// downloading capture — plus the chosen tiling layout (set by "Play all
    /// collab instances (current downloads)" ▸ a Layout submenu entry).
    pub(super) play_collab_all_current: Option<(Vec<StreamTarget>, crate::layout::LayoutChoice)>,
    /// `(source instance mid, tracked partner mids, verified-but-untracked
    /// partners, chosen tiling layout)` to open live-edge previews for (set
    /// by "Play all collab instances (live edge)" ▸ a Layout submenu entry)
    /// — the source instance and every collab partner, whether or not it
    /// resolves to a locally-tracked monitor. An untracked partner is played
    /// via a synthetic row cloned from the source instance's own
    /// tool/quality/auth settings (see `player::spawn_play_collab_partner`),
    /// same fallback `spawn_follow_raid` already uses for an untracked raid
    /// target — a title-mention guess (unverified) is still skipped rather
    /// than auto-played.
    pub(super) play_collab_all_live_edge:
        Option<(i64, Vec<i64>, Vec<UntrackedCollabPartner>, crate::layout::LayoutChoice)>,
    /// `(source instance mid, partner)` for a single verified-but-untracked
    /// collab partner's "▷ Live edge" action in the "Play collab instance…"
    /// submenu — same fallback as `play_collab_all_live_edge`'s untracked case.
    pub(super) play_collab_partner_live_edge: Option<(i64, UntrackedCollabPartner)>,
    /// Monitor id whose most recent raid-out target should open live-edge in
    /// the player, no recording (set by "Follow raid").
    pub(super) follow_raid: Option<i64>,
    /// A confirmed-but-untracked collab partner to open the pre-filled
    /// Add-stream form for (set by right-clicking their name in the Name-cell
    /// " × Partner" suffix or 🤝 Collab column/cell — see [`collab_name_label`]).
    pub(super) add_collab_instance: Option<UntrackedCollabPartner>,
    /// Open the Custom layout editor (set by "🖌 Custom…" in either "Play all
    /// collab instances" Layout submenu) — a label per entry (display only)
    /// plus what to actually play once the editor's "Apply now"/"Save as
    /// preset…" fires.
    pub(super) open_layout_editor: Option<(Vec<LayoutAngle>, LayoutEditorTargets)>,
    /// Name of a saved layout to delete (set by the "×" next to it in either
    /// Layout submenu's saved-layouts list).
    pub(super) delete_saved_layout: Option<String>,
    /// "🔄 Rescan disk usage" on an instance row's context menu — this
    /// monitor's id. See `StreamsOut::rescan_channel_disk_usage` for the
    /// channel-row (multi-monitor) version.
    pub(super) rescan_disk_usage: Option<i64>,
}

/// What a Custom layout editor session plays once applied — the same two
/// shapes [`Actions::play_collab_all_current`]/[`Actions::play_collab_all_live_edge`]
/// carry, minus the [`crate::layout::LayoutChoice`] (the editor decides that).
#[derive(Clone)]
pub(super) enum LayoutEditorTargets {
    Current(Vec<StreamTarget>),
    LiveEdge(i64, Vec<i64>, Vec<UntrackedCollabPartner>),
}

/// Who one Custom-layout-editor chip stands for. Built alongside the target
/// list itself (index `i` here is the angle that gets slot `i`), so the canvas
/// can show the actual channel and its avatar instead of "Collab angle 2".
/// The first entry is always the clicked-on instance.
#[derive(Clone)]
pub(super) struct LayoutAngle {
    pub(super) label: String,
    /// That channel's account avatar, when one has been fetched — the same
    /// texture the Name cell draws. `None` for an untracked collab partner
    /// (no local instance, so no cached avatar) or one not fetched yet.
    pub(super) avatar: Option<egui::TextureHandle>,
}

/// A collab partner confirmed via Twitch Shared Chat (`from_title == false`)
/// that isn't a locally-tracked monitor — enough identity to play it live via
/// a synthetic row, but title-mention heuristic hits are never carried this
/// way (too unverified to auto-launch a player for).
#[derive(Clone)]
pub(super) struct UntrackedCollabPartner {
    pub(super) login: String,
    pub(super) name: String,
}

/// Render one capture-instance (monitor) row across all columns, with the Name
/// column carrying the tree disclosure. Returns true if the disclosure (the
/// row's stream history) was toggled. Self-mutating picks land in `a`.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_instance_row(
    tr: &mut egui_extras::TableRow<'_, '_>,
    row: &MonitorWithChannel,
    ptex: &PlatformTextures,
    now: i64,
    recording: bool,
    // Capture ended, finalize (remux/promote, possibly disk-gate-queued) still
    // pending — overrides the "recording" state display.
    finalizing: bool,
    // A subscriber-only CDN session is archiving this monitor's current
    // broadcast (see `crate::downloader::CdnCaptures`). Not "recording" —
    // there is no capture process — but very much not idle either.
    cdn_capture: bool,
    chat_active: bool,
    tint: Option<egui::Color32>,
    // TTL-cached `output_dir` existence (menus re-run per frame while open).
    output_dir_ok: bool,
    depth: usize,
    has_history: bool,
    expanded: bool,
    needs_remux_count: usize,
    stream_target: Option<&StreamTarget>,
    media_player: &str,
    // This instance's own account avatar for the Name cell (None until fetched).
    avatar: Option<&egui::TextureHandle>,
    // Every tracked instance's account avatar, by monitor id — only used to
    // put a face on each collab angle's chip in the Custom layout editor
    // (`LayoutAngle`), which needs the *partners'* avatars, not just this row's.
    instance_avatars: &HashMap<i64, egui::TextureHandle>,
    // This instance's rolling takes: how many are counting down towards
    // auto-deletion and when the first of them goes — the 🕰 rollup badge and
    // the 🕰 column.
    rolling: crate::rolling::RollingRollup,
    // Set when this instance is live but standing by because a sibling is
    // recording the broadcast on the named platform (simulcast dedup) — the ⇄
    // badge in the State cell.
    standby_for: Option<&str>,
    // The most recently started recording for this monitor, if any — the
    // target of the "Backfill head" manual action.
    latest_rec_id: Option<i64>,
    // Every scheduled recording (schema v51) across all monitors — filtered
    // to this row's monitor_id in the "scheduled_rec" cell. The table is
    // small, so a per-row filter is cheaper than threading a prebuilt map.
    sched_recs: &[ScheduledRecordingWithNames],
    // Pre-formatted stop-hold description when a user Stop is suppressing
    // automatic restarts for this monitor (the ✋ state badge).
    stop_hold: Option<String>,
    // This monitor's recent viewer samples (last hour) for the 👁 sparkline;
    // `None` = no samples cached (offline or history disabled).
    spark: Option<&Vec<(i64, i64)>>,
    // Per collab partner (only non-empty when `row.live_collab` is set): the
    // partner, its resolved "current download" target (if it's a locally-
    // tracked monitor with something actively downloading), and its resolved
    // monitor id (if locally tracked at all, for the live-edge action).
    collab_plays: &[(crate::models::CollabPartner, Option<StreamTarget>, Option<i64>)],
    // All rows + the Streams-grid per-channel name colour cache, needed only
    // to colour/link a collab partner's " × Partner" suffix to its own
    // tracked channel (see `tracked_name_label`).
    rows: &[MonitorWithChannel],
    channel_name_colors: &HashMap<i64, (egui::Color32, bool)>,
    // This monitor's most recent `raid_out` event, if any — "Follow raid"'s
    // enabled state and target (re-resolved at click time in dispatch, not
    // read from this snapshot, to avoid acting on a stale target).
    raid_out: Option<&crate::models::StreamEventRow>,
    order: &[usize],
    // Active header filters, for the matched-substring highlight in the
    // game/title cells.
    fhits: Option<&FilterHits>,
    // Whether title-`@mention` collab partners also get a " × @Name" Name-cell
    // suffix, same as confirmed Shared Chat/group partners (just `@`-prefixed
    // per `CollabPartner::display`'s `at_mention` styling). Persisted as
    // `collab_title_mentions_in_name`; default on.
    collab_title_in_name: bool,
    // User-saved tiling layouts (name is the identity), listed in each Layout
    // submenu alongside the 3 built-in presets — see `crate::layout`.
    saved_layouts: &[crate::layout::CustomLayout],
    // This instance's finished-take byte sum (`Store::monitor_disk_usage`) —
    // the "Disk use" cell.
    disk_use: i64,
    // The drives this instance's takes are stored on
    // (`Store::drive_letters_by_monitor`) — the 🖴 cell.
    drives: &[char],
    a: &mut RowActions,
) -> bool {
    let m = &row.monitor;
    let rec = recording_cells(row, now);

    // Right-click context menu (shared with the inline action buttons).
    let add_menu = |ui: &mut egui::Ui, a: &mut RowActions| {
        ui.set_min_width(180.0);
        if recording {
            stop_recording_submenus(ui, m.id, a);
        } else if ui.button("▶  Start recording").clicked() {
            a.start = Some(m.id);
            ui.close();
        }
        if chat_active {
            if ui.button("💬  Stop chat download").clicked() {
                a.stop_chat = Some(m.id);
                ui.close();
            }
        }
        if m.chat_log {
            if ui.button("💬  View chat").clicked() {
                a.view_chat = Some(m.id);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("🔗  Open channel URL").clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(row.monitor.url.clone()));
            ui.close();
        }
        let folder_exists = output_dir_ok;
        if ui
            .add_enabled(folder_exists, egui::Button::new("📂  Open output folder"))
            .clicked()
        {
            crate::platform::open_path(std::path::Path::new(&m.output_dir));
            ui.close();
        }
        {
            let ok = !media_player.is_empty()
                && stream_target.map(|t| playable_with(t, media_player)).unwrap_or(false);
            if ui
                .add_enabled(ok, egui::Button::new("⏵  Play local recording (start)"))
                .on_hover_text(if recording {
                    "Open live capture in the configured media player"
                } else {
                    "Open most recent recording in the configured media player"
                })
                .on_disabled_hover_text(if media_player.is_empty() {
                    "Set a media player in Settings → Defaults first"
                } else if stream_target.is_some() {
                    "In-progress SABR capture needs mpv (separate audio/video files)"
                } else {
                    "No playable capture file found"
                })
                .clicked()
            {
                a.stream_in_player = stream_target.cloned();
                ui.close();
            }
        }
        if ui
            .add_enabled(!media_player.is_empty(), egui::Button::new("▷  Play stream (live edge)"))
            .on_hover_text("Tune into the stream at the live edge in the media player (does not record)")
            .on_disabled_hover_text("Set a media player in Settings → Defaults first")
            .clicked()
        {
            a.play_new_instance = Some(m.id);
            ui.close();
        }
        if m.platform() == Platform::Twitch {
            let target_login = raid_out.map(|r| r.detail.as_str()).filter(|l| !l.is_empty());
            let target_name = raid_out.map(|r| r.target.as_str()).unwrap_or("");
            if ui
                .add_enabled(
                    !media_player.is_empty() && target_login.is_some(),
                    egui::Button::new("▷🏃  Follow raid"),
                )
                .on_hover_text(if target_login.is_some() {
                    format!("Tune into this channel's raid-out target ({target_name}) at the live \
                             edge in the media player (does not record)")
                } else {
                    "Tune into this channel's most recent raid-out target in the media player \
                     (does not record)"
                        .to_string()
                })
                .on_disabled_hover_text(if media_player.is_empty() {
                    "Set a media player in Settings → Defaults first"
                } else if raid_out.is_some() {
                    "Raid target login unknown (Twitch didn't report it) — can't build a URL for it"
                } else {
                    "No recent raid-out target known for this channel (needs conduit mode + \
                     \"Raids via EventSub\" on, in Settings → Accounts)"
                })
                .clicked()
            {
                a.follow_raid = Some(m.id);
                ui.close();
            }
        }
        if row.live_collab.is_some() {
            // Every "play ALL angles" path below skips partners the platform
            // says are offline: there is no live edge to tune into, and their
            // "current download" would be a finished take from an earlier
            // stream rather than this collab. `is_live == None` means unknown,
            // not offline (same reading as `CollabPartner::display`'s 💤), so
            // those stay in. The per-partner "Play collab instance…" submenu
            // below is deliberately NOT filtered — trying an offline partner
            // by hand from their own row stays possible.
            let live_plays: Vec<&(crate::models::CollabPartner, Option<StreamTarget>, Option<i64>)> =
                collab_plays.iter().filter(|(p, _, _)| p.is_live != Some(false)).collect();
            let this_angle = || LayoutAngle {
                label: channel_only_label(row),
                avatar: avatar.cloned(),
            };
            // "All angles, current downloads": this instance plus every collab
            // partner that resolves to a locally-tracked monitor WITH a
            // currently-downloading capture — one player window per angle.
            // Partners that aren't locally tracked have no local file to play
            // at all, so they're silently skipped here regardless of
            // verification (unlike the live-edge variant below). Angles are
            // collected with their labels attached so the Custom editor can
            // name each chip; the play path only needs the targets.
            //
            // Same playability gate as the single-target "Stream in player"
            // button — an unplayable SplitAv target (SABR mid-download under a
            // non-mpv player) would otherwise still get handed a built
            // mpv-flavored command.
            let mut current_angles: Vec<(StreamTarget, LayoutAngle)> = Vec::new();
            if let Some(t) = stream_target.cloned().filter(|t| playable_with(t, media_player)) {
                current_angles.push((t, this_angle()));
            }
            for (partner, target, pmid) in &live_plays {
                if let Some(t) = target.clone().filter(|t| playable_with(t, media_player)) {
                    current_angles.push((
                        t,
                        LayoutAngle {
                            label: partner.name.clone(),
                            avatar: pmid.and_then(|mid| instance_avatars.get(&mid)).cloned(),
                        },
                    ));
                }
            }
            let all_current: Vec<StreamTarget> =
                current_angles.iter().map(|(t, _)| t.clone()).collect();
            // "All angles, live edge": this instance, every collab partner
            // that resolves to a locally-tracked monitor, AND every verified
            // (Shared-Chat-confirmed, not a title-mention guess) partner that
            // ISN'T tracked — the latter play via a synthetic row cloned from
            // this instance's own settings (see `UntrackedCollabPartner`).
            // `m.id` (the clicked-on instance) is threaded separately from the
            // partner mids so only the partners get muted, never this one.
            let tracked_partner_mids: Vec<i64> =
                live_plays.iter().filter_map(|(_, _, mid)| *mid).collect();
            let untracked_live_edge: Vec<UntrackedCollabPartner> = live_plays
                .iter()
                .filter(|(partner, _, pmid)| pmid.is_none() && !partner.from_title)
                .map(|(partner, _, _)| UntrackedCollabPartner {
                    login: partner.login.clone(),
                    name: partner.name.clone(),
                })
                .collect();
            // Same three groups in the same order `dispatch_play_collab_live_edge`
            // spawns them (source, tracked partners, untracked) so chip N is
            // the window that lands in slot N.
            let live_edge_angles: Vec<LayoutAngle> = std::iter::once(this_angle())
                .chain(live_plays.iter().filter(|(_, _, mid)| mid.is_some()).map(
                    |(partner, _, pmid)| LayoutAngle {
                        label: partner.name.clone(),
                        avatar: pmid.and_then(|mid| instance_avatars.get(&mid)).cloned(),
                    },
                ))
                .chain(
                    live_plays
                        .iter()
                        .filter(|(partner, _, pmid)| pmid.is_none() && !partner.from_title)
                        .map(|(partner, _, _)| LayoutAngle {
                            label: partner.name.clone(),
                            avatar: None,
                        }),
                )
                .collect();
            ui.add_enabled_ui(!media_player.is_empty() && !all_current.is_empty(), |ui| {
                ui.menu_button("👥⏵  Play all collab instances (current downloads) ▸ Layout", |ui| {
                    ui.set_min_width(160.0);
                    for preset in crate::layout::BuiltinPreset::ALL {
                        if ui.button(preset.label()).clicked() {
                            a.play_collab_all_current = Some((
                                all_current.clone(),
                                crate::layout::LayoutChoice::Builtin(preset),
                            ));
                            ui.close();
                        }
                    }
                    if !saved_layouts.is_empty() {
                        ui.separator();
                        for l in saved_layouts {
                            ui.horizontal(|ui| {
                                if ui.button(&l.name).clicked() {
                                    a.play_collab_all_current = Some((
                                        all_current.clone(),
                                        crate::layout::LayoutChoice::Saved(l.name.clone()),
                                    ));
                                    ui.close();
                                }
                                if ui.small_button("×").on_hover_text("Delete this layout").clicked() {
                                    a.delete_saved_layout = Some(l.name.clone());
                                }
                            });
                        }
                    }
                    ui.separator();
                    if ui.button("🖌  Custom…").clicked() {
                        let angles: Vec<LayoutAngle> =
                            current_angles.iter().map(|(_, a)| a.clone()).collect();
                        a.open_layout_editor =
                            Some((angles, LayoutEditorTargets::Current(all_current.clone())));
                        ui.close();
                    }
                })
                .response
                .on_hover_text(
                    "Open every collab angle's currently-downloading capture in the \
                     media player at once — one window per angle that has one, \
                     including this instance — tiled per the chosen layout.",
                );
            });
            ui.add_enabled_ui(!media_player.is_empty(), |ui| {
                ui.menu_button("👥▷  Play all collab instances (live edge) ▸ Layout", |ui| {
                    ui.set_min_width(160.0);
                    for preset in crate::layout::BuiltinPreset::ALL {
                        if ui.button(preset.label()).clicked() {
                            a.play_collab_all_live_edge = Some((
                                m.id,
                                tracked_partner_mids.clone(),
                                untracked_live_edge.clone(),
                                crate::layout::LayoutChoice::Builtin(preset),
                            ));
                            ui.close();
                        }
                    }
                    if !saved_layouts.is_empty() {
                        ui.separator();
                        for l in saved_layouts {
                            ui.horizontal(|ui| {
                                if ui.button(&l.name).clicked() {
                                    a.play_collab_all_live_edge = Some((
                                        m.id,
                                        tracked_partner_mids.clone(),
                                        untracked_live_edge.clone(),
                                        crate::layout::LayoutChoice::Saved(l.name.clone()),
                                    ));
                                    ui.close();
                                }
                                if ui.small_button("×").on_hover_text("Delete this layout").clicked() {
                                    a.delete_saved_layout = Some(l.name.clone());
                                }
                            });
                        }
                    }
                    ui.separator();
                    if ui.button("🖌  Custom…").clicked() {
                        a.open_layout_editor = Some((
                            live_edge_angles.clone(),
                            LayoutEditorTargets::LiveEdge(
                                m.id,
                                tracked_partner_mids.clone(),
                                untracked_live_edge.clone(),
                            ),
                        ));
                        ui.close();
                    }
                })
                .response
                .on_hover_text(
                    "Tune into every collab angle at the live edge in the media player at \
                     once (does not record) — this instance, every locally-tracked collab \
                     partner, and every OTHER verified partner too (a synthetic instance \
                     using this one's tool/quality/auth settings, since there's no local \
                     config for a channel you don't track). A title-mention guess that isn't \
                     locally tracked is still skipped — too unverified to auto-launch. Tiled \
                     per the chosen layout.",
                );
            });
            if !collab_plays.is_empty() {
                ui.menu_button("👥  Play collab instance…", |ui| {
                    ui.set_min_width(170.0);
                    for (partner, target, pmid) in collab_plays {
                        let playable = target
                            .as_ref()
                            .map(|t| playable_with(t, media_player))
                            .unwrap_or(false);
                        ui.menu_button(partner.display(partner.from_title), |ui| {
                            if ui
                                .add_enabled(
                                    !media_player.is_empty() && playable,
                                    egui::Button::new("⏵  Current download"),
                                )
                                .on_disabled_hover_text(if target.is_some() {
                                    "In-progress SABR capture needs mpv (separate audio/video files)"
                                } else {
                                    "Not a locally-tracked channel, or nothing is \
                                     currently downloading for it"
                                })
                                .clicked()
                            {
                                a.stream_in_player = target.clone();
                                ui.close();
                            }
                            let untracked = (pmid.is_none() && !partner.from_title).then(|| {
                                UntrackedCollabPartner {
                                    login: partner.login.clone(),
                                    name: partner.name.clone(),
                                }
                            });
                            if ui
                                .add_enabled(
                                    !media_player.is_empty() && (pmid.is_some() || untracked.is_some()),
                                    egui::Button::new("▷  Live edge"),
                                )
                                .on_hover_text(format!(
                                    "{}{}",
                                    if pmid.is_some() {
                                        "Tune in via this locally-tracked instance's own settings."
                                    } else {
                                        "Not locally tracked — plays via a synthetic instance \
                                         using this row's own tool/quality/auth settings."
                                    },
                                    if partner.is_live == Some(false) {
                                        "\n\n💤 Appears offline right now — Shared Chat can stay \
                                         merged after a partner's own stream ends. Still tries \
                                         to play; may just find nothing."
                                    } else {
                                        ""
                                    }
                                ))
                                .on_disabled_hover_text(
                                    "Not a locally-tracked channel, and not a verified collab \
                                     partner (a title-mention guess is too unverified to \
                                     auto-launch)",
                                )
                                .clicked()
                            {
                                if pmid.is_some() {
                                    a.play_new_instance = *pmid;
                                } else if let Some(u) = untracked {
                                    a.play_collab_partner_live_edge = Some((m.id, u));
                                }
                                ui.close();
                            }
                        });
                    }
                });
            }
        }
        if ui.button("📋  Copy URL").clicked() {
            ui.ctx().copy_text(row.monitor.url.clone());
            ui.close();
        }
        // Manually (re)trigger a CDN head backfill for this instance's latest
        // recording — Twitch capture-from-start only, and only while live
        // (the growing CDN playlist this depends on stops being reliably
        // pre-mute-safe once the stream ends). Forced regardless of the
        // "fetch new head backfill on new take" setting (user-initiated).
        if m.platform() == Platform::Twitch {
            let is_live = matches!(m.last_state.as_str(), "live" | "recording");
            if ui
                .add_enabled(
                    is_live && latest_rec_id.is_some(),
                    egui::Button::new("🧩  Backfill head"),
                )
                .on_hover_text(
                    "Fetch the latest recording's missed intro from Twitch's still-growing live \
                     CDN playlist (pre-mute audio). Always forced — ignores the \"fetch new head \
                     backfill on new take\" setting.",
                )
                .on_disabled_hover_text(if latest_rec_id.is_none() {
                    "No recording yet for this instance."
                } else {
                    "This channel isn't currently live — head backfill needs the still-growing \
                     live CDN playlist, which stops being reliably pre-mute-safe once the stream \
                     ends. Use \"Download post-stream VOD\" on the take instead."
                })
                .clicked()
            {
                a.backfill_head = latest_rec_id;
                ui.close();
            }
        }
        ui.separator();
        if ui.button("✏  Edit instance…").clicked() {
            a.edit = Some(m.id);
            ui.close();
        }
        if ui.button("➕  Add instance to channel").clicked() {
            a.add_instance = Some(row.channel.id);
            ui.close();
        }
        if ui
            .button("⮫  Move to another channel…")
            .on_hover_text(
                "Move this instance into a different channel container — its \
                 recordings, schedule, stats, posts, and about history all move \
                 with it. The destination channel's own settings (Auto/Enabled, \
                 color, triggers) apply to it from then on.",
            )
            .clicked()
        {
            a.move_instance = Some(m.id);
            ui.close();
        }
        let master_label = if m.automation_enabled { "⏸  Disable (go dormant)" } else { "✔  Enable" };
        if ui.button(master_label)
            .on_hover_text("Master switch. Off = fully dormant: no detection/recording/fetch until acted on manually. Independent from Auto.")
            .clicked()
        {
            a.toggle_automation = Some((m.id, !m.automation_enabled));
            ui.close();
        }
        let toggle_label = if m.enabled { "⏸  Auto-record off" } else { "✔  Auto-record on" };
        if ui.button(toggle_label)
            .on_hover_text("Whether recording starts automatically on live. Detection and metadata keep running either way; ▶ Start still records manually.")
            .clicked()
        {
            a.toggle_enabled = Some((m.id, !m.enabled));
            ui.close();
        }
        ui.separator();
        if ui.button("📁  Re-organize recordings").on_hover_text("Move all recordings for this monitor into/out of subdirectories.").clicked() {
            a.reorganize_monitor = Some(m.id);
            ui.close();
        }
        if ui
            .button("🔄  Rescan disk usage")
            .on_hover_text(
                "Check every stored take of this instance against disk and clear \
                 any whose file is gone (e.g. deleted outside the app) — the \
                 💾 Disk use column otherwise keeps counting it.",
            )
            .clicked()
        {
            a.rescan_disk_usage = Some(m.id);
            ui.close();
        }
        if ui
            .button("📈  Viewer stats")
            .on_hover_text(
                "Viewer/follower history graphs and sub/bits/raid events for this \
                 channel (also in the Channel Stats tab, or double-click the 👁 cell).",
            )
            .clicked()
        {
            a.open_viewer_stats = Some(row.channel.id);
            ui.close();
        }
        if ui
            .button("🚂  Mark hype train…")
            .on_hover_text(
                "A hype train is running (or just ran) and wasn't captured? \
                 Record it manually — the start time you give also teaches the \
                 chat-side inference what it should have caught.",
            )
            .clicked()
        {
            a.mark_hype = Some(row.channel.id);
            ui.close();
        }
        ui.separator();
        if ui.button("🗑  Delete").clicked() {
            a.delete = Some((m.id, row.channel.name.clone()));
            ui.close();
        }
        ui.separator();
        if ui.button("ℹ  Properties").clicked() {
            a.properties = Some(m.id);
            ui.close();
        }
    };

    let mut disclosure_clicked = false;
    let (ad_c, ad_s) = (row.last_recording_ad_count, row.last_recording_ad_secs);
    // One cell per entry in `order` (the frame's persisted, visibility-filtered
    // column display order — see `effective_order`), dispatched by the
    // column's stable id so the cell bodies below stay verbatim regardless of
    // how the user has hidden/reordered columns.
    for &ci in order {
        tr.col(|ui| { tint_cell(ui, tint); match STREAM_COLUMNS[ci].id {
            "enabled" => {
                let mut on = m.automation_enabled;
                let cb = ui.checkbox(&mut on, "").on_hover_text(
                    "Master switch. Off = fully dormant: no detection, recording, or asset/about/posts/schedule fetch until you act manually (▶ Start, ⟳ Refetch). Independent from Auto.",
                );
                if cb.changed() {
                    a.toggle_automation = Some((m.id, on));
                }
                cb.context_menu(|ui| add_menu(ui, a));
            }
            "auto" => {
                let mut on = m.enabled;
                let cb = ui.checkbox(&mut on, "").on_hover_text(
                    "Auto-record this instance when it goes live (disk-space control). Off = still monitored (state, schedules, metadata, posts stay current) but nothing records unless you press ▶ or a trigger word matches.",
                );
                if cb.changed() {
                    a.toggle_enabled = Some((m.id, on));
                }
                cb.context_menu(|ui| add_menu(ui, a));
            }
            "actions" => {
                ui.push_id(m.id, |ui| {
                    let mut btns: Vec<egui::Response> = Vec::with_capacity(6);
                    if recording {
                        let b = ui.small_button("⏹").on_hover_text(
                            "Stop / abort recording — holds automatic restarts until this \
                             channel starts a NEW broadcast (▶ Start clears the hold). \
                             Right-click the row for timed holds (6 h / 12 h) and a \
                             trigger-exempt hold (Stop (allow triggers)).",
                        );
                        if b.clicked() {
                            a.stop = Some((m.id, None));
                        }
                        btns.push(b);
                    } else {
                        let b = ui
                            .small_button("▶")
                            .on_hover_text("Start recording now (checks if live)");
                        if b.clicked() {
                            a.start = Some(m.id);
                        }
                        btns.push(b);
                    }
                    {
                        let player_ok = !media_player.is_empty()
                            && stream_target.map(|t| playable_with(t, media_player)).unwrap_or(false);
                        let b = ui
                            .add_enabled(player_ok, egui::Button::new("⏵").small())
                            .on_hover_text(if recording {
                                "Play local recording (start)"
                            } else {
                                "Open in player"
                            })
                            .on_disabled_hover_text(if media_player.is_empty() {
                                "Set a media player in Settings → Defaults first"
                            } else if stream_target.is_some() {
                                "In-progress SABR capture needs mpv (separate audio/video files)"
                            } else {
                                "No playable capture file found"
                            });
                        if b.clicked() {
                            a.stream_in_player = stream_target.cloned();
                        }
                        btns.push(b);
                    }
                    {
                        let b = ui
                            .add_enabled(!media_player.is_empty(), egui::Button::new("▷").small())
                            .on_hover_text("Play stream (live edge) in the media player (does not record)")
                            .on_disabled_hover_text("Set a media player in Settings → Defaults first");
                        if b.clicked() {
                            a.play_new_instance = Some(m.id);
                        }
                        btns.push(b);
                    }
                    let b = ui.small_button("✏").on_hover_text("Edit");
                    if b.clicked() {
                        a.edit = Some(m.id);
                    }
                    btns.push(b);
                    let b = ui.small_button("➕").on_hover_text("Add another tool instance");
                    if b.clicked() {
                        a.add_instance = Some(row.channel.id);
                    }
                    btns.push(b);
                    let b = ui.small_button("🗑").on_hover_text("Delete this instance");
                    if b.clicked() {
                        a.delete = Some((m.id, row.channel.name.clone()));
                    }
                    btns.push(b);
                    for b in &btns {
                        b.context_menu(|ui| add_menu(ui, a));
                    }
                });
            }
            "platform" => {
                platform_icon(ui, ptex, m.platform()).on_hover_text(m.platform().label());
            }
            "name" => {
                let (_, name_color) = state_icon(&m.last_state);
                disclosure_clicked = tree_name(
                    ui,
                    depth,
                    has_history,
                    expanded,
                    avatar,
                    egui::RichText::new(instance_label(&row.monitor.url)).color(name_color),
                );
                // "Stream Together" partners as a " × Partner" suffix while
                // this instance's shared-chat session is live — each name
                // coloured/linked when it resolves to a tracked channel (see
                // `collab_plays`'s per-partner monitor-id resolution).
                // Title-`@mention` partners (lower confidence, no shared-chat
                // confirmation) join the same suffix as " × @Name" when the
                // setting is on — `display(true)` adds the `@` so they stay
                // visually distinct from confirmed partners in the run.
                if let Some(c) = &row.live_collab {
                    let shown: Vec<_> = collab_plays
                        .iter()
                        .filter(|(p, _, _)| !p.from_title || collab_title_in_name)
                        .collect();
                    if !shown.is_empty() {
                        let resp = ui
                            .horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                for (p, _, pmid) in &shown {
                                    ui.weak(" × ");
                                    let pcid = pmid.and_then(|mid| {
                                        rows.iter().find(|r| r.monitor.id == mid).map(|r| r.channel.id)
                                    });
                                    let color = pcid.map(|cid| {
                                        let (base, adjust) = channel_name_colors
                                            .get(&cid)
                                            .copied()
                                            .unwrap_or_else(|| (channel_event_color(cid, ""), false));
                                        if adjust {
                                            readable_color(base, tint.unwrap_or_else(|| ui.visuals().panel_fill))
                                        } else {
                                            base
                                        }
                                    });
                                    let (cid, add) = collab_name_label(ui, p, pcid, color);
                                    if let Some(cid) = cid {
                                        a.open_channel_props = Some(cid);
                                    }
                                    if add.is_some() {
                                        a.add_collab_instance = add;
                                    }
                                }
                            })
                            .response;
                        resp.on_hover_text(collab_hover(c));
                    }
                }
                // inspect_with: props are only built while the inspector is
                // open (this runs per row per frame). Auto-id caveat applies —
                // the cell id derives from layout order within the table.
                ui.response().on_hover_text(&row.monitor.url).inspect_with(
                    "Streams grid: instance Name cell",
                    || {
                        vec![
                            ("channel", row.channel.name.clone()),
                            ("url", row.monitor.url.clone()),
                            ("state", m.last_state.clone()),
                        ]
                    },
                );
            }
            "tool" => {
                ui.label(short_tool_label(m.tool)).on_hover_text(m.tool.tooltip());
            }
            "detection" => {
                ui.label(detection_icon(m.detection_method)).on_hover_text(format!(
                    "{}\n\n{}",
                    m.detection_method.label(),
                    m.detection_method.tooltip()
                ));
            }
            "scheduled_rec" => {
                let mine: Vec<&ScheduledRecordingWithNames> =
                    sched_recs.iter().filter(|r| r.rec.monitor_id == m.id && r.rec.enabled).collect();
                if !mine.is_empty() {
                    let lines: Vec<String> = mine.iter().map(|r| describe_recurrence(&r.rec)).collect();
                    ui.label(format!("📅 {}", mine.len()))
                        .on_hover_text(format!("Scheduled recording(s):\n{}", lines.join("\n")));
                }
            }
            "polled" => {
                ui.label(fmt_polled(m.last_checked_at, m.poll_interval_secs))
                    .on_hover_text(format!(
                        "Last checked {} · polled every {}s",
                        if m.last_checked_at.unwrap_or(0) > 0 {
                            fmt_datetime_short(m.last_checked_at.unwrap_or(0))
                        } else {
                            "never".to_string()
                        },
                        m.poll_interval_secs,
                    ));
            }
            "state" => {
                ui.horizontal(|ui| {
                    // Dormant (master switch off) → a paused glyph; its state is
                    // frozen (no detection) so the normal live/idle icon would be
                    // misleading.
                    if !row.automation_on() {
                        ui.colored_label(egui::Color32::GRAY, "⏸").on_hover_text(
                            "Dormant — automation is off (the Enabled switch). No detection, \
                             recording, or fetch until acted on manually.",
                        );
                        return;
                    }
                    let shown_state = if finalizing { "finalizing" } else { &m.last_state };
                    let (icon, color) = state_icon_ack(shown_state, row.last_recording_err_ack);
                    let resp = ui.colored_label(color, icon);
                    let is_failed = !finalizing
                        && (m.last_state == "failed"
                            || row.last_recording_status.as_deref() == Some("failed"));
                    if finalizing {
                        resp.on_hover_text(FINALIZING_HOVER);
                    } else if is_failed && row.last_recording_err_ack {
                        resp.on_hover_text(format!(
                            "Acknowledged — {}",
                            fail_hover(&row.last_recording_log)
                        ));
                    } else if is_failed {
                        resp.on_hover_text(fail_hover(&row.last_recording_log));
                    } else if recording && row.capture_offline {
                        resp.on_hover_text(CAPTURE_OFFLINE_HOVER);
                    } else {
                        resp.on_hover_text(&m.last_state);
                    }
                    rolling_rollup_badge(ui, &rolling, now, "instance");
                    // Subscriber-only: the last take was refused by Twitch, so
                    // what's being archived is the CDN head backfill, behind
                    // the live edge. Shown while the channel is still live —
                    // that's when the lag is actionable.
                    if m.last_state == "live"
                        && (crate::models::sub_only_rejected(&row.last_recording_log)
                            || crate::models::members_only_rejected(&row.last_recording_log))
                    {
                        ui.colored_label(SUB_ONLY_COLOR, SUB_ONLY_BADGE).on_hover_text(
                            sub_only_hover(m.platform(), row.last_recording_started, now),
                        );
                    }
                    // A CDN session IS a capture, just not from the live edge.
                    // Without this the row said plain "live" while gigabytes
                    // were landing on disk — the one state where "are we
                    // recording this?" has a non-obvious answer.
                    if cdn_capture {
                        ui.colored_label(
                            egui::Color32::from_rgb(0x6e, 0xc0, 0x8a),
                            "⭳ CDN",
                        )
                        .on_hover_text(CDN_CAPTURE_HOVER);
                    }
                    if let Some(platform) = standby_for {
                        ui.colored_label(
                            egui::Color32::from_rgb(0x7e, 0x9c, 0xd8),
                            egui::RichText::new("⇄").small(),
                        )
                        .on_hover_text(format!(
                            "Standing by — this channel's {platform} instance is recording this \
                             broadcast, so it isn't captured twice (Simulcast dedup). This \
                             instance stays armed: if that capture stops while the stream is \
                             still live, it takes over. Chat here is still being archived."
                        ));
                    }
                    if recording && !finalizing && row.capture_offline {
                        // The channel is NOT live anymore — the capture is
                        // draining backlog/tail or muxing. Without this the
                        // plain "recording" state reads as "live".
                        ui.colored_label(
                            egui::Color32::from_rgb(0xd0, 0xa0, 0x40),
                            egui::RichText::new("⏬").small(),
                        )
                        .on_hover_text(CAPTURE_OFFLINE_HOVER);
                    }
                    if chat_active {
                        ui.colored_label(
                            egui::Color32::from_rgb(0x4a, 0xc2, 0xff),
                            egui::RichText::new("💬").small(),
                        )
                        .on_hover_text(
                            "Live-chat download is still running.\n\
                             Right-click → Stop chat download to abort it.",
                        );
                    }
                    if let Some(desc) = &stop_hold {
                        ui.colored_label(
                            egui::Color32::from_rgb(0xd0, 0xa0, 0x40),
                            egui::RichText::new("✋").small(),
                        )
                        .on_hover_text(format!(
                            "Manually stopped — auto-record is held {desc}. ▶ Start clears the hold."
                        ));
                    }
                    if recording && !row.last_recording_trigger.is_empty() {
                        ui.colored_label(
                            egui::Color32::from_rgb(0xe8, 0xc5, 0x4a),
                            egui::RichText::new("⚡").small(),
                        )
                        .on_hover_text(format!(
                            "Recording started by a trigger word: {}",
                            row.last_recording_trigger,
                        ));
                    }
                    if needs_remux_count > 0 {
                        let lbl = if needs_remux_count == 1 {
                            "⚠ needs remux".to_string()
                        } else {
                            format!("⚠ {} need remux", needs_remux_count)
                        };
                        let tip = if needs_remux_count == 1 {
                            "1 recording is stuck as .ts — expand and right-click the take to re-remux.".to_string()
                        } else {
                            format!("{} recordings are stuck as .ts — expand and right-click each take to re-remux.", needs_remux_count)
                        };
                        ui.colored_label(egui::Color32::from_rgb(220, 140, 30), lbl)
                            .on_hover_text(tip);
                    }
                });
            }
            "next_stream" => {
                if next_stream_cell(ui, row.next_stream_at, &row.next_stream_title, true) {
                    a.open_schedule = Some(m.id);
                }
            }
            "game" => {
                // While recording, the live meta-log wins; otherwise fall back
                // to the last-detected game so a live-not-recording channel
                // still shows it.
                let v = if rec.active { &row.last_recording_category } else { &row.last_game };
                meta_value_cell(ui, v, fhits.and_then(|f| f.needle("game")));
            }
            "title" => {
                let v = if rec.active { &row.last_recording_title } else { &row.last_title };
                meta_value_cell(ui, v, fhits.and_then(|f| f.needle("title")));
            }
            "collab" => {
                if let Some(c) = &row.live_collab {
                    let hover = collab_hover(c);
                    let resolve = |login: &str| {
                        collab_plays
                            .iter()
                            .find(|(p, _, _)| p.login == login)
                            .and_then(|(_, _, pmid)| *pmid)
                            .and_then(|mid| rows.iter().find(|r| r.monitor.id == mid).map(|r| r.channel.id))
                    };
                    let (clicked, add, resp) =
                        collab_names_row(ui, &c.partners, resolve, channel_name_colors, tint);
                    resp.on_hover_text(hover);
                    if let Some(cid) = clicked {
                        a.open_channel_props = Some(cid);
                    }
                    if add.is_some() {
                        a.add_collab_instance = add;
                    }
                }
            }
            "viewers" => {
                if viewers_cell(ui, row.last_viewers, spark) {
                    a.open_viewer_stats = Some(row.channel.id);
                }
            }
            "changes" => {
                meta_cell(ui, row.last_recording_meta_changes, None, false);
            }
            "ads" => {
                combined_ads_cell(ui, ad_c, ad_s, None, None);
            }
            "went_live" => {
                ts_went_live_label(ui, rec.went_live_secs, rec.went_live_approx);
            }
            "started_on" => {
                ts_label(ui, rec.started_secs);
            }
            "lost_time" => {
                let resp = ui.label(&rec.lost);
                if m.capture_from_start {
                    resp.on_hover_text(
                        "How much of the beginning we missed. Capturing from start, so this drops \
                         to 0 once the capture catches up to the live edge; until then it's an \
                         estimate (the gap before recording began).",
                    );
                }
            }
            "duration" => {
                ui.label(&rec.duration);
            }
            "ad_free" => {
                if let Some((label, hover)) = ad_free_status(m.ad_free, row.ad_free_sub) {
                    ui.colored_label(SUCCESS_GREEN, label).on_hover_text(hover);
                }
            }
            "added" => {
                ui.label(fmt_date(row.channel.created_at));
            }
            "tags" => {
                tags_cell(ui, &row.last_tags, &row.last_language);
            }
            "rolling" => {
                rolling_rollup_cell(ui, &rolling, now, "instance");
            }
            "drives" => {
                drives_cell(ui, drives, "instance");
            }
            "disk_use" if disk_use > 0 => {
                ui.weak(fmt_bytes(disk_use)).on_hover_text(
                    "A stored total, refreshed when the grid reloads — not confirmed \
                     against disk the way an expanded stream/take's own figure is.",
                );
            }
            _ => {}
        }});
    }

    let row_resp = tr.response();
    if row_resp.clicked() || row_resp.secondary_clicked() {
        a.select = Some(m.id);
    }
    row_resp.context_menu(|ui| add_menu(ui, a));
    disclosure_clicked
}


#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use std::path::PathBuf;

    /// Minimal `MonitorWithChannel` fixture for the live-state bubbling tests
    /// below — only the fields those tests actually vary are parameters, the
    /// rest are innocuous defaults.
    fn test_row(
        monitor_id: i64,
        last_state: &str,
        last_recording_status: Option<&str>,
        last_recording_started: Option<i64>,
        last_live_since: Option<i64>,
        last_live_since_approx: bool,
    ) -> MonitorWithChannel {
        MonitorWithChannel {
            channel: Channel {
                id: 1,
                name: "Test Channel".into(),
                url: "https://twitch.tv/test".into(),
                platform: Platform::Twitch,
                created_at: 0,
                color: String::new(),
                preferred_asset: None,
                enabled: true,
                automation_enabled: true,
                primary_group_id: None,
                posts_hidden: false,
            },
            monitor: Monitor {
                id: monitor_id,
                channel_id: 1,
                url: "https://twitch.tv/test".into(),
                enabled: true,
                automation_enabled: true,
                tool: Tool::Streamlink,
                detection_method: DetectionMethod::TwitchApi,
                poll_interval_secs: 60,
                quality: "best".into(),
                output_dir: "C:/rec".into(),
                filename_template: "{name}_{date}_{time}".into(),
                container: Container::Mkv,
                capture_from_start: true,
                dual_capture: false,
                sabr_codec_pref: SabrCodecPref::Inherit,
                sabr_codec_custom: String::new(),
                ad_free: false,
                auth_kind: AuthKind::Inherit,
                auth_value: String::new(),
                audio_tracks: String::new(),
                subtitle_tracks: String::new(),
                chat_log: false,
                fetch_thumbnail: false,
                thumbnail_in_toast: false,
                fetch_chat_assets: false,
                extra_args: String::new(),
                max_concurrent: 1,
                last_checked_at: None,
                last_state: last_state.to_string(),
                last_live_since,
                last_live_since_approx,
            },
            last_recording_started,
            last_recording_ended: None,
            last_recording_status: last_recording_status.map(str::to_string),
            last_recording_err_ack: false,
            last_recording_went_live: last_recording_started,
            last_recording_went_live_approx: false,
            last_recording_lost_secs: None,
            last_recording_ad_count: 0,
            last_recording_ad_secs: 0,
            last_recording_meta_changes: 0,
            last_recording_title: String::new(),
            last_recording_category: String::new(),
            last_recording_log: String::new(),
            last_recording_trigger: String::new(),
            ad_free_sub: None,
            recording_count: 0,
            next_stream_at: None,
            next_stream_title: String::new(),
            last_title: String::new(),
            last_game: String::new(),
            last_thumbnail_url: String::new(),
            last_viewers: -1,
            live_collab: None,
            capture_offline: false,
            last_tags: String::new(),
            last_language: String::new(),
        }
    }

    // ----- deep filtering (collapsed sub-rows must stay matchable) -----

    /// The reported bug's shape: a finished stream's title was visible on a
    /// (collapsed) stream row yet the Title filter matched nothing, because
    /// the channel rollup carries only the primary instance's live title —
    /// blank while offline. The deep haystacks make every descendant value
    /// matchable regardless of expansion.
    #[test]
    fn deep_filter_matches_collapsed_sub_row_values() {
        let now = 1_000_100;
        let mut row = test_row(1, "offline", None, None, None, false);
        row.last_recording_title = "Patch Day! - Plus Mount for 3 Subs!".into();
        let channel = row.channel.clone();
        let mut rec_texts: HashMap<i64, (String, String)> = HashMap::new();
        rec_texts.insert(
            1,
            (
                "jesse and doogs play:\npatch day! - plus mount for 3 subs!".into(),
                "lost in tandem\nfinal fantasy xiv online".into(),
            ),
        );
        let no_pref = crate::platform_pref::PlatformPrefCtx::default();
        let model = vec![channel_cells(
            &channel, &[&row], &HashSet::new(), now, &no_pref, &rec_texts, &HashMap::new(),
            &HashMap::new(), &HashMap::new(),
        )];

        let idx = |id: &str| STREAM_COLUMNS.iter().position(|c| c.id == id).unwrap();
        let sort = SortState::default();
        let mut filters = vec![String::new(); STREAM_COLS];

        filters[idx("title")] = "doog".into();
        assert_eq!(ordered_rows(&model, &sort, &filters), vec![0], "history title matches");
        filters[idx("title")] = "Patch Day".into();
        assert_eq!(ordered_rows(&model, &sort, &filters), vec![0], "case-insensitive");
        filters[idx("title")] = "no such stream".into();
        assert!(ordered_rows(&model, &sort, &filters).is_empty(), "misses still filter out");
        filters[idx("title")].clear();

        filters[idx("game")] = "final fantasy".into();
        assert_eq!(ordered_rows(&model, &sort, &filters), vec![0], "history category matches");
        filters[idx("game")].clear();

        // Instance-row values count too: the URL shown under Name, the tool
        // label an instance row abbreviates.
        filters[idx("name")] = "twitch.tv/test".into();
        assert_eq!(ordered_rows(&model, &sort, &filters), vec![0], "instance URL matches");
        filters[idx("name")].clear();
        filters[idx("tool")] = "streamlink".into();
        assert_eq!(ordered_rows(&model, &sort, &filters), vec![0], "instance tool matches");
    }

    /// Hit marking answers "the channel survived — WHERE is the match?":
    /// only the instance whose data (incl. collapsed history) contains it
    /// gets marked, and channel-level-only matches mark no instance at all.
    #[test]
    fn filter_hits_mark_the_instance_that_contains_the_match() {
        let filters_for = |col: &str, needle: &str| {
            let mut f = vec![String::new(); STREAM_COLS];
            f[STREAM_COLUMNS.iter().position(|c| c.id == col).unwrap()] = needle.into();
            FilterHits::from_filters(&f).expect("one active filter")
        };
        assert!(FilterHits::from_filters(&vec![String::new(); STREAM_COLS]).is_none());

        // Two instances of one channel; the match lives in the second one's
        // (collapsed) stream history — the user's Nihmune shape.
        let twitch = test_row(1, "offline", None, None, None, false);
        let mut youtube = test_row(2, "offline", None, None, None, false);
        youtube.monitor.url = "https://youtube.com/channel/UC_x".into();
        let yt_history = ("【hololive dreams】 pwetty vtubers…".to_string(), String::new());

        let fh = filters_for("title", "hololive dreams");
        assert!(!fh.instance_hit(&twitch, None), "no title data → not marked");
        assert!(fh.instance_hit(&youtube, Some(&yt_history)), "history hit → marked");

        // An instance whose title data exists but doesn't match is ruled OUT
        // even if another column would be neutral.
        let mut twitch_titled = twitch.clone();
        twitch_titled.last_recording_title = "CYBERPUNK FINALE!!".into();
        assert!(!fh.instance_hit(&twitch_titled, None));

        // A channel-level-only filter (e.g. a date column) marks nothing:
        // all-neutral must not read as "hit".
        let fh_added = filters_for("added", "2026");
        assert!(!fh_added.instance_hit(&youtube, Some(&yt_history)));

        // Instance-level Name data is the URL.
        let fh_name = filters_for("name", "youtube.com");
        assert!(fh_name.instance_hit(&youtube, None));
        assert!(!fh_name.instance_hit(&twitch, None));

        // Stream/take hits read the takes' own logged values.
        let mut t = crate::models::Recording::test_stub();
        t.title = "【Hololive Dreams】 pwetty vtubers…".into();
        t.category = "Just Chatting".into();
        assert!(fh.take_hit(&t));
        let g = crate::models::StreamGroup {
            key: "k".into(),
            stream_id: None,
            went_live_at: None,
            went_live_approx: true,
            takes: vec![t],
        };
        assert!(fh.stream_hit(&g, None));
        assert!(!filters_for("game", "dark souls").stream_hit(&g, None));
        // Needle lookup drives the text highlight.
        assert_eq!(fh.needle("title"), Some("hololive dreams"));
        assert_eq!(fh.needle("game"), None);
    }

    /// The deep haystack is filter-only plumbing: values can't run together
    /// across the separator, and the sort key stays the cell's own text.
    #[test]
    fn deep_haystack_separates_values_and_never_affects_sort() {
        let mut a = Cell::text("Alpha");
        a.push_deep("abc");
        a.push_deep("DEF");
        a.push_deep("  "); // blank values are dropped, not stacked
        assert_eq!(a.deep, "abc\ndef", "lowercased, newline-joined");
        assert!(!a.deep.contains("cdef"), "no match across value boundaries");
        match &a.key {
            SortKey::Text(k) => assert_eq!(k, "alpha", "sort key untouched by deep pushes"),
            SortKey::Num(_) => panic!("text cell"),
        }
    }

    // ----- live-state bubbling (Went Live/Started On/Duration w/ Auto off, and
    // channel-row live indicator/count) -----

    #[test]
    fn recording_cells_falls_back_to_poll_live_meta_when_not_recording() {
        // Auto off (or otherwise not currently recording): no recording data at
        // all, but the poll-detected go-live time is known — Went Live/Started
        // On/Duration should reflect it instead of sitting blank.
        let now = 1_000_100;
        let row = test_row(1, "live", None, None, Some(1_000_000), true);
        let cells = recording_cells(&row, now);
        assert!(!cells.active);
        assert_eq!(cells.went_live_secs, 1_000_000);
        assert!(cells.went_live_approx);
        assert!(cells.went_live.ends_with('~'), "approx marker: {}", cells.went_live);
        assert_eq!(cells.started_secs, 1_000_000, "Started On mirrors Went Live");
        assert_eq!(cells.duration_secs, 100);
        assert_eq!(cells.lost_secs, 0, "nothing being captured, so nothing lost");
    }

    #[test]
    fn recording_cells_prefers_active_recording_over_poll_live_meta() {
        // Currently recording: the recording's own timing wins even though a
        // (stale/unrelated) poll-detected go-live time is also present.
        let now = 1_000_100;
        let row = test_row(1, "live", Some("recording"), Some(999_000), Some(1_000_050), false);
        let cells = recording_cells(&row, now);
        assert!(cells.active);
        assert_eq!(cells.started_secs, 999_000);
        assert_eq!(cells.went_live_secs, 999_000); // last_recording_went_live seeded to started above
    }

    #[test]
    fn recording_cells_offline_ignores_stale_live_since() {
        // Offline with no current recording: a leftover last_live_since from a
        // prior session must NOT resurface (last_state gates the fallback).
        let now = 1_000_100;
        let row = test_row(1, "offline", None, None, Some(1_000_000), true);
        let cells = recording_cells(&row, now);
        assert!(!cells.active);
        assert_eq!(cells.went_live_secs, 0);
        assert_eq!(cells.started_secs, 0);
    }

    #[test]
    fn recording_cells_offline_clears_even_a_real_finished_recording() {
        // The instance/channel row represents PRESENT state — once neither
        // recording nor live, Went Live/Started On/Duration/Lost time blank
        // out even when the instance genuinely DOES have a completed past
        // recording on file. That history belongs on the take's own row in
        // the expanded tree, not here (it used to leak through as a
        // "last_recording_*" fallback regardless of current state).
        let now = 1_000_100;
        let row = test_row(1, "offline", Some("completed"), Some(900_000), None, false);
        let cells = recording_cells(&row, now);
        assert!(!cells.active);
        assert_eq!(cells.went_live_secs, 0);
        assert_eq!(cells.started_secs, 0);
        assert_eq!(cells.duration_secs, 0);
        assert_eq!(cells.lost_secs, 0);
    }
    #[test]
    fn channel_primary_picks_earliest_live_instance_and_counts_them() {
        // Two instances live-not-recording at once: the channel row should
        // represent the EARLIER one (and its duration), not just whichever
        // happens to sort first / most-recently-checked.
        let earlier = test_row(1, "live", None, None, Some(1_000_000), false);
        let later = test_row(2, "live", None, None, Some(1_000_500), false);
        let monitors = vec![&later, &earlier]; // deliberately out of time order
        let active = HashSet::new(); // neither is actually recording
        let now = 1_000_600;

        assert_eq!(channel_live_count(&monitors, &active), 2);
        let primary = channel_primary(&monitors, &active, now).expect("one is live");
        assert_eq!(primary.monitor.id, 1, "earliest go-live wins");

        let cells = recording_cells(primary, now);
        assert_eq!(cells.went_live_secs, 1_000_000);
        assert_eq!(cells.duration_secs, 600);
    }

    #[test]
    fn channel_primary_falls_back_to_last_recording_when_none_live() {
        // Nothing currently live/recording: falls back to the most-recent-past
        // recording, matching the original (pre-bubbling) behavior.
        let old = test_row(1, "offline", None, Some(500_000), None, false);
        let newer = test_row(2, "offline", None, Some(600_000), None, false);
        let monitors = vec![&old, &newer];
        let active = HashSet::new();

        assert_eq!(channel_live_count(&monitors, &active), 0);
        let primary = channel_primary(&monitors, &active, 700_000).expect("non-empty");
        assert_eq!(primary.monitor.id, 2, "most recent past recording wins");
    }

    #[test]
    fn channel_primary_preferred_pin_beats_platform_beats_earliest_live() {
        // Three live instances of the same channel: Twitch (earliest),
        // YouTube (later), Kick (latest).
        let twitch = test_row(1, "live", None, None, Some(1_000_000), false);
        let mut youtube = test_row(2, "live", None, None, Some(1_000_100), false);
        youtube.monitor.url = "https://www.youtube.com/@test".into();
        let mut kick = test_row(3, "live", None, None, Some(1_000_200), false);
        kick.monitor.url = "https://kick.com/test".into();
        let monitors = vec![&twitch, &youtube, &kick];
        let active = HashSet::new();
        let now = 1_000_300;

        // No preference configured: identical to plain channel_primary (earliest-live).
        let none_pref = channel_primary_preferred(&monitors, &active, now, &HashSet::new(), None);
        assert_eq!(none_pref.unwrap().monitor.id, 1);

        // A platform preference overrides earliest-live.
        let plat_pref =
            channel_primary_preferred(&monitors, &active, now, &HashSet::new(), Some(Platform::YouTube));
        assert_eq!(plat_pref.unwrap().monitor.id, 2);

        // An instance pin beats both earliest-live AND the platform preference.
        let mut pins = HashSet::new();
        pins.insert(3);
        let pinned =
            channel_primary_preferred(&monitors, &active, now, &pins, Some(Platform::YouTube));
        assert_eq!(pinned.unwrap().monitor.id, 3);

        // A pin on an instance that ISN'T among the live set (offline, or not
        // this channel's) falls through to the platform preference instead of
        // resurrecting a stale/unrelated pick.
        let mut dead_pin = HashSet::new();
        dead_pin.insert(99);
        let fallthrough =
            channel_primary_preferred(&monitors, &active, now, &dead_pin, Some(Platform::Kick));
        assert_eq!(fallthrough.unwrap().monitor.id, 3);
    }

    #[test]
    fn channel_primary_preferred_falls_back_when_preferred_platform_absent() {
        // Preferred platform is Kick, but this channel has no Kick instance at
        // all — must fall back to earliest-live among what IS live, not None.
        let twitch = test_row(1, "live", None, None, Some(1_000_000), false);
        let mut youtube = test_row(2, "live", None, None, Some(1_000_100), false);
        youtube.monitor.url = "https://www.youtube.com/@test".into();
        let monitors = vec![&twitch, &youtube];
        let active = HashSet::new();
        let now = 1_000_300;
        let primary =
            channel_primary_preferred(&monitors, &active, now, &HashSet::new(), Some(Platform::Kick));
        assert_eq!(primary.unwrap().monitor.id, 1, "no Kick instance -> falls back to earliest live");
    }

    #[test]
    fn channel_cells_state_sort_key_orders_recording_live_failed_offline() {
        // The state column must sort by significance (recording > live > failed
        // > offline/idle), not by `Cell::text`'s plain alphabetical key — which
        // only coincidentally matched before this fix and would break the
        // instant a differently-spelled state (e.g. "idle") were ever added.
        let channel = Channel {
            id: 1,
            name: "Test".into(),
            url: "https://twitch.tv/test".into(),
            platform: Platform::Twitch,
            created_at: 0,
            color: String::new(),
            preferred_asset: None,
            enabled: true,
            automation_enabled: true,
            primary_group_id: None,
            posts_hidden: false,
        };
        let recording_row = test_row(1, "recording", Some("recording"), Some(1_000_000), None, false);
        let live_row = test_row(2, "live", None, None, Some(1_000_000), false);
        let failed_row = test_row(3, "failed", Some("failed"), Some(900_000), None, false);
        let offline_row = test_row(4, "offline", None, None, None, false);
        let now = 1_000_100;

        // Looked up by id, not a hardcoded index — `channel_cells` must stay
        // positionally 1:1 with `STREAM_COLUMNS` (a missing entry silently
        // shifts every later column's sort key onto the wrong data instead of
        // erroring, which is exactly what happened here before this fix: the
        // "state" click was actually sorting by "next_stream").
        let state_idx = STREAM_COLUMNS.iter().position(|c| c.id == "state").unwrap();
        let no_pref = crate::platform_pref::PlatformPrefCtx::default();
        let state_priority = |m: &MonitorWithChannel, active: &HashSet<i64>| {
            let cells =
                channel_cells(
                    &channel, &[m], active, now, &no_pref, &HashMap::new(), &HashMap::new(),
                    &HashMap::new(), &HashMap::new(),
                );
            assert_eq!(cells.len(), STREAM_COLS, "channel_cells must have one entry per STREAM_COLUMNS");
            match cells[state_idx].key {
                SortKey::Num(n) => n,
                SortKey::Text(_) => panic!("state cell must be numeric"),
            }
        };
        let mut recording_active = HashSet::new();
        recording_active.insert(1);
        let empty = HashSet::new();

        let recording_p = state_priority(&recording_row, &recording_active);
        let live_p = state_priority(&live_row, &empty);
        let failed_p = state_priority(&failed_row, &empty);
        let offline_p = state_priority(&offline_row, &empty);

        assert!(recording_p > live_p, "recording must outrank live");
        assert!(live_p > failed_p, "live must outrank failed");
        assert!(failed_p > offline_p, "failed must outrank offline");
    }

    /// The 🕰 column has to sort "expires soonest" to the top, which means the
    /// key is *time left* and rows with nothing counting down must sort LAST
    /// rather than as zero seconds — otherwise ascending order buries the
    /// urgent rows under every channel that has no rolling takes at all.
    #[test]
    fn rolling_cell_sorts_soonest_first_and_idle_rows_last() {
        let channel = Channel {
            id: 1,
            name: "Test".into(),
            url: "https://twitch.tv/test".into(),
            platform: Platform::Twitch,
            created_at: 0,
            color: String::new(),
            preferred_asset: None,
            enabled: true,
            automation_enabled: true,
            primary_group_id: None,
            posts_hidden: false,
        };
        let now = 1_000_000;
        let rolling_idx = STREAM_COLUMNS.iter().position(|c| c.id == "rolling").unwrap();
        let no_pref = crate::platform_pref::PlatformPrefCtx::default();
        let key_for = |rollups: &HashMap<i64, crate::rolling::RollingRollup>| {
            let row = test_row(1, "offline", None, None, None, false);
            let cells = channel_cells(
                &channel, &[&row], &HashSet::new(), now, &no_pref, &HashMap::new(),
                &HashMap::new(), rollups, &HashMap::new(),
            );
            assert_eq!(cells.len(), STREAM_COLS, "channel_cells must have one entry per STREAM_COLUMNS");
            match cells[rolling_idx].key {
                SortKey::Num(n) => n,
                SortKey::Text(_) => panic!("rolling cell must be numeric"),
            }
        };

        let soon = HashMap::from([(
            1,
            crate::rolling::RollingRollup { count: 1, soonest: Some(now + 3_600), ttl_secs: 86_400 },
        )]);
        let later = HashMap::from([(
            1,
            crate::rolling::RollingRollup { count: 9, soonest: Some(now + 86_400), ttl_secs: 86_400 },
        )]);
        // Counting but still recording — nothing at risk until it ends, so it
        // belongs with the idle rows at the bottom, not at the top.
        let recording = HashMap::from([(
            1,
            crate::rolling::RollingRollup { count: 1, soonest: None, ttl_secs: 86_400 },
        )]);

        assert_eq!(key_for(&soon), 3_600.0);
        assert!(key_for(&soon) < key_for(&later), "the nearer deadline sorts first");
        assert!(key_for(&later) < key_for(&recording), "a dated row outranks a still-recording one");
        assert_eq!(key_for(&HashMap::new()), f64::INFINITY, "nothing rolling sorts last");
    }

    /// The 🖴 cell's text is also its sort key, so rows sharing a drive have to
    /// group together: same set → same string, in a stable order regardless of
    /// which instance contributed what.
    #[test]
    fn drives_merge_dedup_sort_and_format_stably() {
        assert_eq!(fmt_drives(&[]), "");
        assert_eq!(fmt_drives(&['G']), "G:");
        assert_eq!(fmt_drives(&['A', 'G']), "A:, G:");

        // Two instances, overlapping drives, listed worst-order first.
        let merged = merge_drives([vec!['G', 'A'], vec!['G'], vec![], vec!['C']].into_iter());
        assert_eq!(merged, vec!['A', 'C', 'G']);
        // Order of the contributing lists can't change the answer.
        assert_eq!(merged, merge_drives([vec!['C'], vec!['G'], vec!['A', 'G']].into_iter()));
        assert_eq!(fmt_drives(&merged), "A:, C:, G:");
        assert!(merge_drives(std::iter::empty::<Vec<char>>()).is_empty());
    }

    /// Yellow with the full retention left, red as it runs out — and always a
    /// ratio of the take's OWN retention, never an absolute threshold.
    #[test]
    fn rolling_urgency_ramps_yellow_to_red_by_fraction_of_ttl() {
        let full = rolling_urgency_color(86_400, 86_400);
        let due = rolling_urgency_color(0, 86_400);
        let half = rolling_urgency_color(43_200, 86_400);
        assert_eq!(full, egui::Color32::from_rgb(0xe8, 0xc5, 0x4a), "full window = warning yellow");
        assert_eq!(due, HL_ERROR_TEXT, "out of time = the grid's error red");
        // Halfway is strictly between the two ends on every channel — the
        // yellow's green drains towards the red, its blue creeps up.
        assert!(half.g() < full.g() && half.g() > due.g());
        assert!(half.b() > full.b() && half.b() < due.b());
        // Overdue never wraps back round to calm.
        assert_eq!(rolling_urgency_color(-99_999, 86_400), due);
        // The same 24 h left is calm on a 30 h window (most of it still to
        // run) and nearly out on a 30 d one — the whole reason this divides
        // by the TTL rather than thresholding on hours.
        let short_window = rolling_urgency_color(86_400, 30 * 3_600);
        let long_window = rolling_urgency_color(86_400, 30 * 86_400);
        assert!(short_window.g() > long_window.g());
        // An unknown TTL reads calm rather than screaming red.
        assert_eq!(rolling_urgency_color(600, 0), full);
    }

    #[test]
    fn state_icon_ack_mutes_only_acknowledged_failed() {
        // Acked failed keeps the ⚠ glyph but loses the alarming red — same
        // glyph as unacked, different color, so it's still recognizable as
        // "there was a problem" without demanding attention.
        let (unacked_icon, unacked_color) = state_icon_ack("failed", false);
        let (acked_icon, acked_color) = state_icon_ack("failed", true);
        assert_eq!(unacked_icon, acked_icon);
        assert_ne!(unacked_color, acked_color);
        assert_eq!(unacked_color, state_icon("failed").1, "unacked must match state_icon exactly");
        // err_ack only means something for "failed" — every other state is
        // completely unaffected regardless of the flag.
        for state in ["recording", "live", "completed", "ended", "not_recorded", "aborted", "idle"] {
            assert_eq!(state_icon_ack(state, true), state_icon(state));
            assert_eq!(state_icon_ack(state, false), state_icon(state));
        }
    }

    // ----- multi-level table sort (ordered_rows) -----

    #[test]
    fn ordered_rows_two_level_auto_then_name() {
        // (Auto 1/0 numeric, Name text). Rows deliberately out of order; Name
        // uses mixed case to prove the secondary sort is case-insensitive.
        let mk = |auto: f64, name: &str| vec![Cell::num(auto, ""), Cell::text(name)];
        let rows = vec![
            mk(1.0, "Charlie"), // 0
            mk(0.0, "Alice"),   // 1
            mk(1.0, "alpha"),   // 2  (sorts before Charlie, case-insensitively)
            mk(0.0, "bob"),     // 3
        ];

        // Empty keys → natural (input) order.
        assert_eq!(ordered_rows(&rows, &SortState::default(), &[]), vec![0, 1, 2, 3]);

        // Primary Auto asc, secondary Name asc: auto=0 cluster (Alice, bob) then
        // auto=1 cluster (alpha, Charlie), each alphabetized within the cluster.
        let asc = SortState {
            keys: vec![
                SortLevel { col: 0, ascending: true },
                SortLevel { col: 1, ascending: true },
            ],
        };
        assert_eq!(ordered_rows(&rows, &asc, &[]), vec![1, 3, 2, 0]);

        // Flipping ONLY the secondary reverses within each primary cluster, not
        // across clusters: auto=0 (bob, Alice) then auto=1 (Charlie, alpha).
        let sec_desc = SortState {
            keys: vec![
                SortLevel { col: 0, ascending: true },
                SortLevel { col: 1, ascending: false },
            ],
        };
        assert_eq!(ordered_rows(&rows, &sec_desc, &[]), vec![3, 1, 0, 2]);
    }
    #[test]
    fn actions_col_id_is_actions() {
        // The Show-Actions gate (`effective_order`'s `extra_gate`) keys off this
        // id; guard that it actually resolves to the Actions column.
        let col = super::STREAM_COLUMNS
            .iter()
            .find(|c| c.id == "actions")
            .expect("STREAM_COLUMNS must have an \"actions\" entry");
        assert_eq!(col.title, "Actions");
    }
    #[test]
    fn stream_meta_aggregation_dedups_rebaseline() {
        let smc = |id, at, old: &str, new: &str| StreamMetaChange {
            id,
            recording_id: 0,
            at_secs: at,
            kind: "title".into(),
            old_value: old.into(),
            new_value: new.into(),
        };
        // Take 1 (started 1000): initial "A", then A -> B at +300s.
        let t1 = vec![smc(1, 0, "", "A"), smc(2, 300, "A", "B")];
        // Take 2 (started 2000): re-observes "B" (the duplicate), then B -> C at +120s.
        let t2 = vec![smc(3, 0, "", "B"), smc(4, 120, "B", "C")];

        let agg = aggregate_stream_changes(&[(1000, t1), (2000, t2)]);
        // All rows kept, offsets rebased onto the stream timeline (min start 1000)
        // and sorted: 0, 300, (2000-1000)+0=1000, (2000-1000)+120=1120.
        assert_eq!(
            agg.iter().map(|c| c.at_secs).collect::<Vec<_>>(),
            vec![0, 300, 1000, 1120]
        );
        // The displayed list drops both initial values — including take 2's
        // re-baseline of "B" (the omitted duplicate) — and keeps the real changes.
        let lines = meta_change_lines(&agg);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("A → B"), "{:?}", lines[0]);
        assert!(lines[1].contains("B → C"), "{:?}", lines[1]);
    }
    #[test]
    fn monitor_change_lines_skips_baseline_and_keeps_real_transitions() {
        let mc = |id, at, kind: &str, old: &str, new: &str| MonitorStreamChange {
            id,
            monitor_id: 7,
            at_unix: at,
            kind: kind.into(),
            old_value: old.into(),
            new_value: new.into(),
        };
        let changes = vec![
            mc(1, 1_700_000_000, "title", "", "Baseline title"),
            mc(2, 1_700_000_300, "title", "Baseline title", "New title"),
            mc(3, 1_700_000_600, "category", "", "Just Chatting"),
            mc(4, 1_700_000_900, "category", "Just Chatting", "Games"),
        ];
        let lines = monitor_change_lines(&changes);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("Title: Baseline title → New title"), "{:?}", lines[0]);
        assert!(lines[1].contains("Category: Just Chatting → Games"), "{:?}", lines[1]);
    }
    #[test]
    fn streams_col_min_width_shrinks_only_for_short_ts_datetime_cols() {
        // Regression guard: short-timestamp mode's rendered text is much
        // narrower than these columns' full-mode `min_width`, which used to
        // leave permanent trailing space (reported 2026-07-08). Full mode is
        // untouched either way — `Column::auto()`'s min_width is just a floor.
        let went_live = STREAM_COLUMNS.iter().find(|c| c.id == "went_live").unwrap();
        let polled = STREAM_COLUMNS.iter().find(|c| c.id == "polled").unwrap();
        let name = STREAM_COLUMNS.iter().find(|c| c.id == "name").unwrap();

        set_short_ts(false);
        assert_eq!(streams_col_min_width(went_live), went_live.min_width, "full mode: unchanged");
        assert_eq!(streams_col_min_width(polled), polled.min_width, "full mode: unchanged");

        set_short_ts(true);
        assert!(streams_col_min_width(went_live) < went_live.min_width, "short mode shrinks went_live's floor");
        assert!(streams_col_min_width(polled) < polled.min_width, "short mode shrinks polled's floor");
        assert_eq!(streams_col_min_width(name), name.min_width, "non-datetime column untouched");

        set_short_ts(false); // restore default for other tests
    }

    #[test]
    fn channel_only_label_is_a_name_never_a_url() {
        let mut row = test_row(1, "idle", None, None, None, false);
        row.channel.name = "Camizole".into();
        row.monitor.url = "https://twitch.tv/camizolecorzette".into();
        // The Name cell's own label stays the disambiguating URL path...
        assert_eq!(instance_label(&row.monitor.url), "twitch.tv/camizolecorzette");
        // ...but a layout chip only ever gets the channel name.
        assert_eq!(channel_only_label(&row), "Camizole");

        // Unnamed container: fall back to the URL's last path segment, not
        // the whole path (a trailing slash must not win).
        row.channel.name = "   ".into();
        row.monitor.url = "https://twitch.tv/camizolecorzette/".into();
        assert_eq!(channel_only_label(&row), "camizolecorzette");
        row.monitor.url = "https://youtube.com/@somehandle".into();
        assert_eq!(channel_only_label(&row), "@somehandle");

        // Degenerate: no URL at all still yields something printable.
        row.monitor.url = String::new();
        assert_eq!(channel_only_label(&row), "(no URL)");
    }
}
