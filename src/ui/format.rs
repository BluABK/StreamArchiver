//! Date/time/duration display formatting, DateFmt, and small settings
//! helpers.

use super::*;

/// A wrapping description label for settings grids. Grid cells hand labels
/// unbounded width, so a long help text stretches the whole window sideways
/// instead of wrapping — cap the cell and wrap inside it.
pub(super) fn setting_desc(ui: &mut egui::Ui, text: &str) {
    ui.scope(|ui| {
        ui.set_max_width(620.0);
        ui.add(egui::Label::new(text).wrap());
    });
}

/// Re-register the I/O monitor's recordings roots: current instance/video
/// output dirs + the default output dir + the default video-download dir +
/// every dir PAST recordings live in (a drive an instance moved away from
/// must stay classified and disk-sampled).
pub(super) fn refresh_iomon_roots(
    store: &crate::store::Store,
    default_dir: &str,
    default_video_dir: &str,
) {
    let mut roots: Vec<std::path::PathBuf> = store
        .all_output_dirs()
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();
    for d in [default_dir, default_video_dir] {
        let d = d.trim();
        if !d.is_empty() {
            roots.push(std::path::PathBuf::from(d));
        }
    }
    // The dedicated chat-log root (when configured) counts as a recordings
    // surface too — read from the live static so callers don't need to
    // thread it through (set_chat_root always runs before this on save).
    if let Some(chat_root) = crate::chat::chat_root() {
        roots.push(chat_root);
    }
    roots.extend(crate::downloader::historical_recording_dirs(store));
    crate::iomon::set_recordings_roots(roots);
}

