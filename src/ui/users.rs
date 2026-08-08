//! Users view: everything the archive knows about one chatter.
//!
//! The per-channel user Properties popup (`properties.rs`) answers "what has
//! *this channel* recorded about this name". This view answers the wider
//! question — who is this person across every channel we capture, which streams
//! were they in, what did they say, what did they give, and what did moderators
//! do about it.
//!
//! Three sources meet here:
//!
//! * [`crate::chat_index`] — presence and messages, the part that used to
//!   require reading 2.68 GB of sidecars to answer.
//! * [`crate::store`] — `stream_event` contributions and moderation, and the
//!   take/channel names the index's recording ids resolve to.
//! * The platform, live — a Helix profile lookup for Twitch identities.
//!
//! Everything is loaded **on selection**, never per frame: this is history
//! about a person and it does not move while you read it. That also keeps the
//! index's lock off the render path entirely.

use super::*;

use crate::chat_index::{MessageHit, UserRow, UserStreamRow};

/// How many identities the search box offers at once. A name fragment can match
/// thousands of chatters; the list is a picker, not a census.
const SEARCH_LIMIT: i64 = 200;
/// How many of a chatter's streams the detail panel lists.
const STREAMS_LIMIT: i64 = 500;
/// How many messages one page shows — enough to scroll through a stream's worth
/// without turning the panel into a second chat replay.
const MESSAGES_LIMIT: i64 = 500;
/// Cap on the moderation log, matching the popup's.
const MODERATION_LIMIT: i64 = 200;
/// Cap on a global text search.
const SEARCH_MESSAGES_LIMIT: i64 = 500;

/// Which half of the detail panel is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum UserTab {
    Streams,
    Messages,
    Contributions,
    Moderation,
}

impl UserTab {
    const ALL: [UserTab; 4] =
        [UserTab::Streams, UserTab::Messages, UserTab::Contributions, UserTab::Moderation];

    fn label(self) -> &'static str {
        match self {
            UserTab::Streams => "📺 Streams",
            UserTab::Messages => "💬 Messages",
            UserTab::Contributions => "💎 Contributions",
            UserTab::Moderation => "🔨 Moderation",
        }
    }

    fn hover(self) -> &'static str {
        match self {
            UserTab::Streams => {
                "Every stream this chatter was in, newest first, with their message count. \
                 Click a row to open that take's chat replay with them highlighted."
            }
            UserTab::Messages => {
                "Everything this chatter said, newest first, with a full-text filter scoped \
                 to them."
            }
            UserTab::Contributions => {
                "Bits, subs, gift subs and raids, grouped by channel. Matched by display \
                 name within each channel's own event log, so a rename can leave older ones \
                 under the old name."
            }
            UserTab::Moderation => {
                "Timeouts, bans, deleted messages and removals recorded against this \
                 chatter, and which channel each happened in."
            }
        }
    }
}

/// Everything loaded for the selected identity, snapshotted at selection time.
pub(super) struct UserDetail {
    pub(super) user: UserRow,
    /// Every display name we have seen for this identity, including the ones
    /// merged in from login-keyed rows.
    pub(super) aliases: Vec<String>,
    /// How many of their streams were attributed by name rather than id.
    pub(super) name_matched_streams: i64,
    pub(super) streams: Vec<UserStreamRow>,
    /// Take/channel names for `streams`, resolved from the main database.
    pub(super) labels: std::collections::HashMap<i64, crate::store::TakeLabel>,
    pub(super) messages: Vec<MessageHit>,
    /// The filter `messages` was loaded with, so the box and the list agree.
    pub(super) message_filter: String,
    /// Per-channel contribution lines, keyed by channel name.
    pub(super) contributions: Vec<(String, Vec<String>)>,
    pub(super) moderation: Vec<crate::models::StreamEventRow>,
    pub(super) summary: crate::models::ModerationSummary,
    /// Channels the moderation rows came from, so the list can say whose.
    pub(super) mod_channels: std::collections::HashMap<i64, String>,
}

