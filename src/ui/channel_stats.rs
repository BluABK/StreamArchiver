//! Channel Stats view: per-channel viewer/follower history graphs, stream
//! events (subs / bits / raids), the all-channels overview table, and the
//! 📈 viewer-stats popup (also used for per-stream graphs). Data comes from
//! the `viewer_history` / `stream_event` tables (schema v59) sampled by the
//! scheduler + `meta_watcher`, chat parser, and EventSub raid handler.

use super::*;
use crate::models::{ChannelStatsRow, StreamEventRow, ViewerBucket};

/// Cached query results for the Channel Stats view — reloaded whenever the
/// channel/span selection changes (or on tab re-open / ⟳).
pub(super) struct ChStatsData {
    /// All-channels mode: per-channel viewer aggregates.
    overview: Vec<ChannelStatsRow>,
    /// All-channels mode: per-channel event totals
    /// (`[subs, gifted, bits, raids in, raids out]`).
    event_totals: std::collections::HashMap<i64, [i64; 5]>,
    /// Single-channel mode: viewer/follower buckets.
    viewer: Vec<ViewerBucket>,
    /// Single-channel mode: discrete events, newest first.
    events: Vec<StreamEventRow>,
    /// Single-channel mode: title/category/collab ledger markers
    /// (`at, kind, new_value`).
    changes: Vec<(i64, String, String)>,
}

/// One loaded graph window's worth of data: viewer buckets, discrete events,
/// and title/category/collab ledger markers.
type StatsRange = (Vec<ViewerBucket>, Vec<StreamEventRow>, Vec<(i64, String, String)>);

/// The 📈 popup window: the same viewer graph as the view, for one channel —
/// either span-driven (channel context menu) or clamped to one broadcast's
/// time range (stream-row context menu). Single-instance, like the 🤝
/// collab-history window.
pub(super) struct ViewerStatsPopup {
    channel_id: i64,
    title: String,
    /// `Some((since, until))` = fixed range (per-stream graph, no span
    /// selector); `None` = the popup's own span selector applies.
    range: Option<(i64, i64)>,
    span: super::PollSpan,
    /// Lazily-loaded graph data; `None` = (re)query.
    data: Option<StatsRange>,
}

/// Marker color per event kind (legend + points on the graphs).
fn event_color(kind: &str) -> egui::Color32 {
    match kind {
        "sub" | "resub" => egui::Color32::from_rgb(0x4c, 0xaf, 0x50),
        "subgift" => egui::Color32::from_rgb(0x00, 0xbc, 0xd4),
        "bits" => egui::Color32::from_rgb(0xff, 0x98, 0x00),
        "raid_in" => egui::Color32::from_rgb(0xab, 0x47, 0xbc),
        "raid_out" => egui::Color32::from_rgb(0x7e, 0x57, 0xc2),
        _ => egui::Color32::GRAY,
    }
}

/// Short human label per event kind.
fn event_label(kind: &str) -> &'static str {
    match kind {
        "sub" => "Sub",
        "resub" => "Resub",
        "subgift" => "Gift subs",
        "bits" => "Bits",
        "raid_in" => "Raid in",
        "raid_out" => "Raid out",
        _ => "Event",
    }
}

/// One readable line for the events table below the graphs.
fn event_line(e: &StreamEventRow) -> String {
    match e.kind.as_str() {
        "sub" => format!("{} subscribed (tier {})", e.actor, tier_label(&e.tier)),
        "resub" => format!(
            "{} resubscribed — {} months (tier {})",
            e.actor,
            e.amount.max(1),
            tier_label(&e.tier)
        ),
        "subgift" if e.target.is_empty() => {
            format!("{} gifted {} sub(s) to the community", e.actor, e.amount.max(1))
        }
        "subgift" => format!("{} gifted a sub to {}", e.actor, e.target),
        "bits" => format!("{} cheered {} bits", e.actor, e.amount),
        "raid_in" => format!("{} raided in with {} viewers", e.actor, e.amount),
        "raid_out" => format!("raided out to {} with {} viewers", e.target, e.amount),
        other => format!("{other} by {}", e.actor),
    }
}

fn tier_label(tier: &str) -> &str {
    match tier {
        "1000" => "1",
        "2000" => "2",
        "3000" => "3",
        "Prime" => "Prime",
        "" => "?",
        other => other,
    }
}