pub(super) fn setting_or_empty(core: &AppCore, key: &str) -> String {
    core.store
        .get_setting(key)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Predefined filename-template presets shown in the preset dropdowns.
/// `(display_label, template_string)`
pub(super) const FILENAME_PRESETS: &[(&str, &str)] = &[
    ("Name + date",                "{name}_{date}_{time}"),
    ("Name + title + date",        "{name}_{title}_{date}_{time}"),
    ("Name + date + title",        "{name}_{date}_{time}_{title}"),
    ("Name + date + title + game", "{name}_{date}_{time}_{title}_{games}"),
    ("Date + name",                "{date}_{time}_{name}"),
    ("Date + name + title",        "{date}_{time}_{name}_{title}"),
    ("Date + name + title + game", "{date}_{time}_{name}_{title}_{games}"),
];

/// Predefined quality presets for the video downloader's Quality dropdown.
/// `(display_label, quality_value, tooltip)` — the value is the symbolic
/// string stored in the quality field; `downloader::plan` translates it into
/// the actual yt-dlp `-f` selector / streamlink stream-name chain, so the
/// tool itself picks the real best formats and nobody has to list format IDs
/// by hand.
pub(super) const QUALITY_PRESETS: &[(&str, &str, &str)] = &[
    (
        "Auto — best available",
        "best",
        "Highest resolution the site offers, no cap (8K included), \
         merged with the best audio.",
    ),
    (
        "Auto — up to 4K (2160p)",
        "2160p",
        "Best available video no taller than 2160 pixels, merged with the \
         best audio.",
    ),
    (
        "Auto — up to 1440p",
        "1440p",
        "Best available video no taller than 1440 pixels, merged with the \
         best audio.",
    ),
    (
        "Auto — up to 1080p",
        "1080p",
        "Best available video no taller than 1080 pixels, merged with the \
         best audio.",
    ),
    (
        "Auto — up to 720p",
        "720p",
        "Best available video no taller than 720 pixels, merged with the \
         best audio.",
    ),
    (
        "Auto — up to 480p",
        "480p",
        "Best available video no taller than 480 pixels, merged with the \
         best audio.",
    ),
    (
        "Audio only",
        "audio",
        "Best audio track only, no video (streamlink: the audio_only \
         rendition where the site has one).",
    ),
];

/// Actions requested from a [`quality_preset_row`] this frame, applied by the
/// caller once its borrows end.
#[derive(Default)]
pub(super) struct QualityPresetActions {
    /// A custom preset the user clicked "×" on (delete + reload).
    pub(super) delete: Option<i64>,
    /// 💾 clicked: open the save-preset dialog for the current value.
    pub(super) save: bool,
    /// ✏ clicked: open the quality-preset manager (edit names/selectors).
    pub(super) manage: bool,
}

/// Render the full Quality row: preset ComboBox (built-in auto-best presets +
/// the user's saved selectors) · editable value field · 💾 save · ✏ manage.
/// Selecting a preset writes its value into `quality`; the text field edits
/// the same value directly (hand-typed values show as "Manual" in the combo)
/// and is the raw escape hatch for any yt-dlp `-f` selector.
pub(super) fn quality_preset_row(
    ui: &mut egui::Ui,
    id_salt: &str,
    quality: &mut String,
    custom_presets: &[(i64, String, String)],
) -> QualityPresetActions {
    let mut actions = QualityPresetActions::default();
    let current = QUALITY_PRESETS
        .iter()
        .find(|(_, v, _)| *v == quality.as_str())
        .map(|(l, _, _)| *l)
        .or_else(|| {
            custom_presets
                .iter()
                .find(|(_, _, v)| v.as_str() == quality.as_str())
                .map(|(_, n, _)| n.as_str())
        })
        .unwrap_or("Manual");
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(current)
        .width(180.0)
        .show_ui(ui, |ui| {
            for &(label, value, tip) in QUALITY_PRESETS {
                if ui
                    .selectable_label(quality.as_str() == value, label)
                    .on_hover_text(tip)
                    .clicked()
                {
                    *quality = value.to_string();
                }
            }
            if !custom_presets.is_empty() {
                ui.separator();
                ui.add(egui::Label::new(egui::RichText::new("My presets").weak().small()));
                for (id, name, value) in custom_presets {
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(quality.as_str() == value.as_str(), name.as_str())
                            .on_hover_text(value.as_str())
                            .clicked()
                        {
                            *quality = value.clone();
                        }
                        if ui
                            .small_button("×")
                            .on_hover_text("Delete this preset")
                            .clicked()
                        {
                            actions.delete = Some(*id);
                        }
                    });
                }
            }
        });
    ui.add(
        egui::TextEdit::singleline(quality)
            .desired_width(170.0)
            .hint_text("e.g. 1080p or 137+140"),
    )
    .on_hover_text(
        "The value actually used — type anything here: best · <N>p (max \
         height) · audio · a format ID pair like 137+140 · or any raw yt-dlp \
         -f selector. Raw selectors override the Audio tracks field. 💾 saves \
         the current value as a named preset; ✏ edits your saved presets.",
    );
    if ui
        .button("💾")
        .on_hover_text("Save the current value as a named preset")
        .clicked()
    {
        actions.save = true;
    }
    if ui
        .button("✏")
        .on_hover_text("Manage saved quality presets (rename, edit selector, add, delete)")
        .clicked()
    {
        actions.manage = true;
    }
    actions
}

/// Render a filename-template preset ComboBox with both built-in and user-defined
/// presets. Selecting a preset writes its template into `template`.
///
/// Returns `(delete_id, open_save)`:
/// - `delete_id` — a custom preset the user clicked "×" on (caller should delete + reload)
/// - `open_save` — the 💾 button was clicked (caller should open the save-preset dialog)
pub(super) fn filename_preset_combo(
    ui: &mut egui::Ui,
    id_salt: &str,
    template: &mut String,
    custom_presets: &[(i64, String, String)],
) -> (Option<i64>, bool) {
    let current = FILENAME_PRESETS
        .iter()
        .find(|(_, t)| *t == template.as_str())
        .map(|(l, _)| *l)
        .or_else(|| {
            custom_presets
                .iter()
                .find(|(_, _, t)| t.as_str() == template.as_str())
                .map(|(_, n, _)| n.as_str())
        })
        .unwrap_or("Manual");
    let mut delete_id: Option<i64> = None;
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(current)
        .width(160.0)
        .show_ui(ui, |ui| {
            for &(label, tmpl) in FILENAME_PRESETS {
                if ui.selectable_label(template.as_str() == tmpl, label).clicked() {
                    *template = tmpl.to_string();
                }
            }
            if !custom_presets.is_empty() {
                ui.separator();
                ui.add(egui::Label::new(egui::RichText::new("My presets").weak().small()));
                for (id, name, tmpl) in custom_presets {
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(template.as_str() == tmpl.as_str(), name.as_str())
                            .clicked()
                        {
                            *template = tmpl.clone();
                        }
                        if ui
                            .small_button("×")
                            .on_hover_text("Delete this preset")
                            .clicked()
                        {
                            delete_id = Some(*id);
                        }
                    });
                }
            }
        });
    let open_save = ui
        .button("💾")
        .on_hover_text("Save current template as a named preset")
        .clicked();
    (delete_id, open_save)
}

