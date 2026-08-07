//! Backlog & Stream History views: one shared cross-channel recording list
//! (`Store::recordings_all` + `Store::stream_watch_states`), two presets.
//! Watch-state belongs to the *broadcast* (`StreamGroup::key`), not any one
//! take/file — see `crate::store::watch` for the schema.

use super::*;
use crate::models::RollingState;

/// The four watch states, in display order, paired with their row/filter label.
pub(super) const WATCH_STATES: [(&str, &str); 4] = [
    ("unwatched", "◻ Unwatched"),
    ("started", "▶ Started"),
    ("skipped", "⏭ Skipped"),
    ("watched", "✔ Watched"),
];

/// A broadcast's watch state, defaulting to `("unwatched", None)` when it
/// was never touched (see `Store::stream_watch_states`'s doc).
pub(super) fn effective_watch_state<'a>(
    map: &'a HashMap<String, (String, Option<i64>)>,
    key: &str,
) -> (&'a str, Option<i64>) {
    match map.get(key) {
        Some((s, at)) => (s.as_str(), *at),
        None => ("unwatched", None),
    }
}

/// Whether a take currently at watch-state `current` should auto-advance to
/// `"started"` when it's opened/played — never downgrades an already
/// `"started"`/`"watched"` broadcast. `None` (never touched) counts as
/// `"unwatched"`.
pub(super) fn should_advance_to_started(current: Option<&str>) -> bool {
    matches!(current, None | Some("unwatched") | Some("skipped"))
}

/// Buckets a flat cross-channel recording list by monitor, groups each
/// bucket into broadcasts (`group_recordings` assumes single-monitor input,
/// hence the bucketing pass), then merges + re-sorts newest-first across
/// monitors. Returns `(monitor_id, StreamGroup)` pairs.
pub(super) fn flat_stream_groups(recordings: &[Recording]) -> Vec<(i64, StreamGroup)> {
    let mut by_monitor: HashMap<i64, Vec<Recording>> = HashMap::new();
    for r in recordings {
        by_monitor.entry(r.monitor_id).or_default().push(r.clone());
    }
    let mut out: Vec<(i64, StreamGroup)> = Vec::new();
    for (mid, recs) in by_monitor {
        for g in group_recordings(&recs) {
            out.push((mid, g));
        }
    }
    out.sort_by_key(|(_, g)| std::cmp::Reverse(g.started_at()));
    out
}

/// Stream History's checkbox filter bank (session-only, not persisted) — see
/// the module doc for how each maps to existing `Recording` fields.
#[derive(Default)]
pub(super) struct HistoryFilters {
    pub(super) missing_vod: bool,
    pub(super) muted_vod: bool,
    pub(super) vod_pending: bool,
    pub(super) recorded: bool,
    pub(super) remux_pending: bool,
    pub(super) remuxed: bool,
    pub(super) chapters_embedded: bool,
    pub(super) chapters_pending: bool,
    pub(super) failed_unacked: bool,
    pub(super) head_backfill_pending: bool,
    pub(super) gap_recovered: bool,
    pub(super) stuck_in_cache: bool,
}

impl HistoryFilters {
    fn any_set(&self) -> bool {
        self.missing_vod
            || self.muted_vod
            || self.vod_pending
            || self.recorded
            || self.remux_pending
            || self.remuxed
            || self.chapters_embedded
            || self.chapters_pending
            || self.failed_unacked
            || self.head_backfill_pending
            || self.gap_recovered
            || self.stuck_in_cache
    }

    /// Whether take `t` matches at least one ticked filter ("any of these
    /// states" — OR, not AND), or passes trivially when nothing is ticked.
    pub(super) fn matches(&self, t: &Recording) -> bool {
        if !self.any_set() {
            return true;
        }
        (self.missing_vod && is_missing_vod(t))
            || (self.muted_vod && is_muted_vod(t))
            || (self.vod_pending && is_vod_pending(t))
            || (self.recorded && is_recorded(t))
            || (self.remux_pending && is_remux_pending(t))
            || (self.remuxed && is_remuxed(t))
            || (self.chapters_embedded && is_chapters_embedded(t))
            || (self.chapters_pending && is_chapters_pending(t))
            || (self.failed_unacked && is_failed_unacked(t))
            || (self.head_backfill_pending && is_head_backfill_pending(t))
            || (self.gap_recovered && is_gap_recovered(t))
            || (self.stuck_in_cache && is_stuck_in_cache(t))
    }
}