impl StreamArchiverApp {
    /// Load the Channel Stats data for the current (channel, span) selection.
    fn load_chstats(&mut self) {
        let now = now_unix();
        let since = now - self.chstats_span.secs();
        let store = &self.core.store;
        let data = match self.chstats_channel {
            None => ChStatsData {
                overview: store.channel_stats_overview(since).unwrap_or_default(),
                event_totals: store.stream_event_totals(since).unwrap_or_default(),
                viewer: Vec::new(),
                events: Vec::new(),
                changes: Vec::new(),
            },
            Some(cid) => ChStatsData {
                overview: Vec::new(),
                event_totals: Default::default(),
                viewer: store
                    .viewer_history_range(cid, since, i64::MAX, self.chstats_span.bucket_secs())
                    .unwrap_or_default(),
                events: store.stream_events_range(cid, since, i64::MAX).unwrap_or_default(),
                changes: store.monitor_changes_range(cid, since, i64::MAX).unwrap_or_default(),
            },
        };
        self.chstats_data = Some(data);
    }

    /// `monitor_id -> platform-tag label` for one channel's plot lines,
    /// disambiguated with the instance id when a platform repeats.
    fn monitor_labels(&self, channel_id: i64) -> std::collections::HashMap<i64, String> {
        let mons: Vec<&crate::models::MonitorWithChannel> =
            self.rows.iter().filter(|r| r.channel.id == channel_id).collect();
        let mut counts: std::collections::HashMap<&'static str, usize> = Default::default();
        for m in &mons {
            *counts.entry(m.monitor.platform().label()).or_default() += 1;
        }
        mons.iter()
            .map(|m| {
                let plat = m.monitor.platform().label();
                let label = if counts.get(plat).copied().unwrap_or(0) > 1 {
                    format!("{plat} #{}", m.monitor.id)
                } else {
                    plat.to_string()
                };
                (m.monitor.id, label)
            })
            .collect()
    }