/// Coarse human duration: `45s` / `5m` / `6h` / `2d`.
pub(super) fn parse_capture_mode(fname: &str) -> Option<String> {
    let marker = " (p ";
    let start = fname.find(marker)? + marker.len();
    let end = fname[start..].find(')')?;
    let mode = fname[start..start + end].trim();
    if mode.is_empty() { None } else { Some(mode.to_string()) }
}

pub(super) fn fmt_duration_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s < 90 {
        format!("{s}s")
    } else if s < 90 * 60 {
        format!("{}m", (s + 30) / 60)
    } else if s < 36 * 3600 {
        format!("{}h", (s + 1800) / 3600)
    } else {
        format!("{}d", (s + 12 * 3600) / 86_400)
    }
}

/// `now` / `in 3m` for a future delta in seconds.
pub(super) fn fmt_relative_future(delta: i64) -> String {
    if delta <= 0 {
        "now".to_string()
    } else {
        format!("in {}", fmt_duration_secs(delta))
    }
}

/// Label a take as "SABR"/"DASH" when it's part of a dual capture (two recordings
/// sharing a `take_group`). Returns `None` for ordinary single-recording takes.
pub(super) fn dual_take_variant(g: &StreamGroup, t: &Recording) -> Option<&'static str> {
    // Only label takes that belong to a multi-recording (dual) capture cluster.
    let in_dual = g
        .take_groups()
        .iter()
        .any(|grp| grp.len() >= 2 && grp.iter().any(|r| r.id == t.id));
    if !in_dual {
        return None;
    }
    if t.output_path.contains(".dash.") {
        Some("DASH")
    } else {
        Some("SABR")
    }
}

/// Current size of one take, for the Streams grid's size display. A finished
/// take's `bytes` is already the final, free-to-read value (set once at
/// finalize — see `finish_recording`); a still-`is_active()` take hasn't
/// written that column yet, so it needs a live probe instead (a plain
/// directory-entry read stays near-zero for the whole session while ffmpeg/
/// streamlink holds the file open — see `live_file_len`'s doc comment).
///
/// `bytes` is a one-time snapshot: nothing clears it when a later VOD
/// backfill/recovery attempt fails or the file is deleted/trashed, so a
/// finished take can carry a stale nonzero `bytes` for media that no longer
/// exists. Confirming the file is still there before trusting `bytes` is
/// cheap here — [`FsProbes::is_file`] is the same never-blocking cache every
/// other row probe uses, defaulting to "missing" only until its first result
/// lands.
pub(super) fn take_size_bytes(fs_probes: &mut FsProbes, t: &Recording) -> u64 {
    if t.is_active() {
        return fs_probes.live_len(std::path::Path::new(&t.output_path));
    }
    let bytes = t.bytes.max(0) as u64;
    if bytes == 0 || t.output_path.is_empty() {
        return 0;
    }
    if fs_probes.is_file(std::path::Path::new(&t.output_path)) { bytes } else { 0 }
}

