//! One chat message, drawn: [`render_chat_message`] and the appearance
//! snapshot it reads.

use super::*;

/// Chat-replay text appearance: font size (points) applied uniformly to the
/// timestamp/message/username, plus their colors. Global/shared across every
/// open chat window (`StreamArchiverApp::chat_font_pt`/`chat_ts_color`/
/// `chat_text_color`), edited from the ⚙ "Chat Appearance" panel inside each
/// chat window rather than the global Settings dialog.
pub(in crate::ui) struct ChatAppearance {
    pub(in crate::ui) font_pt: f32,
    /// Emote/emoji pixel size, independent of `font_pt` — see
    /// `StreamArchiverApp::chat_emote_pt`'s doc.
    pub(in crate::ui) emote_pt: f32,
    pub(in crate::ui) ts_color: egui::Color32,
    pub(in crate::ui) text_color: egui::Color32,
    /// Hash of the chosen chat font family's name. The family itself is
    /// always [`crate::fonts::CHAT_FAMILY`] (registered even when no font is
    /// picked, in which case it just mirrors the proportional stack), so this
    /// exists purely to make a font change invalidate `layout_key` — the same
    /// glyphs at the same point size are a different height in a different
    /// face. `u64` rather than the name so this struct stays `Copy`.
    pub(in crate::ui) font_id: u64,
    pub(in crate::ui) ts_mode: ChatTsMode,
}

/// Which clock the chat replay's timestamps show.
///
/// Both are genuinely useful and the right one depends on what you're doing,
/// which is why this is a one-click toolbar toggle rather than a setting
/// buried in a panel: while watching live you want the wall clock, and to
/// seek the local recording you want the offset from its start.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub(in crate::ui) enum ChatTsMode {
    /// `[00:40:10]` — seconds since the broadcast started. The default,
    /// because this is an archive tool and it's what lets you scrub to a
    /// moment in the recording.
    #[default]
    StreamRelative,
    /// `19:30` — local wall-clock time, as Twitch's own popout shows.
    WallClock,
}

impl ChatTsMode {
    pub(in crate::ui) fn parse(s: &str) -> ChatTsMode {
        match s {
            "clock" => ChatTsMode::WallClock,
            _ => ChatTsMode::StreamRelative,
        }
    }
    pub(in crate::ui) fn as_str(self) -> &'static str {
        match self {
            ChatTsMode::WallClock => "clock",
            ChatTsMode::StreamRelative => "relative",
        }
    }
}

/// Wall-clock `HH:MM` for a message, in local time. `None` when the message
/// carries no absolute time (pre-feature logs, where the sidecar only ever
/// stored an offset).
pub(in crate::ui) fn fmt_chat_clock(ts_unix_ms: f64) -> Option<String> {
    (ts_unix_ms > 0.0)
        .then(|| chrono::DateTime::from_timestamp_millis(ts_unix_ms as i64))
        .flatten()
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

/// The timestamp to draw, and the OTHER format as hover text.
///
/// Showing both costs nothing and answers the common one-off question ("what
/// offset was that at?") without touching the toggle at all. A message with
/// no absolute time falls back to the stream-relative form in both modes —
/// better than a blank column.
pub(in crate::ui) fn fmt_chat_ts_mode(msg: &ChatMessage, mode: ChatTsMode) -> (String, String) {
    let rel = fmt_chat_ts(msg.timestamp_secs);
    match (mode, fmt_chat_clock(msg.ts_unix_ms)) {
        (ChatTsMode::WallClock, Some(clock)) => (clock, format!("{rel} into the broadcast")),
        (ChatTsMode::WallClock, None) => (rel, "No wall-clock time recorded for this message".into()),
        (ChatTsMode::StreamRelative, Some(clock)) => (rel, clock),
        (ChatTsMode::StreamRelative, None) => (rel, String::new()),
    }
}

/// Why a row is drawn differently from an ordinary message.
///
/// Twitch renders each of these with its own coloured left accent, and so do
/// we — see [`row_decor`]. `System` is the pre-existing muted notice line;
/// the rest arrive once the sidecar records them.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui) enum ChatNotice {
    /// Moderation/room event captured live (mode change, timeout, clear).
    System,
    /// The sender's first-ever message in this channel (`first-msg=1`).
    FirstMessage,
    /// Sent via a channel-point reward. `reward` is the reward's title once
    /// resolved, else `None` and the raw `reward_id` is all we have.
    Redemption { reward: Option<String>, reward_id: String, cost: Option<i64> },
    /// A sub / resub / gift / mystery gift, carrying Twitch's own rendered
    /// `system-msg` copy verbatim.
    Sub { system_msg: String },
    /// An incoming raid, same.
    Raid { system_msg: String },
    /// A moderator announcement.
    Announce { system_msg: String },
    /// A viewer-milestone watch streak.
    WatchStreak { system_msg: String },
}

