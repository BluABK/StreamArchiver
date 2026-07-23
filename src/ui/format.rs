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

/// Re-register the I/O monitor's recordings roots: current instance output
/// dirs + the default output dir + every dir PAST recordings live in (a drive
/// an instance moved away from must stay classified and disk-sampled).
pub(super) fn refresh_iomon_roots(store: &crate::store::Store, default_dir: &str) {
    let mut roots: Vec<std::path::PathBuf> = store
        .all_output_dirs()
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();
    let d = default_dir.trim();
    if !d.is_empty() {
        roots.push(std::path::PathBuf::from(d));
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
pub(super) fn take_size_bytes(fs_probes: &mut FsProbes, t: &Recording) -> u64 {
    if t.is_active() {
        fs_probes.live_len(std::path::Path::new(&t.output_path))
    } else {
        t.bytes.max(0) as u64
    }
}

/// Hover text for a stream group's total-size label: the byte total plus an
/// average bitrate (a quick way to eyeball whether a take actually captured
/// at the expected quality — a stream that should be 1080p60 but averages
/// 2 Mbps is worth a second look).
pub(super) fn stream_size_hover(total_bytes: u64, captured_secs: i64) -> String {
    let base = format!("{} captured across all takes", fmt_bytes(total_bytes as i64));
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
        assert!(s.starts_with("953.7 MB captured across all takes"), "got {s:?}");
    }

    #[test]
    fn stream_size_hover_omits_bitrate_without_a_duration() {
        // No captured time yet (e.g. a probe landed before duration_secs did)
        // — nothing to divide by, so just the byte count, no "average" line.
        let s = stream_size_hover(500, 0);
        assert!(!s.contains("average"), "got {s:?}");
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
