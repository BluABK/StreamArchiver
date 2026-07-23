//! Issues and notifications windows, quota warnings.

use super::*;

/// Human-readable byte size (B / KB / MB / GB).
/// Everything the background Issues scan computes off the UI thread — every
/// `path.exists()`/`read_dir`/ffprobe in here can block for seconds against
/// the recordings drive under load.
pub(super) struct IssuesScan {
    /// Output file genuinely gone from disk (and no recoverable parts).
    pub(super) missing: Vec<crate::models::Recording>,
    /// Failed/aborted recordings whose file still exists.
    pub(super) errors_with_file: Vec<crate::models::Recording>,
    /// Failed/aborted recordings whose file is gone.
    pub(super) errors_no_file: Vec<crate::models::Recording>,
    /// File-gone takes whose media survived as split per-format parts in
    /// `.cache\` — recoverable via merge.
    pub(super) unmerged: Vec<(crate::models::Recording, Vec<std::path::PathBuf>)>,
    /// Head/live join blocked by codec parameters: (rec, head, live) with
    /// human-readable stream params.
    pub(super) head_mismatch: Vec<(crate::models::Recording, String, String)>,
    /// Rows still marked `recording` whose files have gone quiet: the capture
    /// died unnoticed (power loss / sleep) or the finalize is still pending.
    /// Paired with the seconds since the last write (`None` = nothing on disk).
    pub(super) stale_recording: Vec<(crate::models::Recording, Option<i64>)>,
}

/// A `recording` row whose newest capture-file write is older than this is
/// listed in Issues as stale (a live capture writes continuously).
const STALE_RECORDING_SECS: i64 = 600;

/// (icon, human label) per capture-alert kind (the 🚨 Warnings window rows).
fn alert_kind_label(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "sequence_gap" => ("⛔", "Lost segments"),
        "fetch_failed" => ("⛔", "Failed segment fetches"),
        "tool_error" => ("❌", "Tool errors"),
        _ => ("⚠", "Tool warnings"),
    }
}

/// One-line summary for a capture-alert row: occurrence count, lost time, and
/// recovery progress where applicable.
fn alert_summary(r: &crate::store::CaptureAlertRow) -> String {
    let mut parts = vec![format!(
        "{} occurrence{}",
        crate::models::group_thousands(r.count),
        if r.count == 1 { "" } else { "s" }
    )];
    if r.lost_segments > 0 {
        // Twitch live segments are 2 s; for yt-dlp fragments this is still a
        // usable order-of-magnitude estimate.
        let secs = r.lost_segments * 2;
        parts.push(format!(
            "{} segments (~{}) of content lost",
            crate::models::group_thousands(r.lost_segments),
            fmt_duration(secs)
        ));
    }
    if r.ranges_total > 0 {
        let mark = if r.recovered == r.ranges_total { " ✔" } else { "" };
        parts.push(format!(
            "{}/{} lost ranges recovered from the VOD{mark}",
            r.recovered, r.ranges_total
        ));
        if r.recovered_muted > 0 {
            parts.push(format!(
                "✂ {} recovered segment(s) use DMCA-muted audio",
                crate::models::group_thousands(r.recovered_muted)
            ));
        }
    }
    parts.join(" · ")
}

/// A human explanation when a capture/resume died on a network/DNS failure —
/// matched against the tool's log tail.
pub(super) fn network_failure_hint(log: &str) -> Option<&'static str> {
    let broken = [
        "getaddrinfo failed",
        "Failed to resolve",
        "[Errno 11001]",
        "Temporary failure in name resolution",
        "Name or service not known",
    ]
    .iter()
    .any(|m| log.contains(m));
    broken.then_some(
        "Likely cause: the network/DNS was unavailable when the download tool \
         (re)started — e.g. the machine woke from sleep before the network came \
         back up, or the connection dropped. The stream itself was fine; only \
         this attempt could not reach the site.",
    )
}

/// Deferred Issues-panel actions: collected while rendering inside the
/// viewport closure, applied after it releases its borrows of `self`.
enum Act {
    Remux(usize),
    RemuxError(usize),
    Delete(usize),
    ClearPath(usize),
    DeleteError(usize),
    ClearError(usize),
    ClearMissingError(usize),
    ClearEmpties,
    ClearAllMissing,
    ClearAllErrors,
    ClearFilelessErrors,
    RecoverStuck(usize),
    ConfirmClear,
    ClearAll,
    DismissWarning(String),
    OpenMutedLive(usize),
    OpenMutedRecovered(usize),
    RerunMuted(usize),
    DismissMuted(usize),
    MergeSplit(usize),
    /// Archive the published VOD for an unmerged-split take (covers the part
    /// of the stream the interrupted capture missed).
    DownloadVodUnmerged(usize),
    /// Settle a stale 'recording' row (Issues → "Finalize now").
    FinalizeStale(usize),
    RefetchHeadMatchLive(usize),
    FetchVodForMismatch(usize),
    DismissMismatch(usize),
    /// Open the error-details window: (title, full text). Same text as the
    /// status-column hover — the 🔍 button makes it readable/copyable.
    ViewError(String, String),
}

/// Status-column explainer for stuck-in-cache rows (hover AND 🔍 window).
const STUCK_IN_CACHE_DETAILS: &str =
    "The recording finished successfully, but moving it out of the hidden \
     working folder failed — most commonly because the filename was too long \
     for the filesystem. The file is safe; it just isn't where it should be \
     yet.";

