//! Channel Stats view: per-channel viewer/follower history graphs, stream
//! events (subs / bits / raids), the all-channels overview table, and the
//! 📈 viewer-stats popup (also used for per-stream graphs). Data comes from
//! the `viewer_history` / `stream_event` tables (schema v59) sampled by the
//! scheduler + `meta_watcher`, chat parser, and EventSub raid handler.

use super::*;
use crate::models::{ChannelStatsRow, StreamEventRow, StreamStatRow, ViewerBucket};

/// Cached query results for the Channel Stats view — reloaded whenever the
/// channel/span selection changes (or on tab re-open / ⟳).
pub(super) struct ChStatsData {
    /// All-channels mode: per-channel viewer aggregates.
    overview: Vec<ChannelStatsRow>,
    /// All-channels mode: per-channel event totals
    /// (`[subs, gifted, bits, raids in, raids out, mod actions]`).
    event_totals: std::collections::HashMap<i64, [i64; 6]>,
    /// Single-channel mode: viewer/follower buckets.
    viewer: Vec<ViewerBucket>,
    /// Single-channel mode: discrete events, newest first.
    events: Vec<StreamEventRow>,
    /// Single-channel mode: title/category/collab ledger markers
    /// (`at, kind, new_value`).
    changes: Vec<(i64, String, String)>,
    /// Single-channel mode: per-broadcast breakdown rows, newest first.
    streams: Vec<StreamStatRow>,
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
    /// Lazily-loaded graph data; `None` = (re)query. `pub(super)` so the 🚂
    /// mark dialog can invalidate it after inserting an event.
    pub(super) data: Option<StatsRange>,
    /// Events-list filter text (window-local).
    filter: String,
}

/// Marker color per event kind (legend + points on the graphs).
fn event_color(kind: &str) -> egui::Color32 {
    match kind {
        "sub" | "resub" => egui::Color32::from_rgb(0x4c, 0xaf, 0x50),
        "subgift" => egui::Color32::from_rgb(0x00, 0xbc, 0xd4),
        "bits" => egui::Color32::from_rgb(0xff, 0x98, 0x00),
        "raid_in" => egui::Color32::from_rgb(0xab, 0x47, 0xbc),
        "raid_out" => egui::Color32::from_rgb(0x7e, 0x57, 0xc2),
        "msg_deleted" => egui::Color32::from_rgb(0xef, 0x53, 0x50),
        "timeout" => egui::Color32::from_rgb(0xff, 0xa7, 0x26),
        "ban" => egui::Color32::from_rgb(0xc6, 0x28, 0x28),
        "chat_clear" => egui::Color32::from_rgb(0x8d, 0x6e, 0x63),
        "chat_mode" => egui::Color32::from_rgb(0x78, 0x90, 0x9c),
        "role_change" => egui::Color32::from_rgb(0xff, 0xd5, 0x4f),
        "hype_train" => egui::Color32::from_rgb(0xff, 0x40, 0x81),
        "dono" => egui::Color32::from_rgb(0x66, 0xbb, 0x6a),
        "first_chat" => egui::Color32::from_rgb(0x90, 0xa4, 0xae),
        "milestone" => egui::Color32::from_rgb(0x26, 0xc6, 0xda),
        "announcement" => egui::Color32::from_rgb(0x5c, 0x9d, 0xff),
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
        "msg_deleted" => "Deleted msg",
        "timeout" => "Timeout",
        "ban" => "Ban",
        "chat_clear" => "Chat clear",
        "chat_mode" => "Chat mode",
        "role_change" => "Role change",
        "hype_train" => "Hype train",
        "dono" => "Hype Chat",
        "first_chat" => "First chat",
        "milestone" => "Milestone",
        "announcement" => "Announcement",
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
        "msg_deleted" if e.detail.is_empty() => format!("{}'s message was deleted", e.actor),
        "msg_deleted" => format!("{}'s message was deleted: \u{201c}{}\u{201d}", e.actor, e.detail),
        "timeout" => format!("{} was timed out ({})", e.actor, fmt_timeout(e.amount)),
        "ban" => format!("{} was banned", e.actor),
        "chat_clear" => "chat was cleared".to_string(),
        "chat_mode" => e.detail.clone(),
        "role_change" => format!("{} {}", e.actor, e.detail),
        // The detail names its own source: "(confirmed)" = live GQL state
        // (level, points, conductors), "(inferred)" = chat-burst proxy,
        // "marked manually" = user-recorded via the 🚂 dialog.
        "hype_train" => e.detail.clone(),
        // Hype Chat: a paid pinned message (real on-platform money).
        "dono" => format!("{} sent a {} Hype Chat", e.actor, e.detail),
        "first_chat" if e.detail.is_empty() => format!("{} chatted for the first time", e.actor),
        "first_chat" => {
            format!("{} chatted for the first time: \u{201c}{}\u{201d}", e.actor, e.detail)
        }
        "milestone" => format!("{} hit a {}", e.actor, e.detail),
        "announcement" => format!("📣 {}: {}", e.actor, e.detail),
        other => format!("{other} by {}", e.actor),
    }
}

