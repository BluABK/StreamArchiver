//! Pure calendar math and painting for the schedule: lanes, collisions,
//! time grid, week/month arithmetic.

use super::*;

/// Unix seconds at the start (00:00 local) of today. Used to load schedule
/// entries from the beginning of today so today's full day shows in the calendar.
pub(super) fn today_start_unix() -> i64 {
    use chrono::TimeZone;
    let now = chrono::Local::now();
    let today = now.date_naive();
    let at = |h: u32| {
        today
            .and_hms_opt(h, 0, 0)
            .and_then(|t| chrono::Local.from_local_datetime(&t).earliest())
            .map(|dt| dt.timestamp())
    };
    // Local midnight, falling back to 01:00 when midnight lands in a DST
    // spring-forward gap (a handful of zones transition at 00:00), and finally to
    // `now` if even that doesn't resolve — always ≤ now so "today" stays inclusive.
    at(0).or_else(|| at(1)).unwrap_or_else(|| now.timestamp())
}

/// When a scheduled stream has no end time (YouTube upcoming streams don't carry
/// one), assume this duration when checking for time collisions.
pub(super) const COLLISION_DEFAULT_SECS: i64 = 2 * 3600;

/// The effective end time of a stream: its stated end if valid, else 2 hours past start.
/// Used by collision detection and the time-grid painter.
pub(super) fn effective_end(s: &UpcomingStream) -> i64 {
    s.end_time
        .filter(|&e| e > s.start_time)
        .unwrap_or(s.start_time + COLLISION_DEFAULT_SECS)
}

/// A stream long enough to be treated as an "all-day" event (Google-Calendar
/// style) rather than a timed block — e.g. a multi-day subathon. There's no
/// explicit all-day flag anywhere in the schedule data, so this is a duration
/// heuristic: 20h+ covers both a genuine full-day placeholder and a real
/// multi-day range, while staying well clear of a long-but-ordinary overnight
/// stream (which should still render as a normal time-grid block).
pub(super) const ALL_DAY_THRESHOLD_SECS: i64 = 20 * 3600;
pub(super) fn is_all_day(s: &UpcomingStream) -> bool {
    effective_end(s) - s.start_time >= ALL_DAY_THRESHOLD_SECS
}

/// Format a time range for display: "HH:MM – HH:MM" when end is valid, else "HH:MM".
pub(super) fn fmt_time_range(start: i64, end: Option<i64>) -> String {
    let s = fmt_time_short(start);
    match end.filter(|&e| e > start) {
        Some(e) => format!("{s} – {}", fmt_time_short(e)),
        None => s,
    }
}

/// 16-color palette for calendar event blocks. Indexed by `channel_id % 16` so each
/// channel gets a consistent, distinct color across all schedule views.
/// Parse a CSS-style hex color string (`"#rrggbb"` or `"rrggbb"`) into an egui
/// color. Returns `None` for any other input so callers can fall back gracefully.
pub(super) fn parse_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.trim().trim_start_matches('#');
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(egui::Color32::from_rgb(r, g, b))
    } else {
        None
    }
}

/// Return the display color for a channel: the custom hex `color` if set and
/// valid, otherwise a deterministic palette color keyed on `channel_id`.
pub(super) fn channel_event_color(channel_id: i64, color: &str) -> egui::Color32 {
    if !color.is_empty() {
        if let Some(c) = parse_hex_color(color) {
            return c;
        }
    }
    const PALETTE: &[egui::Color32] = &[
        egui::Color32::from_rgb(0x42, 0x88, 0xc4), // steel blue
        egui::Color32::from_rgb(0x9c, 0x27, 0xb0), // purple
        egui::Color32::from_rgb(0xe9, 0x1e, 0x63), // pink
        egui::Color32::from_rgb(0xf4, 0x43, 0x36), // red
        egui::Color32::from_rgb(0xff, 0x98, 0x00), // orange
        egui::Color32::from_rgb(0xc0, 0x90, 0x00), // amber (darkened for readability)
        egui::Color32::from_rgb(0x38, 0x8e, 0x3c), // green
        egui::Color32::from_rgb(0x00, 0x83, 0x8f), // cyan (darkened)
        egui::Color32::from_rgb(0x00, 0x79, 0x6b), // teal
        egui::Color32::from_rgb(0x30, 0x3f, 0x9f), // indigo
        egui::Color32::from_rgb(0x02, 0x77, 0xbd), // sky blue
        egui::Color32::from_rgb(0x55, 0x8b, 0x2f), // light green
        egui::Color32::from_rgb(0x82, 0x77, 0x17), // lime (darkened)
        egui::Color32::from_rgb(0x6d, 0x40, 0x2f), // brown
        egui::Color32::from_rgb(0x45, 0x5a, 0x64), // blue-grey
        egui::Color32::from_rgb(0x75, 0x75, 0x75), // grey
    ];
    PALETTE[(channel_id.unsigned_abs() as usize) % PALETTE.len()]
}

/// Darken a (possibly bright — e.g. a fetched Twitch broadcaster) channel
/// colour enough that the white text drawn on calendar event blocks stays
/// readable. Colours already dark enough pass through unchanged, so the
/// curated palette and most custom colours are unaffected.
pub(super) fn block_safe_color(c: egui::Color32) -> egui::Color32 {
    let lum = 0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32;
    const MAX_LUM: f32 = 165.0;
    if lum <= MAX_LUM {
        return c;
    }
    let k = MAX_LUM / lum;
    egui::Color32::from_rgb(
        (c.r() as f32 * k) as u8,
        (c.g() as f32 * k) as u8,
        (c.b() as f32 * k) as u8,
    )
}

