//! 🖹 Log view: a live, filterable, colored window over the app's own
//! tracing output (`crate::log_capture`), aiming to make the console/log
//! files unnecessary for day-to-day troubleshooting — search, a minimum-
//! severity filter, a platform quick-filter, and brand-colored tags, all
//! without leaving the app. The console and rotating file log (7-day
//! retention) stay exactly as they are; this is an additional, friendlier
//! view over the same event stream (see `log_capture::LogCaptureLayer`), not
//! a replacement for either.

use super::*;
use crate::log_capture::LogRecord;
use crate::models::Platform;

/// What's currently selected in the level `ComboBox` — `None` shows
/// everything the app's own filter admits (see the hover text on the combo).
type LevelFilter = Option<tracing::Level>;

/// Whether `record` passes the level/platform/text filters currently set.
/// Pure and unit-tested so the filtering logic doesn't have to be exercised
/// through egui to verify it.
fn record_matches(
    record: &LogRecord,
    min_level: LevelFilter,
    platform: Option<Platform>,
    query: &str,
    regex: bool,
) -> bool {
    if let Some(min) = min_level
        && crate::log_capture::level_rank(record.level) > crate::log_capture::level_rank(min)
    {
        return false;
    }
    if let Some(p) = platform
        && crate::log_capture::detect_platform(&record.message) != Some(p)
    {
        return false;
    }
    if query.is_empty() {
        return true;
    }
    if regex {
        return match regex_lite::Regex::new(&format!("(?i){query}")) {
            Ok(re) => re.is_match(&record.message) || re.is_match(&record.fields) || re.is_match(record.target),
            Err(_) => false, // an invalid pattern matches nothing; the error is shown inline
        };
    }
    let q = query.to_lowercase();
    record.message.to_lowercase().contains(&q)
        || record.fields.to_lowercase().contains(&q)
        || record.target.to_lowercase().contains(&q)
}

/// `Some(error message)` when `query` is an invalid regex and regex mode is
/// on — mirrors `triggers::pattern_error`'s inline-error convention.
fn regex_pattern_error(query: &str, regex: bool) -> Option<String> {
    if !regex || query.is_empty() {
        return None;
    }
    regex_lite::Regex::new(&format!("(?i){query}")).err().map(|e| e.to_string())
}

/// One row's rendering inputs, pre-resolved so the row-paint closure does no
/// filtering/formatting work of its own (it just draws).
struct Row {
    /// The source record's sequence number — a stable, unique per-row egui
    /// id salt (see the row loop) and the identity used to build the
    /// right-click "mute like this" suggestion.
    seq: u64,
    time: String,
    level: tracing::Level,
    platform: Option<Platform>,
    text: String, // message + " " + fields, ready to display
}

fn to_row(r: &LogRecord) -> Row {
    use chrono::TimeZone;
    let time = chrono::Utc
        .timestamp_millis_opt(r.time_ms)
        .single()
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string())
        .unwrap_or_default();
    let text =
        if r.fields.is_empty() { r.message.clone() } else { format!("{}  {}", r.message, r.fields) };
    Row {
        seq: r.seq,
        time,
        level: r.level,
        platform: crate::log_capture::detect_platform(&r.message),
        text,
    }
}

fn level_color(level: tracing::Level) -> egui::Color32 {
    match level {
        tracing::Level::ERROR => egui::Color32::from_rgb(230, 80, 80),
        tracing::Level::WARN => egui::Color32::from_rgb(220, 170, 60),
        tracing::Level::INFO => egui::Color32::from_gray(220),
        tracing::Level::DEBUG => egui::Color32::from_rgb(110, 150, 220),
        tracing::Level::TRACE => egui::Color32::from_gray(120),
    }
}

fn level_label(level: tracing::Level) -> &'static str {
    match level {
        tracing::Level::ERROR => "ERROR",
        tracing::Level::WARN => "WARN ",
        tracing::Level::INFO => "INFO ",
        tracing::Level::DEBUG => "DEBUG",
        tracing::Level::TRACE => "TRACE",
    }
}