/// `600` → `10m`, `86400` → `24h` — timeout durations for event lines.
fn fmt_timeout(secs: i64) -> String {
    if secs >= 3600 && secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
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
        self.chstats_loaded_at = now;
        let since = now - self.chstats_span.secs();
        let store = &self.core.store;
        let data = match self.chstats_channel {
            None => ChStatsData {
                overview: store.channel_stats_overview(since).unwrap_or_default(),
                event_totals: store.stream_event_totals(since).unwrap_or_default(),
                viewer: Vec::new(),
                events: Vec::new(),
                changes: Vec::new(),
                streams: Vec::new(),
            },
            Some(cid) => ChStatsData {
                overview: Vec::new(),
                event_totals: Default::default(),
                viewer: store
                    .viewer_history_range(cid, since, i64::MAX, self.chstats_span.bucket_secs())
                    .unwrap_or_default(),
                events: store.stream_events_range(cid, since, i64::MAX).unwrap_or_default(),
                changes: store.monitor_changes_range(cid, since, i64::MAX).unwrap_or_default(),
                streams: store.stream_stats_breakdown(cid, since).unwrap_or_default(),
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
                let mut auto = self.chstats_auto;
                if ui
                    .checkbox(&mut auto, "Auto refresh")
                    .on_hover_text(
                        "Re-run the queries once a minute while this tab is open \
                         (new viewer samples land at that cadence). Off = the view \
                         is a snapshot until you hit ⟳.",
                    )
                    .changed()
                {
                    self.chstats_auto = auto;
                    let _ = self
                        .core
                        .store
                        .set_setting("chstats_auto_refresh", if auto { "1" } else { "0" });
                }
                if ui
                    .button("🚂 Mark hype train")
                    .on_hover_text(
                        "Record a hype train the automatic capture missed: you give \
                         the start time (or how many minutes ago it kicked off) and \
                         the stored contributions right before it teach the \
                         inference what to catch next time.",
                    )
                    .clicked()
                {
                    self.hype_mark_channel = self.chstats_channel.unwrap_or(0);
                    self.hype_mark_abs.clear();
                    self.show_hype_mark = true;
                }
                if let Some(cid) = self.chstats_channel
                    && ui
                        .button("⚙ Sensitivity")
                        .on_hover_text(
                            "Per-channel hype-train inference override: raise or \
                             lower this channel's burst thresholds without touching \
                             the global tuning (Settings → Maintenance → Hype \
                             trains).",
                        )
                        .clicked()
                {
                    self.hype_override_draft = crate::hype::load_overrides(&self.core.store)
                        .get(&cid)
                        .copied()
                        .unwrap_or_default();
                    self.hype_override_for = Some(cid);
                }
            });
            // Auto refresh: invalidate the cache once a minute and keep a
            // repaint scheduled so the tick fires without mouse input.
            if self.chstats_auto {
                let age = now_unix() - self.chstats_loaded_at;
                if age >= 60 && self.chstats_data.is_some() {
                    self.chstats_data = None;
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_secs(
                    (60 - age).clamp(1, 60) as u64,
                ));
            }
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
                 a monitored channel is live (Twitch and Kick report exact counts; \
                 YouTube's are scraped from the watch page).",
            );
        } else {
            let mut clicked: Option<i64> = None;
            egui::Grid::new("chstats_overview_grid")
                .num_columns(8)
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
                    ui.strong("Mod acts").on_hover_text(
                        "Message deletions + timeouts + bans seen in chat while \
                         recording (chat-mode and role changes aren't counted here — \
                         they show as events on the channel's graphs)",
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
                        let [subs, gifted, bits, rin, rout, mod_acts] =
                            ev.copied().unwrap_or_default();
                        ui.label(if gifted > 0 {
                            format!("{subs} (+{gifted} gifted)")
                        } else {
                            subs.to_string()
                        });
                        ui.label(format!("{bits} / {rin} in, {rout} out"));
                        ui.label(mod_acts.to_string());
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
            let mut drill_down: Option<String> = None;
            egui::Grid::new("collab_stats_grid")
                .num_columns(3)
                .striped(true)
                .spacing([32.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("Partner")
                        .on_hover_text("Collaborator (most recent display name)");
                    ui.strong("Sessions")
                        .on_hover_text(
                            "How many recorded collab sessions include them — click to see \
                             which streams.",
                        );
                    ui.strong("Last seen")
                        .on_hover_text("When a session with them was last observed");
                    ui.end_row();
                    for (name, sessions, last_seen) in self.stats_collabs.iter().take(100) {
                        ui.label(name);
                        if ui
                            .add(
                                egui::Label::new(sessions.to_string())
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text("Click to see which streams this collab occurred in.")
                            .clicked()
                        {
                            drill_down = Some(name.clone());
                        }
                        ui.label(fmt_datetime_short(*last_seen));
                        ui.end_row();
                    }
                });
            if let Some(name) = drill_down {
                self.open_partner_sessions(&name);
            }
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
            span.bucket_secs(),
        );

        // ── Top supporters (Twitch-style gifter/cheerer leaderboards, but
        // over OUR archive and the selected span instead of Twitch's weekly
        // reset) ──
        let gifters = top_contributors(&data.events, "subgift", 10);
        let cheerers = top_contributors(&data.events, "bits", 10);
        if !gifters.is_empty() || !cheerers.is_empty() {
            ui.add_space(10.0);
            ui.horizontal_top(|ui| {
                if !gifters.is_empty() {
                    ui.vertical(|ui| {
                        ui.label("🎁 Top gifters:").on_hover_text(
                            "Most subs gifted within the selected span, from the \
                             recorded chat (so only streams recorded with Chat log \
                             count). Community batches count their full size.",
                        );
                        egui::Grid::new("chstats_top_gifters")
                            .num_columns(3)
                            .striped(true)
                            .spacing([12.0, 2.0])
                            .show(ui, |ui| {
                                for (i, (name, total)) in gifters.iter().enumerate() {
                                    ui.label(rank_label(i));
                                    ui.label(name);
                                    ui.label(format!("🎁 {total}"));
                                    ui.end_row();
                                }
                            });
                    });
                    ui.add_space(24.0);
                }
                if !cheerers.is_empty() {
                    ui.vertical(|ui| {
                        ui.label("💎 Top cheerers:").on_hover_text(
                            "Most bits cheered within the selected span, from the \
                             recorded chat (so only streams recorded with Chat log \
                             count).",
                        );
                        egui::Grid::new("chstats_top_cheerers")
                            .num_columns(3)
                            .striped(true)
                            .spacing([12.0, 2.0])
                            .show(ui, |ui| {
                                for (i, (name, total)) in cheerers.iter().enumerate() {
                                    ui.label(rank_label(i));
                                    ui.label(name);
                                    ui.label(format!("💎 {total}"));
                                    ui.end_row();
                                }
                            });
                    });
                }
            });
        }

        // ── Raid history ──
        let raids: Vec<&StreamEventRow> = data
            .events
            .iter()
            .filter(|e| e.kind == "raid_in" || e.kind == "raid_out")
            .collect();
        if !raids.is_empty() {
            ui.add_space(10.0);
            egui::CollapsingHeader::new(format!("⚔ Raid history ({})", raids.len()))
                .default_open(true)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Incoming raids come from chat (while recording) and \
                             EventSub; outgoing raids are EventSub-only (conduit \
                             mode). Newest first.",
                        )
                        .small()
                        .weak(),
                    );
                    egui::Grid::new("chstats_raids")
                        .num_columns(3)
                        .striped(true)
                        .spacing([16.0, 2.0])
                        .show(ui, |ui| {
                            for e in &raids {
                                ui.label(fmt_datetime_short(e.at));
                                if e.kind == "raid_in" {
                                    ui.colored_label(
                                        event_color("raid_in"),
                                        format!("→ in from {}", e.actor),
                                    );
                                } else {
                                    ui.colored_label(
                                        event_color("raid_out"),
                                        format!("← out to {}", e.target),
                                    );
                                }
                                ui.label(format!("{} viewers", e.amount));
                                ui.end_row();
                            }
                        });
                });
        }

        // ── Per-broadcast breakdown ──
        let mut open_clip: Option<(i64, i64)> = None;
        if !data.streams.is_empty() {
            ui.add_space(10.0);
            ui.label(format!("Streams ({}):", data.streams.len())).on_hover_text(
                "Per-broadcast breakdown of the span: every broadcast whose viewer \
                 samples carried the platform's stream id (scrape-detected \
                 broadcasts without an id can't be attributed and aren't listed). \
                 Newest first. 📈 opens the graph clipped to that broadcast.",
            );
            egui::Grid::new("chstats_streams")
                .num_columns(9)
                .striped(true)
                .spacing([16.0, 2.0])
                .show(ui, |ui| {
                    ui.strong("Started");
                    ui.strong("Airtime").on_hover_text("Sampled live time");
                    ui.strong("Peak");
                    ui.strong("Avg").on_hover_text("Airtime-weighted average viewers");
                    ui.strong("Subs").on_hover_text("Subs + resubs (gifted in parens)");
                    ui.strong("Bits");
                    ui.strong("Raids");
                    ui.strong("Mod").on_hover_text("Deletions + timeouts + bans");
                    ui.strong("");
                    ui.end_row();
                    for s in &data.streams {
                        ui.label(fmt_datetime_short(s.started));
                        ui.label(fmt_duration_hm(s.live_secs));
                        ui.label(grid::fmt_viewers(s.peak_viewers));
                        ui.label(grid::fmt_viewers(s.avg_viewers.round() as i64));
                        let [subs, gifted, bits, rin, rout, mods] = s.totals;
                        ui.label(if gifted > 0 {
                            format!("{subs} (+{gifted} gifted)")
                        } else {
                            subs.to_string()
                        });
                        ui.label(bits.to_string());
                        ui.label(format!("{rin} in, {rout} out"));
                        ui.label(mods.to_string());
                        if ui
                            .small_button("📈")
                            .on_hover_text("Open the viewer graph clipped to this broadcast")
                            .clicked()
                        {
                            open_clip = Some((s.started, s.ended));
                        }
                        ui.end_row();
                    }
                });
        }

        ui.add_space(10.0);
        let ev_out =
            events_table_ui(ui, "chstats_events", &data.events, 200, &mut self.chstats_event_filter);
        self.apply_events_table_out(channel_id, ev_out);

        if let Some((started, ended)) = open_clip {
            let name = self
                .channels
                .iter()
                .find(|c| c.id == channel_id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let label = format!("{name} — {}", fmt_datetime_short(started));
            self.open_stream_stats(channel_id, &label, started, ended);
        }
    }

    /// Execute the events table's right-click actions (shared by the Channel
    /// Stats view and the 📈 popup): open the 🚂 mark dialog prefilled with a
    /// row's timestamp, or delete a hype row (tightening the tuning when an
    /// INFERRED burst is deleted — that's a confirmed false positive).
    fn apply_events_table_out(&mut self, channel_id: i64, out: EventsTableOut) {
        if let Some(at) = out.mark_at {
            self.hype_mark_channel = channel_id;
            self.hype_mark_abs = chrono::DateTime::from_timestamp(at, 0)
                .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            self.show_hype_mark = true;
        }
        if let Some((event_id, tighten, monitor_id, at)) = out.delete {
            if tighten {
                let tuning = crate::hype::load_tuning(&self.core.store);
                let (pts, events, _) = crate::hype::observed_burst(
                    &self.core.store,
                    monitor_id,
                    at - tuning.window_secs,
                    at + 1,
                    &tuning,
                );
                crate::hype::tighten_for_false(
                    &self.core.store,
                    pts,
                    events,
                    "a deleted inferred train",
                );
                self.hype_tuning = crate::hype::load_tuning(&self.core.store);
            }
            let _ = self.core.store.delete_stream_event(event_id);
            self.chstats_data = None;
            if let Some(p) = &mut self.viewer_stats_popup {
                p.data = None;
            }
        }
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
            filter: String::new(),
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
            filter: String::new(),
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
        let mut ev_out = EventsTableOut::default();
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
                        // Same bucket-width rule as the load above.
                        let bucket = match popup.range {
                            Some((s, u)) => ((u - s) / 200).max(60),
                            None => popup.span.bucket_secs(),
                        };
                        viewer_graph_ui(
                            ui,
                            "viewer_stats_popup",
                            viewer,
                            events,
                            changes,
                            &labels,
                            bucket,
                        );
                        ui.add_space(8.0);
                        ev_out = events_table_ui(
                            ui,
                            "viewer_stats_popup_events",
                            events,
                            100,
                            &mut popup.filter,
                        );
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
        self.apply_events_table_out(popup_channel, ev_out);
    }
}