pub(super) fn is_missing_vod(r: &Recording) -> bool {
    r.vod_state.as_deref() == Some("not_published")
}
pub(super) fn is_muted_vod(r: &Recording) -> bool {
    r.vod_muted_secs.is_some_and(|s| s > 0)
}
pub(super) fn is_vod_pending(r: &Recording) -> bool {
    r.vod_state.as_deref() == Some("pending")
}
pub(super) fn is_recorded(r: &Recording) -> bool {
    !r.output_path.is_empty() && r.status == "completed"
}
pub(super) fn is_remux_pending(r: &Recording) -> bool {
    r.output_path.ends_with(".ts") && crate::downloader::path_in_cache(&r.output_path)
}
pub(super) fn is_remuxed(r: &Recording) -> bool {
    !r.output_path.is_empty()
        && !r.output_path.ends_with(".ts")
        && !crate::downloader::path_in_cache(&r.output_path)
}
pub(super) fn is_chapters_embedded(r: &Recording) -> bool {
    r.chapters_state == "done"
}
pub(super) fn is_chapters_pending(r: &Recording) -> bool {
    r.chapters_state == "queued"
}
pub(super) fn is_failed_unacked(r: &Recording) -> bool {
    r.status == "failed" && !r.err_ack
}
pub(super) fn is_head_backfill_pending(r: &Recording) -> bool {
    r.head_backfill_state == "queued"
}
pub(super) fn is_gap_recovered(r: &Recording) -> bool {
    r.gap_splice_state == "done"
}
pub(super) fn is_stuck_in_cache(r: &Recording) -> bool {
    r.status == "completed"
        && crate::downloader::path_in_cache(&r.output_path)
        && !r.output_path.ends_with(".ts")
}

/// `(channel name, platform)` for a monitor id, resolved against the
/// already-in-memory `self.rows` — no store hit.
fn channel_label(rows: &[MonitorWithChannel], mid: i64) -> (String, Option<Platform>) {
    match rows.iter().find(|r| r.monitor.id == mid) {
        Some(r) => (r.channel.name.clone(), Some(r.monitor.platform())),
        None => (format!("(removed monitor #{mid})"), None),
    }
}

/// `""` for zero, the number otherwise — count columns read better blank than
/// as a column of noughts.
fn non_zero(n: i64) -> String {
    if n > 0 { n.to_string() } else { String::new() }
}

/// A broadcast's rolling state, rolled up from its takes.
///
/// Rolling-ness is per *take* (it's a per-file TTL), but every take of one
/// broadcast comes from the same instance under the same settings, so in
/// practice they share a TTL and expire together — and Keep is far more useful
/// as "keep this broadcast" than as "keep take 2 of 3". The soonest deadline
/// among the still-counting takes is what the row shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupRolling {
    /// No take of this broadcast is (or ever was) a rolling recording.
    None,
    /// At least one take is still counting down. `deadline` is `None` while a
    /// take is still recording.
    Rolling { deadline: Option<i64> },
    /// Every rolling take was kept — an ordinary archived broadcast that
    /// happens to have come from a rolling recording.
    Kept,
    /// Every rolling take has been swept; the media is gone, the history isn't.
    Expired,
}

pub(super) fn group_rolling(g: &StreamGroup) -> GroupRolling {
    let mut counting: Vec<Option<i64>> = Vec::new();
    let (mut kept, mut expired) = (false, false);
    for t in &g.takes {
        match t.rolling.state(t.ended_at) {
            RollingState::None => {}
            RollingState::Rolling { deadline } => counting.push(deadline),
            RollingState::Kept { .. } => kept = true,
            RollingState::Expired { .. } => expired = true,
        }
    }
    if !counting.is_empty() {
        // A take with no deadline yet (still recording) wins over any dated
        // sibling: "still going" is the honest answer for the broadcast.
        let deadline = if counting.iter().any(Option::is_none) {
            None
        } else {
            counting.iter().flatten().copied().min()
        };
        return GroupRolling::Rolling { deadline };
    }
    // Nothing is counting down any more. Kept wins over expired: a broadcast
    // with one kept take still has media on disk.
    if kept {
        GroupRolling::Kept
    } else if expired {
        GroupRolling::Expired
    } else {
        GroupRolling::None
    }
}

