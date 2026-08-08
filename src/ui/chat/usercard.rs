//! Usercard content: the banner, the per-channel activity summary, and
//! the moderation history section.

use super::*;

/// Paint a left-to-right gradient banner strip (the user's color fading into
/// the panel background) at the current cursor, reserving `height` px of
/// vertical space. Purely decorative — Twitch exposes no per-viewer banner
/// image via the public API, so this approximates the look of the 7TV/
/// native usercard banners without a network fetch.
pub(in crate::ui) fn paint_user_banner(ui: &mut egui::Ui, user_color: egui::Color32, height: f32) {
    let width = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let bg = ui.visuals().panel_fill;
    const STRIPS: i32 = 32;
    for i in 0..STRIPS {
        let t = (i as f32 / (STRIPS - 1) as f32).powf(1.6);
        let col = lerp_color32(user_color, bg, t);
        let x0 = rect.left() + rect.width() * (i as f32 / STRIPS as f32);
        let x1 = rect.left() + rect.width() * ((i + 1) as f32 / STRIPS as f32);
        ui.painter().rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, rect.top()), egui::pos2(x1, rect.bottom())),
            0.0,
            col,
        );
    }
}

/// Summarize a user's locally-recorded contribution history on this channel
/// (bits/gift-subs/raids/timeouts-bans) from the already-loaded `stream_event`
/// rows, matched case-insensitively against their Twitch display name — the
/// `actor` column stores display names, not logins (see `stream_event`'s
/// doc). One line per non-zero category; empty when nothing matched (a
/// lurker, or a channel this app only started recording recently).
pub(in crate::ui) fn summarize_user_events(
    events: &[crate::models::StreamEventRow],
    display_name: &str,
) -> Vec<String> {
    let name_lc = display_name.to_lowercase();
    let mine: Vec<&crate::models::StreamEventRow> =
        events.iter().filter(|e| e.actor.to_lowercase() == name_lc).collect();
    let of_kind = |kind: &str| -> Vec<&&crate::models::StreamEventRow> {
        mine.iter().filter(|e| e.kind == kind).collect()
    };

    let mut lines = Vec::new();
    let bits = of_kind("bits");
    if !bits.is_empty() {
        let total: i64 = bits.iter().map(|e| e.amount).sum();
        lines.push(format!(
            "💎 {total} bits cheered ({} message{})",
            bits.len(),
            if bits.len() == 1 { "" } else { "s" }
        ));
    }
    let gifts = of_kind("subgift");
    if !gifts.is_empty() {
        let total: i64 = gifts.iter().map(|e| e.amount.max(1)).sum();
        lines.push(format!(
            "🎁 {total} sub(s) gifted ({} event{})",
            gifts.len(),
            if gifts.len() == 1 { "" } else { "s" }
        ));
    }
    let raids = of_kind("raid_in");
    if !raids.is_empty() {
        let viewers: i64 = raids.iter().map(|e| e.amount).sum();
        lines.push(format!(
            "📡 Raided this channel {} time{} (brought {viewers} viewer(s) total)",
            raids.len(),
            if raids.len() == 1 { "" } else { "s" }
        ));
    }
    let sub_n = of_kind("sub").len() + of_kind("resub").len();
    if sub_n > 0 {
        lines.push(format!("⭐ {sub_n} subscription event(s) recorded"));
    }
    let timeouts = of_kind("timeout").len();
    let bans = of_kind("ban").len();
    if timeouts > 0 || bans > 0 {
        lines.push(format!(
            "⚠ {timeouts} timeout(s), {bans} ban(s) in this channel's recorded history"
        ));
    }
    lines
}