impl ChatNotice {
    /// Build an event notice from a sidecar `{"marker":"event"}` line's
    /// `kind`. `None` for an unrecognised kind — a newer build's marker read
    /// by an older one degrades to nothing rather than a mislabelled row.
    pub(in crate::ui) fn from_event_kind(kind: &str, system_msg: String) -> Option<ChatNotice> {
        Some(match kind {
            "sub" => ChatNotice::Sub { system_msg },
            "raid" => ChatNotice::Raid { system_msg },
            "announce" => ChatNotice::Announce { system_msg },
            "watchstreak" => ChatNotice::WatchStreak { system_msg },
            _ => return None,
        })
    }
}

/// Twitch's own rendered copy for an event row, when the notice is one that
/// has a headline of its own. `None` for the kinds that decorate an ordinary
/// message rather than replacing it.
pub(in crate::ui) fn notice_headline(notice: &ChatNotice) -> Option<&str> {
    match notice {
        ChatNotice::Sub { system_msg }
        | ChatNotice::Raid { system_msg }
        | ChatNotice::Announce { system_msg }
        | ChatNotice::WatchStreak { system_msg } => {
            (!system_msg.is_empty()).then_some(system_msg.as_str())
        }
        ChatNotice::System | ChatNotice::FirstMessage | ChatNotice::Redemption { .. } => None,
    }
}

/// How a row's background and left accent are drawn.
pub(in crate::ui) struct RowDecor {
    pub(in crate::ui) fill: egui::Color32,
    /// The 3px bar down the left edge, Twitch-style. `None` = ordinary row.
    pub(in crate::ui) accent: Option<egui::Color32>,
}

/// Twitch's own purple, used for first-message and redemption accents.
pub(in crate::ui) const TWITCH_PURPLE: egui::Color32 = egui::Color32::from_rgb(0x91, 0x47, 0xff);

/// A message that named you or matched a highlight rule. Red, as Twitch marks
/// its own mentions — and deliberately NOT the selection colour, so it cannot
/// be confused with "highlight this chatter".
pub(in crate::ui) const HIT_COLOR: egui::Color32 = egui::Color32::from_rgb(0xd6, 0x45, 0x45);

/// Why a row stands out, beyond the message's own kind.
///
/// An enum rather than two booleans because the two reasons are different
/// facts that were being OR-ed into one: `highlighted || hit` made a matched
/// trigger indistinguishable from a watched chatter, and the caller then
/// skipped the trigger check entirely for a watched chatter's messages — so a
/// rule firing on someone you were already watching was invisible.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(in crate::ui) enum RowEmphasis {
    #[default]
    None,
    /// The user's own "highlight this chatter" pick, from a usercard.
    Chatter,
    /// The message names the connected account, or matched a highlight rule.
    Hit,
}