const ROW_H: f32 = 18.0;

/// Deferred-viewport state for the Log view. `rows` is the current
/// filter's matches, oldest first — extended incrementally from
/// `log_capture::since(last_seq)` each frame the filter hasn't changed, or
/// rebuilt from `log_capture::snapshot()` when it has (see `log_view_window`).
pub(super) struct LogViewPopupState {
    pub(super) closed: bool,
    search: String,
    regex: bool,
    min_level: LevelFilter,
    platform: Option<Platform>,
    follow: bool,
    rows: Vec<Row>,
    last_seq: u64,
    last_filter: (String, bool, LevelFilter, Option<Platform>),
    total_captured: usize,
    /// Text box in the 🔇 Mutes menu for adding a new pattern by hand
    /// (separate from the row-context-menu's instant-add path).
    mute_draft: String,
}

impl LogViewPopupState {
    fn new() -> Self {
        Self {
            closed: false,
            search: String::new(),
            regex: false,
            min_level: None,
            platform: None,
            follow: true,
            rows: Vec::new(),
            last_seq: 0,
            last_filter: (String::new(), false, None, None),
            total_captured: 0,
            mute_draft: String::new(),
        }
    }

    /// Full rescan of the whole buffer against the current filter — the
    /// "filter changed" path of [`Self::refresh`], and also what a mute-list
    /// change needs (muting purges the underlying buffer immediately; `rows`
    /// must reflect that on the very same frame, not wait for the next
    /// unrelated filter edit).
    fn rebuild(&mut self) {
        let snapshot = crate::log_capture::snapshot();
        self.rows = snapshot
            .iter()
            .filter(|r| record_matches(r, self.min_level, self.platform, &self.search, self.regex))
            .map(|r| to_row(r))
            .collect();
        self.last_seq = snapshot.last().map(|r| r.seq).unwrap_or(0);
        self.last_filter = (self.search.clone(), self.regex, self.min_level, self.platform);
    }

    /// Bring `rows` up to date with the current filter and the buffer's
    /// latest content. A changed filter forces a full rescan (still just a
    /// `contains`/regex pass over ≤50,000 short strings — fine on a filter
    /// edit, not something to do every frame); an unchanged filter only
    /// scans records newer than `last_seq`, so a quiet log costs nothing per
    /// frame and a busy one costs proportional to how much actually arrived.
    fn refresh(&mut self) {
        let key = (self.search.clone(), self.regex, self.min_level, self.platform);
        if key != self.last_filter {
            self.rebuild();
        } else {
            let newer = crate::log_capture::since(self.last_seq);
            if !newer.is_empty() {
                self.last_seq = newer.last().map(|r| r.seq).unwrap_or(self.last_seq);
                self.rows.extend(
                    newer
                        .iter()
                        .filter(|r| {
                            record_matches(r, self.min_level, self.platform, &self.search, self.regex)
                        })
                        .map(|r| to_row(r)),
                );
            }
        }
        self.total_captured = crate::log_capture::len();
    }
}