/// Draw one Backlog cell. Column order is driven by the user's persisted
/// arrangement, so this dispatches on the column **id**, never on an index —
/// exactly like the Streams/Videos row renderers.
#[allow(clippy::too_many_arguments)]
fn backlog_cell(
    ui: &mut egui::Ui,
    id: &str,
    mid: i64,
    g: &StreamGroup,
    watch_state: &str,
    now: i64,
    rows: &[MonitorWithChannel],
    set_state: &mut Option<(String, i64, &'static str)>,
    open_chat: &mut Option<(i64, i64)>,
) {
    match id {
        "watch" => {
            // The four states as one exclusive strip — clicking any of them
            // sets it directly, which is what a to-do list needs (the
            // auto-advance on play only ever moves you forward).
            ui.spacing_mut().item_spacing.x = 2.0;
            for (s, label) in WATCH_STATES {
                if ui
                    .selectable_label(watch_state == s, label)
                    .on_hover_text(format!("Mark this broadcast \"{s}\""))
                    .clicked()
                {
                    *set_state = Some((g.key.clone(), mid, s));
                }
            }
        }
        "platform" => {
            let (_, platform) = channel_label(rows, mid);
            if let Some(p) = platform {
                ui.weak(p.label()).on_hover_text(p.label());
            }
        }
        "channel" => {
            let (name, _) = channel_label(rows, mid);
            ui.label(egui::RichText::new(&name).strong()).on_hover_text(name);
        }
        "title" => {
            let t = g.title();
            if !t.is_empty() {
                ui.label(t).on_hover_text(t);
            }
        }
        "game" => {
            let c = g.category();
            if !c.is_empty() {
                ui.weak(c).on_hover_text(c);
            }
        }
        "went_live" => {
            if let Some(t) = g.went_live_at {
                ts_label(ui, t);
                if g.went_live_approx {
                    ui.weak("~").on_hover_text("Approximate — our own first-seen time, not the platform's.");
                }
            }
        }
        "started" => ts_label(ui, g.started_at()),
        "duration" => {
            ui.label(fmt_duration(g.captured_secs(now)));
        }
        "size" => {
            let bytes: i64 = g.takes.iter().map(|t| t.bytes).sum();
            if bytes > 0 {
                ui.label(fmt_bytes(bytes));
            } else {
                ui.weak("—").on_hover_text(
                    "No file on disk for this broadcast — never captured, or the media has since \
                     been deleted (manually, or by a rolling recording expiring). The history row \
                     stays either way.",
                );
            }
        }
        "chat" => {
            // The take that actually has the sidecar, so the popup opens the
            // right one on a multi-take broadcast.
            if let Some(t) = g.takes.iter().find(|t| !t.chat_path.is_empty())
                && ui
                    .button("💬")
                    .on_hover_text("Open the chat replay for this broadcast")
                    .clicked()
            {
                *open_chat = Some((mid, t.id));
            }
        }
        "changes" => {
            let n = g.meta_change_count();
            if n > 0 {
                ui.label(format!("✏{n}"))
                    .on_hover_text(format!("{n} title/category change(s) logged during this broadcast"));
            }
        }
        "ads" => {
            let n = g.ad_count();
            if n > 0 {
                ui.label(format!("📢{n}")).on_hover_text(format!(
                    "{n} ad break(s), {} total",
                    fmt_duration(g.ad_secs())
                ));
            }
        }
        "status" => {
            let (icon, color) = state_icon_ack(g.status(), g.takes.last().is_some_and(|t| t.err_ack));
            ui.colored_label(color, icon).on_hover_text(g.status());
        }
        _ => {}
    }
}

impl StreamArchiverApp {
    /// Loads `history_all`/`history_watch` once; call at the top of both
    /// views. `reload_history` forces a refresh (e.g. after "Load more").
    pub(super) fn ensure_history_loaded(&mut self) {
        if self.history_loaded {
            return;
        }
        self.reload_history();
    }

    pub(super) fn reload_history(&mut self) {
        self.history_all = self.core.store.recordings_all(self.history_load_limit).unwrap_or_default();
        self.history_watch = self.core.store.stream_watch_states().unwrap_or_default();
        self.history_loaded = true;
    }

    /// One [`Cell`] per [`BACKLOG_COLUMNS`] entry for one broadcast — the
    /// sort/filter model `ordered_rows` consumes. Kept next to the row renderer
    /// so the two can't drift out of column order.
    fn backlog_cells(
        &self,
        mid: i64,
        g: &StreamGroup,
        watch_state: &str,
        now: i64,
    ) -> Vec<Cell> {
        let (name, platform) = channel_label(&self.rows, mid);
        let bytes: i64 = g.takes.iter().map(|t| t.bytes).sum();
        let has_chat = g.takes.iter().any(|t| !t.chat_path.is_empty());
        let watch_label = WATCH_STATES
            .iter()
            .find(|(s, _)| *s == watch_state)
            .map(|(_, l)| *l)
            .unwrap_or("");
        vec![
            Cell::text(watch_label),
            Cell::text(platform.map(|p| p.label().to_string()).unwrap_or_default()),
            Cell::text(name),
            Cell::text(g.title()),
            Cell::text(g.category()),
            Cell::num(g.went_live_at.unwrap_or(0) as f64, fmt_datetime_short(g.went_live_at.unwrap_or(0))),
            Cell::num(g.started_at() as f64, fmt_datetime_short(g.started_at())),
            Cell::num(g.captured_secs(now) as f64, fmt_duration(g.captured_secs(now))),
            Cell::num(bytes as f64, if bytes > 0 { fmt_bytes(bytes) } else { String::new() }),
            Cell::num(has_chat as i64 as f64, if has_chat { "💬".into() } else { String::new() }),
            Cell::num(g.meta_change_count() as f64, non_zero(g.meta_change_count())),
            Cell::num(g.ad_count() as f64, non_zero(g.ad_count())),
            Cell::text(g.status()),
        ]
    }

    /// The 🕰 Rolling recordings section at the top of Backlog: every broadcast
    /// still counting down towards auto-deletion, soonest first, with its
    /// remaining time and a **Keep** button. Ticking "Show kept" also lists the
    /// ones already rescued, so **Unkeep** is reachable from here rather than
    /// only from the Streams take row.
    ///
    /// Uses the same columns (and the same user arrangement) as the main grid
    /// below it, with TTL + the button prepended — this is the same list seen
    /// through a different lens, not a different list.
    ///
    /// Returns the actions the caller applies after the borrow ends.
    fn backlog_rolling_section(
        &mut self,
        ui: &mut egui::Ui,
        groups: &[(i64, StreamGroup)],
        col_order: &[usize],
        now: i64,
    ) -> Vec<(i64, bool)> {
        use egui_extras::{Column, TableBuilder};
        let mut acts: Vec<(i64, bool)> = Vec::new(); // (recording id, keep?)

        // Soonest-expiring first: this is a countdown list, so recency (the
        // main grid's order) is the wrong axis entirely.
        let mut rolling: Vec<(i64, &StreamGroup, GroupRolling)> = groups
            .iter()
            .map(|(mid, g)| (*mid, g, group_rolling(g)))
            .filter(|(_, _, r)| {
                matches!(r, GroupRolling::Rolling { .. })
                    || (self.backlog_show_kept && matches!(r, GroupRolling::Kept))
            })
            .collect();
        rolling.sort_by_key(|(_, _, r)| match r {
            // Still-recording takes have no deadline yet — they sort last, not
            // first, since nothing is at risk until they finish.
            GroupRolling::Rolling { deadline } => (0, deadline.unwrap_or(i64::MAX)),
            _ => (1, i64::MAX),
        });
        let counting = rolling.iter().filter(|(_, _, r)| matches!(r, GroupRolling::Rolling { .. })).count();
        if counting == 0 && !self.backlog_show_kept {
            return acts;
        }
        let soonest = rolling
            .iter()
            .filter_map(|(_, _, r)| match r {
                GroupRolling::Rolling { deadline } => *deadline,
                _ => None,
            })
            .min();
        let header = match soonest {
            Some(d) => format!("🕰  Rolling recordings ({counting}) — next in {}", crate::rolling::fmt_remaining(d - now)),
            None => format!("🕰  Rolling recordings ({counting})"),
        };

        egui::CollapsingHeader::new(header)
            .id_salt("backlog_rolling_section")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.weak(
                        "These files are deleted automatically when their time runs out. \
                         Keep one to make it a normal archived stream — its history row \
                         survives either way, only the video goes.",
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.checkbox(&mut self.backlog_show_kept, "Show kept")
                            .on_hover_text(
                                "Also list broadcasts you've already kept, so you can Unkeep \
                                 one (which restarts its countdown from now, never from when \
                                 it was recorded).",
                            );
                    });
                });
                if rolling.is_empty() {
                    ui.weak("Nothing rolling right now.");
                    return;
                }
                egui::ScrollArea::both()
                    .id_salt("backlog_rolling_scroll")
                    .max_height(220.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.style_mut().interaction.selectable_labels = false;
                        let mut tb = TableBuilder::new(ui)
                            .id_salt("backlog_rolling_table")
                            .striped(true)
                            .resizable(true)
                            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                            .column(Column::auto().at_least(72.0))
                            .column(Column::auto().at_least(64.0));
                        for &i in col_order {
                            tb = tb.column(Column::auto().at_least(BACKLOG_COLUMNS[i].min_width));
                        }
                        tb.header(20.0, |mut h| {
                            h.col(|ui| {
                                ui.strong("Time left").on_hover_text(
                                    "How long until this broadcast's file is deleted \
                                     automatically. The clock starts when the recording ends, \
                                     so a live one shows no countdown yet.",
                                );
                            });
                            h.col(|ui| {
                                ui.strong("").on_hover_text("Keep / Unkeep");
                            });
                            for &i in col_order {
                                h.col(|ui| {
                                    ui.strong(BACKLOG_COLUMNS[i].title)
                                        .on_hover_text(BACKLOG_COLUMNS[i].tooltip);
                                });
                            }
                        })
                        .body(|mut body| {
                            for (mid, g, state) in &rolling {
                                body.row(24.0, |mut tr| {
                                    tr.col(|ui| match state {
                                        GroupRolling::Rolling { deadline: Some(d) } => {
                                            let left = d - now;
                                            let text = crate::rolling::fmt_remaining(left);
                                            // Under a day is the point at which
                                            // "I should decide about this" turns
                                            // into "decide now".
                                            if left < 86_400 {
                                                ui.colored_label(grid::HL_ERROR_TEXT, text)
                                            } else {
                                                ui.label(text)
                                            }
                                            .on_hover_text(format!("Deleted automatically at {}", fmt_datetime_short(*d)));
                                        }
                                        GroupRolling::Rolling { deadline: None } => {
                                            ui.weak("recording").on_hover_text(
                                                "Still capturing — the countdown starts when it ends.",
                                            );
                                        }
                                        _ => {
                                            ui.weak("kept").on_hover_text("Not counting down.");
                                        }
                                    });
                                    tr.col(|ui| {
                                        let keeping = matches!(state, GroupRolling::Rolling { .. });
                                        let label = if keeping { "📌 Keep" } else { "↩ Unkeep" };
                                        if ui
                                            .button(label)
                                            .on_hover_text(if keeping {
                                                "Stop the countdown — this becomes a normal \
                                                 archived stream (marked as kept from a rolling \
                                                 recording)."
                                            } else {
                                                "Put it back in the rolling set. The countdown \
                                                 restarts from now, so it won't be deleted \
                                                 immediately."
                                            })
                                            .clicked()
                                        {
                                            for t in &g.takes {
                                                if t.rolling.ttl_secs > 0 && t.rolling.expired_at == 0 {
                                                    acts.push((t.id, keeping));
                                                }
                                            }
                                        }
                                    });
                                    let (watch, _) = effective_watch_state(&self.history_watch, &g.key);
                                    for &ci in col_order {
                                        tr.col(|ui| {
                                            backlog_cell(
                                                ui,
                                                BACKLOG_COLUMNS[ci].id,
                                                *mid,
                                                g,
                                                watch,
                                                now,
                                                &self.rows,
                                                &mut None,
                                                &mut None,
                                            );
                                        });
                                    }
                                });
                            }
                        });
                    });
            });
        ui.separator();
        acts
    }

    /// 📥 Backlog: every broadcast across every channel, flat and newest-first,
    /// as a full grid (hide/show/reorder/resize/sort/filter per column, all
    /// persisted — see [`crate::grid_columns`]).
    ///
    /// This can't just be a mode of 📺 Streams: that view is a *tree* grouped
    /// under channel containers, and the whole point here is the opposite
    /// ordering — one flat list sorted by recency, so "what should I catch up
    /// on next" is the first thing on screen.
    pub(super) fn backlog_view(&mut self, ui: &mut egui::Ui) {
        use egui_extras::{Column, TableBuilder};
        self.ensure_history_loaded();
        let now = now_unix();
        let groups = flat_stream_groups(&self.history_all);

        ui.horizontal_wrapped(|ui| {
            ui.label("Show:");
            for (state, label) in WATCH_STATES {
                let on = self.backlog_show_states.contains(state);
                if ui
                    .selectable_label(on, label)
                    .on_hover_text(format!("Toggle showing \"{state}\" broadcasts"))
                    .clicked()
                {
                    if on {
                        self.backlog_show_states.remove(state);
                    } else {
                        self.backlog_show_states.insert(state.to_string());
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("⬇ Load more (+500)")
                    .on_hover_text("Raise the load cap and re-query for older broadcasts")
                    .clicked()
                {
                    self.history_load_limit += 500;
                    self.reload_history();
                }
                if ui.button("⟳ Refresh").on_hover_text("Reload from the database").clicked() {
                    self.reload_history();
                }
            });
        });
        ui.separator();

        if groups.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| ui.weak("No recordings yet."));
            return;
        }

        let mut sort = std::mem::take(&mut self.backlog_sort);
        let mut filters = std::mem::take(&mut self.backlog_filters);
        let mut entries = self.backlog_grid.entries.clone();
        let col_order = grid_columns::effective_order(&BACKLOG_COLUMNS, &entries, |_| true);
        let order_changed = self.backlog_grid.note_order(&col_order);

        // The rolling section sits above the main grid and is deliberately NOT
        // subject to the watch-state chips: a file about to be deleted has to
        // be visible whether or not you've watched it. Rendered before the
        // model below is built so it can still borrow `self` mutably.
        let rolling_acts = self.backlog_rolling_section(ui, &groups, &col_order, now);
        if !rolling_acts.is_empty() {
            for (rec_id, keep) in rolling_acts {
                let _ = if keep {
                    self.core.store.keep_rolling_recording(rec_id, now)
                } else {
                    self.core.store.unkeep_rolling_recording(rec_id, now)
                };
            }
            self.reload_history();
            self.backlog_sort = sort;
            self.backlog_filters = filters;
            return;
        }

        // Watch-state chips filter BEFORE the model is built, so a hidden state
        // can't be reached by column sorting either.
        let visible: Vec<(i64, &StreamGroup, &str)> = groups
            .iter()
            .map(|(mid, g)| {
                let (state, _) = effective_watch_state(&self.history_watch, &g.key);
                (*mid, g, state)
            })
            .filter(|(_, _, state)| self.backlog_show_states.contains(*state))
            .collect();
        let model: Vec<Vec<Cell>> =
            visible.iter().map(|(mid, g, state)| self.backlog_cells(*mid, g, state, now)).collect();

        let mut set_state: Option<(String, i64, &'static str)> = None;
        let mut want_reorder = false;
        let mut open_chat: Option<(i64, i64)> = None;

        egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            let mut tb = TableBuilder::new(ui)
                .id_salt(GridTableId::Backlog.key())
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
            if order_changed {
                tb.reset();
            }
            for &i in &col_order {
                let c = &BACKLOG_COLUMNS[i];
                let seed = self.backlog_grid.widths.get(c.id);
                let col = if c.stretch {
                    Column::remainder().at_least(c.min_width)
                } else if order_changed && let Some(w) = seed {
                    Column::auto_with_initial_suggestion(w).at_least(c.min_width)
                } else if c.initial > 0.0 {
                    Column::initial(c.initial).at_least(c.min_width).clip(true)
                } else {
                    Column::auto().at_least(c.min_width)
                };
                tb = tb.column(col);
            }
            let table = tb.header(46.0, |mut header| {
                for &i in &col_order {
                    let c = &BACKLOG_COLUMNS[i];
                    let (rect, _) = header.col(|ui| {
                        if grid_header_cell(
                            ui, GridTableId::Backlog, i, c, true, &mut sort, &mut filters[i],
                            &mut entries, &BACKLOG_COLUMNS, |_| false,
                        ) {
                            want_reorder = true;
                        }
                    });
                    self.backlog_grid.widths.note(c.id, rect.width());
                }
            });
            table.body(|body| {
                let order = ordered_rows(&model, &sort, &filters);
                body.rows(24.0, order.len(), |mut tr| {
                    let (mid, g, state) = visible[order[tr.index()]];
                    for &ci in &col_order {
                        tr.col(|ui| {
                            backlog_cell(
                                ui,
                                BACKLOG_COLUMNS[ci].id,
                                mid,
                                g,
                                state,
                                now,
                                &self.rows,
                                &mut set_state,
                                &mut open_chat,
                            );
                        });
                    }
                });
            });
        });

        // Persist only on an actual change, so an untouched view doesn't write
        // the settings row every frame.
        if sort != self.backlog_sort {
            let keys: Vec<(usize, bool)> = sort.keys.iter().map(|l| (l.col, l.ascending)).collect();
            let persisted = grid_columns::unresolve_sort(&BACKLOG_COLUMNS, &keys);
            grid_columns::save_sort(&self.core.store, GridTableId::Backlog, &persisted);
        }
        self.backlog_sort = sort;
        self.backlog_filters = filters;
        if want_reorder {
            self.reorder_columns = Some(Arc::new(Mutex::new(ReorderColumnsState {
                table: GridTableId::Backlog,
                draft: entries.clone(),
                apply: false,
                cancel: false,
            })));
        }
        if entries != self.backlog_grid.entries {
            self.backlog_grid.entries = entries;
            grid_columns::save_columns(&self.core.store, GridTableId::Backlog, &self.backlog_grid.entries);
        }
        if let Some((key, mid, state)) = set_state {
            let _ = self.core.store.set_stream_watch_state(&key, mid, state);
            self.reload_history();
        }
        if let Some((mid, rid)) = open_chat {
            let ctx = ui.ctx().clone();
            self.open_chat_popup(mid, Some(rid), &ctx);
        }
    }

    pub(super) fn stream_history_view(&mut self, ui: &mut egui::Ui) {
        self.ensure_history_loaded();
        let groups = flat_stream_groups(&self.history_all);

        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.history_search)
                    .hint_text("Filter…")
                    .desired_width(160.0),
            )
            .on_hover_text("Matches the channel name.");
            ui.checkbox(&mut self.history_filters.missing_vod, "Missing/deleted VOD")
                .on_hover_text("The streamer never published a VOD for this take.");
            ui.checkbox(&mut self.history_filters.muted_vod, "Muted VOD")
                .on_hover_text("The published Twitch VOD has DMCA-muted seconds.");
            ui.checkbox(&mut self.history_filters.vod_pending, "VOD check pending")
                .on_hover_text("The background VOD checker hasn't resolved this take yet.");
            ui.checkbox(&mut self.history_filters.recorded, "Recorded")
                .on_hover_text("Capture completed and a local file exists.");
            ui.checkbox(&mut self.history_filters.remux_pending, "Remux pending")
                .on_hover_text("Still a .ts capture in the cache dir — the automatic remux to MKV failed.");
            ui.checkbox(&mut self.history_filters.remuxed, "Remuxed")
                .on_hover_text("Finished in its final (non-cache, non-.ts) container.");
            ui.checkbox(&mut self.history_filters.chapters_embedded, "Chapters embedded")
                .on_hover_text("Chapter markers were embedded into the finished file.");
            ui.checkbox(&mut self.history_filters.chapters_pending, "Chapters pending")
                .on_hover_text("Chapter embedding is queued but hasn't run yet.");
            ui.checkbox(&mut self.history_filters.failed_unacked, "Failed (unacked)")
                .on_hover_text("Failed and not yet acknowledged — still bubbling up as ⚠.");
            ui.checkbox(&mut self.history_filters.head_backfill_pending, "Head-backfill pending")
                .on_hover_text("A missed-beginning backfill is queued for this take.");
            ui.checkbox(&mut self.history_filters.gap_recovered, "Gap-recovered")
                .on_hover_text("A lost-segment gap was successfully spliced back in.");
            ui.checkbox(&mut self.history_filters.stuck_in_cache, "Stuck in cache")
                .on_hover_text("Capture completed but the promote-to-output-dir move never finished.");
        });
        ui.separator();

        let search = self.history_search.trim().to_lowercase();
        let mut open_vod: Option<(String, Recording)> = None;
        let mut open_remux: Option<(String, Recording)> = None;
        let mut open_chapters: Option<i64> = None;
        let mut shown = 0usize;

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (mid, g) in &groups {
                if !g.takes.iter().any(|t| self.history_filters.matches(t)) {
                    continue;
                }
                let (name, platform) = channel_label(&self.rows, *mid);
                if !search.is_empty() && !name.to_lowercase().contains(&search) {
                    continue;
                }
                shown += 1;
                let last = g.takes.last();
                ui.horizontal(|ui| {
                    ui.set_min_width(220.0);
                    ui.label(egui::RichText::new(&name).strong());
                    if let Some(p) = platform {
                        ui.weak(format!("{p:?}"));
                    }
                    ts_label(ui, g.started_at());
                    ui.label(fmt_duration(g.captured_secs(now_unix())));
                    let (state, _) = effective_watch_state(&self.history_watch, &g.key);
                    ui.weak(WATCH_STATES.iter().find(|(s, _)| *s == state).map(|(_, l)| *l).unwrap_or(""));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(t) = last {
                            if (t.vod_state.is_some() || t.vod_muted_secs.is_some())
                                && ui.small_button("ℹ VOD").clicked()
                            {
                                open_vod = Some((name.clone(), t.clone()));
                            }
                            if (is_remux_pending(t) || is_stuck_in_cache(t))
                                && ui.small_button("ℹ Remux").clicked()
                            {
                                open_remux = Some((name.clone(), t.clone()));
                            }
                            if !t.chapters_state.is_empty() && ui.small_button("ℹ Chapters").clicked() {
                                open_chapters = Some(t.id);
                            }
                        }
                    });
                });
                ui.separator();
            }
            if shown == 0 {
                ui.weak("Nothing matches the current filter/search.");
            }
        });

        ui.horizontal(|ui| {
            ui.weak(format!("{shown} shown / {} loaded", self.history_all.len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("⟳ Refresh")
                    .on_hover_text("Reload from the database")
                    .clicked()
                {
                    self.reload_history();
                }
                if ui
                    .button("⬇ Load more (+500)")
                    .on_hover_text("Raise the load cap and re-query for older recordings")
                    .clicked()
                {
                    self.history_load_limit += 500;
                    self.reload_history();
                }
            });
        });

        if let Some((name, rec)) = open_vod {
            self.vod_info_popup_cache.insert(rec.id, (name, rec.clone()));
            if !self.vod_info_popups.contains(&rec.id) {
                self.vod_info_popups.push(rec.id);
            }
        }
        if let Some((name, rec)) = open_remux {
            self.remux_info_popup_cache.insert(rec.id, (name, rec.clone()));
            if !self.remux_info_popups.contains(&rec.id) {
                self.remux_info_popups.push(rec.id);
            }
        }
        if let Some(rid) = open_chapters
            && !self.chapters_popups.contains(&rid)
        {
            self.chapters_popups.push(rid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(output_path: &str) -> Recording {
        Recording {
            id: 1,
            monitor_id: 1,
            started_at: 0,
            ended_at: None,
            status: "completed".into(),
            bytes: 0,
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
        }
    }

    /// A finished broadcast made of `takes`, for the rollup tests below.
    fn group(takes: Vec<Recording>) -> StreamGroup {
        StreamGroup {
            key: "m1:s1".into(),
            stream_id: Some("s1".into()),
            went_live_at: Some(0),
            went_live_approx: false,
            takes,
        }
    }

    #[test]
    fn group_rolling_rolls_takes_up_to_one_broadcast_state() {
        use crate::models::Rolling;
        let ended = |ttl: i64, kept: i64, expired: i64| {
            let mut r = rec("C:/out/a.mkv");
            r.ended_at = Some(1_000);
            r.rolling = Rolling { ttl_secs: ttl, from: 0, kept_at: kept, expired_at: expired };
            r
        };

        // Nothing rolling at all.
        assert!(matches!(group_rolling(&group(vec![rec("C:/out/a.mkv")])), GroupRolling::None));

        // One counting-down take makes the whole broadcast rolling, and the
        // SOONEST deadline is the one that matters (that's when the first file
        // goes).
        let mut early = ended(60, 0, 0);
        early.ended_at = Some(100);
        let late = ended(60, 0, 0); // ends at 1000
        let g = group(vec![early, late]);
        assert!(matches!(group_rolling(&g), GroupRolling::Rolling { deadline: Some(160) }));

        // A still-recording take wins over any dated sibling: "still going" is
        // the honest answer for the broadcast.
        let mut live = ended(60, 0, 0);
        live.ended_at = None;
        let g = group(vec![ended(60, 0, 0), live]);
        assert!(matches!(group_rolling(&g), GroupRolling::Rolling { deadline: None }));

        // All kept → Kept; all expired → Expired; a mix keeps the broadcast
        // "kept", since one kept take still means media on disk.
        assert!(matches!(group_rolling(&group(vec![ended(60, 5, 0)])), GroupRolling::Kept));
        assert!(matches!(group_rolling(&group(vec![ended(60, 0, 9)])), GroupRolling::Expired));
        assert!(matches!(
            group_rolling(&group(vec![ended(60, 5, 0), ended(60, 0, 9)])),
            GroupRolling::Kept
        ));

        // One counting take outranks kept/expired siblings — something is still
        // at risk, so the row belongs in the rolling section.
        assert!(matches!(
            group_rolling(&group(vec![ended(60, 5, 0), ended(60, 0, 0)])),
            GroupRolling::Rolling { .. }
        ));
    }

    #[test]
    fn effective_watch_state_defaults_unwatched() {
        let map = HashMap::new();
        assert_eq!(effective_watch_state(&map, "s1:abc"), ("unwatched", None));
        let mut map = HashMap::new();
        map.insert("s1:abc".to_string(), ("watched".to_string(), Some(1000)));
        assert_eq!(effective_watch_state(&map, "s1:abc"), ("watched", Some(1000)));
    }

    #[test]
    fn advances_from_unwatched_and_skipped_only() {
        assert!(should_advance_to_started(None));
        assert!(should_advance_to_started(Some("unwatched")));
        assert!(should_advance_to_started(Some("skipped")));
        assert!(!should_advance_to_started(Some("started")));
        assert!(!should_advance_to_started(Some("watched")));
    }

    #[test]
    fn vod_predicates() {
        let mut r = rec("C:/out/stream.mkv");
        r.vod_state = Some("not_published".into());
        assert!(is_missing_vod(&r));
        assert!(!is_vod_pending(&r));

        r.vod_state = Some("pending".into());
        assert!(is_vod_pending(&r));
        assert!(!is_missing_vod(&r));

        r.vod_muted_secs = Some(30);
        assert!(is_muted_vod(&r));
        r.vod_muted_secs = Some(0);
        assert!(!is_muted_vod(&r));
    }

    #[test]
    fn remux_predicates_key_on_cache_dir_and_extension() {
        let pending = rec("C:/out/.sa-cache/stream.ts");
        assert!(is_remux_pending(&pending));
        assert!(!is_remuxed(&pending));

        let done = rec("C:/out/stream.mkv");
        assert!(is_remuxed(&done));
        assert!(!is_remux_pending(&done));

        let mut stuck = rec("C:/out/.sa-cache/stream.mkv");
        stuck.status = "completed".into();
        assert!(is_stuck_in_cache(&stuck));
        assert!(!is_remux_pending(&stuck)); // not a .ts
    }

    #[test]
    fn chapters_and_failure_predicates() {
        let mut r = rec("C:/out/stream.mkv");
        r.chapters_state = "done".into();
        assert!(is_chapters_embedded(&r));
        assert!(!is_chapters_pending(&r));

        r.chapters_state = "queued".into();
        assert!(is_chapters_pending(&r));
        assert!(!is_chapters_embedded(&r));

        r.status = "failed".into();
        r.err_ack = false;
        assert!(is_failed_unacked(&r));
        r.err_ack = true;
        assert!(!is_failed_unacked(&r));
    }
}