/// Attribute a monitor-scoped `stream_stats_for_monitor` result set to one
/// specific take: exact `stream_id` match when the take has one (the
/// reliable case — `stream_stats_for_monitor` only ever groups samples that
/// carry a stream id), else the broadcast whose sampled `[started, ended]`
/// envelope overlaps the take's own `[started_at, ended_at]` window (±15 min,
/// matching `Store::stream_stats_breakdown`'s own time-window fallback) —
/// covers the take/scrape-path recordings that never got an id stamped.
/// `None` when nothing overlaps (too old, too short to sample, or the take
/// is still live and hasn't accumulated a settled window yet).
pub(super) fn find_take_stats<'a>(
    stats: &'a [crate::models::StreamStatRow],
    t: &Recording,
) -> Option<&'a crate::models::StreamStatRow> {
    if let Some(sid) = t.stream_id.as_deref().filter(|s| !s.is_empty()) {
        return stats.iter().find(|s| s.stream_id == sid);
    }
    let end = t.ended_at.unwrap_or(t.started_at);
    stats
        .iter()
        .find(|s| t.started_at <= s.ended + 900 && end >= s.started - 900)
}

/// Compact multi-line summary of a broadcast's event totals (`[subs,
/// gifted, bits, raids in, raids out, mod actions]`, same layout as
/// `StreamStatRow::totals`) for a stats hover — a zero category is omitted
/// entirely rather than padding the tooltip with "0 bits" lines.
pub(super) fn format_event_totals(totals: [i64; 6]) -> String {
    let [subs, gifted, bits, rin, rout, mods] = totals;
    let mut lines = Vec::new();
    if subs > 0 || gifted > 0 {
        lines.push(if gifted > 0 {
            format!("{subs} subs (+{gifted} gifted)")
        } else {
            format!("{subs} subs")
        });
    }
    if bits > 0 {
        lines.push(format!("{bits} bits"));
    }
    if rin > 0 || rout > 0 {
        lines.push(format!("{rin} raids in, {rout} raids out"));
    }
    if mods > 0 {
        lines.push(format!("{mods} mod actions (deletions/timeouts/bans)"));
    }
    lines.join("\n")
}

/// Hover text for a stream group's total-size label: the byte total plus an
/// average bitrate (a quick way to eyeball whether a take actually captured
/// at the expected quality — a stream that should be 1080p60 but averages
/// 2 Mbps is worth a second look).
pub(super) fn stream_size_hover(total_bytes: u64, captured_secs: i64) -> String {
    // "on disk", not "captured": a take's size now includes whatever a
    // head-backfill join or gap splice added, which was fetched from the CDN
    // rather than captured live. Same correction the Background view's
    // "Total on disk" got, for the same reason.
    let base = format!("{} on disk across all takes", fmt_bytes(total_bytes as i64));
    if captured_secs <= 0 {
        return base;
    }
    let mbps = (total_bytes as f64 * 8.0) / (captured_secs as f64 * 1_000_000.0);
    format!("{base}\n≈{mbps:.1} Mbps average")
}

/// Split a stored `--cookies-from-browser` value into `(browser, profile)`.
/// `profile` is everything after the first `:` — a profile/session name or an
/// absolute path (which may itself contain a `:` drive letter, hence split-once).
/// yt-dlp parses the same way. Empty profile when there's no `:`.
pub(super) fn split_browser_profile(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((b, p)) => (b.trim().to_string(), p.trim().to_string()),
        None => (raw.trim().to_string(), String::new()),
    }
}

/// Compose a `--cookies-from-browser` value from a browser + optional profile
/// (`firefox` or `firefox:<profile>`). Empty browser → empty (no cookies).
pub(super) fn compose_browser_profile(browser: &str, profile: &str) -> String {
    let b = browser.trim();
    let p = profile.trim();
    if b.is_empty() {
        String::new()
    } else if p.is_empty() {
        b.to_string()
    } else {
        format!("{b}:{p}")
    }
}

/// User-selectable display format for dates/timestamps (the Settings "Date
/// format" control). Read globally via [`active_date_fmt`] so the free-function
/// formatters can honor it without threading the setting through every call site.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) enum DateFmt {
    /// ISO 8601-style `2026-06-21` / `2026-06-21 14:02:33` (the default).
    #[default]
    Iso,
    /// ISO without seconds: `2026-06-21 14:02`.
    IsoNoSecs,
    /// US `06/21/2026` / `06/21/2026 02:02 PM`.
    Us,
    /// European `21.06.2026` / `21.06.2026 14:02`.
    Eu,
    /// Compact, year-less `06-21` / `06-21 14:02:33` (narrowest).
    Compact,
}

