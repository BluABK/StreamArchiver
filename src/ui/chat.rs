//! Chat popup: log parsing (Twitch/YouTube), segments/emotes, colors,
//! emoji handling.

use super::*;

/// Source platform of a captured chat message (drives username colouring,
/// which identity keys a usercard, and whether a live lookup is possible).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ChatPlatform {
    YouTube,
    Twitch,
}

/// One renderable piece of a chat message body. Built once at parse time;
/// [`render_chat_message`] just walks it. `file: None` means "no local image"
/// (offline / not downloaded / undecodable / unknown id) → the renderer falls back
/// to drawing `fallback_text` (the emoji glyph) if set, else `name` (the emote
/// code). A `Text` segment may contain spaces.
#[derive(Clone)]
pub(super) enum ChatSegment {
    Text(String),
    Emote {
        name: String,
        file: Option<std::path::PathBuf>,
        /// Where a queued image download will land when `file` is `None` —
        /// [`upgrade_pending_emotes`] promotes it to `file` once it exists on
        /// disk, replacing the old "re-parse the whole file after the emoji
        /// download" upgrade pass.
        pending: Option<std::path::PathBuf>,
        /// For emoji: the Unicode glyph to show when there's no image, so it
        /// degrades to a (mono) glyph rather than a code. `None` for code emotes.
        fallback_text: Option<String>,
    },
}

/// A single parsed chat message (YouTube `.live_chat.json` or Twitch `.chat.jsonl`).
#[derive(Clone)]
pub(super) struct ChatMessage {
    /// Seconds from stream start (negative = chat arrived before we started recording).
    pub(super) timestamp_secs: f64,
    pub(super) author: String,
    /// Verbatim message body with emote codes left inline. KEPT (never replaced by
    /// rendered names) so the popup search filter still matches an emote by its
    /// code/shortcut even when it renders as an image.
    pub(super) text: String,
    /// Render plan: text runs interleaved with emote references. Always built; the
    /// "render emotes" toggle is applied at draw time, not here.
    pub(super) segments: Vec<ChatSegment>,
    /// Twitch: raw IRC badge segment per entry, e.g. `"subscriber/12"`.
    /// YouTube: badge tooltip text, e.g. `"Member"`.
    pub(super) badges: Vec<String>,
    /// Cached icon path per `badges` entry (index-aligned), resolved once at
    /// parse time via [`resolve_twitch_badge_icon`]. `None` at an index means
    /// "no cached icon for this badge" (not yet fetched, or YouTube — this
    /// whole vec is empty for YouTube messages) — the renderer falls back to
    /// [`badge_display`]'s glyph for that entry.
    pub(super) badge_icons: Vec<Option<std::path::PathBuf>>,
    /// Explicit hex colour from Twitch USERCOLOR; `None` when unset or YouTube.
    pub(super) color_override: Option<egui::Color32>,
    pub(super) platform: ChatPlatform,
    /// Twitch: sender's lowercase login (deletion/purge markers match on it).
    /// Empty for YouTube and pre-feature logs.
    pub(super) login: String,
    /// Per-message id — what a `del` marker references. Twitch's IRCv3 `id`
    /// tag, or YouTube's `liveChatTextMessageRenderer.id`. Empty on
    /// pre-feature logs, where single-message deletions can't be matched.
    pub(super) msg_id: String,
    /// YouTube's stable `UC…` author channel id — what a
    /// `markChatItemsByAuthorAsDeletedAction` names when a moderator removes
    /// everything one person said, and the key the usercard cross-references
    /// against recorded moderation events. Empty for Twitch (which identifies
    /// a chatter by `login` instead) and for pre-feature logs.
    pub(super) author_id: String,
    /// `Some(reason)` once a moderation marker struck this message
    /// ("deleted by a moderator", "timed out (10m)", …) — renders
    /// strikethrough with the reason on hover. Applied by
    /// [`ChatLog::apply_markers`], never set at parse time.
    pub(super) deleted: Option<String>,
    /// System notice line (chat-mode change, role change, timeout/ban
    /// announcement) — renders as a muted ℹ line, no author.
    ///
    /// Kept alongside `notice` for now: `ChatLog::apply_markers` and four
    /// construction sites key off it, and retiring it in the same change that
    /// introduces `notice` would mean touching all of them at once.
    pub(super) system: bool,
    /// Absolute send time (unix milliseconds), for the wall-clock timestamp
    /// mode. `0.0` on pre-feature logs that only ever stored an offset — the
    /// renderer then falls back to the stream-relative form.
    pub(super) ts_unix_ms: f64,
    /// Why this row is not an ordinary message, if it isn't. Drives both the
    /// row's accent colour ([`row_decor`]) and what gets drawn inside it.
    /// `Box`ed to keep `ChatMessage` small — it's cloned in several places
    /// and the overwhelming majority of rows have no notice at all.
    pub(super) notice: Option<Box<ChatNotice>>,
    /// Display name this message replies to (Twitch reply threads) — rendered
    /// as a small "↩ name" prefix. Empty when not a reply / pre-feature logs.
    pub(super) reply_to: String,
    /// Which channel this message actually came from, during a Twitch Shared
    /// Chat ("Stream Together") session — resolved once at parse time from
    /// the raw `source_room_id` tag against this take's recorded collab
    /// partners (`ChatPopup::source_partners`). Empty when: not a shared
    /// session, the message originated in the channel being viewed (no
    /// partner to name), or the id didn't resolve to a known partner. Never
    /// set for YouTube.
    pub(super) source_name: String,
    /// Twitch numeric user id (IRCv3 `user-id` tag) — used to key the
    /// usercard's live Helix avatar/account-created lookup. Empty for
    /// YouTube and pre-feature logs (the usercard's live section then always
    /// shows "N/A", same as a failed lookup).
    pub(super) user_id: String,
    /// Raw Twitch `badge-info` tag (e.g. `"subscriber/61"`) — exact
    /// cumulative sub-months, distinct from `badges`' display tier bucket.
    /// The usercard renders "Subscriber · N months" from this when present.
    /// Empty for YouTube / pre-feature logs / non-subscribers.
    pub(super) badge_info: String,
}

impl ChatMessage {
    /// What a [`ChatMarker::Purge`] matches this message against: the Twitch
    /// login, or YouTube's author channel id. Empty when neither is known
    /// (pre-feature logs), which can never match a marker — deliberately, since
    /// an empty key would otherwise strike every anonymous-looking message.
    pub(super) fn purge_key(&self) -> &str {
        if !self.login.is_empty() { &self.login } else { &self.author_id }
    }
}

/// How often a chat window sweeps its log for chatters whose 7TV paint it
/// hasn't looked up yet. Coarse on purpose: paints are cosmetic and change
/// rarely, and this must never approach a per-message or per-frame cost.
pub(super) const PAINT_SWEEP_SECS: u64 = 300;
/// How far back a paint sweep looks. The visible window plus a wide margin —
/// scanning a six-hour log's every sender would ask 7TV about thousands of
/// people nobody is looking at.
pub(super) const PAINT_SCAN_MESSAGES: usize = 500;

/// Height estimate for a chat row that hasn't been drawn yet (≈ one line).
pub(super) const CHAT_ROW_EST: f32 = 20.0;

/// A loaded chat log plus the state the incremental loaders and the
/// virtualized renderer need. Lives behind `ChatPopup::load_state`'s mutex;
/// the UI renders straight from the guard (no per-frame clone) while the
/// background tasks append/prepend under the same lock.
/// A moderation marker parsed from the sidecar (written live by the chat
/// logger next to the messages), applied to the in-memory log by
/// [`ChatLog::apply_markers`].
#[derive(Clone)]
pub(super) enum ChatMarker {
    /// A single message was deleted (matched by [`ChatMessage::msg_id`]).
    Delete { msg_id: String },
    /// Everything one chatter had said up to this point was removed — a
    /// Twitch timeout/ban, or YouTube's remove-by-author. `key` is matched
    /// against [`ChatMessage::purge_key`]: a lowercase Twitch login, or a
    /// YouTube `UC…` channel id.
    Purge { key: String, reason: String },
    /// The whole chat was cleared.
    Clear,
}

/// A marker plus when it happened (seconds from stream start, same clock as
/// `ChatMessage::timestamp_secs`).
#[derive(Clone)]
pub(super) struct MarkerAt {
    pub(super) ts_secs: f64,
    pub(super) marker: ChatMarker,
}

pub(super) struct ChatLog {
    pub(super) messages: Vec<ChatMessage>,
    /// Measured row heights, parallel to `messages` (estimates until a row has
    /// actually been drawn once). Drives the virtualized scroll offsets.
    pub(super) row_heights: Vec<f32>,
    /// What `row_heights` was measured under: `(width, appearance key)`. A
    /// resize changes wrapping, and so does anything in [`ChatAppearance`] —
    /// font size, font family, timestamp format. Either changing while the
    /// cache still holds the old heights means wrong scroll offsets and a
    /// jumping scrollbar, so both reset the cache to estimates.
    pub(super) measured_key: (f32, u64),
    /// Byte offset just past the last fully-parsed line of the chat file; the
    /// live tail reload resumes here instead of re-parsing the whole file.
    pub(super) parsed_to: u64,
    /// True while the pre-tail (older) part of the file is still parsing in
    /// the background — the newest messages are already shown.
    pub(super) loading_older: bool,
    /// Every moderation marker seen in the file. Kept so `apply_markers` can
    /// re-run after the phase-2 splice (a tail marker may target a pre-tail
    /// message that wasn't parsed yet when the marker arrived).
    pub(super) markers: Vec<MarkerAt>,
}

impl ChatLog {
    /// Strike out messages targeted by the stored moderation markers.
    /// Idempotent full pass (one map-building walk over markers + one walk
    /// over messages); called only when markers arrive or messages prepend,
    /// not per frame.
    pub(super) fn apply_markers(&mut self) {
        if self.markers.is_empty() {
            return;
        }
        let mut deleted_ids: HashMap<&str, ()> = HashMap::new();
        // login -> (latest purge ts, reason)
        let mut purges: HashMap<&str, (f64, &str)> = HashMap::new();
        let mut clear_ts = f64::NEG_INFINITY;
        for m in &self.markers {
            match &m.marker {
                ChatMarker::Delete { msg_id } => {
                    deleted_ids.insert(msg_id.as_str(), ());
                }
                ChatMarker::Purge { key, reason } => {
                    let e = purges.entry(key.as_str()).or_insert((m.ts_secs, reason));
                    if m.ts_secs >= e.0 {
                        *e = (m.ts_secs, reason);
                    }
                }
                ChatMarker::Clear => clear_ts = clear_ts.max(m.ts_secs),
            }
        }
        for msg in &mut self.messages {
            if msg.system {
                continue;
            }
            if !msg.msg_id.is_empty() && deleted_ids.contains_key(msg.msg_id.as_str()) {
                msg.deleted = Some("deleted by a moderator".into());
            } else if let Some((pts, reason)) =
                (!msg.purge_key().is_empty()).then(|| purges.get(msg.purge_key())).flatten()
                && msg.timestamp_secs <= *pts
            {
                msg.deleted = Some((*reason).to_string());
            } else if msg.timestamp_secs <= clear_ts {
                msg.deleted = Some("chat cleared".into());
            }
        }
    }
}

pub(super) enum ChatLoadState {
    Loading,
    Loaded(ChatLog),
    NoFile,
    Error(String),
}

/// State of a usercard's live Twitch Helix lookup (avatar + account-created
/// date) — separate from the local-only fields on [`UserCardPopup`], which
/// are available immediately from the click, no network involved.
pub(super) enum UserCardFetch {
    /// The "fetch live Twitch info" setting is off — the live section always
    /// shows "N/A" and no request is made.
    Disabled,
    Loading,
    Loaded { avatar_path: Option<std::path::PathBuf>, created_at: Option<String> },
    /// The Helix call failed; a warning was filed (see `store::upsert_capture_alert`
    /// / `AppEvent::CaptureAlert`). The live section shows "N/A".
    Failed,
}