/// Decide a row's decoration. Pure, so the mapping is testable.
///
/// Precedence, strongest first: a **hit**, then a watched **chatter**, then the
/// message's own kind.
///
/// A hit outranks a watched chatter because it is the new information. Every
/// message from a watched chatter is marked already, so losing one of them to
/// the hit colour costs nothing — whereas the reverse hides the one message of
/// theirs that actually said the thing you asked to be told about. Both
/// outrank the message kind: they were asked for explicitly, and losing them
/// behind a sub notice would defeat the point of asking.
pub(in crate::ui) fn row_decor(
    msg: &ChatMessage,
    emphasis: RowEmphasis,
    visuals: &egui::Visuals,
) -> RowDecor {
    match emphasis {
        RowEmphasis::Hit => {
            return RowDecor {
                fill: HIT_COLOR.gamma_multiply(0.22),
                accent: Some(HIT_COLOR),
            };
        }
        RowEmphasis::Chatter => {
            return RowDecor {
                fill: visuals.selection.bg_fill.gamma_multiply(0.35),
                accent: Some(visuals.selection.bg_fill),
            };
        }
        RowEmphasis::None => {}
    }
    let Some(notice) = msg.notice.as_deref() else {
        return RowDecor { fill: egui::Color32::TRANSPARENT, accent: None };
    };
    let tinted = |c: egui::Color32| RowDecor { fill: c.gamma_multiply(0.16), accent: Some(c) };
    match notice {
        // The muted informational line keeps its plain look — it is already
        // visually distinct (italic, weak, no author) and an accent bar would
        // give routine room events more weight than a sub.
        ChatNotice::System => RowDecor { fill: egui::Color32::TRANSPARENT, accent: None },
        ChatNotice::FirstMessage | ChatNotice::Redemption { .. } => tinted(TWITCH_PURPLE),
        ChatNotice::Sub { .. } | ChatNotice::WatchStreak { .. } => {
            tinted(egui::Color32::from_rgb(0x3e, 0x9b, 0xd6))
        }
        ChatNotice::Raid { .. } => tinted(egui::Color32::from_rgb(0x2e, 0xa0, 0x43)),
        ChatNotice::Announce { .. } => tinted(egui::Color32::from_rgb(0xd6, 0x9b, 0x3e)),
    }
}

/// At most this many colour runs per painted name.
///
/// egui caches a galley per `LayoutJob`, so each run is real layout work.
/// Twelve is enough that a 15-character name reads as a smooth sweep at chat
/// sizes, and it bounds the cost: even a screen where every sender has a
/// paint is a few hundred runs, and painted chatters are a small minority in
/// practice.
pub(in crate::ui) const MAX_PAINT_RUNS: usize = 12;

/// Draw a username as a quantized gradient, or `None` if there's nothing to
/// draw it with.
///
/// **Static, never animated — deliberately.** egui keys its galley cache on
/// the `LayoutJob`, so recolouring every frame would bust that cache for every
/// painted name on screen, every frame; and the repaint needed to drive it
/// comes from a deferred child viewport, which `MIN_ANIM_REPAINT_SECS`
/// documents (with a measured repro) as able to starve the ROOT viewport to
/// zero passes per second. A moving name gradient is not worth risking the
/// whole UI's frame loop.
pub(in crate::ui) fn paint_name_job(
    text: &str,
    paint: &crate::cosmetics::Paint,
    font: egui::FontId,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let chars: Vec<char> = text.chars().collect();
    // One run per character up to the cap, then evenly-sized chunks.
    let runs = chars.len().clamp(1, MAX_PAINT_RUNS);
    let per = chars.len().div_ceil(runs).max(1);
    let mut i = 0;
    while i < chars.len() {
        let end = (i + per).min(chars.len());
        // Sample at the chunk's midpoint so the sweep is centred on it.
        let t = ((i + end) as f32 / 2.0) / chars.len() as f32;
        let [r, g, b, a] = paint.sample(t);
        job.append(
            &chars[i..end].iter().collect::<String>(),
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: egui::Color32::from_rgba_unmultiplied(r, g, b, a),
                ..Default::default()
            },
        );
        i = end;
    }
    job
}

/// The family the chat replay renders in. Always registered — with no user
/// pick it mirrors the proportional stack, so this is safe unconditionally.
pub(in crate::ui) fn chat_family() -> egui::FontFamily {
    egui::FontFamily::Name(crate::fonts::CHAT_FAMILY.into())
}

/// Stable hash of a font family name, for [`ChatAppearance::font_id`].
pub(in crate::ui) fn font_name_key(name: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(name, &mut h);
    std::hash::Hasher::finish(&h)
}