impl DateFmt {
    pub(super) const ALL: [DateFmt; 5] = [
        DateFmt::Iso,
        DateFmt::IsoNoSecs,
        DateFmt::Us,
        DateFmt::Eu,
        DateFmt::Compact,
    ];

    pub(super) fn as_str(self) -> &'static str {
        match self {
            DateFmt::Iso => "iso",
            DateFmt::IsoNoSecs => "iso_no_secs",
            DateFmt::Us => "us",
            DateFmt::Eu => "eu",
            DateFmt::Compact => "compact",
        }
    }

    pub(super) fn parse(s: &str) -> DateFmt {
        match s {
            "iso_no_secs" => DateFmt::IsoNoSecs,
            "us" => DateFmt::Us,
            "eu" => DateFmt::Eu,
            "compact" => DateFmt::Compact,
            _ => DateFmt::Iso,
        }
    }

    /// chrono pattern for a date-only value.
    pub(super) fn date_pattern(self) -> &'static str {
        match self {
            DateFmt::Iso | DateFmt::IsoNoSecs => "%Y-%m-%d",
            DateFmt::Us => "%m/%d/%Y",
            DateFmt::Eu => "%d.%m.%Y",
            DateFmt::Compact => "%m-%d",
        }
    }

    /// chrono pattern for a full timestamp.
    pub(super) fn datetime_pattern(self) -> &'static str {
        match self {
            DateFmt::Iso => "%Y-%m-%d %H:%M:%S",
            DateFmt::IsoNoSecs => "%Y-%m-%d %H:%M",
            DateFmt::Us => "%m/%d/%Y %I:%M %p",
            DateFmt::Eu => "%d.%m.%Y %H:%M",
            DateFmt::Compact => "%m-%d %H:%M:%S",
        }
    }

    /// chrono pattern for a time-only value (12-hour for US, else 24-hour). Used
    /// by the Schedule calendar chips, which only have room for the time.
    pub(super) fn time_pattern(self) -> &'static str {
        match self {
            DateFmt::Us => "%I:%M %p",
            _ => "%H:%M",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            DateFmt::Iso => "ISO — 2026-06-21 14:02:33",
            DateFmt::IsoNoSecs => "ISO, no seconds — 2026-06-21 14:02",
            DateFmt::Us => "US — 06/21/2026 02:02 PM",
            DateFmt::Eu => "EU — 21.06.2026 14:02",
            DateFmt::Compact => "Compact — 06-21 14:02:33",
        }
    }
}

/// The active [`DateFmt`] discriminant (index into [`DateFmt::ALL`]). The UI runs
/// single-threaded; this is a cheap shared cell set at startup and on save so the
/// formatters below don't need the setting passed in.
pub(super) static ACTIVE_DATE_FMT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub(super) fn active_date_fmt() -> DateFmt {
    let i = ACTIVE_DATE_FMT.load(std::sync::atomic::Ordering::Relaxed) as usize;
    DateFmt::ALL.get(i).copied().unwrap_or(DateFmt::Iso)
}

pub(super) fn set_active_date_fmt(f: DateFmt) {
    let i = DateFmt::ALL.iter().position(|&x| x == f).unwrap_or(0) as u8;
    ACTIVE_DATE_FMT.store(i, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the "compact timestamps" mode is active. Set at startup and when the
/// top-bar toggle changes so formatters don't need the flag threaded through.
pub(super) static SHORT_TS_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The compact timestamp pattern (e.g. `"%d/%m %H:%M"`). Protected by a mutex so
/// it can be changed at runtime without a full restart.
pub(super) static SHORT_TS_PAT: std::sync::OnceLock<std::sync::Mutex<String>> =
    std::sync::OnceLock::new();

/// Whether the Debug view is available: always in debug builds; in release
/// builds only when launched with `--debug`. Computed once (the process args
/// can't change at runtime).
pub(super) fn debug_view_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        cfg!(debug_assertions) || std::env::args().any(|a| a == "--debug")
    })
}

