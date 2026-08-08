//! The info strips above the message list: top supporters, the Hype
//! Train, and the users-in-chat panel's grouping.

use super::*;

/// Twitch's Hype Train accent, shared by the strip's icon and its progress
/// bar — the same pink the Channel Stats event plot gives `hype_train` rows,
/// so one broadcast reads the same in both views.
pub(in crate::ui) const HYPE_COLOR: egui::Color32 = egui::Color32::from_rgb(0xff, 0x40, 0x81);

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
    pub(in crate::ui) level: i64,
    pub(in crate::ui) total: i64,
    pub(in crate::ui) goal: i64,
    pub(in crate::ui) expires_at: i64,
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
                level: e.level,
                total: e.amount,
                goal: e.goal,
                expires_at: e.expires_at,
            }),
    }
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