/// Dim/desaturate an event tile's fill for a monitor that isn't set to
/// Auto-record — blended 40% toward gray, on top of whatever alpha the
/// caller's own hidden/hovered ladder already produced. There's no
/// hatched/striped texture-fill anywhere in this app to reuse, and building
/// one from scratch is riskier than extending the alpha-blend mechanism
/// every tile painter already has.
pub(super) fn dim_for_no_auto(c: egui::Color32) -> egui::Color32 {
    const GRAY: u8 = 128;
    const BLEND: f32 = 0.4;
    let mix = |ch: u8| (ch as f32 * (1.0 - BLEND) + GRAY as f32 * BLEND) as u8;
    egui::Color32::from_rgba_unmultiplied(mix(c.r()), mix(c.g()), mix(c.b()), c.a())
}

/// What a trigger-rule dry run against a scheduled event's own known
/// title/game would do — mirrors `downloader::supervisor::try_begin`'s
/// live-poll order (blacklist vetoes before whitelist is even considered).
#[derive(Clone)]
pub(super) enum TriggerPreview {
    /// Neither a whitelist nor blacklist rule matches.
    None,
    /// A whitelist rule would force-record even with Auto off.
    WouldFire(crate::triggers::TriggerHit),
    /// A blacklist rule vetoes — no recording even if a whitelist rule also matched.
    Blocked(crate::triggers::TriggerHit),
}

/// Blacklist-then-whitelist dry run against a scheduled event's KNOWN
/// title/game. Pure — `rules`/`block_rules` are already-resolved effective
/// rule sets (see `effective_rules`/`effective_block_rules`), so this needs
/// no Store access and is cheap to call per event once the rule sets are
/// resolved (once per channel/monitor, not per event — see `schedule_view`).
pub(super) fn preview_trigger(
    rules: &[crate::triggers::TriggerRule],
    block_rules: &[crate::triggers::TriggerRule],
    title: &str,
    game: Option<&str>,
) -> TriggerPreview {
    let title = (!title.is_empty()).then_some(title);
    if let Some(hit) = crate::triggers::first_match(block_rules, title, game) {
        return TriggerPreview::Blocked(hit);
    }
    match crate::triggers::first_match(rules, title, game) {
        Some(hit) => TriggerPreview::WouldFire(hit),
        None => TriggerPreview::None,
    }
}

/// Precomputed per-event UI signals — built once per frame in `schedule_view`
/// (not per tile-painting call site), since resolving effective trigger
/// rules hits the Store. Keyed by [`UpcomingStream::segment_id`].
pub(super) struct EventSignals {
    /// `Monitor.enabled` — the confusingly-named Auto-record flag (distinct
    /// from `Monitor.automation_enabled`, the master on/off switch).
    pub(super) auto: bool,
    /// This event's own broadcast is being recorded right now.
    pub(super) recording_now: bool,
    pub(super) trigger: TriggerPreview,
}

impl EventSignals {
    /// Icon prefix (with a trailing space when non-empty) for the tile's
    /// existing icon-composition point — shared by every tile painter so
    /// the priority order (recording-now, then trigger) stays consistent
    /// everywhere. Reuses this app's existing icon vocabulary: ⚡ already
    /// means "trigger" (notifications, the Streams grid's own trigger
    /// badge). 🔴 (not ⏺) for recording-now — the month cell already uses
    /// "⏺ rec" for a *different* signal (a Scheduled Recording rule due
    /// that day, see `schedule_month_grid`'s day-cell badge), and reusing
    /// the same glyph right next to it for "this broadcast is being
    /// recorded right now" would read as the same thing when it isn't.
    pub(super) fn icon_prefix(&self) -> String {
        let mut s = String::new();
        if self.recording_now {
            s.push_str("🔴 ");
        }
        match &self.trigger {
            TriggerPreview::WouldFire(_) => s.push_str("⚡ "),
            TriggerPreview::Blocked(_) => s.push_str("🚫 "),
            TriggerPreview::None => {}
        }
        s
    }

    /// Extra hover-text lines for this event's signals, appended after the
    /// base `schedule_detail_line`/`_merged` text. Empty when nothing's
    /// notable (Auto is on, nothing recording, no trigger rule involved).
    pub(super) fn hover_lines(&self) -> String {
        let mut lines = String::new();
        if !self.auto {
            lines.push_str("\nAuto-record: off");
        }
        if self.recording_now {
            lines.push_str("\n🔴 Recording now");
        }
        match &self.trigger {
            TriggerPreview::WouldFire(hit) => {
                lines.push_str(&format!("\n⚡ Trigger would fire: {}", hit.describe()));
            }
            TriggerPreview::Blocked(hit) => {
                lines.push_str(&format!("\n🚫 Blocked by blacklist: {}", hit.describe()));
            }
            TriggerPreview::None => {}
        }
        lines
    }
}

/// Source-priority weight for auto-merge primary selection: lower = higher priority.
/// YouTube sources beat platform (Twitch) sources because they're manually scheduled
/// and have more accurate timing than the Twitch advance schedule.
pub(super) fn merge_source_priority(source: &str) -> u8 {
    match source {
        "youtube_api" => 0,
        s if s.starts_with("youtube") => 1,
        "manual" => 2,
        "platform" => 4,
        _ => 3,
    }
}