impl StreamArchiverApp {
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn log_view_window(&mut self, ctx: &egui::Context) {
        if !self.show_log_view {
            self.log_view_popup = None;
            return;
        }
        if self.log_view_popup.is_none() {
            self.log_view_popup = Some(Arc::new(Mutex::new(LogViewPopupState::new())));
        }
        let popup_state = self.log_view_popup.clone().unwrap();

        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("log_view_vp"),
            egui::ViewportBuilder::default()
                .with_title("🖹 Log")
                .with_inner_size([980.0, 560.0]),
            popup_state.clone(),
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                s.refresh();
                // Live-tailing: keep the frame loop running while open so new
                // lines actually arrive without needing input to wake it up.
                ctx.request_repaint_after(std::time::Duration::from_millis(250));

                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("log_min_level")
                            .selected_text(match s.min_level {
                                None => "All levels",
                                Some(tracing::Level::ERROR) => "Error",
                                Some(tracing::Level::WARN) => "Warn+",
                                Some(tracing::Level::INFO) => "Info+",
                                Some(tracing::Level::DEBUG) => "Debug+",
                                Some(tracing::Level::TRACE) => "Trace+",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.min_level, None, "All levels");
                                ui.selectable_value(&mut s.min_level, Some(tracing::Level::ERROR), "Error");
                                ui.selectable_value(&mut s.min_level, Some(tracing::Level::WARN), "Warn+");
                                ui.selectable_value(&mut s.min_level, Some(tracing::Level::INFO), "Info+");
                                ui.selectable_value(&mut s.min_level, Some(tracing::Level::DEBUG), "Debug+");
                                ui.selectable_value(&mut s.min_level, Some(tracing::Level::TRACE), "Trace+");
                            })
                            .response
                            .on_hover_text(
                                "Minimum severity to show. This only narrows what's already \
                                 captured — the app's own log filter (default: info level, \
                                 debug for streamarchiver's own code; RUST_LOG overrides it) \
                                 decides what's captured in the first place, same as the \
                                 console/file log.",
                            );

