//! The info strips above the message list: top supporters, the Hype
//! Train, and the users-in-chat panel's grouping.

use super::*;

/// Twitch's Hype Train accent, shared by the strip's icon and its progress
/// bar — the same pink the Channel Stats event plot gives `hype_train` rows,
/// so one broadcast reads the same in both views.
pub(in crate::ui) const HYPE_COLOR: egui::Color32 = egui::Color32::from_rgb(0xff, 0x40, 0x81);

/// One info card above the message list.
///
/// Twitch's popout gets its "smooth and flowy" feel from translucent rounded
/// panels that sit *on* the chat rather than boxing it in — so this is a
/// semi-transparent fill over the window background with rounded corners and
/// no border, instead of `Frame::group`'s hard 1px outline. The alpha is what
/// does the work: a fully opaque card reads as a separate widget bolted on
/// top, a translucent one reads as part of the same surface.
pub(in crate::ui) fn chat_card<R>(
    ui: &mut egui::Ui,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let r = egui::Frame::new()
        .fill(card_fill(ui.visuals()))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(9, 6))
        .show(ui, add);
    ui.add_space(4.0);
    r
}

/// The card's translucent fill: the panel colour lifted (dark mode) or
/// deepened (light mode), then made partly transparent.
///
/// Derived from the theme rather than hardcoded so a restyled or light-mode
/// app doesn't end up with a card that clashes with everything around it.
/// The alpha is the part that matters — an opaque card reads as a widget
/// bolted on top of the chat, a translucent one reads as part of the same
/// surface, which is exactly the difference the official popout has.
fn card_fill(v: &egui::Visuals) -> egui::Color32 {
    const ALPHA: u8 = 235;
    let p = v.panel_fill;
    let shift = |c: u8| {
        if v.dark_mode { c.saturating_add(16) } else { c.saturating_sub(14) }
    };
    egui::Color32::from_rgba_unmultiplied(shift(p.r()), shift(p.g()), shift(p.b()), ALPHA)
}

/// The most recent Hype Train for a broadcast, everything the chat replay
/// needs to draw a Twitch-style progress bar (or, once it's over, a static
/// reached-level summary) — see [`load_broadcast_stats`]'s doc for where
/// `goal`/`expires_at` come from.
pub(in crate::ui) struct HypeTrainDisplay {
    /// Pre-formatted line (`detectors::HypeTrainState::detail()`), shown
    /// as-is once the train's no longer running (or `goal`/`expires_at`
    /// weren't captured — pre-v86 rows, or an inference-only "(inferred)"
    /// row that GQL never confirmed).
    pub(in crate::ui) detail: String,
    /// Which train this is (the event row's `tier`: a GQL execution id,
    /// `manual:<ts>`, or `""` for an inferred one). What makes "a NEW train
    /// started" answerable — without it, a fresh train and a poll update of
    /// the running one look identical.
    pub(in crate::ui) train_id: String,
    pub(in crate::ui) level: i64,
    pub(in crate::ui) total: i64,
    pub(in crate::ui) goal: i64,
    pub(in crate::ui) expires_at: i64,
}

/// How long a finished Hype Train stays on screen, saying so, before the card
/// hides itself. Long enough to notice one ended while you were reading chat;
/// short enough that it isn't still there half an hour later.
pub(in crate::ui) const HYPE_ENDED_GRACE_SECS: i64 = 300;

/// What the Hype Train card should be doing right now.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(in crate::ui) enum HypePhase {
    /// Running: live bar, `frac` of the way to the next level, `remaining`
    /// seconds on the clock.
    Running { frac: f32, remaining: i64 },
    /// Over, within the grace window — the bar stays, greyed, and says so.
    Ended { since_secs: i64 },
    /// No timing to reason about, so just the reached-level summary line:
    /// a pre-v86 row, or an inferred one GQL never confirmed.
    Summary,
    /// Over and past the grace window, on a live view.
    Hidden,
}