/// Indices into `all` whose visible (channel not in `hidden`) time window overlaps
/// another visible stream's — i.e. two streams scheduled at the same time. A
/// stream with no/invalid end time is treated as [`COLLISION_DEFAULT_SECS`] long.
pub(super) fn schedule_collisions(all: &[UpcomingStream], hidden: &HashSet<i64>) -> HashSet<usize> {
    // (original index, start, effective end) for visible streams, sorted by start.
    let mut spans: Vec<(usize, i64, i64)> = all
        .iter()
        .enumerate()
        .filter(|(_, s)| !hidden.contains(&s.channel_id))
        .map(|(i, s)| (i, s.start_time, effective_end(s)))
        .collect();
    spans.sort_by_key(|&(_, start, _)| start);
    let mut out = HashSet::new();
    for a in 0..spans.len() {
        let (ia, _, ea) = spans[a];
        for &(ib, sb, _) in &spans[a + 1..] {
            // Sorted by start, so once a later stream begins at/after `a` ends, no
            // remaining stream can overlap `a`.
            if sb >= ea {
                break;
            }
            out.insert(ia);
            out.insert(ib);
        }
    }
    out
}

/// Shift a date by `delta` whole months, clamping the day to the target month's
/// length (e.g. Jan 31 + 1 month → Feb 28). Used by month-view navigation.
pub(super) fn shift_month(d: chrono::NaiveDate, delta: i32) -> chrono::NaiveDate {
    use chrono::Datelike;
    let total = d.year() * 12 + (d.month() as i32 - 1) + delta;
    let ny = total.div_euclid(12);
    let nm = total.rem_euclid(12) as u32 + 1;
    // Try the same day, then earlier, until one is valid for the target month.
    for day in (1..=d.day()).rev() {
        if let Some(nd) = chrono::NaiveDate::from_ymd_opt(ny, nm, day) {
            return nd;
        }
    }
    d
}

/// The Monday of the week containing `d` (weeks start on Monday).
pub(super) fn week_start(d: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    let lead = d.weekday().num_days_from_monday();
    d.checked_sub_days(chrono::Days::new(lead as u64)).unwrap_or(d)
}

/// Offset a date by `n` days (negative = backwards), saturating on overflow.
pub(super) fn add_days(d: chrono::NaiveDate, n: i64) -> chrono::NaiveDate {
    if n >= 0 {
        d.checked_add_days(chrono::Days::new(n as u64))
    } else {
        d.checked_sub_days(chrono::Days::new(n.unsigned_abs()))
    }
    .unwrap_or(d)
}

/// Calendar header title, e.g. `June 2026`.
pub(super) fn month_title(y: i32, m: u32) -> String {
    const NAMES: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    let name = NAMES.get((m.max(1) - 1) as usize).copied().unwrap_or("");
    format!("{name} {y}")
}

/// Multi-line detail for one upcoming stream — the calendar hover text and the
/// "Copy details" payload.
/// Like [`schedule_detail_line`] but appends a "Merged: …" line when this event
/// is the primary of a merge group.
pub(super) fn schedule_detail_line_merged(s: &UpcomingStream, merge_label: Option<&str>) -> String {
    let mut base = schedule_detail_line(s);
    if let Some(label) = merge_label {
        base.push_str(&format!("\nMerged: {label}"));
    }
    base
}

pub(super) fn schedule_detail_line(s: &UpcomingStream) -> String {
    let mut parts = vec![
        format!("{}  {}", fmt_datetime_short(s.start_time), s.channel_name),
        format!("Platform: {}", s.platform().label()),
    ];
    if let Some(end) = s.end_time.filter(|&e| e > s.start_time) {
        parts.push(format!("Until: {}", fmt_datetime_short(end)));
    }
    if !s.title.is_empty() {
        parts.push(format!("Title: {}", s.title));
    }
    if !s.category.is_empty() {
        parts.push(format!("Category: {}", s.category));
    }
    if !s.collab.is_empty() {
        parts.push(format!("With: {}", s.collab));
    }
    if !s.url.is_empty() {
        parts.push(s.url.clone());
    }
    // Always name the source (badge + label) so the hover/copy text says where the
    // schedule came from — published platform schedule, an OCR'd image, Discord, or
    // a manual correction.
    let (badge, label) = source_badge(&s.source);
    parts.push(format!("Source: {badge} {label}"));
    parts.join("\n")
}

/// Render the compact source-indicator badge for an upcoming-stream entry next to
/// the platform icon (`📡` published schedule · `📺` YouTube · `📷` OCR'd image ·
/// `💬` Discord · `✏` manually edited), with a hover naming the source. Shared by
/// every calendar render site so the origin is always visible at a glance.
pub(super) fn schedule_source_badge(ui: &mut egui::Ui, source: &str) {
    let (icon, label) = source_badge(source);
    ui.add(egui::Label::new(egui::RichText::new(icon).small().weak()))
        .on_hover_text(format!("Source: {label}"));
}