impl StreamArchiverApp {
    /// The Users view: a search box, a list of matching identities, and one
    /// identity's full record.
    pub(super) fn users_view(&mut self, ui: &mut egui::Ui) {
        let Some(index) = crate::chat_index::shared() else {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("The chat index could not be opened.").strong(),
                );
                ui.label(
                    egui::RichText::new(
                        "Without it there is no way to look a chatter up. \
                         The app log says why; the index rebuilds itself from the chat \
                         logs once the problem is fixed.",
                    )
                    .weak(),
                );
            });
            return;
        };

        if !crate::chat_scan::index_enabled(&self.core.store) {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("⚠").color(grid::HL_WARN_TEXT));
                ui.label(
                    egui::RichText::new(
                        "Chat indexing is switched off — what follows is whatever was \
                         indexed before it was disabled.",
                    )
                    .weak(),
                );
                if ui
                    .button("Settings ▸")
                    .on_hover_text("Open Settings, where chat indexing can be switched back on")
                    .clicked()
                {
                    self.view = View::Settings;
                }
            });
            ui.separator();
        }

        // ── Search ───────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.label("🔍");
            let resp = ui
                .add(
                    egui::TextEdit::singleline(&mut self.users_query)
                        .hint_text("chatter name, login, or platform id")
                        .desired_width(260.0),
                )
                .on_hover_text(
                    "Find a chatter by display name, Twitch login, or an exact platform id \
                     (a Twitch user-id or a YouTube UC… channel id). Old names still work: \
                     a chatter who has been renamed is found under either.",
                );
            if resp.changed() {
                self.users_search_dirty = true;
            }
            if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || (self.users_search_dirty && resp.changed())
            {
                self.reload_user_search();
            }
            if ui
                .button("Search")
                .on_hover_text("Search the chat index for matching chatters")
                .clicked()
            {
                self.reload_user_search();
            }
            ui.separator();
            ui.label("💬");
            let hit = ui
                .add(
                    egui::TextEdit::singleline(&mut self.users_text_query)
                        .hint_text("search every message")
                        .desired_width(220.0),
                )
                .on_hover_text(
                    "Full-text search across every indexed chat message, from every channel. \
                     Words are matched as typed; add * for a prefix search (poggers*).",
                );
            if (hit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || ui
                    .button("Find")
                    .on_hover_text("Search all indexed messages for this text")
                    .clicked()
            {
                self.reload_global_message_search();
            }
        });

        // ── Index one channel on demand ──────────────────────────────────
        // Looking someone up in a channel whose logs haven't been read yet is
        // exactly when the background trickle feels too slow, so the shortcut
        // lives here rather than buried in Settings.
        ui.horizontal_wrapped(|ui| {
            let selected = self
                .users_scan_channel
                .and_then(|id| self.channels.iter().find(|c| c.id == id))
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "(pick a channel)".to_string());
            egui::ComboBox::from_id_salt("users_scan_channel")
                .selected_text(selected)
                .width(180.0)
                .show_ui(ui, |ui| {
                    for c in &self.channels {
                        ui.selectable_value(&mut self.users_scan_channel, Some(c.id), &c.name);
                    }
                })
                .response
                .on_hover_text(
                    "Read one channel's chat logs ahead of the background queue — useful when \
                     you're looking someone up and that channel's streams haven't been \
                     indexed yet.",
                );
            ui.add(
                egui::DragValue::new(&mut self.users_scan_count)
                    .range(1..=500)
                    .prefix("last "),
            )
            .on_hover_text("How many of that channel's most recent chat logs to read.");
            let busy = self.users_scan_running;
            let can = self.users_scan_channel.is_some() && !busy;
            if ui
                .add_enabled(can, egui::Button::new("Index those chat logs"))
                .on_hover_text(
                    "Read those chat logs now instead of waiting for the background sweep. \
                     Still queues behind any capture using the same drive, and skips logs \
                     already indexed.",
                )
                .clicked()
                && let Some(cid) = self.users_scan_channel
            {
                self.spawn_channel_index(ui.ctx(), cid, self.users_scan_count as usize);
            }
            if busy {
                ui.spinner();
                ui.label(egui::RichText::new("reading…").small().weak());
            }
            // Take the guard's value out before touching `self` again — holding
            // a lock on one of our own fields across a `&mut self` call is a
            // borrow error, and dropping it first is also simply correct.
            let finished = self.users_scan_done.lock().unwrap().take();
            if let Some(done) = finished {
                self.users_scan_running = false;
                self.status = done;
                self.reload_user_search();
            }
        });
        ui.separator();

        if let Some(err) = self.users_error.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(grid::HL_ERROR_TEXT, &err);
                if ui.button("✖").on_hover_text("Dismiss").clicked() {
                    self.users_error = None;
                }
            });
        }

        // Index coverage, stated plainly — an empty result means something very
        // different before the backlog has drained than after.
        if let Ok(h) = index.health() {
            let remaining = self.users_takes_total.saturating_sub(h.takes_indexed + h.takes_failed);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} chatters · {} messages · {} streams indexed",
                        thousands(h.users),
                        thousands(h.messages),
                        thousands(h.takes_indexed)
                    ))
                    .small()
                    .weak(),
                );
                if remaining > 0 {
                    ui.label(
                        egui::RichText::new(format!("· {} still to read", thousands(remaining)))
                            .small()
                            .color(grid::HL_WARN_TEXT),
                    )
                    .on_hover_text(
                        "Chat logs are read a few at a time in the background, behind any \
                         recording that is using the disk. Until that finishes, a chatter may \
                         be missing streams they were really in — so an empty result here is \
                         not yet proof of absence.",
                    );
                    if ui
                        .small_button("Read them all now")
                        .on_hover_text(
                            "Index the remaining chat logs as fast as the disk gate allows \
                             instead of a few a minute. Still yields to any running capture.",
                        )
                        .clicked()
                    {
                        self.settings.chat_index_batch =
                            crate::chat_scan::INDEX_BATCH_MAX.to_string();
                        self.settings.chat_index_enabled = true;
                        let ctx = ui.ctx().clone();
                        self.save_settings(&ctx);
                        self.status = "Chat indexing set to full speed.".to_string();
                    }
                }
            });
        }
        ui.separator();

        // ── Global message search results ────────────────────────────────
        if !self.users_text_hits.is_empty() || self.users_text_searched {
            self.global_message_results(ui);
            return;
        }

        if self.users_results.is_empty() && self.users_searched {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("No chatter matches that.").weak());
                ui.label(
                    egui::RichText::new(
                        "Only people who actually said something are indexed — lurkers leave \
                         no trace in a chat log.",
                    )
                    .small()
                    .weak(),
                );
            });
            return;
        }

        egui::Panel::left("users_list")
            .resizable(true)
            .default_size(240.0)
            .show_inside(ui, |ui| self.user_list(ui));

        match self.users_detail.take() {
            Some(mut detail) => {
                self.user_detail_panel(ui, &mut detail);
                self.users_detail = Some(detail);
            }
            None => {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Pick a chatter to see their record.").weak());
                });
            }
        }
    }

    /// The left-hand result list.
    fn user_list(&mut self, ui: &mut egui::Ui) {
        let mut select: Option<i64> = None;
        egui::ScrollArea::vertical().id_salt("users_result_list").show(ui, |ui| {
            for u in &self.users_results {
                let selected = self.users_selected == Some(u.id);
                let color = crate::ui::chat::readable_color(
                    crate::ui::chat::twitch_username_color(&u.display),
                    ui.visuals().panel_fill,
                );
                let resp = ui.selectable_label(
                    selected,
                    egui::RichText::new(display_or_key(u)).color(color),
                );
                let resp = resp.on_hover_text(format!(
                    "{} · {} message(s) across {} stream(s){}",
                    platform_label(&u.platform),
                    thousands(u.msgs_total),
                    thousands(u.streams_total),
                    if u.name_matched {
                        "\n\nIdentified by name only — this chatter's logs predate \
                         platform-id capture, so a since-renamed account could be someone else."
                    } else {
                        ""
                    }
                ));
                if resp.clicked() {
                    select = Some(u.id);
                }
                ui.label(
                    egui::RichText::new(format!(
                        "   {} msgs · {} streams",
                        thousands(u.msgs_total),
                        thousands(u.streams_total)
                    ))
                    .small()
                    .weak(),
                );
            }
        });
        if let Some(id) = select {
            self.select_user(id);
        }
    }

    /// The right-hand detail panel for one identity.
    fn user_detail_panel(&mut self, ui: &mut egui::Ui, detail: &mut UserDetail) {
        let color = crate::ui::chat::readable_color(
            crate::ui::chat::twitch_username_color(&detail.user.display),
            ui.visuals().panel_fill,
        );
        crate::ui::chat::paint_user_banner(
            ui,
            crate::ui::chat::twitch_username_color(&detail.user.display),
            32.0,
        );
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(display_or_key(&detail.user)).strong().size(17.0).color(color),
            );
            ui.label(egui::RichText::new(platform_label(&detail.user.platform)).small().weak());
            if ui
                .button("📋")
                .on_hover_text("Copy this chatter's name to the clipboard")
                .clicked()
            {
                ui.ctx().copy_text(detail.user.display.clone());
            }
            if detail.user.platform == "twitch"
                && !detail.user.login.is_empty()
                && ui
                    .button("🔗 Twitch")
                    .on_hover_text("Open twitch.tv/{login} in your browser")
                    .clicked()
            {
                crate::platform::open_url(&format!("https://twitch.tv/{}", detail.user.login));
            }
            if detail.user.platform == "youtube"
                && !detail.user.name_matched
                && ui
                    .button("🔗 YouTube")
                    .on_hover_text("Open this chatter's YouTube channel in your browser")
                    .clicked()
            {
                crate::platform::open_url(&format!(
                    "https://www.youtube.com/channel/{}",
                    detail.user.key
                ));
            }
        });

        // Identity line: what we key them on, and how sure that is.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if detail.user.name_matched {
                ui.label(egui::RichText::new("⚠ name-matched").color(grid::HL_WARN_TEXT).small())
                    .on_hover_text(
                        "This chatter is identified by name, not by a platform id: their chat \
                         logs predate id capture (2026-08-05 for Twitch). A login points at \
                         whoever holds that name TODAY, so if this account has been renamed \
                         since, some of this history may belong to someone else.",
                    );
            } else {
                ui.label(egui::RichText::new(format!("id {}", detail.user.key)).small().weak())
                    .on_hover_text(
                        "The platform's own account id. Stable across renames — everything \
                         filed under it is genuinely the same account.",
                    );
            }
            if detail.name_matched_streams > 0 && !detail.user.name_matched {
                ui.label(
                    egui::RichText::new(format!(
                        "· {} stream(s) folded in by name",
                        detail.name_matched_streams
                    ))
                    .small()
                    .color(grid::HL_WARN_TEXT),
                )
                .on_hover_text(
                    "Some of this chatter's older streams were matched to this account by \
                     login rather than by id, because the logs predate id capture. If they \
                     had a different name back then, those streams may belong to someone else.",
                );
            }
            if detail.aliases.len() > 1 {
                ui.label(
                    egui::RichText::new(format!("· also seen as {}", detail.aliases.join(", ")))
                        .small()
                        .weak(),
                )
                .on_hover_text("Every display name this identity has been recorded under.");
            }
        });
        ui.label(
            egui::RichText::new(format!(
                "{} message(s) across {} stream(s) · first seen {} · last seen {}",
                thousands(detail.user.msgs_total),
                thousands(detail.user.streams_total),
                fmt_datetime_short(detail.user.first_seen),
                fmt_datetime_short(detail.user.last_seen),
            ))
            .small()
            .weak(),
        )
        .on_hover_text(
            "Counted across every channel we archive — not what the platform says, but what \
             our own chat logs contain.",
        );
        ui.separator();

        ui.horizontal(|ui| {
            for tab in UserTab::ALL {
                let count = match tab {
                    UserTab::Streams => detail.streams.len(),
                    UserTab::Messages => detail.messages.len(),
                    UserTab::Contributions => detail.contributions.len(),
                    UserTab::Moderation => detail.moderation.len(),
                };
                let label = if count > 0 {
                    format!("{} ({count})", tab.label())
                } else {
                    tab.label().to_string()
                };
                if ui
                    .selectable_label(self.users_tab == tab, label)
                    .on_hover_text(tab.hover())
                    .clicked()
                {
                    self.users_tab = tab;
                }
            }
        });
        ui.separator();

        match self.users_tab {
            UserTab::Streams => self.user_streams_tab(ui, detail),
            UserTab::Messages => self.user_messages_tab(ui, detail),
            UserTab::Contributions => user_contributions_tab(ui, detail),
            UserTab::Moderation => user_moderation_tab(ui, detail),
        }
    }

    fn user_streams_tab(&mut self, ui: &mut egui::Ui, detail: &UserDetail) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        if detail.streams.is_empty() {
            ui.label(egui::RichText::new("No indexed streams for this chatter.").weak());
            return;
        }
        let mut open: Option<i64> = None;
        egui::ScrollArea::vertical().id_salt("user_streams").show(ui, |ui| {
            egui::Grid::new("user_streams_grid").num_columns(4).striped(true).show(ui, |ui| {
                for s in &detail.streams {
                    let label = detail.labels.get(&s.rec_id);
                    let channel =
                        label.map(|l| l.channel.as_str()).unwrap_or("(recording deleted)");
                    ui.label(channel);
                    ui.label(
                        egui::RichText::new(fmt_datetime_short(s.first_at)).monospace().small(),
                    );
                    ui.label(egui::RichText::new(format!("{} msgs", thousands(s.msgs))).small())
                        .on_hover_text(format!(
                            "First message {} · last {}",
                            fmt_datetime_short(s.first_at),
                            fmt_datetime_short(s.last_at)
                        ));
                    let title = label.map(|l| l.title.as_str()).unwrap_or("");
                    if ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(if title.is_empty() { "(no title)" } else { title })
                                    .small()
                                    .weak(),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_text(
                            "Open this take's chat replay, positioned at this chatter's first \
                             message.",
                        )
                        .clicked()
                    {
                        open = Some(s.rec_id);
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(rec_id) = open
            && let Some(s) = detail.streams.iter().find(|s| s.rec_id == rec_id)
        {
            self.open_chat_at(ctx, rec_id, s.first_at, &detail.user.display);
        }
    }

    fn user_messages_tab(&mut self, ui: &mut egui::Ui, detail: &mut UserDetail) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        ui.horizontal(|ui| {
            ui.label("Filter:");
            let resp = ui
                .add(
                    egui::TextEdit::singleline(&mut self.users_msg_filter)
                        .hint_text("words in their messages")
                        .desired_width(240.0),
                )
                .on_hover_text(
                    "Full-text search within this chatter's messages only. Words are matched \
                     as typed; add * for a prefix search.",
                );
            if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || ui.button("Apply").on_hover_text("Re-run the search").clicked()
            {
                self.reload_user_messages(detail);
            }
            if !detail.message_filter.is_empty()
                && ui.button("✖").on_hover_text("Clear the filter").clicked()
            {
                self.users_msg_filter.clear();
                self.reload_user_messages(detail);
            }
        });
        ui.separator();
        if detail.messages.is_empty() {
            ui.label(
                egui::RichText::new(if detail.message_filter.is_empty() {
                    "No messages indexed for this chatter."
                } else {
                    "No messages match that."
                })
                .weak(),
            );
            return;
        }
        let mut open: Option<(i64, i64)> = None;
        egui::ScrollArea::vertical().id_salt("user_messages").show(ui, |ui| {
            for m in &detail.messages {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(
                        egui::RichText::new(fmt_datetime_short(m.at)).monospace().small().weak(),
                    );
                    if let Some(l) = detail.labels.get(&m.rec_id)
                        && ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new(format!("[{}]", l.channel)).small().weak(),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text("Open this take's chat replay at this message")
                            .clicked()
                    {
                        open = Some((m.rec_id, m.at));
                    }
                    ui.label(&m.text);
                });
            }
        });
        if let Some((rec_id, at)) = open {
            self.open_chat_at(ctx, rec_id, at, &detail.user.display);
        }
    }

    /// Results of the whole-archive text search.
    fn global_message_results(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        ui.horizontal(|ui| {
            if ui
                .button("← Back to chatters")
                .on_hover_text("Leave the message search and go back to the chatter list")
                .clicked()
            {
                self.users_text_hits.clear();
                self.users_text_searched = false;
                self.users_text_query.clear();
            }
            ui.label(
                egui::RichText::new(format!("{} match(es)", thousands(self.users_text_hits.len() as i64)))
                    .small()
                    .weak(),
            );
            if self.users_text_hits.len() as i64 >= SEARCH_MESSAGES_LIMIT {
                ui.label(
                    egui::RichText::new("· showing the newest only")
                        .small()
                        .color(grid::HL_WARN_TEXT),
                )
                .on_hover_text(format!(
                    "The search stops at {SEARCH_MESSAGES_LIMIT} results, newest first. \
                     Narrow the words to see older ones."
                ));
            }
        });
        ui.separator();
        if self.users_text_hits.is_empty() {
            ui.label(egui::RichText::new("Nothing indexed matches that.").weak());
            return;
        }
        let mut pick: Option<i64> = None;
        let mut open: Option<(i64, i64, String)> = None;
        egui::ScrollArea::vertical().id_salt("global_msg_hits").show(ui, |ui| {
            for m in &self.users_text_hits {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(
                        egui::RichText::new(fmt_datetime_short(m.at)).monospace().small().weak(),
                    );
                    if let Some(l) = self.users_text_labels.get(&m.rec_id)
                        && ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new(format!("[{}]", l.channel)).small().weak(),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text("Open this take's chat replay at this message")
                            .clicked()
                    {
                        open = Some((m.rec_id, m.at, m.display.clone()));
                    }
                    let color = crate::ui::chat::readable_color(
                        crate::ui::chat::twitch_username_color(&m.display),
                        ui.visuals().panel_fill,
                    );
                    if ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(format!("{}:", m.display)).color(color),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_text("Open this chatter's full record")
                        .clicked()
                    {
                        pick = Some(m.user_id);
                    }
                    ui.label(&m.text);
                });
            }
        });
        if let Some((rec_id, at, who)) = open {
            self.open_chat_at(ctx, rec_id, at, &who);
        }
        if let Some(id) = pick {
            self.users_text_hits.clear();
            self.users_text_searched = false;
            self.select_user(id);
        }
    }

    /// Open a take's chat replay with this chatter highlighted.
    ///
    /// Jumping to an exact message in a five-hour log would land you in a wall
    /// of strangers' text; highlighting the person instead makes every line of
    /// theirs findable by scrolling, which is what "show me this in context"
    /// actually means. `at` names the moment the caller cared about — the
    /// replay's own scrollback is the navigation.
    fn open_chat_at(&mut self, ctx: &egui::Context, rec_id: i64, at: i64, login: &str) {
        let Some(label) = self
            .users_text_labels
            .get(&rec_id)
            .or_else(|| self.users_detail.as_ref().and_then(|d| d.labels.get(&rec_id)))
            .cloned()
        else {
            self.users_error = Some(
                "That recording is no longer in the database — its chat replay can't be opened."
                    .to_string(),
            );
            return;
        };
        self.open_chat_popup(label.monitor_id, Some(rec_id), ctx);
        // Highlight this chatter in the freshly-opened replay (the newest popup
        // is the one we just pushed).
        if let Some(popup) = self.chat_popups.last() {
            popup.lock().unwrap().highlight_login = Some(login.to_lowercase());
        }
        let _ = at;
    }

    /// Read one channel's most recent chat logs on a background task.
    fn spawn_channel_index(&mut self, ctx: &egui::Context, channel_id: i64, limit: usize) {
        let Some(index) = crate::chat_index::shared().cloned() else {
            self.users_error = Some("The chat index is not available.".to_string());
            return;
        };
        self.users_scan_running = true;
        let store = self.core.store.clone();
        let done = self.users_scan_done.clone();
        let ctx = ctx.clone();
        let name = self
            .channels
            .iter()
            .find(|c| c.id == channel_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        self.core.rt.spawn(async move {
            let now = crate::models::now_unix();
            let (read, msgs, failed) =
                crate::chat_scan::index_channel_now(&store, &index, channel_id, limit, now).await;
            let mut line = if read == 0 && failed == 0 {
                format!("{name}: every chat log was already indexed.")
            } else {
                format!("{name}: read {read} chat log(s), {msgs} message(s) indexed.")
            };
            if failed > 0 {
                line.push_str(&format!(" {failed} could not be read (missing or unreadable)."));
            }
            *done.lock().unwrap() = Some(line);
            ctx.request_repaint();
        });
    }

    // ----- loading -----

    /// Run the identity search and reset the detail panel.
    pub(super) fn reload_user_search(&mut self) {
        self.users_search_dirty = false;
        self.users_searched = !self.users_query.trim().is_empty();
        self.users_text_hits.clear();
        self.users_text_searched = false;
        let Some(index) = crate::chat_index::shared() else { return };
        self.users_results = index.find_users(&self.users_query, SEARCH_LIMIT).unwrap_or_default();
        self.users_takes_total = self.core.store.chat_index_candidates().map(|v| v.len() as i64).unwrap_or(0);
        // Keep the selection only if it survived the new search.
        if let Some(sel) = self.users_selected
            && self.users_results.iter().any(|u| u.id == sel)
        {
            return;
        }
        self.users_selected = None;
        self.users_detail = None;
        if let Some(first) = self.users_results.first().map(|u| u.id) {
            self.select_user(first);
        }
    }

    /// Load one identity's whole record. Every query runs here, once — nothing
    /// below this point touches the database on a render pass.
    pub(super) fn select_user(&mut self, user_id: i64) {
        let Some(index) = crate::chat_index::shared() else { return };
        self.users_selected = Some(user_id);
        self.users_msg_filter.clear();
        let Ok(Some(user)) = index.user(user_id) else {
            self.users_detail = None;
            return;
        };
        let streams = index.user_streams(user_id, STREAMS_LIMIT).unwrap_or_default();
        let messages = index.user_messages(user_id, "", MESSAGES_LIMIT).unwrap_or_default();
        let aliases = index.aliases(user_id).unwrap_or_default();
        let name_matched_streams = index.name_matched_streams(user_id).unwrap_or(0);

        // Resolve every recording id the two lists mention in one query.
        let mut ids: Vec<i64> = streams.iter().map(|s| s.rec_id).collect();
        ids.extend(messages.iter().map(|m| m.rec_id));
        ids.sort_unstable();
        ids.dedup();
        let labels = self.core.store.take_labels(&ids).unwrap_or_default();

        // Contributions and moderation live in the main database, keyed by
        // display name — so they are per-channel, and only for the channels this
        // chatter actually appeared in.
        let mut channel_ids: Vec<i64> = streams.iter().map(|s| s.channel_id).collect();
        channel_ids.sort_unstable();
        channel_ids.dedup();
        let now = crate::models::now_unix();
        let mut contributions = Vec::new();
        let mut moderation: Vec<crate::models::StreamEventRow> = Vec::new();
        let mut mod_channels = std::collections::HashMap::new();
        for cid in channel_ids {
            let channel = self
                .channels
                .iter()
                .find(|c| c.id == cid)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("channel {cid}"));
            let events = self.core.store.stream_events_range(cid, 0, now).unwrap_or_default();
            let lines = crate::ui::chat::summarize_user_events(&events, &user.display);
            if !lines.is_empty() {
                contributions.push((channel.clone(), lines));
            }
            let mods = self
                .core
                .store
                .moderation_events_for_user(cid, &user.display, &user.key, MODERATION_LIMIT)
                .unwrap_or_default();
            for m in &mods {
                mod_channels.insert(m.monitor_id, channel.clone());
            }
            moderation.extend(mods);
        }
        moderation.sort_by(|a, b| b.at.cmp(&a.at));
        let summary = crate::models::ModerationSummary::from_events(&moderation);

        self.users_detail = Some(UserDetail {
            user,
            aliases,
            name_matched_streams,
            streams,
            labels,
            messages,
            message_filter: String::new(),
            contributions,
            moderation,
            summary,
            mod_channels,
        });
    }

    /// Re-run the per-user message filter.
    fn reload_user_messages(&mut self, detail: &mut UserDetail) {
        let Some(index) = crate::chat_index::shared() else { return };
        let filter = self.users_msg_filter.trim().to_string();
        detail.messages =
            index.user_messages(detail.user.id, &filter, MESSAGES_LIMIT).unwrap_or_default();
        detail.message_filter = filter;
        let mut ids: Vec<i64> = detail.messages.iter().map(|m| m.rec_id).collect();
        ids.sort_unstable();
        ids.dedup();
        // Merge rather than replace: the streams tab's labels are still needed.
        if let Ok(more) = self.core.store.take_labels(&ids) {
            detail.labels.extend(more);
        }
    }

    /// Run the whole-archive text search.
    fn reload_global_message_search(&mut self) {
        let Some(index) = crate::chat_index::shared() else { return };
        let q = self.users_text_query.trim().to_string();
        self.users_text_searched = !q.is_empty();
        if q.is_empty() {
            self.users_text_hits.clear();
            return;
        }
        self.users_text_hits =
            index.search_messages(&q, SEARCH_MESSAGES_LIMIT).unwrap_or_default();
        let mut ids: Vec<i64> = self.users_text_hits.iter().map(|m| m.rec_id).collect();
        ids.sort_unstable();
        ids.dedup();
        self.users_text_labels = self.core.store.take_labels(&ids).unwrap_or_default();
    }
}