pub(super) fn short_ts_on() -> bool {
    SHORT_TS_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

pub(super) fn set_short_ts(on: bool) {
    SHORT_TS_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn short_ts_pattern() -> String {
    SHORT_TS_PAT
        .get_or_init(|| std::sync::Mutex::new("%d/%m %H:%M".to_string()))
        .lock()
        .unwrap()
        .clone()
}

pub(super) fn set_short_ts_pattern(pat: &str) {
    *SHORT_TS_PAT
        .get_or_init(|| std::sync::Mutex::new("%d/%m %H:%M".to_string()))
        .lock()
        .unwrap() = pat.to_string();
}

/// Compact variant of [`fmt_datetime_short`] — uses [`short_ts_pattern`] instead of the
/// active [`DateFmt`]. Never checks [`short_ts_on`]; call it only when you want compact.
pub(super) fn fmt_datetime_compact(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    let pat = short_ts_pattern();
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format(&pat).to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as a local date in the active [`DateFmt`] (empty if
/// unset).
pub(super) fn fmt_date(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format(active_date_fmt().date_pattern())
                .to_string()
        })
        .unwrap_or_default()
}
pub(super) fn fmt_datetime_short(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format(active_date_fmt().datetime_pattern())
                .to_string()
        })
        .unwrap_or_default()
}

/// "Polled" cell text: the last-checked timestamp with the poll interval in
/// parentheses, e.g. `2026-06-21 14:02:33 (60s)`. When never polled, shows just
/// the interval `(60s)` so the configured cadence is still visible.
pub(super) fn fmt_polled(last_checked: Option<i64>, interval_secs: i64) -> String {
    let secs = last_checked.unwrap_or(0);
    if short_ts_on() {
        // Compact: HH:MM only — no date, no interval (full info on hover).
        if secs <= 0 {
            return String::new();
        }
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default()
    } else {
        let when = fmt_datetime_short(secs);
        if when.is_empty() {
            format!("({interval_secs}s)")
        } else {
            format!("{when} ({interval_secs}s)")
        }
    }
}

/// Format a duration in seconds as `HH:MM:SS`.
pub(super) fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Local time-of-day for a unix timestamp in the active [`DateFmt`] (e.g. `14:02`
/// or `02:02 PM`). Empty if unset. Used by the Schedule calendar chips.
pub(super) fn fmt_time_short(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format(active_date_fmt().time_pattern())
                .to_string()
        })
        .unwrap_or_default()
}

/// The local calendar date a unix timestamp falls on (for bucketing schedule
/// entries into calendar cells).
pub(super) fn local_date(secs: i64) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.with_timezone(&chrono::Local).date_naive())
}

/// Split a unix timestamp into local `("YYYY-MM-DD", "HH:MM")` for the
/// Edit-schedule dialog fields. Empty pair on an out-of-range timestamp.
pub(super) fn split_local_datetime(unix: i64) -> (String, String) {
    match chrono::DateTime::from_timestamp(unix, 0) {
        Some(dt) => {
            let local = dt.with_timezone(&chrono::Local);
            (
                local.format("%Y-%m-%d").to_string(),
                local.format("%H:%M").to_string(),
            )
        }
        None => (String::new(), String::new()),
    }
}