                        egui::ComboBox::from_id_salt("log_platform_filter")
                            .selected_text(s.platform.map(|p| p.label()).unwrap_or("All platforms"))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.platform, None, "All platforms");
                                for p in Platform::ALL {
                                    ui.selectable_value(&mut s.platform, Some(p), p.label());
                                }
                            })
                            .response
                            .on_hover_text("Only lines tagged with this platform, e.g. [Twitch].");

                        ui.add(
                            egui::TextEdit::singleline(&mut s.search)
                                .hint_text("Search…")
                                .desired_width(220.0),
                        )
                        .on_hover_text("Matches the message, its fields, and the module path.");
                        if !s.search.is_empty()
                            && ui.button("✕").on_hover_text("Clear search").clicked()
                        {
                            s.search.clear();
                        }
                        ui.checkbox(&mut s.regex, "Regex").on_hover_text(
                            "Treat the search box as a case-insensitive regular expression \
                             instead of a plain substring match.",
                        );
                        if let Some(err) = regex_pattern_error(&s.search, s.regex) {
                            ui.colored_label(egui::Color32::from_rgb(230, 100, 100), "⚠")
                                .on_hover_text(format!("Invalid pattern: {err}"));
                        }

                        ui.checkbox(&mut s.follow, "Follow").on_hover_text(
                            "Auto-scroll to the newest line. Turned off automatically doesn't \
                             happen — scroll up yourself to read older lines without fighting \
                             the tail, then re-enable this to jump back to live.",
                        );

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("📂 Open logs folder")
                                .on_hover_text(
                                    "Open the rotating file log's folder — this view is a live \
                                     session-only window; the file log is the durable record \
                                     (7-day retention).",
                                )
                                .clicked()
                            {
                                crate::platform::open_path(&crate::app_paths::logs_dir());
                            }
                            if ui
                                .button("📋 Copy")
                                .on_hover_text("Copy every currently-visible (filtered) line to the clipboard.")
                                .clicked()
                            {
                                let text = s
                                    .rows
                                    .iter()
                                    .map(|r| format!("{} {} {}", r.time, level_label(r.level), r.text))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                ui.ctx().copy_text(text);
                            }
                            if ui
                                .button("🗑 Clear")
                                .on_hover_text(
                                    "Clear this in-memory view only — the file log is untouched.",
                                )
                                .clicked()
                            {
                                crate::log_capture::clear();
                                s.rows.clear();
                                s.last_seq = 0;
                            }
                            let mutes = crate::log_capture::mute_list();
                            let mut mutes_changed = false;
                            ui.menu_button(format!("🔇 Mutes ({})", mutes.len()), |ui| {
                                if mutes.is_empty() {
                                    ui.weak("Nothing muted. Right-click a line to mute lines like it, or add a pattern below.");
                                } else {
                                    for (i, m) in mutes.iter().enumerate() {
                                        ui.horizontal(|ui| {
                                            if ui.small_button("✕").on_hover_text("Un-mute").clicked() {
                                                crate::log_capture::remove_mute(i);
                                                mutes_changed = true;
                                            }
                                            ui.label(m);
                                        });
                                    }
                                    ui.separator();
                                }
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut s.mute_draft)
                                            .hint_text("Add pattern…")
                                            .desired_width(160.0),
                                    );
                                    if ui.button("Mute").clicked() && !s.mute_draft.trim().is_empty() {
                                        crate::log_capture::add_mute(&s.mute_draft);
                                        s.mute_draft.clear();
                                        mutes_changed = true;
                                    }
                                });
                            })
                            .response
                            .on_hover_text(
                                "Case-insensitive substrings that stop matching lines from ever \
                                 being captured — not just hidden, but never buffered, so a \
                                 noisy or runaway source (e.g. a debug-only warning that logs \
                                 itself into more of the same warning) can't drown out \
                                 everything else. Session-only; a restart clears the list.",
                            );
                            if mutes_changed {
                                s.rebuild();
                            }
                            ui.weak(format!(
                                "{} / {} captured",
                                s.rows.len(),
                                s.total_captured
                            ))
                            .on_hover_text(format!(
                                "Ring buffer holds up to {} lines this session; older ones \
                                 have already scrolled out and are only in the file log.",
                                crate::log_capture::CAPACITY
                            ));
                        });
                    });
                    ui.separator();

                    if s.rows.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| {
                            ui.weak(if s.total_captured == 0 {
                                "No log lines captured yet."
                            } else {
                                "No lines match the current filter."
                            })
                        });
                        return;
                    }

                    // Deferred rather than mutating `s` from inside the row
                    // closure below: `row` borrows `s.rows`, and adding a
                    // mute needs `&mut s` (to rebuild `rows` from the
                    // now-purged buffer) — the two can't overlap.
                    let mut mute_requested: Option<String> = None;

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(s.follow)
                        .show_rows(ui, ROW_H, s.rows.len(), |ui, range| {
                            for row in &s.rows[range] {
                                let resp = ui
                                    .horizontal(|ui| {
                                        ui.set_height(ROW_H);
                                        ui.label(
                                            egui::RichText::new(&row.time).monospace().weak(),
                                        );
                                        ui.label(
                                            egui::RichText::new(level_label(row.level))
                                                .monospace()
                                                .strong()
                                                .color(level_color(row.level)),
                                        );
                                        if let Some(p) = row.platform {
                                            let (r, g, b) = crate::logfmt::PlatTag(p).rgb();
                                            ui.label(
                                                egui::RichText::new(format!("[{}]", p.label()))
                                                    .monospace()
                                                    .color(egui::Color32::from_rgb(r, g, b)),
                                            );
                                        }
                                        ui.add(
                                            egui::Label::new(egui::RichText::new(&row.text).monospace())
                                                .wrap_mode(egui::TextWrapMode::Truncate),
                                        );
                                    })
                                    .response;
                                // A stable id salted on the record's own seq
                                // (unique, never reused) rather than letting
                                // egui derive one from structural position —
                                // this row's slot in the visible range shifts
                                // constantly as the log grows/scrolls.
                                let id = ui.make_persistent_id(("log_row", row.seq));
                                let resp = ui.interact(resp.rect, id, egui::Sense::click());
                                resp.context_menu(|ui| {
                                    if ui.button("🔇 Mute lines like this").clicked() {
                                        mute_requested =
                                            Some(crate::log_capture::suggested_mute_pattern(&row.text));
                                        ui.close();
                                    }
                                    if ui.button("📋 Copy line").clicked() {
                                        ui.ctx().copy_text(format!(
                                            "{} {} {}",
                                            row.time,
                                            level_label(row.level),
                                            row.text
                                        ));
                                        ui.close();
                                    }
                                });
                            }
                        });

                    if let Some(pattern) = mute_requested {
                        crate::log_capture::add_mute(&pattern);
                        s.rebuild();
                    }
                });
            },
        );
        if popup_state.lock().unwrap().closed {
            self.show_log_view = false;
            self.log_view_popup = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(level: tracing::Level, message: &str, fields: &str) -> LogRecord {
        LogRecord { seq: 0, time_ms: 0, level, target: "streamarchiver::test", message: message.into(), fields: fields.into() }
    }

    #[test]
    fn min_level_hides_less_severe_records() {
        let r = rec(tracing::Level::DEBUG, "poll tick", "");
        assert!(record_matches(&r, None, None, "", false));
        assert!(record_matches(&r, Some(tracing::Level::DEBUG), None, "", false));
        assert!(!record_matches(&r, Some(tracing::Level::INFO), None, "", false));
        assert!(!record_matches(&r, Some(tracing::Level::WARN), None, "", false));
    }

    #[test]
    fn platform_filter_matches_the_embedded_tag_only() {
        let yt = rec(tracing::Level::INFO, "recording finished: [YouTube] girl_dm_", "monitor_id=28");
        let tw = rec(tracing::Level::INFO, "recording finished: [Twitch] Nihmune", "monitor_id=31");
        assert!(record_matches(&yt, None, Some(Platform::YouTube), "", false));
        assert!(!record_matches(&yt, None, Some(Platform::Twitch), "", false));
        assert!(record_matches(&tw, None, Some(Platform::Twitch), "", false));
        assert!(record_matches(&yt, None, None, "", false), "no platform filter matches everything");
    }

    #[test]
    fn text_search_checks_message_fields_and_target() {
        let r = rec(tracing::Level::WARN, "chapters: embed failed", "rec_id=3212 channel=Nihmune");
        assert!(record_matches(&r, None, None, "embed failed", false));
        assert!(record_matches(&r, None, None, "NIHMUNE", false), "case-insensitive");
        assert!(record_matches(&r, None, None, "rec_id=3212", false), "matches fields too");
        assert!(record_matches(&r, None, None, "streamarchiver::test", false), "matches target too");
        assert!(!record_matches(&r, None, None, "girl_dm_", false));
    }

    #[test]
    fn regex_mode_matches_patterns_and_invalid_patterns_match_nothing() {
        let r = rec(tracing::Level::ERROR, "monitor_id=28 rec_id=3212", "");
        assert!(record_matches(&r, None, None, r"rec_id=\d+", true));
        assert!(!record_matches(&r, None, None, r"rec_id=\d{5,}", true));
        // Unbalanced group — invalid regex, must not panic or match everything.
        assert!(!record_matches(&r, None, None, "(unterminated", true));
    }

    #[test]
    fn regex_pattern_error_is_none_unless_regex_mode_is_on_and_broken() {
        assert!(regex_pattern_error("(unterminated", false).is_none(), "not in regex mode");
        assert!(regex_pattern_error("", true).is_none(), "empty query");
        assert!(regex_pattern_error("valid.*pattern", true).is_none());
        assert!(regex_pattern_error("(unterminated", true).is_some());
    }

    #[test]
    fn to_row_joins_message_and_fields_with_a_gap() {
        let r = rec(tracing::Level::INFO, "recording finished", "monitor_id=28 bytes=0");
        let row = to_row(&r);
        assert_eq!(row.text, "recording finished  monitor_id=28 bytes=0");
        let no_fields = rec(tracing::Level::INFO, "just a message", "");
        assert_eq!(to_row(&no_fields).text, "just a message");
    }
}