impl ChatAppearance {
    /// Everything here that changes how tall a row comes out, folded into one
    /// value. Colours are deliberately excluded — recolouring text cannot
    /// change its height, and including them would throw away the whole
    /// height cache every time the colour picker moves a pixel.
    ///
    /// Any future field that affects layout (font family, timestamp format,
    /// paints) MUST be folded in here, or rows keep their stale measured
    /// heights and the virtualized list scrolls to the wrong place.
    pub(in crate::ui) fn layout_key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&self.font_pt.to_bits(), &mut h);
        std::hash::Hash::hash(&self.emote_pt.to_bits(), &mut h);
        std::hash::Hash::hash(&self.font_id, &mut h);
        // `[00:40:10]` and `19:30` are different widths, which changes where
        // a long message wraps and therefore how tall its row is.
        std::hash::Hash::hash(&self.ts_mode, &mut h);
        std::hash::Hasher::finish(&h)
    }
}

/// A username click in the chat replay — everything the usercard needs to
/// build its local-only fields immediately; the live Twitch lookup (avatar/
/// account-created date) is fetched separately, keyed by `user_id`. Also
/// built (cloned) for each row of the Users-in-chat panel, which needs the
/// same shape to open a usercard on click without re-scanning the log.
#[derive(Clone)]
pub(in crate::ui) struct UserCardClick {
    /// Twitch login; empty for YouTube, which identifies a chatter by
    /// `user_id` (their `UC…` channel id) instead.
    pub(in crate::ui) login: String,
    pub(in crate::ui) display_name: String,
    pub(in crate::ui) color: Option<egui::Color32>,
    pub(in crate::ui) badges: Vec<String>,
    pub(in crate::ui) badge_icons: Vec<Option<std::path::PathBuf>>,
    pub(in crate::ui) badge_info: String,
    /// The platform's own id for this chatter: Twitch's numeric `user-id`, or
    /// YouTube's channel id. Decides both the live-lookup path and how
    /// recorded moderation events are matched back to them.
    pub(in crate::ui) user_id: String,
    pub(in crate::ui) platform: ChatPlatform,
}

impl UserCardClick {
    /// What identifies this chatter within a log: the Twitch login, else the
    /// platform id. Same key [`ChatMessage::purge_key`] produces, so a card
    /// and a moderation marker always agree on who is who.
    pub(in crate::ui) fn key(&self) -> &str {
        if !self.login.is_empty() { &self.login } else { &self.user_id }
    }
}

/// A context-menu action chosen from a message's username. Applied by the
/// wrapper (`chat_popup_window`), not here: inserting into the send box's
/// draft and opening a Properties window both need `&mut self`, which the
/// deferred render closure doesn't have — same "stash on the struct, consume
/// after" shape [`UserCardClick`] already uses via `usercard_click`.
pub(in crate::ui) enum RowMenuAction {
    /// `@{name} ` should be inserted into the send box's draft.
    Reply(String),
    /// Open this Twitch channel's Properties window — offered only when the
    /// message's login matches one of the app's own monitored channels.
    OpenProperties(i64),
}