    pub(super) fn channel_stats_view(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Channel stats");
                if ui
                    .button("⟳ Refresh")
                    .on_hover_text("Re-run the stats queries for the current selection")
                    .clicked()
                {
                    self.chstats_data = None;
                    self.stats_collabs =
                        self.core.store.collab_partner_overview().unwrap_or_default();
                }
            });
            ui.separator();

            // ── Channel + span selection ──
            let mut channels: Vec<(i64, String)> =
                self.channels.iter().map(|c| (c.id, c.name.clone())).collect();
            channels.sort_by_key(|(_, n)| n.to_lowercase());
            let selected_name = self
                .chstats_channel
                .and_then(|id| channels.iter().find(|(cid, _)| *cid == id))
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| "— All channels —".into());
            ui.horizontal(|ui| {
                ui.label("Channel:").on_hover_text(
                    "Pick a channel for its viewer/follower graphs and event list, \
                     or All channels for the comparison table.",
                );
                let mut changed = false;
                egui::ComboBox::from_id_salt("chstats_channel")
                    .selected_text(selected_name)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(self.chstats_channel.is_none(), "— All channels —")
                            .clicked()
                        {
                            self.chstats_channel = None;
                            changed = true;
                        }
                        for (cid, name) in &channels {
                            if ui
                                .selectable_label(self.chstats_channel == Some(*cid), name)
                                .clicked()
                            {
                                self.chstats_channel = Some(*cid);
                                changed = true;
                            }
                        }
                    });
                ui.separator();
                ui.label("Span:").on_hover_text(
                    "How far back to look. Viewer history is kept forever (optionally \
                     compressed to 10-minute buckets after a configurable age — \
                     Settings → Maintenance → Channel stats history).",
                );
                for s in super::PollSpan::ALL {
                    let resp = ui
                        .selectable_label(self.chstats_span == s, s.label())
                        .on_hover_text(format!(
                            "Show the last {} in {} buckets",
                            s.label(),
                            s.bucket_label()
                        ));
                    if resp.clicked() && self.chstats_span != s {
                        self.chstats_span = s;
                        changed = true;
                    }
                }
                if changed {
                    self.chstats_data = None;
                }
            });
            ui.add_space(6.0);

            if self.chstats_data.is_none() {
                self.load_chstats();
            }

            match self.chstats_channel {
                None => self.chstats_overview_section(ui),
                Some(cid) => self.chstats_channel_section(ui, cid),
            }
        });
    }

    /// All-channels comparison table + the 🤝 collab partner aggregate.
    fn chstats_overview_section(&mut self, ui: &mut egui::Ui) {
        let Some(data) = &self.chstats_data else { return };
        ui.heading("Viewers").on_hover_text(
            "Per-channel viewer aggregates within the selected span, sampled \
             roughly once a minute while live (poll + in-recording refresh). \
             Click a channel to open its graphs.",
        );
        ui.separator();
        if data.viewer.is_empty() && data.overview.is_empty() {
            ui.weak(
                "No viewer history in this span yet — samples accumulate whenever \
                 a monitored channel is live (viewer counts come from Twitch and \
                 Kick; YouTube provides none while recording).",
            );
        } else {
            let mut clicked: Option<i64> = None;
            egui::Grid::new("chstats_overview_grid")
                .num_columns(7)
                .striped(true)
                .spacing([24.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("Channel").on_hover_text("Click a name to open its graphs");
                    ui.strong("Peak").on_hover_text("Highest sampled viewer count in the span");
                    ui.strong("Avg").on_hover_text(
                        "Average viewers while live (weighted by sampled airtime)",
                    );
                    ui.strong("Airtime").on_hover_text("Sampled live time in the span");
                    ui.strong("Followers").on_hover_text(
                        "Latest known follower total (Kick only — Twitch/YouTube \
                         don't expose totals without owner credentials)",
                    );
                    ui.strong("Subs").on_hover_text(
                        "Subs + resubs seen in chat / gifted subs (chat is only \
                         watched while recording with Chat log on)",
                    );
                    ui.strong("Bits / Raids").on_hover_text(
                        "Bits cheered (chat) and raids in/out (chat + EventSub)",
                    );
                    ui.end_row();
                    for row in &data.overview {
                        if ui.link(&row.name).on_hover_text("Open this channel's graphs").clicked()
                        {
                            clicked = Some(row.channel_id);
                        }
                        ui.label(grid::fmt_viewers(row.peak_viewers));
                        ui.label(grid::fmt_viewers(row.avg_viewers.round() as i64));
                        ui.label(fmt_duration_hm(row.live_secs));
                        ui.label(
                            row.followers.map(grid::fmt_viewers).unwrap_or_default(),
                        );
                        let ev = data.event_totals.get(&row.channel_id);
                        let [subs, gifted, bits, rin, rout] =
                            ev.copied().unwrap_or_default();
                        ui.label(if gifted > 0 {
                            format!("{subs} (+{gifted} gifted)")
                        } else {
                            subs.to_string()
                        });
                        ui.label(format!("{bits} / {rin} in, {rout} out"));
                        ui.end_row();
                    }
                });
            if let Some(cid) = clicked {
                self.chstats_channel = Some(cid);
                self.chstats_data = None;
            }
        }
        ui.add_space(16.0);

        // ── 🤝 Collabs (moved here from the App Stats view) ──
        ui.heading("🤝 Collabs").on_hover_text(
            "Everyone your monitored channels have streamed together with \
             (Twitch \"Stream Together\" shared chats, plus @mention-in-title \
             collabs), aggregated across all recorded sessions. Right-click a \
             channel row → 🤝 Collab history for its per-channel list.",
        );
        ui.separator();
        if self.stats_collabs.is_empty() {
            ui.weak(
                "No collabs recorded yet — sessions accumulate whenever a live \
                 Twitch channel is seen streaming together with someone.",
            );
        } else {
            ui.label(format!(
                "{} collab partner(s) seen. @name = only ever seen as a title \
                 mention (unconfirmed).",
                self.stats_collabs.len()
            ));
            ui.add_space(4.0);
            egui::Grid::new("collab_stats_grid")
                .num_columns(3)
                .striped(true)
                .spacing([32.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("Partner")
                        .on_hover_text("Collaborator (most recent display name)");
                    ui.strong("Sessions")
                        .on_hover_text("How many recorded collab sessions include them");
                    ui.strong("Last seen")
                        .on_hover_text("When a session with them was last observed");
                    ui.end_row();
                    for (name, sessions, last_seen) in self.stats_collabs.iter().take(100) {
                        ui.label(name);
                        ui.label(sessions.to_string());
                        ui.label(fmt_datetime_short(*last_seen));
                        ui.end_row();
                    }
                });
            if self.stats_collabs.len() > 100 {
                ui.weak(format!(
                    "(+{} more, by session count)",
                    self.stats_collabs.len() - 100
                ))
                .on_hover_text(
                    "Only the 100 most frequent partners are listed here; the \
                     per-channel 🤝 Collab history windows show everything.",
                );
            }
        }
        ui.add_space(8.0);
    }

    /// Single-channel graphs + event list.
    fn chstats_channel_section(&mut self, ui: &mut egui::Ui, channel_id: i64) {
        let labels = self.monitor_labels(channel_id);
        let Some(data) = &self.chstats_data else { return };
        if data.viewer.is_empty() && data.events.is_empty() {
            ui.weak(
                "Nothing sampled for this channel in the selected span yet. Viewer \
                 samples accumulate while the channel is live; sub/bits events \
                 need a recording with Chat log enabled; raids also arrive via \
                 EventSub (conduit mode).",
            );
            return;
        }
        let span = self.chstats_span;
        viewer_graph_ui(
            ui,
            "chstats",
            &data.viewer,
            &data.events,
            &data.changes,
            &labels,
            span.bucket_label(),
        );
        ui.add_space(10.0);
        events_table_ui(ui, "chstats_events", &data.events, 200);
    }

    /// Open the 📈 popup for a whole channel (span-driven).
    pub(super) fn open_viewer_stats(&mut self, channel_id: i64) {
        let name = self
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("channel {channel_id}"));
        self.viewer_stats_popup = Some(ViewerStatsPopup {
            channel_id,
            title: name,
            range: None,
            span: super::PollSpan::Day,
            data: None,
        });
    }

    /// Open the 📈 popup clamped to one broadcast (`since..until`; pass
    /// `until = 0` for "still live / unknown end" — clamps to now).
    pub(super) fn open_stream_stats(
        &mut self,
        channel_id: i64,
        label: &str,
        since: i64,
        until: i64,
    ) {
        // Pad the window a little so the raid-out after the end and the pre-
        // stream baseline are visible.
        let until = if until > 0 { until } else { now_unix() };
        self.viewer_stats_popup = Some(ViewerStatsPopup {
            channel_id,
            title: label.to_string(),
            range: Some((since - 900, until + 900)),
            span: super::PollSpan::Day,
            data: None,
        });
    }

    /// Render the 📈 popup viewport (registered at the end of `ui()`).
    pub(super) fn viewer_stats_window(&mut self, ctx: &egui::Context) {
        let Some(popup) = &mut self.viewer_stats_popup else { return };
        // (Re)load lazily for the current span/range.
        if popup.data.is_none() {
            let (since, until, bucket) = match popup.range {
                Some((s, u)) => {
                    // Aim for ~200 points across the fixed range.
                    let b = ((u - s) / 200).max(60);
                    (s, u, b)
                }
                None => (now_unix() - popup.span.secs(), i64::MAX, popup.span.bucket_secs()),
            };
            let store = &self.core.store;
            popup.data = Some((
                store
                    .viewer_history_range(popup.channel_id, since, until, bucket)
                    .unwrap_or_default(),
                store.stream_events_range(popup.channel_id, since, until).unwrap_or_default(),
                store.monitor_changes_range(popup.channel_id, since, until).unwrap_or_default(),
            ));
        }
        let popup_channel = popup.channel_id;
        let labels = self.monitor_labels(popup_channel);
        let popup = self.viewer_stats_popup.as_mut().unwrap();
        let mut open = true;
        let mut new_span: Option<super::PollSpan> = None;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("viewer_stats_vp"),
            egui::ViewportBuilder::default()
                .with_title(format!("{} — viewer stats", popup.title))
                .with_inner_size([720.0, 480.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                // Same pattern as every other viewport window here (a
                // viewport hands us a Context, not a Ui).
                #[allow(deprecated)]
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        if popup.range.is_none() {
                            ui.horizontal(|ui| {
                                ui.label("Span:");
                                for s in super::PollSpan::ALL {
                                    if ui
                                        .selectable_label(popup.span == s, s.label())
                                        .on_hover_text(format!(
                                            "Show the last {} in {} buckets",
                                            s.label(),
                                            s.bucket_label()
                                        ))
                                        .clicked()
                                        && popup.span != s
                                    {
                                        new_span = Some(s);
                                    }
                                }
                            });
                            ui.add_space(4.0);
                        }
                        let Some((viewer, events, changes)) = &popup.data else { return };
                        if viewer.is_empty() && events.is_empty() {
                            ui.weak("No samples in this range.");
                            return;
                        }
                        let bucket_label = match popup.range {
                            Some(_) => "auto",
                            None => popup.span.bucket_label(),
                        };
                        viewer_graph_ui(
                            ui,
                            "viewer_stats_popup",
                            viewer,
                            events,
                            changes,
                            &labels,
                            bucket_label,
                        );
                        ui.add_space(8.0);
                        events_table_ui(ui, "viewer_stats_popup_events", events, 100);
                    });
                });
            },
        );
        if let Some(s) = new_span {
            popup.span = s;
            popup.data = None;
        }
        if !open {
            self.viewer_stats_popup = None;
        }
    }
}