/// One open chat usercard (at most one per chat window — clicking a
/// different username just replaces it, matching Twitch's own popout).
pub(super) struct UserCardPopup {
    pub(super) login: String,
    pub(super) display_name: String,
    pub(super) color: Option<egui::Color32>,
    pub(super) badges: Vec<String>,
    pub(super) badge_icons: Vec<Option<std::path::PathBuf>>,
    pub(super) badge_info: String,
    pub(super) user_id: String,
    /// How many messages from this user are in the currently-loaded log, and
    /// the earliest one's timestamp — computed once when the card opens by
    /// scanning the already-loaded `ChatLog`, not re-scanned per frame.
    pub(super) message_count: usize,
    pub(super) first_seen_secs: Option<f64>,
    /// Up to the last 50 messages from this user in the currently-loaded
    /// log, oldest first — a local "recent activity" feed, no network
    /// involved. Same scan pass as `message_count`/`first_seen_secs`.
    pub(super) recent_messages: Vec<(f64, String)>,
    /// Human-readable summary lines cross-referencing this user's Twitch
    /// login against the channel's locally-recorded `stream_event` history
    /// (bits/gift-subs/raids/timeouts) — computed once from the DB when the
    /// card opens, not re-queried per frame. Empty when nothing matched.
    pub(super) channel_stats: Vec<String>,
    /// Which platform's chat this card was opened from — decides whether the
    /// live Twitch lookup runs at all, and which profile link is offered.
    pub(super) platform: ChatPlatform,
    /// Every moderation action this channel has on record against them,
    /// newest first (`Store::moderation_events_for_user`), and the state
    /// derived from it. Queried once on open, like `channel_stats`.
    pub(super) moderation: Vec<crate::models::StreamEventRow>,
    pub(super) mod_summary: crate::models::ModerationSummary,
    pub(super) fetch: Arc<Mutex<UserCardFetch>>,
}

/// One entry in the Users-in-chat panel: everything needed to open that
/// person's usercard on click (same shape as a username click,
/// [`UserCardClick`]) plus their role-grouping label.
pub(super) struct ChatUserEntry {
    pub(super) click: UserCardClick,
    /// Section this user is listed under — highest-priority badge on their
    /// LATEST message in the log (so a promotion mid-broadcast reflects
    /// their current role, not whatever they were when they first spoke).
    pub(super) role: &'static str,
}

/// State for the 👥 "Users in chat" panel — the set of unique Twitch
/// chatters in the currently-loaded log, grouped by role. Built by scanning
/// the log once (not per frame); [`Self::stale`] says when to rebuild (the
/// message count grew — e.g. a live tail-reload appended new messages).
pub(super) struct UsersPanelState {
    pub(super) filter: String,
    pub(super) entries: Vec<ChatUserEntry>,
    /// `ChatLog::messages.len()` when `entries` was last built.
    pub(super) built_at_count: usize,
}

/// Global chat-replay settings shared across every open chat window (and
/// the Settings dialog's Display section) — replaces what used to be 8
/// separate `StreamArchiverApp` fields (`render_emotes`/`animate_emotes`/
/// `fetch_unknown_emotes`/`fetch_usercard_info`/`chat_font_pt`/
/// `chat_emote_pt`/`chat_ts_color`/`chat_text_color`). One `Arc<Mutex<>>` on
/// `self`, cloned into every `ChatPopup` at open time — a change from
/// either the Settings window or one chat window's own ⚙ panel is visible
/// to every other open window immediately, since they all lock the SAME
/// instance.
pub(super) struct ChatSettingsState {
    pub(super) render_emotes: bool,
    pub(super) animate_emotes: bool,
    /// Whether clicking an emote in chat Gigantifies it — see
    /// [`K_CHAT_GIGANTIFY`]'s doc for what that means and doesn't mean here.
    pub(super) gigantify_enabled: bool,
    pub(super) fetch_unknown_emotes: bool,
    pub(super) fetch_usercard_info: bool,
    pub(super) font_pt: f32,
    /// Timestamp size relative to `font_pt` — see [`K_CHAT_TS_SIZE_OFFSET`].
    pub(super) ts_size_offset: f32,
    /// Vertical gap between rows, in pixels — see [`K_CHAT_ROW_SPACING`].
    pub(super) row_spacing: f32,
    pub(super) emote_pt: f32,
    /// Target height for "wide" emotes (decoded aspect ratio over
    /// [`crate::ui::chat::emotes::WIDE_EMOTE_ASPECT_THRESHOLD`]) — separate
    /// from `emote_pt` because a single size + a fixed max-width cap
    /// crushes wide emotes short of their configured height before regular
    /// ones are affected at all. See [`K_CHAT_EMOTE_WIDE_PT`].
    pub(super) emote_wide_pt: f32,
    pub(super) ts_color: egui::Color32,
    pub(super) text_color: egui::Color32,
    /// Feature switch for the Hype Train card — see [`K_CHAT_SHOW_HYPE`].
    /// Distinct from `ChatPopup::show_hype`, which is one window's own
    /// collapse for this session.
    pub(super) show_hype_train: bool,
    /// Feature switch for the channel-info card (top supporters, goals) —
    /// see [`K_CHAT_SHOW_INFO`].
    pub(super) show_channel_info: bool,
    /// Chat font family by display name (`""` = follow the app font). See
    /// [`K_CHAT_FONT_FAMILY`].
    pub(super) chat_font: String,
    /// The DEFAULT wall-clock vs stream-relative timestamp mode — see
    /// [`ChatTsMode`]. Per-instance overrides live in `ts_mode_by_monitor`;
    /// read them together via [`ChatSettingsState::ts_mode_for`].
    pub(super) ts_mode: ChatTsMode,
    /// Per-instance overrides of `ts_mode`, keyed by monitor id. Absent =
    /// inherit the default.
    pub(super) ts_mode_by_monitor: HashMap<i64, ChatTsMode>,
    /// Whether 7TV gradient usernames render — see [`crate::cosmetics`].
    pub(super) render_paints: bool,
    /// Where the Creator Goal bar's fill comes from — see [`GoalColor`].
    pub(super) goal_color: GoalColor,
    /// Where the send button's fill comes from — see [`SendButtonColor`].
    pub(super) send_button_color: SendButtonColor,
    /// The custom highlight rules. Live here rather than snapshotted per
    /// window: a rule added while a chat window is open has to start
    /// accenting rows immediately, without reopening it.
    pub(super) highlight_rules: Vec<crate::chat_highlight::HighlightRule>,
}