/// Top contributors of one event kind within the loaded span:
/// `(display name, total amount)` sorted by total, case-insensitive identity.
fn top_contributors(events: &[StreamEventRow], kind: &str, limit: usize) -> Vec<(String, i64)> {
    let mut by_actor: std::collections::HashMap<String, (String, i64)> = Default::default();
    for e in events.iter().filter(|e| e.kind == kind && !e.actor.is_empty()) {
        let entry = by_actor
            .entry(e.actor.to_lowercase())
            .or_insert_with(|| (e.actor.clone(), 0));
        entry.1 += e.amount.max(1);
    }
    let mut out: Vec<(String, i64)> = by_actor.into_values().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    out.truncate(limit);
    out
}

/// Medal for leaderboard ranks 1–3, plain number beyond.
fn rank_label(i: usize) -> String {
    match i {
        0 => "🥇".into(),
        1 => "🥈".into(),
        2 => "🥉".into(),
        n => format!("{}", n + 1),
    }
}

/// `3h 24m`-style duration for airtime cells.
fn fmt_duration_hm(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 { format!("{h}h {m:02}m") } else { format!("{m}m") }
}

/// `60` → `1 min`, `600` → `10 min`, `43200` → `12 h` — bucket-width caption.
fn fmt_bucket(secs: i64) -> String {
    if secs >= 86_400 {
        format!("{} d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{} h", secs / 3_600)
    } else {
        format!("{} min", (secs / 60).max(1))
    }
}

