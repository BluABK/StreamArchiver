//! Chat popup: log parsing (Twitch/YouTube), segments/emotes, colors,
//! emoji handling.

use super::*;

/// Source platform of a captured chat message (drives username colouring).
#[derive(Clone)]
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
    /// Twitch IRCv3 message id — what a `del` marker references. Empty for
    /// YouTube / old logs (single-message deletions then can't match).
    pub(super) msg_id: String,
    /// `Some(reason)` once a moderation marker struck this message
    /// ("deleted by a moderator", "timed out (10m)", …) — renders
    /// strikethrough with the reason on hover. Applied by
    /// [`ChatLog::apply_markers`], never set at parse time.
    pub(super) deleted: Option<String>,
    /// System notice line (chat-mode change, role change, timeout/ban
    /// announcement) — renders as a muted ℹ line, no author.
    pub(super) system: bool,
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
    /// A single message was deleted (matched by Twitch message id).
    Delete { msg_id: String },
    /// Everything a user said up to this point was purged (timeout/ban).
    Purge { login: String, reason: String },
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
    /// The width `row_heights` was measured at — a resize changes wrapping, so
    /// the cache resets to estimates.
    pub(super) measured_width: f32,
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
                ChatMarker::Purge { login, reason } => {
                    let e = purges.entry(login.as_str()).or_insert((m.ts_secs, reason));
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
                (!msg.login.is_empty()).then(|| purges.get(msg.login.as_str())).flatten()
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
    pub(super) fetch_unknown_emotes: bool,
    pub(super) fetch_usercard_info: bool,
    pub(super) font_pt: f32,
    pub(super) emote_pt: f32,
    pub(super) ts_color: egui::Color32,
    pub(super) text_color: egui::Color32,
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
        }
    }
}

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
    /// Top gift-sub contributors for this broadcast (display name, total
    /// gifted), from `store::stream_event`. Local DB only, no network.
    /// Computed once on popup-open/recording-switch, refreshed on the same
    /// cadence as the live tail-reload while the recording is still going.
    pub(super) top_gifters: Vec<(String, i64)>,
    /// Top bits contributors for this broadcast (display name, total bits).
    pub(super) top_cheerers: Vec<(String, i64)>,
    /// This broadcast's most recent Hype Train, if any — see
    /// [`HypeTrainDisplay`]'s doc. A long broadcast may have had several;
    /// only the latest is kept (see `load_broadcast_stats`'s doc for why).
    pub(super) hype_train: Option<HypeTrainDisplay>,
    /// When the popup last triggered a background re-read of the chat file.
    /// Used to tail a live recording: the file is re-parsed every few seconds
    /// while `recording.ended_at` is `None`.
    pub(super) last_reload: std::time::Instant,
    /// Third-party emote code → resolved on-disk image path, built ONCE on
    /// popup-open from the channel's BTTV/FFZ/7TV manifests (case-sensitive keys).
    /// Empty for YouTube / when chat assets aren't fetched. `Arc` so the
    /// background (re)parse tasks share it without rebuilding per tick.
    pub(super) emote_map: Arc<HashMap<String, std::path::PathBuf>>,
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
}
/// The Twitch broadcaster's chosen chat name colour for `name`'s `account`, if
/// the asset fetch cached one (`…/{name}/twitch/{account}/name_color.txt`, e.g.
/// `#9146FF`; legacy pre-account dir as fallback). `None` when the streamer set
/// no colour or assets haven't been fetched.
pub(super) fn load_twitch_name_color(name: &str, account: &str) -> Option<egui::Color32> {
    for dir in crate::assets::asset_read_dirs(name, Platform::Twitch, account) {
        if let Ok(s) = crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, dir.join("name_color.txt")) {
            return parse_chat_hex_color(s.trim());
        }
    }
    None
}

/// Build a Twitch channel's third-party emote lookup: case-sensitive emote code →
/// resolved on-disk image path. Reads the per-channel BTTV/FFZ/7TV manifests once
/// (called on popup-open, not per message) and resolves each entry to its file in
/// the per-channel or shared-global cache, keeping only those that exist.
///
/// Precedence on a code defined by multiple providers: **7TV > BTTV > FFZ** — we
/// insert in that order with `or_insert`, so the first (highest-priority) provider
/// to define a code wins and later duplicates don't clobber it.
/// The Twitch `emotes/` dir for (channel, account): the account dir when it has
/// content, else the legacy pre-account per-platform dir (read fallback).
pub(super) fn twitch_emotes_dir(name: &str, account: &str) -> std::path::PathBuf {
    let [primary, legacy] = crate::assets::asset_read_dirs(name, Platform::Twitch, account);
    let primary = primary.join("emotes");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &primary) {
        return primary;
    }
    let legacy = legacy.join("emotes");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &legacy) { legacy } else { primary }
}

/// Where a Twitch channel's chat badge icons live: a per-channel dir (sub/
/// VIP/broadcaster badges — earned relative to the channel being watched, so
/// always resolved against it, not a fallback index like third-party
/// emotes) plus the shared global dir (mod/staff/bits/etc, downloaded once
/// for all channels). Built ONCE per popup-open — see [`resolve_twitch_badge_icon`]'s
/// doc for why resolution happens at parse time, not render time.
pub(crate) struct TwitchBadgeDirs {
    pub(crate) channel: Option<std::path::PathBuf>,
    pub(crate) global: std::path::PathBuf,
}

/// The Twitch `badges/` dir for (channel, account), mirroring [`twitch_emotes_dir`]'s
/// account-dir-with-legacy-fallback resolution.
pub(super) fn twitch_badge_dir(name: &str, account: &str) -> std::path::PathBuf {
    let [primary, legacy] = crate::assets::asset_read_dirs(name, Platform::Twitch, account);
    let primary = primary.join("badges");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &primary) {
        return primary;
    }
    let legacy = legacy.join("badges");
    if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &legacy) { legacy } else { primary }
}

/// The shared global Twitch badge dir (mod/staff/turbo/bits/etc — not tied to
/// any one channel), matching `assets::fetch_twitch_badges`'s own path.
pub(crate) fn twitch_global_badge_dir() -> std::path::PathBuf {
    crate::app_paths::platform_assets_dir().join("twitch").join("global_badges")
}

/// Resolve a raw IRC badge entry (`"subscriber/12"`) to its cached icon file,
/// checking the channel-specific dir first (sub/VIP/broadcaster badges are
/// earned per-channel) then the shared global dir (mod/staff/bits/etc).
/// `None` when the set/version isn't cached (never fetched, still
/// downloading, or a YouTube badge — this is Twitch-only) — the caller falls
/// back to a text glyph. Resolved ONCE per message at parse time (like
/// first-party emotes in [`build_twitch_segments`]) rather than per frame, to
/// keep filesystem stats out of the render loop.
pub(super) fn resolve_twitch_badge_icon(
    raw: &str,
    dirs: &TwitchBadgeDirs,
) -> Option<std::path::PathBuf> {
    let (set_id, version) = raw.split_once('/')?;
    for base in dirs.channel.iter().chain(std::iter::once(&dirs.global)) {
        let p = base.join(set_id).join(version).join("2x.png");
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &p) {
            return Some(p);
        }
    }
    None
}