/// `3h 24m`-style duration for airtime cells.
fn fmt_duration_hm(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 { format!("{h}h {m:02}m") } else { format!("{m}m") }
}

/// The shared viewer/follower graph: one viewer line per monitor, event
/// markers along the baseline, category/collab changes as vertical lines,
/// plus a separate follower plot when any follower data exists.
fn viewer_graph_ui(
    ui: &mut egui::Ui,
    id_prefix: &str,
    viewer: &[ViewerBucket],
    events: &[StreamEventRow],
    changes: &[(i64, String, String)],
    labels: &std::collections::HashMap<i64, String>,
    bucket_label: &str,
) {
    let now = now_unix();
    let to_x = |t: i64| (t - now) as f64 / 3600.0; // hours relative to now
    let span_hours = viewer
        .first()
        .map(|b| (now - b.t) as f64 / 3600.0)
        .unwrap_or(1.0)
        .max(1.0);
    let days = span_hours > 48.0;
    let fmt_x = move |h: f64| {
        if days { format!("{:+.1}d", h / 24.0) } else { format!("{h:+.1}h") }
    };

    ui.label("Viewers:").on_hover_text(format!(
        "Peak live viewers per {bucket_label} bucket, one line per platform \
         instance. Diamonds along the baseline are events (subs, bits, \
         raids); dotted vertical lines are category changes; 🤝 lines are \
         collab set changes. X axis is time relative to now. Gaps mean the \
         channel was offline (no samples).",
    ));
    // monitor -> time-ordered points; split a line where a gap exceeds ~3
    // buckets so offline time doesn't render as a misleading bridge.
    let mut per_monitor: std::collections::BTreeMap<i64, Vec<(i64, i64)>> = Default::default();
    for b in viewer {
        per_monitor.entry(b.monitor_id).or_default().push((b.t, b.viewers));
    }
    let fx = fmt_x;
    egui_plot::Plot::new(format!("{id_prefix}_viewer_plot"))
        .height(220.0)
        .legend(egui_plot::Legend::default())
        .allow_scroll(false)
        .include_y(0.0)
        .x_axis_formatter(move |mark, _| fx(mark.value))
        .y_axis_formatter(|mark, _| grid::fmt_viewers(mark.value.max(0.0) as i64))
        .label_formatter(move |name, v| {
            format!("{name}\n{}: {}", fx(v.x), grid::fmt_viewers(v.y.max(0.0) as i64))
        })
        .show(ui, |plot_ui| {
            // Category / collab change markers first (under the data).
            for (t, kind, _new) in changes {
                let (color, style) = match kind.as_str() {
                    "category" => (egui::Color32::from_gray(120), egui_plot::LineStyle::dotted_dense()),
                    "collab" => (egui::Color32::from_rgb(0x42, 0xa5, 0xf5), egui_plot::LineStyle::dashed_loose()),
                    _ => continue, // title changes are too chatty to mark
                };
                plot_ui.vline(
                    egui_plot::VLine::new(String::new(), to_x(*t)).color(color).style(style),
                );
            }
            for (mid, pts) in &per_monitor {
                let name = labels.get(mid).cloned().unwrap_or_else(|| format!("monitor {mid}"));
                // Split at gaps > 3 buckets (offline time).
                let gap = viewer_gap_secs(pts);
                let mut seg: Vec<[f64; 2]> = Vec::new();
                let mut segments: Vec<Vec<[f64; 2]>> = Vec::new();
                let mut prev_t: Option<i64> = None;
                for (t, v) in pts {
                    if let Some(p) = prev_t
                        && t - p > gap
                        && !seg.is_empty()
                    {
                        segments.push(std::mem::take(&mut seg));
                    }
                    seg.push([to_x(*t), *v as f64]);
                    prev_t = Some(*t);
                }
                if !seg.is_empty() {
                    segments.push(seg);
                }
                for (i, s) in segments.into_iter().enumerate() {
                    // Same name on every segment folds them into one legend
                    // entry; only the first carries it to avoid duplicates.
                    let n = if i == 0 { name.clone() } else { String::new() };
                    plot_ui.line(egui_plot::Line::new(n, egui_plot::PlotPoints::from(s)));
                }
            }
            // Event markers along the baseline, grouped by kind for the legend.
            let mut by_kind: std::collections::BTreeMap<&str, Vec<[f64; 2]>> = Default::default();
            for e in events {
                by_kind.entry(e.kind.as_str()).or_default().push([to_x(e.at), 0.0]);
            }
            for (kind, pts) in by_kind {
                plot_ui.points(
                    egui_plot::Points::new(event_label(kind), egui_plot::PlotPoints::from(pts))
                        .shape(egui_plot::MarkerShape::Diamond)
                        .radius(4.0)
                        .color(event_color(kind)),
                );
            }
        });

    // Follower plot only when there's any follower data (Kick).
    let has_followers = viewer.iter().any(|b| b.followers.is_some());
    if has_followers {
        ui.add_space(8.0);
        ui.label("Followers:").on_hover_text(
            "Platform-reported follower total over time (Kick only — Twitch and \
             YouTube don't expose totals without owner credentials).",
        );
        egui_plot::Plot::new(format!("{id_prefix}_follower_plot"))
            .height(120.0)
            .legend(egui_plot::Legend::default())
            .allow_scroll(false)
            .x_axis_formatter(move |mark, _| fx(mark.value))
            .y_axis_formatter(|mark, _| grid::fmt_viewers(mark.value.max(0.0) as i64))
            .label_formatter(move |name, v| {
                format!("{name}\n{}: {}", fx(v.x), grid::fmt_viewers(v.y.max(0.0) as i64))
            })
            .show(ui, |plot_ui| {
                let mut per_monitor: std::collections::BTreeMap<i64, Vec<[f64; 2]>> =
                    Default::default();
                for b in viewer {
                    if let Some(f) = b.followers {
                        per_monitor.entry(b.monitor_id).or_default().push([to_x(b.t), f as f64]);
                    }
                }
                for (mid, pts) in per_monitor {
                    let name =
                        labels.get(&mid).cloned().unwrap_or_else(|| format!("monitor {mid}"));
                    plot_ui.line(egui_plot::Line::new(name, egui_plot::PlotPoints::from(pts)));
                }
            });
    }
}