/// Draw the category/collab-change vertical lines shared by the viewer and
/// events plots, so a marker in either panel still lines up with a category
/// switch.
fn draw_change_vlines(
    plot_ui: &mut egui_plot::PlotUi,
    changes: &[(i64, String, String)],
    to_x: impl Fn(i64) -> f64,
) {
    for (t, kind, _new) in changes {
        let (color, style) = match kind.as_str() {
            "category" => (egui::Color32::from_gray(120), egui_plot::LineStyle::dotted_dense()),
            "collab" => (egui::Color32::from_rgb(0x42, 0xa5, 0xf5), egui_plot::LineStyle::dashed_loose()),
            _ => continue, // title changes are too chatty to mark
        };
        plot_ui.vline(egui_plot::VLine::new(String::new(), to_x(*t)).color(color).style(style));
    }
}

/// The shared viewer/events/follower graph: one viewer line per monitor on
/// its own plot, event markers (subs, bits, hype trains, …) on a SEPARATE
/// plot with its own scale — a hype train's point total can be tens of
/// thousands, which used to share the viewer-count axis and flatten it —
/// category/collab changes as vertical lines mirrored on both, plus a
/// separate follower plot when any follower data exists.
fn viewer_graph_ui(
    ui: &mut egui::Ui,
    id_prefix: &str,
    viewer: &[ViewerBucket],
    events: &[StreamEventRow],
    changes: &[(i64, String, String)],
    labels: &std::collections::HashMap<i64, String>,
    bucket_secs: i64,
) {
    let now = now_unix();
    let to_x = |t: i64| (t - now) as f64 / 3600.0; // hours relative to now
    let span_hours = viewer
        .first()
        .map(|b| (now - b.t) as f64 / 3600.0)
        .unwrap_or(1.0)
        .max(1.0);
    // Wall-clock axis labels (local time) — the relative "-0.1h" style made it
    // impossible to line the graph up with the events table's timestamps.
    let multi_day = span_hours > 48.0;
    let fmt_x = move |h: f64| {
        use chrono::TimeZone;
        let t = now + (h * 3600.0).round() as i64;
        match chrono::Local.timestamp_opt(t, 0).single() {
            Some(d) if multi_day => d.format("%d %b %H:%M").to_string(),
            Some(d) => d.format("%H:%M").to_string(),
            None => String::new(),
        }
    };
    let bucket_label = fmt_bucket(bucket_secs);

    ui.label("Viewers:").on_hover_text(format!(
        "Peak live viewers per {bucket_label} bucket, one line per platform \
         instance, plotted at each bucket's center. Dotted vertical lines are \
         category changes; 🤝 lines are collab set changes. X axis is local \
         clock time. Gaps mean the channel was offline (no samples). Events \
         (subs, bits, raids, hype trains, …) are on their own plot below, on \
         their own scale.",
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
            draw_change_vlines(plot_ui, changes, to_x);
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
                    // Bucket-CENTER placement: a bucket's peak covers
                    // [t, t+bucket), so plotting at t start would shift the
                    // line half a bucket early next to the exact-time event
                    // markers (visible as "misaligned raids" on wide spans).
                    seg.push([to_x(*t + bucket_secs / 2), *v as f64]);
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
        });

    // Events plot: subs/bits/raids/hype trains/… on their OWN scale, kept
    // off the viewer plot above — a single big hype train's point total
    // (tens of thousands) used to share the viewer-count axis and flatten
    // the viewer line to nothing.
    ui.add_space(8.0);
    ui.label("Events:").on_hover_text(
        "Chat/channel events (subs, bits, raids, hype trains, timeouts, …) \
         at their exact time, placed at the event's own size — a raid's \
         party size lands near the viewer level it delivered, a hype \
         train near its point total, 1-unit events hug the baseline. \
         Dotted/dashed vertical lines mirror the category/collab changes \
         above. Hover a marker to see who did it.",
    );
    // Filled per frame while the pointer is near event markers — the plot's
    // own label can only show coordinates; this names who did it.
    let mut hover_events: Vec<String> = Vec::new();
    let events_resp = egui_plot::Plot::new(format!("{id_prefix}_events_plot"))
        .height(140.0)
        .legend(egui_plot::Legend::default())
        .allow_scroll(false)
        .include_y(0.0)
        .x_axis_formatter(move |mark, _| fx(mark.value))
        .y_axis_formatter(|mark, _| grid::fmt_viewers(mark.value.max(0.0) as i64))
        .show(ui, |plot_ui| {
            draw_change_vlines(plot_ui, changes, to_x);
            // Event markers, grouped by kind for the legend. Y = the event's
            // own size (bits, gift-batch count, raid party, hype-train
            // points, timeout secs) — a scale shared only among events, not
            // with viewer count.
            let mut by_kind: std::collections::BTreeMap<&str, Vec<[f64; 2]>> = Default::default();
            for e in events {
                // First-time chatters are too dense to mark (dozens per
                // stream) — they stay in the events list below, filterable.
                if e.kind == "first_chat" {
                    continue;
                }
                by_kind
                    .entry(e.kind.as_str())
                    .or_default()
                    .push([to_x(e.at), e.amount.max(0) as f64]);
            }
            for (kind, pts) in by_kind {
                plot_ui.points(
                    egui_plot::Points::new(event_label(kind), egui_plot::PlotPoints::from(pts))
                        .shape(egui_plot::MarkerShape::Diamond)
                        .radius(4.0)
                        .color(event_color(kind)),
                );
            }
            // Who-did-it hover: name every event whose marker is within a few
            // pixels of the pointer (a gift train stacks many diamonds).
            if let Some(ptr) = plot_ui.pointer_coordinate() {
                let tf = *plot_ui.transform();
                let ptr_px = tf.position_from_point(&ptr);
                for e in events.iter().filter(|e| e.kind != "first_chat") {
                    let px = tf.position_from_point(&egui_plot::PlotPoint::new(
                        to_x(e.at),
                        e.amount.max(0) as f64,
                    ));
                    if px.distance(ptr_px) < 8.0 {
                        hover_events
                            .push(format!("{} — {}", fmt_datetime_short(e.at), event_line(e)));
                    }
                }
            }
        });
    if !hover_events.is_empty() {
        events_resp.response.on_hover_ui_at_pointer(|ui| {
            for line in hover_events.iter().take(12) {
                ui.label(line);
            }
            if hover_events.len() > 12 {
                ui.weak(format!("(+{} more here)", hover_events.len() - 12));
            }
        });
    }

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
                        per_monitor
                            .entry(b.monitor_id)
                            .or_default()
                            .push([to_x(b.t + bucket_secs / 2), f as f64]);
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

/// Row actions picked from the events table's right-click menu — the caller
/// (which owns `self`/the channel context) executes them.
#[derive(Default)]
pub(super) struct EventsTableOut {
    /// "🚂 A train started here" on a contribution row — that row's unix ts.
    pub mark_at: Option<i64>,
    /// 🗑 on a hype_train row: `(event id, tighten, monitor_id, at)` —
    /// `tighten` is true for inferred rows (deleting one is a false-positive
    /// signal for the auto-tune).
    pub delete: Option<(i64, bool, i64, i64)>,
}

/// The recent-events table shown under the graphs, with a live text filter
/// (user, recipient, kind, or detail — case-insensitive).
fn events_table_ui(
    ui: &mut egui::Ui,
    id: &str,
    events: &[StreamEventRow],
    limit: usize,
    filter: &mut String,
) -> EventsTableOut {
    let mut out = EventsTableOut::default();
    if events.is_empty() {
        return out;
    }
    let q = filter.trim().to_lowercase();
    let shown: Vec<&StreamEventRow> = events
        .iter()
        .filter(|e| {
            q.is_empty()
                || e.actor.to_lowercase().contains(&q)
                || e.target.to_lowercase().contains(&q)
                || e.detail.to_lowercase().contains(&q)
                || event_label(&e.kind).to_lowercase().contains(&q)
        })
        .collect();
    ui.horizontal(|ui| {
        let count = if q.is_empty() {
            format!("Events ({}):", events.len())
        } else {
            format!("Events ({} of {}):", shown.len(), events.len())
        };
        ui.label(count).on_hover_text(
            "Subs, resubs, gift subs and bits come from the recorded chat (so only \
             while a recording with Chat log was running); raids come from chat \
             and/or EventSub; hype trains from live polling, chat inference, or \
             manual marks. Newest first. Right-click a sub/bits row for \"🚂 a \
             train started here\", or a hype-train row to delete it.",
        );
        ui.label("🔍");
        ui.add(
            egui::TextEdit::singleline(filter)
                .hint_text("Filter events…")
                .desired_width(180.0),
        )
        .on_hover_text(
            "Show only events mentioning this text: who did it, who received it, \
             the event kind, or the detail (deleted-message text, mode change, …). \
             Case-insensitive.",
        );
        if !filter.is_empty() && ui.small_button("✖").on_hover_text("Clear filter").clicked() {
            filter.clear();
        }
    });
    if shown.is_empty() {
        ui.weak("No events match the filter.");
        return out;
    }
    egui::Grid::new(id).num_columns(3).striped(true).spacing([16.0, 2.0]).show(ui, |ui| {
        for e in shown.iter().take(limit) {
            ui.label(fmt_datetime_short(e.at));
            ui.colored_label(event_color(&e.kind), event_label(&e.kind));
            let line = ui.label(event_line(e));
            // Row actions (labels are hover-sense — re-interact with click
            // sense on the same rect so right-click registers).
            let is_contrib =
                matches!(e.kind.as_str(), "sub" | "resub" | "subgift" | "bits" | "dono");
            let is_hype = e.kind == "hype_train";
            if is_contrib || is_hype {
                let ctx_resp = ui.interact(
                    line.rect,
                    egui::Id::new(id).with("evctx").with(e.id),
                    egui::Sense::click(),
                );
                ctx_resp.context_menu(|ui| {
                    if is_contrib
                        && ui
                            .button("🚂 A train started here")
                            .on_hover_text(
                                "Mark a hype train as kicking off at this event's \
                                 time — opens the manual-mark dialog with the \
                                 timestamp prefilled.",
                            )
                            .clicked()
                    {
                        out.mark_at = Some(e.at);
                        ui.close();
                    }
                    if is_hype {
                        let inferred = e.detail.contains("(inferred)");
                        let (label, hover) = if inferred {
                            (
                                "🗑 Not a train (delete & tighten)",
                                "Delete this inferred burst AND count it as a false \
                                 positive: with auto-tune on, the thresholds move up \
                                 past this burst's size so it wouldn't fire again.",
                            )
                        } else {
                            (
                                "🗑 Delete event",
                                "Remove this train from the history. No tuning \
                                 change (confirmed/manual trains aren't inference \
                                 mistakes).",
                            )
                        };
                        if ui.button(label).on_hover_text(hover).clicked() {
                            out.delete = Some((e.id, inferred, e.monitor_id, e.at));
                            ui.close();
                        }
                    }
                });
            }
            ui.end_row();
        }
    });
    if shown.len() > limit {
        ui.weak(format!("(+{} more in this span)", shown.len() - limit));
    }
    out
}