#[allow(clippy::too_many_arguments)]
pub(in crate::ui) fn render_chat_message(
    ui: &mut egui::Ui,
    msg: &ChatMessage,
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    render_emotes: bool,
    animate: bool,
    now: f64,
    misses: &mut Vec<std::path::PathBuf>,
    icons: Option<&UiTextures>,
    paints: &HashMap<String, crate::cosmetics::Paint>,
    ctx: &egui::Context,
    appearance: &ChatAppearance,
    // Twitch login (lowercased) -> monitor id, for every channel this app
    // monitors — decides whether "Open Properties" appears in a username's
    // context menu. Built once at popup-open, not per row.
    channel_by_login: &HashMap<String, i64>,
    // Whether this window has a send box at all (live Twitch take, connected
    // account) — "Reply" is hidden rather than shown disabled on a window
    // that can never send.
    can_send: bool,
) -> (Option<UserCardClick>, Option<RowMenuAction>) {
    let (shown_ts, other_ts) = fmt_chat_ts_mode(msg, appearance.ts_mode);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        // Timestamp — monospace, sized/colored to match the message body
        // (Twitch's own popout renders both at the same size).
        ui.label(
            // Monospace on purpose, and NOT the chat family: the bracketed
            // stream-relative timestamp is a column, and a proportional face
            // destroys the alignment that makes it scannable. The wall-clock
            // form keeps it too, so switching modes doesn't shuffle the layout.
            egui::RichText::new(shown_ts)
                .monospace()
                .size(appearance.font_pt)
                .color(appearance.ts_color),
        )
        .on_hover_text(other_ts);
        // System notice (moderation marker: mode change, timeout/ban, clear)
        // — muted ℹ line, no author/badges.
        if msg.system {
            let weak = ui.visuals().weak_text_color();
            ui_icon(ui, icons, ICON_INFO, appearance.font_pt, weak);
            ui.label(
                egui::RichText::new(&msg.text)
                    .italics()
                    .font(egui::FontId::new(appearance.font_pt, chat_family()))
                    .color(weak),
            )
            .on_hover_text(
                "Moderation/room event captured live from Twitch chat while recording",
            );
            return (None, None);
        }
        // Standalone event rows (sub / resub / gift / raid / announcement /
        // watch streak): Twitch's own `system-msg` copy verbatim, on its own
        // line above the sender's message if they left one. Using Twitch's
        // string rather than composing our own gets the pluralisation,
        // tier wording and localisation right for free.
        if let Some(line) = msg.notice.as_deref().and_then(notice_headline) {
            ui.label(
                egui::RichText::new(line)
                    .strong()
                    .font(egui::FontId::new(appearance.font_pt, chat_family()))
                    .color(appearance.text_color),
            );
            // A sub/raid notice with no message body of its own is the whole
            // row; one that carries a message falls through and renders the
            // sender beneath it.
            if msg.text.is_empty() {
                return (None, None);
            }
            ui.end_row();
        }
        // A channel-point redemption gets Twitch's header line ("X redeemed
        // Hydrate!"), with the cost where Twitch puts it.
        if let Some(ChatNotice::Redemption { reward, reward_id, cost }) = msg.notice.as_deref() {
            let title = reward.clone().unwrap_or_else(|| "a channel-point reward".to_string());
            ui.label(
                egui::RichText::new(format!("redeemed {title}"))
                    .font(egui::FontId::new(appearance.font_pt * 0.92, chat_family()))
                    .color(ui.visuals().weak_text_color()),
            )
            .on_hover_text(match cost {
                Some(c) => format!("{} channel points · reward id {reward_id}", crate::models::group_thousands(*c)),
                // Only redemptions that carry a message reach chat at all, and
                // IRC never names the reward — the title comes from a separate
                // lookup, so a miss leaves just the id.
                None => format!("Reward id {reward_id} (title not resolved)"),
            });
        }
        // Badges — real cached Twitch badge icons when resolved (Phase 1's
        // `ChatMessage::badge_icons`, index-aligned with `badges`), falling
        // back to the glyph (not yet cached, still downloading, or YouTube —
        // `badge_icons` is empty there).
        //
        // Reserved to a FIXED width (`BADGE_SLOTS` worth), regardless of how
        // many badges this particular message actually has — otherwise every
        // row's badge count shifts where the username starts, and a chat
        // full of mixed sub/mod/no-badge senders reads as a ragged mess
        // instead of a column. Twitch's own popout has the same alignment
        // issue; this fixes it rather than replicating it. A message with
        // MORE badges than `BADGE_SLOTS` (rare — broadcaster+mod+sub+bits+
        // partner all at once) just overflows the reserved width for that
        // one row rather than being truncated.
        const BADGE_SLOTS: usize = 3;
        let badge_h: f32 = (appearance.font_pt * 1.1).clamp(14.0, 32.0);
        let badge_slot_w = badge_h + ui.spacing().item_spacing.x;
        let reserved_w = (BADGE_SLOTS.max(msg.badges.len()) as f32) * badge_slot_w;
        ui.allocate_ui(egui::vec2(reserved_w, badge_h), |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for (i, badge) in msg.badges.iter().enumerate() {
                    let icon = msg.badge_icons.get(i).and_then(|o| o.as_ref());
                    let drawn = icon.and_then(|path| {
                        draw_cached_emote(ui, cache, path, false, badge_h, now, misses, ctx)
                    });
                    if let Some((resp, _tex)) = drawn {
                        resp.on_hover_text(badge_label(badge));
                    } else {
                        let (sym, color) = badge_display(badge, &msg.platform);
                        ui.label(egui::RichText::new(sym).small().color(color))
                            .on_hover_text(badge_label(badge));
                    }
                }
            });
        });
        // Shared Chat source indicator — a small colored dot naming the OTHER
        // channel this message actually came from (own-channel messages
        // during the same session get no dot, see `ChatMessage::source_name`'s
        // doc). Deterministic per-name color, same function used when a
        // sender has no explicit Twitch USERCOLOR, so it's consistent with
        // how that channel's own name would render in its own chat.
        if !msg.source_name.is_empty() {
            let dot_color = twitch_username_color(&msg.source_name);
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, dot_color);
            resp.on_hover_text(format!("From {}'s chat (Shared Chat)", msg.source_name));
        }
        // Username — bold, platform/user colour, adjusted for contrast on the
        // chat panel's background so dark colours stay legible. Clickable
        // wherever the message carries an identity to build a card from: a
        // Twitch login, or a YouTube author channel id. Pre-feature logs have
        // neither and stay a plain label.
        let name_color = chat_username_color(msg, ui.visuals().panel_fill);
        let name_font = egui::FontId::new(appearance.font_pt, chat_family());
        // A 7TV paint replaces the flat colour with a quantized gradient. The
        // name keeps its `:` suffix and everything else about the row.
        let painted = (!msg.user_id.is_empty())
            .then(|| paints.get(&msg.user_id))
            .flatten()
            .map(|p| paint_name_job(&format!("{}:", msg.author), p, name_font.clone()));
        let name_text = egui::RichText::new(format!("{}:", msg.author))
            .strong()
            .font(name_font)
            .color(name_color);
        let mut click: Option<UserCardClick> = None;
        let mut menu_action: Option<RowMenuAction> = None;
        // Shared by the left-click handler and the "View user info"
        // context-menu item — building the card needs no per-caller state,
        // so both just call this.
        let build_click = || UserCardClick {
            login: msg.login.clone(),
            display_name: msg.author.clone(),
            color: msg.color_override,
            badges: msg.badges.clone(),
            badge_icons: msg.badge_icons.clone(),
            badge_info: msg.badge_info.clone(),
            // Twitch numeric id, or YouTube's channel id — whichever this
            // platform gave us.
            user_id: if msg.user_id.is_empty() {
                msg.author_id.clone()
            } else {
                msg.user_id.clone()
            },
            platform: msg.platform.clone(),
        };
        if !msg.purge_key().is_empty() {
            let resp = ui
                .add(match painted {
                    Some(job) => egui::Label::new(job).sense(egui::Sense::click()),
                    None => egui::Label::new(name_text).sense(egui::Sense::click()),
                })
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(
                    "Click for user info — messages in this log, what this channel has \
                     recorded about them, and any moderation actions against them. \
                     Right-click for more.",
                );
            if resp.clicked() {
                click = Some(build_click());
            }
            let monitor_id = channel_by_login.get(&msg.login.to_lowercase()).copied();
            resp.context_menu(|ui| {
                if ui.button("View user info").clicked() {
                    click = Some(build_click());
                    ui.close();
                }
                if can_send && ui.button(format!("Reply to @{}", msg.author)).clicked() {
                    menu_action = Some(RowMenuAction::Reply(msg.author.clone()));
                    ui.close();
                }
                // Only offered when this chatter IS one of the app's own
                // monitored channels (e.g. a fellow streamer chatting during
                // a raid or a Shared Chat collab) — not every viewer.
                if let Some(mid) = monitor_id
                    && ui.button("Open Properties").clicked()
                {
                    menu_action = Some(RowMenuAction::OpenProperties(mid));
                    ui.close();
                }
            });
        } else {
            match painted {
                Some(job) => ui.label(job),
                None => ui.label(name_text),
            };
        }
        // Reply-thread prefix (Twitch): who this message answers.
        if !msg.reply_to.is_empty() {
            let weak = ui.visuals().weak_text_color();
            ui_icon(ui, icons, ICON_REPLY, appearance.font_pt * 0.85, weak)
                .on_hover_text("This message is a reply in a thread");
            ui.label(egui::RichText::new(&msg.reply_to).small().color(weak))
                .on_hover_text("This message is a reply in a thread");
        }
        // A moderator-struck message: the archived original renders
        // struck-through (live chat hides it; the archive keeps receipts).
        // Emotes drop to their text fallback so the strike reads clearly.
        if let Some(reason) = &msg.deleted {
            for seg in &msg.segments {
                let t = match seg {
                    ChatSegment::Text(t) => t.as_str(),
                    ChatSegment::Emote { name, fallback_text, .. } => {
                        fallback_text.as_deref().unwrap_or(name)
                    }
                };
                ui.label(
                    egui::RichText::new(t)
                        .strikethrough()
                        .weak()
                        .font(egui::FontId::new(appearance.font_pt, chat_family())),
                )
                    .on_hover_text(reason);
            }
            ui.label(egui::RichText::new(format!("({reason})")).small().weak().italics());
            return (click, menu_action);
        }
        // Message body — text runs and (when enabled & on disk) inline emote images.
        let emote_h = appearance.emote_pt;
        for seg in &msg.segments {
            match seg {
                ChatSegment::Text(t) => {
                    // One label per run: egui wraps a multi-word galley at word
                    // boundaries inside horizontal_wrapped while preserving the run's
                    // internal/leading/trailing whitespace verbatim. A run
                    // containing a URL is split further so the link renders as
                    // its own clickable widget; everything around it keeps
                    // going through the plain-text path unchanged.
                    for part in split_text_urls(t) {
                        match part {
                            TextPart::Plain(s) => {
                                ui.label(
                                    egui::RichText::new(s)
                                        .font(egui::FontId::new(appearance.font_pt, chat_family()))
                                        .color(appearance.text_color),
                                );
                            }
                            TextPart::Url(url) => {
                                let resp = ui
                                    .add(
                                        egui::Label::new(
                                            egui::RichText::new(url)
                                                .font(egui::FontId::new(
                                                    appearance.font_pt,
                                                    chat_family(),
                                                ))
                                                .color(ui.visuals().hyperlink_color)
                                                .underline(),
                                        )
                                        .sense(egui::Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text(url);
                                if resp.clicked() {
                                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                                }
                                resp.context_menu(|ui| {
                                    if ui.button("Copy Link").clicked() {
                                        ui.ctx().copy_text(url.to_string());
                                        ui.close();
                                    }
                                    if ui.button("Open in Browser").clicked() {
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                                        ui.close();
                                    }
                                });
                            }
                        }
                    }
                }
                ChatSegment::Emote { name, file, fallback_text, .. } => {
                    let drawn = render_emotes
                        && file.as_ref().is_some_and(|f| {
                            match draw_cached_emote(ui, cache, f, animate, emote_h, now, misses, ctx)
                            {
                                Some((resp, tex)) => {
                                    queue_alt_image_preview(ctx, &resp, &tex);
                                    let resp = resp.on_hover_text(format!(
                                        "{name}\nAlt: preview full size · right-click: more"
                                    ));
                                    let path = f.clone();
                                    resp.context_menu(|ui| {
                                        if ui.button("Copy Image").clicked() {
                                            copy_emote_image_to_clipboard(&path);
                                            ui.close();
                                        }
                                        if ui.button("Open File").clicked() {
                                            open_path(&path);
                                            ui.close();
                                        }
                                        if ui.button("Open Folder").clicked() {
                                            if let Some(dir) = path.parent() {
                                                open_path(dir);
                                            }
                                            ui.close();
                                        }
                                    });
                                    true
                                }
                                None => false,
                            }
                        });
                    if !drawn {
                        // No image (off / loading / not on disk / undecodable): show
                        // the emoji glyph if we have one, else the emote code.
                        ui.label(fallback_text.as_deref().unwrap_or(name));
                    }
                }
            }
        }
        // Twitch tags a first-ever message in the channel. Trailing rather
        // than right-aligned: this is a wrapped inline layout, so a
        // right-aligned chip would need its own pass to place and would
        // collide with a long message's last line anyway.
        if matches!(msg.notice.as_deref(), Some(ChatNotice::FirstMessage)) {
            ui.label(egui::RichText::new("FIRST MESSAGE").small().strong().color(TWITCH_PURPLE))
                .on_hover_text("The first message this account has ever sent in this channel");
        }
        (click, menu_action)
    })
    .inner
}

