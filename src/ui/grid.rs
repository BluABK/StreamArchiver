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
/// Readable red for inline error/validation *text* (the row tint [`HL_ERROR`] is
/// too dark to read as a foreground colour).
pub(super) const HL_ERROR_TEXT: egui::Color32 = egui::Color32::from_rgb(0xe0, 0x6c, 0x6c);

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
pub(super) fn meta_value_cell(ui: &mut egui::Ui, value: &str) {
    if value.is_empty() {
        return;
    }
    ui.add(egui::Label::new(value).truncate());
}

/// Render a 🤝 Collab cell: comma-joined partner names (shared-chat partners
/// first, title `@mentions` as `@name`), truncated to the column, with a
/// detail hover (who, host, since-when, source). Blank when not collabing.
/// Returns true on double-click (open the channel's 🤝 collab history).
pub(super) fn collab_cell(ui: &mut egui::Ui, collab: Option<&crate::models::CollabLive>) -> bool {
    let Some(c) = collab else { return false };
    let names = c.names();
    if names.is_empty() {
        return false;
    }
    ui.add(egui::Label::new(names).sense(egui::Sense::click()).truncate())
        .on_hover_text(format!(
            "{}\n\nDouble-click for the full collab history.",
            collab_hover(c)
        ))
        .double_clicked()
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
        let names: Vec<&str> = shared.iter().map(|p| p.name.as_str()).collect();
        lines.push(format!("Streaming together with {}{host}", names.join(", ")));
        if c.since_unix > 0 {
            lines.push(format!("Shared chat since {}", fmt_datetime_short(c.since_unix)));
        }
    }
    let mentions: Vec<String> = c
        .partners
        .iter()
        .filter(|p| p.from_title)
        .map(|p| format!("@{}", p.name))
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
pub(super) const STREAM_COLUMNS: [GridCol; 23] = [
    GridCol { id: "enabled",     title: "On",         tooltip: "Master switch. Off = fully dormant: no detection, recording, or asset/about/posts/schedule fetch until you act manually (▶ Start, ⟳ Refetch). Independent from Auto (which only gates automatic recording). The channel checkbox and each instance checkbox are independent.", min_width: 30.0, initial: 0.0, sortable: true, stretch: false },
    GridCol { id: "auto",        title: "Auto",       tooltip: "Auto-record: automatically record to disk when the stream goes live (a disk-space control). It does NOT gate detection, metadata, posts, schedules or assets — those always run while the channel is On. Manual Start still records, and trigger words (Settings → Downloads) can still start a recording while Auto is off. The channel checkbox and each instance checkbox are independent.", min_width: 36.0,  initial: 0.0,   sortable: true,  stretch: false },
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
    GridCol { id: "ad_free",     title: "Ad-free",    tooltip: "Marked or auto-detected ad-free (sub / Turbo / Premium) — captures have no ad-break cuts.", min_width: 54.0,  initial: 0.0,   sortable: true, stretch: false },
    GridCol { id: "added",       title: "Added",      tooltip: "When the channel was added.",                                                               min_width: 84.0,  initial: 0.0,   sortable: true, stretch: false },
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

/// Background "Active tasks" columns (no sort/filter — hide/reorder only).
pub(super) const BG_ACTIVE_COLUMNS: [GridCol; 4] = [
    GridCol { id: "channel", title: "Channel / Label", tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "task",    title: "Task",            tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "detail",  title: "Detail",          tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: true },
    GridCol { id: "elapsed", title: "Elapsed",         tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
];

/// Background "Recent" columns (no sort/filter — hide/reorder only).
pub(super) const BG_RECENT_COLUMNS: [GridCol; 4] = [
    GridCol { id: "channel", title: "Channel / Label", tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "task",    title: "Task",            tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "detail",  title: "Detail",          tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: true },
    GridCol { id: "outcome", title: "Outcome",         tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
];

/// Processes window columns (no sort/filter — hide/reorder only).
pub(super) const PROCESSES_COLUMNS: [GridCol; 7] = [
    GridCol { id: "pid",     title: "PID",     tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "type",    title: "Type",    tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "name",    title: "Name",    tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: true },
    GridCol { id: "tool",    title: "Tool",    tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "status",  title: "Status",  tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "uptime",  title: "Uptime",  tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
    GridCol { id: "actions", title: "Actions", tooltip: "", min_width: 0.0, initial: 0.0, sortable: false, stretch: false },
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
/// substring), `key` is what's sorted.
pub(super) struct Cell {
    pub(super) text: String,
    pub(super) key: SortKey,
}

impl Cell {
    /// A text cell — filter and sort both use the string.
    pub(super) fn text(s: impl Into<String>) -> Cell {
        let s = s.into();
        Cell {
            key: SortKey::Text(s.to_lowercase()),
            text: s,
        }
    }
    /// A numeric cell — sorts by `n`, filters/shows `display`.
    pub(super) fn num(n: f64, display: impl Into<String>) -> Cell {
        Cell {
            text: display.into(),
            key: SortKey::Num(n),
        }
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
    pub(super) model: Vec<Vec<Cell>>,
    /// Snapshot of the preferred-platform-when-multiple-live config, loaded
    /// once per rebuild rather than per channel row per frame.
    pub(super) platform_pref: crate::platform_pref::PlatformPrefCtx,
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
                        .map(|cell| match &cell.key {
                            // Text cells store a pre-lowercased sort key —
                            // match it instead of re-lowercasing the display
                            // text per row per frame.
                            SortKey::Text(k) => k.contains(f.as_str()),
                            SortKey::Num(_) => cell.text.to_lowercase().contains(f.as_str()),
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
        "completed" => ("✔", SUCCESS_GREEN),                         // green
        _ => ("○", Color32::from_gray(0x70)),                        // idle/unknown — dim
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
) {
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
            .on_hover_text("Fetching the missed start from the live VOD — check the Background tab for progress.");
    } else if backfill_queued {
        ui.colored_label(egui::Color32::from_gray(0xa0), "⏳ backfill queued")
            .on_hover_text(format!(
                "Will check for a missed start to backfill from the live VOD shortly \
                 (waiting ~{}s for the CDN/stream to settle).",
                crate::downloader::HEAD_BACKFILL_SETTLE_SECS
            ));
    }
}

/// Whether a live `HeadBackfill` background task is currently working on
/// `rec_id` (either the head-fetch or the head+live concat phase).
pub(super) fn head_backfill_running(tasks: &[crate::events::BackgroundTask], rec_id: i64) -> bool {
    tasks.iter().any(|t| {
        matches!(t.kind, crate::events::BackgroundTaskKind::HeadBackfill(rid) if rid == rec_id)
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
pub(super) fn channel_cells(
    channel: &Channel,
    monitors: &[&MonitorWithChannel],
    active: &HashSet<i64>,
    now: i64,
    platform_pref: &crate::platform_pref::PlatformPrefCtx,
) -> Vec<Cell> {
    if monitors.is_empty() {
        // Empty container: just the name + "added"; everything else blank. Index
        // order matches STREAM_COLUMNS: On=0, Name=3, Added=last.
        let mut cells: Vec<Cell> = (0..STREAM_COLS).map(|_| Cell::text(String::new())).collect();
        cells[0] = Cell::num(0.0, "off");
        cells[3] = Cell::text(channel.name.clone());
        cells[STREAM_COLS - 1] = Cell::num(channel.created_at as f64, fmt_date(channel.created_at));
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
    vec![
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
            } else if primary.last_recording_status.as_deref() == Some("failed") {
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
    ]
}

/// Self-mutating actions collected while rendering a capture-instance row.
#[derive(Default)]
pub(super) struct RowActions {
    pub(super) start: Option<i64>,                 // monitor id
    /// `(monitor id, hold hours)` — `None` hours = hold until a new broadcast.
    pub(super) stop: Option<(i64, Option<i64>)>,
    pub(super) stop_chat: Option<i64>,             // monitor id
    pub(super) view_chat: Option<i64>,             // monitor id
    pub(super) edit: Option<i64>,                  // monitor id
    pub(super) add_instance: Option<i64>,          // channel id
    pub(super) delete: Option<(i64, String)>,      // (monitor id, channel name)
    pub(super) toggle_enabled: Option<(i64, bool)>,
    pub(super) toggle_automation: Option<(i64, bool)>,
    pub(super) select: Option<i64>,                // monitor id
    pub(super) open_schedule: Option<i64>,         // monitor id (open its Next stream popup)
    pub(super) open_collab_history: Option<i64>,   // channel id (open its 🤝 collab history)
    pub(super) open_viewer_stats: Option<i64>,     // channel id (open its 📈 viewer stats)
    pub(super) properties: Option<i64>,            // monitor id
    pub(super) reorganize_monitor: Option<i64>,    // monitor id
    pub(super) reorganize_channel: Option<i64>,    // channel id
    /// Target to open in the configured media player (set by "Stream in player").
    pub(super) stream_in_player: Option<StreamTarget>,
    /// Monitor id to open a live stream in the player without recording (set by "Play new instance").
    pub(super) play_new_instance: Option<i64>,
    /// Recording id to manually (re)trigger head backfill for (set by "Backfill head").
    pub(super) backfill_head: Option<i64>,
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
    order: &[usize],
    a: &mut RowActions,
) -> bool {
    let m = &row.monitor;
    let rec = recording_cells(row, now);

    // Right-click context menu (shared with the inline action buttons).
    let add_menu = |ui: &mut egui::Ui, a: &mut RowActions| {
        ui.set_min_width(180.0);
        if recording {
            if ui
                .button("⏹  Stop recording")
                .on_hover_text(
                    "Stops the take and holds automatic restarts until this channel \
                     goes offline and starts a NEW broadcast. ▶ Start clears the hold.",
                )
                .clicked()
            {
                a.stop = Some((m.id, None));
                ui.close();
            }
            if ui
                .button("⏹  Stop for 6 hours")
                .on_hover_text(
                    "Stops the take and holds automatic restarts for 6 hours, \
                     regardless of offline/online cycles. ▶ Start clears the hold.",
                )
                .clicked()
            {
                a.stop = Some((m.id, Some(6)));
                ui.close();
            }
            if ui
                .button("⏹  Stop for 12 hours")
                .on_hover_text(
                    "Stops the take and holds automatic restarts for 12 hours, \
                     regardless of offline/online cycles. ▶ Start clears the hold.",
                )
                .clicked()
            {
                a.stop = Some((m.id, Some(12)));
                ui.close();
            }
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
                .add_enabled(ok, egui::Button::new("⏵  Stream in player"))
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
            .add_enabled(!media_player.is_empty(), egui::Button::new("▷  Play new instance"))
            .on_hover_text("Tune into the stream at the live edge in the media player (does not record)")
            .on_disabled_hover_text("Set a media player in Settings → Defaults first")
            .clicked()
        {
            a.play_new_instance = Some(m.id);
            ui.close();
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
                             Right-click the row for timed holds (6 h / 12 h).",
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
                                "Stream in player"
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
                            .on_hover_text("Play new instance in media player at the live edge (does not record)")
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
                // "Stream Together" partners as a weak " × Partner" suffix
                // while this instance's shared-chat session is live.
                if let Some(c) = &row.live_collab {
                    let suffix = c.name_suffix();
                    if !suffix.is_empty() {
                        ui.add(egui::Label::new(egui::RichText::new(suffix).weak()).truncate())
                            .on_hover_text(collab_hover(c));
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
                    let (icon, color) = state_icon(shown_state);
                    let resp = ui.colored_label(color, icon);
                    let is_failed = !finalizing
                        && (m.last_state == "failed"
                            || row.last_recording_status.as_deref() == Some("failed"));
                    if finalizing {
                        resp.on_hover_text(FINALIZING_HOVER);
                    } else if is_failed {
                        resp.on_hover_text(fail_hover(&row.last_recording_log));
                    } else {
                        resp.on_hover_text(&m.last_state);
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
                meta_value_cell(ui, v);
            }
            "title" => {
                let v = if rec.active { &row.last_recording_title } else { &row.last_title };
                meta_value_cell(ui, v);
            }
            "collab" => {
                if collab_cell(ui, row.live_collab.as_ref()) {
                    a.open_collab_history = Some(row.channel.id);
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
            let cells = channel_cells(&channel, &[m], active, now, &no_pref);
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
}