/// Decide what to draw for `train`. Pure, so the whole lifecycle is testable
/// without a clock or a UI.
///
/// `live_view` must be true only when the chat window is following a
/// still-running recording. **The auto-hide is a live-view rule and nothing
/// else**: this is an archive tool, so opening the chat for a three-week-old
/// take has to keep showing that the broadcast had a Level 4 train. Hiding it
/// because five minutes of wall-clock time have passed since then would be a
/// straight regression.
///
/// Honest limitation, worth repeating wherever this is surfaced: `expires_at`
/// comes from this app's ~60 s GQL poll and Twitch sends no explicit end
/// event, so "ended" can be announced up to a minute late, and a train that
/// finishes early by completing its last level sits in `Running` until the
/// timer it was last seen with lapses.
pub(in crate::ui) fn hype_phase(train: &HypeTrainDisplay, now: i64, live_view: bool) -> HypePhase {
    if train.goal <= 0 || train.expires_at <= 0 {
        return HypePhase::Summary;
    }
    if now < train.expires_at {
        return HypePhase::Running {
            frac: (train.total as f32 / train.goal as f32).clamp(0.0, 1.0),
            remaining: (train.expires_at - now).max(0),
        };
    }
    let since = now - train.expires_at;
    if since < HYPE_ENDED_GRACE_SECS {
        HypePhase::Ended { since_secs: since }
    } else if live_view {
        HypePhase::Hidden
    } else {
        HypePhase::Summary
    }
}