fn user_contributions_tab(ui: &mut egui::Ui, detail: &UserDetail) {
    if detail.contributions.is_empty() {
        ui.label(
            egui::RichText::new("No bits, gift subs or raids on record for this chatter.").weak(),
        );
        ui.label(
            egui::RichText::new(
                "Contributions are matched by display name within each channel's own event \
                 log, so a chatter who has been renamed may have older ones filed under the \
                 old name.",
            )
            .small()
            .weak(),
        );
        return;
    }
    egui::ScrollArea::vertical().id_salt("user_contributions").show(ui, |ui| {
        for (channel, lines) in &detail.contributions {
            ui.label(egui::RichText::new(channel).strong());
            for line in lines {
                ui.label(format!("   {line}"));
            }
            ui.add_space(4.0);
        }
    });
}

fn user_moderation_tab(ui: &mut egui::Ui, detail: &UserDetail) {
    let now = crate::models::now_unix();
    let (line, warn) =
        crate::ui::chat::moderation_state_line(detail.summary.state(&detail.moderation), now);
    if warn {
        ui.colored_label(grid::HL_ERROR_TEXT, line);
    } else {
        ui.label(line);
    }
    ui.label(
        egui::RichText::new(
            "Captured passively from chat: neither platform says who moderated, why, or when \
             someone was un-banned — this is only what was last seen, per channel.",
        )
        .small()
        .weak(),
    );
    if detail.summary.is_clean() {
        return;
    }
    let s = detail.summary;
    let mut parts: Vec<String> = Vec::new();
    if s.deleted > 0 {
        parts.push(format!("{} message(s) deleted", s.deleted));
    }
    if s.timeouts > 0 {
        parts.push(format!("{} timeout(s)", s.timeouts));
    }
    if s.bans > 0 {
        parts.push(format!("{} ban(s)", s.bans));
    }
    if s.purges > 0 {
        parts.push(format!("{} removal(s)", s.purges));
    }
    ui.separator();
    ui.label(egui::RichText::new(parts.join(" · ")).small().weak());
    egui::ScrollArea::vertical().id_salt("user_moderation").show(ui, |ui| {
        for e in &detail.moderation {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                ui.label(egui::RichText::new(fmt_datetime_short(e.at)).monospace().small().weak());
                if let Some(ch) = detail.mod_channels.get(&e.monitor_id) {
                    ui.label(egui::RichText::new(format!("[{ch}]")).small().weak());
                }
                ui.label(egui::RichText::new(crate::ui::chat::moderation_event_line(e)).small());
            });
        }
    });
}