/// Human-readable label for a raw badge entry's set id, used as hover text —
/// e.g. `"subscriber/12"` → `"Subscriber"`. Falls back to the raw set id
/// title-cased for anything not explicitly named.
pub(super) fn badge_label(raw: &str) -> String {
    let set_id = raw.split('/').next().unwrap_or(raw);
    match set_id {
        "broadcaster" => "Broadcaster".to_string(),
        "moderator" => "Moderator".to_string(),
        "vip" => "VIP".to_string(),
        "subscriber" => "Subscriber".to_string(),
        "founder" => "Founder".to_string(),
        "bits" | "bits-leader" => "Bits".to_string(),
        "premium" => "Prime Gaming".to_string(),
        "partner" => "Verified Partner".to_string(),
        "staff" => "Twitch Staff".to_string(),
        "admin" => "Twitch Admin".to_string(),
        "turbo" => "Turbo".to_string(),
        _ => {
            let mut c = set_id.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

pub(super) fn build_emote_map(name: &str, account: &str) -> HashMap<String, std::path::PathBuf> {
    use crate::assets::EmoteManifestEntry;
    let emotes_dir = twitch_emotes_dir(name, account);
    let plat = crate::app_paths::platform_assets_dir();
    let mut map: HashMap<String, std::path::PathBuf> = HashMap::new();

    let load = |file: &str| -> Vec<EmoteManifestEntry> {
        crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, emotes_dir.join(file))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    };
    let mut insert = |entries: Vec<EmoteManifestEntry>, base_dir: &dyn Fn(&EmoteManifestEntry) -> std::path::PathBuf| {
        for e in entries {
            // Skip empty/whitespace-only codes (old name-less manifests, or odd
            // provider data) — they could never match a chat token anyway.
            if e.name.trim().is_empty() {
                continue;
            }
            // Resolves the current `{id}_{name}.{ext}` filename fetchers write,
            // falling back to the pre-rename `{id}.{ext}` form — see
            // `resolve_emote_path`'s doc for why this must stay in sync with
            // the fetchers instead of hardcoding one scheme here.
            let path = crate::assets::resolve_emote_path(&base_dir(&e), &e);
            if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &path) {
                map.entry(e.name).or_insert(path);
            }
        }
    };

    // 7TV: always in the shared global cache.
    insert(load("7tv.json"), &|_| plat.join("7tv").join("emotes"));
    // BTTV: per-channel for channel emotes, shared global for shared emotes.
    let bttv_channel = emotes_dir.join("bttv");
    let bttv_shared = plat.join("bttv").join("emotes");
    insert(load("bttv.json"), &|e| {
        if e.shared { bttv_shared.clone() } else { bttv_channel.clone() }
    });
    // FFZ: always in the shared global cache.
    insert(load("ffz.json"), &|_| plat.join("ffz").join("emotes"));
    map
}
/// Truncate a label to at most `max` characters, appending `…` when shortened.
/// Char-aware so it never splits a multi-byte UTF-8 emote code mid-codepoint.
pub(super) fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Twitch `/me` actions arrive over IRC wrapped as `\x01ACTION <body>\x01` (CTCP).
/// The `emotes` tag's offsets index `<body>`, and the wrapper is protocol noise, so
/// it must be unwrapped before slicing / searching / display (otherwise emote
/// offsets are shifted and the raw control chars show in the replay). Returns the
/// inner body when both the prefix and the trailing `\x01` are present, else the
/// input unchanged. The raw `.chat.jsonl` keeps the wrapper for archival fidelity.
pub(super) fn strip_ctcp_action(text: &str) -> &str {
    text.strip_prefix("\u{1}ACTION ")
        .and_then(|s| s.strip_suffix('\u{1}'))
        .unwrap_or(text)
}

/// Slice `text` into [`ChatSegment`]s for a Twitch message: first-party emotes are
/// placed by the IRC `emotes` tag (id + inclusive **code-point** ranges), then any
/// remaining plain-text gaps are word-matched against the third-party `emote_map`.
///
/// Offsets are Unicode code points (Rust `char` index), not bytes and not UTF-16.
/// Every slice goes through `text.get(..)`, and any malformed/overlapping/out-of-
/// range tag aborts first-party substitution for the whole message (→ one Text
/// segment, still word-matched), so this never panics on hostile input.
pub(super) fn build_twitch_segments(
    text: &str,
    emotes_tag: &str,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
    twitch_fallback_index: &HashMap<String, std::path::PathBuf>,
    fetch_unknown_emotes: bool,
    fetches: &mut Vec<EmojiFetch>,
) -> Vec<ChatSegment> {
    let spans = parse_first_party_spans(text, emotes_tag);
    if spans.is_empty() {
        return word_match_segments(text, emote_map);
    }
    // Emit text gaps (word-matched) and first-party emote images in order.
    let mut out: Vec<ChatSegment> = Vec::new();
    let mut cursor = 0usize;
    for (b0, b1, id) in spans {
        if b0 > cursor {
            if let Some(gap) = text.get(cursor..b0) {
                out.extend(word_match_segments(gap, emote_map));
            }
        }
        let name = text.get(b0..b1).unwrap_or("").to_string();
        // First-party files are `{id}.png` (static) or `{id}.gif` (animated — we
        // render its first frame). Probe both so animated channel emotes show too.
        // Twitch lets any subscriber use their sub emotes in ANY channel's chat,
        // not just the one they subscribed to — an id missing from the open
        // channel's own dir may still be cached under a DIFFERENT channel's
        // (e.g. this app also archives that other streamer), so fall back to
        // every other cached channel's Twitch emote dir before giving up — via
        // a precomputed stem index (`twitch_fallback_index`), not a fresh
        // filesystem probe per occurrence (see its doc for why that mattered).
        let file = twitch_dir
            .and_then(|d| find_emote_file(d, &id, &name))
            .or_else(|| find_emote_fallback(twitch_fallback_index, &id, &name));
        // Still nothing local — the poster's home channel may not be
        // monitored/archived here at all. When enabled, queue an on-demand
        // fetch straight from Twitch's CDN by id (see `twitch_emote_cdn_fetch`)
        // and mark it pending; `upgrade_pending_emotes` promotes it to `file`
        // once the background download lands, same as a Unicode emoji glyph.
        let (file, pending) = if file.is_some() {
            (file, None)
        } else if fetch_unknown_emotes {
            let (dest, url) = crate::assets::twitch_emote_cdn_fetch(&id, &name);
            if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &dest) {
                (Some(dest), None)
            } else if !crate::iomon::fs::exists_sync(
                crate::iomon::Cat::AssetCache,
                dest.with_extension("404"),
            ) {
                fetches.push(EmojiFetch { dest: dest.clone(), urls: vec![url] });
                (None, Some(dest))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        out.push(ChatSegment::Emote { name, file, fallback_text: None, pending });
        cursor = b1;
    }
    if cursor < text.len() {
        if let Some(rest) = text.get(cursor..) {
            out.extend(word_match_segments(rest, emote_map));
        }
    }
    out
}

/// Parse the IRC `emotes` tag into a sorted list of `(byte_start, byte_end, id)`
/// spans over `text`, converting inclusive code-point offsets to validated byte
/// ranges in ONE walk. Returns empty when the tag is empty OR anything is
/// malformed/overlapping/out-of-range (caller then renders the text verbatim).
pub(super) fn parse_first_party_spans(text: &str, emotes_tag: &str) -> Vec<(usize, usize, String)> {
    if emotes_tag.is_empty() {
        return Vec::new();
    }
    // Collect (cp_start, cp_end_inclusive, id), dropping any malformed entry.
    let mut ranges: Vec<(usize, usize, String)> = Vec::new();
    for group in emotes_tag.split('/') {
        let Some((id, positions)) = group.split_once(':') else {
            return Vec::new();
        };
        if id.is_empty() {
            return Vec::new();
        }
        for pair in positions.split(',') {
            let Some((s, e)) = pair.split_once('-') else {
                return Vec::new();
            };
            let (Ok(s), Ok(e)) = (s.parse::<usize>(), e.parse::<usize>()) else {
                return Vec::new();
            };
            if e < s {
                return Vec::new();
            }
            ranges.push((s, e, id.to_string()));
        }
    }
    ranges.sort_by_key(|r| r.0);
    // Resolve code-point offsets → byte offsets in one pass. b0 = byte index of the
    // first char with cp_idx >= start; b1 = first char with cp_idx >= end+1 (or
    // text.len() when the span reaches end-of-string).
    let total_cps = text.chars().count();
    let mut spans: Vec<(usize, usize, String)> = Vec::with_capacity(ranges.len());
    let mut cursor_cp = 0usize; // overlap guard: next allowed start (code points)
    for (s, e, id) in ranges {
        if e >= total_cps || s < cursor_cp {
            // Out of range, or overlaps/touches a previous span → bail entirely.
            return Vec::new();
        }
        let mut b0: Option<usize> = None;
        let mut b1: Option<usize> = None;
        for (cp_idx, (byte_idx, _ch)) in text.char_indices().enumerate() {
            if b0.is_none() && cp_idx >= s {
                b0 = Some(byte_idx);
            }
            if cp_idx >= e + 1 {
                b1 = Some(byte_idx);
                break;
            }
        }
        let b0 = match b0 {
            Some(b) => b,
            None => return Vec::new(),
        };
        let b1 = b1.unwrap_or(text.len()); // span ended at end-of-string
        if b1 <= b0 || text.get(b0..b1).is_none() {
            return Vec::new();
        }
        spans.push((b0, b1, id));
        cursor_cp = e + 1;
    }
    spans
}

/// Split `text` on Unicode whitespace, emitting an `Emote` segment for each
/// whitespace-delimited token that exactly (case-sensitively) matches the
/// third-party `emote_map`, and `Text` for everything else (whitespace runs and
/// non-matching words), preserving the original spacing. Used both for whole
/// messages without first-party emotes and for the text gaps between them.
pub(super) fn word_match_segments(
    text: &str,
    emote_map: &HashMap<String, std::path::PathBuf>,
) -> Vec<ChatSegment> {
    if text.is_empty() {
        return Vec::new();
    }
    if emote_map.is_empty() {
        return vec![ChatSegment::Text(text.to_string())];
    }
    let mut out: Vec<ChatSegment> = Vec::new();
    let mut pending = String::new(); // accumulates Text (whitespace + non-emote words)
    // Walk maximal non-whitespace runs (candidate emote codes) and the whitespace
    // between them, so tabs / NBSP / multiple spaces are preserved verbatim.
    let mut rest = text;
    while !rest.is_empty() {
        // Leading whitespace run.
        let ws_end = rest
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        if ws_end > 0 {
            pending.push_str(&rest[..ws_end]);
            rest = &rest[ws_end..];
            continue;
        }
        // Non-whitespace token run.
        let tok_end = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let token = &rest[..tok_end];
        if let Some(path) = emote_map.get(token) {
            if !pending.is_empty() {
                out.push(ChatSegment::Text(std::mem::take(&mut pending)));
            }
            out.push(ChatSegment::Emote {
                name: token.to_string(),
                file: Some(path.clone()),
                fallback_text: None,
                pending: None,
            });
        } else {
            pending.push_str(token);
        }
        rest = &rest[tok_end..];
    }
    if !pending.is_empty() {
        out.push(ChatSegment::Text(pending));
    }
    out
}
// ── Chat viewer helpers ──────────────────────────────────────────────────────

/// Derive the chat sidecar path from a recording's output path.
/// Locate a recording's chat sidecar. yt-dlp's `live_chat` writer **appends** to the
/// `-o` value (keeping the video extension), so the YouTube sidecar is
/// `<output_path>.live_chat.json` (e.g. `clip.mkv.live_chat.json`) — not a simple
/// extension swap. The Twitch native logger instead **replaces** the extension
/// (`clip.chat.jsonl`). We try both forms, plus the legacy pre-`.cache` YouTube name
/// (`clip.ts.live_chat.json`).
pub(super) fn chat_file_for_recording(rec: &Recording) -> Option<std::path::PathBuf> {
    chat_file_candidates(rec).into_iter().find(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::ChatSidecar, p))
}

/// The candidate sidecar paths [`chat_file_for_recording`] probes, in order.
///
/// An explicit [`Recording::chat_path`] always comes first and, when set, is
/// the only candidate — persisted at spawn for every producer since the
/// dedicated chat-root feature. The derived fallbacks (legacy takes, plus
/// chat-root mirrors) live in [`crate::chat::chat_file_candidates`], shared
/// with the migration sweep.
pub(super) fn chat_file_candidates(rec: &Recording) -> Vec<std::path::PathBuf> {
    crate::chat::chat_file_candidates(&rec.chat_path, &rec.output_path)
}

/// [`chat_file_for_recording`] for render paths: existence via the non-blocking
/// [`FsProbes`] cache, so per-frame callers (the chat popup's recording picker,
/// the Streams-grid context menus) never stat the recordings drive themselves.
/// Answers can lag a probe round-trip (~a frame) behind the direct version.
pub(super) fn chat_file_for_recording_cached(
    fs: &mut FsProbes,
    rec: &Recording,
) -> Option<std::path::PathBuf> {
    chat_file_candidates(rec).into_iter().find(|p| fs.is_file(p))
}

pub(super) fn fmt_recording_label(rec: &Recording) -> String {
    let dt = chrono::DateTime::from_timestamp(rec.started_at, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| rec.started_at.to_string());
    format!("{dt} ({})", rec.status)
}

pub(super) fn fmt_chat_ts(secs: f64) -> String {
    if secs < 0.0 {
        return format!("-{}", fmt_chat_ts(-secs));
    }
    let s = secs as u64;
    format!("[{:02}:{:02}:{:02}]", s / 3600, (s % 3600) / 60, s % 60)
}

/// Soft cap on decoded emote-frame GPU memory; the cache is LRU-evicted past this.
pub(super) const EMOTE_BUDGET_BYTES: usize = 192 * 1024 * 1024;

/// Chat-replay text appearance: font size (points) applied uniformly to the
/// timestamp/message/username, plus their colors. Global/shared across every
/// open chat window (`StreamArchiverApp::chat_font_pt`/`chat_ts_color`/
/// `chat_text_color`), edited from the ⚙ "Chat Appearance" panel inside each
/// chat window rather than the global Settings dialog.
pub(super) struct ChatAppearance {
    pub(super) font_pt: f32,
    /// Emote/emoji pixel size, independent of `font_pt` — see
    /// `StreamArchiverApp::chat_emote_pt`'s doc.
    pub(super) emote_pt: f32,
    pub(super) ts_color: egui::Color32,
    pub(super) text_color: egui::Color32,
}

/// A username click in the chat replay — everything the usercard needs to
/// build its local-only fields immediately; the live Twitch lookup (avatar/
/// account-created date) is fetched separately, keyed by `user_id`. Also
/// built (cloned) for each row of the Users-in-chat panel, which needs the
/// same shape to open a usercard on click without re-scanning the log.
#[derive(Clone)]
pub(super) struct UserCardClick {
    pub(super) login: String,
    pub(super) display_name: String,
    pub(super) color: Option<egui::Color32>,
    pub(super) badges: Vec<String>,
    pub(super) badge_icons: Vec<Option<std::path::PathBuf>>,
    pub(super) badge_info: String,
    pub(super) user_id: String,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_chat_message(
    ui: &mut egui::Ui,
    msg: &ChatMessage,
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    render_emotes: bool,
    animate: bool,
    now: f64,
    misses: &mut Vec<std::path::PathBuf>,
    ctx: &egui::Context,
    appearance: &ChatAppearance,
) -> Option<UserCardClick> {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        // Timestamp — monospace, sized/colored to match the message body
        // (Twitch's own popout renders both at the same size).
        ui.label(
            egui::RichText::new(fmt_chat_ts(msg.timestamp_secs))
                .monospace()
                .size(appearance.font_pt)
                .color(appearance.ts_color),
        );
        // System notice (moderation marker: mode change, timeout/ban, clear)
        // — muted ℹ line, no author/badges.
        if msg.system {
            ui.label(
                egui::RichText::new(format!("ℹ {}", msg.text))
                    .italics()
                    .color(ui.visuals().weak_text_color()),
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
        // chat panel's background so dark colours stay legible. Clickable on
        // Twitch (opens the usercard) — YouTube messages carry no `login`/
        // `user_id` to build one from, so they stay a plain label.
        let name_color = chat_username_color(msg, ui.visuals().panel_fill);
        let name_text = egui::RichText::new(format!("{}:", msg.author))
            .strong()
            .size(appearance.font_pt)
            .color(name_color);
        let mut click: Option<UserCardClick> = None;
        if matches!(msg.platform, ChatPlatform::Twitch) && !msg.login.is_empty() {
            let resp = ui
                .add(egui::Label::new(name_text).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Click for user info");
            if resp.clicked() {
                click = Some(UserCardClick {
                    login: msg.login.clone(),
                    display_name: msg.author.clone(),
                    color: msg.color_override,
                    badges: msg.badges.clone(),
                    badge_icons: msg.badge_icons.clone(),
                    badge_info: msg.badge_info.clone(),
                    user_id: msg.user_id.clone(),
                });
            }
        } else {
            ui.label(name_text);
        }
        // Reply-thread prefix (Twitch): who this message answers.
        if !msg.reply_to.is_empty() {
            ui.label(
                egui::RichText::new(format!("↩ {}", msg.reply_to))
                    .small()
                    .color(ui.visuals().weak_text_color()),
            )
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
                ui.label(egui::RichText::new(t).strikethrough().weak().size(appearance.font_pt))
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
                            .size(appearance.font_pt)
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

/// CDN URL for an emote given provider, id, and extension.
pub(super) fn emote_cdn_url(provider: EmoteProvider, id: &str, ext: &str) -> String {
    match provider {
        EmoteProvider::Twitch => {
            if ext == "gif" {
                format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/animated/dark/3.0")
            } else {
                format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/static/dark/3.0")
            }
        }
        EmoteProvider::SevenTv => format!("https://cdn.7tv.app/emote/{id}/4x.{ext}"),
        EmoteProvider::Bttv => format!("https://cdn.betterttv.net/emote/{id}/3x.{ext}"),
        EmoteProvider::Ffz => format!("https://cdn.frankerfacez.com/emoticon/{id}/4"),
    }
}
/// Copy an image file's raw bytes to the Windows clipboard under the `PNG` format.
/// Most apps (Discord, browsers, image editors) accept `CF_PNG` for paste.
pub(super) fn copy_emote_image_to_clipboard(path: &std::path::Path) {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    let Ok(bytes) = crate::iomon::fs::read_sync(crate::iomon::Cat::AssetCache, path) else { return };

    let fmt_name: Vec<u16> = "PNG\0".encode_utf16().collect();
    let fmt = unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(fmt_name.as_ptr())) };
    if fmt == 0 {
        return;
    }

    unsafe {
        let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) else { return };
        let ptr = GlobalLock(hmem);
        if ptr.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(hmem);

        if OpenClipboard(None).is_ok() {
            let _ = EmptyClipboard();
            // SetClipboardData takes ownership of hmem on success; do not free it.
            let _ = SetClipboardData(
                fmt,
                Some(windows::Win32::Foundation::HANDLE(hmem.0 as *mut std::ffi::c_void)),
            );
            let _ = CloseClipboard();
        }
    }
}

/// Lay out a provider's emotes as a wrapping grid of fixed-width cells: the emote
/// image above its code. `deprecated` cells skip the image entirely (the file is
/// gone) — they show a 🚫 placeholder and strike through the code. Loading cells
/// show a `…` until the off-thread decode lands.
#[allow(clippy::too_many_arguments)]
pub(super) fn emote_viewer_grid(
    ui: &mut egui::Ui,
    emotes: &[ViewerEmote],
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    animate: bool,
    now: f64,
    misses: &mut Vec<std::path::PathBuf>,
    ctx: &egui::Context,
    deprecated: bool,
    provider: EmoteProvider,
    pending_properties: &mut Option<ViewerEmote>,
) {
    const CELL_W: f32 = 92.0;
    const IMG_H: f32 = 44.0;
    ui.horizontal_wrapped(|ui| {
        for e in emotes {
            let cell = ui.allocate_ui(egui::vec2(CELL_W, IMG_H + 22.0), |ui| {
                // Virtualize: only decode/upload/draw emotes whose cell is on screen.
                // `draw_cached_emote` stamps `last_drawn = now` on every Ready entry it
                // touches, which pins it against `evict_emote_cache` (it keeps anything
                // with `last_drawn >= now`). Drawing every emote each frame would pin the
                // entire provider — hundreds of animated emotes — past EMOTE_BUDGET_BYTES
                // and the LRU could never reclaim it. Off-screen cells reserve the same
                // band height (so wrap points / scroll extent stay put) but skip the cache
                // entirely, letting scrolled-away emotes age out and be evicted.
                let visible = ui.is_rect_visible(ui.max_rect());
                ui.vertical_centered(|ui| {
                    let img_resp = if deprecated {
                        ui.add_space((IMG_H - 18.0) / 2.0);
                        ui.label(egui::RichText::new("🚫").size(18.0).weak());
                        ui.add_space((IMG_H - 18.0) / 2.0);
                        None
                    } else if !visible {
                        ui.add_space(IMG_H);
                        None
                    } else {
                        let r = draw_cached_emote(ui, cache, &e.path, animate, IMG_H, now, misses, ctx);
                        if r.is_none() {
                            ui.add_space(IMG_H / 2.0 - 6.0);
                            ui.weak("…");
                            ui.add_space(IMG_H / 2.0 - 6.0);
                        }
                        r
                    };

                    // Alt-hover: show enlarged image + emote info as a tooltip.
                    // on_hover_ui_at_pointer takes self; clone the response so
                    // img_resp stays usable for the label below.
                    if let Some((resp, _)) = img_resp.clone() {
                        if resp.hovered() && ctx.input(|i| i.modifiers.alt) {
                            let (epath, ename, eid, eext) = (
                                e.path.clone(),
                                e.name.clone(),
                                e.id.clone(),
                                e.ext.clone(),
                            );
                            resp.on_hover_ui_at_pointer(|ui| {
                                ui.set_max_width(280.0);
                                // Render cached texture at 3-4× cell size.
                                // The cache caps decode at 56 px so no re-upload.
                                draw_cached_emote(
                                    ui, cache, &epath, false, 160.0, now,
                                    &mut Vec::new(), ctx,
                                );
                                ui.separator();
                                let url = emote_cdn_url(provider, &eid, &eext);
                                egui::Grid::new(
                                    egui::Id::new("alt_emote_tip").with(&eid),
                                )
                                .num_columns(2)
                                .show(ui, |ui| {
                                    ui.label("Name:");
                                    ui.label(&ename);
                                    ui.end_row();
                                    ui.label("ID:");
                                    ui.label(&eid);
                                    ui.end_row();
                                    ui.label("URL:");
                                    ui.label(&url);
                                    ui.end_row();
                                });
                            });
                        }
                    }

                    let mut rt = egui::RichText::new(truncate_label(&e.name, 12)).small();
                    if deprecated {
                        rt = rt.strikethrough().weak();
                    }
                    ui.label(rt).on_hover_text(&e.name);
                });
            });

            // Right-click context menu on the entire cell.
            // allocate_ui returns Sense::hover(), which makes secondary_clicked()
            // always false and context_menu never fires. Re-interact with Sense::click()
            // on the same rect so the right-click is detected properly.
            let ctx_resp = ui.interact(
                cell.response.rect,
                egui::Id::new("emote_ctx").with(&e.id),
                egui::Sense::click(),
            );
            ctx_resp.context_menu(|ui| {
                if ui.button("Copy Image").clicked() {
                    copy_emote_image_to_clipboard(&e.path);
                    ui.close();
                }
                if ui.button("Open File").clicked() {
                    open_path(&e.path);
                    ui.close();
                }
                if ui.button("Open Folder").clicked() {
                    if let Some(dir) = e.path.parent() {
                        open_path(dir);
                    }
                    ui.close();
                }
                if ui.button("Copy URL").clicked() {
                    ui.ctx().copy_text(emote_cdn_url(provider, &e.id, &e.ext));
                    ui.close();
                }
                ui.separator();
                if ui.button("Properties").clicked() {
                    *pending_properties = Some(ViewerEmote {
                        name: e.name.clone(),
                        id: e.id.clone(),
                        ext: e.ext.clone(),
                        path: e.path.clone(),
                        exists: e.exists,
                    });
                    ui.close();
                }
            });
        }
    });
}

/// Draw an emote from the decode cache. Returns the image `Response` when drawn, or
/// `None` (caller shows the text fallback) when the emote is still loading / failed.
/// Promotes a freshly-decoded entry to GPU textures (UI-thread upload), advances
/// the animation against the global clock `now`, and records `last_drawn` for LRU.
#[allow(clippy::too_many_arguments)]
/// Draws the emote and returns its `Response` plus a clone of the texture it
/// drew — the clone lets callers queue the standard Alt-hover full-resolution
/// preview ([`queue_alt_image_preview`]) without reaching back into the
/// mutex-guarded cache.
pub(super) fn draw_cached_emote(
    ui: &mut egui::Ui,
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    path: &Path,
    animate: bool,
    emote_h: f32,
    now: f64,
    misses: &mut Vec<std::path::PathBuf>,
    ctx: &egui::Context,
) -> Option<(egui::Response, egui::TextureHandle)> {
    use crate::emote_anim::EmoteLoad;
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    // Promote Decoded → Ready by uploading the frames to GPU textures here (must be
    // on the UI thread / with a live `ctx`).
    if matches!(g.get(path), Some(EmoteLoad::Decoded(..))) {
        if let Some(EmoteLoad::Decoded(imgs, delays)) = g.remove(path) {
            let anim = crate::emote_anim::upload(imgs, delays, ctx, &path.to_string_lossy());
            g.insert(path.to_path_buf(), EmoteLoad::Ready(anim));
        }
    }
    match g.get_mut(path) {
        None => {
            g.insert(path.to_path_buf(), EmoteLoad::Loading);
            misses.push(path.to_path_buf());
            None
        }
        Some(EmoteLoad::Loading) | Some(EmoteLoad::Failed) | Some(EmoteLoad::Decoded(..)) => None,
        Some(EmoteLoad::Ready(anim)) => {
            anim.last_drawn = now;
            let s = anim.size();
            // Height ≤ emote_h, width capped at 112, aspect preserved. Never upscale
            // (`.min(1.0)`) — a small emote keeps its native size, matching the prior
            // loader behaviour. `s` is already downscaled to ≤56px at decode time.
            let scale = (emote_h / s.y.max(1.0)).min(112.0 / s.x.max(1.0)).min(1.0);
            let size = egui::vec2(s.x * scale, s.y * scale);
            if animate && anim.is_animated() {
                let (tex, remaining) = anim.frame_at(now);
                let tex = tex.clone();
                let resp = ui.add(
                    egui::Image::from_texture(&tex)
                        .fit_to_exact_size(size)
                        .sense(egui::Sense::click()),
                );
                // Only schedule the next frame for emotes actually on screen, so a
                // scrolled-away animation doesn't keep waking the UI.
                if ui.is_rect_visible(resp.rect) {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f32(
                        remaining.min(1.0),
                    ));
                }
                Some((resp, tex))
            } else {
                let (tex, _) = anim.frame_at(0.0);
                let tex = tex.clone();
                let resp = ui.add(
                    egui::Image::from_texture(&tex)
                        .fit_to_exact_size(size)
                        .sense(egui::Sense::click()),
                );
                Some((resp, tex))
            }
        }
    }
}

/// Evict the least-recently-drawn ready emotes once the decoded-frame cache exceeds
/// [`EMOTE_BUDGET_BYTES`]. Emotes drawn this frame (`last_drawn == now`) are kept.
pub(super) fn evict_emote_cache(
    cache: &Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>,
    now: f64,
) {
    use crate::emote_anim::EmoteLoad;
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    let total: usize = g
        .values()
        .map(|v| if let EmoteLoad::Ready(a) = v { a.bytes } else { 0 })
        .sum();
    if total <= EMOTE_BUDGET_BYTES {
        return;
    }
    let mut ready: Vec<(std::path::PathBuf, f64, usize)> = g
        .iter()
        .filter_map(|(k, v)| match v {
            EmoteLoad::Ready(a) => Some((k.clone(), a.last_drawn, a.bytes)),
            _ => None,
        })
        .collect();
    ready.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut cur = total;
    for (k, last_drawn, bytes) in ready {
        if cur <= EMOTE_BUDGET_BYTES {
            break;
        }
        if last_drawn >= now {
            continue; // visible this frame — keep
        }
        g.remove(&k);
        cur -= bytes;
    }
}

pub(super) fn badge_display(badge: &str, platform: &ChatPlatform) -> (&'static str, egui::Color32) {
    match platform {
        ChatPlatform::Twitch => {
            let name = badge.split('/').next().unwrap_or(badge);
            match name {
                "broadcaster" => ("📡", egui::Color32::from_rgb(0xe9, 0x1e, 0x63)),
                "moderator" | "mod" => ("⚔", egui::Color32::from_rgb(0x00, 0xad, 0x03)),
                "subscriber" => ("★", egui::Color32::from_rgb(0x96, 0x4b, 0xff)),
                "bits" => ("💎", egui::Color32::from_rgb(0x00, 0xc7, 0xac)),
                "premium" => ("👑", egui::Color32::from_rgb(0xff, 0xd7, 0x00)),
                "partner" => ("✓", egui::Color32::from_rgb(0x97, 0x45, 0xff)),
                _ => ("•", egui::Color32::GRAY),
            }
        }
        ChatPlatform::YouTube => {
            let lower = badge.to_lowercase();
            if lower.contains("member") {
                ("⭐", egui::Color32::from_rgb(0xff, 0xd7, 0x00))
            } else if lower.contains("moderator") {
                ("⚔", egui::Color32::from_rgb(0x00, 0xad, 0x03))
            } else if lower.contains("verified") || lower.contains("owner") {
                ("✓", egui::Color32::from_rgb(0x4a, 0xc2, 0xff))
            } else {
                ("•", egui::Color32::GRAY)
            }
        }
    }
}

/// The display colour for a chat author's name, adjusted to stay legible on the
/// chat panel's background `bg`. The base colour mirrors each platform: a Twitch
/// user's chosen USERCOLOR (or their deterministic default from Twitch's 15-colour
/// palette), and YouTube's role-based name colours (mod/member/owner/regular).
pub(super) fn chat_username_color(msg: &ChatMessage, bg: egui::Color32) -> egui::Color32 {
    let base = match (msg.color_override, &msg.platform) {
        // Twitch USERCOLOR (IRC `color` tag), used as-is by both platforms when set.
        (Some(c), _) => c,
        (None, ChatPlatform::Twitch) => twitch_username_color(&msg.author),
        (None, ChatPlatform::YouTube) => youtube_username_color(&msg.badges),
    };
    readable_color(base, bg)
}

/// Twitch's 15 default name colours, assigned to users who never picked one.
/// Twitch keys this off the name (first + last char), so the same user is always
/// the same colour — we reproduce that exactly for ASCII names.
pub(super) fn twitch_username_color(name: &str) -> egui::Color32 {
    const DEFAULTS: [egui::Color32; 15] = [
        egui::Color32::from_rgb(0xFF, 0x00, 0x00), // Red
        egui::Color32::from_rgb(0x00, 0x00, 0xFF), // Blue
        egui::Color32::from_rgb(0x00, 0x80, 0x00), // Green
        egui::Color32::from_rgb(0xB2, 0x22, 0x22), // FireBrick
        egui::Color32::from_rgb(0xFF, 0x7F, 0x50), // Coral
        egui::Color32::from_rgb(0x9A, 0xCD, 0x32), // YellowGreen
        egui::Color32::from_rgb(0xFF, 0x45, 0x00), // OrangeRed
        egui::Color32::from_rgb(0x2E, 0x8B, 0x57), // SeaGreen
        egui::Color32::from_rgb(0xDA, 0xA5, 0x20), // GoldenRod
        egui::Color32::from_rgb(0xD2, 0x69, 0x1E), // Chocolate
        egui::Color32::from_rgb(0x5F, 0x9E, 0xA0), // CadetBlue
        egui::Color32::from_rgb(0x1E, 0x90, 0xFF), // DodgerBlue
        egui::Color32::from_rgb(0xFF, 0x69, 0xB4), // HotPink
        egui::Color32::from_rgb(0x8A, 0x2B, 0xE2), // BlueViolet
        egui::Color32::from_rgb(0x00, 0xFF, 0x7F), // SpringGreen
    ];
    let b = name.as_bytes();
    if b.is_empty() {
        return egui::Color32::GRAY;
    }
    let n = (b[0] as usize + b[b.len() - 1] as usize) % DEFAULTS.len();
    DEFAULTS[n]
}

/// YouTube live-chat name colours by role (derived from the author's badges):
/// moderator blue, member green, owner gold, and a neutral grey for everyone else
/// (YouTube doesn't per-user colour regular names). Readability is applied later.
pub(super) fn youtube_username_color(badges: &[String]) -> egui::Color32 {
    let has = |needle: &str| badges.iter().any(|b| b.to_lowercase().contains(needle));
    if has("owner") {
        egui::Color32::from_rgb(0xFF, 0xD6, 0x00) // channel owner — gold
    } else if has("moderator") {
        egui::Color32::from_rgb(0x5E, 0x84, 0xF1) // YouTube moderator blue
    } else if has("member") {
        egui::Color32::from_rgb(0x2B, 0xA6, 0x40) // YouTube member green
    } else {
        egui::Color32::from_rgb(0xB0, 0xB0, 0xB0) // regular — neutral grey
    }
}

/// WCAG relative luminance of a colour (sRGB → linear, then the standard weights).
pub(super) fn relative_luminance(c: egui::Color32) -> f32 {
    let lin = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio between two colours (1.0 = identical, 21.0 = black/white).
pub(super) fn contrast_ratio(a: egui::Color32, b: egui::Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Nudge `fg`'s lightness away from the background (lighter on a dark bg, darker on
/// a light bg) until it clears a contrast floor, preserving hue — the way Twitch
/// lightens dark name colours in dark mode so e.g. pure blue stays legible. Returns
/// `fg` unchanged when it's already comfortable.
pub(super) fn readable_color(fg: egui::Color32, bg: egui::Color32) -> egui::Color32 {
    // Slightly under WCAG AA (4.5): names are bold, and staying closer keeps the
    // colour vivid rather than washing it toward white/black.
    const TARGET: f32 = 4.0;
    if contrast_ratio(fg, bg) >= TARGET {
        return fg;
    }
    // Push toward whichever extreme can actually out-contrast the background, not a
    // flat luminance midpoint — for a mid-tone background, lightening toward white
    // may never reach the target while darkening toward black does (and vice-versa).
    let lighten = contrast_ratio(egui::Color32::WHITE, bg) >= contrast_ratio(egui::Color32::BLACK, bg);
    let (h, s, mut l) = rgb_to_hsl(fg);
    let mut out = fg;
    for _ in 0..50 {
        l = if lighten { (l + 0.02).min(1.0) } else { (l - 0.02).max(0.0) };
        out = hsl_to_rgb(h, s, l);
        if contrast_ratio(out, bg) >= TARGET {
            return out;
        }
        if l <= 0.0 || l >= 1.0 {
            break; // can't push further; return the best we reached
        }
    }
    out
}

/// sRGB → HSL (hue degrees 0–360, saturation/lightness 0–1).
pub(super) fn rgb_to_hsl(c: egui::Color32) -> (f32, f32, f32) {
    let (r, g, b) = (
        c.r() as f32 / 255.0,
        c.g() as f32 / 255.0,
        c.b() as f32 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-6 {
        return (0.0, 0.0, l); // achromatic (grey)
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    ((h * 60.0).rem_euclid(360.0), s, l)
}

/// HSL → sRGB (inverse of [`rgb_to_hsl`]).
pub(super) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> egui::Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| (((v + m) * 255.0).round()).clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgb(to(r1), to(g1), to(b1))
}

pub(super) fn parse_chat_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

/// `#RRGGBB` for `c`, ignoring alpha — inverse of [`parse_chat_hex_color`].
pub(super) fn hex_color_string(c: egui::Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b())
}

/// Per-channel linear interpolation between two opaque colors, `t` in `0..=1`.
pub(super) fn lerp_color32(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    egui::Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Paint a left-to-right gradient banner strip (the user's color fading into
/// the panel background) at the current cursor, reserving `height` px of
/// vertical space. Purely decorative — Twitch exposes no per-viewer banner
/// image via the public API, so this approximates the look of the 7TV/
/// native usercard banners without a network fetch.
pub(super) fn paint_user_banner(ui: &mut egui::Ui, user_color: egui::Color32, height: f32) {
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
pub(super) fn summarize_user_events(
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

/// The most recent Hype Train for a broadcast, everything the chat replay
/// needs to draw a Twitch-style progress bar (or, once it's over, a static
/// reached-level summary) — see [`load_broadcast_stats`]'s doc for where
/// `goal`/`expires_at` come from.
pub(super) struct HypeTrainDisplay {
    /// Pre-formatted line (`detectors::HypeTrainState::detail()`), shown
    /// as-is once the train's no longer running (or `goal`/`expires_at`
    /// weren't captured — pre-v86 rows, or an inference-only "(inferred)"
    /// row that GQL never confirmed).
    pub(super) detail: String,
    pub(super) level: i64,
    pub(super) total: i64,
    pub(super) goal: i64,
    pub(super) expires_at: i64,
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
pub(super) fn load_broadcast_stats(
    store: &crate::store::Store,
    monitor_id: i64,
    since: i64,
    until: i64,
) -> (Vec<(String, i64)>, Vec<(String, i64)>, Option<HypeTrainDisplay>) {
    let events = store.stream_events_for_monitor_range(monitor_id, since, until).unwrap_or_default();
    let top_gifters = crate::ui::channel_stats::top_contributors(&events, "subgift", 5);
    let top_cheerers = crate::ui::channel_stats::top_contributors(&events, "bits", 5);
    let hype_train = events
        .iter()
        .filter(|e| e.kind == "hype_train")
        .max_by_key(|e| e.at)
        .map(|e| HypeTrainDisplay {
            detail: e.detail.clone(),
            level: e.level,
            total: e.amount,
            goal: e.goal,
            expires_at: e.expires_at,
        });
    (top_gifters, top_cheerers, hype_train)
}

/// Role section a Twitch chatter is grouped under in the Users-in-chat
/// panel, from the highest-priority badge on their message. No "Chat Bots"
/// section (unlike Twitch's own list) — there's no reliable local signal for
/// bot accounts (no badge marks a bot as such); they just land in Users.
pub(super) fn user_role_label(badges: &[String]) -> &'static str {
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
pub(super) fn build_users_panel(log: &ChatLog) -> Vec<ChatUserEntry> {
    let mut latest: HashMap<&str, &ChatMessage> = HashMap::new();
    for m in &log.messages {
        if matches!(m.platform, ChatPlatform::Twitch) && !m.login.is_empty() {
            latest.insert(&m.login, m);
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
                user_id: m.user_id.clone(),
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

/// First existing first-party Twitch emote image for `id` in `dir`, trying the
/// formats Twitch uses (static `.png`, animated `.gif`) plus `.webp`, and —
/// per extension — the current `{id}_{name}.{ext}` filename the fetcher
/// writes before falling back to the pre-rename `{id}.{ext}` form. `None`
/// when none exist.
pub(super) fn find_emote_file(dir: &Path, id: &str, name: &str) -> Option<std::path::PathBuf> {
    let sanitized = crate::assets::sanitize_emote_name(name);
    ["png", "gif", "webp"].iter().find_map(|ext| {
        let new_path = dir.join(format!("{id}_{sanitized}.{ext}"));
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &new_path) {
            return Some(new_path);
        }
        let old_path = dir.join(format!("{id}.{ext}"));
        crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &old_path).then_some(old_path)
    })
}

/// Look up `id`/`name` in a precomputed cross-channel stem index (see
/// `assets::index_emote_stems`) — an O(1) hashmap probe of both filename
/// forms instead of a filesystem stat, since this runs once per first-party
/// emote OCCURRENCE (a chat log routinely repeats the same emote hundreds of
/// times) times however many other channels are archived.
pub(super) fn find_emote_fallback(
    index: &HashMap<String, std::path::PathBuf>,
    id: &str,
    name: &str,
) -> Option<std::path::PathBuf> {
    let sanitized = crate::assets::sanitize_emote_name(name);
    index
        .get(&format!("{id}_{sanitized}"))
        .or_else(|| index.get(id))
        .cloned()
}

/// An emoji image not yet on disk that the renderer would otherwise show as a
/// glyph. Collected during parse; the popup tries each `url` in order (Twemoji's
/// FE0F naming is irregular) and writes the first that succeeds to `dest`.
/// `pub(crate)`: also built/consumed by the "Fetch missing chat emotes"
/// maintenance sweep (`downloader::supervisor::cmd_fetch_missing_chat_emotes`),
/// which reuses this same struct + `download_emoji_images` rather than a
/// parallel download mechanism.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EmojiFetch {
    pub(crate) dest: std::path::PathBuf,
    pub(crate) urls: Vec<String>,
}

/// One parsed slice of a chat file: the messages, the emoji images to fetch,
/// and the byte offset just past the last complete line — the resume point for
/// the next incremental pass. `pub(crate)` for the same reason as `EmojiFetch`.
pub(crate) struct ChatChunk {
    // `ChatMessage`/`MarkerAt` stay UI-internal (`pub(super)`) — the
    // maintenance sweep this struct is also shared with only ever reads
    // `fetches`/`parsed_to`, never these.
    pub(super) messages: Vec<ChatMessage>,
    pub(crate) fetches: Vec<EmojiFetch>,
    pub(crate) parsed_to: u64,
    /// Moderation markers found in this byte range (Twitch sidecars only).
    pub(super) markers: Vec<MarkerAt>,
}

/// Split a text run into [`ChatSegment`]s, turning each Unicode-emoji cluster into
/// an `Emote` that resolves to a cached Twemoji image (with the glyph as fallback),
/// and recording any not-yet-downloaded image in `fetches`. Plain text passes
/// through unchanged (fast path).
pub(super) fn emoji_split(text: &str, fetches: &mut Vec<EmojiFetch>) -> Vec<ChatSegment> {
    let runs = crate::emoji::segment(text);
    if runs.iter().all(|(_, is_emoji)| !is_emoji) {
        return vec![ChatSegment::Text(text.to_string())];
    }
    let emoji_dir = emoji_cache_dir();
    let mut out = Vec::with_capacity(runs.len());
    for (slice, is_emoji) in runs {
        if is_emoji {
            let key = crate::emoji::cache_key(slice);
            let dest = emoji_dir.join(format!("{key}.png"));
            let file = crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &dest).then(|| dest.clone());
            // Skip re-fetching emoji we've already failed to download (a `.404`
            // marker), so a liberal false-positive / missing asset isn't re-requested
            // on every live tail-reload.
            if file.is_none() && !crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, emoji_dir.join(format!("{key}.404"))) {
                fetches.push(EmojiFetch {
                    dest: dest.clone(),
                    urls: crate::emoji::twemoji_url_candidates(slice),
                });
            }
            let pending = file.is_none().then_some(dest);
            out.push(ChatSegment::Emote {
                name: slice.to_string(),
                file,
                fallback_text: Some(slice.to_string()),
                pending,
            });
        } else if !slice.is_empty() {
            out.push(ChatSegment::Text(slice.to_string()));
        }
    }
    out
}

/// The shared emoji image cache directory (`asset-cache/emotes/emoji/`).
pub(super) fn emoji_cache_dir() -> std::path::PathBuf {
    crate::app_paths::asset_cache_dir()
        .join("emotes")
        .join("emoji")
}

/// Expand the `Text` segments of an already-built segment list, splitting out any
/// Unicode emoji into image segments. Emote segments are left untouched.
pub(super) fn expand_emoji(segments: Vec<ChatSegment>, fetches: &mut Vec<EmojiFetch>) -> Vec<ChatSegment> {
    let mut out = Vec::with_capacity(segments.len());
    for seg in segments {
        match seg {
            ChatSegment::Text(t) => out.extend(emoji_split(&t, fetches)),
            other => out.push(other),
        }
    }
    out
}

/// File extension to use for a downloaded image, from the URL path (png/gif/webp),
/// defaulting to `png`.
pub(super) fn url_ext(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .and_then(|p| p.rsplit('.').next())
        .filter(|e| matches!(*e, "png" | "gif" | "webp" | "jpg" | "jpeg"))
        .unwrap_or("png")
}

/// How much of the file's tail the phase-1 (instant) parse covers. Enough for
/// hundreds of Twitch lines / dozens of (much fatter) YouTube lines.
pub(super) const CHAT_TAIL_BYTES: u64 = 512 * 1024;

/// Parse the byte range `[from, to)` of a chat file (`to == None` reads to the
/// current EOF). Only complete (newline-terminated) lines are parsed; a
/// trailing partial line — the logger may be mid-write — is left for the next
/// pass via `parsed_to`, so incremental tail reads never split a message. Both
/// formats (Twitch `.chat.jsonl`, YouTube `.live_chat.json`) are line-delimited
/// JSON, so byte-offset resumption is exact.
#[allow(clippy::too_many_arguments)]
pub(super) fn parse_chat_chunk(
    path: &Path,
    from: u64,
    to: Option<u64>,
    start_unix_secs: i64,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
    twitch_fallback_index: &HashMap<String, std::path::PathBuf>,
    fetch_unknown_emotes: bool,
    source_partners: &HashMap<String, crate::models::CollabPartner>,
    badge_dirs: &TwitchBadgeDirs,
) -> anyhow::Result<ChatChunk> {
    use std::io::{Read, Seek, SeekFrom};
    // Read window: bounds peak memory on huge logs — the previous whole-range
    // slurp held roughly 2x the file size in RAM for a marathon stream's
    // phase-2 parse.
    const WINDOW: usize = 8 * 1024 * 1024;
    let chat_region = crate::iomon::classify(path);
    let mut f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let len = f.metadata()?.len();
    let end = to.unwrap_or(len).min(len);
    if from >= end {
        return Ok(ChatChunk {
            messages: Vec::new(),
            fetches: Vec::new(),
            parsed_to: from,
            markers: Vec::new(),
        });
    }
    f.seek(SeekFrom::Start(from))?;
    let is_yt = path.to_string_lossy().ends_with("live_chat.json");
    let start_ms = start_unix_secs as f64 * 1000.0;
    let mut messages = Vec::new();
    let mut fetches: Vec<EmojiFetch> = Vec::new();
    let mut markers: Vec<MarkerAt> = Vec::new();
    let mut parsed_to = from;
    let mut pos = from;
    // Carries a partial line across window boundaries.
    let mut buf: Vec<u8> = Vec::new();
    while pos < end {
        let take = WINDOW.min((end - pos) as usize);
        let old_len = buf.len();
        buf.resize(old_len + take, 0);
        let read_start = std::time::Instant::now();
        let read_res = f.read_exact(&mut buf[old_len..]);
        crate::iomon::record_region(
            crate::iomon::Cat::ChatSidecar,
            chat_region,
            crate::iomon::OpKind::Read,
            take as u64,
            read_start.elapsed(),
            true,
        );
        read_res?;
        pos += take as u64;
        let complete = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            // No boundary yet — a single line larger than the window; grow
            // the buffer with the next window.
            None if pos < end => continue,
            // A bounded chunk ends on a known line boundary; an unbounded
            // tail can end mid-line while the logger is writing.
            None if to.is_some() => buf.len(),
            None => 0,
        };
        {
            let text = String::from_utf8_lossy(&buf[..complete]);
            for line in text.lines() {
                if line.is_empty() {
                    continue;
                }
                if is_yt {
                    parse_yt_chat_line(line, &mut messages, &mut fetches);
                } else if line.contains("\"marker\":") {
                    // Moderation marker / notice line (cheap substring gate —
                    // markers are rare next to messages).
                    if let Some((marker, notice)) = parse_twitch_marker_line(line, start_ms) {
                        if let Some(m) = marker {
                            markers.push(m);
                        }
                        if let Some(n) = notice {
                            messages.push(n);
                        }
                    }
                } else if let Some(m) = parse_twitch_chat_line(
                    line,
                    start_ms,
                    emote_map,
                    twitch_dir,
                    twitch_fallback_index,
                    fetch_unknown_emotes,
                    &mut fetches,
                    source_partners,
                    badge_dirs,
                ) {
                    messages.push(m);
                }
            }
        }
        buf.drain(..complete);
        parsed_to = pos - buf.len() as u64;
    }
    // De-duplicate so the same emoji isn't downloaded once per occurrence.
    fetches.sort_by(|a, b| a.dest.cmp(&b.dest));
    fetches.dedup();
    Ok(ChatChunk { messages, fetches, parsed_to, markers })
}

/// The first line boundary within the file's last [`CHAT_TAIL_BYTES`] — where
/// the phase-1 tail parse starts. 0 for small files (just parse everything).
pub(super) fn chat_tail_start(path: &Path) -> anyhow::Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let len = f.metadata()?.len();
    if len <= CHAT_TAIL_BYTES {
        return Ok(0);
    }
    f.seek(SeekFrom::Start(len - CHAT_TAIL_BYTES))?;
    let mut buf = vec![0u8; CHAT_TAIL_BYTES as usize];
    let read_start = std::time::Instant::now();
    let read_res = f.read_exact(&mut buf);
    crate::iomon::record(
        crate::iomon::Cat::ChatSidecar,
        path,
        crate::iomon::OpKind::Read,
        CHAT_TAIL_BYTES,
        read_start.elapsed(),
    );
    read_res?;
    Ok(match buf.iter().position(|&b| b == b'\n') {
        Some(i) => len - CHAT_TAIL_BYTES + i as u64 + 1,
        None => 0, // no boundary in the tail (one giant line) — parse it all
    })
}

/// Parse a chunk of a chat file off the UI thread, mapping both join and parse
/// errors to a string for [`ChatLoadState::Error`]. `pub(crate)`: also used
/// by `downloader::supervisor::cmd_fetch_missing_chat_emotes` to parse a
/// whole log (`from: 0, to: None`) during the maintenance sweep.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn parse_chunk_blocking(
    path: std::path::PathBuf,
    from: u64,
    to: Option<u64>,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
    twitch_fallback_index: Arc<HashMap<String, std::path::PathBuf>>,
    fetch_unknown_emotes: bool,
    source_partners: Arc<HashMap<String, crate::models::CollabPartner>>,
    badge_dirs: Arc<TwitchBadgeDirs>,
) -> Result<ChatChunk, String> {
    tokio::task::spawn_blocking(move || {
        parse_chat_chunk(
            &path,
            from,
            to,
            start_ts,
            &emote_map,
            twitch_dir.as_deref(),
            &twitch_fallback_index,
            fetch_unknown_emotes,
            &source_partners,
            &badge_dirs,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// Promote pending emote images that have landed on disk since the parse.
/// The existence checks run OUTSIDE the chat mutex and off the async threads
/// (`spawn_blocking`) — stat-ing thousands of segments while holding the lock
/// the renderer takes every frame froze the whole app.
pub(super) async fn upgrade_pending_emotes(state: &Arc<Mutex<ChatLoadState>>) {
    let pending: Vec<std::path::PathBuf> = {
        let st = state.lock().unwrap();
        let ChatLoadState::Loaded(log) = &*st else { return };
        let mut set: HashSet<std::path::PathBuf> = HashSet::new();
        for m in &log.messages {
            for seg in &m.segments {
                if let ChatSegment::Emote { file: None, pending: Some(p), .. } = seg {
                    set.insert(p.clone());
                }
            }
        }
        set.into_iter().collect()
    };
    if pending.is_empty() {
        return;
    }
    let on_disk: HashSet<std::path::PathBuf> = tokio::task::spawn_blocking(move || {
        pending.into_iter().filter(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, p)).collect()
    })
    .await
    .unwrap_or_default();
    if on_disk.is_empty() {
        return;
    }
    if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
        for m in &mut log.messages {
            for seg in &mut m.segments {
                if let ChatSegment::Emote { file, pending, .. } = seg
                    && file.is_none()
                    && pending.as_ref().is_some_and(|p| on_disk.contains(p))
                {
                    *file = pending.take();
                }
            }
        }
    }
}

/// Download missing emoji/emoji-emote images (sequential, best-effort; 404s for a
/// liberally-detected non-emoji just leave the glyph). Capped so a pathological
/// message can't trigger thousands of requests.
///
/// `pace`: delay after each successful download, `None` for the interactive
/// per-popup path (`load_chat`/`tail_chat` — a user is waiting, and a normal
/// chat log only ever queues a handful of emoji). The "Fetch missing chat
/// emotes" maintenance sweep (`downloader::supervisor::cmd_fetch_missing_chat_emotes`)
/// passes `Some(150ms)` — matching every other bulk emote fetcher in
/// `assets.rs` (BTTV/FFZ/7TV/Twitch channel emotes all pace themselves the
/// same way) — since a sweep across months of chat logs can turn up
/// hundreds of distinct missing ids in one run, all hitting Twitch's CDN
/// back to back with no delay otherwise.
pub(crate) async fn download_emoji_images(fetches: &[EmojiFetch], pace: Option<std::time::Duration>) {
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return;
    };
    for f in fetches.iter().take(300) {
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &f.dest) {
            continue;
        }
        let mut got = false;
        let mut network_error = false; // a transient failure → don't negative-cache
        for url in &f.urls {
            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(bytes) = resp.bytes().await {
                        if let Some(parent) = f.dest.parent() {
                            let _ = crate::iomon::fs::create_dir_all(crate::iomon::Cat::AssetCache, parent).await;
                        }
                        if crate::iomon::fs::write(crate::iomon::Cat::AssetCache, &f.dest, &bytes).await.is_ok() {
                            got = true;
                            break;
                        }
                    }
                }
                // 404 = this candidate name doesn't exist → try the next candidate.
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {}
                // Any other HTTP status or a transport error is transient-ish.
                Ok(_) | Err(_) => network_error = true,
            }
        }
        if got && let Some(d) = pace {
            tokio::time::sleep(d).await;
        }
        // Negative-cache ONLY a definitive miss (every candidate 404'd, no network
        // error), so a transient offline failure can't permanently block a real
        // emoji. `dest` is `{key}.png` → marker `{key}.404`.
        if !got && !network_error {
            let marker = f.dest.with_extension("404");
            if let Some(parent) = marker.parent() {
                let _ = crate::iomon::fs::create_dir_all(crate::iomon::Cat::AssetCache, parent).await;
            }
            let _ = crate::iomon::fs::write(crate::iomon::Cat::AssetCache, &marker, b"").await;
        }
    }
}

/// Load a chat file into `state`, tail-first: the newest [`CHAT_TAIL_BYTES`]
/// parse and display immediately, then the rest of the file parses in the
/// background and is spliced in front (`loading_older` marks the gap). Then —
/// when `fetch_emoji` — missing emoji images download once and upgrade the
/// in-memory segments in place. Runs entirely off the UI thread.
#[allow(clippy::too_many_arguments)]
pub(super) async fn load_chat(
    state: Arc<Mutex<ChatLoadState>>,
    loading: Arc<AtomicBool>,
    path: Option<std::path::PathBuf>,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
    twitch_fallback_index: Arc<HashMap<String, std::path::PathBuf>>,
    fetch_unknown_emotes: bool,
    fetch_emoji: bool,
    source_partners: Arc<HashMap<String, crate::models::CollabPartner>>,
    badge_dirs: Arc<TwitchBadgeDirs>,
    ctx: egui::Context,
) {
    let Some(path) = path else {
        *state.lock().unwrap() = ChatLoadState::NoFile;
        ctx.request_repaint();
        return;
    };
    // Phase 1: the file's tail — the newest messages show instantly instead of
    // waiting for a full-file parse.
    let head_end = {
        let p = path.clone();
        match tokio::task::spawn_blocking(move || chat_tail_start(&p)).await {
            Ok(Ok(off)) => off,
            Ok(Err(e)) => {
                *state.lock().unwrap() = ChatLoadState::Error(e.to_string());
                ctx.request_repaint();
                return;
            }
            Err(e) => {
                *state.lock().unwrap() = ChatLoadState::Error(e.to_string());
                ctx.request_repaint();
                return;
            }
        }
    };
    let mut fetches = match parse_chunk_blocking(
        path.clone(),
        head_end,
        None,
        start_ts,
        emote_map.clone(),
        twitch_dir.clone(),
        twitch_fallback_index.clone(),
        fetch_unknown_emotes,
        source_partners.clone(),
        badge_dirs.clone(),
    )
    .await
    {
        Ok(chunk) => {
            let mut log = ChatLog {
                messages: chunk.messages,
                row_heights: Vec::new(),
                measured_width: 0.0,
                parsed_to: chunk.parsed_to,
                loading_older: head_end > 0,
                markers: chunk.markers,
            };
            log.apply_markers();
            *state.lock().unwrap() = ChatLoadState::Loaded(log);
            ctx.request_repaint();
            chunk.fetches
        }
        Err(e) => {
            *state.lock().unwrap() = ChatLoadState::Error(e);
            ctx.request_repaint();
            return;
        }
    };
    // Phase 2: everything before the tail, spliced in front when ready.
    if head_end > 0 {
        match parse_chunk_blocking(
            path.clone(),
            0,
            Some(head_end),
            start_ts,
            emote_map.clone(),
            twitch_dir.clone(),
            twitch_fallback_index.clone(),
            fetch_unknown_emotes,
            source_partners.clone(),
            badge_dirs.clone(),
        )
        .await
        {
            Ok(older) => {
                fetches.extend(older.fetches);
                if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
                    // Heights are index-parallel to messages: prepend matching
                    // estimates, or every measured tail height would be
                    // re-attributed to the oldest rows and the virtualized
                    // offsets/scrollbar would scramble.
                    if !log.row_heights.is_empty() {
                        let n = older.messages.len();
                        log.row_heights
                            .splice(0..0, std::iter::repeat_n(CHAT_ROW_EST, n));
                    }
                    log.messages.splice(0..0, older.messages);
                    log.loading_older = false;
                    // Tail markers may target just-prepended older messages
                    // (e.g. a ban purging someone's whole history).
                    log.markers.extend(older.markers);
                    log.apply_markers();
                }
            }
            Err(_) => {
                if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
                    log.loading_older = false;
                }
            }
        }
        ctx.request_repaint();
    }
    // Phase 3: emoji downloads + in-place upgrade. Only one download pass runs
    // at a time (a concurrent tail reload just skips kicking off another).
    fetches.sort_by(|a, b| a.dest.cmp(&b.dest));
    fetches.dedup();
    if fetch_emoji && !fetches.is_empty() && !loading.swap(true, Ordering::SeqCst) {
        download_emoji_images(&fetches, None).await;
        loading.store(false, Ordering::SeqCst);
        upgrade_pending_emotes(&state).await;
        ctx.request_repaint();
    }
}

/// Incremental tail reload for a live recording: parse only the bytes appended
/// since the last pass and push them onto the existing log. (The previous
/// implementation re-parsed the entire file every few seconds.)
#[allow(clippy::too_many_arguments)]
pub(super) async fn tail_chat(
    state: Arc<Mutex<ChatLoadState>>,
    loading: Arc<AtomicBool>,
    path: std::path::PathBuf,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
    twitch_fallback_index: Arc<HashMap<String, std::path::PathBuf>>,
    fetch_unknown_emotes: bool,
    fetch_emoji: bool,
    source_partners: Arc<HashMap<String, crate::models::CollabPartner>>,
    badge_dirs: Arc<TwitchBadgeDirs>,
    ctx: egui::Context,
) {
    let from = {
        match &*state.lock().unwrap() {
            ChatLoadState::Loaded(log) => Some(log.parsed_to),
            // Initial load still in flight — let it finish first.
            ChatLoadState::Loading => return,
            // The sidecar may have appeared since (opened seconds after the
            // recording started) or a transient read error cleared — retry the
            // full tail-first load instead of staying broken forever.
            ChatLoadState::NoFile | ChatLoadState::Error(_) => None,
        }
    };
    let Some(from) = from else {
        load_chat(
            state,
            loading,
            Some(path),
            start_ts,
            emote_map,
            twitch_dir,
            twitch_fallback_index,
            fetch_unknown_emotes,
            fetch_emoji,
            source_partners,
            badge_dirs,
            ctx,
        )
        .await;
        return;
    };
    let Ok(chunk) = parse_chunk_blocking(
        path,
        from,
        None,
        start_ts,
        emote_map,
        twitch_dir,
        twitch_fallback_index,
        fetch_unknown_emotes,
        source_partners,
        badge_dirs,
    )
    .await
    else {
        return;
    };
    // Always advance past parsed complete lines — a chunk of non-message lines
    // (tickers, moderation events) must not freeze the resume offset, or every
    // 3s pass re-reads an ever-growing suffix.
    if chunk.parsed_to > from || !chunk.messages.is_empty() {
        if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
            // Overlapping reloads on a slow read could double-append; only the
            // pass that still matches the resume offset lands.
            if log.parsed_to == from {
                log.messages.extend(chunk.messages);
                log.parsed_to = chunk.parsed_to;
                // A live deletion/purge marker targets messages already shown
                // — re-apply so they strike through on the next frame. (No new
                // markers = nothing to do: appended messages postdate every
                // stored marker.)
                if !chunk.markers.is_empty() {
                    log.markers.extend(chunk.markers);
                    log.apply_markers();
                }
            }
        }
        ctx.request_repaint();
    }
    if fetch_emoji && !chunk.fetches.is_empty() && !loading.swap(true, Ordering::SeqCst) {
        download_emoji_images(&chunk.fetches, None).await;
        loading.store(false, Ordering::SeqCst);
        upgrade_pending_emotes(&state).await;
        ctx.request_repaint();
    }
}