/// "just now" / "2m ago" / "1h 5m ago" for the ended-train line. Deliberately
/// coarse: this only ever spans the grace window on a live view.
pub(in crate::ui) fn fmt_ago(secs: i64) -> String {
    match secs {
        s if s < 30 => "just now".to_string(),
        s if s < 90 => "1m ago".to_string(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s => format!("{}h {}m ago", s / 3600, (s % 3600) / 60),
    }
}

/// Everything the info strips above the message list draw, loaded in one
/// pass. A struct rather than a tuple because it is about to grow a fourth
/// member (creator goals) and a 4-wide anonymous tuple threaded through
/// three call sites is how fields get silently swapped.
#[derive(Default)]
pub(in crate::ui) struct BroadcastStats {
    pub(in crate::ui) top_gifters: Vec<(String, i64)>,
    pub(in crate::ui) top_cheerers: Vec<(String, i64)>,
    pub(in crate::ui) hype_train: Option<HypeTrainDisplay>,
}

impl BroadcastStats {
    /// Nothing to show — the strips collapse entirely rather than drawing an
    /// empty card.
    pub(in crate::ui) fn is_empty(&self) -> bool {
        self.top_gifters.is_empty() && self.top_cheerers.is_empty() && self.hype_train.is_none()
    }
}

/// This broadcast's top-supporters leaderboard (gift subs / bits, top 5
/// each) and its most recent Hype Train, from the locally-recorded
/// `stream_event` history — purely local DB query, no network, no new
/// capture. Only the LATEST train is returned (a long/generous broadcast
/// can rack up several over its runtime; showing the whole history read as
/// a wall of text with no clear "this one's current" signal). `since`/
/// `until` should be the viewed recording's span — pass `until =
/// now_unix()` for a still-live recording so an in-progress train's latest
/// poll is picked up.
pub(in crate::ui) fn load_broadcast_stats(
    store: &crate::store::Store,
    monitor_id: i64,
    since: i64,
    until: i64,
) -> BroadcastStats {
    let events = store.stream_events_for_monitor_range(monitor_id, since, until).unwrap_or_default();
    BroadcastStats {
        top_gifters: crate::ui::channel_stats::top_contributors(&events, "subgift", 5),
        top_cheerers: crate::ui::channel_stats::top_contributors(&events, "bits", 5),
        hype_train: events
            .iter()
            .filter(|e| e.kind == "hype_train")
            .max_by_key(|e| e.at)
            .map(|e| HypeTrainDisplay {
                detail: e.detail.clone(),
                train_id: e.tier.clone(),
                level: e.level,
                total: e.amount,
                goal: e.goal,
                expires_at: e.expires_at,
            }),
    }
}

/// Draw the info cards above the message list: channel info (top supporters)
/// and the Hype Train. Returns `true` when something on screen is counting —
/// a running train's clock, or an ended one's grace window — so the caller
/// can schedule a repaint.
///
/// **The caller must request that repaint from INSIDE the deferred viewport
/// closure.** Outside it, `ctx`'s current viewport is the root, and the tick
/// would go to the wrong window: the countdown would freeze and the card
/// would never hide itself.
pub(in crate::ui) fn chat_info_cards(
    ui: &mut egui::Ui,
    popup: &mut ChatPopup,
    icons: Option<&UiTextures>,
    live_view: bool,
    now: i64,
) -> bool {
    if popup.stats.is_empty() {
        return false;
    }
    // A train we haven't seen before, still running, re-opens this window's
    // collapse — the user asked to be shown a NEW train even after hiding the
    // last one. Checked here rather than at each of the three places that
    // refresh `stats`, so it can't be forgotten by one of them.
    if let Some(t) = &popup.stats.hype_train
        && popup.hype_seen_id != t.train_id
    {
        popup.hype_seen_id = t.train_id.clone();
        if matches!(hype_phase(t, now, live_view), HypePhase::Running { .. }) {
            popup.show_hype = true;
        }
    }

    let (want_hype, want_info) = {
        let cs = popup.settings.lock().unwrap();
        (cs.show_hype_train, cs.show_channel_info)
    };
    let mut ticking = false;

    if want_info
        && popup.show_info
        && !(popup.stats.top_gifters.is_empty() && popup.stats.top_cheerers.is_empty())
    {
        chat_card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(egui::RichText::new("Top supporters").weak().small());
                let accent = ui.visuals().weak_text_color();
                let entry = |ui: &mut egui::Ui, icon, name: &str, n: i64, hover: &str| {
                    ui_icon(ui, icons, icon, 13.0, accent).on_hover_text(hover);
                    ui.label(format!("{name} ×{n}")).on_hover_text(hover);
                };
                for (name, n) in &popup.stats.top_gifters {
                    entry(ui, ICON_GIFT, name, *n, "Gift subs given this broadcast");
                }
                for (name, n) in &popup.stats.top_cheerers {
                    entry(ui, ICON_GEM, name, *n, "Bits cheered this broadcast");
                }
            });
        });
    }

    if want_hype
        && popup.show_hype
        && let Some(train) = &popup.stats.hype_train
    {
        // Every branch says the same thing about provenance, so say it once.
        const HYPE_HOVER: &str =
            "Reconstructed from this app's own periodic (~60s) anonymous Twitch \
             poll, not a live push update — Twitch sends no explicit start or end \
             event, so the bar can lag a few seconds and \"ended\" can be up to a \
             minute late.";
        let phase = hype_phase(train, now, live_view);
        let bar = |ui: &mut egui::Ui, frac: f32, fill: egui::Color32, text: String| {
            ui.add(
                egui::ProgressBar::new(frac)
                    .text(text)
                    .fill(fill)
                    .corner_radius(3.0)
                    .desired_width(ui.available_width()),
            )
            .on_hover_text(HYPE_HOVER);
        };
        match phase {
            HypePhase::Hidden => {}
            HypePhase::Running { frac, remaining } => {
                ticking = true;
                chat_card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui_icon(ui, icons, ICON_TRAIN, 15.0, HYPE_COLOR);
                        bar(
                            ui,
                            frac,
                            HYPE_COLOR,
                            format!(
                                "Hype Train · Lvl {} · {}/{} · {}:{:02}",
                                train.level.max(1),
                                crate::models::group_thousands(train.total),
                                crate::models::group_thousands(train.goal),
                                remaining / 60,
                                remaining % 60,
                            ),
                        );
                    });
                });
            }
            HypePhase::Ended { since_secs } => {
                ticking = true;
                chat_card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui_icon(ui, icons, ICON_TRAIN, 15.0, HYPE_COLOR.gamma_multiply(0.5));
                        bar(
                            ui,
                            1.0,
                            HYPE_COLOR.gamma_multiply(0.35),
                            format!(
                                "Hype Train ended · Lvl {} · {} pts · {}",
                                train.level.max(1),
                                crate::models::group_thousands(train.total),
                                fmt_ago(since_secs),
                            ),
                        );
                    });
                });
            }
            HypePhase::Summary => {
                chat_card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui_icon(ui, icons, ICON_TRAIN, 15.0, HYPE_COLOR);
                        ui.label(&train.detail).on_hover_text(HYPE_HOVER);
                    });
                });
            }
        }
    }
    ticking
}