/// Right-click menu for an upcoming-stream entry (calendar chip + day popup): copy
/// its URL / platform / title / channel / full details, or open it in the browser.
/// All actions are immediate (copy/open go straight through the egui context).
/// `merge_label` is non-None when this event is the primary of a merge (auto or manual),
/// and its value is the human-readable description used in the "Un-merge" button.
pub(super) fn schedule_copy_menu(ui: &mut egui::Ui, s: &UpcomingStream, hidden: bool, merge_label: Option<&str>) {
    ui.set_min_width(180.0);
    // Un-merge actions (shown at top when this is a merged primary).
    if let Some(label) = merge_label {
        let is_auto = label.starts_with("auto");
        let is_manual = label.starts_with("manual");
        if is_auto {
            if ui
                .button("🔀  Un-merge (auto)")
                .on_hover_text(format!("Show merged events separately ({label})"))
                .clicked()
            {
                ui.ctx().data_mut(|d| {
                    d.insert_temp(egui::Id::new("sched_unmerge_auto"), s.segment_id)
                });
                ui.close();
            }
        }
        if is_manual {
            if ui
                .button("🔀  Un-merge (manual)")
                .on_hover_text(format!("Remove manual merge ({label})"))
                .clicked()
            {
                ui.ctx().data_mut(|d| {
                    d.insert_temp(egui::Id::new("sched_unmerge_manual"), s.segment_id)
                });
                ui.close();
            }
        }
        if is_auto || is_manual {
            ui.separator();
        }
    }
    // Hide / show toggle
    let hide_label = if hidden { "👁  Show" } else { "🙈  Hide" };
    if ui.button(hide_label).clicked() {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_hide"), s.segment_id));
        ui.close();
    }
    ui.separator();
    if ui
        .add_enabled(!s.url.is_empty(), egui::Button::new("📋  Copy URL"))
        .clicked()
    {
        ui.ctx().copy_text(s.url.clone());
        ui.close();
    }
    if ui.button("📋  Copy platform").clicked() {
        ui.ctx().copy_text(s.platform().label().to_string());
        ui.close();
    }
    if ui
        .add_enabled(!s.title.is_empty(), egui::Button::new("📋  Copy title"))
        .clicked()
    {
        ui.ctx().copy_text(s.title.clone());
        ui.close();
    }
    if ui.button("📋  Copy channel").clicked() {
        ui.ctx().copy_text(s.channel_name.clone());
        ui.close();
    }
    if ui.button("📋  Copy details").clicked() {
        ui.ctx().copy_text(schedule_detail_line(s));
        ui.close();
    }
    ui.separator();
    if ui
        .add_enabled(!s.url.is_empty(), egui::Button::new("🌐  Open in browser"))
        .clicked()
    {
        ui.ctx().open_url(egui::OpenUrl::new_tab(s.url.clone()));
        ui.close();
    }
    ui.separator();
    if ui.button("✏  Edit…").clicked() {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_edit"), s.segment_id));
        ui.close();
    }
    // "Open source" goes to where the item came from (platform schedule page or
    // the source image); disabled for manual edits and Discord events, which have
    // no external origin to open.
    let open_src_enabled = !s.is_manual()
        && ScheduleSourceKind::from_id(&s.source).is_some_and(|k| k.has_open_target());
    if ui
        .add_enabled(open_src_enabled, egui::Button::new("🔗  Open source"))
        .on_hover_text("Open where this schedule item came from (the platform schedule page or the source image).")
        .on_disabled_hover_text("No external source to open (manually edited or a Discord event).")
        .clicked()
    {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_open_src"), s.segment_id));
        ui.close();
    }
    ui.separator();
    if ui.button("📺  Go to channel").clicked() {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_jump"), s.monitor_id));
        ui.close();
    }
    if ui.button("▶  Start recording").clicked() {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_start"), s.monitor_id));
        ui.close();
    }
    if ui
        .button("📅  Schedule recording…")
        .on_hover_text(
            "Force-record this (or a repeat of it) at the scheduled time without turning Auto on.",
        )
        .clicked()
    {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_schedule_rec"), s.segment_id));
        ui.close();
    }
    ui.separator();
    if ui
        .button(egui::RichText::new("🗑  Delete").color(egui::Color32::from_rgb(0xe0, 0x50, 0x50)))
        .on_hover_text("Permanently suppress this event (tombstone — won't reappear on refresh).")
        .clicked()
    {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("sched_delete"), s.segment_id));
        ui.close();
    }
}

/// One detailed schedule row (⚠ if colliding · time · platform · channel — title
/// (category)) with a hover detail and the copy context menu. Shared by the day
/// popup and the Day view.
#[allow(clippy::too_many_arguments)]
pub(super) fn schedule_detail_row(
    ui: &mut egui::Ui,
    s: &UpcomingStream,
    colliding: bool,
    hidden: bool,
    ptex: &PlatformTextures,
    merge_label: Option<&str>,
    // The channel's shared Streams-list colour (`App::sched_color`).
    color: egui::Color32,
    // Auto-record tint + recording-now/trigger-preview badges — see
    // `EventSignals`. `None` when this event has no precomputed signals yet.
    sig: Option<&EventSignals>,
) {
    let stripe_color = if sig.is_some_and(|s| !s.auto) { dim_for_no_auto(color) } else { color };
    let resp = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            // 3px colored left stripe
            let (stripe_rect, _) = ui.allocate_exact_size(
                egui::vec2(3.0, ui.text_style_height(&egui::TextStyle::Body)),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(stripe_rect, egui::CornerRadius::same(2), stripe_color);
            if colliding {
                ui.colored_label(HL_COLLISION, "⚠");
            }
            if merge_label.is_some() {
                ui.label(egui::RichText::new("🔀").small());
            }
            let sig_icon = sig.map(EventSignals::icon_prefix).unwrap_or_default();
            if !sig_icon.is_empty() {
                ui.label(egui::RichText::new(sig_icon.trim_end()).small());
            }
            ui.label(egui::RichText::new(fmt_time_range(s.start_time, s.end_time)).monospace());
            platform_icon(ui, ptex, s.platform());
            schedule_source_badge(ui, &s.source);
            let mut line = s.channel_name.clone();
            if !s.title.is_empty() {
                line.push_str(" — ");
                line.push_str(&s.title);
            }
            if !s.category.is_empty() {
                line.push_str(&format!("  ({})", s.category));
            }
            ui.add(egui::Label::new(line).truncate());
        })
        .response
        .interact(egui::Sense::click());
    let ml_owned = merge_label.map(str::to_string);
    let hover_extra = sig.map(EventSignals::hover_lines).unwrap_or_default();
    resp.on_hover_text(format!("{}{hover_extra}", schedule_detail_line_merged(s, merge_label)))
        .context_menu(|ui| schedule_copy_menu(ui, s, hidden, ml_owned.as_deref()));
}

