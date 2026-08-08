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
    ctx: &egui::Context,
    appearance: &ChatAppearance,
) -> Option<UserCardClick> {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        // Timestamp — monospace, sized/colored to match the message body
        // (Twitch's own popout renders both at the same size).
        ui.label(
            // Monospace on purpose, and NOT the chat family: the bracketed
            // stream-relative timestamp is a column, and a proportional face
            // destroys the alignment that makes it scannable.
            egui::RichText::new(fmt_chat_ts(msg.timestamp_secs))
                .monospace()
                .size(appearance.font_pt)
                .color(appearance.ts_color),
        );
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
            return None;
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
        let name_text = egui::RichText::new(format!("{}:", msg.author))
            .strong()
            .font(egui::FontId::new(appearance.font_pt, chat_family()))
            .color(name_color);
        let mut click: Option<UserCardClick> = None;
        if !msg.purge_key().is_empty() {
            let resp = ui
                .add(egui::Label::new(name_text).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(
                    "Click for user info — messages in this log, what this channel has \
                     recorded about them, and any moderation actions against them",
                );
            if resp.clicked() {
                click = Some(UserCardClick {
                    login: msg.login.clone(),
                    display_name: msg.author.clone(),
                    color: msg.color_override,
                    badges: msg.badges.clone(),
                    badge_icons: msg.badge_icons.clone(),
                    badge_info: msg.badge_info.clone(),
                    // Twitch numeric id, or YouTube's channel id — whichever
                    // this platform gave us.
                    user_id: if msg.user_id.is_empty() {
                        msg.author_id.clone()
                    } else {
                        msg.user_id.clone()
                    },
                    platform: msg.platform.clone(),
                });
            }
        } else {
            ui.label(name_text);
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
            return click;
        }
        // Message body — text runs and (when enabled & on disk) inline emote images.
        let emote_h = appearance.emote_pt;
        for seg in &msg.segments {
            match seg {
                ChatSegment::Text(t) => {
                    // One label per run: egui wraps a multi-word galley at word
                    // boundaries inside horizontal_wrapped while preserving the run's
                    // internal/leading/trailing whitespace verbatim.
                    ui.label(
                        egui::RichText::new(t.as_str())
                            .font(egui::FontId::new(appearance.font_pt, chat_family()))
                            .color(appearance.text_color),
                    );
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
        click
    })
    .inner
}