/// Parse one line of a YouTube `.live_chat.json` file (a line can carry several
/// messages in the VOD-replay format), appending to `out`.
pub(super) fn parse_yt_chat_line(line: &str, out: &mut Vec<ChatMessage>, fetches: &mut Vec<EmojiFetch>) {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    if let Some(replay) = v.get("replayChatItemAction") {
        // VOD replay format: replayChatItemAction.{videoOffsetTimeMsec, actions[]}
        let offset_ms = replay
            .get("videoOffsetTimeMsec")
            .and_then(|x| x.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| x.as_i64()));
        if let Some(actions) = replay.get("actions").and_then(|a| a.as_array()) {
            for action in actions {
                if let Some(msg) = yt_action_to_msg(action, offset_ms, fetches) {
                    out.push(msg);
                }
            }
        }
    } else if let Some(msg) = yt_action_to_msg(&v, None, fetches) {
        // Live format: addChatItemAction directly at the top level of each line.
        out.push(msg);
    }
}

pub(super) fn yt_action_to_msg(
    action: &serde_json::Value,
    offset_ms: Option<i64>,
    fetches: &mut Vec<EmojiFetch>,
) -> Option<ChatMessage> {
    let r = action.pointer("/addChatItemAction/item/liveChatTextMessageRenderer")?;
    let ts_secs = if let Some(ms) = offset_ms {
        ms as f64 / 1000.0
    } else {
        r["timestampUsec"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1_000_000.0
    };
    let author = r
        .pointer("/authorName/simpleText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // YouTube pre-tokenizes the body as `message.runs[]`: text runs are literal,
    // emoji runs carry either a standard unicode char (`emojiId`) or a custom
    // channel emoji (image-only). Build the display `segments` and the verbatim
    // search `text` in one pass.
    let mut text = String::new();
    let mut segments: Vec<ChatSegment> = Vec::new();
    if let Some(runs) = r["message"]["runs"].as_array() {
        for run in runs {
            if let Some(t) = run["text"].as_str() {
                text.push_str(t);
                // Text runs can themselves contain literal unicode emoji.
                segments.extend(emoji_split(t, fetches));
            } else if let Some(emoji) = run.get("emoji") {
                let shortcut = emoji["shortcuts"]
                    .as_array()
                    .and_then(|s| s.first())
                    .and_then(|e| e.as_str());
                let emoji_id = emoji["emojiId"].as_str();
                let label = shortcut.or(emoji_id).unwrap_or("[emoji]");
                text.push_str(label);
                if emoji["isCustomEmoji"].as_bool() == Some(true) {
                    // Custom channel emoji: image-only. Download YouTube's own PNG
                    // (largest thumbnail) into the cache; until present, fall back to
                    // the shortcut text.
                    let url = emoji
                        .pointer("/image/thumbnails")
                        .and_then(|t| t.as_array())
                        .and_then(|a| a.last())
                        .and_then(|t| t["url"].as_str());
                    let mut pending = None;
                    let file = emoji_id.zip(url).and_then(|(id, url)| {
                        let dest = crate::app_paths::asset_cache_dir()
                            .join("emotes")
                            .join("youtube")
                            .join(format!(
                                "{}.{}",
                                crate::downloader::sanitize_filename(id),
                                url_ext(url)
                            ));
                        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &dest) {
                            Some(dest)
                        } else {
                            fetches.push(EmojiFetch {
                                dest: dest.clone(),
                                urls: vec![url.to_string()],
                            });
                            pending = Some(dest);
                            None
                        }
                    });
                    segments.push(ChatSegment::Emote {
                        name: label.to_string(),
                        file,
                        fallback_text: None,
                        pending,
                    });
                } else {
                    // Standard unicode emoji: `emojiId` is the actual char(s) → route
                    // through the shared Twemoji emoji pipeline for colour.
                    let glyph = emoji_id.or(shortcut).unwrap_or("[emoji]");
                    segments.extend(emoji_split(glyph, fetches));
                }
            }
        }
    }
    let badges: Vec<String> = r["authorBadges"]
        .as_array()
        .map(|bs| {
            bs.iter()
                .filter_map(|b| {
                    b.pointer("/liveChatAuthorBadgeRenderer/tooltip")
                        .and_then(|t| t.as_str())
                        .map(|t| t.split('(').next().unwrap_or(t).trim().to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ChatMessage {
        timestamp_secs: ts_secs,
        author,
        text,
        segments,
        badges,
        badge_icons: Vec::new(),
        color_override: None,
        platform: ChatPlatform::YouTube,
        login: String::new(),
        msg_id: String::new(),
        deleted: None,
        system: false,
        reply_to: String::new(),
        source_name: String::new(),
        user_id: String::new(),
        badge_info: String::new(),
    })
}

/// Parse a moderation **marker line** from a Twitch sidecar (written live by
/// the chat logger: `{"ts":…,"marker":"del"|"purge"|"clear"|"notice",…}`).
/// Returns the marker to apply (if any) and a visible system notice message
/// (purges, clears, and `notice` lines get one; single deletions don't — the
/// strikethrough is enough).
fn parse_twitch_marker_line(line: &str, start_ms: f64) -> Option<(Option<MarkerAt>, Option<ChatMessage>)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = v["marker"].as_str()?;
    let ts_secs = (v["ts"].as_f64().unwrap_or(0.0) - start_ms) / 1000.0;
    let notice = |text: String| ChatMessage {
        timestamp_secs: ts_secs,
        author: String::new(),
        segments: vec![ChatSegment::Text(text.clone())],
        text,
        badges: Vec::new(),
        badge_icons: Vec::new(),
        color_override: None,
        platform: ChatPlatform::Twitch,
        login: String::new(),
        msg_id: String::new(),
        deleted: None,
        system: true,
        reply_to: String::new(),
        source_name: String::new(),
        user_id: String::new(),
        badge_info: String::new(),
    };
    match kind {
        "del" => {
            let id = v["id"].as_str().unwrap_or("");
            if id.is_empty() {
                return None;
            }
            let marker = ChatMarker::Delete { msg_id: id.to_string() };
            Some((Some(MarkerAt { ts_secs, marker }), None))
        }
        "purge" => {
            let login = v["login"].as_str().unwrap_or("").to_lowercase();
            if login.is_empty() {
                return None;
            }
            let reason = match v["secs"].as_i64() {
                Some(s) if s >= 3600 && s % 3600 == 0 => format!("timed out ({}h)", s / 3600),
                Some(s) if s >= 60 => format!("timed out ({}m)", s / 60),
                Some(s) => format!("timed out ({s}s)"),
                None => "banned".to_string(),
            };
            let n = notice(format!("{login} was {reason}"));
            Some((
                Some(MarkerAt { ts_secs, marker: ChatMarker::Purge { login, reason } }),
                Some(n),
            ))
        }
        "clear" => Some((
            Some(MarkerAt { ts_secs, marker: ChatMarker::Clear }),
            Some(notice("chat was cleared by moderators".into())),
        )),
        "notice" => {
            let text = v["text"].as_str().unwrap_or("").to_string();
            (!text.is_empty()).then(|| (None, Some(notice(text))))
        }
        _ => None,
    }
}

/// Parse one line of a Twitch `.chat.jsonl` file. `start_ms` is the stream
/// start in unix milliseconds (timestamps become offsets from it).
#[allow(clippy::too_many_arguments)]
pub(super) fn parse_twitch_chat_line(
    line: &str,
    start_ms: f64,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
    twitch_fallback_index: &HashMap<String, std::path::PathBuf>,
    fetch_unknown_emotes: bool,
    fetches: &mut Vec<EmojiFetch>,
    source_partners: &HashMap<String, crate::models::CollabPartner>,
    badge_dirs: &TwitchBadgeDirs,
) -> Option<ChatMessage> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ts_ms = v["ts"].as_f64().unwrap_or(0.0);
    let author = v["name"]
        .as_str()
        .or_else(|| v["login"].as_str())
        .unwrap_or("")
        .to_string();
    // Unwrap `/me` CTCP actions so the emote offsets (which index the inner
    // body) align and the raw control chars don't show in the replay/search.
    let text = strip_ctcp_action(v["text"].as_str().unwrap_or("")).to_string();
    let color_override = v["color"].as_str().and_then(parse_chat_hex_color);
    // Split raw badge tag "subscriber/12,moderator/1" into one entry per badge.
    let badges = v["badges"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_default();
    // Resolved once here (not per render frame) — see `resolve_twitch_badge_icon`'s doc.
    let badge_icons: Vec<Option<std::path::PathBuf>> =
        badges.iter().map(|b| resolve_twitch_badge_icon(b, badge_dirs)).collect();
    // `emotes` tag is absent on pre-feature logs → empty → first-party emotes
    // simply don't render (third-party word-matching still applies).
    let emotes_tag = v["emotes"].as_str().unwrap_or("");
    let segments = build_twitch_segments(
        &text,
        emotes_tag,
        emote_map,
        twitch_dir,
        twitch_fallback_index,
        fetch_unknown_emotes,
        fetches,
    );
    // Split literal unicode emoji out of the text segments into colour images.
    let segments = expand_emoji(segments, fetches);
    // Present only during an active Shared Chat session (Twitch tags every
    // message with it then, including ones typed locally). Resolved against
    // this take's recorded partners — a local message's own room id was
    // never recorded as a "partner" (the monitored channel itself is never
    // in that list, see `CollabPartner`'s doc), so it naturally falls
    // through to no indicator; only messages from an actual OTHER channel
    // resolve to a name.
    let source_room_id = v["source_room_id"].as_str().unwrap_or("");
    let source_name = if source_room_id.is_empty() {
        String::new()
    } else {
        source_partners.get(source_room_id).map(|p| p.name.clone()).unwrap_or_default()
    };
    Some(ChatMessage {
        timestamp_secs: (ts_ms - start_ms) / 1000.0,
        author,
        text,
        segments,
        badges,
        badge_icons,
        color_override,
        platform: ChatPlatform::Twitch,
        login: v["login"].as_str().unwrap_or("").to_lowercase(),
        msg_id: v["id"].as_str().unwrap_or("").to_string(),
        deleted: None,
        system: false,
        reply_to: v["reply"].as_str().unwrap_or("").to_string(),
        source_name,
        user_id: v["user_id"].as_str().unwrap_or("").to_string(),
        badge_info: v["badge_info"].as_str().unwrap_or("").to_string(),
    })
}

impl StreamArchiverApp {
    // ── Chat log viewer ──────────────────────────────────────────────────────

    /// Open the chat popup for a monitor. `rec_id` picks a specific recording
    /// (a take/stream row's "View chat"); `None` falls back to the most recent
    /// recording that has a chat file.
    pub(super) fn open_chat_popup(&mut self, monitor_id: i64, rec_id: Option<i64>, ctx: &egui::Context) {
        let row = self.rows.iter().find(|r| r.monitor.id == monitor_id);
        let monitor_name = row.map(|r| r.channel.name.clone()).unwrap_or_default();
        let platform = row.map(|r| r.monitor.platform());
        // The emote/badge cache is per-ACCOUNT: this monitor's URL names which
        // account's assets to use (a channel can hold a main + alt Twitch).
        let account = row
            .map(|r| asset_account(&r.monitor.url, r.monitor.platform()))
            .unwrap_or_default();
        // Twitch: build the third-party emote map (BTTV/FFZ/7TV) once and point at
        // the first-party emote dir, plus every OTHER cached channel's first-party
        // dir as a fallback (any subscriber can use their sub emotes in any
        // channel's chat — see `twitch_fallback_index`'s doc). YouTube/others:
        // empty map, no dir (emotes come inline in the runs / aren't word-matched).
        let (emote_map, twitch_emote_dir, twitch_fallback_index) = if platform == Some(Platform::Twitch) {
            let dir = twitch_emotes_dir(&monitor_name, &account).join("twitch");
            let fallback_dirs: Vec<_> =
                crate::assets::all_twitch_emote_dirs().into_iter().filter(|d| *d != dir).collect();
            let index = crate::assets::index_emote_stems(&fallback_dirs);
            (Arc::new(build_emote_map(&monitor_name, &account)), Some(dir), Arc::new(index))
        } else {
            (Arc::new(HashMap::new()), None, Arc::new(HashMap::new()))
        };
        let twitch_badge_dirs = Arc::new(if platform == Some(Platform::Twitch) {
            TwitchBadgeDirs {
                channel: Some(twitch_badge_dir(&monitor_name, &account)),
                global: twitch_global_badge_dir(),
            }
        } else {
            TwitchBadgeDirs { channel: None, global: twitch_global_badge_dir() }
        });

        let recs = self
            .core
            .store
            .recordings_for_monitor(monitor_id)
            .unwrap_or_default();
        let rec = rec_id
            .and_then(|id| recs.iter().find(|r| r.id == id))
            .or_else(|| recs.iter().rev().find(|r| chat_file_for_recording(r).is_some()))
            .or_else(|| recs.last())
            .cloned();

        // This take's recorded collab partners, keyed by Twitch broadcaster id
        // — resolves each message's `source_room_id` tag to a name for the
        // "which channel was this from" indicator during a Shared Chat
        // session. Twitch-only; empty when this take has no stream id or no
        // collab was ever recorded for it (messages then render with no
        // indicator, same as a pre-feature log).
        let source_partners: Arc<HashMap<String, crate::models::CollabPartner>> = Arc::new(
            if platform == Some(Platform::Twitch) {
                rec.as_ref()
                    .and_then(|r| r.stream_id.as_deref())
                    .filter(|sid| !sid.is_empty())
                    .and_then(|sid| self.core.store.collab_partners_for_stream(monitor_id, sid).ok())
                    .map(|partners| {
                        partners
                            .into_iter()
                            .filter(|p| !p.id.is_empty())
                            .map(|p| (p.id.clone(), p))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                HashMap::new()
            },
        );

        // This broadcast's top-supporters leaderboard + Hype Train summary —
        // local DB query, no network. Twitch-only (subgift/bits/hype_train
        // are all Twitch chat-event kinds).
        let (top_gifters, top_cheerers, hype_train) = if platform == Some(Platform::Twitch) {
            rec.as_ref()
                .map(|r| {
                    let since = r.went_live_at.unwrap_or(r.started_at);
                    let until = r.ended_at.unwrap_or_else(crate::models::now_unix);
                    load_broadcast_stats(&self.core.store, monitor_id, since, until)
                })
                .unwrap_or_default()
        } else {
            Default::default()
        };

        let (fetch_unknown_emotes, render_emotes) = {
            let cs = self.chat_settings.lock().unwrap();
            (cs.fetch_unknown_emotes, cs.render_emotes)
        };
        let state = Arc::new(Mutex::new(ChatLoadState::Loading));
        let loading = Arc::new(AtomicBool::new(false));
        if let Some(r) = &rec {
            self.core.rt.spawn(load_chat(
                state.clone(),
                loading.clone(),
                chat_file_for_recording(r),
                r.went_live_at.unwrap_or(r.started_at),
                emote_map.clone(),
                twitch_emote_dir.clone(),
                twitch_fallback_index.clone(),
                fetch_unknown_emotes,
                render_emotes,
                source_partners.clone(),
                twitch_badge_dirs.clone(),
                ctx.clone(),
            ));
        } else {
            *state.lock().unwrap() = ChatLoadState::NoFile;
        }
        let popup = ChatPopup {
            monitor_id,
            monitor_name,
            is_twitch: platform == Some(Platform::Twitch),
            recording: rec,
            all_recordings: recs,
            load_state: state,
            search: String::new(),
            full_view: false,
            hide_shared: false,
            highlight_login: None,
            show_appearance: false,
            ts_color_hex: String::new(),
            text_color_hex: String::new(),
            user_card: None,
            users_panel: None,
            top_gifters,
            top_cheerers,
            hype_train,
            last_reload: std::time::Instant::now(),
            emote_map,
            twitch_emote_dir,
            twitch_fallback_index,
            twitch_badge_dirs,
            source_partners,
            fetch_unknown_emotes,
            loading,
            error_retries: 0,
            filter_cache: None,
            settings: self.chat_settings.clone(),
            closed: false,
            decode_misses: Vec::new(),
            usercard_click: None,
        };
        // One chat window per monitor: re-targeting an already-open window
        // (e.g. "View chat" on another take) replaces its content in place;
        // a different monitor gets its own window.
        match self.chat_popups.iter_mut().find(|p| p.lock().unwrap().monitor_id == monitor_id) {
            Some(slot) => *slot.lock().unwrap() = popup,
            None => self.chat_popups.push(Arc::new(Mutex::new(popup))),
        }
    }

    #[allow(deprecated)]
    /// Render every open chat window (one OS viewport per monitor).
    pub(super) fn chat_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for idx in 0..self.chat_popups.len() {
            if self.chat_popup_window(ctx, idx) {
                closed.push(self.chat_popups[idx].lock().unwrap().monitor_id);
            }
        }
        if !closed.is_empty() {
            self.chat_popups.retain(|p| !closed.contains(&p.lock().unwrap().monitor_id));
            if self.chat_popups.is_empty() {
                // Free all decoded emote frame textures once the last chat
                // window is gone.
                self.clear_emote_cache();
            }
        }
    }

    /// Render one chat window; returns true when the user closed it.
    #[allow(deprecated)]
    pub(super) fn chat_popup_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        const CHAT_RELOAD_SECS: u64 = 3;
        let popup_arc = self.chat_popups[idx].clone();
        let mut popup = popup_arc.lock().unwrap();
        // Watchdog: name this phase so a freeze dialog points at the chat popup.
        self.heartbeat.set_context(format!("Chat: {}", popup.monitor_name));
        self.heartbeat.set_activity(crate::watchdog::Activity::Chat);
        let title = format!("💬  Chat — {}", popup.monitor_name);
        let vp_id = egui::ViewportId::from_hash_of(("chat_popup_vp", popup.monitor_id));

        // Whether the selected recording is still in progress (chat file is growing).
        let rec_active = popup.recording.as_ref().map_or(false, |r| r.ended_at.is_none());
        // An errored load retries with a FULL sidecar re-read — back that off
        // exponentially (3s → 6 → … → capped ~3min) instead of hammering the
        // recordings drive every tick. Loaded resets the ladder; NoFile stays
        // on the fast tick (retrying a missing file is one cheap stat, and the
        // sidecar usually appears seconds into a recording).
        let errored = matches!(&*popup.load_state.lock().unwrap(), ChatLoadState::Error(_));
        if !errored && matches!(&*popup.load_state.lock().unwrap(), ChatLoadState::Loaded(_)) {
            popup.error_retries = 0;
        }
        let reload_after = if errored {
            std::time::Duration::from_secs((CHAT_RELOAD_SECS << popup.error_retries.min(6)).min(180))
        } else {
            std::time::Duration::from_secs(CHAT_RELOAD_SECS)
        };
        // Collect everything needed for a tail-reload before the `show` closure
        // borrows `popup` so we can act on it cleanly afterwards.
        type ReloadInfo = (
            std::path::PathBuf,
            i64,
            Arc<Mutex<ChatLoadState>>,
            Arc<HashMap<String, std::path::PathBuf>>,
            Option<std::path::PathBuf>,
            Arc<HashMap<String, std::path::PathBuf>>,
            bool,
            Arc<AtomicBool>,
            Arc<HashMap<String, crate::models::CollabPartner>>,
            Arc<TwitchBadgeDirs>,
        );
        let reload_info: Option<ReloadInfo> =
            if rec_active && popup.last_reload.elapsed() >= reload_after {
                // Sidecar located via the probe cache: this runs on the UI
                // thread every 3s per live popup, and a direct stat against
                // the recordings drive can block the frame for seconds.
                let mut fs_guard = self.fs_probes.lock().unwrap();
                let fs = &mut *fs_guard;
                popup.recording.as_ref().and_then(|r| {
                    chat_file_for_recording_cached(fs, r).map(|path| {
                        (
                            path,
                            r.went_live_at.unwrap_or(r.started_at),
                            popup.load_state.clone(),
                            popup.emote_map.clone(),
                            popup.twitch_emote_dir.clone(),
                            popup.twitch_fallback_index.clone(),
                            popup.fetch_unknown_emotes,
                            popup.loading.clone(),
                            popup.source_partners.clone(),
                            popup.twitch_badge_dirs.clone(),
                        )
                    })
                })
            } else {
                None
            };

        // The emote cache is shared (Arc<Mutex>), so the closure can use a clone
        // without borrowing `self`. Copy the render toggles out too. `now` is the
        // global animation clock — all instances of an emote animate in lockstep.
        let anim_cache = self.emote_anim.clone();
        let (render_emotes, animate_emotes, appearance) = {
            let cs = self.chat_settings.lock().unwrap();
            (
                cs.render_emotes,
                cs.animate_emotes,
                ChatAppearance {
                    font_pt: cs.font_pt,
                    emote_pt: cs.emote_pt,
                    ts_color: cs.ts_color,
                    text_color: cs.text_color,
                },
            )
        };
        let now = ctx.input(|i| i.time);

        // Release the lock before registering the deferred closure — it
        // takes its own lock on the SAME Arc each time it repaints, which
        // would deadlock against this one if it were still held.
        drop(popup);
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            vp_id,
            egui::ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size([480.0, 600.0]),
            popup_arc.clone(),
            shared,
            move |ctx, popup, shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    popup.closed = true;
                }
                // Consumed at the end of this closure into `popup.decode_misses`/
                // `popup.usercard_click` — the deferred closure doesn't run
                // synchronously with the wrapper call, so these can't be plain
                // captured locals the wrapper reads back after the call returns.
                let mut decode_misses: Vec<std::path::PathBuf> = Vec::new();
                let mut usercard_click: Option<UserCardClick> = None;
                egui::CentralPanel::default().show(ctx, |ui| {
                    // ── Toolbar ──────────────────────────────────────────────
                    ui.horizontal(|ui| {
                        // Recording picker: only if >1 recording has a chat file.
                        // Probe-cache lookups: this filter re-runs EVERY FRAME
                        // over the monitor's whole take history (4 candidate
                        // paths each) — direct stats here were measured in the
                        // thousands per second against the recordings drive.
                        let recs_with_chat: Vec<_> = {
                            let mut fs_guard = shared.fs_probes.lock().unwrap();
                            popup
                                .all_recordings
                                .iter()
                                .filter(|r| chat_file_for_recording_cached(&mut fs_guard, r).is_some())
                                .collect()
                            // `fs_guard` dropped here — the rest of this closure
                            // (recording-switch handler, etc.) may take its own
                            // `self.fs_probes` lock elsewhere; a `std::sync::Mutex`
                            // is not reentrant.
                        };
                        if recs_with_chat.len() > 1 {
                            let cur_label = popup
                                .recording
                                .as_ref()
                                .map(fmt_recording_label)
                                .unwrap_or_default();
                            egui::ComboBox::from_id_salt("chat_rec_pick")
                                .selected_text(cur_label)
                                .show_ui(ui, |ui| {
                                    for rec in &recs_with_chat {
                                        let label = fmt_recording_label(rec);
                                        let selected = popup
                                            .recording
                                            .as_ref()
                                            .map(|r| r.id == rec.id)
                                            .unwrap_or(false);
                                        if ui.selectable_label(selected, &label).clicked() {
                                            let new_rec = (*rec).clone();
                                            let state = Arc::new(Mutex::new(ChatLoadState::Loading));
                                            let path = chat_file_for_recording(&new_rec);
                                            let start_ts =
                                                new_rec.went_live_at.unwrap_or(new_rec.started_at);
                                            let emap = popup.emote_map.clone();
                                            let tdir = popup.twitch_emote_dir.clone();
                                            let tfallback = popup.twitch_fallback_index.clone();
                                            let bdirs = popup.twitch_badge_dirs.clone();
                                            let funknown = popup.fetch_unknown_emotes;
                                            // A different recording is a
                                            // different broadcast — its
                                            // Shared Chat partners (if any)
                                            // aren't the same set.
                                            let source_partners: Arc<HashMap<String, crate::models::CollabPartner>> =
                                                Arc::new(
                                                    new_rec
                                                        .stream_id
                                                        .as_deref()
                                                        .filter(|sid| !sid.is_empty())
                                                        .and_then(|sid| {
                                                            shared
                                                                .core
                                                                .store
                                                                .collab_partners_for_stream(popup.monitor_id, sid)
                                                                .ok()
                                                        })
                                                        .map(|partners| {
                                                            partners
                                                                .into_iter()
                                                                .filter(|p| !p.id.is_empty())
                                                                .map(|p| (p.id.clone(), p))
                                                                .collect()
                                                        })
                                                        .unwrap_or_default(),
                                                );
                                            popup.source_partners = source_partners.clone();
                                            // A different recording is a different
                                            // broadcast — its leaderboard/Hype Train
                                            // history is scoped to its own time span.
                                            let (top_gifters, top_cheerers, hype_train) = if popup.is_twitch {
                                                let since = start_ts;
                                                let until =
                                                    new_rec.ended_at.unwrap_or_else(crate::models::now_unix);
                                                load_broadcast_stats(&shared.core.store, popup.monitor_id, since, until)
                                            } else {
                                                (Vec::new(), Vec::new(), None)
                                            };
                                            popup.top_gifters = top_gifters;
                                            popup.top_cheerers = top_cheerers;
                                            popup.hype_train = hype_train;
                                            popup.load_state = state.clone();
                                            popup.recording = Some(new_rec);
                                            popup.last_reload = std::time::Instant::now();
                                            // Keyed on (query, count) only — a
                                            // different log with the same count
                                            // would reuse stale match indices.
                                            popup.filter_cache = None;
                                            shared.core.rt.spawn(load_chat(
                                                state,
                                                popup.loading.clone(),
                                                path,
                                                start_ts,
                                                emap,
                                                tdir,
                                                tfallback,
                                                funknown,
                                                render_emotes,
                                                source_partners,
                                                bdirs,
                                                ctx.clone(),
                                            ));
                                        }
                                    }
                                });
                            ui.separator();
                        }

                        // Search filter
                        ui.label("🔍");
                        ui.add(
                            egui::TextEdit::singleline(&mut popup.search)
                                .hint_text("Filter…")
                                .desired_width(150.0),
                        );
                        if !popup.search.is_empty() && ui.small_button("✕").clicked() {
                            popup.search.clear();
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.toggle_value(&mut popup.full_view, "View full");
                            if ui
                                .button("⚙")
                                .on_hover_text("Chat appearance: font size and colors")
                                .clicked()
                            {
                                popup.show_appearance = !popup.show_appearance;
                                if popup.show_appearance {
                                    let cs = popup.settings.lock().unwrap();
                                    let (ts, tx) = (cs.ts_color, cs.text_color);
                                    drop(cs);
                                    popup.ts_color_hex = hex_color_string(ts);
                                    popup.text_color_hex = hex_color_string(tx);
                                }
                            }
                            if ui
                                .button("👥")
                                .on_hover_text("Users in chat (from this log)")
                                .clicked()
                            {
                                popup.users_panel = if popup.users_panel.is_some() {
                                    None
                                } else {
                                    Some(UsersPanelState {
                                        filter: String::new(),
                                        entries: Vec::new(),
                                        built_at_count: 0,
                                    })
                                };
                            }
                            ui.checkbox(&mut popup.hide_shared, "Hide shared")
                                .on_hover_text(
                                    "During an active Shared Chat session, hide messages that \
                                     came from another channel — show only this channel's own \
                                     messages. Useful when a merged chat is too noisy to follow.",
                                );
                        });
                    });
                    ui.separator();

                    if popup.show_appearance {
                        let (mut font_pt, mut emote_pt, mut ts_color, mut text_color) = {
                            let cs = popup.settings.lock().unwrap();
                            (cs.font_pt, cs.emote_pt, cs.ts_color, cs.text_color)
                        };
                        egui::Window::new("Chat Appearance")
                            .id(egui::Id::new(("chat_appearance_win", popup.monitor_id)))
                            .collapsible(false)
                            .resizable(false)
                            .default_pos(egui::pos2(120.0, 60.0))
                            .show(ctx, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Font size:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut font_pt)
                                                .range(8.0..=32.0)
                                                .suffix(" pt"),
                                        )
                                        .on_hover_text(
                                            "Exact point size for the timestamp, message text, \
                                             and username — applies to every open chat window.",
                                        )
                                        .changed()
                                    {
                                        popup.settings.lock().unwrap().font_pt = font_pt;
                                        let _ = shared.core.store.set_setting(
                                            K_CHAT_FONT_PT,
                                            &font_pt.to_string(),
                                        );
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Emote size:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut emote_pt)
                                                .range(12.0..=64.0)
                                                .suffix(" px"),
                                        )
                                        .on_hover_text(
                                            "Pixel size for emotes and emoji in the chat replay \
                                             — independent of the text font size, applies to \
                                             every open chat window.",
                                        )
                                        .changed()
                                    {
                                        popup.settings.lock().unwrap().emote_pt = emote_pt;
                                        let _ = shared.core.store.set_setting(
                                            K_CHAT_EMOTE_PT,
                                            &emote_pt.to_string(),
                                        );
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Timestamp color:");
                                    let wheel_changed = egui::color_picker::color_edit_button_srgba(
                                        ui,
                                        &mut ts_color,
                                        egui::color_picker::Alpha::Opaque,
                                    )
                                    .on_hover_text("Color of the [hh:mm:ss] timestamp prefix.")
                                    .changed();
                                    if wheel_changed {
                                        popup.ts_color_hex = hex_color_string(ts_color);
                                    }
                                    // Egui's color-wheel popup only offers a "copy"
                                    // button (RGB numbers, not hex) with no matching
                                    // paste target — this hex field is that missing
                                    // paste target: type or paste a `#RRGGBB` value
                                    // directly, applied as soon as it parses.
                                    let hex_changed = ui
                                        .add(
                                            egui::TextEdit::singleline(&mut popup.ts_color_hex)
                                                .desired_width(64.0)
                                                .hint_text("#RRGGBB"),
                                        )
                                        .on_hover_text(
                                            "Type or paste a hex color (e.g. #FFFFFF) — applies \
                                             as soon as it's a valid 6-digit hex value.",
                                        )
                                        .changed();
                                    if hex_changed
                                        && let Some(parsed) = parse_chat_hex_color(popup.ts_color_hex.trim())
                                    {
                                        ts_color = parsed;
                                    }
                                    let ts_color_was = popup.settings.lock().unwrap().ts_color;
                                    if wheel_changed || (hex_changed && ts_color != ts_color_was) {
                                        popup.settings.lock().unwrap().ts_color = ts_color;
                                        let _ = shared.core.store.set_setting(
                                            K_CHAT_TS_COLOR,
                                            &hex_color_string(ts_color),
                                        );
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Message color:");
                                    let wheel_changed = egui::color_picker::color_edit_button_srgba(
                                        ui,
                                        &mut text_color,
                                        egui::color_picker::Alpha::Opaque,
                                    )
                                    .on_hover_text("Color of the message body text.")
                                    .changed();
                                    if wheel_changed {
                                        popup.text_color_hex = hex_color_string(text_color);
                                    }
                                    let hex_changed = ui
                                        .add(
                                            egui::TextEdit::singleline(&mut popup.text_color_hex)
                                                .desired_width(64.0)
                                                .hint_text("#RRGGBB"),
                                        )
                                        .on_hover_text(
                                            "Type or paste a hex color (e.g. #FFFFFF) — applies \
                                             as soon as it's a valid 6-digit hex value.",
                                        )
                                        .changed();
                                    if hex_changed
                                        && let Some(parsed) = parse_chat_hex_color(popup.text_color_hex.trim())
                                    {
                                        text_color = parsed;
                                    }
                                    let text_color_was = popup.settings.lock().unwrap().text_color;
                                    if wheel_changed || (hex_changed && text_color != text_color_was) {
                                        popup.settings.lock().unwrap().text_color = text_color;
                                        let _ = shared.core.store.set_setting(
                                            K_CHAT_TEXT_COLOR,
                                            &hex_color_string(text_color),
                                        );
                                    }
                                });
                                ui.add_space(4.0);
                                if ui
                                    .button("Reset to defaults")
                                    .on_hover_text("Restore the default 14pt / 24px white/white appearance.")
                                    .clicked()
                                {
                                    {
                                        let mut cs = popup.settings.lock().unwrap();
                                        cs.font_pt = CHAT_FONT_PT_DEFAULT;
                                        cs.emote_pt = CHAT_EMOTE_PT_DEFAULT;
                                        cs.ts_color = egui::Color32::WHITE;
                                        cs.text_color = egui::Color32::WHITE;
                                    }
                                    popup.ts_color_hex = hex_color_string(egui::Color32::WHITE);
                                    popup.text_color_hex = hex_color_string(egui::Color32::WHITE);
                                    let _ = shared.core.store.set_setting(
                                        K_CHAT_FONT_PT,
                                        &CHAT_FONT_PT_DEFAULT.to_string(),
                                    );
                                    let _ = shared.core.store.set_setting(
                                        K_CHAT_EMOTE_PT,
                                        &CHAT_EMOTE_PT_DEFAULT.to_string(),
                                    );
                                    let _ = shared.core.store.set_setting(K_CHAT_TS_COLOR, "#FFFFFF");
                                    let _ = shared.core.store.set_setting(K_CHAT_TEXT_COLOR, "#FFFFFF");
                                }
                            });
                    }

                    // ── Usercard ─────────────────────────────────────────────
                    if let Some(card) = &popup.user_card {
                        let mut open = true;
                        egui::Window::new(format!("👤 {}", card.display_name))
                            .id(egui::Id::new(("chat_usercard_win", popup.monitor_id)))
                            .collapsible(false)
                            .resizable(true)
                            .default_width(340.0)
                            .open(&mut open)
                            .show(ctx, |ui| {
                                let banner_color = card
                                    .color
                                    .unwrap_or_else(|| twitch_username_color(&card.display_name));
                                paint_user_banner(ui, banner_color, 40.0);
                                ui.horizontal(|ui| {
                                    // Avatar (live lookup) — reuses the same
                                    // decode/GPU-upload cache as emotes/badges.
                                    let avatar_drawn = if let UserCardFetch::Loaded {
                                        avatar_path: Some(p), ..
                                    } = &*card.fetch.lock().unwrap()
                                    {
                                        draw_cached_emote(
                                            ui,
                                            &anim_cache,
                                            p,
                                            false,
                                            64.0,
                                            now,
                                            &mut decode_misses,
                                            ctx,
                                        )
                                        .is_some()
                                    } else {
                                        false
                                    };
                                    if !avatar_drawn {
                                        ui.allocate_ui(egui::vec2(64.0, 64.0), |ui| {
                                            ui.centered_and_justified(|ui| ui.weak("👤"));
                                        });
                                    }
                                    ui.vertical(|ui| {
                                        let base = card.color.unwrap_or_else(|| {
                                            twitch_username_color(&card.display_name)
                                        });
                                        let color = readable_color(base, ui.visuals().panel_fill);
                                        ui.label(
                                            egui::RichText::new(&card.display_name)
                                                .strong()
                                                .size(16.0)
                                                .color(color),
                                        );
                                        ui.horizontal(|ui| {
                                            for (i, badge) in card.badges.iter().enumerate() {
                                                let icon =
                                                    card.badge_icons.get(i).and_then(|o| o.as_ref());
                                                let drawn = icon.and_then(|path| {
                                                    draw_cached_emote(
                                                        ui,
                                                        &anim_cache,
                                                        path,
                                                        false,
                                                        18.0,
                                                        now,
                                                        &mut decode_misses,
                                                        ctx,
                                                    )
                                                });
                                                if let Some((resp, _)) = drawn {
                                                    resp.on_hover_text(badge_label(badge));
                                                } else {
                                                    let (sym, c) =
                                                        badge_display(badge, &ChatPlatform::Twitch);
                                                    ui.label(egui::RichText::new(sym).color(c))
                                                        .on_hover_text(badge_label(badge));
                                                }
                                            }
                                        });
                                    });
                                });
                                ui.separator();
                                egui::Grid::new("usercard_grid").num_columns(2).show(ui, |ui| {
                                    if let Some((set_id, months)) = card
                                        .badge_info
                                        .split_once('/')
                                        .filter(|(s, _)| *s == "subscriber")
                                    {
                                        let _ = set_id;
                                        let tier = card
                                            .badges
                                            .iter()
                                            .find(|b| b.starts_with("subscriber/"))
                                            .and_then(|b| b.split('/').nth(1))
                                            .and_then(|v| v.parse::<i64>().ok())
                                            .map(|v| if v >= 3000 { 3 } else if v >= 2000 { 2 } else { 1 })
                                            .unwrap_or(1);
                                        ui.label("Subscriber:");
                                        ui.label(format!("Tier {tier} · {months} month(s)"));
                                        ui.end_row();
                                    }
                                    ui.label("Messages in this log:");
                                    ui.label(card.message_count.to_string());
                                    ui.end_row();
                                    if !card.user_id.is_empty() {
                                        ui.label("User ID:");
                                        ui.label(&card.user_id);
                                        ui.end_row();
                                    }
                                    if let Some(secs) = card.first_seen_secs {
                                        ui.label("First seen:");
                                        ui.label(fmt_chat_ts(secs));
                                        ui.end_row();
                                    }
                                    ui.label("Account created:");
                                    let created = match &*card.fetch.lock().unwrap() {
                                        UserCardFetch::Loaded { created_at: Some(c), .. } => {
                                            chrono::DateTime::parse_from_rfc3339(c)
                                                .map(|dt| dt.format("%Y-%m-%d").to_string())
                                                .unwrap_or_else(|_| c.clone())
                                        }
                                        UserCardFetch::Loading => "…".to_string(),
                                        _ => "N/A".to_string(),
                                    };
                                    ui.label(created);
                                    ui.end_row();
                                });

                                // Cross-referenced against this channel's locally-recorded
                                // event history (bits/gifts/raids/timeouts) — see
                                // `summarize_user_events`'s doc. Local-only, no network.
                                if !card.channel_stats.is_empty() {
                                    ui.separator();
                                    ui.label(egui::RichText::new("This channel:").weak());
                                    for line in &card.channel_stats {
                                        ui.label(line);
                                    }
                                }

                                // A local "recent activity" feed — this user's own messages
                                // from the currently-loaded log, newest at the bottom.
                                if !card.recent_messages.is_empty() {
                                    ui.separator();
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Recent messages in this log ({}):",
                                            card.recent_messages.len()
                                        ))
                                        .weak(),
                                    );
                                    egui::ScrollArea::vertical()
                                        .id_salt("usercard_recent_messages")
                                        .max_height(150.0)
                                        .auto_shrink([false, true])
                                        .stick_to_bottom(true)
                                        .show(ui, |ui| {
                                            for (ts, text) in &card.recent_messages {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 3.0;
                                                    ui.label(
                                                        egui::RichText::new(fmt_chat_ts(*ts))
                                                            .monospace()
                                                            .small()
                                                            .weak(),
                                                    );
                                                    ui.label(egui::RichText::new(text).small());
                                                });
                                            }
                                        });
                                }

                                ui.separator();
                                ui.horizontal(|ui| {
                                    let highlighted =
                                        popup.highlight_login.as_deref() == Some(card.login.as_str());
                                    if ui
                                        .selectable_label(highlighted, "🔔")
                                        .on_hover_text("Highlight messages of this user")
                                        .clicked()
                                    {
                                        popup.highlight_login =
                                            if highlighted { None } else { Some(card.login.clone()) };
                                    }
                                    if ui
                                        .button("Copy username")
                                        .on_hover_text("Copy this user's login to the clipboard")
                                        .clicked()
                                    {
                                        ctx.copy_text(card.login.clone());
                                    }
                                    if ui
                                        .button("Open Twitch profile")
                                        .on_hover_text("Open twitch.tv/{login} in your browser")
                                        .clicked()
                                    {
                                        crate::platform::open_url(&format!(
                                            "https://twitch.tv/{}",
                                            card.login
                                        ));
                                    }
                                });
                            });
                        if !open {
                            popup.user_card = None;
                        }
                    }

                    // ── Users in chat ────────────────────────────────────────
                    if let Some(panel) = &mut popup.users_panel {
                        // Rebuild whenever the log has grown since the last
                        // build (a live tail-reload appended new messages) —
                        // cheap staleness check, not a per-frame rescan.
                        let count = match &*popup.load_state.lock().unwrap() {
                            ChatLoadState::Loaded(log) => log.messages.len(),
                            _ => 0,
                        };
                        if count != panel.built_at_count {
                            panel.entries = match &*popup.load_state.lock().unwrap() {
                                ChatLoadState::Loaded(log) => build_users_panel(log),
                                _ => Vec::new(),
                            };
                            panel.built_at_count = count;
                        }
                        let mut open = true;
                        let mut clicked: Option<UserCardClick> = None;
                        egui::Window::new("👥 Users in chat")
                            .id(egui::Id::new(("chat_users_panel_win", popup.monitor_id)))
                            .collapsible(false)
                            .resizable(true)
                            .default_width(220.0)
                            .default_height(400.0)
                            .open(&mut open)
                            .show(ctx, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut panel.filter)
                                        .hint_text("Filter…")
                                        .desired_width(f32::INFINITY),
                                )
                                .on_hover_text("Narrow the list by username (case-insensitive).");
                                ui.separator();
                                let q = panel.filter.to_lowercase();
                                egui::ScrollArea::vertical().auto_shrink([false, false]).show(
                                    ui,
                                    |ui| {
                                        let mut last_role: Option<&str> = None;
                                        for entry in panel.entries.iter().filter(|e| {
                                            q.is_empty() || e.click.display_name.to_lowercase().contains(&q)
                                        }) {
                                            if last_role != Some(entry.role) {
                                                ui.add_space(if last_role.is_some() { 6.0 } else { 0.0 });
                                                ui.label(egui::RichText::new(entry.role).weak().strong());
                                                last_role = Some(entry.role);
                                            }
                                            // Same contrast adjustment as the chat rows
                                            // themselves (`chat_username_color`) — an
                                            // unadjusted dark USERCOLOR (navy, dark green,
                                            // etc.) is hard to read on this panel's dark
                                            // background otherwise.
                                            let base = entry
                                                .click
                                                .color
                                                .unwrap_or_else(|| twitch_username_color(&entry.click.display_name));
                                            let color = readable_color(base, ui.visuals().panel_fill);
                                            if ui
                                                .add(
                                                    egui::Label::new(
                                                        egui::RichText::new(&entry.click.display_name)
                                                            .color(color),
                                                    )
                                                    .sense(egui::Sense::click()),
                                                )
                                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                .on_hover_text("Click for user info")
                                                .clicked()
                                            {
                                                clicked = Some(entry.click.clone());
                                            }
                                        }
                                    },
                                );
                            });
                        if let Some(c) = clicked {
                            usercard_click = Some(c);
                        }
                        if !open {
                            popup.users_panel = None;
                        }
                    }

                    // ── Leaderboard / Hype Train ────────────────────────────
                    // Matches Twitch's own layout: a top-supporters strip and
                    // an ongoing/reached Hype Train indicator sit above the
                    // message list. Built entirely from `stream_event` (see
                    // `load_broadcast_stats`'s doc) — no live carousel/train
                    // capture exists, so this is a local reconstruction: the
                    // leaderboard won't match Twitch's exact carousel (no
                    // follow/viewer-count data available to us), and the
                    // Hype Train bar reflects this app's own periodic
                    // (~60s) Twitch poll, not a smooth animated countdown.
                    if !popup.top_gifters.is_empty() || !popup.top_cheerers.is_empty() || popup.hype_train.is_some()
                    {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            if !popup.top_gifters.is_empty() || !popup.top_cheerers.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    ui.label(egui::RichText::new("Top supporters:").weak());
                                    for (name, n) in &popup.top_gifters {
                                        ui.label(format!("🎁 {name} ×{n}"))
                                            .on_hover_text("Gift subs given this broadcast");
                                    }
                                    for (name, n) in &popup.top_cheerers {
                                        ui.label(format!("💎 {name} ×{n}"))
                                            .on_hover_text("Bits cheered this broadcast");
                                    }
                                });
                            }
                            if let Some(train) = &popup.hype_train {
                                if !popup.top_gifters.is_empty() || !popup.top_cheerers.is_empty() {
                                    ui.separator();
                                }
                                // `goal`/`expires_at` are only populated (v86+) for a
                                // GQL-confirmed train; `now < expires_at` is this app's
                                // best-effort "is it still running" signal (Twitch
                                // gives no explicit end event) — everything else
                                // (pre-v86 rows, inference-only rows GQL never
                                // confirmed, or a train whose timer has lapsed) falls
                                // back to the plain reached-level summary line.
                                let now = crate::models::now_unix();
                                let live = train.goal > 0 && train.expires_at > now;
                                if live {
                                    let frac = (train.total as f32 / train.goal as f32).clamp(0.0, 1.0);
                                    let remaining = (train.expires_at - now).max(0);
                                    ui.horizontal(|ui| {
                                        ui.label("🚂");
                                        ui.add(
                                            egui::ProgressBar::new(frac)
                                                .text(format!(
                                                    "Hype Train · Lvl {} · {}/{} · {}:{:02}",
                                                    train.level.max(1),
                                                    crate::models::group_thousands(train.total),
                                                    crate::models::group_thousands(train.goal),
                                                    remaining / 60,
                                                    remaining % 60,
                                                ))
                                                .fill(egui::Color32::from_rgb(0x2e, 0xa0, 0x43))
                                                .desired_width(ui.available_width() - 24.0),
                                        )
                                        .on_hover_text(
                                            "Reconstructed from this app's periodic (~60s) \
                                             anonymous Twitch poll — not a live push update, \
                                             so it can lag a few seconds behind Twitch's own bar.",
                                        );
                                    });
                                } else {
                                    ui.horizontal(|ui| {
                                        ui.label("🚂");
                                        ui.label(&train.detail).on_hover_text(
                                            "This broadcast's most recent Hype Train — the last \
                                             confirmed poll before it ended (or a chat-inferred \
                                             estimate if Twitch's GQL never confirmed it).",
                                        );
                                    });
                                }
                            }
                        });
                        ui.add_space(2.0);
                    }

                    // ── Content ──────────────────────────────────────────────
                    // Render straight from the mutex guard — the old code
                    // cloned the entire parsed log (every message + segments)
                    // every single frame.
                    let mut guard = popup.load_state.lock().unwrap();
                    match &mut *guard {
                        ChatLoadState::Loading => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Loading chat…");
                            });
                            ctx.request_repaint();
                        }
                        ChatLoadState::NoFile => {
                            ui.add_space(8.0);
                            ui.label("No chat file found for this recording.");
                            ui.weak("Chat logging must be enabled and a recording must exist.");
                        }
                        ChatLoadState::Error(e) => {
                            ui.colored_label(egui::Color32::RED, format!("Failed to load: {e}"));
                        }
                        ChatLoadState::Loaded(log) => {
                            // Keep the height cache aligned with the message
                            // list: tail appends get estimates at the end; a
                            // shrink (recording switch) resets everything.
                            let n = log.messages.len();
                            if log.row_heights.len() > n {
                                log.row_heights.clear();
                            }
                            log.row_heights.resize(n, CHAT_ROW_EST);

                            // Search filter + "Hide shared" filter, recomputed only
                            // when the query, message count, or hide_shared changes
                            // — not every frame.
                            let q = popup.search.to_lowercase();
                            let hide_shared = popup.hide_shared;
                            if q.is_empty() && !hide_shared {
                                popup.filter_cache = None;
                            } else {
                                let stale = popup
                                    .filter_cache
                                    .as_ref()
                                    .is_none_or(|(cq, cn, ch, _)| *cq != q || *cn != n || *ch != hide_shared);
                                if stale {
                                    let idx: Vec<u32> = log
                                        .messages
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, m)| {
                                            (q.is_empty()
                                                || m.text.to_lowercase().contains(&q)
                                                || m.author.to_lowercase().contains(&q))
                                                && (!hide_shared || m.source_name.is_empty())
                                        })
                                        .map(|(i, _)| i as u32)
                                        .collect();
                                    popup.filter_cache = Some((q.clone(), n, hide_shared, idx));
                                }
                            }
                            let filtered: Option<&[u32]> =
                                popup.filter_cache.as_ref().map(|(_, _, _, v)| v.as_slice());
                            let count = filtered.map_or(n, |v| v.len());

                            ui.horizontal(|ui| {
                                ui.weak(format!("{count} messages"));
                                if log.loading_older {
                                    ui.spinner();
                                    ui.weak("loading older messages…");
                                }
                            });

                            let stick = q.is_empty() && !popup.full_view;
                            const GAP: f32 = 2.0;
                            const OVERSCAN: f32 = 300.0;
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .stick_to_bottom(stick)
                                .show_viewport(ui, |ui, viewport| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    // Wrapping depends on width — a resize
                                    // re-measures everything.
                                    let w = ui.available_width();
                                    if (w - log.measured_width).abs() > 0.5 {
                                        log.measured_width = w;
                                        for h in &mut log.row_heights {
                                            *h = CHAT_ROW_EST;
                                        }
                                    }
                                    // One cheap pass over the cached heights
                                    // finds the on-screen window; only rows
                                    // within the viewport (± overscan) are
                                    // laid out — everything else is two
                                    // spacers, so a 6-hour log renders a few
                                    // dozen rows per frame, not all of them.
                                    // f64 accumulation: an f32 running sum
                                    // drifts past ~2M px (100k+ rows), which
                                    // desyncs offsets from rendered heights
                                    // and can retrigger repaints forever.
                                    let top = f64::from(viewport.min.y - OVERSCAN);
                                    let bottom = f64::from(viewport.max.y + OVERSCAN);
                                    let mut y = 0.0f64;
                                    let mut first = count;
                                    let mut offset = 0.0f64;
                                    let mut last = count;
                                    let mut last_y = 0.0f64;
                                    for di in 0..count {
                                        let mi = filtered.map_or(di, |v| v[di] as usize);
                                        let h = f64::from(log.row_heights[mi] + GAP);
                                        if first == count && y + h > top {
                                            first = di;
                                            offset = y;
                                        }
                                        if last == count && y > bottom {
                                            last = di;
                                            last_y = y;
                                        }
                                        y += h;
                                    }
                                    if last == count {
                                        last_y = y;
                                    }
                                    let total = y;
                                    ui.add_space(offset as f32);
                                    let mut mismeasured = false;
                                    // A translucent tint of the theme's own selection color for
                                    // the highlighted user's rows (🔔 "Highlight messages of
                                    // this user" in the usercard) — reuses the same visual
                                    // language egui already uses for "this is selected/marked",
                                    // just softened so message text stays legible on top.
                                    let sel = ui.visuals().selection.bg_fill;
                                    let highlight_bg =
                                        egui::Color32::from_rgba_unmultiplied(sel.r(), sel.g(), sel.b(), 40);
                                    for di in first..last {
                                        let mi = filtered.map_or(di, |v| v[di] as usize);
                                        let highlighted = popup
                                            .highlight_login
                                            .as_deref()
                                            .is_some_and(|hl| {
                                                let login = &log.messages[mi].login;
                                                !login.is_empty() && login == hl
                                            });
                                        let r = egui::Frame::new()
                                            .fill(if highlighted {
                                                highlight_bg
                                            } else {
                                                egui::Color32::TRANSPARENT
                                            })
                                            .inner_margin(egui::Margin::symmetric(2, 1))
                                            .corner_radius(2.0)
                                            .show(ui, |ui| {
                                                ui.scope(|ui| {
                                                    render_chat_message(
                                                        ui,
                                                        &log.messages[mi],
                                                        &anim_cache,
                                                        render_emotes,
                                                        animate_emotes,
                                                        now,
                                                        &mut decode_misses,
                                                        ctx,
                                                        &appearance,
                                                    )
                                                })
                                            });
                                        if let Some(req) = r.inner.inner {
                                            usercard_click = Some(req);
                                        }
                                        let h = r.response.rect.height();
                                        if (h - log.row_heights[mi]).abs() > 0.5 {
                                            log.row_heights[mi] = h;
                                            mismeasured = true;
                                        }
                                        ui.add_space(GAP);
                                    }
                                    // Reserve the space of everything below
                                    // the rendered window so the scrollbar
                                    // spans the whole log.
                                    if total > last_y {
                                        ui.add_space((total - last_y) as f32);
                                    }
                                    if mismeasured {
                                        // Offsets were computed from estimates
                                        // — redo with real heights next frame.
                                        ctx.request_repaint();
                                    }
                                });
                        }
                    }
                });
                draw_alt_image_preview(ctx);
                popup.decode_misses.extend(decode_misses);
                if usercard_click.is_some() {
                    popup.usercard_click = usercard_click;
                }
            },
        );
        // Decode any newly-seen emotes off the UI thread, then LRU-evict the cache.
        let decode_misses = std::mem::take(&mut popup_arc.lock().unwrap().decode_misses);
        self.pump_emote_decodes(decode_misses, now, ctx);

        // A username was clicked this frame: build the usercard. Local fields
        // (badges/color/sub-months) come straight from the click; session
        // stats are a fresh scan of the currently-loaded log (cheap — chat
        // logs are at most tens of thousands of messages, and this only runs
        // on a click, not per frame).
        let usercard_click = popup_arc.lock().unwrap().usercard_click.take();
        if let Some(req) = usercard_click {
            const RECENT_MESSAGES_CAP: usize = 50;
            let (message_count, first_seen_secs, recent_messages) = {
                let load_state = popup_arc.lock().unwrap().load_state.clone();
                let guard = load_state.lock().unwrap();
                if let ChatLoadState::Loaded(log) = &*guard {
                    let mut all: Vec<(f64, String)> = log
                        .messages
                        .iter()
                        .filter(|m| m.login == req.login)
                        .map(|m| (m.timestamp_secs, m.text.clone()))
                        .collect();
                    let count = all.len();
                    let first = all.first().map(|(ts, _)| *ts);
                    if all.len() > RECENT_MESSAGES_CAP {
                        all.drain(0..all.len() - RECENT_MESSAGES_CAP);
                    }
                    (count, first, all)
                } else {
                    (0, None, Vec::new())
                }
            };
            let monitor_id = popup_arc.lock().unwrap().monitor_id;
            // Cross-reference this user's Twitch display name against the
            // channel's locally-recorded `stream_event` history — local DB
            // query, no network, so it's fine to run inline on the click.
            let channel_stats = self
                .core
                .store
                .get_monitor_with_channel(monitor_id)
                .ok()
                .flatten()
                .and_then(|m| {
                    self.core
                        .store
                        .stream_events_range(m.channel.id, 0, crate::models::now_unix())
                        .ok()
                })
                .map(|events| summarize_user_events(&events, &req.display_name))
                .unwrap_or_default();
            let want_live =
                self.chat_settings.lock().unwrap().fetch_usercard_info && !req.user_id.is_empty();
            let fetch = Arc::new(Mutex::new(if want_live {
                UserCardFetch::Loading
            } else {
                UserCardFetch::Disabled
            }));
            if want_live {
                if let Some(dctx) = self.core.detect_ctx() {
                    let fetch2 = fetch.clone();
                    let user_id = req.user_id.clone();
                    let login = req.login.clone();
                    let store = self.core.store.clone();
                    let events = self.core.events.clone();
                    self.core.rt.spawn(async move {
                        let result = async {
                            let (client_id, token) = dctx.twitch_helix_auth().await?;
                            crate::assets::fetch_usercard_info(&client_id, &token, &user_id).await
                        }
                        .await;
                        match result {
                            Ok(info) => {
                                *fetch2.lock().unwrap() = UserCardFetch::Loaded {
                                    avatar_path: info.avatar_path,
                                    created_at: info.created_at,
                                };
                            }
                            Err(e) => {
                                *fetch2.lock().unwrap() = UserCardFetch::Failed;
                                // File a warning through the same path capture-log
                                // alerts use, so a failed live lookup shows up in
                                // the 🚨 Warnings window / 🔔 feed instead of
                                // silently degrading to "N/A" with no trace.
                                let alert = crate::store::NewCaptureAlert {
                                    kind: "usercard_lookup_failed".to_string(),
                                    severity: "warning".to_string(),
                                    source: "chat_usercard".to_string(),
                                    take_key: format!("usercard:{login}"),
                                    monitor_id: Some(monitor_id),
                                    recording_id: None,
                                    video_id: None,
                                    channel: login.clone(),
                                    count: 1,
                                    lost_segments: 0,
                                    last_line: format!(
                                        "Twitch usercard lookup failed for {login}: {e:#}"
                                    ),
                                };
                                if let Ok((id, _)) = store.upsert_capture_alert(&alert) {
                                    let _ = events.send(crate::events::AppEvent::CaptureAlert {
                                        severity: "warning".to_string(),
                                        title: format!("Usercard lookup failed: {login}"),
                                        body: format!("{e:#}"),
                                        monitor_id: Some(monitor_id),
                                        channel: login,
                                        recording_id: None,
                                        ref_key: format!("usercard:{id}"),
                                    });
                                }
                            }
                        }
                    });
                } else {
                    *fetch.lock().unwrap() = UserCardFetch::Failed;
                }
            }
            popup_arc.lock().unwrap().user_card = Some(UserCardPopup {
                login: req.login,
                display_name: req.display_name,
                color: req.color,
                badges: req.badges,
                badge_icons: req.badge_icons,
                badge_info: req.badge_info,
                user_id: req.user_id,
                message_count,
                first_seen_secs,
                recent_messages,
                channel_stats,
                fetch,
            });
        }

        // Tail-reload: while the recording is live, parse only the bytes
        // appended since the last pass and push them onto the shown log —
        // the whole file is never re-read.
        if let Some((path, start_ts, state, emap, tdir, tfallback, funknown, loading, spartners, bdirs)) = reload_info {
            let mut p = popup_arc.lock().unwrap();
            p.last_reload = std::time::Instant::now();
            // Same cadence as the tail-reload: the leaderboard/Hype Train
            // rows keep changing while the broadcast is still live. Cheap
            // indexed local query — naturally empty for a non-Twitch monitor
            // (those event kinds are only ever written by the Twitch chat
            // parser), so no separate platform check is needed here.
            let (top_gifters, top_cheerers, hype_train) = load_broadcast_stats(
                &self.core.store,
                p.monitor_id,
                start_ts,
                crate::models::now_unix(),
            );
            p.top_gifters = top_gifters;
            p.top_cheerers = top_cheerers;
            p.hype_train = hype_train;
            if errored {
                p.error_retries = p.error_retries.saturating_add(1);
            }
            drop(p);
            self.core.rt.spawn(tail_chat(
                state,
                loading,
                path,
                start_ts,
                emap,
                tdir,
                tfallback,
                funknown,
                render_emotes,
                spartners,
                bdirs,
                ctx.clone(),
            ));
        }
        // Keep the UI alive while a live recording is open so the next
        // interval check fires automatically.
        if rec_active {
            ctx.request_repaint_after(std::time::Duration::from_secs(CHAT_RELOAD_SECS));
        }

        popup_arc.lock().unwrap().closed
    }

    /// Drop all decoded emote frames and bump the epoch so any in-flight decode
    /// task skips its insert (poison-safe).
    pub(super) fn clear_emote_cache(&self) {
        self.emote_epoch.fetch_add(1, Ordering::SeqCst);
        self.emote_anim
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Decode newly-seen emotes off the UI thread, then enforce the LRU memory
    /// budget on the (now drawn) cache. Bounds how many decodes start per frame so
    /// opening a view with hundreds of distinct emotes doesn't spawn a blocking-
    /// thread storm; the over-cap ones revert to "unseen" and retry next frame. The
    /// epoch guard drops results whose cache was cleared (view closed / assets
    /// refetched) mid-decode. Shared by the chat replay popup and the emote viewer.
    pub(super) fn pump_emote_decodes(
        &self,
        mut decode_misses: Vec<std::path::PathBuf>,
        now: f64,
        ctx: &egui::Context,
    ) {
        // Watchdog: the decode/upload/evict sweep is the most texture-churning phase.
        self.heartbeat.set_activity(crate::watchdog::Activity::EmoteDecodePump);
        const MAX_DECODE_PER_FRAME: usize = 64;
        if decode_misses.len() > MAX_DECODE_PER_FRAME {
            let mut g = self.emote_anim.lock().unwrap_or_else(|e| e.into_inner());
            for path in &decode_misses[MAX_DECODE_PER_FRAME..] {
                g.remove(path);
            }
            decode_misses.truncate(MAX_DECODE_PER_FRAME);
        }
        let epoch = self.emote_epoch.load(Ordering::SeqCst);
        for path in decode_misses {
            let cache = self.emote_anim.clone();
            let epoch_at = self.emote_epoch.clone();
            let ctx2 = ctx.clone();
            self.core.rt.spawn_blocking(move || {
                let decoded = crate::iomon::fs::read_sync(crate::iomon::Cat::AssetCache, &path).ok().and_then(|b| crate::emote_anim::decode(&b));
                let entry = match decoded {
                    Some((imgs, delays)) => crate::emote_anim::EmoteLoad::Decoded(imgs, delays),
                    None => crate::emote_anim::EmoteLoad::Failed,
                };
                let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
                if epoch_at.load(Ordering::SeqCst) == epoch {
                    g.insert(path, entry);
                    drop(g);
                    ctx2.request_repaint();
                }
            });
        }
        evict_emote_cache(&self.emote_anim, now);
    }
}

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

        let (gifters, cheerers, train) = load_broadcast_stats(&store, mid, 0, 200);
        assert_eq!(gifters, vec![("Alice".to_string(), 5), ("Bob".to_string(), 2)]);
        assert_eq!(cheerers, vec![("Alice".to_string(), 300)]);
        let train = train.expect("latest train present");
        assert_eq!(train.detail, "level 3 · 4,200 pts (confirmed)");
        assert_eq!((train.level, train.total, train.goal, train.expires_at), (3, 4200, 5000, 400));
    }

    fn plain_msg(ts: f64, login: &str, msg_id: &str, text: &str) -> ChatMessage {
        ChatMessage {
            timestamp_secs: ts,
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
            measured_width: 0.0,
            parsed_to: 0,
            loading_older: false,
            markers: Vec::new(),
        };
        // A YouTube message (no login) must never show up in a Twitch-only panel.
        let mut yt = plain_msg(1.0, "", "", "yt viewer");
        yt.platform = ChatPlatform::YouTube;
        log.messages.push(yt);

        let entries = build_users_panel(&log);
        // Deduped: one entry for "bob" (not two), using their later, mod-badged message.
        assert_eq!(entries.len(), 2);
        let bob = entries.iter().find(|e| e.click.login == "bob").expect("bob present");
        assert_eq!(bob.role, "Moderators");
        // Moderators sort before Users (alice has no badges).
        assert_eq!(entries[0].click.login, "bob");
        assert_eq!(entries[1].click.login, "alice");
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
            measured_width: 0.0,
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

    #[test]
    fn old_sidecar_lines_without_id_still_parse() {
        // Pre-v60 lines have no `id`/`login`-marker fields; they must load
        // exactly as before, just without deletion matching.
        let line = r#"{"ts":1700000000000,"login":"bob","name":"Bob","text":"hi"}"#;
        let mut fetches = Vec::new();
        let m = parse_twitch_chat_line(
            line, 1_700_000_000_000.0, &HashMap::new(), None, &HashMap::new(), false, &mut fetches,
            &HashMap::new(), &empty_badge_dirs(),
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
            &partners, &empty_badge_dirs(),
        )
        .expect("parses");
        assert_eq!(m.source_name, "OtherStreamer");

        // Present but unmatched (the local channel's own id, not a
        // "partner") -> no indicator, not an error/placeholder.
        let line = r#"{"ts":1700000000000,"login":"bob","name":"Bob","text":"hi","source_room_id":"111"}"#;
        let m = parse_twitch_chat_line(
            line, 1_700_000_000_000.0, &HashMap::new(), None, &HashMap::new(), false, &mut fetches,
            &partners, &empty_badge_dirs(),
        )
        .expect("parses");
        assert_eq!(m.source_name, "");

        // No tag at all (no active shared session, or a pre-feature log) -> no indicator.
        let line = r#"{"ts":1700000000000,"login":"bob","name":"Bob","text":"hi"}"#;
        let m = parse_twitch_chat_line(
            line, 1_700_000_000_000.0, &HashMap::new(), None, &HashMap::new(), false, &mut fetches,
            &partners, &empty_badge_dirs(),
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
        let (expected_dest, expected_url) =
            crate::assets::twitch_emote_cdn_fetch("8675309", "brandNewEmoteCode");
        assert_eq!(pending.as_deref(), Some(expected_dest.as_path()));
        assert_eq!(fetches.len(), 1);
        assert_eq!(fetches[0].dest, expected_dest);
        assert_eq!(fetches[0].urls, vec![expected_url]);
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