/// Subtle background tint for today's calendar cell (low-alpha accent, so it reads
/// on both light and dark themes).
pub(super) const TODAY_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x2c, 0x4a, 0x6e, 0x40);
/// Marker color for a stream that overlaps another (a scheduling collision).
pub(super) const HL_COLLISION: egui::Color32 = egui::Color32::from_rgb(0xff, 0x8a, 0x5c);

// ── Time-grid calendar constants ─────────────────────────────────────────────
/// Pixels per hour in the time-grid (Week + Day) views.
pub(super) const SCHED_HOUR_PX: f32 = 60.0;
/// Total scrollable height of the 24-hour time grid.
pub(super) const SCHED_TOTAL_H: f32 = 24.0 * SCHED_HOUR_PX;
/// Width of the left-side "HH:MM" hour-label column in the time grid.
pub(super) const SCHED_TIME_COL_W: f32 = 44.0;
/// Gap between day columns in the time grid.
pub(super) const SCHED_COL_GAP: f32 = 4.0;
/// Minimum event block height so zero/short-duration events remain clickable.
pub(super) const SCHED_MIN_BLOCK_H: f32 = 22.0;

/// How many filtered posts the Posts feed lays out per "page" (see
/// `AppState::posts_render_limit`).
pub(super) const POSTS_PAGE_SIZE: usize = 30;

/// Schedule-view zoom bounds/step (Ctrl+Plus/Minus; Ctrl+0 resets to 1.0).
/// Scales the calendar body's font + element sizes; the toolbar/sidebar chrome
/// stays normal size.
pub(super) const SCHEDULE_ZOOM_MIN: f32 = 0.6;
pub(super) const SCHEDULE_ZOOM_MAX: f32 = 2.0;
pub(super) const SCHEDULE_ZOOM_STEP: f32 = 0.1;

// ── Week-view header extras: scheduled-recording avatars + all-day bars ─────
/// Height reserved for the row of channel avatars under the week-view day
/// number (only reserved when at least one is due to show that week).
pub(super) const SCHED_AVATAR_ROW_H: f32 = 20.0;
pub(super) const SCHED_AVATAR_PX: f32 = 14.0;
/// Height/gap of each Google-Calendar-style all-day event bar in the week view.
pub(super) const SCHED_ALL_DAY_BAR_H: f32 = 18.0;
pub(super) const SCHED_ALL_DAY_BAR_GAP: f32 = 2.0;

/// Seconds past local midnight for a unix timestamp (for positioning on the time grid).
pub(super) fn secs_since_midnight(unix: i64) -> f32 {
    use chrono::Timelike;
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| {
            let local = dt.with_timezone(&chrono::Local);
            (local.hour() * 3600 + local.minute() * 60 + local.second()) as f32
        })
        .unwrap_or(0.0)
}

/// Assign events to non-overlapping lanes (columns within a day column) so that
/// concurrent events are displayed side-by-side. Returns `(stream_idx, lane, total_lanes)`.
pub(super) fn layout_event_lanes(
    indices: &[usize],
    all: &[UpcomingStream],
    // Compact mode: every event occupies exactly this many seconds of lane
    // (its chip height), so lanes only split when *start times* land within
    // the same chip — not for the whole real duration.
    max_span_secs: Option<i64>,
) -> Vec<(usize, usize, usize)> {
    if indices.is_empty() {
        return vec![];
    }
    // lane_end[i] = effective end of the last event assigned to lane i
    let mut lane_end: Vec<i64> = Vec::new();
    let mut assignments: Vec<(usize, usize)> = Vec::new(); // (stream_idx, lane)

    for &idx in indices {
        let s = &all[idx];
        let end = match max_span_secs {
            Some(span) => s.start_time + span,
            None => effective_end(s),
        };
        // Find the first lane that is free at s.start_time.
        let lane = lane_end
            .iter()
            .position(|&le| le <= s.start_time)
            .unwrap_or_else(|| {
                lane_end.push(0);
                lane_end.len() - 1
            });
        lane_end[lane] = end;
        assignments.push((idx, lane));
    }

    let total = lane_end.len().max(1);
    assignments.into_iter().map(|(i, l)| (i, l, total)).collect()
}