/// Role section a Twitch chatter is grouped under in the Users-in-chat
/// panel, from the highest-priority badge on their message. No "Chat Bots"
/// section (unlike Twitch's own list) — there's no reliable local signal for
/// bot accounts (no badge marks a bot as such); they just land in Users.
pub(in crate::ui) fn user_role_label(badges: &[String]) -> &'static str {
    let has = |set: &str| badges.iter().any(|b| b.split('/').next() == Some(set));
    if has("broadcaster") {
        "Broadcaster"
    } else if has("moderator") {
        "Moderators"
    } else if has("vip") {
        "VIPs"
    } else if has("subscriber") || has("founder") {
        "Subscribers"
    } else {
        "Users"
    }
}

/// Build the Users-in-chat panel's entries: one per unique Twitch login that
/// sent at least one message in `log`, using their LATEST message's
/// name/color/badges (so a mid-broadcast promotion — e.g. new mod — shows
/// their current role, not whoever they were when they first spoke).
/// Ordered by [`user_role_label`]'s priority, alphabetical within each
/// group. YouTube messages (empty `login`) are never included — this panel
/// is Twitch-only, same as the usercard it feeds.
pub(in crate::ui) fn build_users_panel(log: &ChatLog) -> Vec<ChatUserEntry> {
    let mut latest: HashMap<&str, &ChatMessage> = HashMap::new();
    for m in &log.messages {
        // Keyed the same way a moderation marker matches (login on Twitch,
        // author channel id on YouTube), so both platforms' chatters are
        // listed and each one's card opens on the identity it was built from.
        if !m.system && !m.purge_key().is_empty() {
            latest.insert(m.purge_key(), m);
        }
    }
    let mut entries: Vec<ChatUserEntry> = latest
        .into_values()
        .map(|m| ChatUserEntry {
            role: user_role_label(&m.badges),
            click: UserCardClick {
                login: m.login.clone(),
                display_name: m.author.clone(),
                color: m.color_override,
                badges: m.badges.clone(),
                badge_icons: m.badge_icons.clone(),
                badge_info: m.badge_info.clone(),
                user_id: if m.user_id.is_empty() { m.author_id.clone() } else { m.user_id.clone() },
                platform: m.platform,
            },
        })
        .collect();
    const ROLE_ORDER: [&str; 5] = ["Broadcaster", "Moderators", "VIPs", "Subscribers", "Users"];
    entries.sort_by(|a, b| {
        let ra = ROLE_ORDER.iter().position(|r| *r == a.role).unwrap_or(usize::MAX);
        let rb = ROLE_ORDER.iter().position(|r| *r == b.role).unwrap_or(usize::MAX);
        ra.cmp(&rb).then_with(|| {
            a.click.display_name.to_lowercase().cmp(&b.click.display_name.to_lowercase())
        })
    });
    entries
}