impl StreamArchiverApp {
    /// Returns the list of active (non-dismissed) quota warning keys.
    /// Each key is a stable string used for both display and dismissal tracking.
    pub(super) fn active_quota_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.yt_quota_today >= self.yt_quota_cutoff {
            let key = "youtube_units_exceeded".to_string();
            if !self.dismissed_quota_warnings.contains(&key) {
                warnings.push(key);
            }
        } else if self.yt_quota_cutoff > 0
            && self.yt_quota_today as f32 / self.yt_quota_cutoff as f32 >= 0.9
        {
            let key = "youtube_units_near_cutoff".to_string();
            if !self.dismissed_quota_warnings.contains(&key) {
                warnings.push(key);
            }
        }
        if self.yt_search_today >= self.yt_search_cutoff {
            let key = "youtube_search_exceeded".to_string();
            if !self.dismissed_quota_warnings.contains(&key) {
                warnings.push(key);
            }
        } else if self.yt_search_cutoff > 0
            && self.yt_search_today as f32 / self.yt_search_cutoff as f32 >= 0.9
        {
            let key = "youtube_search_near_cutoff".to_string();
            if !self.dismissed_quota_warnings.contains(&key) {
                warnings.push(key);
            }
        }
        warnings
    }
    /// The notifications feed window (bell button). A persisted, filterable,
    /// searchable aggregation of went-live / recording / error / schedule /
    /// YouTube-post / task-failure events. Mirrors `issues_window`: the unread
    /// badge count is refreshed on a throttle even while closed, so the header
    /// bell stays live. Both the count and the row list are cheap SQLite reads,
    /// done synchronously.
    #[allow(deprecated)] // CentralPanel::show inside a viewport (matches issues_window)
    pub(super) fn notifications_window(&mut self, ctx: &egui::Context) {
        use std::time::{Duration, Instant};
        let interval = if self.show_notifications {
            Duration::from_secs(3)
        } else {
            Duration::from_secs(60)
        };
        let stale = self
            .notif_refreshed
            .map(|t| t.elapsed() >= interval)
            .unwrap_or(true);
        if stale {
            self.notif_unread = self.core.store.unread_notification_count().unwrap_or(0);
            if self.show_notifications {
                self.notifications = self.core.store.list_notifications(500).unwrap_or_default();
            }
            self.notif_refreshed = Some(Instant::now());
        }
        if !self.show_notifications {
            return;
        }

        let now = crate::models::now_unix();
        let mut open = true;
        // Deferred actions (applied after the viewport closure releases &self).
        enum Act {
            OpenUrl(String),
            MarkAllRead,
        }
        let mut act: Option<Act> = None;

        // Session-only category + text filter over the loaded rows → surviving
        // indices (recomputed each frame from last frame's filter values).
        let q = self.notif_search.trim().to_lowercase();
        let kind_filter = self.notif_kind_filter;
        let visible: Vec<usize> = self
            .notifications
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                kind_filter.map(|k| r.kind == k.id()).unwrap_or(true)
                    && (q.is_empty()
                        || r.title.to_lowercase().contains(&q)
                        || r.body.to_lowercase().contains(&q)
                        || r.channel.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect();

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("notifications_vp"),
            egui::ViewportBuilder::default()
                .with_title("🔔 Notifications")
                .with_inner_size([720.0, 520.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    // ── Toolbar: kind filter + search + mark-all-read ──
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("notif_kind_filter")
                            .selected_text(match self.notif_kind_filter {
                                None => "All kinds".to_string(),
                                Some(k) => k.label().to_string(),
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.notif_kind_filter, None, "All kinds");
                                for k in crate::models::NotificationKind::ALL {
                                    ui.selectable_value(
                                        &mut self.notif_kind_filter,
                                        Some(k),
                                        format!("{} {}", k.icon(), k.label()),
                                    );
                                }
                            });
                        ui.add(
                            egui::TextEdit::singleline(&mut self.notif_search)
                                .hint_text("Filter…")
                                .desired_width(180.0),
                        );
                        if !self.notif_search.is_empty()
                            && ui.button("✕").on_hover_text("Clear filter").clicked()
                        {
                            self.notif_search.clear();
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("✔ Mark all read").clicked() {
                                act = Some(Act::MarkAllRead);
                            }
                        });
                    });
                    ui.separator();

                    if self.notifications.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| ui.weak("No notifications yet."));
                        return;
                    }
                    if visible.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| ui.weak("No notifications match the filter."));
                        return;
                    }

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for &i in &visible {
                                let r = &self.notifications[i];
                                let accent = match r.severity.as_str() {
                                    "error" => egui::Color32::from_rgb(220, 90, 90),
                                    "warn" => egui::Color32::from_rgb(210, 160, 60),
                                    _ => ui.visuals().hyperlink_color,
                                };
                                let icon = crate::models::NotificationKind::from_id(&r.kind)
                                    .map(|k| k.icon())
                                    .unwrap_or("•");
                                egui::Frame::group(ui.style()).show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        // Unread rows show a filled accent dot; read rows a dim one.
                                        ui.label(
                                            egui::RichText::new(if r.read { "○" } else { "●" })
                                                .small()
                                                .color(accent),
                                        );
                                        ui.label(egui::RichText::new(icon).color(accent));
                                        ui.vertical(|ui| {
                                            let mut title = egui::RichText::new(&r.title).strong();
                                            if !r.read {
                                                title = title.color(accent);
                                            }
                                            ui.label(title);
                                            if !r.body.is_empty() {
                                                ui.label(egui::RichText::new(&r.body).weak());
                                            }
                                            ui.horizontal(|ui| {
                                                ui.small(fmt_datetime_short(r.created_at));
                                                if !r.channel.is_empty() {
                                                    ui.small(format!("· {}", r.channel));
                                                }
                                            });
                                        });
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if !r.action_url.is_empty() {
                                                    let label = if r.action_label.is_empty() {
                                                        "Open"
                                                    } else {
                                                        r.action_label.as_str()
                                                    };
                                                    if ui.button(label).clicked() {
                                                        act = Some(Act::OpenUrl(r.action_url.clone()));
                                                    }
                                                }
                                            },
                                        );
                                    });
                                });
                            }
                        });
                });
            },
        );

        if !open {
            self.show_notifications = false;
        }
        match act {
            Some(Act::OpenUrl(url)) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Some(Act::MarkAllRead) => {
                let _ = self.core.store.mark_notifications_read_before(now);
                self.notif_unread = 0;
                for r in &mut self.notifications {
                    r.read = true;
                }
            }
            None => {}
        }
    }

    /// 🚨 Warnings window: capture alerts scraped from the tools' own log
    /// files (streamlink sequence gaps / failed fetches = lost data, yt-dlp
    /// ERROR/WARNING lines). One aggregated row per (take, kind); red rows are
    /// errors, yellow rows warnings; acknowledging clears the header badge but
    /// keeps the row — new occurrences un-acknowledge automatically. The badge
    /// counts refresh on the same open/closed throttle as the bell.
    #[allow(deprecated)] // CentralPanel::show(ctx) inside a viewport
    pub(super) fn warnings_window(&mut self, ctx: &egui::Context) {
        use std::time::{Duration, Instant};
        let interval = if self.show_warnings {
            Duration::from_secs(3)
        } else {
            Duration::from_secs(60)
        };
        let stale = self.warn_refreshed.map(|t| t.elapsed() >= interval).unwrap_or(true);
        if stale {
            self.warn_badge = self.core.store.alert_badge_counts().unwrap_or((0, 0));
            // The Streams-grid take/stream badges ride the same throttle.
            self.rec_alert_badges =
                self.core.store.alert_badges_by_recording().unwrap_or_default();
            if self.show_warnings {
                self.warnings_rows = self.core.store.list_capture_alerts(500).unwrap_or_default();
            }
            self.warn_refreshed = Some(Instant::now());
        }
        if !self.show_warnings {
            return;
        }

        let mut open = true;
        // Deferred actions (applied after the viewport closure releases &self).
        enum Act {
            Ack(i64),
            AckAll,
            /// Batch-ack every alert of one category ("Ack all disk full").
            AckGroup(Vec<i64>),
            OpenLog(String),
            /// Open the folder holding a recording's recovered patch files.
            OpenPatches(i64),
        }
        let mut act: Option<Act> = None;

        let q = self.warn_search.trim().to_lowercase();
        let sev = self.warn_sev_filter;
        let visible: Vec<usize> = self
            .warnings_rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                let cat = crate::downloader::alert_category(&r.kind, &r.last_line).1;
                (!self.warn_hide_acked || !r.acked)
                    && sev.map(|errs_only| (r.severity == "error") == errs_only).unwrap_or(true)
                    && (q.is_empty()
                        || r.channel.to_lowercase().contains(&q)
                        || r.kind.to_lowercase().contains(&q)
                        || cat.to_lowercase().contains(&q)
                        || r.last_line.to_lowercase().contains(&q)
                        || r.take_key.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect();
        // Unacked alerts grouped by category, for the "Ack group" menu —
        // plus a state-based "Fixed" group covering every green row (fully
        // recovered or superseded), so healed history clears in one click.
        let mut ack_groups: std::collections::BTreeMap<(&str, &str), Vec<i64>> =
            std::collections::BTreeMap::new();
        let mut fixed_ids: Vec<i64> = Vec::new();
        for r in &self.warnings_rows {
            if !r.acked {
                let cat = crate::downloader::alert_category(&r.kind, &r.last_line);
                ack_groups.entry(cat).or_default().push(r.id);
                // Mirrors the row-tint logic below: healed (every lost range
                // re-fetched) or superseded (a later completed take covers
                // the dead one).
                let healed = r.ranges_total > 0 && r.recovered == r.ranges_total;
                let superseded = !healed
                    && r.severity == "error"
                    && r.superseded
                    && r.ranges_total == 0;
                if healed || superseded {
                    fixed_ids.push(r.id);
                }
            }
        }

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("warnings_vp"),
            egui::ViewportBuilder::default()
                .with_title("🚨 Capture warnings")
                .with_inner_size([860.0, 520.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("warn_sev_filter")
                            .selected_text(match self.warn_sev_filter {
                                None => "All severities",
                                Some(true) => "Errors only",
                                Some(false) => "Warnings only",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.warn_sev_filter, None, "All severities");
                                ui.selectable_value(&mut self.warn_sev_filter, Some(true), "Errors only");
                                ui.selectable_value(&mut self.warn_sev_filter, Some(false), "Warnings only");
                            })
                            .response
                            .on_hover_text(
                                "Errors mean data is missing from a capture (lost segments, \
                                 failed fetches, tool errors); warnings are non-fatal tool \
                                 complaints.",
                            );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.warn_search)
                                .hint_text("Filter…")
                                .desired_width(180.0),
                        )
                        .on_hover_text("Matches channel, kind, file path, and the last log line.");
                        if !self.warn_search.is_empty()
                            && ui.button("✕").on_hover_text("Clear filter").clicked()
                        {
                            self.warn_search.clear();
                        }
                        ui.checkbox(&mut self.warn_hide_acked, "Hide acknowledged").on_hover_text(
                            "Only show alerts that still need attention — acknowledged rows \
                             (including healed/superseded ones you've cleared) drop out of \
                             the list until new damage un-acknowledges them again.",
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("✔ Acknowledge all")
                                .on_hover_text(
                                    "Clears the header badge for every listed alert. Rows stay \
                                     here for reference; an alert that keeps occurring will \
                                     re-light the badge on its next occurrence.",
                                )
                                .clicked()
                            {
                                act = Some(Act::AckAll);
                            }
                            ui.menu_button("✔ Ack group…", |ui| {
                                if ack_groups.is_empty() {
                                    ui.weak("Nothing unacknowledged.");
                                }
                                if !fixed_ids.is_empty() {
                                    if ui
                                        .button(format!("✅ Fixed ({})", fixed_ids.len()))
                                        .on_hover_text(
                                            "Acknowledge every green row at once — alerts whose \
                                             damage was fully recovered from the VOD, or whose \
                                             failed take was superseded by a later completed \
                                             take. Red (unhealed) and yellow rows are left \
                                             untouched.",
                                        )
                                        .clicked()
                                    {
                                        act = Some(Act::AckGroup(fixed_ids.clone()));
                                        ui.close();
                                    }
                                    ui.separator();
                                }
                                for ((icon, label), ids) in &ack_groups {
                                    if ui
                                        .button(format!("{icon} {label} ({})", ids.len()))
                                        .on_hover_text(format!(
                                            "Acknowledge all {} unacknowledged '{label}' \
                                             alert(s) at once.",
                                            ids.len()
                                        ))
                                        .clicked()
                                    {
                                        act = Some(Act::AckGroup(ids.clone()));
                                        ui.close();
                                    }
                                }
                            })
                            .response
                            .on_hover_text(
                                "Acknowledge one whole category at once — e.g. every 'Disk \
                                 full' alert from one bad night, without touching the rest.",
                            );
                        });
                    });
                    ui.separator();

                    if self.warnings_rows.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| {
                            ui.weak("No capture warnings — the tools' logs are clean.")
                        });
                        return;
                    }
                    if visible.is_empty() {
                        ui.add_space(24.0);
                        ui.vertical_centered(|ui| ui.weak("No alerts match the filter."));
                        return;
                    }

                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for &i in &visible {
                            let r = &self.warnings_rows[i];
                            let error = r.severity == "error";
                            // Fully healed: every lost range was re-fetched
                            // from the VOD — the row flips green so recovered
                            // damage doesn't keep reading as an open wound.
                            let healed = r.ranges_total > 0 && r.recovered == r.ranges_total;
                            // Superseded: the take died, but a later take of
                            // the same broadcast completed — the failure
                            // healed itself at the stream level. (Takes with
                            // lost ranges keep normal recovery rendering.)
                            let superseded =
                                !healed && error && r.superseded && r.ranges_total == 0;
                            // Row tint: green when healed, red for errors,
                            // yellow for warnings — dimmed once acknowledged.
                            let (rgb, accent) = if healed || superseded {
                                ((25, 95, 45), egui::Color32::from_rgb(110, 200, 130))
                            } else if error {
                                ((120, 25, 25), egui::Color32::from_rgb(230, 100, 100))
                            } else {
                                ((120, 95, 10), egui::Color32::from_rgb(220, 175, 60))
                            };
                            let alpha = if r.acked { 25 } else { 70 };
                            let tint = egui::Color32::from_rgba_unmultiplied(rgb.0, rgb.1, rgb.2, alpha);
                            egui::Frame::group(ui.style()).fill(tint).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (icon, kind_label) = alert_kind_label(&r.kind);
                                    let icon = if healed {
                                        "✅"
                                    } else if superseded {
                                        "🔁"
                                    } else {
                                        icon
                                    };
                                    ui.label(egui::RichText::new(icon).color(accent))
                                        .on_hover_text(if superseded {
                                            "Superseded — this capture attempt died, but a later \
                                             take of the same broadcast completed. New takes \
                                             re-fetch the full stream head (deep rewind / VOD \
                                             backfill), so the completed take should cover this \
                                             one's content. This alert no longer counts toward \
                                             the 🚨 badge."
                                        } else if healed {
                                            "Recovered — every lost range was re-fetched from the \
                                             VOD; the content exists as patch files next to the \
                                             recording. Ranges that only survived as DMCA-muted \
                                             copies are fetched anyway (video intact, audio \
                                             silenced) — a muted patch beats no patch — and are \
                                             marked '-muted' in the filename."
                                        } else if error {
                                            "ERROR — content is missing from this capture."
                                        } else {
                                            "Warning — the tool complained, no data loss detected."
                                        });
                                    ui.vertical(|ui| {
                                        let mut title = if r.channel.is_empty() {
                                            kind_label.to_string()
                                        } else {
                                            format!("{kind_label} — {}", r.channel)
                                        };
                                        if healed {
                                            title.push_str(if r.recovered_muted > 0 {
                                                " — recovered (partly muted)"
                                            } else {
                                                " — recovered"
                                            });
                                        } else if superseded {
                                            title.push_str(" — superseded by a later take");
                                        }
                                        let mut rich = egui::RichText::new(title).strong();
                                        if !r.acked {
                                            rich = rich.color(accent);
                                        }
                                        ui.label(rich);
                                        ui.label(egui::RichText::new(alert_summary(r)).weak())
                                            .on_hover_text(format!(
                                                "Last matching log line:\n{}",
                                                r.last_line
                                            ));
                                        ui.horizontal(|ui| {
                                            let span = if r.last_at > r.first_at {
                                                format!(
                                                    "{} — {}",
                                                    fmt_datetime_short(r.first_at),
                                                    fmt_datetime_short(r.last_at)
                                                )
                                            } else {
                                                fmt_datetime_short(r.first_at)
                                            };
                                            ui.small(span).on_hover_text(
                                                "First and most recent occurrence.",
                                            );
                                            if !r.source.is_empty() {
                                                ui.small(format!("· {}", r.source))
                                                    .on_hover_text("The capture tool whose log reported this.");
                                            }
                                            let (cicon, clabel) = crate::downloader::alert_category(
                                                &r.kind,
                                                &r.last_line,
                                            );
                                            ui.small(format!("· {cicon} {clabel}"))
                                                .on_hover_text(
                                                    "Alert category — the ✔ Ack group menu \
                                                     acknowledges every alert of one category \
                                                     at once, and the filter box matches \
                                                     category names.",
                                                );
                                        });
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if !r.acked
                                                && ui
                                                    .button("✔ Ack")
                                                    .on_hover_text(
                                                        "Acknowledge: clears this alert from the \
                                                         header badge. It stays listed, and new \
                                                         occurrences will re-light it.",
                                                    )
                                                    .clicked()
                                            {
                                                act = Some(Act::Ack(r.id));
                                            }
                                            if ui
                                                .button("📂 Log")
                                                .on_hover_text(
                                                    "Open the capture tool's log file — every \
                                                     matched line, in context.",
                                                )
                                                .clicked()
                                            {
                                                act = Some(Act::OpenLog(r.take_key.clone()));
                                            }
                                            if r.recovered > 0
                                                && let Some(rec_id) = r.recording_id
                                                && ui
                                                    .button("🩹 Patches")
                                                    .on_hover_text(
                                                        "Open the folder with the recovered patch \
                                                         files ({stem}.recovered-….mkv) — the \
                                                         re-fetched content for each lost range.",
                                                    )
                                                    .clicked()
                                            {
                                                act = Some(Act::OpenPatches(rec_id));
                                            }
                                        },
                                    );
                                });
                            });
                        }
                    });
                });
            },
        );

        if !open {
            self.show_warnings = false;
        }
        match act {
            Some(Act::Ack(id)) => {
                let _ = self.core.store.ack_capture_alert(id);
                if let Some(r) = self.warnings_rows.iter_mut().find(|r| r.id == id) {
                    r.acked = true;
                }
                self.warn_badge = self.core.store.alert_badge_counts().unwrap_or((0, 0));
            }
            Some(Act::AckAll) => {
                let _ = self.core.store.ack_all_capture_alerts();
                for r in &mut self.warnings_rows {
                    r.acked = true;
                }
                self.warn_badge = (0, 0);
            }
            Some(Act::AckGroup(ids)) => {
                let _ = self.core.store.ack_capture_alerts(&ids);
                for r in &mut self.warnings_rows {
                    if ids.contains(&r.id) {
                        r.acked = true;
                    }
                }
                self.warn_badge = self.core.store.alert_badge_counts().unwrap_or((0, 0));
            }
            Some(Act::OpenLog(path)) => {
                crate::platform::open_path(std::path::Path::new(&path));
            }
            Some(Act::OpenPatches(rec_id)) => {
                // The patch files sit next to the recording; take the first
                // done range's out_path and open its folder.
                let dir = self
                    .core
                    .store
                    .gap_ranges_in_state(rec_id, "done")
                    .unwrap_or_default()
                    .iter()
                    .find(|g| !g.out_path.is_empty())
                    .and_then(|g| {
                        std::path::Path::new(&g.out_path).parent().map(std::path::Path::to_path_buf)
                    });
                if let Some(dir) = dir {
                    crate::platform::open_path(&dir);
                }
            }
            None => {}
        }
    }

    /// Issues panel: lists all recordings whose output path is still a `.ts`
    /// file inside a `.cache` directory, and lets the user re-remux them to MKV.
    /// See [`IssuesScan`] for the parts computed off-thread.
    #[allow(deprecated)]
    // The per-column `match ISSUES_COLUMNS[ci].id { "actions" => { if ... } }`
    // arms below are single-`if` bodies by nature of the column-dispatch
    // pattern; collapsing into a match-guard (clippy's suggestion) would mean
    // evaluating UI-drawing/click checks as match guards, which is far less
    // readable here than the small lint it avoids.
    #[allow(clippy::collapsible_match)]
    pub(super) fn issues_window(&mut self, ctx: &egui::Context) {
        use std::time::Duration;
        self.issues_drain_scan(ctx);
        self.issues_refresh_scan();
        if !self.show_issues {
            return;
        }

        // Build owned lookup: monitor_id -> (channel_name, platform) to avoid
        // holding a borrow on self.rows inside the viewport closure.
        let mon_info: std::collections::HashMap<i64, (String, crate::models::Platform)> = self
            .rows
            .iter()
            .map(|r| {
                (
                    r.monitor.id,
                    (r.channel.name.clone(), r.monitor.platform()),
                )
            })
            .collect();

        // Clone the platform textures handle so the closure doesn't borrow self.
        let ptex = self.platform_tex.clone();
        let now = crate::models::now_unix();
        let has_active_remux = self
            .background_tasks
            .iter()
            .any(|bt| bt.kind == crate::events::BackgroundTaskKind::Remux);

        // Sizes go through the TTL probe cache — this runs every frame while
        // the Issues window is open.
        let fs = &mut self.fs_probes;
        let n_empty = self.issues_recs.iter().filter(|r| {
            fs.len(std::path::Path::new(&r.output_path)) == 0
        }).count();
        let n_missing = self.issues_missing.len();
        let n_errors = self.issues_errors.len();
        let n_missing_errors = self.issues_errors_no_file.len();
        let n_stuck = self.issues_stuck.len();
        let confirm_clear = self.issues_confirm_clear;
        let quota_warnings = self.active_quota_warnings();

        let mut open = true;
        let mut act: Option<Act> = None;
        // Persisted column order/visibility, taken as a local copy (mutated by
        // the header's column-chooser context menu, written back + persisted
        // once after the viewport closure below). Shared by all 5 row-shape
        // blocks below (needs-remux / stuck / missing / errors-no-file /
        // errors) — all must use this SAME order to stay aligned with the
        // header and with each other.
        let mut issues_entries = self.issues_grid.entries.clone();
        let issues_order = grid_columns::effective_order(&ISSUES_COLUMNS, &issues_entries, |_| true);
        let issues_reset = self.issues_grid.note_order(&issues_order);

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("issues_vp"),
            egui::ViewportBuilder::default()
                .with_title("⚠ Issues")
                .with_inner_size([1000.0, 420.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                if has_active_remux {
                    ctx.request_repaint_after(Duration::from_secs(1));
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    // These lists grow unboundedly (e.g. a long backlog of
                    // unmerged split captures) — cap them in their own
                    // scroll region so they can never push the toolbar and
                    // main table off the bottom of the window with no way
                    // to reach them.
                    egui::ScrollArea::vertical()
                        .id_salt("issues_top_sections")
                        .max_height(300.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            self.issues_quota_section(ui, &quota_warnings, &mut act);
                            self.issues_stale_recording_section(ui, &mut act);
                            self.issues_muted_vod_section(ui, &mut act);
                            self.issues_unmerged_section(ui, has_active_remux, &mut act);
                            self.issues_head_mismatch_section(ui, &mut act);
                        });
                    self.issues_toolbar(
                        ui,
                        n_empty,
                        n_missing,
                        n_errors,
                        n_missing_errors,
                        n_stuck,
                        confirm_clear,
                        &mut act,
                    );
                    if self.issues_recs.is_empty()
                        && n_missing == 0
                        && n_errors == 0
                        && n_missing_errors == 0
                        && n_stuck == 0
                    {
                        if self.issues_unmerged.is_empty()
                            && self.issues_head_mismatch.is_empty()
                            && self.issues_stale_recording.is_empty()
                        {
                            ui.weak("No recording issues found — all recordings are in their final format.");
                        }
                        return;
                    }
                    self.issues_table(
                        ui,
                        &mon_info,
                        &ptex,
                        now,
                        &mut act,
                        &mut issues_entries,
                        &issues_order,
                        issues_reset,
                    );
                });
                self.issues_error_window(ctx);
            },
        );
        if issues_entries != self.issues_grid.entries {
            self.issues_grid.entries = issues_entries;
            grid_columns::save_columns(&self.core.store, GridTableId::Issues, &self.issues_grid.entries);
        }

        if !open {
            self.show_issues = false;
        }
        self.issues_apply_act(act);
    }

    /// Drain any completed background missing-file check so the badge count
    /// stays current even when the panel is hidden.
    fn issues_drain_scan(&mut self, ctx: &egui::Context) {
        use std::time::Duration;
        if let Some(rx) = &self.issues_missing_load {
            match rx.try_recv() {
                Ok(scan) => {
                    self.issues_missing = scan.missing;
                    self.issues_errors = scan.errors_with_file;
                    self.issues_errors_no_file = scan.errors_no_file;
                    self.issues_unmerged = scan.unmerged;
                    self.issues_head_mismatch = scan.head_mismatch;
                    self.issues_stale_recording = scan.stale_recording;
                    self.issues_missing_load = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.issues_missing_load = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still in flight — keep repainting so we pick it up promptly.
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
            }
        }
    }

    /// Refresh the Issues lists when stale. DB-only queries (fast, system
    /// drive) run synchronously; everything that stats the recordings drive
    /// runs off-thread (see [`IssuesScan`]).
    fn issues_refresh_scan(&mut self) {
        use std::time::{Duration, Instant};
        // Always refresh so the toolbar button count stays current even when the
        // panel is closed — but much less often then: the badge going stale for
        // a few minutes is fine, and each sweep stats every recording on the
        // recordings drive (real head seeks while captures are writing).
        let interval = if self.show_issues {
            Duration::from_secs(5)
        } else if self.issues_dirty {
            // Something changed recently — bring the badge up to date soon,
            // but never sweep-per-event (see pump_messages).
            Duration::from_secs(15)
        } else {
            Duration::from_secs(300)
        };
        let stale = self
            .issues_refreshed
            .map(|t| t.elapsed() >= interval)
            .unwrap_or(true);
        if stale && self.issues_missing_load.is_none() {
            self.issues_dirty = false;
            // DB-only queries (fast, system drive) stay synchronous.
            self.issues_recs = self.core.store.recordings_needing_remux().unwrap_or_default();
            self.issues_stuck = self.core.store.recordings_stuck_in_cache().unwrap_or_default();
            self.issues_muted_vod = self.core.store.recordings_muted_vod_unresolved().unwrap_or_default();
            // Everything that stats the recordings drive — the up-to-500-path
            // missing-file sweep AND the error partition — runs off-thread
            // (one exists() there can block the frame for seconds under load).
            let core = self.core.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("issues-missing-check".into())
                .spawn(move || {
                    let candidates = core.store.recordings_with_final_path().unwrap_or_default();
                    let gone: Vec<_> = candidates
                        .into_iter()
                        .filter(|r| !crate::iomon::fs::exists_sync(crate::iomon::Cat::FsProbe, &r.output_path))
                        .collect();
                    // A "gone" take whose media survived as split per-format
                    // parts in `.cache\` (tool died before its own merge) is
                    // NOT lost — list it as recoverable, never as missing.
                    let (unmerged, missing): (Vec<_>, Vec<_>) = gone
                        .into_iter()
                        .map(|r| {
                            let capture = std::path::Path::new(&r.output_path);
                            let mut parts = crate::downloader::find_split_media(capture);
                            if parts.is_empty() {
                                // The tool died mid-write: the media may
                                // survive only as unfinished `.part`
                                // sequences (largest one per format).
                                parts = crate::downloader::find_split_parts(capture);
                            }
                            (r, parts)
                        })
                        .partition(|(_, parts)| !parts.is_empty());
                    let missing: Vec<_> = missing.into_iter().map(|(r, _)| r).collect();
                    // Partition errors: file gone → treated as missing.
                    let all_errors = core.store.recordings_with_errors().unwrap_or_default();
                    let (with_file, no_file): (Vec<_>, Vec<_>) =
                        all_errors.into_iter().partition(|r| {
                            r.output_path.is_empty()
                                || crate::iomon::fs::exists_sync(crate::iomon::Cat::FsProbe, &r.output_path)
                        });
                    // A failed take whose media survives as split parts is
                    // recoverable — list it ONLY under unmerged (with the
                    // merge action), not as a plain dead "file missing" row.
                    let unmerged_ids: std::collections::HashSet<i64> =
                        unmerged.iter().map(|(r, _)| r.id).collect();
                    let no_file: Vec<_> = no_file
                        .into_iter()
                        .filter(|r| !unmerged_ids.contains(&r.id))
                        .collect();
                    // Rows still marked 'recording' whose files have gone
                    // quiet: capture died unnoticed. Takes whose finalize is
                    // already in flight (remux queued at the disk gate — the
                    // Streams grid shows them "finalizing") are excluded:
                    // offering "Finalize now" there would double-promote.
                    let now = crate::models::now_unix();
                    let finalizing_recs: std::collections::HashSet<i64> =
                        core.finalizing.lock().unwrap().values().copied().collect();
                    let stale_recording: Vec<_> = core
                        .store
                        .recordings_marked_recording()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|r| !finalizing_recs.contains(&r.id))
                        .filter_map(|r| {
                            // A capture in its first minutes may not have
                            // files yet — never list those.
                            if now - r.started_at < STALE_RECORDING_SECS {
                                return None;
                            }
                            if r.output_path.is_empty() {
                                return Some((r, None));
                            }
                            let age = crate::downloader::latest_capture_activity(&r.output_path)
                                .map(|t| (now - t).max(0));
                            match age {
                                Some(a) if a < STALE_RECORDING_SECS => None,
                                other => Some((r, other)),
                            }
                        })
                        .collect();
                    // Head/live joins blocked by codec parameters, with the
                    // actual stream params probed for the explainer.
                    let head_mismatch: Vec<_> = core
                        .store
                        .recordings_with_head_mismatch()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|r| {
                            let head = r
                                .backfill_path
                                .as_deref()
                                .map(probe_dims_sync)
                                .unwrap_or_default();
                            let live = probe_dims_sync(&r.output_path);
                            (r, head, live)
                        })
                        .collect();
                    let _ = tx.send(IssuesScan {
                        missing,
                        errors_with_file: with_file,
                        errors_no_file: no_file,
                        unmerged,
                        head_mismatch,
                        stale_recording,
                    });
                })
                .ok();
            self.issues_missing_load = Some(rx);
            self.issues_refreshed = Some(Instant::now());
        }
    }

    /// ── Quota warnings ── one row per active warning + dismiss button.
    fn issues_quota_section(
        &self,
        ui: &mut egui::Ui,
        quota_warnings: &[String],
        act: &mut Option<Act>,
    ) {
        for key in quota_warnings {
            let (msg, color) = match key.as_str() {
                "youtube_units_exceeded" => (
                    format!("YouTube Data API daily unit quota reached ({} / {} units). API calls are paused until tomorrow.", self.yt_quota_today, self.yt_quota_cutoff),
                    egui::Color32::from_rgb(200, 80, 80),
                ),
                "youtube_units_near_cutoff" => (
                    format!("YouTube Data API units near cutoff ({} / {} units today).", self.yt_quota_today, self.yt_quota_cutoff),
                    egui::Color32::from_rgb(200, 150, 60),
                ),
                "youtube_search_exceeded" => (
                    format!("YouTube search.list daily limit reached ({} / 100 queries). Search-based detection paused until tomorrow.", self.yt_search_today, ),
                    egui::Color32::from_rgb(200, 80, 80),
                ),
                "youtube_search_near_cutoff" => (
                    format!("YouTube search.list queries near limit ({} / 100 today, cutoff at {}).", self.yt_search_today, self.yt_search_cutoff),
                    egui::Color32::from_rgb(200, 150, 60),
                ),
                _ => continue,
            };
            ui.horizontal(|ui| {
                ui.colored_label(color, &msg);
                if ui.small_button("✕ Dismiss").clicked() {
                    *act = Some(Act::DismissWarning(key.clone()));
                }
            });
        }
        if !quota_warnings.is_empty() {
            ui.separator();
        }
    }

    /// ── DMCA-muted published VODs (live recording kept) ──
    fn issues_muted_vod_section(&mut self, ui: &mut egui::Ui, act: &mut Option<Act>) {
        if !self.issues_muted_vod.is_empty() {
            ui.label(
                egui::RichText::new("✂ DMCA-muted VODs (live recording kept)").strong(),
            );
            egui::Grid::new("issues_muted_vod_grid")
                .num_columns(6)
                .spacing([10.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    for (i, m) in self.issues_muted_vod.iter().enumerate() {
                        let mins = (m.muted_secs / 60).max(1);
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 120, 30),
                            format!("{} — ~{mins} min muted", m.channel),
                        );
                        let live_ok = !m.output_path.is_empty()
                            && self.fs_probes.is_file(std::path::Path::new(&m.output_path));
                        if ui
                            .add_enabled(live_ok, egui::Button::new("▶ Open live recording"))
                            .clicked()
                        {
                            *act = Some(Act::OpenMutedLive(i));
                        }
                        let rec = m.recovered_path.as_deref().unwrap_or("");
                        let rec_ok = !rec.is_empty()
                            && self.fs_probes.is_file(std::path::Path::new(rec));
                        if ui
                            .add_enabled(rec_ok, egui::Button::new("📼 Open recovered VOD"))
                            .clicked()
                        {
                            *act = Some(Act::OpenMutedRecovered(i));
                        }
                        if ui.button("♻ Re-run recovery").clicked() {
                            *act = Some(Act::RerunMuted(i));
                        }
                        if ui
                            .button("✓ Keep live / dismiss")
                            .on_hover_text("Acknowledge — the live recording has the full audio.")
                            .clicked()
                        {
                            *act = Some(Act::DismissMuted(i));
                        }
                        ui.weak(
                            m.recovery_state
                                .as_deref()
                                .map(|s| format!("recovery: {s}"))
                                .unwrap_or_default(),
                        );
                        ui.end_row();
                    }
                });
            ui.separator();
        }
    }

    /// ── Rows stuck in 'recording' with no live capture ──
    fn issues_stale_recording_section(&self, ui: &mut egui::Ui, act: &mut Option<Act>) {
        if self.issues_stale_recording.is_empty() {
            return;
        }
        ui.label(
            egui::RichText::new("⏸ Marked 'recording' but not being written").strong(),
        );
        ui.weak(
            "These takes claim to be recording, but their files have not been \
             written for a while. Either the capture process died without the \
             app noticing (power loss, sleep, forced kill), or the post-capture \
             finalize is still waiting for its turn at the disk gate (then it \
             shows a remux job here and under Background jobs). Finalize now \
             promotes whatever was captured and settles the row.",
        );
        let now = crate::models::now_unix();
        egui::Grid::new("issues_stale_recording_grid")
            .num_columns(3)
            .spacing([10.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for (i, (rec, age)) in self.issues_stale_recording.iter().enumerate() {
                    let name = std::path::Path::new(&rec.output_path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("recording {}", rec.id));
                    let age_s = match age {
                        Some(a) => format!("last write {} ago", fmt_duration(*a)),
                        None => "no capture file found on disk".to_string(),
                    };
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 160, 30),
                        format!("{name} — {age_s}"),
                    );
                    // An in-flight finalize/remux for this take (startup re-drive
                    // or a manual action) is a Remux background task keyed by the
                    // recording id.
                    let task = self.background_tasks.iter().find(|bt| {
                        bt.kind == crate::events::BackgroundTaskKind::Remux
                            && bt.id == rec.id as u64
                    });
                    ui.horizontal(|ui| {
                        if let Some(bt) = task {
                            let elapsed = (now - bt.started_at).max(0);
                            if let Some(p) = bt.progress {
                                ui.add(
                                    egui::ProgressBar::new(p)
                                        .show_percentage()
                                        .desired_width(110.0),
                                );
                            }
                            ui.colored_label(
                                egui::Color32::from_rgb(80, 160, 220),
                                format!("⏳ finalizing… {}", fmt_duration(elapsed)),
                            )
                            .on_hover_text(
                                "The finalize is queued/running — remuxes take turns on \
                                 the recordings drive, so a backlog can hold this for a \
                                 while. Progress shows once ffmpeg starts.",
                            );
                        } else if ui
                            .button("🛠 Finalize now")
                            .on_hover_text(
                                "Promote whatever was captured (remux/move it out of \
                                 the working folder) and settle this row.",
                            )
                            .clicked()
                        {
                            *act = Some(Act::FinalizeStale(i));
                        }
                    });
                    if ui.button("🔍")
                        .on_hover_text("View details in a window.")
                        .clicked()
                    {
                        let mut text = format!(
                            "Status: recording (stale)\n{age_s}\nStarted: {}\nPath: {}",
                            fmt_datetime_short(rec.started_at),
                            rec.output_path
                        );
                        if let Some(hint) = network_failure_hint(&rec.log_excerpt) {
                            text.push_str("\n\n");
                            text.push_str(hint);
                        }
                        if !rec.log_excerpt.is_empty() {
                            text.push_str("\n\n");
                            text.push_str(rec.log_excerpt.trim());
                        }
                        *act = Some(Act::ViewError(name.clone(), text));
                    }
                    ui.end_row();
                }
            });
        ui.separator();
    }

    /// ── Unmerged split captures (recoverable, NOT lost) ──
    fn issues_unmerged_section(
        &mut self,
        ui: &mut egui::Ui,
        has_active_remux: bool,
        act: &mut Option<Act>,
    ) {
        if !self.issues_unmerged.is_empty() {
            ui.label(
                egui::RichText::new("🧩 Unmerged split captures (recoverable)").strong(),
            );
            ui.weak(
                "The download tool died before merging its per-format files — the \
                 final file was never written (the take reads as 0 bytes / gone), \
                 but the video and audio survived as parts in `.cache\\`. Rows \
                 marked (interrupted) recovered from unfinished working files: the \
                 merged video is intact up to where the capture stopped, but its \
                 very tail may be cut, and the stream continued past that point — \
                 Download VOD gets the whole broadcast if it's still published. \
                 Merge is lossless and runs throttled like any finalize pass.",
            );
            let now = crate::models::now_unix();
            egui::Grid::new("issues_unmerged_grid")
                .num_columns(4)
                .spacing([10.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    for (i, (rec, parts)) in self.issues_unmerged.iter().enumerate() {
                        let name = std::path::Path::new(&rec.output_path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| rec.output_path.clone());
                        let total: u64 = parts
                            .iter()
                            .map(|p| self.fs_probes.len(p))
                            .sum();
                        let partial = parts
                            .iter()
                            .any(|p| p.to_string_lossy().ends_with(".part"));
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 30),
                            format!(
                                "{name} — {} part(s), {}{}",
                                parts.len(),
                                fmt_bytes(total as i64),
                                if partial { " (interrupted)" } else { "" },
                            ),
                        );
                        // This take's own merge (running or queued for the disk
                        // gate) — keyed by the recording id. Show its live state
                        // instead of the button.
                        let merge_task = self.background_tasks.iter().find(|bt| {
                            bt.kind == crate::events::BackgroundTaskKind::Remux
                                && bt.id == rec.id as u64
                        });
                        ui.horizontal(|ui| {
                            if let Some(bt) = merge_task {
                                let elapsed = (now - bt.started_at).max(0);
                                if let Some(p) = bt.progress {
                                    ui.add(
                                        egui::ProgressBar::new(p)
                                            .show_percentage()
                                            .desired_width(110.0),
                                    );
                                }
                                ui.colored_label(
                                    egui::Color32::from_rgb(80, 160, 220),
                                    bt.progress_info
                                        .clone()
                                        .unwrap_or_else(|| "⏳ merging…".into()),
                                )
                                .on_hover_text(format!(
                                    "Elapsed: {} — a queued merge shows what currently \
                                     holds the disk gate; speed/position appear once \
                                     its own ffmpeg starts.",
                                    fmt_duration(elapsed)
                                ));
                            } else if ui
                                .add_enabled(!has_active_remux, egui::Button::new("🧩 Merge into MKV"))
                                .on_hover_text(
                                    "Losslessly mux the parts into the final MKV, promote it, \
                                     and mark the recording completed. Parts are deleted only \
                                     on success.",
                                )
                                .on_disabled_hover_text(
                                    "Another remux/merge is running — this one starts after \
                                     it (see Background jobs for the live queue).",
                                )
                                .clicked()
                            {
                                *act = Some(Act::MergeSplit(i));
                            }
                        });
                        if ui
                            .button("📼 Download VOD")
                            .on_hover_text(
                                "Archive the published VOD instead / as well — the only \
                                 way to get the part of the stream after the capture \
                                 died.",
                            )
                            .clicked()
                        {
                            *act = Some(Act::DownloadVodUnmerged(i));
                        }
                        if ui.button("🔍")
                            .on_hover_text("View details in a window.")
                            .clicked()
                        {
                            let mut text = format!(
                                "Status: {}\nPath: {}\n\nSurviving parts:",
                                rec.status, rec.output_path
                            );
                            for p in parts {
                                text.push_str(&format!(
                                    "\n  {} ({})",
                                    p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                                    fmt_bytes(self.fs_probes.len(p) as i64),
                                ));
                            }
                            if let Some(hint) = network_failure_hint(&rec.log_excerpt) {
                                text.push_str("\n\n");
                                text.push_str(hint);
                            }
                            if !rec.log_excerpt.is_empty() {
                                text.push_str("\n\n");
                                text.push_str(rec.log_excerpt.trim());
                            }
                            *act = Some(Act::ViewError(name.clone(), text));
                        }
                        ui.end_row();
                    }
                });
            ui.separator();
        }
    }

    /// ── Head/live join mismatches ──
    fn issues_head_mismatch_section(&self, ui: &mut egui::Ui, act: &mut Option<Act>) {
        if !self.issues_head_mismatch.is_empty() {
            ui.label(
                egui::RichText::new("🔗 Head backfill can't join the live capture").strong(),
            );
            ui.weak(
                "The backfilled head and the live capture carry different stream \
                 parameters, so a lossless join is impossible. Usual cause: the \
                 capture joined seconds after go-live, before Twitch listed the \
                 source rendition — the take recorded a transcode while the head \
                 fetched at source. Both files are kept and playable; pick a fix:",
            );
            egui::Grid::new("issues_head_mismatch_grid")
                .num_columns(4)
                .spacing([10.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    for (i, (rec, head, live)) in self.issues_head_mismatch.iter().enumerate() {
                        let name = std::path::Path::new(&rec.output_path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| rec.output_path.clone());
                        let (head_d, live_d) = (
                            if head.is_empty() { "?" } else { head.as_str() },
                            if live.is_empty() { "?" } else { live.as_str() },
                        );
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 30),
                            format!("{name} — head {head_d} vs live {live_d}"),
                        );
                        if ui
                            .button("🧩 Re-fetch head @ live quality")
                            .on_hover_text(
                                "Fetch the head again at the live capture's own rendition \
                                 so the lossless join can succeed. Full quality is then \
                                 available via the VOD instead. (Post-stream: any \
                                 DMCA-muted section fetches muted.)",
                            )
                            .clicked()
                        {
                            *act = Some(Act::RefetchHeadMatchLive(i));
                        }
                        if ui
                            .button("📼 Download VOD (source quality)")
                            .on_hover_text(
                                "Grab the published VOD at source quality instead — the \
                                 full stream, including the head, at the better \
                                 resolution the live capture missed.",
                            )
                            .clicked()
                        {
                            *act = Some(Act::FetchVodForMismatch(i));
                        }
                        if ui
                            .button("✓ Keep parts / dismiss")
                            .on_hover_text(
                                "Acknowledge — keep the head and live capture as separate \
                                 playable files.",
                            )
                            .clicked()
                        {
                            *act = Some(Act::DismissMismatch(i));
                        }
                        ui.end_row();
                    }
                });
            ui.separator();
        }
    }

    /// Summary count + Refresh + the bulk delete/clear buttons.
    #[allow(clippy::too_many_arguments)]
    fn issues_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        n_empty: usize,
        n_missing: usize,
        n_errors: usize,
        n_missing_errors: usize,
        n_stuck: usize,
        confirm_clear: bool,
        act: &mut Option<Act>,
    ) {
        ui.horizontal(|ui| {
            ui.label(format!(
                "{} recording(s) need attention",
                self.issues_recs.len()
                    + n_missing
                    + n_errors
                    + n_stuck
                    + self.issues_muted_vod.len()
                    + self.issues_unmerged.len()
                    + self.issues_head_mismatch.len()
                    + self.issues_stale_recording.len()
            ));
            if ui.button("⟳ Refresh").clicked() {
                self.issues_refreshed = None;
            }
            ui.separator();
            if n_empty > 0 {
                if ui.button(format!("🗑 Delete {} empty", n_empty))
                    .on_hover_text("Delete all 0-byte captures — they contain no data.")
                    .clicked()
                {
                    *act = Some(Act::ClearEmpties);
                }
            }
            if !self.issues_recs.is_empty() {
                if confirm_clear {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 80, 80),
                        format!("Delete all {} capture files?", self.issues_recs.len()),
                    );
                    if ui.button("✓ Yes, delete all").clicked() {
                        *act = Some(Act::ClearAll);
                    }
                    if ui.button("✗ Cancel").clicked() {
                        *act = Some(Act::ConfirmClear);
                    }
                } else if ui.button("🗑 Delete all")
                    .on_hover_text("Delete all .ts capture files and remove them from the list.")
                    .clicked()
                {
                    *act = Some(Act::ConfirmClear);
                }
            }
            if n_missing > 0 {
                if ui.button(format!("🔗 Clear {} missing", n_missing))
                    .on_hover_text("Clear DB path for recordings whose output file was deleted from disk.")
                    .clicked()
                {
                    *act = Some(Act::ClearAllMissing);
                }
            }
            if n_missing_errors > 0 {
                if ui.button(format!("✕ Clear {} no-file failed", n_missing_errors))
                    .on_hover_text("Remove DB records for failed recordings whose output file no longer exists on disk.")
                    .clicked()
                {
                    *act = Some(Act::ClearFilelessErrors);
                }
            }
            if n_errors > 0 {
                if ui.button(format!("✕ Clear all {} failed", n_errors))
                    .on_hover_text("Delete DB records for all failed/aborted/orphaned recordings that still have a file. Files are deleted too.")
                    .clicked()
                {
                    *act = Some(Act::ClearAllErrors);
                }
            }
        });
        ui.separator();
    }

    /// The Issues grid: shared column header + the five row shapes
    /// (needs-remux / stuck-in-cache / file-missing / failed-no-file /
    /// failed), all drawn in the SAME column order so they stay aligned.
    #[allow(clippy::too_many_arguments)]
    fn issues_table(
        &mut self,
        ui: &mut egui::Ui,
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        now: i64,
        act: &mut Option<Act>,
        issues_entries: &mut [grid_columns::ColumnEntry],
        issues_order: &[usize],
        issues_reset: bool,
    ) {
        use egui_extras::{Column, TableBuilder};
        let mut tb = TableBuilder::new(ui)
            .id_salt(GridTableId::Issues.key())
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        if issues_reset {
            tb.reset();
        }
        for &i in issues_order {
            let c = &ISSUES_COLUMNS[i];
            let col = if c.stretch {
                Column::remainder().clip(true).at_least(c.min_width)
            } else {
                Column::auto().at_least(c.min_width)
            };
            tb = tb.column(col);
        }
        tb.header(20.0, |mut h| {
            for &i in issues_order {
                let c = &ISSUES_COLUMNS[i];
                h.col(|ui| {
                    if grid_header_cell_plain(ui, GridTableId::Issues, c, issues_entries, &ISSUES_COLUMNS) {
                        self.reorder_columns = Some(ReorderColumnsState {
                            table: GridTableId::Issues,
                            draft: issues_entries.to_vec(),
                        });
                    }
                });
            }
        })
            .body(|mut body| {
                self.issues_remux_rows(&mut body, issues_order, mon_info, ptex, now, act);
                self.issues_stuck_rows(&mut body, issues_order, mon_info, ptex, act);
                self.issues_missing_rows(&mut body, issues_order, mon_info, ptex, act);
                self.issues_fileless_error_rows(&mut body, issues_order, mon_info, ptex, act);
                self.issues_error_rows(&mut body, issues_order, mon_info, ptex, act);
            });
    }

    /// Rows for recordings whose output is still a `.ts` in the capture
    /// cache — re-remuxable to MKV.
    // The per-column `match ISSUES_COLUMNS[ci].id { "actions" => { if ... } }`
    // arms are single-`if` bodies by nature of the column-dispatch pattern
    // (see `issues_window`).
    #[allow(clippy::collapsible_match)]
    #[allow(clippy::too_many_arguments)]
    fn issues_remux_rows(
        &mut self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        now: i64,
        act: &mut Option<Act>,
    ) {
        for (i, rec) in self.issues_recs.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            let path = std::path::Path::new(&rec.output_path);
            let fname = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let file_bytes = self.fs_probes.len(path);
            let empty = file_bytes == 0;
            // Parse the recording mode from "(p <mode>  )" in the filename.
            let mode = parse_capture_mode(&fname).unwrap_or_default();
            let remux_task = self.background_tasks.iter().find(|bt| {
                bt.kind == crate::events::BackgroundTaskKind::Remux
                    && bt.id == rec.id as u64
            });
            let remuxing = remux_task.is_some();
            // Check finished_tasks for a prior failed remux attempt.
            let remux_err = self.finished_tasks.iter().find_map(|(t, outcome, _)| {
                if t.kind == crate::events::BackgroundTaskKind::Remux
                    && t.id == rec.id as u64
                {
                    if let crate::events::TaskOutcome::Failed(msg) = outcome {
                        return Some(msg.clone());
                    }
                }
                None
            });
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname)
                                .on_hover_text(&rec.output_path);
                        }
                        "size" => {
                            if empty {
                                ui.colored_label(
                                    egui::Color32::from_rgb(180, 60, 60),
                                    "empty",
                                );
                            } else {
                                ui.label(fmt_bytes(file_bytes as i64));
                            }
                        }
                        "type" => {
                            // "TS" is implicit for all rows; show the mode qualifier if present.
                            let type_str = if mode.is_empty() {
                                "TS".to_string()
                            } else {
                                format!("TS · {mode}")
                            };
                            ui.label(type_str)
                                .on_hover_text(format!("status: {}", rec.status));
                        }
                        "status" => {
                            if let Some(bt) = remux_task {
                                let elapsed = (now - bt.started_at).max(0);
                                let hover = bt.progress_info.as_deref()
                                    .map(|i| format!("{}\nElapsed: {}", i, fmt_duration(elapsed)))
                                    .unwrap_or_else(|| fmt_duration(elapsed));
                                if let Some(p) = bt.progress {
                                    ui.add(
                                        egui::ProgressBar::new(p)
                                            .show_percentage()
                                            .desired_width(110.0),
                                    )
                                    .on_hover_text(hover);
                                } else if let Some(ref info) = bt.progress_info {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 160, 220),
                                        info,
                                    )
                                    .on_hover_text(format!("Elapsed: {}", fmt_duration(elapsed)));
                                } else {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(80, 160, 220),
                                        format!("⏳ remuxing… {}", fmt_duration(elapsed)),
                                    );
                                }
                            } else if empty {
                                ui.colored_label(
                                    egui::Color32::from_rgb(180, 60, 60),
                                    "✗ empty — no data",
                                ).on_hover_text("Capture wrote 0 bytes. Delete this file.");
                            } else if let Some(ref err) = remux_err {
                                ui.colored_label(
                                    egui::Color32::from_rgb(180, 60, 60),
                                    "✗ remux failed",
                                ).on_hover_text(err.as_str());
                            } else {
                                let (icon, color) = state_icon(&rec.status);
                                ui.colored_label(color, icon)
                                    .on_hover_text(&rec.status);
                            }
                        }
                        "actions" => {
                            if !remuxing {
                                if empty {
                                    ui.add_enabled(false, egui::Button::new("🔄").small())
                                        .on_hover_text("Empty capture — nothing to remux.");
                                } else if remux_err.is_some() {
                                    ui.add_enabled(false, egui::Button::new("🔄").small())
                                        .on_hover_text("Remux failed — see status cell.");
                                } else if ui
                                    .button("🔄")
                                    .on_hover_text("Re-remux: convert .ts → .mkv via ffmpeg.")
                                    .clicked()
                                {
                                    *act = Some(Act::Remux(i));
                                }
                                if ui.button("🗑")
                                    .on_hover_text(
                                        if empty {
                                            "Delete this empty capture file."
                                        } else {
                                            "Delete the .ts capture file and remove from list."
                                        }
                                    )
                                    .clicked()
                                {
                                    *act = Some(Act::Delete(i));
                                }
                                if ui.button("🔍")
                                    .on_hover_text("View error details in a window.")
                                    .clicked()
                                {
                                    let details = if empty {
                                        "Capture wrote 0 bytes. Delete this file.".to_string()
                                    } else if let Some(ref err) = remux_err {
                                        err.clone()
                                    } else {
                                        rec.status.clone()
                                    };
                                    *act = Some(Act::ViewError(fname.clone(), details));
                                }
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// Stuck-in-cache rows: capture succeeded but the promote-to-output-dir
    /// move never completed (non-.ts, so distinct from the re-remux rows) —
    /// most commonly a filename-length overflow. "Recover" retries the move
    /// with a shortened name if that's what's blocking it.
    #[allow(clippy::collapsible_match)]
    fn issues_stuck_rows(
        &mut self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        act: &mut Option<Act>,
    ) {
        for (k, rec) in self.issues_stuck.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            let path = std::path::Path::new(&rec.output_path);
            let fname = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let file_bytes = self.fs_probes.len(path);
            let mode = parse_capture_mode(&fname).unwrap_or_default();
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname).on_hover_text(&rec.output_path);
                        }
                        "size" => { ui.label(fmt_bytes(file_bytes as i64)); }
                        "type" => {
                            let ext = path
                                .extension()
                                .map(|e| e.to_string_lossy().to_uppercase())
                                .unwrap_or_else(|| "?".into());
                            let type_str = if mode.is_empty() {
                                ext
                            } else {
                                format!("{ext} · {mode}")
                            };
                            ui.label(type_str).on_hover_text(format!("status: {}", rec.status));
                        }
                        "status" => {
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 150, 60),
                                "⚠ stuck in cache",
                            ).on_hover_text(STUCK_IN_CACHE_DETAILS);
                        }
                        "actions" => {
                            if ui
                                .button("📦")
                                .on_hover_text("Recover: move it to its output folder, shortening the name if needed.")
                                .clicked()
                            {
                                *act = Some(Act::RecoverStuck(k));
                            }
                            if ui.button("🔍")
                                .on_hover_text("View error details in a window.")
                                .clicked()
                            {
                                *act = Some(Act::ViewError(
                                    fname.clone(),
                                    format!(
                                        "{STUCK_IN_CACHE_DETAILS}\nPath: {}",
                                        rec.output_path
                                    ),
                                ));
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// Missing-output-file rows (completed/failed/ended but file gone from disk).
    #[allow(clippy::collapsible_match)]
    fn issues_missing_rows(
        &self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        act: &mut Option<Act>,
    ) {
        for (j, rec) in self.issues_missing.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            let path = std::path::Path::new(&rec.output_path);
            let fname = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_uppercase())
                .unwrap_or_else(|| "?".into());
            let details = format!(
                "Output file was deleted from disk.\nDB status: {}\nPath: {}",
                rec.status, rec.output_path
            );
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname).on_hover_text(&rec.output_path);
                        }
                        "size" => {
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 130, 30),
                                "gone",
                            );
                        }
                        "type" => { ui.label(ext.as_str()); }
                        "status" => {
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 130, 30),
                                "✗ file missing",
                            ).on_hover_text(&details);
                        }
                        "actions" => {
                            if ui.button("🔗 Clear path")
                                .on_hover_text("Remove the stale path from the database record.")
                                .clicked()
                            {
                                *act = Some(Act::ClearPath(j));
                            }
                            if ui.button("🔍")
                                .on_hover_text("View error details in a window.")
                                .clicked()
                            {
                                *act = Some(Act::ViewError(fname.clone(), details.clone()));
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// ── Failed but file gone (treated as missing) ──
    #[allow(clippy::collapsible_match)]
    fn issues_fileless_error_rows(
        &self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        act: &mut Option<Act>,
    ) {
        for (j2, rec) in self.issues_errors_no_file.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            let path = std::path::Path::new(&rec.output_path);
            let fname = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_uppercase())
                .unwrap_or_else(|| "?".to_string());
            let details = {
                let mut parts = vec![
                    format!("status: {}", rec.status),
                    format!("path: {}", rec.output_path),
                ];
                if let Some(hint) = network_failure_hint(&rec.log_excerpt) {
                    parts.push(format!("\n{hint}"));
                }
                if !rec.log_excerpt.is_empty() {
                    parts.push(rec.log_excerpt.trim().to_string());
                }
                parts.join("\n")
            };
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname).on_hover_text(&rec.output_path);
                        }
                        "size" => {
                            ui.colored_label(egui::Color32::from_rgb(200, 130, 30), "gone");
                        }
                        "type" => { ui.label(ext.as_str()); }
                        "status" => {
                            let exit_str = rec.exit_code
                                .map(|c| format!(" (exit {c})"))
                                .unwrap_or_default();
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 80, 80),
                                format!("✗ {}{} — file missing", rec.status, exit_str),
                            ).on_hover_text(&details);
                        }
                        "actions" => {
                            if ui.button("✕ Clear")
                                .on_hover_text("Permanently remove this failed recording from the database.")
                                .clicked()
                            {
                                *act = Some(Act::ClearMissingError(j2));
                            }
                            if ui.button("🔍")
                                .on_hover_text("View error details in a window.")
                                .clicked()
                            {
                                *act = Some(Act::ViewError(fname.clone(), details.clone()));
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// ── Failed / aborted / orphaned rows ──
    #[allow(clippy::collapsible_match)]
    fn issues_error_rows(
        &mut self,
        body: &mut egui_extras::TableBody<'_>,
        issues_order: &[usize],
        mon_info: &std::collections::HashMap<i64, (String, crate::models::Platform)>,
        ptex: &Option<PlatformTextures>,
        act: &mut Option<Act>,
    ) {
        for (k, rec) in self.issues_errors.iter().enumerate() {
            let (ch_name, platform) = mon_info
                .get(&rec.monitor_id)
                .map(|(n, p)| (n.as_str(), *p))
                .unwrap_or(("?", crate::models::Platform::Generic));
            let has_file = !rec.output_path.is_empty()
                && self.fs_probes.is_file(std::path::Path::new(&rec.output_path));
            let has_ts = rec.output_path.ends_with(".ts");
            let path = std::path::Path::new(&rec.output_path);
            let fname = if rec.output_path.is_empty() {
                "—".to_string()
            } else {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| rec.output_path.clone())
            };
            let file_size = if has_file {
                self.fs_probes.len(path)
            } else {
                0
            };
            let exit_str = match rec.exit_code {
                Some(c) => format!("exit {c}"),
                None => String::new(),
            };
            // Build a hover text from whatever info we have.
            let hover = {
                let mut parts = vec![format!("status: {}", rec.status)];
                if !exit_str.is_empty() { parts.push(exit_str.clone()); }
                if !rec.output_path.is_empty() { parts.push(format!("path: {}", rec.output_path)); }
                if let Some(hint) = network_failure_hint(&rec.log_excerpt) {
                    parts.push(format!("\n{hint}"));
                }
                if !rec.log_excerpt.is_empty() { parts.push(format!("\n{}", rec.log_excerpt.trim())); }
                parts.join("\n")
            };
            body.row(22.0, |mut row| {
                for &ci in issues_order {
                    row.col(|ui| match ISSUES_COLUMNS[ci].id {
                        "platform" => {
                            if let Some(ptex) = ptex {
                                platform_icon(ui, ptex, platform);
                            } else {
                                ui.label(platform.label());
                            }
                        }
                        "channel" => { ui.label(ch_name); }
                        "started" => { ui.label(fmt_datetime_short(rec.started_at)); }
                        "file" => {
                            ui.label(&fname).on_hover_text(&rec.output_path);
                        }
                        "size" => {
                            if has_file && file_size > 0 {
                                ui.label(fmt_bytes(file_size as i64));
                            } else if has_file {
                                ui.colored_label(egui::Color32::from_rgb(180, 60, 60), "empty");
                            } else {
                                ui.weak("—");
                            }
                        }
                        "type" => {
                            let ext = if rec.output_path.is_empty() {
                                "—".to_string()
                            } else {
                                path.extension()
                                    .map(|e| e.to_string_lossy().to_uppercase())
                                    .unwrap_or_else(|| "?".to_string())
                            };
                            ui.label(ext);
                        }
                        "status" => {
                            let color = egui::Color32::from_rgb(200, 80, 80);
                            let label = if exit_str.is_empty() {
                                format!("✗ {}", rec.status)
                            } else {
                                format!("✗ {} ({})", rec.status, exit_str)
                            };
                            ui.colored_label(color, label)
                                .on_hover_text(&hover);
                        }
                        "actions" => {
                            // Remux if there's a .ts file on disk.
                            if has_file && has_ts {
                                if ui.button("🔄")
                                    .on_hover_text("Attempt to remux this partial .ts to MKV.")
                                    .clicked()
                                {
                                    *act = Some(Act::RemuxError(k));
                                }
                            }
                            // Delete file + clear path.
                            if has_file {
                                if ui.button("🗑")
                                    .on_hover_text("Delete the output file and clear it from the database.")
                                    .clicked()
                                {
                                    *act = Some(Act::DeleteError(k));
                                }
                            }
                            // Remove DB record entirely.
                            if ui.button("✕ Clear")
                                .on_hover_text("Permanently remove this failed recording from the database.")
                                .clicked()
                            {
                                *act = Some(Act::ClearError(k));
                            }
                            if ui.button("🔍")
                                .on_hover_text("View error details in a window.")
                                .clicked()
                            {
                                *act = Some(Act::ViewError(fname.clone(), hover.clone()));
                            }
                        }
                        _ => {}
                    });
                }
            });
        }
    }

    /// Apply the single action collected during this frame's render, after
    /// the viewport closure has released its borrows of `self`.
    fn issues_apply_act(&mut self, act: Option<Act>) {
        if let Some(Act::Remux(i)) = act {
            if let Some(rec) = self.issues_recs.get(i) {
                // The promoted location = the capture path minus its cache
                // component (handles per-dir AND central-root layouts).
                let dest = crate::downloader::strip_cache_component(std::path::Path::new(
                    &rec.output_path,
                ))
                .map(|p| p.with_extension("mkv"));
                if let Some(dest) = dest {
                    self.core.manual(crate::events::ManualCommand::ReRemux {
                        rec_id: rec.id,
                        capture: std::path::PathBuf::from(&rec.output_path),
                        final_: dest,
                    });
                    self.status = format!("Re-remux started for recording {}…", rec.id);
                }
            }
        }
        if let Some(Act::RecoverStuck(k)) = act
            && let Some(rec) = self.issues_stuck.get(k)
        {
            let capture = std::path::PathBuf::from(&rec.output_path);
            // The promoted location = the capture path minus its cache
            // component (handles per-dir AND central-root layouts); its parent
            // is the output dir the file should move to.
            let output_dir = crate::downloader::strip_cache_component(&capture)
                .and_then(|p| p.parent().map(Path::to_path_buf));
            if let Some(output_dir) = output_dir {
                self.core.manual(crate::events::ManualCommand::RecoverStuckCapture {
                    rec_id: rec.id,
                    capture,
                    output_dir,
                });
                self.status = format!("Recovering recording {}…", rec.id);
            }
        }
        if let Some(Act::Delete(i)) = act {
            if let Some(rec) = self.issues_recs.get(i).cloned() {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.clear_recording_capture(rec.id);
                self.issues_recs.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearEmpties) = act {
            let empties: Vec<_> = self.issues_recs.iter().filter(|r| {
                crate::iomon::fs::metadata_sync(crate::iomon::Cat::RecordingDelete, &r.output_path).map(|m| m.len()).unwrap_or(0) == 0
            }).cloned().collect();
            for rec in empties {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.clear_recording_capture(rec.id);
                self.issues_recs.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearPath(j)) = act {
            if let Some(rec) = self.issues_missing.get(j).cloned() {
                let _ = self.core.store.clear_recording_capture(rec.id);
                self.issues_missing.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearAllMissing) = act {
            let all: Vec<_> = self.issues_missing.drain(..).collect();
            for rec in all {
                let _ = self.core.store.clear_recording_capture(rec.id);
            }
        }
        if let Some(Act::ConfirmClear) = act {
            self.issues_confirm_clear = !self.issues_confirm_clear;
        }
        if let Some(Act::ClearAll) = act {
            let all: Vec<_> = self.issues_recs.drain(..).collect();
            for rec in all {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.clear_recording_capture(rec.id);
            }
            self.issues_confirm_clear = false;
        }
        if let Some(Act::DismissWarning(ref key)) = act {
            self.dismissed_quota_warnings.insert(key.clone());
        }
        if let Some(Act::OpenMutedLive(i)) = act
            && let Some(p) = self
                .issues_muted_vod
                .get(i)
                .map(|m| m.output_path.clone())
                .filter(|p| !p.is_empty())
        {
            open_path(std::path::Path::new(&p));
        }
        if let Some(Act::OpenMutedRecovered(i)) = act
            && let Some(rp) = self
                .issues_muted_vod
                .get(i)
                .and_then(|m| m.recovered_path.clone())
                .filter(|p| !p.is_empty())
        {
            open_path(std::path::Path::new(&rp));
        }
        if let Some(Act::RerunMuted(i)) = act
            && let Some(rec_id) = self.issues_muted_vod.get(i).map(|m| m.rec_id)
        {
            self.open_recover_vod_from_seed(rec_id);
        }
        if let Some(Act::DismissMuted(i)) = act
            && let Some(rec_id) = self.issues_muted_vod.get(i).map(|m| m.rec_id)
        {
            let _ = self.core.store.recording_vod_dl_acknowledge(rec_id);
            self.issues_refreshed = None; // force the list to refresh
        }
        if let Some(Act::MergeSplit(i)) = act
            && let Some((rec, _)) = self.issues_unmerged.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::MergeSplitCapture(rec.id));
            self.status = format!("Merging split capture for recording {}…", rec.id);
        }
        if let Some(Act::DownloadVodUnmerged(i)) = act
            && let Some((rec, _)) = self.issues_unmerged.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::ArchiveVodNow(rec.id));
            self.status = format!("Downloading the published VOD for recording {}…", rec.id);
        }
        if let Some(Act::FinalizeStale(i)) = act
            && let Some((rec, _)) = self.issues_stale_recording.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::FinalizeRecording(rec.id));
            self.status = format!("Finalizing recording {}…", rec.id);
            self.issues_refreshed = None;
        }
        if let Some(Act::RefetchHeadMatchLive(i)) = act
            && let Some((rec, _, _)) = self.issues_head_mismatch.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::BackfillHeadMatchLive(rec.id));
            self.status = format!("Re-fetching head at the live quality for recording {}…", rec.id);
            self.issues_refreshed = None;
        }
        if let Some(Act::FetchVodForMismatch(i)) = act
            && let Some((rec, _, _)) = self.issues_head_mismatch.get(i)
        {
            self.core
                .manual(crate::events::ManualCommand::ArchiveVodNow(rec.id));
            self.status = format!("Downloading the published VOD for recording {}…", rec.id);
        }
        if let Some(Act::DismissMismatch(i)) = act
            && let Some((rec, _, _)) = self.issues_head_mismatch.get(i)
        {
            // "mismatch_ack": still skips join re-attempts (any "mismatch*"
            // state does) but no longer lists in Issues.
            let _ = self.core.store.set_head_backfill_state(rec.id, "mismatch_ack");
            self.issues_refreshed = None;
        }
        if let Some(Act::RemuxError(k)) = act {
            if let Some(rec) = self.issues_errors.get(k) {
                let dest = std::path::Path::new(&rec.output_path)
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|d| {
                        std::path::Path::new(&rec.output_path)
                            .file_stem()
                            .map(|s| d.join(format!("{}.mkv", s.to_string_lossy())))
                    });
                if let Some(dest) = dest {
                    self.core.manual(crate::events::ManualCommand::ReRemux {
                        rec_id: rec.id,
                        capture: std::path::PathBuf::from(&rec.output_path),
                        final_: dest,
                    });
                    self.status = format!("Re-remux started for recording {}…", rec.id);
                }
            }
        }
        if let Some(Act::DeleteError(k)) = act {
            if let Some(rec) = self.issues_errors.get(k).cloned() {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.clear_recording_capture(rec.id);
                self.issues_errors.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearError(k)) = act {
            if let Some(rec) = self.issues_errors.get(k).cloned() {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.delete_recording(rec.id);
                self.issues_errors.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ClearAllErrors) = act {
            let all: Vec<_> = self.issues_errors.drain(..).collect();
            for rec in all {
                let path = std::path::Path::new(&rec.output_path);
                if crate::iomon::fs::exists_sync(crate::iomon::Cat::RecordingDelete, path) {
                    let _ = crate::iomon::fs::remove_file_sync(crate::iomon::Cat::RecordingDelete, path);
                }
                let _ = self.core.store.delete_recording(rec.id);
            }
        }
        if let Some(Act::ClearFilelessErrors) = act {
            // issues_errors_no_file holds all failed recordings where the file is gone.
            let all: Vec<_> = self.issues_errors_no_file.drain(..).collect();
            for rec in all {
                let _ = self.core.store.delete_recording(rec.id);
            }
        }
        if let Some(Act::ClearMissingError(j2)) = act {
            if let Some(rec) = self.issues_errors_no_file.get(j2).cloned() {
                let _ = self.core.store.delete_recording(rec.id);
                self.issues_errors_no_file.retain(|r| r.id != rec.id);
            }
        }
        if let Some(Act::ViewError(title, text)) = act {
            self.issues_error_view = Some((title, text));
        }
    }

    /// The error-details window (🔍 row button): the status-column hover text
    /// in a selectable, copyable form. One window, re-targeted by each click.
    fn issues_error_window(&mut self, ctx: &egui::Context) {
        let Some((title, text)) = self.issues_error_view.clone() else {
            return;
        };
        let mut open = true;
        egui::Window::new(if title.is_empty() || title == "—" {
            "Details".to_string()
        } else {
            format!("Details — {title}")
        })
        .id(egui::Id::new("issues_error_view"))
        .open(&mut open)
        .collapsible(false)
        .default_size([640.0, 260.0])
        .show(ctx, |ui| {
            if ui.button("📋 Copy").clicked() {
                ui.ctx().copy_text(text.clone());
            }
            ui.separator();
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Read-only but selectable.
                    let mut s = text.as_str();
                    ui.add(
                        egui::TextEdit::multiline(&mut s)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                });
        });
        if !open {
            self.issues_error_view = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_hint_matches_dns_failures() {
        // The exact shape of the Maid Mint / Anya failed-resume logs (2026-07-12).
        assert!(network_failure_hint(
            "WARNING: [youtube:tab] HTTPSConnection(host='www.youtube.com', port=443): \
             Failed to resolve 'www.youtube.com' ([Errno 11001] getaddrinfo failed). \
             Retrying (1/3)..."
        )
        .is_some());
        assert!(network_failure_hint("Temporary failure in name resolution").is_some());
        // Real tool errors must not be blamed on the network.
        assert!(network_failure_hint("ERROR: This live event has ended.").is_none());
        assert!(network_failure_hint("").is_none());
    }
}