/// Draw a 24-hour time-grid for one or more day columns. Called by both the Week
/// and Day views. `days` lists the calendar dates; `col_w` is the per-column
/// content width (excluding the time label column and gaps).
#[allow(clippy::too_many_arguments)]
pub(super) fn schedule_time_grid(
    ui: &mut egui::Ui,
    id: &str,
    days: &[chrono::NaiveDate],
    col_w: f32,
    zoom: f32,
    all: &[UpcomingStream],
    by_day: &HashMap<chrono::NaiveDate, Vec<usize>>,
    collide: &HashSet<usize>,
    exclude: &HashSet<usize>,
    open_day: &mut Option<chrono::NaiveDate>,
    hidden_segs: &HashSet<i64>,
    selected: &HashSet<i64>,
    merge_labels: &HashMap<i64, String>,
    // Per-channel shared Streams-list colours (`App::schedule_chan_colors`);
    // missing channels fall back to the custom/palette colour.
    chan_colors: &HashMap<i64, egui::Color32>,
    // Collapse every event to a one-line chip at its start time.
    compact: bool,
    // Auto-record tint + recording-now/trigger-preview badges — precomputed
    // once per frame in `schedule_view`, keyed by `segment_id`.
    signals: &HashMap<i64, EventSignals>,
) {
    use chrono::Timelike;
    let hour_px = SCHED_HOUR_PX * zoom;
    let total_h = SCHED_TOTAL_H * zoom;
    let time_col_w = SCHED_TIME_COL_W * zoom;
    let col_gap = SCHED_COL_GAP * zoom;
    let min_block_h = SCHED_MIN_BLOCK_H * zoom;
    // Seconds of grid one compact chip covers (its pixel height in time units)
    // — the lane-collision window and the rendered height in compact mode.
    let chip_span_secs = (min_block_h / hour_px * 3600.0).ceil();

    // Scroll to show the current local hour, but only the first time the view
    // appears so subsequent frames don't fight the user's manual scroll position.
    let init_id = egui::Id::new(id).with("scroll_init");
    let already_init: bool = ui.ctx().data(|d| d.get_temp(init_id).unwrap_or(false));
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false]);
    if !already_init {
        let now_hour = chrono::Local::now().hour() as f32;
        let initial_offset = (now_hour * hour_px - 120.0 * zoom).max(0.0);
        scroll = scroll.vertical_scroll_offset(initial_offset);
        ui.ctx().data_mut(|d| d.insert_temp(init_id, true));
    }

    let grid_w = time_col_w + days.len() as f32 * (col_w + col_gap);

    let mut clicked_day: Option<chrono::NaiveDate> = None;

    scroll.show(ui, |ui| {
            let (response, painter) = ui.allocate_painter(
                egui::vec2(grid_w, total_h),
                egui::Sense::hover(),
            );
            let origin = response.rect.min;

            // ── Hour grid lines + labels ──────────────────────────────────
            let grid_line_color = egui::Color32::from_white_alpha(18);
            let half_line_color = egui::Color32::from_white_alpha(8);
            let label_color = ui.visuals().weak_text_color();
            let font = egui::FontId::proportional(10.0 * zoom);

            for hour in 0u32..24 {
                let y = origin.y + hour as f32 * hour_px;
                // Hour label
                painter.text(
                    egui::pos2(origin.x + 2.0, y + 2.0),
                    egui::Align2::LEFT_TOP,
                    format!("{hour:02}:00"),
                    font.clone(),
                    label_color,
                );
                // Full-hour line across all columns
                painter.line_segment(
                    [
                        egui::pos2(origin.x + time_col_w, y),
                        egui::pos2(origin.x + grid_w, y),
                    ],
                    egui::Stroke::new(1.0, grid_line_color),
                );
                // Half-hour line (lighter)
                let yh = y + hour_px / 2.0;
                painter.line_segment(
                    [
                        egui::pos2(origin.x + time_col_w, yh),
                        egui::pos2(origin.x + grid_w, yh),
                    ],
                    egui::Stroke::new(1.0, half_line_color),
                );
            }

            // ── Vertical column dividers ──────────────────────────────────
            let divider_color = egui::Color32::from_white_alpha(22);
            for col in 0..=days.len() {
                let x = origin.x + time_col_w + col as f32 * (col_w + col_gap);
                painter.line_segment(
                    [egui::pos2(x, origin.y), egui::pos2(x, origin.y + total_h)],
                    egui::Stroke::new(1.0, divider_color),
                );
            }

            // ── Current-time indicator (red line) ─────────────────────────
            {
                let now = chrono::Local::now();
                let today = now.date_naive();
                let now_secs = (now.hour() * 3600 + now.minute() * 60 + now.second()) as f32;
                if let Some(col_idx) = days.iter().position(|&d| d == today) {
                    let x_start = origin.x + time_col_w + col_idx as f32 * (col_w + col_gap);
                    let x_end = x_start + col_w;
                    let y = origin.y + now_secs / 3600.0 * hour_px;
                    painter.line_segment(
                        [egui::pos2(x_start, y), egui::pos2(x_end, y)],
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(0xff, 0x44, 0x44)),
                    );
                    // Small circle at left edge
                    painter.circle_filled(
                        egui::pos2(x_start, y),
                        4.0,
                        egui::Color32::from_rgb(0xff, 0x44, 0x44),
                    );
                }
            }

            // ── Event blocks ──────────────────────────────────────────────
            for (col_idx, &day) in days.iter().enumerate() {
                let col_x = origin.x + time_col_w + col_idx as f32 * (col_w + col_gap);
                let day_indices = by_day.get(&day).map(Vec::as_slice).unwrap_or(&[]);
                // All-day events (see `is_all_day`) render in the week view's
                // separate bar strip instead — skip them here to avoid a
                // duplicate (and grossly clipped) block in the time grid.
                let indices: Vec<usize> =
                    day_indices.iter().copied().filter(|i| !exclude.contains(i)).collect();
                let layout =
                    layout_event_lanes(&indices, all, compact.then_some(chip_span_secs as i64));

                for (stream_idx, lane, total_lanes) in layout {
                    let s = &all[stream_idx];
                    let start_secs = secs_since_midnight(s.start_time);
                    let end_secs = if compact {
                        start_secs + chip_span_secs
                    } else {
                        secs_since_midnight(effective_end(s))
                    };

                    // Clip to day boundaries (midnight transitions handled by bucketing).
                    let end_secs = if end_secs <= start_secs {
                        // Event ends on next day — clip at midnight.
                        SCHED_TOTAL_H / SCHED_HOUR_PX * 3600.0
                    } else {
                        end_secs
                    };
                    let duration_secs = (end_secs - start_secs).max(0.0);

                    let top = origin.y + start_secs / 3600.0 * hour_px;
                    let block_h = (duration_secs / 3600.0 * hour_px).max(min_block_h);
                    let lane_w = (col_w - 2.0 * (total_lanes as f32 - 1.0)) / total_lanes as f32;
                    let left = col_x + 1.0 + lane as f32 * (lane_w + 2.0);

                    let block_rect = egui::Rect::from_min_size(
                        egui::pos2(left, top),
                        egui::vec2(lane_w, block_h),
                    );
                    // Per-block interactive region: hover coloring, tooltip,
                    // left-click (→ day popup), right-click context menu.
                    // Must be allocated before painting so hovered() is accurate.
                    let is_hidden = hidden_segs.contains(&s.segment_id);
                    let evt_id = egui::Id::new("sched_evt").with(day).with(stream_idx);
                    let evt_resp = ui.interact(block_rect, evt_id, egui::Sense::click());
                    let hovered = evt_resp.hovered();

                    let sig = signals.get(&s.segment_id);
                    let color = chan_colors
                        .get(&s.channel_id)
                        .copied()
                        .unwrap_or_else(|| channel_event_color(s.channel_id, &s.channel_color));
                    let fill = if is_hidden {
                        // Soft-hidden: keep hue but drop alpha to ~35% so it reads as "ghost"
                        egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 85)
                    } else if hovered {
                        color
                    } else {
                        egui::Color32::from_rgba_unmultiplied(
                            color.r(), color.g(), color.b(), 210,
                        )
                    };
                    let fill = if sig.is_some_and(|s| !s.auto) { dim_for_no_auto(fill) } else { fill };
                    let rounding = egui::CornerRadius::same(4);
                    painter.rect_filled(block_rect, rounding, fill);
                    // Slightly darker left edge strip for depth
                    painter.rect_filled(
                        egui::Rect::from_min_size(block_rect.min, egui::vec2(3.0, block_h)),
                        egui::CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 },
                        egui::Color32::from_rgba_unmultiplied(
                            (color.r() as i32 - 30).max(0) as u8,
                            (color.g() as i32 - 30).max(0) as u8,
                            (color.b() as i32 - 30).max(0) as u8,
                            255,
                        ),
                    );
                    if collide.contains(&stream_idx) {
                        painter.rect_stroke(
                            block_rect,
                            rounding,
                            egui::Stroke::new(1.5, HL_COLLISION),
                            egui::StrokeKind::Inside,
                        );
                    }
                    // Selection highlight: bright white border
                    let is_selected = selected.contains(&s.segment_id);
                    if is_selected {
                        painter.rect_stroke(
                            block_rect,
                            rounding,
                            egui::Stroke::new(2.0, egui::Color32::WHITE),
                            egui::StrokeKind::Inside,
                        );
                    }

                    // Merge label for this block (non-empty = primary of a merge)
                    let merge_label = merge_labels.get(&s.segment_id).map(String::as_str);

                    // Text inside the block — clip to block_rect so text never
                    // bleeds into adjacent columns.
                    let text_rect = block_rect.shrink2(egui::vec2(5.0, 3.0) * zoom);
                    if text_rect.height() >= 12.0 * zoom {
                        let text_painter = painter.with_clip_rect(block_rect);
                        let name_font = egui::FontId::proportional(11.0 * zoom);
                        let time_font = egui::FontId::proportional(10.0 * zoom);
                        let (name_color, time_color, title_color) = if is_hidden {
                            (
                                egui::Color32::from_white_alpha(110),
                                egui::Color32::from_white_alpha(90),
                                egui::Color32::from_white_alpha(70),
                            )
                        } else {
                            (
                                egui::Color32::WHITE,
                                egui::Color32::from_white_alpha(200),
                                egui::Color32::from_white_alpha(180),
                            )
                        };
                        let text_y = text_rect.top();
                        let badge = source_badge(&s.source).0;
                        // Prepend 🔀 if this block is a merged primary
                        let merge_icon = if merge_label.is_some() { "🔀 " } else { "" };
                        // ⏺ recording-now / ⚡ trigger-would-fire / 🚫 blacklisted
                        let sig_icon = sig.map(EventSignals::icon_prefix).unwrap_or_default();
                        if compact {
                            // One line, start time first: "HH:MM Channel — Title".
                            let mut line = format!(
                                "{}{}{}{} {} {}",
                                if is_hidden { "⊘ " } else { "" },
                                sig_icon,
                                merge_icon,
                                badge,
                                fmt_time_short(s.start_time),
                                s.channel_name,
                            );
                            if !s.title.is_empty() {
                                line.push_str(" — ");
                                line.push_str(&s.title);
                            }
                            text_painter.text(
                                egui::pos2(text_rect.left(), block_rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                line,
                                name_font,
                                name_color,
                            );
                        } else {
                        let name_str = if is_hidden {
                            format!("⊘ {}{}{}{}", sig_icon, merge_icon, badge, s.channel_name)
                        } else {
                            format!("{}{}{} {}", sig_icon, merge_icon, badge, s.channel_name)
                        };
                        text_painter.text(
                            egui::pos2(text_rect.left(), text_y),
                            egui::Align2::LEFT_TOP,
                            name_str,
                            name_font,
                            name_color,
                        );
                        if block_h >= 36.0 * zoom {
                            text_painter.text(
                                egui::pos2(text_rect.left(), text_y + 13.0 * zoom),
                                egui::Align2::LEFT_TOP,
                                fmt_time_range(s.start_time, s.end_time),
                                time_font.clone(),
                                time_color,
                            );
                        }
                        if block_h >= 56.0 * zoom && !s.title.is_empty() {
                            text_painter.text(
                                egui::pos2(text_rect.left(), text_y + 26.0 * zoom),
                                egui::Align2::LEFT_TOP,
                                &s.title,
                                time_font,
                                title_color,
                            );
                        }
                        }
                    }

                    let block_clicked = evt_resp.clicked();
                    let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.shift);
                    let merge_label_owned = merge_label.map(str::to_string);
                    let hover_extra = sig.map(EventSignals::hover_lines).unwrap_or_default();
                    evt_resp
                        .on_hover_text(format!("{}{hover_extra}", schedule_detail_line_merged(s, merge_label)))
                        .context_menu(|ui| schedule_copy_menu(ui, s, is_hidden, merge_label_owned.as_deref()));
                    if block_clicked {
                        if ctrl_held {
                            // Ctrl+click: toggle in selection without opening day
                            ui.ctx().data_mut(|d| {
                                d.insert_temp(egui::Id::new("sched_sel_toggle"), s.segment_id)
                            });
                        } else {
                            // Plain click: select single event + open day popup
                            ui.ctx().data_mut(|d| {
                                d.insert_temp(egui::Id::new("sched_sel_single"), s.segment_id)
                            });
                            clicked_day = Some(day);
                        }
                    }
                }
            }
        });

    if let Some(day) = clicked_day {
        *open_day = Some(day);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(id: i64, start: i64, end: Option<i64>) -> crate::models::UpcomingStream {
        crate::models::UpcomingStream {
            segment_id: id,
            monitor_id: 1,
            channel_id: 1,
            channel_name: "ch".into(),
            url: String::new(),
            start_time: start,
            end_time: end,
            title: String::new(),
            category: String::new(),
            source: "platform".into(),
            channel_color: String::new(),
            merged_into: None,
            auto_merge_excluded: false,
            collab: String::new(),
        }
    }

    /// Compact mode: two long overlapping streams whose *starts* are far apart
    /// share one lane (chips don't collide); two starts within the chip span
    /// still split into side-by-side lanes.
    #[test]
    fn compact_lanes_split_only_when_starts_collide() {
        // 3h-long streams starting 1h apart: overlap in real time, not as chips.
        let all = vec![stream(1, 0, Some(3 * 3600)), stream(2, 3600, Some(4 * 3600))];
        let full = layout_event_lanes(&[0, 1], &all, None);
        assert_eq!(full.iter().map(|&(_, _, t)| t).max(), Some(2), "full mode overlaps");
        let compact = layout_event_lanes(&[0, 1], &all, Some(660));
        assert!(compact.iter().all(|&(_, _, t)| t == 1), "chips fit one lane");
        // Starts 5 min apart with an 11-min chip span → chips collide → 2 lanes.
        let close = vec![stream(1, 0, Some(3600)), stream(2, 300, Some(3600))];
        let compact = layout_event_lanes(&[0, 1], &close, Some(660));
        assert!(compact.iter().all(|&(_, _, t)| t == 2));
    }

    /// Bright colours (e.g. fetched Twitch spring green) darken for white block
    /// text; already-dark palette colours pass through unchanged.
    #[test]
    fn block_safe_color_darkens_only_bright_colors() {
        let bright = egui::Color32::from_rgb(0x00, 0xff, 0x7f); // Twitch spring green
        let safe = block_safe_color(bright);
        assert!(safe.g() < 0xff, "bright green must darken, got {safe:?}");
        // Hue preserved: green still dominates.
        assert!(safe.g() > safe.r() && safe.g() > safe.b());
        let dark = egui::Color32::from_rgb(0x42, 0x88, 0xc4); // palette steel blue
        assert_eq!(block_safe_color(dark), dark);
    }

    fn rule(pattern: &str) -> crate::triggers::TriggerRule {
        crate::triggers::TriggerRule { pattern: pattern.into(), ..Default::default() }
    }

    #[test]
    fn preview_trigger_none_when_nothing_matches() {
        let rules = vec![rule("karaoke")];
        assert!(matches!(
            preview_trigger(&rules, &[], "Just chatting", None),
            TriggerPreview::None
        ));
    }

    #[test]
    fn preview_trigger_would_fire_on_whitelist_match() {
        let rules = vec![rule("karaoke")];
        match preview_trigger(&rules, &[], "Friday Karaoke Night", None) {
            TriggerPreview::WouldFire(hit) => assert_eq!(hit.matched, "Friday Karaoke Night"),
            _ => panic!("expected WouldFire, got a different preview"),
        }
    }

    #[test]
    fn preview_trigger_blacklist_blocks_even_with_a_whitelist_match() {
        // Same title matches BOTH lists — blacklist must win (mirrors
        // supervisor.rs's try_begin: blacklist checked first, unconditional veto).
        let rules = vec![rule("karaoke")];
        let block_rules = vec![rule("rerun")];
        match preview_trigger(&rules, &block_rules, "Karaoke rerun night", None) {
            TriggerPreview::Blocked(hit) => assert_eq!(hit.matched, "Karaoke rerun night"),
            _ => panic!("expected Blocked, got a different preview"),
        }
    }

    #[test]
    fn preview_trigger_matches_on_game_field_too() {
        let rules = vec![crate::triggers::TriggerRule {
            field: crate::triggers::TriggerField::Game,
            ..rule("just chatting")
        }];
        match preview_trigger(&rules, &[], "unrelated title", Some("Just Chatting")) {
            TriggerPreview::WouldFire(hit) => assert_eq!(hit.field, "game"),
            _ => panic!("expected WouldFire on the game field, got a different preview"),
        }
    }

    #[test]
    fn schedule_mode_parse_defaults_to_week() {
        assert!(ScheduleMode::parse("") == ScheduleMode::Week);
        assert!(ScheduleMode::parse("bogus") == ScheduleMode::Week);
        assert!(ScheduleMode::parse("month") == ScheduleMode::Month);
        for m in ScheduleMode::ALL {
            assert!(ScheduleMode::parse(m.as_str()) == m, "round-trip failed for {}", m.label());
        }
    }
}