/// Line-split threshold: 3× the median sample spacing, floor 10 minutes —
/// tolerates downsampled (10-min) history without shredding raw (1-min) lines
/// at every missed poll.
fn viewer_gap_secs(pts: &[(i64, i64)]) -> i64 {
    let mut deltas: Vec<i64> =
        pts.windows(2).map(|w| w[1].0 - w[0].0).filter(|d| *d > 0).collect();
    if deltas.is_empty() {
        return 600;
    }
    deltas.sort_unstable();
    (deltas[deltas.len() / 2] * 3).max(600)
}

/// The recent-events table shown under the graphs.
fn events_table_ui(ui: &mut egui::Ui, id: &str, events: &[StreamEventRow], limit: usize) {
    if events.is_empty() {
        return;
    }
    ui.label(format!("Events ({}):", events.len())).on_hover_text(
        "Subs, resubs, gift subs and bits come from the recorded chat (so only \
         while a recording with Chat log was running); raids come from chat \
         and/or EventSub. Newest first.",
    );
    egui::Grid::new(id).num_columns(3).striped(true).spacing([16.0, 2.0]).show(ui, |ui| {
        for e in events.iter().take(limit) {
            ui.label(fmt_datetime_short(e.at));
            ui.colored_label(event_color(&e.kind), event_label(&e.kind));
            ui.label(event_line(e));
            ui.end_row();
        }
    });
    if events.len() > limit {
        ui.weak(format!("(+{} more in this span)", events.len() - limit));
    }
}