/// One piece of a text run after pulling any URLs out of it — a message can
/// mix ordinary words and links freely, and everything BUT the URL still goes
/// through the same plain-text label as before.
enum TextPart<'a> {
    Plain(&'a str),
    Url(&'a str),
}

/// Split a chat message's text run at `http://`/`https://` URLs. A URL runs
/// to the next whitespace, then sheds trailing punctuation (`.`, `,`, `)`, …)
/// that's almost always sentence punctuation rather than part of the link —
/// "check this out: https://example.com/x." must not turn the trailing `.`
/// into part of the address.
fn split_text_urls(text: &str) -> Vec<TextPart<'_>> {
    let mut parts = Vec::new();
    let mut pos = 0;
    while let Some(start) = next_url_start(text, pos) {
        if start > pos {
            parts.push(TextPart::Plain(&text[pos..start]));
        }
        let rest = &text[start..];
        let end_rel = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let mut end = start + end_rel;
        // "https://" is the longer of the two schemes (8 bytes) — never trim
        // into either scheme while stripping trailing punctuation.
        while end > start + 8 {
            let c = text[..end].chars().next_back().expect("end > start implies a prior char");
            if matches!(c, '.' | ',' | '!' | '?' | ')' | ';' | ':' | '\'' | '"') {
                end -= c.len_utf8();
            } else {
                break;
            }
        }
        parts.push(TextPart::Url(&text[start..end]));
        pos = end;
    }
    if pos < text.len() {
        parts.push(TextPart::Plain(&text[pos..]));
    }
    parts
}