/// One line describing a chatter's last known moderation state, plus whether
/// it should be drawn as a warning.
///
/// "Last known" is the whole point: neither platform tells an anonymous
/// listener about un-bans or un-timeouts, so a ban we recorded in March says
/// nothing about today. The wording never asserts a present-tense state that
/// we can't actually observe — except for a timeout whose own duration hasn't
/// run out yet, which is arithmetic rather than a guess.
pub(in crate::ui) fn moderation_state_line(
    state: crate::models::ModerationState,
    now: i64,
) -> (String, bool) {
    use crate::models::ModerationState as S;
    match state {
        S::Clean => ("✔ No moderation actions on record".to_string(), false),
        S::MessagesDeleted => {
            ("⚠ Has had messages deleted, but was never timed out or banned".to_string(), false)
        }
        S::TimedOut { at, secs, until } if until > now => (
            format!(
                "⏳ Timed out for {} on {} — {} left",
                fmt_timeout_secs(secs),
                fmt_datetime_short(at),
                crate::rolling::fmt_remaining(until - now)
            ),
            true,
        ),
        S::TimedOut { at, secs, .. } => (
            format!("⚠ Last timed out for {} on {}", fmt_timeout_secs(secs), fmt_datetime_short(at)),
            false,
        ),
        S::Banned { at } => {
            (format!("🚫 Banned on {} (no un-ban seen since)", fmt_datetime_short(at)), true)
        }
        S::Purged { at } => (
            format!(
                "🚫 All their messages were removed on {} — YouTube doesn't say whether that \
                 was a timeout or a ban",
                fmt_datetime_short(at)
            ),
            true,
        ),
    }
}

/// `600` → `"10m"`. Shared by the usercard and instance Properties.
pub(in crate::ui) fn fmt_timeout_secs(secs: i64) -> String {
    match secs {
        s if s <= 0 => "an unknown time".to_string(),
        s if s >= 86_400 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s >= 3600 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// One recorded moderation action as a single readable line.
pub(in crate::ui) fn moderation_event_line(e: &crate::models::StreamEventRow) -> String {
    match e.kind.as_str() {
        "msg_deleted" if e.detail.is_empty() => "message deleted".to_string(),
        "msg_deleted" => format!("message deleted: \u{201c}{}\u{201d}", e.detail),
        "timeout" => format!("timed out for {}", fmt_timeout_secs(e.amount)),
        "ban" => "banned".to_string(),
        "chat_purge" if e.detail.is_empty() => "all messages removed".to_string(),
        "chat_purge" => format!("all messages removed ({})", e.detail),
        other => other.to_string(),
    }
}

/// The usercard's 🔨 Moderation section: what this channel has on record
/// against this chatter, and what that adds up to.
///
/// Always shown, including the reassuring empty case — "nothing on record" is
/// exactly the answer someone opening this is looking for, and a section that
/// silently vanishes can't distinguish "clean" from "not checked".
pub(in crate::ui) fn usercard_moderation_section(ui: &mut egui::Ui, card: &UserCardPopup) {
    let now = crate::models::now_unix();
    ui.separator();
    ui.label(egui::RichText::new("🔨 Moderation:").weak()).on_hover_text(
        "Timeouts, bans and deleted messages this channel has recorded for this chatter, \
         across every broadcast — not just the one you're viewing. Captured passively from \
         chat: neither platform tells a listener WHO moderated, why, or when someone was \
         un-banned, so this is only ever what was last seen.",
    );
    let (line, warn) = moderation_state_line(card.mod_summary.state(&card.moderation), now);
    if warn {
        ui.colored_label(grid::HL_ERROR_TEXT, line);
    } else {
        ui.label(line);
    }
    if card.mod_summary.is_clean() {
        return;
    }
    let s = card.mod_summary;
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
    ui.label(egui::RichText::new(parts.join(" · ")).small().weak());
    egui::ScrollArea::vertical()
        .id_salt("usercard_moderation")
        .max_height(120.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for e in &card.moderation {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    ui.label(
                        egui::RichText::new(fmt_datetime_short(e.at)).monospace().small().weak(),
                    );
                    ui.label(egui::RichText::new(moderation_event_line(e)).small());
                });
            }
        });
}