/// Parse local `YYYY-MM-DD` + `HH:MM` (or `HH:MM:SS`) into unix seconds in the
/// machine's local timezone. `None` on malformed input or a nonexistent/ambiguous
/// local time (a DST gap/overlap), so the Edit dialog can show a validation error.
pub(super) fn parse_local_datetime(date: &str, time: &str) -> Option<i64> {
    use chrono::{NaiveDate, NaiveTime, TimeZone};
    let d = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    let t = NaiveTime::parse_from_str(time.trim(), "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(time.trim(), "%H:%M:%S"))
        .ok()?;
    chrono::Local
        .from_local_datetime(&d.and_time(t))
        .single()
        .map(|dt| dt.timestamp())
}

/// Unix timestamp of local midnight for `d` (falls back to `0` on the
/// essentially-impossible case of no valid local instant that day).
pub(super) fn local_midnight(d: chrono::NaiveDate) -> i64 {
    use chrono::TimeZone;
    chrono::Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// Format seconds-since-local-midnight (a [`ScheduledRecording::time_of_day_secs`])
/// as `HH:MM`.
pub(super) fn split_time_of_day(secs: i64) -> String {
    let secs = secs.clamp(0, 86_399);
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

/// Parse `HH:MM` (or `HH:MM:SS`) into seconds-since-local-midnight. `None` on
/// malformed input.
pub(super) fn parse_time_of_day(time: &str) -> Option<i64> {
    use chrono::{NaiveTime, Timelike};
    let t = NaiveTime::parse_from_str(time.trim(), "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(time.trim(), "%H:%M:%S"))
        .ok()?;
    Some(t.num_seconds_from_midnight() as i64)
}


#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use std::path::PathBuf;

    #[test]
    fn date_fmt_parse_roundtrip() {
        for f in DateFmt::ALL {
            assert_eq!(DateFmt::parse(f.as_str()), f);
        }
        // Unknown / empty falls back to the ISO default.
        assert_eq!(DateFmt::parse("bogus"), DateFmt::Iso);
        assert_eq!(DateFmt::parse(""), DateFmt::Iso);
    }

    #[test]
    fn active_date_fmt_roundtrip() {
        for f in DateFmt::ALL {
            set_active_date_fmt(f);
            assert_eq!(active_date_fmt(), f);
        }
        set_active_date_fmt(DateFmt::Iso); // restore default for other tests
    }

    #[test]
    fn fmt_polled_shows_interval() {
        // Never polled -> just the interval, so the cadence is still visible.
        assert_eq!(fmt_polled(None, 60), "(60s)");
        assert_eq!(fmt_polled(Some(0), 30), "(30s)");
        // Polled -> "<timestamp> (Ns)"; the timestamp is local/tz-dependent, so
        // assert only the stable suffix and that a timestamp is present.
        let s = fmt_polled(Some(1_700_000_000), 45);
        assert!(s.ends_with(" (45s)"), "got {s:?}");
        assert!(s.len() > " (45s)".len());
    }
    #[test]
    fn stream_size_hover_includes_bitrate_when_timed() {
        // 1,000,000,000 bytes / 8s = 8 billion bits / 8s = 1000 Mbps — fmt_bytes
        // is binary (base-1024), so the byte count reads as "953.7 MB", not "1 GB".
        let s = stream_size_hover(1_000_000_000, 8);
        assert!(s.contains("1000.0 Mbps average"), "got {s:?}");
        assert!(s.starts_with("953.7 MB on disk across all takes"), "got {s:?}");
    }

    #[test]
    fn stream_size_hover_omits_bitrate_without_a_duration() {
        // No captured time yet (e.g. a probe landed before duration_secs did)
        // — nothing to divide by, so just the byte count, no "average" line.
        let s = stream_size_hover(500, 0);
        assert!(!s.contains("average"), "got {s:?}");
    }

    fn rec_with_bytes(output_path: &str, bytes: i64) -> Recording {
        Recording {
            id: 1,
            monitor_id: 1,
            started_at: 0,
            ended_at: Some(1),
            status: "completed".into(),
            bytes,
            exit_code: None,
            output_path: output_path.into(),
            went_live_at: None,
            went_live_approx: false,
            lost_secs: None,
            stream_id: None,
            take_group: None,
            ad_count: 0,
            ad_secs: 0,
            meta_change_count: 0,
            title: String::new(),
            category: String::new(),
            log_excerpt: String::new(),
            notes: String::new(),
            vod_id: None,
            vod_state: None,
            vod_muted_secs: None,
            vod_views: None,
            recovery_state: None,
            recovered_path: None,
            vod_dl_state: None,
            vod_dl_path: None,
            vod_dl_video_id: None,
            backfill_path: None,
            full_path: None,
            trigger_info: String::new(),
            head_backfill_state: String::new(),
            gap_splice_state: String::new(),
            trigger_rule_json: String::new(),
            err_ack: false,
            sabr_live_edge_fallback: false,
            chapters_state: String::new(),
            chapters_json: String::new(),
            chapters_attempts: 0,
            chat_path: String::new(),
            rolling: crate::models::Rolling::default(),
            not_recorded_reason: String::new(),
            gated: false,
        }
    }

    /// Poll `drain + take_size_bytes` until it settles on `want` or a deadline
    /// passes — `FsProbes`'s worker answers asynchronously.
    fn wait_take_size(fp: &mut FsProbes, t: &Recording, want: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            fp.drain_results();
            if take_size_bytes(fp, t) == want {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn take_size_bytes_hides_a_stale_byte_count_for_a_gone_file() {
        // The bug this guards: `bytes` is written once at finalize and never
        // cleared, so a take whose file was later deleted or whose VOD
        // backfill failed still carries its old nonzero `bytes` — the Streams
        // grid must not keep showing that size once the file is confirmed
        // gone (see `take_size_bytes`'s doc comment).
        let dir = std::env::temp_dir().join(format!(
            "sa_take_size_{}_{}",
            std::process::id(),
            crate::models::now_unix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("real.mkv");
        std::fs::write(&file, vec![0u8; 1024]).unwrap();
        let t = rec_with_bytes(&file.to_string_lossy(), 1024);
        let mut fp = FsProbes::new(egui::Context::default());

        // File still on disk: the cached `bytes` value is trusted once the
        // probe confirms it.
        assert!(wait_take_size(&mut fp, &t, 1024), "size never settled on 1024");

        // The file vanishes (deleted, or a VOD backfill that never wrote
        // one) — `bytes` itself is untouched, but the probe now says
        // missing, so the badge must drop to 0 rather than keep 1024.
        std::fs::remove_file(&file).unwrap();
        assert!(wait_take_size(&mut fp, &t, 0), "stale size was never cleared");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_size_bytes_skips_the_probe_for_a_zero_byte_or_pathless_take() {
        // A take that never wrote anything (bytes == 0) or has no
        // output_path at all must read 0 without needing any FsProbes result
        // — the pessimistic `is_file` placeholder is `false` on first sight,
        // so these would already read 0, but they must not queue file I/O.
        let mut fp = FsProbes::new(egui::Context::default());
        let t = rec_with_bytes("", 0);
        assert_eq!(take_size_bytes(&mut fp, &t), 0);
        assert!(fp.files.is_empty(), "zero-byte take queued a needless probe");

        let t = rec_with_bytes("", 500);
        assert_eq!(take_size_bytes(&mut fp, &t), 0);
        assert!(fp.files.is_empty(), "pathless take queued a needless probe");
    }

    #[test]
    fn browser_profile_roundtrip() {
        // No profile.
        assert_eq!(split_browser_profile("firefox"), ("firefox".into(), String::new()));
        assert_eq!(compose_browser_profile("firefox", ""), "firefox");

        // Named profile.
        assert_eq!(
            split_browser_profile("firefox:dmrf6eed.YouTube"),
            ("firefox".into(), "dmrf6eed.YouTube".into())
        );
        assert_eq!(
            compose_browser_profile("firefox", "dmrf6eed.YouTube"),
            "firefox:dmrf6eed.YouTube"
        );

        // Absolute-path profile: the drive-letter colon stays in the profile
        // (split on the FIRST colon only, matching yt-dlp).
        let raw = r"firefox:C:\Users\Blu\AppData\Roaming\Mozilla\Firefox\Profiles\dmrf6eed.YouTube";
        let (b, p) = split_browser_profile(raw);
        assert_eq!(b, "firefox");
        assert_eq!(p, r"C:\Users\Blu\AppData\Roaming\Mozilla\Firefox\Profiles\dmrf6eed.YouTube");
        assert_eq!(compose_browser_profile(&b, &p), raw);

        // Empty browser -> empty (no cookies), even with a profile.
        assert_eq!(compose_browser_profile("", "whatever"), "");
    }
}