impl ChatSettingsState {
    pub(super) fn load(store: &crate::store::Store) -> ChatSettingsState {
        let flag = |key: &str, default: bool| {
            store
                .get_setting(key)
                .ok()
                .flatten()
                .map(|v| if default { v != "0" } else { v == "1" })
                .unwrap_or(default)
        };
        ChatSettingsState {
            // Inline/animated emotes and the unknown-emote CDN fetch all
            // default on; only an explicit "0" disables each.
            render_emotes: flag(K_RENDER_EMOTES, true),
            animate_emotes: flag(K_ANIMATE_EMOTES, true),
            gigantify_enabled: flag(K_CHAT_GIGANTIFY, true),
            fetch_unknown_emotes: flag(K_FETCH_UNKNOWN_EMOTES, true),
            // Live Twitch usercard lookup defaults OFF — unlike the emote/
            // badge fetchers this hits the network on every usercard open,
            // not just once per missing asset.
            fetch_usercard_info: flag(K_FETCH_USERCARD_INFO, false),
            font_pt: store
                .get_setting(K_CHAT_FONT_PT)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(CHAT_FONT_PT_DEFAULT),
            emote_pt: store
                .get_setting(K_CHAT_EMOTE_PT)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(CHAT_EMOTE_PT_DEFAULT),
            emote_wide_pt: store
                .get_setting(K_CHAT_EMOTE_WIDE_PT)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(CHAT_EMOTE_WIDE_PT_DEFAULT),
            ts_size_offset: store
                .get_setting(K_CHAT_TS_SIZE_OFFSET)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(CHAT_TS_SIZE_OFFSET_DEFAULT),
            row_spacing: store
                .get_setting(K_CHAT_ROW_SPACING)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(CHAT_ROW_SPACING_DEFAULT),
            ts_color: store
                .get_setting(K_CHAT_TS_COLOR)
                .ok()
                .flatten()
                .and_then(|v| parse_chat_hex_color(&v))
                .unwrap_or(egui::Color32::WHITE),
            text_color: store
                .get_setting(K_CHAT_TEXT_COLOR)
                .ok()
                .flatten()
                .and_then(|v| parse_chat_hex_color(&v))
                .unwrap_or(egui::Color32::WHITE),
            // Both cards default on: they're the Twitch-parity furniture, and
            // each one already hides itself when the broadcast has nothing to
            // put in it.
            show_hype_train: flag(K_CHAT_SHOW_HYPE, true),
            show_channel_info: flag(K_CHAT_SHOW_INFO, true),
            chat_font: store.get_setting(K_CHAT_FONT_FAMILY).ok().flatten().unwrap_or_default(),
            render_paints: crate::cosmetics::render_paints(store),
            highlight_rules: crate::chat_highlight::load_rules(store),
            goal_color: GoalColor::parse(
                &store.get_setting(K_CHAT_GOAL_COLOR).ok().flatten().unwrap_or_default(),
            ),
            send_button_color: SendButtonColor::parse(
                &store.get_setting(K_CHAT_SEND_BUTTON_COLOR).ok().flatten().unwrap_or_default(),
            ),
            ts_mode: ChatTsMode::parse(
                store.get_setting(K_CHAT_TS_MODE).ok().flatten().unwrap_or_default().as_str(),
            ),
            ts_mode_by_monitor: store
                .get_setting(K_CHAT_TS_MODE_BY_MONITOR)
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
                .map(|m| {
                    m.into_iter()
                        .filter_map(|(k, v)| Some((k.parse().ok()?, ChatTsMode::parse(&v))))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

impl ChatSettingsState {
    /// The timestamp mode one instance should use: its own override, else the
    /// global default.
    pub(super) fn ts_mode_for(&self, monitor_id: i64) -> ChatTsMode {
        self.ts_mode_by_monitor.get(&monitor_id).copied().unwrap_or(self.ts_mode)
    }

    /// Set one instance's mode, and persist.
    ///
    /// Choosing the default DELETES the override rather than storing a copy of
    /// it — so an instance that follows the default keeps following it when
    /// the default later changes, instead of being silently pinned to what it
    /// happened to be at the time. Same reasoning as the scoped capture
    /// settings' `is_inherit`.
    pub(super) fn set_ts_mode_for(
        &mut self,
        store: &crate::store::Store,
        monitor_id: i64,
        mode: ChatTsMode,
    ) {
        if mode == self.ts_mode {
            self.ts_mode_by_monitor.remove(&monitor_id);
        } else {
            self.ts_mode_by_monitor.insert(monitor_id, mode);
        }
        let map: HashMap<String, &str> = self
            .ts_mode_by_monitor
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_str()))
            .collect();
        if let Ok(json) = serde_json::to_string(&map) {
            let _ = store.set_setting(K_CHAT_TS_MODE_BY_MONITOR, &json);
        }
    }
}

/// How far behind the live chat this window is, and how fresh that answer is.
///
/// Sampled ONLY when new messages actually land, never per frame. Age of the
/// newest message looks like lag but isn't: on a quiet chat it just counts up
/// while nothing is wrong. What we can honestly measure is "when a message
/// reached this window, how old was it" — so that is what this records.
#[derive(Clone, Copy, Default)]
pub(super) struct ChatLag {
    /// Milliseconds between Twitch stamping the newest message and this
    /// window having it. `None` until the first batch arrives.
    pub(super) ms: Option<i64>,
    /// Message count at the last sample, to notice the next batch.
    pub(super) at_count: usize,
    /// When that sample was taken, so a stale one can be shown as such
    /// rather than presented as current.
    pub(super) sampled_at: Option<std::time::Instant>,
}

/// A lag sample older than this is stale — chat has simply been quiet, and
/// the number says nothing about right now.
pub(super) const CHAT_LAG_STALE_SECS: u64 = 30;

impl ChatLag {
    /// Note the log's current state, sampling if new messages arrived.
    ///
    /// `newest_ms` is the newest message's `tmi-sent-ts`. A non-positive
    /// value (a pre-feature log with no absolute time) records nothing —
    /// better no number than a wrong one.
    pub(super) fn observe(&mut self, count: usize, newest_ms: f64, now_ms: i64) {
        if count == self.at_count || count == 0 {
            return;
        }
        self.at_count = count;
        if newest_ms <= 0.0 {
            return;
        }
        // Clamped at zero: this compares OUR clock against Twitch's, and a
        // machine running fast would otherwise report a negative lag.
        self.ms = Some((now_ms - newest_ms as i64).max(0));
        self.sampled_at = Some(std::time::Instant::now());
    }

    /// `(text, stale)` for the status row, or `None` when nothing has been
    /// measured yet.
    pub(super) fn label(&self) -> Option<(String, bool)> {
        let ms = self.ms?;
        let stale = self
            .sampled_at
            .is_none_or(|t| t.elapsed().as_secs() >= CHAT_LAG_STALE_SECS);
        let secs = ms as f64 / 1000.0;
        let text = if secs < 1.0 {
            "under 1s behind".to_string()
        } else if secs < 60.0 {
            format!("{secs:.1}s behind")
        } else {
            format!("{}m behind", (secs / 60.0).round() as i64)
        };
        Some((text, stale))
    }
}

/// What the lag figure means, in full. Long because every part of it is a
/// question someone will ask on seeing the number.
pub(super) const CHAT_LAG_HOVER: &str =
    "How far behind the live chat this window is: when the newest messages arrived, this is      how old Twitch said they were.

     The replay reads a file rather than holding its own connection — chat is captured by      an IRC client that buffers to disk every 2s, and this window re-reads that file every      3s — so a couple of seconds is normal and unavoidable without changing that design.      Notifications do NOT go through this path: a mention pings from the capture client      immediately, even though its row shows up here a moment later.

     Measured only when new messages land, so a quiet chat shows the last real      measurement rather than counting up. It compares this machine's clock against      Twitch's, so a badly-set system clock will show here as lag that isn't real.";

/// Twitch's own send-button purple — the default fill for the "Chat"-style
/// send button, same tone as `rows::TWITCH_PURPLE`.
pub(super) const SEND_BUTTON_COLOR: egui::Color32 = egui::Color32::from_rgb(0x91, 0x47, 0xff);

/// Where the send button's fill colour comes from — same idiom as
/// [`GoalColor`], one flavour of "Fixed | inherit the channel's accent" per
/// customizable chat accent rather than a single generic type, matching how
/// the rest of this module keeps each setting's meaning explicit in its own
/// name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SendButtonColor {
    Fixed(egui::Color32),
    /// The channel's own display colour — the same one the Streams grid, the
    /// notifications feed, and (optionally) the goal bar give it.
    Channel,
}

impl SendButtonColor {
    pub(super) fn parse(s: &str) -> SendButtonColor {
        match s.trim() {
            "channel" => SendButtonColor::Channel,
            hex => SendButtonColor::Fixed(parse_chat_hex_color(hex).unwrap_or(SEND_BUTTON_COLOR)),
        }
    }

    pub(super) fn as_setting(self) -> String {
        match self {
            SendButtonColor::Channel => "channel".to_string(),
            SendButtonColor::Fixed(c) => hex_color_string(c),
        }
    }

    /// Resolve to an actual colour — `channel` is this window's channel
    /// colour, snapshotted at open (see `ChatPopup::channel_color`'s doc for
    /// why: computing it needs `&mut self`, which the deferred render
    /// closure doesn't have).
    pub(super) fn resolve(self, channel: egui::Color32) -> egui::Color32 {
        match self {
            SendButtonColor::Fixed(c) => c,
            SendButtonColor::Channel => channel,
        }
    }
}

/// The chat window's send bar. See [`crate::chat_send`] for the transport and
/// the rate policy; this is only what the window needs to draw and drive it.
pub(super) struct SendBar {
    pub(super) draft: String,
    /// Local send budget — purely client-side bookkeeping.
    pub(super) limiter: crate::chat_send::SendLimiter,
    /// The channel's Twitch id, resolved once in the background on open.
    /// `None` until it lands; the bar is disabled meanwhile.
    pub(super) broadcaster_id: Arc<Mutex<Option<String>>>,
    /// The last send's result, shown under the box.
    pub(super) status: Arc<Mutex<Option<crate::chat_send::SendOutcome>>>,
    /// True while a send is in flight.
    pub(super) sending: Arc<std::sync::atomic::AtomicBool>,
    /// Messages sent from here that haven't come back through the sidecar
    /// yet: `(text, sent_at_unix)`. Drawn as pending rows at reduced alpha
    /// and dropped once the tail-reload returns them — without this the
    /// round trip (IRC → the logger's 2s flush → the window's 3s tail poll)
    /// is 2-5 seconds of apparent silence, which reads as broken.
    pub(super) pending: Vec<(String, i64)>,
    /// Whether the emote picker panel is showing.
    pub(super) picker_open: bool,
    /// The picker's own search box.
    pub(super) picker_filter: String,
    /// Which `:code` suggestion is selected. Clamped to the match count each
    /// frame rather than reset, so it survives typing another character.
    pub(super) complete_sel: usize,
    /// The query Esc dismissed, so the list stays down for THAT word but
    /// comes back for the next one. Empty = nothing dismissed.
    pub(super) complete_dismissed: String,
    /// Which `@name` suggestion is selected — same shape as `complete_sel`,
    /// for the mention autocomplete instead of the emote one.
    pub(super) mention_sel: usize,
    /// Same as `complete_dismissed`, for the mention autocomplete.
    pub(super) mention_dismissed: String,
    /// Sent messages from this window, oldest first — Up/Down recall.
    pub(super) history: Vec<String>,
    /// Index into `history` currently shown in the draft box. `None` means
    /// "not browsing": the box holds either the live unsent draft, or
    /// nothing has been recalled yet this session.
    pub(super) history_pos: Option<usize>,
    /// The draft as it was the moment history browsing started (the first
    /// Up press), so paging back down past the newest entry restores
    /// exactly what was being typed instead of losing it.
    pub(super) history_stash: String,
}

/// How long a pending row waits for its own message to come back before it
/// gives up and says so. It can legitimately never arrive — chat capture may
/// not be running for this channel at all — and a row stuck at half opacity
/// forever would be worse than an honest note.
pub(super) const PENDING_TIMEOUT_SECS: i64 = 30;

pub(super) struct ChatPopup {
    /// Monitor this window belongs to — keys the viewport id, so each channel
    /// gets its OWN chat window (opening another channel's chat no longer
    /// replaces the one already open).
    pub(super) monitor_id: i64,
    pub(super) monitor_name: String,
    /// Snapshot of the monitor's platform at popup-open — the deferred
    /// closure can't reach `self.rows` to look this up itself, and the
    /// platform of an existing monitor never changes mid-session.
    pub(super) is_twitch: bool,
    /// Currently-viewed recording (`None` = monitor has no recordings at all).
    pub(super) recording: Option<Recording>,
    pub(super) all_recordings: Vec<Recording>,
    pub(super) load_state: Arc<Mutex<ChatLoadState>>,
    pub(super) search: String,
    /// When `true`: show the entire log from the top (no cap, stick-to-bottom off).
    /// When `false` (default): show the last 500 msgs and stick to bottom.
    pub(super) full_view: bool,
    /// When `true`, hide messages whose `source_name` is non-empty (i.e. came
    /// from a different channel during an active Shared Chat session) — shows
    /// only this channel's own messages. Per-window, ephemeral (not
    /// persisted), like `full_view`.
    pub(super) hide_shared: bool,
    /// Twitch login currently highlighted (via a usercard's 🔔 "Highlight
    /// messages of this user"), if any — at most one at a time, matching
    /// Twitch's own popout chat. Matched case-sensitively against
    /// `ChatMessage::login` (already lowercased at parse time).
    pub(super) highlight_login: Option<String>,
    /// Whether the ⚙ "Chat Appearance" panel is currently open for this
    /// window. Per-window UI state; the values it edits
    /// (`StreamArchiverApp::chat_font_pt`/`chat_ts_color`/`chat_text_color`)
    /// are global/shared, same as `render_emotes`.
    pub(super) show_appearance: bool,
    /// Editable `#RRGGBB` text buffers backing the Chat Appearance panel's
    /// hex fields — lets a color be pasted in directly, not just picked from
    /// the wheel/sliders (egui's stock color picker has no paste target of
    /// its own). Re-synced from the live color whenever the panel opens or
    /// the swatch picker itself changes it; left alone otherwise so in-
    /// progress typing isn't clobbered.
    pub(super) ts_color_hex: String,
    pub(super) text_color_hex: String,
    /// The currently-open usercard, if any — at most one per chat window.
    pub(super) user_card: Option<UserCardPopup>,
    /// The 👥 Users-in-chat panel, if open — `None` when closed (the panel
    /// content isn't kept around while hidden).
    pub(super) users_panel: Option<UsersPanelState>,
    /// What the info strips above the message list draw — top supporters and
    /// the most recent Hype Train, from `store::stream_event`. Local DB only,
    /// no network. Computed once on popup-open/recording-switch, refreshed on
    /// the same cadence as the live tail-reload while the recording is still
    /// going. See [`BroadcastStats`].
    pub(super) stats: BroadcastStats,
    /// Per-window, per-session collapse for the Hype Train card. Distinct
    /// from `ChatSettingsState::show_hype_train`, which is the feature
    /// switch: this is "not right now, in this window", the same shape as
    /// `full_view`/`hide_shared`. A new train re-opens it (see
    /// `hype_seen_id`) but can never override the feature switch.
    pub(super) show_hype: bool,
    /// Same, for the channel-info card (top supporters, goals).
    pub(super) show_info: bool,
    /// The connected Twitch login at the time this window opened, for
    /// accenting rows that name you. The RULES live on the shared settings
    /// (see `ChatSettingsState::highlight_rules`) so an edit applies at once;
    /// the login is snapshotted because it only changes when an account is
    /// connected, which is a restart-shaped event anyway.
    ///
    /// Accenting only — the notification half runs in the live chat logger
    /// (see [`crate::chat_highlight`]), so a ping never depends on a window
    /// being open.
    pub(super) my_login: String,
    /// This channel's own display colour, snapshotted at open for the
    /// "inherit channel colour" goal-bar option — computing it needs
    /// `&mut self`, which the deferred render closure doesn't have.
    pub(super) channel_color: egui::Color32,
    /// 7TV paints for the chatters in this log, keyed by Twitch user id.
    /// Behind a mutex because a background fetch fills it in while the window
    /// renders. Empty for YouTube, and when the feature is off.
    pub(super) paints: Arc<Mutex<HashMap<String, crate::cosmetics::Paint>>>,
    /// User ids already asked about — including ones with NO paint, so a miss
    /// isn't re-requested every few minutes.
    pub(super) paints_asked: Arc<Mutex<std::collections::HashSet<String>>>,
    /// How far behind the live chat this window is — see [`ChatLag`].
    pub(super) lag: ChatLag,
    /// When the last paint fetch ran, so the sweep is coalesced rather than
    /// firing on every 3s tail tick.
    pub(super) paints_checked: Option<std::time::Instant>,
    /// `(channel name, account)` — where the paint cache is written back to.
    pub(super) paints_key: (String, String),
    /// The message being typed in the send bar, and everything about whether
    /// it may go out. `None` when this window can't send at all (not Twitch,
    /// an archived take, no connected account) — the bar isn't drawn then,
    /// rather than sitting permanently disabled on every historical view.
    pub(super) send: Option<SendBar>,
    /// The `train_id` of the most recent Hype Train this window has already
    /// reacted to. A different id while the train is running means one just
    /// STARTED, which re-opens `show_hype` — the user asked to be shown a new
    /// train even if they'd collapsed the last one.
    pub(super) hype_seen_id: String,
    /// When the popup last triggered a background re-read of the chat file.
    /// Used to tail a live recording: the file is re-parsed every few seconds
    /// while `recording.ended_at` is `None`.
    pub(super) last_reload: std::time::Instant,
    /// Third-party emote code → resolved on-disk image path, built ONCE on
    /// popup-open from the channel's BTTV/FFZ/7TV manifests (case-sensitive keys).
    /// Empty for YouTube / when chat assets aren't fetched. `Arc` so the
    /// background (re)parse tasks share it without rebuilding per tick.
    pub(super) emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    /// The same emotes as `emote_map`, grouped and in display order, plus
    /// Twitch's first-party ones — what the emote picker and the `:code`
    /// autocomplete offer. Built once at open from the same manifest read.
    pub(super) emote_catalog: Arc<Vec<CatalogEmote>>,
    /// `…/{channel}/twitch/emotes/twitch/` — Twitch first-party emotes are
    /// id-keyed (resolved as `{id}.png` at parse time). `None` for YouTube.
    pub(super) twitch_emote_dir: Option<std::path::PathBuf>,
    /// `{filename stem} -> path` index over every OTHER cached channel's
    /// Twitch emote dir — the fallback lookup for an id missing from
    /// `twitch_emote_dir`. Twitch lets any subscriber use their sub emotes in
    /// any chat, so a message routinely references an emote this app only
    /// ever fetched for a different monitored channel. A precomputed index
    /// (not a directory list probed per occurrence) — see
    /// `assets::index_emote_stems`'s doc for why: probing several dozen
    /// channels' worth of dirs per repeated emote occurrence made a large
    /// chat log take over a minute to load. Built ONCE on popup-open; empty
    /// for YouTube. `Arc` for the same reason as `emote_map`.
    pub(super) twitch_fallback_index: Arc<HashMap<String, std::path::PathBuf>>,
    /// This channel's badge icon dirs (channel-specific + shared global) —
    /// see [`TwitchBadgeDirs`]. Channel-level like `emote_map`: built ONCE on
    /// popup-open, reused unchanged across recording switches within the
    /// same popup (badges aren't per-broadcast). Empty for YouTube.
    pub(super) twitch_badge_dirs: Arc<TwitchBadgeDirs>,
    /// The channel's cached channel-point reward titles, keyed by reward id
    /// (`rewards.json`, written by the asset fetcher). Built ONCE on popup
    /// open, like `emote_map` — IRC only ever names a reward by its UUID, so
    /// without this a redemption row can't say what was redeemed. Empty when
    /// assets haven't been fetched, or for a non-Twitch monitor.
    pub(super) rewards: Arc<HashMap<String, crate::assets::RewardEntry>>,
    /// This take's recorded Shared Chat / collab partners, keyed by Twitch
    /// broadcaster id (`store::collab::collab_partners_for_stream`) — resolves
    /// each message's raw `source_room_id` tag to a name for the "which
    /// channel was this from" indicator. Built ONCE on popup-open from
    /// `recording.stream_id`; empty when the recording has no stream id or no
    /// collab was ever recorded for it (messages then render with no
    /// indicator, same as a pre-feature log). `Arc` for the same reason as
    /// `emote_map` — shared with the background (re)parse tasks.
    pub(super) source_partners: Arc<HashMap<String, crate::models::CollabPartner>>,
    /// Snapshot of the "Fetch unknown emotes from Twitch" setting at
    /// popup-open — when true, a first-party id missing from BOTH
    /// `twitch_emote_dir` and `twitch_fallback_index` gets fetched straight
    /// from Twitch's CDN by id (see `assets::twitch_emote_cdn_fetch`),
    /// independent of channel monitoring. A separate toggle from
    /// `render_emotes` by design — the latter covers purely-local rendering,
    /// this one gates a NEW network fetch for channels not otherwise
    /// archived here.
    pub(super) fetch_unknown_emotes: bool,
    /// True while a background emoji-download pass is running, so the 3s tail-reload
    /// doesn't pile up overlapping download passes for the same chat.
    pub(super) loading: Arc<AtomicBool>,
    /// Consecutive tail-reloads spent in `ChatLoadState::Error`. An errored
    /// reload retries with a FULL re-read of the sidecar (potentially hundreds
    /// of MB on the recordings drive), so the retry interval backs off
    /// exponentially instead of re-reading every 3 seconds forever. Reset on a
    /// successful load.
    pub(super) error_retries: u32,
    /// Cached search-filter result: (lowercased query, message count, and
    /// `hide_shared` when computed, matching message indices). Recomputed
    /// only when the query, message count, or `hide_shared` changes — the
    /// filter used to lowercase every message every frame.
    pub(super) filter_cache: Option<(String, usize, bool, Vec<u32>)>,
    /// Shared with every other open chat window and the Settings dialog —
    /// see [`ChatSettingsState`].
    pub(super) settings: Arc<Mutex<ChatSettingsState>>,
    /// Set by the deferred closure on close; read back by `chat_popup_window`
    /// next call.
    pub(super) closed: bool,
    /// Emote decode-cache misses queued this render pass — drained by
    /// `chat_popup_window`'s wrapper via `pump_emote_decodes`, same as
    /// before the migration, just relocated from a captured local.
    pub(super) decode_misses: Vec<std::path::PathBuf>,
    /// A username click this render pass, consumed by the wrapper to spawn
    /// the (optional) live usercard lookup — same "mutate inside the
    /// closure, consume after" shape `decode_misses` already used.
    pub(super) usercard_click: Option<UserCardClick>,
    /// A username context-menu action this render pass ("Reply" / "Open
    /// Properties") — same stash-then-consume shape as `usercard_click`.
    pub(super) row_action: Option<RowMenuAction>,
    /// Emote occurrences the user has clicked to Gigantify, keyed by
    /// `(msg_id, segment index within that message)` — the segment index
    /// disambiguates the same emote code repeated in one message ("Kappa
    /// Kappa Kappa"), toggling only the one actually clicked. Per-window,
    /// ephemeral: unlike almost everything else here this isn't behind
    /// `ChatSettingsState`, since it's "I want to look at THIS one bigger
    /// right now", not a preference to remember. Mutated directly inside the
    /// deferred render closure — unlike `row_action`, nothing here needs
    /// `&mut self`, so there's no stash-and-consume-later step.
    pub(super) gigantified: std::collections::HashSet<(String, usize)>,
    /// Twitch login (lowercased) -> monitor id, for every Twitch channel this
    /// app monitors. Built ONCE on popup-open from `self.rows`, like
    /// `emote_map` — decides whether a username's context menu offers "Open
    /// Properties" (only chatters who are themselves a monitored channel,
    /// e.g. during a raid or a Shared Chat collab, get the option).
    pub(super) channel_by_login: Arc<HashMap<String, i64>>,
    /// `egui::InputState::time` up to which the log's stick-to-bottom is held
    /// off, even though it would otherwise engage. Refreshed to "now + a
    /// short grace" on every frame the primary mouse button is down anywhere
    /// — a live chat that keeps snapping to the newest message the instant
    /// one arrives yanks the viewport (and any in-progress drag-selection)
    /// out from under a user who's mid-select, which reads as "chat cancels
    /// my highlighted text." The grace period after release, not just while
    /// held, gives a completed selection time to survive long enough to
    /// Ctrl+C before the next message re-engages the snap.
    pub(super) pause_stick_until: f64,
}

/// The deferred viewport one monitor's chat window lives in.
///
/// One function rather than an inline `from_hash_of` at each use, because two
/// unrelated places depend on agreeing: the window is *declared* under this id,
/// and background sidecar loads have to *wake* it by the same id. A mismatch
/// wakes a viewport nothing draws — silently, and only on a live channel the
/// user isn't touching, which is exactly how it went unnoticed.
/// The chat popup's OS window title. One function on purpose: the window
/// dock finds the popup's HWND by EXACT title match, so the builder
/// (`chat_popup_window`) and dock-on-play (`reconcile_chat_dock_on_play`)
/// must agree to the byte — two format strings drifting apart would leave
/// every auto-dock stuck Pending until its 60 s timeout.
pub(in crate::ui) fn chat_window_title(name: &str) -> String {
    format!("💬  Chat — {name}")
}

pub(in crate::ui) fn chat_vp_id(monitor_id: i64) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("chat_popup_vp", monitor_id))
}