/// What to call an identity in a list. A YouTube chatter who never said
/// anything under a name still has a channel id, and showing that beats
/// showing nothing.
fn display_or_key(u: &UserRow) -> String {
    if !u.display.is_empty() {
        return u.display.clone();
    }
    if !u.login.is_empty() {
        return u.login.clone();
    }
    u.key.clone()
}

fn platform_label(platform: &str) -> &'static str {
    match platform {
        "twitch" => "Twitch",
        "youtube" => "YouTube",
        "kick" => "Kick",
        _ => "chat",
    }
}

/// Thousands separators — a chatter with 40,000 messages should not read as
/// "40000".
fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1 000");
        assert_eq!(thousands(40_123), "40 123");
        assert_eq!(thousands(1_234_567), "1 234 567");
        assert_eq!(thousands(-1_500), "-1 500");
    }

    #[test]
    fn a_nameless_identity_still_has_something_to_show() {
        let row = |display: &str, login: &str, key: &str| UserRow {
            id: 1,
            platform: "youtube".into(),
            key: key.into(),
            login: login.into(),
            display: display.into(),
            first_seen: 0,
            last_seen: 0,
            msgs_total: 0,
            streams_total: 0,
            name_matched: false,
        };
        assert_eq!(display_or_key(&row("Ann", "ann", "UCx")), "Ann");
        assert_eq!(display_or_key(&row("", "ann", "UCx")), "ann");
        assert_eq!(display_or_key(&row("", "", "UCx")), "UCx");
    }
}