/// The byte offset of the next `http://` or `https://` in `text` at or after
/// `from`, whichever comes first.
fn next_url_start(text: &str, from: usize) -> Option<usize> {
    let https = text[from..].find("https://").map(|p| p + from);
    let http = text[from..].find("http://").map(|p| p + from);
    match (https, http) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod url_split_tests {
    use super::*;

    fn plains<'a>(parts: &'a [TextPart<'a>]) -> Vec<&'a str> {
        parts
            .iter()
            .filter_map(|p| match p {
                TextPart::Plain(s) => Some(*s),
                TextPart::Url(_) => None,
            })
            .collect()
    }

    fn urls<'a>(parts: &'a [TextPart<'a>]) -> Vec<&'a str> {
        parts
            .iter()
            .filter_map(|p| match p {
                TextPart::Url(s) => Some(*s),
                TextPart::Plain(_) => None,
            })
            .collect()
    }

    #[test]
    fn plain_text_with_no_url_is_untouched() {
        let parts = split_text_urls("just chatting, no links here");
        assert_eq!(urls(&parts), Vec::<&str>::new());
        assert_eq!(plains(&parts), vec!["just chatting, no links here"]);
    }

    #[test]
    fn a_bare_url_becomes_its_own_part() {
        let parts = split_text_urls("check https://example.com/x out");
        assert_eq!(plains(&parts), vec!["check ", " out"]);
        assert_eq!(urls(&parts), vec!["https://example.com/x"]);
    }

    #[test]
    fn trailing_sentence_punctuation_is_not_part_of_the_link() {
        let parts = split_text_urls("go here: http://example.com/a.");
        assert_eq!(urls(&parts), vec!["http://example.com/a"]);
        assert_eq!(plains(&parts), vec!["go here: ", "."]);
    }

    #[test]
    fn a_url_at_the_very_start_or_end_keeps_no_empty_plain_part() {
        let parts = split_text_urls("https://example.com");
        assert_eq!(urls(&parts), vec!["https://example.com"]);
        assert!(plains(&parts).is_empty());
    }

    #[test]
    fn two_urls_in_one_run_both_split_out() {
        let parts = split_text_urls("https://a.com and https://b.com");
        assert_eq!(urls(&parts), vec!["https://a.com", "https://b.com"]);
        assert_eq!(plains(&parts), vec![" and "]);
    }
}