// The window and its parts live in `src/ui/chat/`; this file keeps the
// shared types every one of them needs. Each submodule is a pure move out
// of what used to be one 5,000-line file — same items, same bodies.
mod colors;
mod compose;
mod emotes;
mod helpers;
mod parse;
mod rows;
mod strips;
mod usercard;
mod window;

pub(in crate::ui) use colors::*;
pub(in crate::ui) use compose::*;
pub(crate) use emotes::*;
pub(crate) use helpers::*;
pub(crate) use parse::*;
pub(in crate::ui) use rows::*;
pub(in crate::ui) use strips::*;
pub(in crate::ui) use usercard::*;

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use std::path::PathBuf;

    fn empty_badge_dirs() -> TwitchBadgeDirs {
        TwitchBadgeDirs { channel: None, global: PathBuf::new() }
    }

    fn stream_event(kind: &str, actor: &str, amount: i64) -> crate::models::StreamEventRow {
        crate::models::StreamEventRow {
            id: 0,
            monitor_id: 1,
            at: 0,
            stream_id: String::new(),
            kind: kind.to_string(),
            actor: actor.to_string(),
            target: String::new(),
            amount,
            tier: String::new(),
            detail: String::new(),
            goal: 0,
            expires_at: 0,
            level: 0,
        }
    }

    #[test]
    fn summarize_user_events_matches_actor_case_insensitively_and_ignores_others() {
        let events = vec![
            stream_event("bits", "CoolViewer", 100),
            stream_event("bits", "coolviewer", 50),
            stream_event("subgift", "CoolViewer", 3),
            stream_event("raid_in", "SomeoneElse", 20),
        ];
        let lines = summarize_user_events(&events, "coolVIEWER");
        assert!(lines.iter().any(|l| l.contains("150 bits") && l.contains("2 message")));
        assert!(lines.iter().any(|l| l.contains("3 sub(s) gifted")));
        assert!(!lines.iter().any(|l| l.contains("Raided")));
    }

    #[test]
    fn summarize_user_events_empty_when_no_match() {
        let events = vec![stream_event("bits", "Someone", 10)];
        assert!(summarize_user_events(&events, "NobodyHere").is_empty());
    }

    #[test]
    fn load_broadcast_stats_ranks_gifters_cheerers_and_surfaces_hype_trains() {
        use crate::store::test_util::sample_monitor;
        let store = crate::store::Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mid = store.insert_monitor(&sample_monitor(cid)).unwrap();

        store.record_stream_event(mid, 100, "s1", "subgift", "Alice", "", 5, "", "").unwrap();
        store.record_stream_event(mid, 110, "s1", "subgift", "Bob", "", 2, "", "").unwrap();
        store.record_stream_event(mid, 120, "s1", "bits", "Alice", "", 300, "", "").unwrap();
        // Outside the queried window — must not be counted.
        store.record_stream_event(mid, 9_999, "s1", "bits", "Alice", "", 5_000, "", "").unwrap();
        // Two distinct trains during this broadcast — only the LATEST
        // (by start time) should come back, not a full history.
        store
            .upsert_hype_train_event(mid, 20, "s1", "train0", 1000, "level 1 · 1,000 pts (confirmed)", 1000, 500, 1)
            .unwrap();
        store
            .upsert_hype_train_event(mid, 105, "s1", "train1", 4200, "level 3 · 4,200 pts (confirmed)", 5000, 400, 3)
            .unwrap();

        let stats = load_broadcast_stats(&store, mid, 0, 200);
        assert!(!stats.is_empty());
        assert_eq!(stats.top_gifters, vec![("Alice".to_string(), 5), ("Bob".to_string(), 2)]);
        assert_eq!(stats.top_cheerers, vec![("Alice".to_string(), 300)]);
        let train = stats.hype_train.expect("latest train present");
        assert_eq!(train.detail, "level 3 · 4,200 pts (confirmed)");
        assert_eq!((train.level, train.total, train.goal, train.expires_at), (3, 4200, 5000, 400));

        // A monitor with no events at all collapses the strips entirely
        // rather than drawing an empty card.
        let empty_mid = store.insert_monitor(&sample_monitor(cid)).unwrap();
        assert!(load_broadcast_stats(&store, empty_mid, 0, 200).is_empty());
    }

    fn train(goal: i64, expires_at: i64) -> HypeTrainDisplay {
        HypeTrainDisplay {
            detail: "level 3 · 4,200 pts (confirmed)".into(),
            train_id: "t1".into(),
            level: 3,
            total: 4200,
            goal,
            expires_at,
        }
    }

    /// The whole Hype Train card lifecycle, without a clock or a UI.
    #[test]
    fn hype_phase_runs_then_grace_then_hides_only_on_a_live_view() {
        let t = train(5000, 1_000);

        // Running: fraction of the way to the next level, seconds left.
        assert_eq!(
            hype_phase(&t, 940, true),
            HypePhase::Running { frac: 4200.0 / 5000.0, remaining: 60 }
        );

        // Just over: the grace window says so rather than vanishing mid-read.
        assert_eq!(hype_phase(&t, 1_000, true), HypePhase::Ended { since_secs: 0 });
        assert_eq!(
            hype_phase(&t, 1_000 + HYPE_ENDED_GRACE_SECS - 1, true),
            HypePhase::Ended { since_secs: HYPE_ENDED_GRACE_SECS - 1 }
        );

        // Past the grace window, a LIVE view hides it — that's the whole
        // point of the auto-hide.
        assert_eq!(hype_phase(&t, 1_000 + HYPE_ENDED_GRACE_SECS, true), HypePhase::Hidden);

        // …but an ARCHIVED take must keep showing it forever. This is an
        // archive tool; a three-week-old broadcast still had a Level 3 train,
        // and hiding it because wall-clock time passed would be a regression.
        assert_eq!(hype_phase(&t, 1_000 + HYPE_ENDED_GRACE_SECS, false), HypePhase::Summary);
        assert_eq!(hype_phase(&t, 99_999_999, false), HypePhase::Summary);
    }

    /// A row with no timing (pre-v86, or inference-only that GQL never
    /// confirmed) has nothing to count against, so it is always the static
    /// summary — never "ended", never hidden.
    #[test]
    fn hype_phase_untimed_rows_are_always_a_summary() {
        for t in [train(0, 1_000), train(5000, 0), train(0, 0)] {
            for live in [true, false] {
                assert_eq!(hype_phase(&t, 1_000_000, live), HypePhase::Summary);
                assert_eq!(hype_phase(&t, 0, live), HypePhase::Summary);
            }
        }
    }

    #[test]
    fn fmt_ago_reads_naturally_across_the_grace_window() {
        assert_eq!(fmt_ago(0), "just now");
        assert_eq!(fmt_ago(29), "just now");
        assert_eq!(fmt_ago(60), "1m ago");
        assert_eq!(fmt_ago(299), "4m ago");
        assert_eq!(fmt_ago(3_900), "1h 5m ago");
    }

    /// Both timestamp formats are always available: whichever isn't shown is
    /// on the hover, so the common one-off "what offset was that?" never
    /// needs the toggle at all.
    #[test]
    fn timestamp_modes_each_show_the_other_on_hover() {
        let mut m = plain_msg(2410.0, "bob", "id1", "hi");
        // 2026-08-08T17:30:00Z, as unix ms.
        m.ts_unix_ms = 1_786_296_600_000.0;
        let clock = fmt_chat_clock(m.ts_unix_ms).expect("absolute time present");

        let (shown, hover) = fmt_chat_ts_mode(&m, ChatTsMode::StreamRelative);
        assert_eq!(shown, "[00:40:10]");
        assert_eq!(hover, clock, "relative mode hovers the wall clock");

        let (shown, hover) = fmt_chat_ts_mode(&m, ChatTsMode::WallClock);
        assert_eq!(shown, clock);
        assert_eq!(hover, "[00:40:10] into the broadcast");
    }

    /// A pre-feature log has no absolute time. Wall-clock mode must fall back
    /// to the relative form rather than leaving the column blank.
    #[test]
    fn wall_clock_falls_back_when_the_log_has_no_absolute_time() {
        let m = plain_msg(2410.0, "bob", "id1", "hi");
        assert_eq!(m.ts_unix_ms, 0.0);
        assert_eq!(fmt_chat_clock(0.0), None);
        let (shown, hover) = fmt_chat_ts_mode(&m, ChatTsMode::WallClock);
        assert_eq!(shown, "[00:40:10]");
        assert!(hover.contains("No wall-clock time"));
    }

    #[test]
    fn timestamp_mode_round_trips_through_its_setting() {
        for m in [ChatTsMode::StreamRelative, ChatTsMode::WallClock] {
            assert_eq!(ChatTsMode::parse(m.as_str()), m);
        }
        // Unknown / unset falls back to the archive-friendly default.
        assert_eq!(ChatTsMode::parse(""), ChatTsMode::StreamRelative);
        assert_eq!(ChatTsMode::parse("nonsense"), ChatTsMode::StreamRelative);
    }

    /// Every notice kind gets an accent; an ordinary message gets none. Both
    /// emphases outrank the message's own kind — they were asked for, and
    /// losing one behind a sub notice would defeat the point of asking.
    #[test]
    fn row_decor_accents_notices_and_lets_an_explicit_highlight_win() {
        let v = egui::Visuals::dark();
        let none = RowEmphasis::None;
        let with = |n: Option<ChatNotice>| {
            let mut m = plain_msg(1.0, "bob", "id1", "hi");
            m.notice = n.map(Box::new);
            m
        };

        assert!(row_decor(&with(None), none, &v).accent.is_none(), "ordinary message");
        // A muted room event stays muted: an accent bar would give it more
        // weight than a sub, which it does not deserve.
        assert!(row_decor(&with(Some(ChatNotice::System)), none, &v).accent.is_none());

        for n in [
            ChatNotice::FirstMessage,
            ChatNotice::Redemption { reward: None, reward_id: "r".into(), cost: None },
            ChatNotice::Sub { system_msg: "subbed".into() },
            ChatNotice::Raid { system_msg: "raided".into() },
            ChatNotice::Announce { system_msg: "listen up".into() },
            ChatNotice::WatchStreak { system_msg: "streak".into() },
        ] {
            assert!(row_decor(&with(Some(n.clone())), none, &v).accent.is_some(), "{n:?}");
        }

        let sub = with(Some(ChatNotice::Sub { system_msg: "subbed".into() }));
        assert_eq!(
            row_decor(&sub, RowEmphasis::Chatter, &v).accent,
            Some(v.selection.bg_fill),
            "an explicit highlight outranks the notice kind"
        );
    }

    /// The "FIRST MESSAGE" corner chip is a fact about the message, not a
    /// substitute for a hit/chatter accent — it must survive even when one
    /// of those wins the row's fill and accent bar.
    #[test]
    fn first_message_corner_label_survives_an_emphasis_override() {
        let v = egui::Visuals::dark();
        let mut m = plain_msg(1.0, "bob", "id1", "hi");
        m.notice = Some(Box::new(ChatNotice::FirstMessage));

        assert_eq!(row_decor(&m, RowEmphasis::None, &v).corner_label.unwrap().0, "FIRST MESSAGE");
        assert_eq!(row_decor(&m, RowEmphasis::Hit, &v).corner_label.unwrap().0, "FIRST MESSAGE");
        assert_eq!(row_decor(&m, RowEmphasis::Chatter, &v).corner_label.unwrap().0, "FIRST MESSAGE");

        // An ordinary message never gets one, regardless of emphasis.
        let mut ordinary = plain_msg(1.0, "bob", "id2", "hi");
        ordinary.notice = None;
        assert!(row_decor(&ordinary, RowEmphasis::Hit, &v).corner_label.is_none());
    }

    /// A matched trigger and a watched chatter must not look the same, and the
    /// trigger must win.
    ///
    /// These were one boolean OR-ed together, which had two consequences the
    /// user hit: the two reasons were indistinguishable on screen, and the
    /// caller skipped the trigger check entirely for a watched chatter — so a
    /// rule firing on someone already being watched changed nothing visible.
    #[test]
    fn a_matched_trigger_is_distinct_from_a_watched_chatter_and_outranks_it() {
        let v = egui::Visuals::dark();
        let msg = plain_msg(1.0, "bob", "id1", "hi");

        let hit = row_decor(&msg, RowEmphasis::Hit, &v);
        let chatter = row_decor(&msg, RowEmphasis::Chatter, &v);
        assert_eq!(hit.accent, Some(HIT_COLOR));
        assert_eq!(chatter.accent, Some(v.selection.bg_fill));
        assert_ne!(hit.accent, chatter.accent, "the two reasons must be tellable apart");
        assert_ne!(hit.fill, chatter.fill);

        // And a hit is never invisible behind a notice kind either.
        let mut sub = msg;
        sub.notice = Some(Box::new(ChatNotice::Sub { system_msg: "subbed".into() }));
        assert_eq!(row_decor(&sub, RowEmphasis::Hit, &v).accent, Some(HIT_COLOR));
    }

    /// Only the kinds that REPLACE a message have a headline; the ones that
    /// merely decorate one must not, or the row would render Twitch's copy
    /// where the user's own message belongs.
    #[test]
    fn only_event_notices_have_a_headline() {
        assert_eq!(
            notice_headline(&ChatNotice::Sub { system_msg: "Bob subscribed".into() }),
            Some("Bob subscribed")
        );
        assert_eq!(notice_headline(&ChatNotice::FirstMessage), None);
        assert_eq!(
            notice_headline(&ChatNotice::Redemption {
                reward: Some("Hydrate!".into()),
                reward_id: "r".into(),
                cost: Some(50),
            }),
            None
        );
        assert_eq!(notice_headline(&ChatNotice::System), None);
        // An event whose system-msg tag was missing has nothing to say.
        assert_eq!(notice_headline(&ChatNotice::Raid { system_msg: String::new() }), None);
    }

    /// A painted name is one `LayoutJob` of flat-coloured runs. The cap is
    /// what keeps this affordable — egui does real layout work per run, and
    /// this happens for every painted sender on screen.
    #[test]
    fn a_painted_name_is_quantized_into_bounded_runs() {
        let paint = crate::cosmetics::Paint {
            name: "g".into(),
            angle: 90,
            stops: vec![
                crate::cosmetics::Stop { at: 0.0, rgba: [0, 0, 0, 255] },
                crate::cosmetics::Stop { at: 1.0, rgba: [255, 255, 255, 255] },
            ],
        };
        let font = egui::FontId::proportional(14.0);

        // A short name gets one run per character…
        let short = paint_name_job("bob:", &paint, font.clone());
        assert_eq!(short.sections.len(), 4);
        assert_eq!(short.text, "bob:");

        // …and a long one is chunked, never exceeding the cap.
        let long_name = "a".repeat(120);
        let long = paint_name_job(&long_name, &paint, font.clone());
        assert!(long.sections.len() <= MAX_PAINT_RUNS, "{} runs", long.sections.len());
        assert_eq!(long.text, long_name, "every character still renders");

        // The runs actually differ, i.e. it reads as a gradient.
        let colors: Vec<_> = long.sections.iter().map(|s| s.format.color).collect();
        assert!(colors.first() != colors.last(), "the sweep is visible");

        // Chunking must not split a multi-byte character.
        let cjk = "あいうえおかきくけこさしすせそたちつてと:";
        let job = paint_name_job(cjk, &paint, font);
        assert_eq!(job.text, cjk);
    }

    /// Each chat window must own a distinct viewport, and keep the id it has
    /// always had.
    ///
    /// Both halves matter and neither is cosmetic. A shared id would put every
    /// channel's chat in one OS window; a *changed* id would wake a viewport
    /// nothing is drawing, which is precisely the bug that had the replay
    /// sitting 30 s behind live — background loads were repainting the main
    /// window instead of the popup, so nothing surfaced until the pointer
    /// crossed it.
    #[test]
    fn every_chat_window_owns_a_distinct_stable_viewport() {
        assert_ne!(chat_vp_id(7), chat_vp_id(8));
        assert_eq!(chat_vp_id(7), chat_vp_id(7));
        // The literal the windows were first declared under. Changing it moves
        // every user's saved chat-window position, so it is pinned here rather
        // than left to whatever `chat_vp_id` happens to hash today.
        assert_eq!(chat_vp_id(7), egui::ViewportId::from_hash_of(("chat_popup_vp", 7_i64)));
    }

    /// Per-instance timestamp overrides, and the reason they are stored by
    /// ABSENCE: an instance following the default has to keep following it
    /// when the default later changes, rather than being silently pinned to
    /// whatever it happened to be at the time.
    #[test]
    fn timestamp_overrides_are_per_instance_and_inherit_by_absence() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let mut cs = ChatSettingsState::load(&store);
        cs.ts_mode = ChatTsMode::StreamRelative;
        cs.ts_mode_by_monitor.clear();

        // Everything inherits to start with.
        assert_eq!(cs.ts_mode_for(1), ChatTsMode::StreamRelative);
        assert_eq!(cs.ts_mode_for(2), ChatTsMode::StreamRelative);

        // Overriding ONE instance leaves the others alone — the whole point.
        cs.set_ts_mode_for(&store, 1, ChatTsMode::WallClock);
        assert_eq!(cs.ts_mode_for(1), ChatTsMode::WallClock);
        assert_eq!(cs.ts_mode_for(2), ChatTsMode::StreamRelative);

        // Setting an instance back to the default clears its override…
        cs.set_ts_mode_for(&store, 1, ChatTsMode::StreamRelative);
        assert!(cs.ts_mode_by_monitor.is_empty(), "matching the default is not an override");

        // …so changing the default later carries it, instead of leaving it
        // pinned to the old value.
        cs.ts_mode = ChatTsMode::WallClock;
        assert_eq!(cs.ts_mode_for(1), ChatTsMode::WallClock);

        // An instance that really did opt out stays opted out.
        cs.set_ts_mode_for(&store, 2, ChatTsMode::StreamRelative);
        assert_eq!(cs.ts_mode_for(2), ChatTsMode::StreamRelative);
        assert_eq!(cs.ts_mode_for(1), ChatTsMode::WallClock);

        // And it survives a reload from the store.
        let reloaded = ChatSettingsState::load(&store);
        assert_eq!(reloaded.ts_mode_by_monitor.get(&2), Some(&ChatTsMode::StreamRelative));
        assert_eq!(reloaded.ts_mode_by_monitor.get(&1), None);
    }

    #[test]
    fn goal_colour_round_trips_including_the_channel_sentinel() {
        assert_eq!(GoalColor::parse("channel"), GoalColor::Channel);
        assert_eq!(GoalColor::Channel.as_setting(), "channel");
        let c = egui::Color32::from_rgb(0x12, 0x34, 0x56);
        assert_eq!(GoalColor::parse("#123456"), GoalColor::Fixed(c));
        assert_eq!(GoalColor::parse(&GoalColor::Fixed(c).as_setting()), GoalColor::Fixed(c));
        // Unset / junk falls back to the default rather than erroring.
        assert_eq!(GoalColor::parse(""), GoalColor::Fixed(GOAL_COLOR));
        assert_eq!(GoalColor::parse("not a colour"), GoalColor::Fixed(GOAL_COLOR));
        // "Use the channel colour" only resolves to it when actually chosen.
        let ch = egui::Color32::from_rgb(9, 9, 9);
        assert_eq!(GoalColor::Channel.resolve(ch), ch);
        assert_eq!(GoalColor::Fixed(c).resolve(ch), c);
    }

    /// The distinction the whole indicator rests on: a chat nobody is talking
    /// in is not a chat we are behind on. Sampling only when the message
    /// count moves is what keeps those apart.
    #[test]
    fn lag_samples_only_when_new_messages_arrive() {
        let mut lag = ChatLag::default();
        // 10:00:00.000 exactly, in unix ms.
        let t0 = 1_786_000_000_000i64;

        // Nothing measured yet.
        assert!(lag.label().is_none());

        // Two messages land, the newest stamped 2.4s ago.
        lag.observe(2, (t0 - 2_400) as f64, t0);
        assert_eq!(lag.ms, Some(2_400));
        assert_eq!(lag.label().map(|(t, _)| t), Some("2.4s behind".to_string()));

        // Twenty seconds pass with NO new messages. The reading must not
        // drift upward — nothing is wrong, chat is just quiet.
        lag.observe(2, (t0 - 2_400) as f64, t0 + 20_000);
        assert_eq!(lag.ms, Some(2_400), "a quiet chat is not a lagging one");

        // A new message arrives, this one fresher.
        lag.observe(3, (t0 + 20_000 - 900) as f64, t0 + 20_000);
        assert_eq!(lag.ms, Some(900));
        assert_eq!(lag.label().map(|(t, _)| t), Some("under 1s behind".to_string()));
    }

    /// A machine whose clock runs ahead of Twitch's would otherwise report a
    /// negative lag, and a log with no absolute time has nothing to measure.
    #[test]
    fn lag_refuses_to_invent_a_number() {
        let t0 = 1_786_000_000_000i64;

        // Clock skew: the "newest" message is stamped in our future.
        let mut lag = ChatLag::default();
        lag.observe(1, (t0 + 5_000) as f64, t0);
        assert_eq!(lag.ms, Some(0), "clamped, never negative");

        // A pre-feature log carries no absolute time at all.
        let mut lag = ChatLag::default();
        lag.observe(1, 0.0, t0);
        assert_eq!(lag.ms, None);
        assert!(lag.label().is_none(), "no number beats a wrong number");

        // An empty log measures nothing.
        let mut lag = ChatLag::default();
        lag.observe(0, 0.0, t0);
        assert_eq!(lag.ms, None);
    }

    #[test]
    fn lag_reads_naturally_at_every_scale() {
        let mut lag = ChatLag::default();
        let t0 = 1_786_000_000_000i64;
        for (ago_ms, want) in [
            (0i64, "under 1s behind"),
            (999, "under 1s behind"),
            (1_000, "1.0s behind"),
            (3_450, "3.5s behind"),
            (59_000, "59.0s behind"),
            (90_000, "2m behind"),
        ] {
            // A fresh counter each time so every sample is "new".
            lag = ChatLag { at_count: 0, ..lag };
            lag.observe(1, (t0 - ago_ms) as f64, t0);
            assert_eq!(lag.label().map(|(t, _)| t), Some(want.to_string()), "{ago_ms}ms");
        }
    }

    /// The height cache is keyed on everything that changes how tall a row
    /// comes out. Colours deliberately are not: recolouring text can't resize
    /// it, and folding them in would dump the whole cache on every drag of the
    /// colour picker.
    #[test]
    fn layout_key_tracks_sizes_and_ignores_colors() {
        let base = ChatAppearance {
            font_pt: 14.0,
            ts_pt: 13.0,
            emote_pt: 24.0,
            emote_wide_pt: 24.0,
            ts_color: egui::Color32::WHITE,
            text_color: egui::Color32::WHITE,
            font_id: font_name_key(""),
            ts_mode: ChatTsMode::StreamRelative,
        };
        let key = base.layout_key();
        assert_eq!(ChatAppearance { font_pt: 14.0, ..base }.layout_key(), key, "same settings");
        assert_ne!(ChatAppearance { font_pt: 20.0, ..base }.layout_key(), key, "font size");
        assert_ne!(ChatAppearance { ts_pt: 20.0, ..base }.layout_key(), key, "timestamp size");
        assert_ne!(ChatAppearance { emote_pt: 40.0, ..base }.layout_key(), key, "emote size");
        assert_ne!(
            ChatAppearance { emote_wide_pt: 40.0, ..base }.layout_key(),
            key,
            "wide emote size"
        );
        assert_eq!(
            ChatAppearance { ts_color: egui::Color32::RED, text_color: egui::Color32::BLUE, ..base }
                .layout_key(),
            key,
            "colours must not invalidate measured heights"
        );
        // `[00:40:10]` and `19:30` are different widths, so a long message
        // wraps at a different point and the row is a different height.
        assert_ne!(
            ChatAppearance { ts_mode: ChatTsMode::WallClock, ..base }.layout_key(),
            key,
            "timestamp mode"
        );
        // A different face is a different height at the same point size, so
        // the cache has to drop — this is the whole reason `font_id` exists.
        assert_ne!(
            ChatAppearance { font_id: font_name_key("Comic Sans MS"), ..base }.layout_key(),
            key,
            "font family"
        );
    }

    fn plain_msg(ts: f64, login: &str, msg_id: &str, text: &str) -> ChatMessage {
        ChatMessage {
            timestamp_secs: ts,
            ts_unix_ms: 0.0,
            notice: None,
            author: login.to_string(),
            text: text.to_string(),
            segments: vec![ChatSegment::Text(text.to_string())],
            badges: Vec::new(),
            badge_icons: Vec::new(),
            color_override: None,
            platform: ChatPlatform::Twitch,
            login: login.to_string(),
            msg_id: msg_id.to_string(),
            deleted: None,
            system: false,
            reply_to: String::new(),
            source_name: String::new(),
            user_id: String::new(),
            author_id: String::new(),
            badge_info: String::new(),
        }
    }

    #[test]
    fn user_role_label_prioritizes_broadcaster_over_everything() {
        assert_eq!(
            user_role_label(&["subscriber/12".into(), "broadcaster/1".into()]),
            "Broadcaster"
        );
        assert_eq!(user_role_label(&["moderator/1".into()]), "Moderators");
        assert_eq!(user_role_label(&["vip/1".into()]), "VIPs");
        assert_eq!(user_role_label(&["founder/0".into()]), "Subscribers");
        assert_eq!(user_role_label(&[]), "Users");
        assert_eq!(user_role_label(&["glhf-pledge/1".into()]), "Users");
    }

    #[test]
    fn build_users_panel_dedupes_by_login_using_the_latest_message() {
        let mut log = ChatLog {
            messages: vec![
                plain_msg(0.0, "bob", "1", "hi"),
                plain_msg(5.0, "alice", "2", "hey"),
                {
                    let mut m = plain_msg(10.0, "bob", "3", "back again");
                    m.author = "Bob".to_string();
                    m.badges = vec!["moderator/1".to_string()];
                    m
                },
            ],
            row_heights: Vec::new(),
            measured_key: (0.0, 0),
            parsed_to: 0,
            loading_older: false,
            markers: Vec::new(),
        };
        // A YouTube chatter is listed too, keyed by their channel id — that's
        // the identity their usercard and any deletion marker both use.
        let mut yt = plain_msg(1.0, "", "", "yt viewer");
        yt.platform = ChatPlatform::YouTube;
        yt.author = "YT Viewer".to_string();
        yt.author_id = "UCyt".to_string();
        log.messages.push(yt);
        // One with no identity at all (a pre-feature log line) still can't be:
        // there would be nothing to open a card on.
        let mut anon = plain_msg(2.0, "", "", "anonymous");
        anon.platform = ChatPlatform::YouTube;
        log.messages.push(anon);

        let entries = build_users_panel(&log);
        // Deduped: one entry for "bob" (not two), using their later, mod-badged message.
        assert_eq!(entries.len(), 3);
        let bob = entries.iter().find(|e| e.click.login == "bob").expect("bob present");
        assert_eq!(bob.role, "Moderators");
        // Moderators sort before Users (alice has no badges).
        assert_eq!(entries[0].click.login, "bob");
        let yt_entry = entries
            .iter()
            .find(|e| e.click.display_name == "YT Viewer")
            .expect("the YouTube chatter is listed");
        assert_eq!((yt_entry.click.login.as_str(), yt_entry.click.user_id.as_str()), ("", "UCyt"));
        assert_eq!(yt_entry.click.key(), "UCyt");
    }

    #[test]
    fn youtube_deletions_and_author_purges_strike_messages() {
        // A replay line carrying a message, then the two moderator actions.
        let mut msgs = Vec::new();
        let mut markers = Vec::new();
        let mut fetches = Vec::new();
        let mut last = 0.0;
        for line in [
            r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"1000","actions":[{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{"id":"m1","authorExternalChannelId":"UCspam","authorName":{"simpleText":"Spammer"},"message":{"runs":[{"text":"spam"}]}}}}}]}}"#,
            r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"2000","actions":[{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{"id":"m2","authorExternalChannelId":"UCspam","authorName":{"simpleText":"Spammer"},"message":{"runs":[{"text":"more spam"}]}}}}}]}}"#,
            r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"3000","actions":[{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{"id":"m3","authorExternalChannelId":"UCok","authorName":{"simpleText":"Regular"},"message":{"runs":[{"text":"hello"}]}}}}}]}}"#,
            r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"4000","actions":[{"markChatItemAsDeletedAction":{"targetItemId":"m1"}}]}}"#,
            r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"5000","actions":[{"markChatItemsByAuthorAsDeletedAction":{"externalChannelId":"UCspam","deletedStateMessage":{"runs":[{"text":"Message deleted by moderator"}]}}}]}}"#,
        ] {
            parse_yt_chat_line(line, &mut msgs, &mut markers, &mut fetches, &mut last);
        }
        assert_eq!(msgs.len(), 3);
        assert_eq!(markers.len(), 2);
        // A later message from the purged author, after the action.
        let mut later = msgs[0].clone();
        later.timestamp_secs = 9.0;
        later.msg_id = "m4".into();
        msgs.push(later);

        let mut log = ChatLog {
            messages: msgs,
            row_heights: Vec::new(),
            measured_key: (0.0, 0),
            parsed_to: 0,
            loading_older: false,
            markers,
        };
        log.apply_markers();
        assert_eq!(log.messages[0].deleted.as_deref(), Some("deleted by a moderator"));
        assert_eq!(
            log.messages[1].deleted.as_deref(),
            Some("Message deleted by moderator"),
            "the by-author removal strikes their other messages, using YouTube's own wording"
        );
        assert_eq!(log.messages[2].deleted, None, "a different author is untouched");
        assert_eq!(log.messages[3].deleted, None, "messages after the removal stand");
    }

    #[test]
    fn moderation_markers_strike_messages() {
        // del marker: only the referenced message id is struck.
        let (marker, notice) = parse_twitch_marker_line(
            r#"{"ts":100000,"marker":"del","id":"abc"}"#,
            0.0,
        )
        .expect("del marker parses");
        assert!(notice.is_none(), "single deletions get no notice line");
        // purge marker: notice line + purge of earlier messages by that login.
        let (purge, purge_notice) = parse_twitch_marker_line(
            r#"{"ts":150000,"marker":"purge","login":"spammer","secs":600}"#,
            0.0,
        )
        .expect("purge marker parses");
        let notice_msg = purge_notice.expect("purges announce themselves");
        assert!(notice_msg.system);
        assert_eq!(notice_msg.text, "spammer was timed out (10m)");

        let mut log = ChatLog {
            messages: vec![
                plain_msg(50.0, "alice", "abc", "deleted one"),
                plain_msg(60.0, "alice", "def", "kept"),
                plain_msg(70.0, "spammer", "ggg", "spam early"),
                plain_msg(200.0, "spammer", "hhh", "after the timeout"),
                notice_msg,
            ],
            row_heights: Vec::new(),
            measured_key: (0.0, 0),
            parsed_to: 0,
            loading_older: false,
            markers: vec![marker.unwrap(), purge.unwrap()],
        };
        log.apply_markers();
        assert_eq!(log.messages[0].deleted.as_deref(), Some("deleted by a moderator"));
        assert_eq!(log.messages[1].deleted, None, "same author, different id");
        assert_eq!(log.messages[2].deleted.as_deref(), Some("timed out (10m)"));
        assert_eq!(log.messages[3].deleted, None, "messages after the purge stand");
        assert_eq!(log.messages[4].deleted, None, "system notices are never struck");
        // Idempotent: re-applying changes nothing.
        log.apply_markers();
        assert_eq!(log.messages[1].deleted, None);
    }

    /// The new PRIVMSG fields become notices; a log written before they
    /// existed simply has none, which is the whole backward-compatibility
    /// story. Redemption beats first-message when a line is both: it's the
    /// more specific fact and it carries its own header line.
    #[test]
    fn sidecar_notice_fields_round_trip_and_old_lines_stay_plain() {
        let parse = |line: &str| {
            let mut fetches = Vec::new();
            parse_twitch_chat_line(
                line, 1_700_000_000_000.0, &HashMap::new(), None, &HashMap::new(), false,
                &mut fetches, &HashMap::new(), &empty_badge_dirs(), &HashMap::new(),
            )
            .expect("line parses")
        };
        let base = r#""ts":1700000000000,"login":"bob","name":"Bob","text":"hi""#;

        assert_eq!(parse(&format!("{{{base}}}")).notice, None, "an old line has no notice");

        assert_eq!(
            parse(&format!(r#"{{{base},"first":true}}"#)).notice.as_deref(),
            Some(&ChatNotice::FirstMessage)
        );

        assert_eq!(
            parse(&format!(r#"{{{base},"reward_id":"abc-123"}}"#)).notice.as_deref(),
            Some(&ChatNotice::Redemption {
                reward: None,
                reward_id: "abc-123".into(),
                cost: None
            })
        );

        // Highlight My Message names itself; no lookup needed.
        assert_eq!(
            parse(&format!(r#"{{{base},"msg_kind":"highlighted-message"}}"#)).notice.as_deref(),
            Some(&ChatNotice::Redemption {
                reward: Some("Highlight My Message".into()),
                reward_id: String::new(),
                cost: None
            })
        );

        // Both at once: the redemption wins.
        let both = parse(&format!(r#"{{{base},"first":true,"reward_id":"abc"}}"#));
        assert!(matches!(both.notice.as_deref(), Some(ChatNotice::Redemption { .. })));

        // Absolute time is preserved for the wall-clock timestamp mode.
        assert_eq!(parse(&format!("{{{base}}}")).ts_unix_ms, 1_700_000_000_000.0);
    }

    /// An `event` marker becomes a rendered row carrying Twitch's own copy —
    /// and an unknown kind (a newer build's marker read by an older one)
    /// degrades to nothing rather than a mislabelled row.
    #[test]
    fn event_markers_render_as_notice_rows() {
        let parse = |line: &str| parse_twitch_marker_line(line, 1_700_000_000_000.0);

        let (marker, msg) = parse(
            r#"{"marker":"event","kind":"sub","ts":1700000060000,"login":"bob","name":"Bob",
                "text":"Bob subscribed at Tier 1.","body":"still here!"}"#,
        )
        .expect("event parses");
        assert!(marker.is_none(), "an event strikes nothing");
        let m = msg.expect("event produces a row");
        assert_eq!(
            m.notice.as_deref(),
            Some(&ChatNotice::Sub { system_msg: "Bob subscribed at Tier 1.".into() })
        );
        assert_eq!(m.author, "Bob");
        assert_eq!(m.login, "bob");
        assert_eq!(m.text, "still here!", "the user's own message rides under the headline");
        assert_eq!(m.timestamp_secs, 60.0);
        // NOT a system row: `apply_markers` skips those, and a sub notice can
        // legitimately be struck by a moderator.
        assert!(!m.system);

        // A raid with no message of its own is just the headline.
        let (_, msg) = parse(
            r#"{"marker":"event","kind":"raid","ts":1700000000000,"name":"Ann","text":"Ann raided"}"#,
        )
        .unwrap();
        let m = msg.unwrap();
        assert!(m.text.is_empty() && m.segments.is_empty());
        assert_eq!(notice_headline(m.notice.as_deref().unwrap()), Some("Ann raided"));

        // Unknown kind, and an empty headline: neither produces a row.
        assert!(parse(r#"{"marker":"event","kind":"newthing","ts":1,"text":"x"}"#).is_none());
        assert!(parse(r#"{"marker":"event","kind":"sub","ts":1,"text":""}"#).is_none());
    }

    #[test]
    fn old_sidecar_lines_without_id_still_parse() {
        // Pre-v60 lines have no `id`/`login`-marker fields; they must load
        // exactly as before, just without deletion matching.
        let line = r#"{"ts":1700000000000,"login":"bob","name":"Bob","text":"hi"}"#;
        let mut fetches = Vec::new();
        let m = parse_twitch_chat_line(
            line, 1_700_000_000_000.0, &HashMap::new(), None, &HashMap::new(), false, &mut fetches,
            &HashMap::new(), &empty_badge_dirs(), &HashMap::new(),
        )
        .expect("old line parses");
        assert_eq!(m.msg_id, "");
        assert_eq!(m.login, "bob");
        assert!(!m.system && m.deleted.is_none());
    }

    #[test]
    fn resolves_source_room_id_to_a_known_partner_only() {
        let mut partners = HashMap::new();
        partners.insert(
            "999".to_string(),
            crate::models::CollabPartner {
                id: "999".into(),
                login: "othersteamer".into(),
                name: "OtherStreamer".into(),
                from_title: false,
                is_live: None,
            },
        );
        let mut fetches = Vec::new();

        // Matches a recorded partner -> named indicator.
        let line = r#"{"ts":1700000000000,"login":"bob","name":"Bob","text":"hi","source_room_id":"999"}"#;
        let m = parse_twitch_chat_line(
            line, 1_700_000_000_000.0, &HashMap::new(), None, &HashMap::new(), false, &mut fetches,
            &partners, &empty_badge_dirs(), &HashMap::new(),
        )
        .expect("parses");
        assert_eq!(m.source_name, "OtherStreamer");

        // Present but unmatched (the local channel's own id, not a
        // "partner") -> no indicator, not an error/placeholder.
        let line = r#"{"ts":1700000000000,"login":"bob","name":"Bob","text":"hi","source_room_id":"111"}"#;
        let m = parse_twitch_chat_line(
            line, 1_700_000_000_000.0, &HashMap::new(), None, &HashMap::new(), false, &mut fetches,
            &partners, &empty_badge_dirs(), &HashMap::new(),
        )
        .expect("parses");
        assert_eq!(m.source_name, "");

        // No tag at all (no active shared session, or a pre-feature log) -> no indicator.
        let line = r#"{"ts":1700000000000,"login":"bob","name":"Bob","text":"hi"}"#;
        let m = parse_twitch_chat_line(
            line, 1_700_000_000_000.0, &HashMap::new(), None, &HashMap::new(), false, &mut fetches,
            &partners, &empty_badge_dirs(), &HashMap::new(),
        )
        .expect("parses");
        assert_eq!(m.source_name, "");
    }

    #[test]
    fn resolve_twitch_badge_icon_prefers_channel_then_falls_back_to_global() {
        let root = std::env::temp_dir().join(format!("sa-badge-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let channel = root.join("channel");
        let global = root.join("global");
        std::fs::create_dir_all(channel.join("subscriber").join("12")).unwrap();
        std::fs::create_dir_all(global.join("subscriber").join("12")).unwrap();
        std::fs::create_dir_all(global.join("moderator").join("1")).unwrap();

        let dirs = TwitchBadgeDirs { channel: Some(channel.clone()), global: global.clone() };

        // Missing everywhere -> None (glyph fallback).
        assert!(resolve_twitch_badge_icon("subscriber/12", &dirs).is_none());

        // Only the global copy exists -> falls back to it.
        std::fs::write(global.join("moderator").join("1").join("2x.png"), b"x").unwrap();
        assert_eq!(
            resolve_twitch_badge_icon("moderator/1", &dirs),
            Some(global.join("moderator").join("1").join("2x.png"))
        );

        // Both exist -> channel-specific wins (a channel's own sub badge art
        // can differ from the global default).
        std::fs::write(channel.join("subscriber").join("12").join("2x.png"), b"x").unwrap();
        std::fs::write(global.join("subscriber").join("12").join("2x.png"), b"x").unwrap();
        assert_eq!(
            resolve_twitch_badge_icon("subscriber/12", &dirs),
            Some(channel.join("subscriber").join("12").join("2x.png"))
        );

        // Malformed entry (no '/') -> None, never panics.
        assert!(resolve_twitch_badge_icon("subscriber", &dirs).is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    // ----- first-party emote offset parsing (IRC `emotes` tag) -----

    #[test]
    fn first_party_spans_ascii_sorted_byte_ranges() {
        // "Kappa Keepo Kappa": cp ranges 0-4 / 6-10 / 12-16 == byte ranges (ASCII).
        let text = "Kappa Keepo Kappa";
        let spans = parse_first_party_spans(text, "25:0-4,12-16/1902:6-10");
        assert_eq!(
            spans,
            vec![
                (0, 5, "25".to_string()),
                (6, 11, "1902".to_string()),
                (12, 17, "25".to_string()),
            ]
        );
        for (b0, b1, _) in &spans {
            assert!(text.get(*b0..*b1).is_some());
        }
    }

    #[test]
    fn first_party_offsets_are_code_points_not_utf16_or_bytes() {
        // A leading astral emoji (😀 = 1 code point, 2 UTF-16 units, 4 bytes) before
        // the emote. Twitch counts code points, so "Kappa" is cp 2..=6.
        let text = "😀 Kappa";
        let spans = parse_first_party_spans(text, "25:2-6");
        assert_eq!(spans.len(), 1);
        let (b0, b1, id) = &spans[0];
        assert_eq!(id, "25");
        assert_eq!(text.get(*b0..*b1), Some("Kappa")); // not "appa"/"aKapp"/garbage
    }

    #[test]
    fn first_party_trailing_emote_reaches_end_of_string() {
        // Emote as the final token: end+1 is one-past-end → b1 must be text.len().
        let text = "gg Kappa";
        let spans = parse_first_party_spans(text, "25:3-7");
        assert_eq!(spans, vec![(3, 8, "25".to_string())]);
        assert_eq!(text.get(3..8), Some("Kappa"));
    }

    #[test]
    fn first_party_bails_on_overlap_reversed_or_oob() {
        // Overlapping spans → abort first-party entirely (empty).
        assert!(parse_first_party_spans("abcde", "25:0-2/1902:1-4").is_empty());
        // Reversed (end < start) → empty.
        assert!(parse_first_party_spans("abcde", "25:5-3").is_empty());
        // Out of range (end >= code-point count) → empty.
        assert!(parse_first_party_spans("hi", "25:0-5").is_empty());
        // Malformed → empty.
        assert!(parse_first_party_spans("hi", "garbage").is_empty());
        // Empty tag → empty.
        assert!(parse_first_party_spans("hi", "").is_empty());
    }

    // ----- third-party word matching -----

    fn emote(name: &str, file: &Option<PathBuf>) -> ChatSegment {
        ChatSegment::Emote { name: name.into(), file: file.clone(), fallback_text: None, pending: None }
    }

    #[test]
    fn word_match_is_case_sensitive_and_whole_token() {
        let mut map = HashMap::new();
        let p = PathBuf::from("/x/poggers.webp");
        map.insert("POGGERS".to_string(), p.clone());
        let segs = word_match_segments("hi POGGERS poggers POGGERSx", &map);
        // Only the exact, whole-token "POGGERS" matches; "poggers"/"POGGERSx" stay text.
        let emotes: Vec<_> = segs
            .iter()
            .filter(|s| matches!(s, ChatSegment::Emote { .. }))
            .collect();
        assert_eq!(emotes.len(), 1);
        assert!(matches!(&emotes[0], ChatSegment::Emote { name, .. } if name == "POGGERS"));
    }

    #[test]
    fn word_match_preserves_spacing_and_tabs() {
        let mut map = HashMap::new();
        map.insert("Kappa".to_string(), PathBuf::from("/x/k.png"));
        // Tab-separated tokens still match (Unicode-whitespace tokenization).
        let segs = word_match_segments("a\tKappa\tb", &map);
        // Reconstructing all text + emote names must round-trip the original.
        let mut rebuilt = String::new();
        for s in &segs {
            match s {
                ChatSegment::Text(t) => rebuilt.push_str(t),
                ChatSegment::Emote { name, .. } => rebuilt.push_str(name),
            }
        }
        assert_eq!(rebuilt, "a\tKappa\tb");
        assert!(segs.iter().any(|s| matches!(s, ChatSegment::Emote { name, .. } if name == "Kappa")));
    }

    #[test]
    fn empty_map_yields_single_text_segment() {
        let map: HashMap<String, PathBuf> = HashMap::new();
        let segs = word_match_segments("hello world", &map);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], ChatSegment::Text(t) if t == "hello world"));
    }

    #[test]
    fn strips_ctcp_action_wrapper() {
        // `/me` actions are unwrapped; the inner body is what emote offsets index.
        assert_eq!(strip_ctcp_action("\u{1}ACTION Kappa\u{1}"), "Kappa");
        assert_eq!(strip_ctcp_action("\u{1}ACTION \u{1}"), "");
        // Plain messages and malformed wrappers pass through untouched.
        assert_eq!(strip_ctcp_action("hello"), "hello");
        assert_eq!(strip_ctcp_action("\u{1}ACTION no-suffix"), "\u{1}ACTION no-suffix");
    }

    #[test]
    fn action_message_emote_offsets_align_after_strip() {
        // `/me Kappa` → stored `\x01ACTION Kappa\x01`; after stripping, offset 0-4
        // must land on "Kappa", not on the control-char-prefixed wrapper.
        let stripped = strip_ctcp_action("\u{1}ACTION Kappa\u{1}");
        let map = HashMap::new();
        let mut fetches = Vec::new();
        let segs =
            build_twitch_segments(stripped, "25:0-4", &map, None, &HashMap::new(), false, &mut fetches);
        assert!(matches!(&segs[0], ChatSegment::Emote { name, .. } if name == "Kappa"));
    }

    /// Regression guard: `find_emote_file` must resolve the CURRENT
    /// `{id}_{name}.{ext}` filename fetchers write (see `assets.rs`'s
    /// `fetch_twitch_emotes`), not just the pre-rename `{id}.{ext}` form —
    /// this fell out of sync with the fetcher and silently broke rendering
    /// for every first-party emote fetched since (2026-08-02).
    #[test]
    fn find_emote_file_resolves_new_and_old_filename_schemes() {
        let dir = std::env::temp_dir().join(format!("sa-emote-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Neither form present.
        assert!(find_emote_file(&dir, "111", "anyany4Cheer").is_none());
        // New form only.
        std::fs::write(dir.join("111_anyany4Cheer.png"), b"x").unwrap();
        assert_eq!(find_emote_file(&dir, "111", "anyany4Cheer"), Some(dir.join("111_anyany4Cheer.png")));
        // Old form only, different id — still resolves.
        std::fs::write(dir.join("222.gif"), b"x").unwrap();
        assert_eq!(find_emote_file(&dir, "222", "someOldEmote"), Some(dir.join("222.gif")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression guard: Twitch lets any subscriber use their sub emotes in
    /// ANY channel's chat. An id missing from the currently-open channel's
    /// own dir must still resolve if it's cached under a DIFFERENT channel's
    /// dir (e.g. this app also archives that other streamer) — via the
    /// precomputed `twitch_fallback_index` `build_twitch_segments` consults
    /// after the primary dir misses.
    #[test]
    fn build_twitch_segments_falls_back_to_other_channels_emote_dirs() {
        let root = std::env::temp_dir().join(format!("sa-emote-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let primary = root.join("anya");
        let other_a = root.join("nihmune");
        let other_b = root.join("layna");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&other_a).unwrap();
        std::fs::create_dir_all(&other_b).unwrap();
        // "nihmunHeart" only exists under Nihmune's dir, not Anya's own.
        std::fs::write(other_a.join("555_nihmunHeart.png"), b"x").unwrap();

        let map = HashMap::new();
        let fallback_index = crate::assets::index_emote_stems(&[other_a.clone(), other_b.clone()]);
        let mut fetches = Vec::new();
        let segs = build_twitch_segments(
            "nihmunHeart", "555:0-10", &map, Some(&primary), &fallback_index, false, &mut fetches,
        );
        let ChatSegment::Emote { name, file, .. } = &segs[0] else {
            panic!("expected an Emote segment");
        };
        assert_eq!(name, "nihmunHeart");
        assert_eq!(file.as_deref(), Some(other_a.join("555_nihmunHeart.png").as_path()));

        // An id present under NEITHER the primary nor any fallback dir still
        // renders as text (file: None) instead of panicking or matching wrong
        // — `fetch_unknown_emotes: false` here, so nothing gets enqueued either.
        let segs_missing = build_twitch_segments(
            "totallyUnknown", "999:0-13", &map, Some(&primary), &fallback_index, false, &mut fetches,
        );
        assert!(matches!(&segs_missing[0], ChatSegment::Emote { file: None, pending: None, .. }));
        assert!(fetches.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression guard: an id missing from BOTH the primary dir and the
    /// fallback index — the poster's home channel isn't monitored/archived
    /// here at all — queues a CDN fetch by id (see
    /// `assets::twitch_emote_cdn_fetch`) instead of giving up permanently,
    /// when "Fetch unknown emotes from Twitch" is on.
    #[test]
    fn build_twitch_segments_queues_a_cdn_fetch_for_a_totally_unknown_id_when_enabled() {
        let map = HashMap::new();
        let mut fetches = Vec::new();
        let segs = build_twitch_segments(
            "brandNewEmoteCode", "8675309:0-16", &map, None, &HashMap::new(), true, &mut fetches,
        );
        let ChatSegment::Emote { name, file, pending, .. } = &segs[0] else {
            panic!("expected an Emote segment");
        };
        assert_eq!(name, "brandNewEmoteCode");
        assert!(file.is_none(), "not on disk yet");
        let (expected_dest, expected_urls) =
            crate::assets::twitch_emote_cdn_fetch("8675309", "brandNewEmoteCode");
        assert_eq!(pending.as_deref(), Some(expected_dest.as_path()));
        assert_eq!(fetches.len(), 1);
        assert_eq!(fetches[0].dest, expected_dest);
        assert_eq!(fetches[0].urls, expected_urls);
    }

    /// The same total miss, with the toggle off: no fetch queued, no `pending`
    /// — today's plain-text-forever behavior is preserved when the user
    /// hasn't opted into fetching from channels not otherwise archived here.
    #[test]
    fn build_twitch_segments_skips_cdn_fetch_when_toggle_is_off() {
        let map = HashMap::new();
        let mut fetches = Vec::new();
        let segs = build_twitch_segments(
            "brandNewEmoteCode", "8675309:0-16", &map, None, &HashMap::new(), false, &mut fetches,
        );
        let ChatSegment::Emote { file, pending, .. } = &segs[0] else {
            panic!("expected an Emote segment");
        };
        assert!(file.is_none() && pending.is_none());
        assert!(fetches.is_empty());
    }

    // ----- username colour readability -----

    #[test]
    fn hsl_round_trips_within_one_step() {
        for c in [
            egui::Color32::from_rgb(0xFF, 0x00, 0x00),
            egui::Color32::from_rgb(0x00, 0x00, 0xFF),
            egui::Color32::from_rgb(0x8A, 0x2B, 0xE2),
            egui::Color32::from_rgb(0x12, 0x34, 0x56),
            egui::Color32::from_rgb(0x80, 0x80, 0x80),
        ] {
            let (h, s, l) = rgb_to_hsl(c);
            let back = hsl_to_rgb(h, s, l);
            // Allow ±1 per channel for rounding.
            assert!((c.r() as i32 - back.r() as i32).abs() <= 1);
            assert!((c.g() as i32 - back.g() as i32).abs() <= 1);
            assert!((c.b() as i32 - back.b() as i32).abs() <= 1);
        }
    }

    #[test]
    fn readable_color_lightens_dark_color_on_dark_bg() {
        let bg = egui::Color32::from_rgb(0x1e, 0x1e, 0x1e); // egui dark panel-ish
        let blue = egui::Color32::from_rgb(0x00, 0x00, 0xFF); // unreadable on dark
        assert!(contrast_ratio(blue, bg) < 4.0);
        let fixed = readable_color(blue, bg);
        assert!(contrast_ratio(fixed, bg) >= 4.0);
        // Hue stays blue-ish: blue channel remains the dominant one.
        assert!(fixed.b() > fixed.r() && fixed.b() > fixed.g());
    }

    #[test]
    fn readable_color_picks_reachable_direction_on_midtone_bg() {
        // On a mid-grey bg, lightening blue toward white never clears 4.0, but
        // darkening does — the direction must be chosen by reachable contrast.
        let bg = egui::Color32::from_gray(128);
        let blue = egui::Color32::from_rgb(0x00, 0x00, 0xFF);
        let fixed = readable_color(blue, bg);
        assert!(contrast_ratio(fixed, bg) >= 4.0);
    }

    #[test]
    fn readable_color_keeps_already_legible_color() {
        let bg = egui::Color32::from_rgb(0x1e, 0x1e, 0x1e);
        let coral = egui::Color32::from_rgb(0xFF, 0x7F, 0x50); // already high-contrast
        assert_eq!(readable_color(coral, bg), coral);
    }

    #[test]
    fn readable_color_darkens_pale_color_on_light_bg() {
        let bg = egui::Color32::WHITE;
        let pale = egui::Color32::from_rgb(0xFF, 0xFF, 0x00); // yellow: invisible on white
        assert!(contrast_ratio(pale, bg) < 4.0);
        let fixed = readable_color(pale, bg);
        assert!(contrast_ratio(fixed, bg) > contrast_ratio(pale, bg));
    }

    #[test]
    fn twitch_default_color_is_deterministic_per_name() {
        // Same name → same colour every time (Twitch's stable default assignment).
        assert_eq!(twitch_username_color("Kappa"), twitch_username_color("Kappa"));
    }
    #[test]
    fn build_twitch_combines_first_party_offset_and_thirdparty_word() {
        // First-party "Kappa" by offset (no file on disk → name fallback), and a
        // third-party "POGGERS" by word match in the trailing gap.
        let mut map = HashMap::new();
        let pog = PathBuf::from("/x/poggers.webp");
        map.insert("POGGERS".to_string(), pog.clone());
        // "Kappa POGGERS": Kappa at cp 0-4; POGGERS is the gap word.
        let mut fetches = Vec::new();
        let segs = build_twitch_segments(
            "Kappa POGGERS", "25:0-4", &map, None, &HashMap::new(), false, &mut fetches,
        );
        // Expect: Emote(Kappa, None) then Text(" ") then Emote(POGGERS, Some).
        assert!(matches!(&segs[0], ChatSegment::Emote { name, file, .. } if name == "Kappa" && file.is_none()));
        assert!(segs.iter().any(|s| matches!(s, ChatSegment::Emote { name, file, .. } if name == "POGGERS" && file.as_ref() == Some(&pog))));
    }

    fn rec_with_output(path: &str) -> crate::models::Recording {
        crate::models::Recording {
            id: 1,
            monitor_id: 1,
            started_at: 0,
            ended_at: None,
            status: "recording".into(),
            bytes: 0,
            exit_code: None,
            output_path: path.into(),
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
            vod_views: None,
            err_ack: false,
            sabr_live_edge_fallback: false,
            chapters_state: String::new(),
            chapters_json: String::new(),
            chapters_attempts: 0,
            chat_path: String::new(),
            rolling: crate::models::Rolling::default(),
            not_recorded_reason: String::new(),
            gated: false,
        }
    }

    #[test]
    fn finds_youtube_live_chat_append_form() {
        // yt-dlp appends `.live_chat.json` to the -o value, so the sidecar keeps the
        // video extension: `<output_path>.live_chat.json`.
        let dir = std::env::temp_dir().join(format!("sa-chat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("clip.mkv");
        std::fs::write(format!("{}.live_chat.json", out.to_string_lossy()), "{}").unwrap();

        let found = chat_file_for_recording(&rec_with_output(&out.to_string_lossy()));
        assert_eq!(found.as_deref(), Some(out.with_extension("mkv.live_chat.json").as_path()));

        // Twitch native logger uses the extension-replace form.
        let tout = dir.join("vod.mkv");
        std::fs::write(tout.with_extension("chat.jsonl"), "{}").unwrap();
        let tfound = chat_file_for_recording(&rec_with_output(&tout.to_string_lossy()));
        assert_eq!(tfound.as_deref(), Some(tout.with_extension("chat.jsonl").as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A chat-only session (`downloader::chat_only`) has no video file, so its
    /// sidecar can only be found through the explicit `chat_path` column.
    #[test]
    fn explicit_chat_path_wins_over_the_derived_forms() {
        let dir = std::env::temp_dir().join(format!("sa-chatpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("Streamer - live.chat.jsonl");
        std::fs::write(&sidecar, "{}").unwrap();

        // The not-recorded take that owns it: no output_path at all.
        let mut rec = rec_with_output("");
        rec.chat_path = sidecar.to_string_lossy().into_owned();
        assert_eq!(chat_file_for_recording(&rec).as_deref(), Some(sidecar.as_path()));

        // It's also the ONLY candidate: an ordinary take's derived paths must
        // not be probed alongside it and accidentally match a neighbour.
        let mut rec = rec_with_output(&dir.join("Streamer - live.mkv").to_string_lossy());
        std::fs::write(dir.join("Streamer - live.mkv.live_chat.json"), "{}").unwrap();
        rec.chat_path = sidecar.to_string_lossy().into_owned();
        assert_eq!(chat_file_candidates(&rec).len(), 1);
        assert_eq!(chat_file_for_recording(&rec).as_deref(), Some(sidecar.as_path()));

        // Empty `chat_path` (every ordinary take) keeps the old behaviour.
        rec.chat_path.clear();
        assert_eq!(
            chat_file_for_recording(&rec).as_deref(),
            Some(dir.join("Streamer - live.mkv.live_chat.json").as_path())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
