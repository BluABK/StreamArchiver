//! The on-demand egui window: channel table, add/edit form, and settings.
//!
//! Runs reactive (repaints only on input/events). The tray thread wakes it via
//! `Context::request_repaint`. Closing the window hides it to the tray; the
//! tray "Quit" item triggers a real close.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use tracing::{debug, info, warn};
use tray_icon::TrayIcon;

use crate::app_core::AppCore;
use crate::events::{ManualCommand, UiCommand};
use crate::models::{
    AdBreak, AuthKind, Channel, Container, DailyRecordingStat, DetectionMethod, DownloadDefaults,
    GlobalStats,
    K_APP_ICON, K_DIALOG_ICON, K_DISCORD_SCHEDULE, K_DISCORD_TOKEN, K_FILENAME_MEDIA, K_MONITOR_DEFAULTS,
    K_OCR_COMMAND, K_OCR_EFFORT, K_OCR_FALLBACK_MODEL, K_OCR_MAX_BUDGET, K_OCR_MODEL,
    K_OCR_OFFSET, K_OCR_STATS, K_OCR_TIMEOUT_SECS, K_OCR_TIMEZONE, K_SCHEDULE_TITLE_FILL,
    K_YT_API_DETECT, K_YT_API_SCHEDULE, K_YT_COMMUNITY_MAX_POSTS, K_YT_API_QUOTA_CUTOFF, K_YT_SEARCH_QUOTA_CUTOFF,
    K_REMUX_EMBED_THUMBNAIL, K_REMUX_EMBED_TITLE, K_REMUX_TITLE_TEMPLATE, K_REMUX_EMBED_SUBS,
    K_FILE_SPLIT_ENABLED, K_FILE_SPLIT_VIDEOS, K_FILE_SPLIT_SUBS, K_FILE_SPLIT_CHAT,
    K_FILE_SPLIT_THUMBS, K_FILE_SPLIT_LOGS,
    MediaInfoMode, Monitor, MonitorDefaults, MonitorStreamChange, MonitorWithChannel, OcrStats, Platform,
    PollStats, RecurrenceKind, Recording, SabrCodecPref, ScheduleSegment, ScheduledRecording,
    ScheduledRecordingWithNames, StreamGroup, StreamMetaChange, StreamStatRow, Tool, UpcomingStream,
    Video, group_recordings, now_unix,
};
use crate::google_oauth;
use crate::grid_columns::{self, ColumnEntry, GridCol, GridState, GridTableId};
use crate::imports::{self, ImportCandidate};
use crate::inspector::Inspectable;
use crate::oauth::{self, AuthFlow};
use crate::platform::AutoStart;
use crate::saved_views::{self, SavedView};
use crate::schedule_source::{
    ScheduleSourceKind, SourceEntry, load_channel_cfg, load_channel_scope, load_monitor_scope,
    load_source_order, save_channel_cfg, save_channel_scope, save_monitor_scope, save_source_order,
    source_badge,
};

const K_TWITCH_ID: &str = "twitch_client_id";
const K_TWITCH_SECRET: &str = "twitch_client_secret";
const K_YT_KEY: &str = "youtube_api_key";
const K_KICK_ID: &str = "kick_client_id";
const K_KICK_SECRET: &str = "kick_client_secret";
const K_DEFAULT_OUT: &str = "default_output_dir";
/// Default output folder for on-demand **video downloads** (Videos tab /
/// Recover VOD) — separate from [`K_DEFAULT_OUT`] (live stream recordings);
/// seeds `DownloadDefaults`' per-platform output dirs instead of the
/// recording default. `pub(crate)`: `app_core.rs`'s headless I/O-monitor-root
/// init reads it too (same reason `K_SABR_POT_ARGS` is `pub(crate)`).
pub(crate) const K_VIDEO_DEFAULT_OUT: &str = "default_video_output_dir";
const K_MAX_CONCURRENT: &str = "max_concurrent_downloads";
const K_DOWNLOAD_AUTH: &str = "download_auth_method";
const K_COOKIES_BROWSER: &str = "cookies_browser";
const K_YTDLP_ARGS: &str = "ytdlp_default_args";
/// Optional explicit path to the system yt-dlp binary; empty ⇒ `yt-dlp` on PATH.
const K_YTDLP_BINARY: &str = "ytdlp_binary_path";
/// Path to the SABR dev-build yt-dlp; empty ⇒ SABR capture disabled.
const K_SABR_BINARY: &str = "ytdlp_sabr_binary_path";
/// Master toggle: use the SABR build for YouTube capture-from-start.
const K_SABR_ENABLED: &str = "ytdlp_sabr_enabled";
/// SABR format selector (e.g. `ba[protocol=sabr]+bv[protocol=sabr]`).
const K_SABR_FORMAT: &str = "ytdlp_sabr_format";
/// SABR `--extractor-args` value.
const K_SABR_EXTRACTOR_ARGS: &str = "ytdlp_sabr_extractor_args";
/// Manual raw SABR args; when non-empty, replaces the format+extractor-args preset.
const K_SABR_RAW_ARGS: &str = "ytdlp_sabr_raw_args";
/// PO-token-provider `--extractor-args` (e.g. bgutil), a separate `--extractor-args`
/// entry on the SABR command. Absent ⇒ bgutil default; explicit empty ⇒ disabled.
/// `pub(crate)`: `pot_server` derives the managed server's base URL from it.
pub(crate) const K_SABR_POT_ARGS: &str = "ytdlp_sabr_pot_args";
/// PO-token fallback client (retry a PO-rejected take without a token).
/// Absent ⇒ the `tv` default; explicit empty ⇒ disabled. Mirrors
/// `downloader::K_SABR_PO_FALLBACK_CLIENT` — the supervisor reads it there.
const K_SABR_PO_FALLBACK: &str = crate::downloader::K_SABR_PO_FALLBACK_CLIENT;
/// Experimental: append `enable_live_deep_rewind=true` to the SABR extractor-args
/// (rewinds past the normal DVR window; dev-build-only). Absent ⇒ off.
const K_SABR_DEEP_REWIND: &str = "ytdlp_sabr_deep_rewind";
/// DASH-companion format selector for dual capture.
const K_DASH_FORMAT: &str = "ytdlp_dash_format";
/// GLOBAL default SABR video codec/quality preference (a [`SabrCodecPref`] id,
/// e.g. `auto`/`best`/`h264`). Per-monitor `Inherit` falls through to this.
const K_SABR_CODEC_PREF: &str = "ytdlp_sabr_codec_pref";
/// GLOBAL raw `-S` string used when `K_SABR_CODEC_PREF == custom`.
const K_SABR_CODEC_CUSTOM: &str = "ytdlp_sabr_codec_custom";
const K_WEBSUB_URL: &str = "websub_vps_url";
const K_WEBSUB_TOKEN: &str = "websub_token";
const K_WEBSUB_POLL: &str = "websub_poll_secs";
/// Whether Streams rows get a status background tint (recording / ad / error).
const K_STATUS_BGCOLOR: &str = "status_bgcolor";
/// How dates/timestamps are formatted throughout the UI (see [`DateFmt`]).
const K_DATE_FORMAT: &str = "date_format";
/// Whether the per-row Actions column is shown (the row context menu has the same
/// actions, so it can be hidden to reclaim width).
const K_SHOW_ACTIONS: &str = "show_actions";
/// Whether timestamp columns use the compact short format (off = full datetime).
const K_SHORT_TIMESTAMPS: &str = "short_timestamps";
/// chrono format pattern used for the compact timestamp display; default `%d/%m %H:%M`.
const K_SHORT_TS_FMT: &str = "short_ts_fmt";
/// Last-selected Settings category tab (restored on launch).
const K_SETTINGS_TAB: &str = "settings_tab";
/// Whether chat-replay emote codes render as inline images (off ⇒ show the code
/// text). Default on; only an explicit `"0"` disables. Needs "Fetch chat assets".
const K_RENDER_EMOTES: &str = "render_emotes_in_chat";
/// Whether animated emotes play (off ⇒ a static first frame). Default on; only an
/// explicit `"0"` disables. Off is the perf/RAM escape hatch for heavy channels.
const K_ANIMATE_EMOTES: &str = "animate_emotes_in_chat";
/// Whether clicking an emote in chat shows it much larger inline — a local
/// echo of Twitch's Bits-powered Gigantify effect (see the checkbox's own
/// hover text for why this can't replay REAL historical Gigantify events).
/// Default on; only an explicit `"0"` disables.
const K_CHAT_GIGANTIFY: &str = "chat_gigantify_enabled";
/// Whether a first-party Twitch emote id missing from every locally-cached
/// channel (this app's own monitored channels) gets fetched straight from
/// Twitch's public CDN by id, for a poster whose home channel isn't
/// monitored/archived here at all. Default on; only an explicit `"0"`
/// disables. Separate from [`K_RENDER_EMOTES`] on purpose: the latter covers
/// purely-local rendering, this one gates a NEW network fetch for channels
/// the user hasn't added. See `assets::twitch_emote_cdn_fetch`.
const K_FETCH_UNKNOWN_EMOTES: &str = "fetch_unknown_twitch_emotes";
/// Chat-replay text size in points, applied uniformly to the timestamp,
/// message body, and username (they render at the same size on Twitch's own
/// popout chat — the default here used to leave the timestamp noticeably
/// smaller). Edited from the ⚙ "Chat Appearance" panel inside each chat
/// window, not Settings — see [`crate::ui::chat`]'s doc.
const K_CHAT_FONT_PT: &str = "chat_font_size_pt";
const CHAT_FONT_PT_DEFAULT: f32 = 14.0;
/// Chat-replay emote size in pixels — independent of `K_CHAT_FONT_PT` (a
/// reader may want large emotes with small text or vice versa). Applies to
/// both first/third-party emotes and Unicode emoji; badge icons scale off
/// the font size instead (see `ui::chat::render_chat_message`).
const K_CHAT_EMOTE_PT: &str = "chat_emote_size_px";
const CHAT_EMOTE_PT_DEFAULT: f32 = 24.0;
/// Target height for "wide" emotes specifically (decoded aspect ratio well
/// over 1:1 — 7TV's walk-cycle/banner-style emotes commonly). Separate from
/// `K_CHAT_EMOTE_PT` because a single size + a fixed max-width cap crushes a
/// wide emote's height too, not just its width — see
/// `ui::chat::emotes::draw_cached_emote`'s `wide` parameter doc.
const K_CHAT_EMOTE_WIDE_PT: &str = "chat_emote_wide_size_px";
const CHAT_EMOTE_WIDE_PT_DEFAULT: f32 = 24.0;
/// Timestamp size relative to `K_CHAT_FONT_PT` (points, can be negative).
/// Default -1: a hair smaller than the message body reads as a timestamp,
/// not a fourth column of body text — configurable because that's a
/// preference, not a fact about what's "correct".
const K_CHAT_TS_SIZE_OFFSET: &str = "chat_ts_size_offset_pt";
const CHAT_TS_SIZE_OFFSET_DEFAULT: f32 = -1.0;
/// Vertical gap between chat rows, in pixels. Twitch's own popout gives
/// each line noticeably more breathing room than a 2px hairline; the
/// default here splits the difference rather than matching either exactly,
/// and it's configurable because "how much" is a preference.
const K_CHAT_ROW_SPACING: &str = "chat_row_spacing_px";
const CHAT_ROW_SPACING_DEFAULT: f32 = 6.0;
/// Chat-replay timestamp color (hex `#RRGGBB`). Default white — the previous
/// hardcoded `weak_text_color()` rendered too dark-grey to read comfortably.
const K_CHAT_TS_COLOR: &str = "chat_ts_color";
/// Chat-replay message body color (hex `#RRGGBB`). Default white.
const K_CHAT_TEXT_COLOR: &str = "chat_text_color";
/// Whether opening a chat usercard also does a live Twitch Helix lookup for
/// the user's avatar + account-created date, on top of the always-available
/// local data (badges/color/sub-months/session stats). Default OFF — unlike
/// [`K_FETCH_UNKNOWN_EMOTES`] this hits the network on every usercard open,
/// not just once per missing asset. A failed lookup shows "N/A" and files a
/// warning (see `ui::chat`'s usercard fetch).
const K_FETCH_USERCARD_INFO: &str = "fetch_usercard_twitch_info";
/// Whether the chat window's Hype Train card is available at all. Default on;
/// only an explicit `"0"` disables. This is the FEATURE switch — the toolbar's
/// 🚂 toggle collapses the card in one window for this session, and a train
/// starting re-opens that per-window toggle but never overrides this one. See
/// `ui::chat::strips::hype_phase`.
const K_CHAT_SHOW_HYPE: &str = "chat_show_hype_train";
/// Whether the chat window's channel-info card (top supporters, and creator
/// goals once those land) is available at all. Default on; the toolbar toggle
/// is the per-window collapse, same shape as [`K_CHAT_SHOW_HYPE`].
const K_CHAT_SHOW_INFO: &str = "chat_show_channel_info";
/// App-wide UI font family, by its installed display name (`""` = egui's
/// bundled default). Stored as a NAME, not a path, so the setting survives the
/// font being reinstalled to a different file. The pick is inserted in FRONT
/// of egui's default rather than replacing it — the default carries the UI
/// icon glyphs still used outside the chat window. See [`crate::fonts`].
pub(crate) const K_APP_FONT_FAMILY: &str = "app_font_family";
/// Chat-replay font family, same shape as [`K_APP_FONT_FAMILY`] but for the
/// chat window only — it renders in its own registered family
/// ([`crate::fonts::CHAT_FAMILY`]) so the two can differ without either
/// losing the non-Latin fallbacks.
pub(crate) const K_CHAT_FONT_FAMILY: &str = "chat_font_family";
/// Which clock the chat replay's timestamps show: `"relative"` (default,
/// `[00:40:10]` into the broadcast) or `"clock"` (`19:30` local time, as
/// Twitch's own popout does). Flipped from the 🕒 toolbar toggle in any chat
/// window rather than buried in Settings — both are the right answer at
/// different moments. See `ui::chat::rows::ChatTsMode`.
const K_CHAT_TS_MODE: &str = "chat_timestamp_mode";
/// Fill colour for the chat window's Creator Goal bar: a `#RRGGBB`, or the
/// sentinel `"channel"` to use the channel's own display colour. Twitch's own
/// goal red is deliberately loud on a live page and reads as harsh in a
/// desktop chat window sitting open for hours, so the default here is a
/// muted version of it — and the whole thing is configurable because "how
/// loud" is a matter of taste, not correctness.
const K_CHAT_GOAL_COLOR: &str = "chat_goal_color";
/// Send button fill: a hex colour, or `"channel"` to inherit the channel's
/// own display colour — same `"channel"`-or-hex shape as [`K_CHAT_GOAL_COLOR`].
/// Default: Twitch's own send-button purple.
const K_CHAT_SEND_BUTTON_COLOR: &str = "chat_send_button_color";
/// Per-instance timestamp-mode overrides, as `{"<monitor id>": "clock"}`.
/// An instance absent from the map INHERITS [`K_CHAT_TS_MODE`] — the same
/// delete-not-store shape the scoped capture settings use, so "follow the
/// default" and "happens to match the default" stay the same state.
const K_CHAT_TS_MODE_BY_MONITOR: &str = "chat_timestamp_mode_by_monitor";
/// Path to the media player binary used by "Play local recording (start)" on
/// recording rows. `pub(crate)`: also read directly by auto-play Follow raid
/// (`downloader::raid_follow`), which builds its own minimal `SettingsForm`
/// snapshot rather than depending on the UI's live one.
pub(crate) const K_MEDIA_PLAYER: &str = "media_player_path";
/// Window-title template for "Play stream (live edge)" — see
/// [`crate::ui::player::render_live_title`] for the token list.
const K_LIVE_TITLE_TEMPLATE: &str = "live_edge_title_template";
/// Keep pushing an updated title (over mpv's IPC socket) as the monitor's
/// title/game changes, for the launch paths this app spawns mpv directly for.
/// Read via [`live_title_auto_update_setting`], never raw — the two readers
/// (settings form, follow-raid auto-play) once decoded the unset case
/// differently (`== "1"` vs `!= "0"`), so the same fresh install had the
/// feature off in the UI but on for auto-played raid windows.
const K_LIVE_TITLE_AUTO_UPDATE: &str = "live_edge_title_auto_update";

/// The one decoding of [`K_LIVE_TITLE_AUTO_UPDATE`]: on unless explicitly
/// `"0"`. Default-on is the right side of the old inconsistency to keep —
/// the feature is best-effort, mpv-only, and invisible unless a title
/// template is set (which has a non-empty default).
pub(crate) fn live_title_auto_update_setting(store: &crate::store::Store) -> bool {
    store
        .get_setting(K_LIVE_TITLE_AUTO_UPDATE)
        .ok()
        .flatten()
        .is_none_or(|v| v != "0")
}
/// Mute every non-clicked-on instance opened by "Play all collab instances
/// (live edge)".
const K_MUTE_COLLAB_INSTANCES: &str = "mute_collab_instances";
/// Window-title template for an untracked collab partner (see
/// [`crate::ui::player::spawn_play_collab_partner`]).
const K_COLLAB_UNTRACKED_TITLE_TEMPLATE: &str = "collab_untracked_title_template";

/// Browsers yt-dlp can read cookies from (for the Settings dropdown).
const COOKIE_BROWSERS: [&str; 8] = [
    "firefox", "chrome", "chromium", "edge", "brave", "opera", "vivaldi", "safari",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Streams,
    Videos,
    Schedule,
    Posts,
    Background,
    Files,
    /// Cross-channel recording history triaged by watch-state (unwatched /
    /// started / skipped / watched) — see `history::backlog_view`.
    Backlog,
    /// Cross-channel recording history filtered by VOD/remux/chapters state
    /// — see `history::stream_history_view`.
    StreamHistory,
    /// Log of automatic media disposals (trash/Recycle Bin/permanent),
    /// grouped by channel — restore or permanently delete a soft-deleted
    /// (Trash-method) file. See `trash::trash_view`.
    Trash,
    Settings,
    /// Per-channel viewer/follower/event history ("Channel Stats" tab).
    ChannelStats,
    /// App/system health ("App Stats" tab): OCR, API quota, poll health,
    /// recording totals.
    Stats,
    IoMonitor,
    Debug,
    /// Who chatted where: search a chatter and see every stream they were in,
    /// what they said, gave, and what moderators did — see `users::users_view`.
    Users,
    /// In-app manual (the embedded README, sectioned) + About (version/build
    /// info and data paths). Reached via the Help ▾ menu.
    Help,
}

/// Previous-frame top-bar measurements driving the primary-tab overflow: tabs
/// collapse into a `»` menu before the left content can ever reach the
/// right-aligned status buttons.
struct TopBarLayout {
    /// Width the right-to-left button cluster used last frame (0.0 on the
    /// very first frame — the caller substitutes a conservative reservation).
    right_w: f32,
    /// How many primary tabs were visible last frame (hysteresis anchor).
    visible: usize,
}

impl Default for TopBarLayout {
    fn default() -> Self {
        // `visible = MAX` means "unconstrained" — the first frame takes the
        // pure fit count without a growth-hysteresis penalty.
        TopBarLayout { right_w: 0.0, visible: usize::MAX }
    }
}

/// How many leading primary tabs fit: all of them when the row fits whole;
/// otherwise the largest prefix that fits alongside the `»` overflow button.
/// Shrinking applies immediately (overlap must never happen), but growing
/// past `prev_visible` requires `hysteresis` px of spare room so a width
/// sitting exactly on the boundary doesn't flicker tabs in and out.
fn partition_tabs(
    widths: &[f32],
    budget: f32,
    overflow_w: f32,
    prev_visible: usize,
    hysteresis: f32,
) -> usize {
    let n = widths.len();
    let total: f32 = widths.iter().sum();
    // Ideal count at this width, ignoring hysteresis.
    let fit = if total <= budget {
        n
    } else {
        let mut used = overflow_w;
        let mut k = 0;
        for w in widths {
            if used + w > budget {
                break;
            }
            used += w;
            k += 1;
        }
        k
    };
    let prev = prev_visible.min(n);
    if fit <= prev {
        return fit; // shrink (or no change): apply immediately
    }
    // Growth: only take the extra tabs when the grown layout has spare room.
    let grown_used =
        if fit == n { total } else { overflow_w + widths[..fit].iter().sum::<f32>() };
    if grown_used + hysteresis <= budget { fit } else { prev }
}

/// Timespan choices for the Stats view's detection-history graphs. Each span
/// picks its own display bucket width so every view lands around 60–360
/// points per line; the underlying `poll_history` table stores minute
/// resolution regardless (aggregation happens in the query).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PollSpan {
    OneMin,
    FiveMin,
    TenMin,
    FifteenMin,
    ThirtyMin,
    Hour,
    SixHours,
    TwelveHours,
    Day,
    Week,
    Month,
    ThreeMonths,
    Year,
    /// Everything ever sampled (Channel Stats keeps history forever; the poll
    /// graphs' own table only retains 60 days, so it shows what exists).
    All,
}

impl PollSpan {
    const ALL: [PollSpan; 14] = [
        PollSpan::OneMin,
        PollSpan::FiveMin,
        PollSpan::TenMin,
        PollSpan::FifteenMin,
        PollSpan::ThirtyMin,
        PollSpan::Hour,
        PollSpan::SixHours,
        PollSpan::TwelveHours,
        PollSpan::Day,
        PollSpan::Week,
        PollSpan::Month,
        PollSpan::ThreeMonths,
        PollSpan::Year,
        PollSpan::All,
    ];

    fn label(self) -> &'static str {
        match self {
            PollSpan::OneMin => "1 m",
            PollSpan::FiveMin => "5 m",
            PollSpan::TenMin => "10 m",
            PollSpan::FifteenMin => "15 m",
            PollSpan::ThirtyMin => "30 m",
            PollSpan::Hour => "1 h",
            PollSpan::SixHours => "6 h",
            PollSpan::TwelveHours => "12 h",
            PollSpan::Day => "24 h",
            PollSpan::Week => "7 d",
            PollSpan::Month => "30 d",
            PollSpan::ThreeMonths => "90 d",
            PollSpan::Year => "1 y",
            PollSpan::All => "All",
        }
    }

    /// How far back the view reaches.
    fn secs(self) -> i64 {
        match self {
            PollSpan::OneMin => 60,
            PollSpan::FiveMin => 5 * 60,
            PollSpan::TenMin => 10 * 60,
            PollSpan::FifteenMin => 15 * 60,
            PollSpan::ThirtyMin => 30 * 60,
            PollSpan::Hour => 3_600,
            PollSpan::SixHours => 6 * 3_600,
            PollSpan::TwelveHours => 12 * 3_600,
            PollSpan::Day => 86_400,
            PollSpan::Week => 7 * 86_400,
            PollSpan::Month => 30 * 86_400,
            PollSpan::ThreeMonths => 90 * 86_400,
            PollSpan::Year => 365 * 86_400,
            // "Since forever": 50 years reaches past any sample without
            // underflowing the `now - secs()` arithmetic callers do.
            PollSpan::All => 50 * 365 * 86_400,
        }
    }

    /// Display bucket width for this span (what one plotted point covers).
    fn bucket_secs(self) -> i64 {
        match self {
            // Sub-hour spans all show the raw minute samples.
            PollSpan::OneMin
            | PollSpan::FiveMin
            | PollSpan::TenMin
            | PollSpan::FifteenMin
            | PollSpan::ThirtyMin => 60,
            PollSpan::Hour => 60,             // minute detail
            PollSpan::SixHours => 300,        // 5 min
            PollSpan::TwelveHours => 600,     // 10 min
            PollSpan::Day => 1_800,           // 30 min
            PollSpan::Week => 3 * 3_600,      // 3 h
            PollSpan::Month => 12 * 3_600,    // 12 h
            PollSpan::ThreeMonths => 86_400,  // 1 d
            PollSpan::Year => 3 * 86_400,     // 3 d
            PollSpan::All => 7 * 86_400,      // 1 w
        }
    }

    /// Human label for [`PollSpan::bucket_secs`] (tooltips, y-axis captions).
    fn bucket_label(self) -> &'static str {
        match self {
            PollSpan::OneMin
            | PollSpan::FiveMin
            | PollSpan::TenMin
            | PollSpan::FifteenMin
            | PollSpan::ThirtyMin => "1 min",
            PollSpan::Hour => "1 min",
            PollSpan::SixHours => "5 min",
            PollSpan::TwelveHours => "10 min",
            PollSpan::Day => "30 min",
            PollSpan::Week => "3 h",
            PollSpan::Month => "12 h",
            PollSpan::ThreeMonths => "1 d",
            PollSpan::Year => "3 d",
            PollSpan::All => "1 w",
        }
    }

    /// Whether the graphs' relative time axis reads better in days than hours.
    fn axis_in_days(self) -> bool {
        matches!(
            self,
            PollSpan::Week
                | PollSpan::Month
                | PollSpan::ThreeMonths
                | PollSpan::Year
                | PollSpan::All
        )
    }
}

/// Period selector for the Stats view's Recordings breakdown. `Day` lists the
/// 7 individual days of the current calendar week; `Week`/`Month`/`Year` each
/// show two summary rows (the current, still-elapsing period and the last
/// fully-elapsed one) rather than a long trend table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordingsPeriod {
    Day,
    Week,
    Month,
    Year,
}

impl RecordingsPeriod {
    const ALL: [RecordingsPeriod; 4] = [
        RecordingsPeriod::Day,
        RecordingsPeriod::Week,
        RecordingsPeriod::Month,
        RecordingsPeriod::Year,
    ];

    fn label(self) -> &'static str {
        match self {
            RecordingsPeriod::Day => "Day",
            RecordingsPeriod::Week => "Week",
            RecordingsPeriod::Month => "Month",
            RecordingsPeriod::Year => "Year",
        }
    }
}

/// Per-monitor lowercase `(titles, categories)` filter haystacks — the shape
/// `Store::monitor_meta_filter_texts` returns (see `deep_filter_texts`).
type DeepFilterTexts = std::collections::HashMap<i64, (String, String)>;

/// The Stats view's "downloading right now" readout, derived from the newest
/// `iomon` sample. Cached rather than recomputed per frame — the sampler only
/// produces one of these a second.
#[derive(Clone, Default)]
struct NetLive {
    /// Current B/s and live tool count, indexed by `iomon::NetKind` position.
    per_kind: [(u64, u32); crate::iomon::NET_KIND_COUNT],
    /// Bytes downloaded per class since the app started.
    session: [u64; crate::iomon::NET_KIND_COUNT],
    /// Age of the underlying sample in ms — a stalled sampler must not be
    /// read as "nothing is downloading".
    age_ms: i64,
    /// False until the sampler has produced its first sample.
    have_sample: bool,
}

impl NetLive {
    /// Fold the newest sample's per-process rates into per-class totals.
    fn capture() -> Self {
        let mut out = NetLive {
            session: crate::iomon::net_session_totals(),
            ..Default::default()
        };
        if let Some(s) = crate::iomon::latest() {
            out.have_sample = true;
            out.age_ms = chrono::Utc::now().timestamp_millis() - s.at_ms;
            for p in &s.procs {
                let slot = &mut out.per_kind[p.net as usize];
                // `net_bps`, not `read_bps`: the graph and the history are fed
                // from the same root-only number, so the two must agree.
                slot.0 += p.net_bps;
                slot.1 += 1;
            }
        }
        out
    }

    /// Total current download rate across every network class.
    fn total_bps(&self) -> u64 {
        crate::iomon::NetKind::NETWORK
            .iter()
            .map(|k| self.per_kind[*k as usize].0)
            .sum()
    }
}

mod app;
mod assets_helpers;
mod background;
mod calendar;
mod channel_stats;
// `pub(crate)`, unlike its neighbors: `downloader::supervisor`'s
// "Fetch missing chat emotes" maintenance sweep reuses this module's
// `parse_chat_chunk`/`EmojiFetch`/`download_emoji_images` directly rather
// than duplicating chat-log parsing.
pub(crate) mod chat;
mod debug;
mod dialogs;
mod files;
mod format;
mod grid;
mod help;
mod history;
pub(crate) mod io_view;
mod issues;
mod layout_editor;
mod log_view;
pub(crate) mod player;
mod popup;
mod posts;
mod pot_log;
mod properties;
mod schedule;
mod settings;
mod streams;
mod trash;
mod users;
mod videos;

#[allow(unused_imports)]
use {app::*, assets_helpers::*, background::*, calendar::*, chat::*, debug::*, dialogs::*, files::*, format::*, grid::*, help::*, history::*, io_view::*, issues::*, layout_editor::*, player::*, popup::*, posts::*, properties::*, schedule::*, settings::*, streams::*, trash::*, videos::*};

/// Backing state for the add/edit dialog. `name` is the channel (container) name;
/// `url` is this *instance's* source URL (the platform is derived from it).
struct MonitorForm {
    monitor_id: Option<i64>,
    channel_id: Option<i64>,
    name: String,
    url: String,
    tool: Tool,
    detection_method: DetectionMethod,
    poll_interval_secs: i64,
    quality: String,
    output_dir: String,
    filename_template: String,
    container: Container,
    capture_from_start: bool,
    /// YouTube dual capture: also run a DASH companion process (system yt-dlp).
    dual_capture: bool,
    /// Manually mark this instance ad-free (member/sub/Turbo) — drives the Ad-free
    /// column when auto detection isn't available.
    ad_free: bool,
    /// Auto-record toggle (the "Auto" column) — disk recording only.
    enabled: bool,
    /// Master automation toggle (the "Enabled" column) — off = fully dormant.
    automation_enabled: bool,
    auth_kind: AuthKind,
    auth_value: String,
    /// Audio tracks to capture (streamlink `--hls-audio-select`): empty = default,
    /// `all`/`*` = every track, or a comma-separated list.
    audio_tracks: String,
    /// Subtitle tracks to capture (yt-dlp `--sub-langs`): empty = none, `all` =
    /// every subtitle, or a comma-separated list of language codes.
    subtitle_tracks: String,
    /// Capture chat alongside the recording (Twitch IRC sidecar / yt-dlp live_chat).
    chat_log: bool,
    /// Download stream thumbnail at recording start (yt-dlp: --write-thumbnail;
    /// Twitch/Kick/YouTube: fetch URL from detection metadata).
    fetch_thumbnail: bool,
    /// Use the stream thumbnail (when fetched) as the hero image in the
    /// recording-started notification instead of the channel's static banner.
    thumbnail_in_toast: bool,
    /// Download channel icon, banner, badges, and emotes (Twitch: BTTV/FFZ/7TV too)
    /// into channel_assets/ alongside recordings.
    fetch_chat_assets: bool,
    extra_args: String,
    /// YouTube SABR video codec/quality preference (Inherit ⇒ follow the global
    /// default) + its raw `-S` sort when `Custom`.
    sabr_codec_pref: SabrCodecPref,
    sabr_codec_custom: String,
    /// Platform the tool/detection defaults were last set for; a URL change to a
    /// different platform re-applies that platform's defaults.
    last_platform: Option<Platform>,
    /// Platform/name `output_dir` was last resolved for — tracked separately
    /// from `last_platform` because a `{name}`-templated output dir also
    /// needs re-resolving when the name changes (typed after a brand-new
    /// channel's URL is pasted), which none of the other platform defaults do.
    output_dir_platform: Option<Platform>,
    output_dir_name: String,
    /// Post-stream VOD-download overrides for this instance (`None` = inherit the
    /// channel/global default). Loaded from / saved to the monitor scope map.
    vod_download: Option<bool>,
    vod_replace: Option<bool>,
    /// Head-backfill-on-new-take overrides for this instance (`None` = inherit
    /// the channel/global default). Loaded from / saved to the monitor scope map.
    head_backfill_fetch: Option<bool>,
    head_backfill_replace: Option<bool>,
    /// Automatic-deletion overrides for this instance (`None` = inherit the
    /// channel/global default): what happens to head/live parts after a
    /// full.mkv join, and how automatic media deletes are executed. Loaded
    /// from / saved to the monitor disposal scope map (`crate::disposal`).
    join_cleanup: Option<crate::disposal::JoinCleanup>,
    disposal_method: Option<crate::disposal::DisposalMethod>,
    /// Rolling-recording overrides for this instance (`None`/empty = inherit
    /// the channel/global default): whether its captures are auto-deleted
    /// after a TTL unless kept, and how long that TTL is (in **hours**, as
    /// typed — stored as seconds). See [`crate::rolling`].
    rolling: Option<bool>,
    rolling_ttl_hours: String,
    /// Simulcast-dedup overrides for this instance (`None` = inherit the
    /// channel, then the global default). `Off` here is an exemption:
    /// always record this instance even when a preferred sibling is live.
    /// See [`crate::simulcast`].
    simulcast_pref: Option<crate::simulcast::SimulcastPref>,
    simulcast_ad_free_pref: Option<crate::simulcast::SimulcastPref>,
    /// "Always show this instance's info on the channel row when it's live" —
    /// the strongest tier of `crate::platform_pref` (beats both the channel
    /// and global platform preference). Loaded from / saved to the monitor
    /// pin map.
    primary_pin: bool,
    /// Chapter-embedding master toggle override for this instance (`None` =
    /// inherit the channel/global default). Loaded from / saved to the
    /// monitor chapters scope map (`crate::chapters`).
    chapters_enabled: Option<bool>,
    /// Title/category coalesce-window override for this instance, seconds
    /// (empty = inherit the channel/global default).
    chapters_coalesce_secs: String,
    /// Follow-raid overrides for this instance (`None` = inherit the
    /// channel/global default). Loaded from / saved to the monitor raid-
    /// follow scope maps (`crate::raid_follow`).
    follow_my_raids: Option<bool>,
    record_me_as_raid_target: Option<bool>,
    /// Auto-play (no recording) follow-raid override for this instance, and
    /// whether this instance is excluded from ever being auto-played as a
    /// raid target — both independent of the record-side fields above.
    follow_my_raids_play: Option<bool>,
    exclude_from_auto_play: Option<bool>,
    /// The other half of the gate the manual "🗑🔥 Delete file from disk"
    /// take-row action needs (see `crate::manual_delete`) — plain bool, not
    /// an inherit chain: off by default. The channel-level half lives on
    /// `ChannelForm::allow_delete`.
    allow_delete: bool,
    /// Set by the deferred closure on Save/Enter; read back by
    /// `form_window` next call.
    do_save: bool,
    /// Set by the deferred closure on Cancel/close.
    closed: bool,
    open_format_designer: bool,
    browse_req: Option<PendingBrowse>,
    preset_delete: Option<i64>,
    preset_save_tmpl: Option<String>,
    /// Snapshot of `self.monitor_defaults`/`self.settings.default_output_dir`,
    /// refreshed by `form_window` every call — the deferred closure can't
    /// reach `self` to re-resolve platform defaults live as the URL/name
    /// change.
    monitor_defaults: MonitorDefaults,
    default_output_dir: String,
    /// Snapshot of `self.custom_presets`, refreshed every call.
    custom_presets: Vec<(i64, String, String)>,
}

impl MonitorForm {
    /// "Add stream": a new channel container + its first instance.
    fn new_channel(defaults: &MonitorDefaults, default_output_dir: &str) -> MonitorForm {
        // Use Generic platform as the starting point; once the user pastes a URL
        // the platform-change handler re-resolves tool/detection/etc. for that platform.
        let p = Platform::Generic;
        MonitorForm {
            monitor_id: None,
            channel_id: None,
            name: String::new(),
            primary_pin: false,
            url: String::new(),
            tool: defaults.resolve_tool(p),
            detection_method: defaults.resolve_detection(p),
            poll_interval_secs: defaults.resolve_poll_interval(p),
            quality: defaults.resolve_quality(p),
            // Name is still blank at this point (not yet typed) — a
            // `{name}`-templated output dir starts empty and is re-resolved
            // once the user types it (see `output_dir_platform`/`output_dir_name`
            // and the dialog's live re-resolve block).
            output_dir: defaults.resolve_output_dir(p, "", default_output_dir),
            filename_template: defaults.resolve_filename_template(p),
            container: defaults.resolve_container(p),
            capture_from_start: defaults.resolve_from_start(p),
            dual_capture: false,
            ad_free: false,
            enabled: true,
            automation_enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            // New monitors default to max-archival: every audio + subtitle track,
            // chat logging, thumbnails, and channel assets all on.
            audio_tracks: "all".into(),
            subtitle_tracks: "all".into(),
            chat_log: true,
            fetch_thumbnail: true,
            thumbnail_in_toast: false,
            fetch_chat_assets: true,
            extra_args: String::new(),
            sabr_codec_pref: SabrCodecPref::Inherit,
            sabr_codec_custom: String::new(),
            last_platform: None,
            output_dir_platform: Some(p),
            output_dir_name: String::new(),
            vod_download: None,
            vod_replace: None,
            head_backfill_fetch: None,
            head_backfill_replace: None,
            join_cleanup: None,
            disposal_method: None,
            rolling: None,
            rolling_ttl_hours: String::new(),
            simulcast_pref: None,
            simulcast_ad_free_pref: None,
            chapters_enabled: None,
            chapters_coalesce_secs: String::new(),
            follow_my_raids: None,
            record_me_as_raid_target: None,
            follow_my_raids_play: None,
            exclude_from_auto_play: None,
            allow_delete: false,
            do_save: false,
            closed: false,
            open_format_designer: false,
            browse_req: None,
            preset_delete: None,
            preset_save_tmpl: None,
            monitor_defaults: defaults.clone(),
            default_output_dir: default_output_dir.to_string(),
            custom_presets: Vec::new(),
        }
    }

    fn from_existing(row: &MonitorWithChannel) -> MonitorForm {
        let m = &row.monitor;
        MonitorForm {
            monitor_id: Some(m.id),
            channel_id: Some(row.channel.id),
            name: row.channel.name.clone(),
            url: m.url.clone(),
            tool: m.tool,
            detection_method: m.detection_method,
            poll_interval_secs: m.poll_interval_secs,
            quality: m.quality.clone(),
            output_dir: m.output_dir.clone(),
            filename_template: m.filename_template.clone(),
            container: m.container,
            capture_from_start: m.capture_from_start,
            dual_capture: m.dual_capture,
            ad_free: m.ad_free,
            enabled: m.enabled,
            automation_enabled: m.automation_enabled,
            auth_kind: m.auth_kind,
            auth_value: m.auth_value.clone(),
            audio_tracks: m.audio_tracks.clone(),
            subtitle_tracks: m.subtitle_tracks.clone(),
            chat_log: m.chat_log,
            fetch_thumbnail: m.fetch_thumbnail,
            thumbnail_in_toast: m.thumbnail_in_toast,
            fetch_chat_assets: m.fetch_chat_assets,
            extra_args: m.extra_args.clone(),
            sabr_codec_pref: m.sabr_codec_pref,
            sabr_codec_custom: m.sabr_codec_custom.clone(),
            // Don't override the saved tool/detection just because the form opened.
            last_platform: Some(m.platform()),
            // `output_dir` above is already the stored literal — these two
            // match its "already resolved for" state so the live re-resolve
            // block doesn't immediately overwrite it just because the form opened.
            output_dir_platform: Some(m.platform()),
            output_dir_name: row.channel.name.clone(),
            // Overridden by the caller from the monitor scope map (needs the store).
            vod_download: None,
            vod_replace: None,
            head_backfill_fetch: None,
            head_backfill_replace: None,
            join_cleanup: None,
            disposal_method: None,
            rolling: None,
            rolling_ttl_hours: String::new(),
            simulcast_pref: None,
            simulcast_ad_free_pref: None,
            primary_pin: false,
            chapters_enabled: None,
            chapters_coalesce_secs: String::new(),
            follow_my_raids: None,
            record_me_as_raid_target: None,
            follow_my_raids_play: None,
            exclude_from_auto_play: None,
            allow_delete: false,
            do_save: false,
            closed: false,
            open_format_designer: false,
            browse_req: None,
            preset_delete: None,
            preset_save_tmpl: None,
            // Refreshed by `form_window` before the very first render.
            monitor_defaults: MonitorDefaults::default(),
            default_output_dir: String::new(),
            custom_presets: Vec::new(),
        }
    }

    /// Add another instance to an existing channel container. The URL is blank so
    /// the user enters a (possibly different-platform) source.
    fn add_instance(channel: &Channel, defaults: &MonitorDefaults, default_output_dir: &str) -> MonitorForm {
        let p = Platform::Generic;
        MonitorForm {
            monitor_id: None,
            channel_id: Some(channel.id),
            name: channel.name.clone(),
            primary_pin: false,
            url: String::new(),
            tool: defaults.resolve_tool(p),
            detection_method: defaults.resolve_detection(p),
            poll_interval_secs: defaults.resolve_poll_interval(p),
            quality: defaults.resolve_quality(p),
            output_dir: defaults.resolve_output_dir(p, &channel.name, default_output_dir),
            filename_template: defaults.resolve_filename_template(p),
            container: defaults.resolve_container(p),
            capture_from_start: defaults.resolve_from_start(p),
            dual_capture: false,
            ad_free: false,
            enabled: true,
            automation_enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            // New monitors default to max-archival: every audio + subtitle track,
            // chat logging, thumbnails, and channel assets all on.
            audio_tracks: "all".into(),
            subtitle_tracks: "all".into(),
            chat_log: true,
            fetch_thumbnail: true,
            thumbnail_in_toast: false,
            fetch_chat_assets: true,
            extra_args: String::new(),
            sabr_codec_pref: SabrCodecPref::Inherit,
            sabr_codec_custom: String::new(),
            last_platform: None,
            output_dir_platform: Some(p),
            output_dir_name: channel.name.clone(),
            vod_download: None,
            vod_replace: None,
            head_backfill_fetch: None,
            head_backfill_replace: None,
            join_cleanup: None,
            disposal_method: None,
            rolling: None,
            rolling_ttl_hours: String::new(),
            simulcast_pref: None,
            simulcast_ad_free_pref: None,
            chapters_enabled: None,
            chapters_coalesce_secs: String::new(),
            follow_my_raids: None,
            record_me_as_raid_target: None,
            follow_my_raids_play: None,
            exclude_from_auto_play: None,
            allow_delete: false,
            do_save: false,
            closed: false,
            open_format_designer: false,
            browse_req: None,
            preset_delete: None,
            preset_save_tmpl: None,
            monitor_defaults: defaults.clone(),
            default_output_dir: default_output_dir.to_string(),
            custom_presets: Vec::new(),
        }
    }

    /// "Add stream" for a confirmed-but-untracked collab partner — right-click
    /// → "Add as new instance" on their name in the Streams grid (Name-cell
    /// suffix or 🤝 Collab column). A brand-new channel, like `new_channel`,
    /// but with the URL/name already known from the partner's Twitch login/
    /// display name instead of blank — the dialog's own platform-detected
    /// re-resolve (`form.last_platform`/`output_dir_platform` mismatch on
    /// first render) fills in Twitch's tool/detection/output-dir defaults
    /// exactly as if the user had pasted the URL and typed the name by hand.
    fn from_collab_partner(
        login: &str,
        name: &str,
        defaults: &MonitorDefaults,
        default_output_dir: &str,
    ) -> MonitorForm {
        let mut mf = Self::new_channel(defaults, default_output_dir);
        mf.name = name.to_string();
        mf.url = format!("https://twitch.tv/{login}");
        mf
    }
}

/// A three-state **Inherit / On / Off** dropdown for an `Option<bool>` override
/// (`None` = inherit the level above). Returns the combo's response for hovers.
fn tristate_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<bool>) -> egui::Response {
    let text = match value {
        None => "Inherit",
        Some(true) => "On",
        Some(false) => "Off",
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(text)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Inherit");
            ui.selectable_value(value, Some(true), "On");
            ui.selectable_value(value, Some(false), "Off");
        })
        .response
}

/// An **Inherit / Keep parts / Delete head / Delete head + capture** dropdown
/// for an `Option<JoinCleanup>` override (`None` = inherit the level above).
/// Returns the combo's response for hovers. See [`crate::disposal`].
fn join_cleanup_combo(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut Option<crate::disposal::JoinCleanup>,
) -> egui::Response {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.map(crate::disposal::JoinCleanup::label).unwrap_or("Inherit"))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Inherit");
            for c in crate::disposal::JoinCleanup::ALL {
                ui.selectable_value(value, Some(c), c.label());
            }
        })
        .response
}

/// An **Inherit / Trash folder / Recycle Bin / Delete permanently** dropdown
/// for an `Option<DisposalMethod>` override (`None` = inherit the level
/// above). Returns the combo's response for hovers. See [`crate::disposal`].
fn disposal_method_combo(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut Option<crate::disposal::DisposalMethod>,
) -> egui::Response {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.map(crate::disposal::DisposalMethod::label).unwrap_or("Inherit"))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Inherit");
            for m in crate::disposal::DisposalMethod::ALL {
                ui.selectable_value(value, Some(m), m.label());
            }
        })
        .response
}

/// An **Inherit / Twitch / YouTube / Kick** dropdown for an `Option<Platform>`
/// override (`None` = inherit the level above). Returns the combo's response
/// for hovers. See [`crate::platform_pref`].
fn platform_pref_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<Platform>) -> egui::Response {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.map(Platform::label).unwrap_or("Inherit"))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Inherit");
            ui.selectable_value(value, Some(Platform::Twitch), Platform::Twitch.label());
            ui.selectable_value(value, Some(Platform::YouTube), Platform::YouTube.label());
            ui.selectable_value(value, Some(Platform::Kick), Platform::Kick.label());
        })
        .response
}

/// An **Inherit / (off) / Twitch / YouTube / Kick** dropdown for a simulcast
/// dedup override (`None` = inherit the level above). `off_label` spells out
/// what `Off` means in this particular row — "record every live instance" for
/// the everyday preference, "no ad-free override" for the other — since the
/// same enum backs both. See [`crate::simulcast`].
fn simulcast_pref_combo(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut Option<crate::simulcast::SimulcastPref>,
    off_label: &str,
) -> egui::Response {
    use crate::simulcast::SimulcastPref;
    let selected = match value {
        None => "Inherit",
        Some(SimulcastPref::Off) => off_label,
        Some(p) => p.label(),
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Inherit");
            ui.selectable_value(value, Some(SimulcastPref::Off), off_label);
            for p in SimulcastPref::ALL.into_iter().filter(|p| *p != SimulcastPref::Off) {
                ui.selectable_value(value, Some(p), p.label());
            }
        })
        .response
}

/// Backing state for the always-visible "download a video" form on the Videos tab.
///
/// Fields are pre-filled from the detected platform's saved defaults whenever the
/// platform changes; the user can override any of them per download.
struct VideoForm {
    url: String,
    title: String,
    tool: Tool,
    /// See [`Video::tool_binary`]. Reset alongside `tool` on a platform change.
    tool_binary: String,
    quality: String,
    output_dir: String,
    filename_template: String,
    /// `None` = "Default (per-platform)": use the snapshotted platform-default
    /// auth below. `Some(kind)` overrides it with `auth_value` for this download.
    auth_override: Option<AuthKind>,
    auth_value: String,
    /// The platform default's auth, snapshotted at pre-fill (used when
    /// `auth_override` is `None`) so every field resolves from one snapshot.
    default_auth_kind: AuthKind,
    default_auth_value: String,
    extra_args: String,
    /// Resolve and use the real stream/video title (sticky across downloads).
    auto_title: bool,
    /// Audio / subtitle track selection + chat logging (sticky across downloads,
    /// like `auto_title` — not reset when the platform changes). See [`Video`].
    audio_tracks: String,
    subtitle_tracks: String,
    chat_log: bool,
    /// Platform the form is currently filled for; a change triggers a re-fill.
    last_platform: Option<Platform>,
}

impl VideoForm {
    fn new() -> VideoForm {
        VideoForm {
            url: String::new(),
            title: String::new(),
            tool: Tool::YtDlp,
            tool_binary: String::new(),
            quality: "best".into(),
            output_dir: String::new(),
            filename_template: "{name}_{date}_{time}".into(),
            auth_override: None,
            auth_value: String::new(),
            default_auth_kind: AuthKind::Inherit,
            default_auth_value: String::new(),
            extra_args: String::new(),
            auto_title: false,
            // Archive all audio + subtitle tracks by default (matches new
            // monitors); chat is the niche extra, opt-in per download.
            audio_tracks: "all".into(),
            subtitle_tracks: "all".into(),
            chat_log: false,
            last_platform: None,
        }
    }
}

// `pub(crate)`: auto-play Follow raid (`downloader::raid_follow`) constructs
// a minimal instance of this directly (a couple of named fields + `..Default::default()`)
// rather than depending on the UI's live one.
#[derive(Default)]
pub(crate) struct SettingsForm {
    twitch_client_id: String,
    twitch_client_secret: String,
    /// Google OAuth client (TV/device type) for "Connect YouTube" → subscriptions
    /// import. Separate from the YouTube Data API key (which can't read user data).
    google_client_id: String,
    google_client_secret: String,
    youtube_api_key: String,
    /// Per-operation opt-ins to use the YouTube Data API key instead of scraping
    /// (each costs quota — see the Settings section).
    youtube_api_detect: bool,
    youtube_api_schedule: bool,
    /// Daily quota cutoff for the YouTube Data API (units). Empty = default (9000).
    youtube_api_quota_cutoff: String,
    /// Daily search.list query cutoff (queries). Empty = default (90).
    youtube_search_quota_cutoff: String,
    kick_client_id: String,
    kick_client_secret: String,
    default_output_dir: String,
    /// Default output folder for on-demand video downloads (Videos tab /
    /// Recover VOD) — seeds `DownloadDefaults`' per-platform output dirs,
    /// separate from `default_output_dir` (live stream recordings).
    default_video_output_dir: String,
    /// Dedicated chat-log root folder ([`crate::chat::K_CHAT_ROOT`]): chat
    /// sidecars are written under `{root}\{drive}\{output-dir path}\` instead
    /// of next to the recordings, moving chat I/O off the capture drives.
    /// Empty = sidecars next to recordings (the default).
    chat_log_root: String,
    max_concurrent_downloads: String,
    /// VOD/video download rate limit (yt-dlp `--limit-rate` syntax, e.g. `4M`);
    /// empty = unlimited (the default). Never applied to live captures.
    download_rate_limit: String,
    capture_cache_root: String,
    /// yt-dlp `--postprocessor-args` specs (`;;`-separated); empty = none.
    /// Throttles yt-dlp's internal ffmpeg passes (e.g. the SABR merge).
    ytdlp_ppa: String,
    /// Global download-auth default: "none" or "cookies".
    download_auth_method: String,
    /// Browser to read cookies from (yt-dlp `--cookies-from-browser`).
    cookies_browser: String,
    /// Optional browser profile/session (the part after `browser:`).
    cookies_profile: String,
    /// YouTube WebSub VPS relay (yt-websub) — base URL, bearer token, poll secs.
    websub_vps_url: String,
    websub_token: String,
    websub_poll_secs: String,
    /// When to probe captures for the {resolution}/{fps}/… filename variables.
    filename_media_info: MediaInfoMode,
    /// How dates/timestamps are displayed throughout the UI.
    date_fmt: DateFmt,
    /// chrono format string used for the compact timestamp mode (K_SHORT_TS_FMT).
    short_ts_fmt: String,
    /// Which calendar granularity the Schedule tab opens to on launch.
    schedule_default_view: ScheduleMode,
    /// Global extra arguments prepended to every yt-dlp invocation (all monitors).
    /// Per-monitor extra_args are appended after these, so they take precedence.
    ytdlp_default_args: String,
    /// Explicit path to the system yt-dlp binary; empty ⇒ `yt-dlp` on PATH.
    ytdlp_binary_path: String,
    /// Path to the SABR dev-build yt-dlp (for YouTube capture-from-start). Empty ⇒
    /// SABR disabled (capture-from-start uses the system binary's normal path).
    sabr_binary_path: String,
    /// Master toggle: use the SABR build for YouTube capture-from-start.
    sabr_enabled: bool,
    /// SABR format selector + extractor-args preset.
    sabr_format: String,
    sabr_extractor_args: String,
    /// Experimental deep-rewind toggle (appends enable_live_deep_rewind=true).
    sabr_deep_rewind: bool,
    /// Manual raw SABR args; non-empty overrides the format+extractor-args preset.
    sabr_raw_args: String,
    /// PO-token-provider `--extractor-args` (e.g. bgutil) for the SABR command.
    sabr_pot_args: String,
    /// Retry a PO-rejected take via the no-token `tv` client (stored as the
    /// client name in `K_SABR_PO_FALLBACK`, "" = disabled).
    sabr_po_fallback: bool,
    /// GLOBAL default video codec/quality preference + its raw `-S` (when Custom).
    sabr_codec_pref: SabrCodecPref,
    sabr_codec_custom: String,
    /// DASH-companion format selector used by dual (SABR+DASH) capture.
    dash_format: String,
    /// Managed bgutil GVS PO token server: auto-launch at startup, the built
    /// server's directory (holds `main.js`), and the node binary to run it
    /// with. Empty path fields fall back to the module defaults.
    pot_server_autostart: bool,
    pot_server_dir: String,
    pot_server_node: String,
    /// Discord user token + whether to import stream schedules from Discord events
    /// (opt-in; automating a user token is against Discord's ToS).
    discord_token: String,
    discord_schedule: bool,
    /// Image→schedule OCR pipeline (shells out to an LLM CLI). `ocr_command` is the
    /// executable; `ocr_model`/`ocr_fallback_model` the primary + retry models;
    /// `ocr_timezone`/`ocr_offset` the timezone/UTC offset to assume for banner
    /// times. Empty fields fall back to the built-in defaults.
    ocr_command: String,
    ocr_model: String,
    ocr_fallback_model: String,
    ocr_timezone: String,
    ocr_offset: String,
    /// Per-call USD budget cap passed as `--max-budget-usd` (empty = no cap).
    ocr_max_budget: String,
    /// Process timeout in seconds (empty/0 = default 150 s).
    ocr_timeout_secs: String,
    /// Effort level passed as `--effort` (empty = omit; low/medium/high/xhigh/max).
    ocr_effort: String,
    /// File path to a PNG used as the main icon in crash and freeze dialogs.
    /// Empty = standard Windows error/warning icon. Requires a restart to take effect.
    dialog_icon: String,
    /// File path to an image replacing the built-in app icon (window/taskbar,
    /// tray, toast attribution). Empty = built-in icon. Applies on save.
    app_icon: String,
    /// Global "go to the next schedule source when an event has no title" toggle:
    /// after a winner is found, keep querying lower-priority sources to fill in
    /// blank titles (e.g. a Twitch schedule with times but no titles).
    schedule_title_fill: bool,
    /// How many recent YouTube community posts to scan for a schedule image
    /// (backlog depth). Empty = built-in default (5). Per-channel override in
    /// channel Properties.
    youtube_community_max_posts: String,
    // --- Remux embedding options ---
    /// Embed the thumbnail sidecar as MKV cover art on remux.
    remux_embed_thumbnail: bool,
    /// Embed a title metadata tag in the MKV on remux.
    remux_embed_title: bool,
    /// Template used to generate the MKV title tag.
    remux_title_template: String,
    /// Embed subtitle sidecar files as MKV subtitle streams on remux.
    remux_embed_subs: bool,
    /// Post-processing disk throttle: ffmpeg `-readrate` multiplier for
    /// finalize remuxes/concats/embeds (0 = unthrottled).
    postproc_readrate: f64,
    /// Write 1 s I/O-monitor samples to a JSONL log under the appdata logs
    /// dir (default on — post-mortems need the data to already exist).
    iomon_sample_log: bool,
    // --- File management ---
    /// Split output files into per-type subdirectories.
    file_split_enabled: bool,
    file_split_videos: String,
    file_split_subs: String,
    file_split_chat: String,
    file_split_thumbs: String,
    file_split_logs: String,
    /// Checkbox for "fetch missing thumbnails" in the Maintenance section.
    fetch_thumb_embed: bool,
    /// Selected preset template for the "Set filename default" row in Maintenance.
    maintenance_filename_preset: String,
    /// Apply the preset to all existing monitors when "Set as Default" is clicked.
    maintenance_apply_all: bool,
    /// Path to the media player binary (e.g. `C:\Progs\mpv\mpv.exe`).
    media_player_path: String,
    /// Window-title template for "Play stream (live edge)" — same
    /// falls-back-to-default-on-empty convention as `media_player_path`.
    live_title_template: String,
    /// Auto-push title/game changes to an already-running live-edge player
    /// via mpv's IPC socket (only for the launch paths this app spawns mpv
    /// directly for — see `ui::player::apply_live_title_and_spawn_updater`).
    live_title_auto_update: bool,
    /// Mute every collab-partner instance opened by "Play all collab
    /// instances (live edge)" — the clicked-on instance itself always keeps
    /// its normal audio. mpv only (same gate as the title auto-update).
    mute_collab_instances: bool,
    /// Window-title template for a collab partner that ISN'T locally tracked
    /// (played via a synthetic instance — see
    /// `ui::player::spawn_play_collab_partner`) — separate from
    /// `live_title_template` since such a partner has no known title/game to
    /// fill those tokens with. Only `{channel}` is meaningful here.
    collab_untracked_title_template: String,
    /// Auto-compress viewer-history samples older than this many days into
    /// 10-minute buckets (`0` = off, keep full resolution forever). Persisted
    /// immediately as `viewer_history_downsample_days`.
    viewer_downsample_days: i64,
    /// Branded casing (H.264, AAC, YouTube…) for machine-value filename
    /// tokens; false = as reported by the tools (h264, aac, youtube).
    token_style_branded: bool,
    /// Raw `value=Text` / `kind:value=Text` override lines for token values.
    token_overrides: String,
    // --- Twitch VOD recovery ---
    /// Re-fetch a live capture's lost segments (sequence gaps) from the VOD
    /// CDN automatically, while the stream is still running. Default on.
    gap_recover: bool,
    /// Splice recovered gap patches into the take's main file once they've
    /// settled, so the result is gapless. Default on — every individual
    /// splice still passes its own safety-check chain regardless (codec
    /// match, a trustworthy PTS anchor, post-splice verification); this
    /// only gates whether the attempt happens at all.
    gap_splice: bool,
    /// What happens to the pre-splice original + consumed patch files after
    /// a successful gapless splice. Default Keep — nothing auto-deleted.
    gap_splice_cleanup: crate::disposal::GapSpliceCleanup,
    /// Let the capture-cache sweep remove a leftover working-dir capture once
    /// its finished archive copy is ffprobe-verified to be at least as long.
    /// Default on; the only sweep rule that may touch a real capture.
    cache_drop_redundant: bool,
    /// Embed chapter markers into finalized recordings (title/category
    /// changes, raids, recovered/muted gap-splice segments). Default on —
    /// global default for the 3-level chain (channel/instance override via
    /// [`crate::chapters::ChaptersScope`]).
    chapters_enabled: bool,
    /// Which event kinds currently produce chapters — flat, global-only
    /// (see [`crate::chapters::chapter_kinds`]). All default on.
    chapters_title: bool,
    chapters_category: bool,
    chapters_raid: bool,
    chapters_recovered_segments: bool,
    chapters_muted_segments: bool,
    /// Minimum raid party size to get its own chapter (default 50).
    chapters_raid_min_viewers: String,
    /// Global default seconds a title change and a category change may land
    /// apart and still merge into one chapter (default 30). Overridable per
    /// channel/instance — see [`crate::chapters::ChaptersScope::coalesce_secs`].
    chapters_coalesce_secs: String,
    /// Auto-recover a Twitch VOD when the VOD checker finds it DMCA-muted.
    auto_recover_muted: bool,
    /// Auto-recover a Twitch VOD when the VOD checker finds it was never published.
    auto_recover_deleted: bool,
    /// Off by default: automatically finish a 👁 "seen live, Auto was off"
    /// row the moment its session closes, AND periodically scan each
    /// platform for broadcasts with no local trace at all. See
    /// `crate::downloader::vod::K_AUTO_BACKFILL_MISSED`.
    auto_backfill_missed: bool,
    /// Newline/comma CDN host override (empty = built-in list).
    recovery_cdn_hosts: String,
    /// Default recovery quality (empty/`chunked` = source, else e.g. `720p60`).
    recovery_quality: String,
    /// Concurrent-HEAD cap for the CDN probes (empty = default 8).
    recovery_max_conc: String,
    // --- Twitch ad-break detection ---
    /// Poll the live Twitch manifest directly for ad-break markers, alongside
    /// streamlink's own (fragile) log line. Default on; never affects the
    /// capture itself.
    ad_probe: bool,
    // --- Chat without recording ---
    /// Keep logging chat for a broadcast that isn't being recorded (Auto-record
    /// off). Default on — chat is a few MB and, unlike the video, can't be
    /// fetched back after the stream ends. Still gated by the instance's own
    /// "Log chat" toggle.
    chat_log_without_recording: bool,
    // --- Post-stream VOD download (global defaults for the 3-level chain) ---
    /// Download the platform's published VOD after a stream ends (alongside).
    vod_dl_enabled: bool,
    /// Replace the live recording with the VOD when the download succeeds.
    vod_dl_replace: bool,
    // --- Head backfill on new takes (global defaults for the 3-level chain) ---
    /// Fetch a fresh, full head backfill for a later take (a reconnect
    /// mid-broadcast), not just the stream's first take. Default on.
    head_backfill_fetch_new_take: bool,
    /// Restart a young Twitch `best` capture when a better rendition appears
    /// after join (Twitch lists the source quality late). Default on.
    quality_upgrade_restart: bool,
    /// Once a fresh head passes its integrity checks, delete older takes'
    /// now-redundant head files for the same stream. Default on.
    head_backfill_replace_old: bool,
    // --- Automatic deletion (global defaults for the 3-level chain) ---
    /// What happens to the head/live parts once a verified full.mkv join
    /// lands. Default keeps both (the opt-in is choosing a delete variant).
    join_cleanup: crate::disposal::JoinCleanup,
    /// How automatic media deletions are executed (trash folder / Recycle
    /// Bin / permanent). Default Recycle Bin.
    disposal_method: crate::disposal::DisposalMethod,
    /// `;`-separated trash folder list, one per drive (same-drive moves only).
    disposal_trash_dirs: String,
    /// `{drive}`-templated fallback trash root applied to any drive not
    /// explicitly listed in `disposal_trash_dirs`. Empty = no default.
    disposal_trash_default_root: String,
    /// Global default for rolling-recording mode: are captures auto-deleted a
    /// set time after they finish unless kept? Default **off**. See
    /// [`crate::rolling`].
    rolling_enabled: bool,
    /// Global default rolling TTL in **hours** as typed; empty falls back to
    /// [`crate::disposal::DEFAULT_ROLLING_TTL_SECS`] (one week).
    rolling_ttl_hours: String,
    // --- Follow raid (global defaults for the 3-level chain) ---
    /// Does raiding out from a monitored channel ever trigger an auto-RECORD
    /// of the target? Default OFF — opt-in, unlike most toggles here, since
    /// it creates new recordings of channels the user didn't curate.
    /// Single-hop only — records until the raid target's own stream ends;
    /// chain-following (the target itself raiding out further) isn't
    /// implemented yet.
    raid_follow_record: bool,
    /// Does raiding out from a monitored channel ever trigger an auto-PLAY
    /// (live-edge player, no recording) of the target? Independent of
    /// `raid_follow_record` — the automatic equivalent of the manual "▷🏃
    /// Follow raid" button. Default OFF, single-hop only, same as above.
    raid_follow_play: bool,
    /// Gate auto-play on the raiding instance having been open in a player
    /// this app launched (still open, or closed within the last ~10 min).
    /// Default ON — the guard against unexplained player windows popping up
    /// for raids of streams nobody was watching.
    raid_follow_play_only_watched: bool,
    /// Output directory for an UNTRACKED raid target's ad-hoc capture
    /// (supports the `{name}` token).
    raid_follow_output_dir: String,
    /// Skip auto-recording a tracked raid target that's currently disabled
    /// (master switch off). Default on. Auto-play has no equivalent of this
    /// — see `crate::raid_follow::is_excluded_from_auto_play` instead.
    raid_skip_disabled_targets: bool,
    /// Global trigger-word rules (start recording on title/game match even with
    /// Auto off). Channel/instance Properties can extend/replace/disable them.
    trigger_rules: Vec<crate::triggers::TriggerRule>,
    /// Global blacklist trigger rules (PREVENT automatic recording on title/game
    /// match; manual Start still records). Same scope inheritance as above.
    trigger_block_rules: Vec<crate::triggers::TriggerRule>,
    /// Deletion method applied to every trigger-started take that doesn't set
    /// its own per-rule override (edited on the rule itself, in
    /// `trigger_rules` — meaningless for a blacklist rule, which never starts
    /// a recording). `None` means trigger-started takes get no special
    /// treatment — the normal monitor/channel/global chain applies. Beats the
    /// monitor/channel overrides whenever it does apply; the per-rule
    /// override beats this in turn.
    trigger_disposal_default: Option<crate::disposal::DisposalMethod>,
    /// User-defined alternate yt-dlp-compatible binaries (alias + path),
    /// selectable alongside the system yt-dlp / SABR build in the Videos-tab
    /// download form.
    custom_tools: Vec<crate::downloader::CustomTool>,
    /// Default concurrent local full-file ffmpeg passes per disk (min 1).
    /// The ceiling when `disk_default_dynamic` is on, the fixed value otherwise.
    disk_default_local: u32,
    /// Default concurrent CDN-fed muxes per disk (min 1). Same ceiling
    /// semantics as `disk_default_local` when dynamic mode is on.
    disk_default_cdn: u32,
    /// Default disk-gate dynamic mode: adapt permits to live disk activity
    /// instead of holding a fixed count. Per-drive overrides carry their own
    /// `dynamic` bit on `DiskLimits` directly.
    disk_default_dynamic: bool,
    /// Default disk-gate emergency pause: block new `local_pass` admissions
    /// (concat/remux/embeds) on every drive without its own override row.
    /// Per-drive overrides carry their own `paused` bit on `DiskLimits`
    /// directly. Usually flipped from the Background tab's quick toggle,
    /// not here — this field exists so a deliberate, persisted pause
    /// survives a restart too.
    disk_default_paused: bool,
    /// Per-drive I/O limit overrides: (drive letter, limits). The default
    /// readrate/rate-limit live in `postproc_readrate`/`download_rate_limit`.
    disk_overrides: Vec<(String, crate::io_gate::DiskLimits)>,
    // --- Chat index ---
    /// Master switch for the chat index (`crate::chat_index`). Default on;
    /// off stops every read and write it does, immediately.
    chat_index_enabled: bool,
    /// Takes indexed per sweep (parsed on save; empty/invalid falls back to
    /// the module default).
    chat_index_batch: String,
    // --- Rolling database backups ---
    /// Periodically `VACUUM INTO` a timestamped snapshot of the live database
    /// (see `crate::db_backup`). Default on — a safety net, not an opt-in.
    db_backup_enabled: bool,
    /// Hours between backups (parsed on save; empty/invalid falls back to
    /// `db_backup::DEFAULT_INTERVAL_HOURS`).
    db_backup_interval_hours: String,
    /// How many rolling snapshots to keep before pruning the oldest (parsed
    /// on save; empty/invalid falls back to `db_backup::DEFAULT_RETENTION_COUNT`).
    db_backup_retention_count: String,
}

impl SettingsForm {
    /// Minimal snapshot for contexts outside the `ui` module that only need
    /// player-launch fields — auto-play Follow raid
    /// (`downloader::raid_follow`) builds one of these rather than depending
    /// on the UI's live `SettingsForm`. Everything else defaults; only
    /// `live_title_template`/`live_title_auto_update` are read by
    /// `player::spawn_play_new_instance`'s Twitch/generic branches for this
    /// launch path (`media_player_path` is passed as its own separate
    /// argument, not read off the form).
    pub(crate) fn for_auto_play(store: &crate::store::Store) -> SettingsForm {
        SettingsForm {
            live_title_template: store
                .get_setting(K_LIVE_TITLE_TEMPLATE)
                .ok()
                .flatten()
                .unwrap_or_default(),
            live_title_auto_update: live_title_auto_update_setting(store),
            ..Default::default()
        }
    }
}

/// Lazy cache of decoded post images, keyed by content hash: an egui texture +
/// its pixel dimensions, or `None` when the decode failed.
type PostImageCache = HashMap<String, Option<(egui::TextureHandle, (u32, u32))>>;

pub struct StreamArchiverApp {
    core: Arc<AppCore>,
    /// Kept alive for the app's lifetime (dropping it removes the tray icon);
    /// also re-iconed live when the custom app icon setting changes.
    tray: TrayIcon,
    ui_rx: Receiver<UiCommand>,
    events_rx: crate::events::EventRx,
    autostart: AutoStart,
    autostart_on: bool,
    /// When false (the default), quitting detaches downloads so they keep running
    /// across a restart/rebuild; when true, quitting stops them. Persisted as the
    /// `stop_downloads_on_quit` setting (stored inverted).
    keep_downloads_on_quit: bool,
    /// Show desktop notifications (toasts) on recording start/finish/error.
    /// Persisted as the `notifications_enabled` setting; default on.
    notifications_enabled: bool,
    /// Subscribe EventSub shared-chat ("Stream Together") events for instant
    /// collab updates — conduit mode only (WebSocket transport's cost cap of
    /// 10 can't afford 3 extra types/channel). Persisted as `collab_eventsub`;
    /// default on. Polling covers collabs either way; this only speeds it up.
    collab_eventsub: bool,
    /// Subscribe EventSub `channel.raid` (both directions) so raids land in
    /// the Channel Stats event history even while not recording — conduit
    /// mode only, same cost-cap reasoning. Persisted as `raid_eventsub`;
    /// default on. Chat still captures incoming raids while recording.
    raid_eventsub: bool,
    /// Parse the stream title for `@handle` mentions as a lower-confidence
    /// collab signal alongside confirmed Shared Chat/group partners (never
    /// duplicating one already found there). Persisted as
    /// `collab_title_mentions`; default on. See
    /// [`crate::detectors::DetectContext::refresh_twitch_collab`].
    collab_title_mentions: bool,
    /// Show title-`@mention` collab partners in the Name-cell " × Partner"
    /// suffix too (as " × @Name", `@`-prefixed to stay visually distinct
    /// from confirmed Shared Chat/group partners) instead of only in the 🤝
    /// Collab column. Persisted as `collab_title_mentions_in_name`; default
    /// on. Purely a display toggle — doesn't affect detection.
    collab_title_in_name: bool,
    /// Do Not Disturb: manually suppress toasts right now. Persisted as
    /// `dnd_enabled`; default off. See [`crate::notifications::dnd_active`].
    dnd_enabled: bool,
    /// Also suppress toasts automatically during `dnd_start`-`dnd_end` each
    /// day. Persisted as `dnd_schedule_enabled`; default off.
    dnd_schedule_enabled: bool,
    /// `"HH:MM"` local time the scheduled DND window begins/ends (a start
    /// later than the end spans midnight). Edited live in Settings; only
    /// persisted once both parse as valid times.
    dnd_start: String,
    dnd_end: String,
    /// Global default preferred platform when a channel has more than one
    /// instance simultaneously live (`None` = earliest-live-wins, the prior
    /// behavior). Persisted as `primary_platform_pref`; overridable per
    /// channel/instance — see [`crate::platform_pref`].
    primary_platform_pref: Option<Platform>,
    /// Global simulcast-dedup preference: which platform to record when a
    /// channel is live on several at once, and the platform that overrides it
    /// when an instance there is ad-free. Both `Off` by default (dedup
    /// disabled) and overridable per channel/instance — see
    /// [`crate::simulcast`]. Distinct from `primary_platform_pref` above,
    /// which is display-only.
    simulcast_pref: crate::simulcast::SimulcastPref,
    simulcast_ad_free_pref: crate::simulcast::SimulcastPref,
    /// The settle window as typed (minutes; empty = the module default). Only
    /// persisted — in seconds — when it parses to something positive.
    simulcast_settle_mins: String,
    /// The process-manager dialog: whether it's open, its last snapshot, and when
    /// that snapshot was taken (throttles the per-row `pid_alive`/DB queries).
    show_processes: bool,
    processes: Vec<crate::app_core::ProcInfo>,
    processes_refreshed: Option<std::time::Instant>,
    /// In-flight background load of the process list (spawned off the UI thread
    /// to avoid blocking on the store mutex during `list_processes()`).
    processes_load: Option<std::sync::mpsc::Receiver<Vec<crate::app_core::ProcInfo>>>,
    /// Deferred-viewport state while the Processes window is open (None =
    /// closed) — see [`dialogs::ProcessesPopupState`].
    processes_popup: Option<Arc<Mutex<dialogs::ProcessesPopupState>>>,
    /// The issues panel: whether it's open, its last snapshot of recordings
    /// that still have a `.ts` path, and when that snapshot was taken.
    show_issues: bool,
    issues_recs: Vec<crate::models::Recording>,
    issues_missing: Vec<crate::models::Recording>,
    /// Failed/aborted/orphaned recordings that have an output file on disk (or no path at all).
    issues_errors: Vec<crate::models::Recording>,
    /// Failed/aborted/orphaned recordings whose output path is set but the file is gone from disk.
    /// Partitioned out of issues_errors at load time; rendered alongside the missing-file section.
    issues_errors_no_file: Vec<crate::models::Recording>,
    /// Completed recordings whose promote-to-output-dir move never finished (a
    /// non-`.ts` file, e.g. a SABR/DASH `.mkv`, still sitting in `.cache\`) —
    /// most commonly because the filename overflowed the filesystem's length
    /// limit. Distinct from issues_recs (a `.ts` awaiting a remux).
    issues_stuck: Vec<crate::models::Recording>,
    /// Recordings whose published VOD came back DMCA-muted (post-stream archive) —
    /// un-muted via recovery, awaiting acknowledgement.
    issues_muted_vod: Vec<crate::models::MutedVodIssue>,
    /// Takes that finalized 0-byte / file-gone but whose media SURVIVED as
    /// split per-format files in `.cache\` (the tool died before its own
    /// merge) — recoverable, so never shown as plain "gone". Each entry
    /// carries the discovered part files.
    issues_unmerged: Vec<(crate::models::Recording, Vec<std::path::PathBuf>)>,
    /// Head backfills that can't be losslessly joined with their live capture
    /// (codec/resolution mismatch), with display strings: (rec, head params,
    /// live params).
    issues_head_mismatch: Vec<(crate::models::Recording, String, String)>,
    /// Recordings whose gap-splice attempt was blocked by a safety check
    /// (codec mismatch, an untrustworthy PTS anchor, or a failed post-splice
    /// verification) — see [`crate::models::Recording::gap_splice_state`].
    issues_gap_splice: Vec<crate::models::Recording>,
    /// Rows still marked `recording` whose files have gone quiet (capture died
    /// unnoticed, or the finalize is pending) + seconds since the last write
    /// (`None` = nothing on disk).
    issues_stale_recording: Vec<(crate::models::Recording, Option<i64>)>,
    /// In-flight background Issues scan (see [`IssuesScan`]). Every
    /// `path.exists()`/ffprobe the Issues panel needs runs on this thread —
    /// against the recordings drive a single stat can block for seconds, so
    /// the UI thread must never do one (see `FsProbes`).
    issues_missing_load: Option<std::sync::mpsc::Receiver<IssuesScan>>,
    issues_refreshed: Option<std::time::Instant>,
    /// A dirty-marking app event landed since the last issues sweep — shortens
    /// the closed-panel refresh interval instead of forcing an immediate one.
    issues_dirty: bool,
    issues_confirm_clear: bool,
    /// Deferred-viewport state while the Issues window is open (None =
    /// closed) — see [`issues::IssuesPopupState`]. Replaces the old separate
    /// `issues_error_view` (now `IssuesPopupState::issues_error_view`).
    issues_popup: Option<Arc<Mutex<issues::IssuesPopupState>>>,
    /// The notifications feed window: whether it's open, its last-loaded rows,
    /// the throttle timestamp, an off-thread load, the cached unread count (the
    /// header badge), and the session-only category + text filters.
    show_notifications: bool,
    notifications: Vec<crate::store::NotificationRow>,
    /// The GVS PO token server log window: whether it's open, plus the tail
    /// text and its off-thread refresh state (reloaded ≤1/s while open).
    show_pot_server_log: bool,
    pot_log_text: String,
    pot_log_refreshed: Option<std::time::Instant>,
    /// Deferred-viewport state while the PO token server log window is open
    /// (None = closed) — see [`pot_log::PotLogPopupState`].
    pot_log_popup: Option<Arc<Mutex<pot_log::PotLogPopupState>>>,
    /// The 🖹 Log view: a live, filterable window over the app's own tracing
    /// output (`crate::log_capture`). Unlike the other popups here it has no
    /// off-thread refresh throttle — its data source is an in-memory ring
    /// buffer, cheap enough to poll every frame — so there's nothing to
    /// track beyond whether it's open and its deferred-viewport state.
    show_log_view: bool,
    log_view_popup: Option<Arc<Mutex<log_view::LogViewPopupState>>>,
    notif_refreshed: Option<std::time::Instant>,
    notif_unread: i64,
    /// Unread `youtube_post`-kind notifications — the 📣 Posts tab badge.
    /// Refreshed on the same throttle as `notif_unread` (both come off the
    /// 🔔 feed's `notification` table); see `notifications_window`.
    posts_unread: i64,
    notif_search: String,
    notif_kind_filter: Option<crate::models::NotificationKind>,
    /// Deferred-viewport state while the Notifications window is open (None =
    /// closed) — see [`issues::NotificationsPopupState`].
    notifications_popup: Option<Arc<Mutex<issues::NotificationsPopupState>>>,
    /// The 🚨 Warnings window (capture alerts scraped from tool logs): open
    /// flag, last-loaded rows, refresh throttle, cached unacked badge counts
    /// `(errors, warnings)`, and session-only text/severity filters.
    show_warnings: bool,
    warnings_rows: Vec<crate::store::CaptureAlertRow>,
    /// recording_id → alert rollup for the Streams-grid take/stream badges
    /// (refreshed on the same throttle as the 🚨 badge counts).
    rec_alert_badges: std::collections::HashMap<i64, crate::store::RecAlertBadge>,
    warn_refreshed: Option<std::time::Instant>,
    warn_badge: (i64, i64),
    /// Files still sitting in a trash folder (restorable, and still costing
    /// disk) — the 🗑 tab badge. Cached on the same throttle as `warn_badge`.
    trash_badge: i64,
    warn_search: String,
    /// `None` = both severities; `Some(true)` = errors only, `Some(false)` =
    /// warnings only.
    warn_sev_filter: Option<bool>,
    /// Hide acknowledged rows in the Warnings window (session-only, not
    /// persisted — the window always opens showing everything).
    warn_hide_acked: bool,
    /// 🚨 Warnings window: paint rows in their severity/state colours
    /// (red/yellow/green tints). Persisted (`warnings_row_bgcolor`, default
    /// on); off = plain rows, the accent-coloured icons/titles still carry
    /// the state.
    warn_bgcolor: bool,
    /// Deferred-viewport state while the Warnings window is open (None =
    /// closed) — see [`issues::WarningsPopupState`].
    warnings_popup: Option<Arc<Mutex<issues::WarningsPopupState>>>,
    /// 🔔 Notifications window: paint rows in their per-kind colours.
    /// Persisted (`notif_row_bgcolor`, default on) — same idea as the
    /// Warnings toggle above.
    notif_bgcolor: bool,
    /// The YouTube posts feed (a top-level tab AND a pop-out window sharing one
    /// render fn): loaded rows, load throttle, session-only channel + text
    /// filters, and a lazy visible-only texture cache keyed by content hash.
    show_posts_window: bool,
    /// Deferred-viewport state while the pop-out Posts window is open (None =
    /// closed) — see [`posts::PostsPopupState`].
    posts_popup: Option<Arc<Mutex<posts::PostsPopupState>>>,
    /// "🚫 Excluded channels…" management window: which channels' posts are
    /// hidden from the feed (`Channel::posts_hidden`, session filter text).
    show_posts_excluded: bool,
    posts_excluded_search: String,
    posts: Vec<crate::store::CommunityPostRow>,
    posts_refreshed: Option<std::time::Instant>,
    posts_search: String,
    posts_channel_filter: Option<i64>,
    /// Whether the posts feed also shows viewer posts (fans posting in the
    /// channel's Community space). Off by default — session-only, like the
    /// other feed filters.
    posts_show_viewer: bool,
    /// How many of the filtered posts to actually lay out this frame. The feed
    /// can hold up to 500 rows, each a rich multi-widget card (links, N
    /// images) — laying all of them out every frame regardless of scroll
    /// position is the main cost of the tab, so only this many render up
    /// front; a "Show more" button at the bottom raises it. Session-only,
    /// reset to the default whenever the filter/search narrows the visible set.
    posts_render_limit: usize,
    /// A single post the feed has been narrowed to, by `post_id` — set by the
    /// 🔔 notifications feed's "View post" button so the post it names can't be
    /// hidden by whatever the Posts window was last filtered to. Overrides the
    /// channel/search/viewer filters entirely while set; the "✕ Show all"
    /// button (and any filter edit) clears it.
    posts_focus_post: Option<String>,
    post_img_cache: Arc<Mutex<PostImageCache>>,
    /// The widget inspector (F12): whether the window is open (session-only,
    /// like the other window flags) and its tab/selection/snapshot state.
    show_inspector: bool,
    inspector: Arc<Mutex<crate::inspector::InspectorState>>,
    quitting: bool,
    /// UI-freeze watchdog heartbeat: stamped each frame so a background thread can
    /// detect (and surface as a native dialog) a hung UI thread. See [`crate::watchdog`].
    heartbeat: crate::watchdog::Heartbeat,
    /// One-shot startup self-heal (see `logic()`): eframe/winit can capture
    /// a MINIMIZED window's degenerate geometry (0×0 inner size — Windows
    /// reports a minimized window's client area as zero) if the app was last
    /// closed while minimized, then restores that on the next launch, only
    /// floored to a generic 64×64 (not this app's real usable minimum) —
    /// producing exactly the "spawns as a sliver, resizing snaps it back"
    /// bug this flag guards against. `false` until the fix has been checked
    /// (and applied, if needed) once.
    startup_window_size_checked: bool,

    view: View,
    /// Help/About view state, built lazily on first open (parses the embedded
    /// README once).
    help: Option<HelpState>,
    /// Previous-frame top-bar measurements for the tab-overflow algorithm.
    topbar: TopBarLayout,
    rows: Vec<MonitorWithChannel>,
    /// All channel containers (incl. empty ones), for the Streams tree.
    channels: Vec<Channel>,
    /// All channel groups, alphabetical — feeds the channel form's group
    /// pickers, the Manage Groups dialog, and the Streams grid's group
    /// header/filter. Reloaded in `reload_rows` and after any group CRUD.
    channel_groups: Vec<crate::models::ChannelGroup>,
    videos: Vec<Video>,
    form: Option<Arc<Mutex<MonitorForm>>>,
    video_form: VideoForm,
    /// Per-platform download defaults editable on the Videos tab (persisted JSON).
    download_defaults: DownloadDefaults,
    /// Per-platform monitor-creation defaults editable in Settings (persisted JSON).
    monitor_defaults: MonitorDefaults,
    /// Active Settings category tab (persisted via `K_SETTINGS_TAB`).
    settings_tab: SettingsTab,
    /// Settings search-box query — when non-empty, matching sections across all
    /// categories are shown instead of the selected tab.
    settings_search: String,
    /// Shared state of the async "List formats" probe (Videos tab).
    format_probe: Arc<Mutex<FormatProbe>>,
    /// Deferred-viewport state for `format_probe_window` (None = closed).
    format_probe_popup: Option<Arc<Mutex<videos::FormatProbePopupState>>>,
    /// Deferred-viewport state for the "🖌 Custom…" layout editor
    /// (`layout_editor_window`), `None` = closed.
    layout_editor: Option<Arc<Mutex<layout_editor::LayoutEditorPopupState>>>,
    /// Backing state for the "Recover VOD" dialog (`None` = closed).
    recover_form: Option<Arc<Mutex<RecoverVodForm>>>,
    /// Shared state of the async Recover-VOD CDN probe.
    recover_probe: Arc<Mutex<RecoverProbe>>,
    /// Shared state of the async "Parse URL" start-time scrape.
    recover_scrape: Arc<Mutex<RecoverScrape>>,
    settings: SettingsForm,
    status: String,
    /// Monitor id of the currently selected row (target for keyboard shortcuts).
    selected_monitor: Option<i64>,
    /// Pending instance-delete confirmation: (monitor id, channel name).
    confirm_delete: Option<Arc<Mutex<ConfirmDialogState<(i64, String)>>>>,
    /// Pending channel-delete confirmation: (channel id, name).
    confirm_delete_channel: Option<Arc<Mutex<ConfirmDialogState<(i64, String)>>>>,
    /// "Move instance to another channel" dialog: `(monitor id, chosen
    /// destination channel id)`. The destination lives here (not a per-frame
    /// local) so the ComboBox selection persists across frames.
    move_instance_dialog: Option<Arc<Mutex<dialogs::MoveInstanceState>>>,
    /// "Merge channel into another" dialog: `(source channel id, chosen
    /// destination channel id)`.
    merge_channel_dialog: Option<Arc<Mutex<dialogs::MergeChannelState>>>,
    /// Pending schedule-segment-delete confirmation: segment id.
    confirm_delete_segment: Option<Arc<Mutex<ConfirmDialogState<i64>>>>,
    /// Backing state for the create/rename-channel dialog.
    channel_form: Option<Arc<Mutex<ChannelForm>>>,
    /// "Manage groups" dialog: open flag, new-group name draft, and an
    /// in-progress inline rename (group id + draft text; `None` = not
    /// renaming any row).
    show_group_manager: bool,
    group_manager_new_name: String,
    group_manager_rename: Option<(i64, String)>,
    /// Recording groups' own new-name draft + inline rename, alongside the
    /// channel-group ones in the same "Manage groups" window (two sections,
    /// separate state so they can't stomp each other).
    recording_group_manager_new_name: String,
    recording_group_manager_rename: Option<(i64, String)>,
    /// All recording groups, alphabetical — feeds the "Add to group…"
    /// dialog's existing-group picker. Reloaded in `reload_rows` and after
    /// any recording-group CRUD.
    recording_groups: Vec<crate::models::RecordingGroup>,
    /// Backing state for the "Add to group…" dialog (`None` = closed).
    add_to_recording_group: Option<AddToRecordingGroupDialog>,
    /// Scheduled recordings (schema v51): the management window's open flag +
    /// last-loaded rows (refreshed in `reload_rows`, cheap — one small table),
    /// the add/edit dialog (`None` = closed), and a pending delete confirmation.
    show_scheduled_recordings: bool,
    scheduled_recordings: Vec<crate::models::ScheduledRecordingWithNames>,
    scheduled_recording_form: Option<Arc<Mutex<ScheduledRecordingForm>>>,
    confirm_delete_scheduled_recording: Option<Arc<Mutex<ConfirmDialogState<(i64, String)>>>>,
    /// Deferred-viewport state while the Scheduled recordings window is open
    /// (None = closed) — see [`schedule::SchedRecsPopupState`].
    scheduled_recordings_popup: Option<Arc<Mutex<schedule::SchedRecsPopupState>>>,
    /// Scheduled recordings window: the "+ Add new" instance-picker
    /// dropdown's selection (session-only, not persisted to the DB). Must
    /// live on `self`, not a per-frame local — a local re-initialized to
    /// the first row every frame would silently discard every click.
    /// 0 = unset (falls back to the first row).
    sched_rec_add_monitor: i64,
    /// Sort + per-column filters for the Streams table.
    streams_sort: SortState,
    streams_filters: Vec<String>,
    /// Expansion state for the Streams history tree (channel id / monitor id /
    /// stream key), and a lazy cache of recordings per expanded monitor.
    expanded_channels: HashSet<i64>,
    expanded_instances: HashSet<i64>,
    expanded_streams: HashSet<String>,
    /// Multi-selected Stream rows (keyed by `StreamGroup::key` → its take
    /// ids, ctrl/shift-click to add, plain click replaces) — feeds the "Add
    /// to group…" bulk action. Take ids are captured at click time (not
    /// re-resolved from the frame cache later, which only holds data for
    /// currently-expanded instances) so a stream added to the selection
    /// stays addable even if its instance gets collapsed before "Add to
    /// group…" is confirmed. Unlike `selected_monitor` (single, cosmetic/
    /// keyboard-shortcut target), this is a real multi-select set.
    selected_streams: HashMap<String, Vec<i64>>,
    /// Year/Month/Week grouping-header toggles — deviations from the
    /// computed default (open for the single newest bucket at each shown
    /// level, closed otherwise), not the open state itself. See
    /// `streams::period_open`.
    period_toggles: HashSet<String>,
    /// Channel-group headers the user has explicitly collapsed — presence =
    /// collapsed (a group defaults open, opposite convention from
    /// `expanded_channels` et al., since you want to see channels the moment
    /// you group them). No header renders at all — and this set is
    /// meaningless — for any channel with no `primary_group_id` set, or
    /// while `streams_group_filter` narrows to a single group.
    collapsed_channel_groups: HashSet<i64>,
    /// Streams grid's "Group" filter (toolbar dropdown): `Some(id)` narrows
    /// the channel list to that group's members (primary or secondary),
    /// bypassing the primary-group header clustering entirely.
    streams_group_filter: Option<i64>,
    /// Streams grid's "Recording group" filter (toolbar dropdown): `Some(id)`
    /// hides any channel/instance with no take in that group, and
    /// force-expands the ones that remain down to their matching streams —
    /// see `build_vis_rows`'s `recording_group_filter` param.
    streams_recording_group_filter: Option<i64>,
    /// Streams grid "Group" checkbox: show/hide the channel-group header
    /// clustering (`collapsed_channel_groups`/primary-group headers) — off
    /// yields a flat list even for channels that have a primary group
    /// assigned. Persisted immediately on toggle (own key, not part of the
    /// batched Settings form — same shape as Schedule's `schedule_compact`).
    /// Also the `group_visually` field a saved view snapshots/restores.
    streams_group_visually: bool,
    /// Streams grid "Only stored" checkbox: hide any channel/instance/stream
    /// with no take that actually has a file on disk (`Recording.output_path`
    /// non-empty) — detected-but-never-recorded and failed/missed streams
    /// disappear, same force-expand-to-matches behavior as the Recording
    /// group filter (`build_vis_rows`'s `recording_group_filter` param, which
    /// this reuses/intersects with rather than a parallel filter dimension).
    /// Persisted immediately on toggle, same shape as `streams_group_visually`.
    streams_only_recorded: bool,
    /// Streams view "Allow deletion" checkbox: master switch gating the
    /// take-row "🗑🔥 Delete file from disk…" context-menu item — OFF blocks
    /// it everywhere regardless of any channel/instance-level allowance.
    /// Persisted immediately on toggle (`manual_delete::K_STREAMS_ALLOW_DELETE`),
    /// default off. See `manual_delete` for the full three-gate design.
    streams_allow_delete: bool,
    /// Pending confirmation for a manual "Delete file from disk" — set by the
    /// take-row context-menu item, cleared on Delete/Cancel.
    confirm_delete_file: Option<Arc<Mutex<ConfirmDialogState<ConfirmDeleteFile>>>>,
    /// Pending confirmation for a bulk "Delete all take files from disk" —
    /// set by the stream-row context-menu item, cleared on Delete/Cancel.
    confirm_delete_stream_files: Option<Arc<Mutex<ConfirmDialogState<ConfirmDeleteStreamFiles>>>>,
    /// Take (recording) ids with a manual file-delete currently in flight —
    /// disables their row's action while the async disposal runs.
    manual_delete_pending: HashSet<i64>,
    /// `(rec_id, outcome)` for a finished manual delete, posted by the
    /// `core.rt.spawn`'d task — drained once per frame (see
    /// `drain_manual_delete_results`), same cross-thread shape as
    /// `trash_action_done`.
    manual_delete_done: std::sync::Arc<std::sync::Mutex<Vec<crate::manual_delete::ManualDeleteOutcome>>>,
    /// Streams grid's saved views (`crate::saved_views`): the currently-
    /// applied view's name (session-only — re-applying one is a single
    /// click, so this isn't persisted across restarts) and the last-loaded
    /// view list (reloaded after any CRUD, mirroring `recording_groups`).
    streams_active_view: Option<String>,
    streams_views: Vec<SavedView>,
    /// Backing state for the "Views" dropdown's popup body (`views_combo_popup`):
    /// the new-view-name draft, and an in-progress inline rename (old name +
    /// draft text; `None` = not renaming any row) — same shape as
    /// `group_manager_new_name`/`group_manager_rename`.
    views_manager_new_name: String,
    views_manager_rename: Option<(String, String)>,
    rec_cache: HashMap<i64, Vec<Recording>>,
    /// Lazy per-monitor per-broadcast viewer/event stats (peak/avg viewers,
    /// sub/bits/raid totals), keyed by monitor id — evicted/cleared at every
    /// site that touches `rec_cache`, since both go stale for the same
    /// reasons. Powers the take-row 👁 stats badge and the Recording
    /// Properties "Viewer stats" section; scoped per-monitor so a
    /// multi-instance channel's simultaneous captures never mix together
    /// (see `Store::stream_stats_for_monitor`).
    take_stats_cache: HashMap<i64, Vec<StreamStatRow>>,
    /// Lazy per-recording ad-break detail (cut list), keyed by recording id;
    /// cleared on reload. Avoids a per-frame DB query for tooltips/the popup.
    ad_break_cache: HashMap<i64, Vec<AdBreak>>,
    /// Recording id whose ad-break cut list is shown in a popup (None = closed).
    ad_popups: Vec<i64>,
    /// Deferred-viewport content for each open `ad_popups` entry.
    ad_popup_registry: PopupRegistry<i64, AdPopupContent>,
    /// Lazy per-recording title/category change log, keyed by recording id;
    /// cleared on reload. Same caching role as `ad_break_cache`.
    meta_change_cache: HashMap<i64, Vec<StreamMetaChange>>,
    /// What the metadata-change popup shows (None = closed): a single take or a
    /// whole stream's aggregated takes.
    meta_popups: Vec<MetaPopup>,
    /// Deferred-viewport content for each open `meta_popups` entry, keyed by
    /// [`MetaPopup::key`].
    meta_popup_registry: PopupRegistry<i64, MetaPopupContent>,
    /// Lazy per-monitor all-time title/category change ledger, keyed by
    /// monitor id; cleared on reload. Independent of any recording — see
    /// [`crate::models::MonitorStreamChange`].
    history_change_cache: HashMap<i64, Vec<MonitorStreamChange>>,
    /// Monitor id whose all-time change history is shown in a popup.
    history_popups: Vec<i64>,
    /// Deferred-viewport content for each open `history_popups` entry —
    /// created once per monitor id, dropped once its id leaves
    /// `history_popups` (see `history_popup_window`).
    history_popup_registry: PopupRegistry<i64, HistoryPopupContent>,
    /// Recording id whose embedded chapter list (stream, file path, chapter
    /// timestamps) is shown in a popup — the Background view's ℹ button on a
    /// Chapters task row.
    chapters_popups: Vec<i64>,
    /// Lazy per-recording (channel name, file path, parsed chapter list) for
    /// the chapters detail popup, keyed by recording id; cleared on reload.
    chapters_popup_cache: HashMap<i64, (String, String, Vec<crate::chapters::Chapter>)>,
    /// Deferred-viewport content for each open `chapters_popups` entry.
    chapters_popup_registry: PopupRegistry<i64, ChaptersPopupContent>,
    /// All-monitor recording history, newest-first, capped at
    /// `history_load_limit` — shared by the Backlog and Stream History
    /// views. Loaded lazily on first visit to either; see
    /// `history::ensure_history_loaded`.
    history_all: Vec<Recording>,
    history_loaded: bool,
    /// "Load more" cap for `recordings_all`; grows by 500 per click.
    history_load_limit: i64,
    /// Broadcast watch-state, keyed by `models::stream_key`/`StreamGroup::key`
    /// — reloaded alongside `history_all`. A key absent here is `"unwatched"`
    /// (see `history::effective_watch_state`).
    history_watch: HashMap<String, (String, Option<i64>)>,
    /// Stream History's checkbox filter bank (session-only, not persisted).
    history_filters: history::HistoryFilters,
    history_search: String,
    /// Backlog: which watch states are currently shown (defaults to
    /// everything but "watched" — a to-do list, not a full log).
    backlog_show_states: HashSet<String>,
    /// Recording id whose VOD-status popup is open (Stream History's ℹ VOD
    /// button) + its cached (channel name, recording) — no extra store read
    /// needed, the row already has the full `Recording`.
    vod_info_popups: Vec<i64>,
    /// Every logged automatic disposal, newest first — the Trash view. Loaded
    /// lazily on first visit and on every re-entry (`switch_view` resets
    /// `trash_loaded`); see `trash::ensure_trash_loaded`.
    trash_records: Vec<crate::store::DisposalRecordDisplay>,
    trash_loaded: bool,
    /// Case-insensitive channel-name filter for the Trash view.
    trash_filter: String,
    /// Which disposal states to show in the Trash view — a checkbox bank
    /// (not a dropdown) so more than one state can be visible at once, e.g.
    /// "In trash" + "Restored" with "Permanently deleted" hidden. All on by
    /// default. Session-only, like `trash_filter`.
    trash_show_soft_deleted: bool,
    trash_show_permanent: bool,
    trash_show_restored: bool,
    /// Set while a Restore/Permanently-delete action is running for that
    /// record id, so its row can disable its buttons and show a spinner
    /// instead of racing a second click against the same file.
    trash_action_pending: HashSet<i64>,
    /// Last Restore/Permanently-delete failure, shown as a dismissable banner
    /// (file locked, already moved by the user, etc.).
    trash_action_error: Option<String>,
    /// `(record id, outcome)` for finished Restore/Permanently-delete actions
    /// — filled from `core.rt.spawn`'d tasks (a background thread, so it
    /// can't touch `self` directly), drained on the UI thread at the top of
    /// `trash::trash_view` each frame.
    trash_action_done: Arc<Mutex<Vec<TrashActionOutcome>>>,
    /// `(record id, path)` pairs pending the "Permanently delete" confirmation
    /// dialog (an irreversible action, unlike Restore) — one row's worth for
    /// the per-row 🗑 button, or every checked row's worth for the toolbar's
    /// "Delete selected".
    confirm_permadelete_trash: Option<Arc<Mutex<ConfirmDialogState<Vec<(i64, String)>>>>>,
    /// Checked disposal-record ids in the Trash view (session-only) — backs
    /// the per-row checkbox + toolbar "Delete selected". Only ever holds ids
    /// of `SoftDeleted` rows (the only state with a delete action); pruned on
    /// every `reload_trash` so a since-restored/deleted row can't linger
    /// checked.
    trash_selected: HashSet<i64>,
    /// True while the "Import history" (`disposal_backfill`) scan is
    /// running, so the button disables and shows a spinner instead of
    /// allowing an overlapping second scan.
    trash_import_running: bool,
    /// Finished import's summary, posted from a `core.rt.spawn`'d task (a
    /// background thread) — drained on the UI thread at the top of
    /// `trash::trash_view` each frame, same shape as `trash_action_done`.
    trash_import_done: Arc<Mutex<Option<crate::disposal_backfill::BackfillReport>>>,
    // ── Users view (`ui::users`) ─────────────────────────────────────────
    /// Identity search box.
    users_query: String,
    /// True between a keystroke and the search that follows it.
    users_search_dirty: bool,
    /// True once a search has actually run, so "no matches" can be told apart
    /// from "you haven't searched yet".
    users_searched: bool,
    users_results: Vec<crate::chat_index::UserRow>,
    users_selected: Option<i64>,
    /// The selected identity's whole record, loaded once on selection — never
    /// on a render pass, so the index's lock stays off the UI thread.
    users_detail: Option<users::UserDetail>,
    users_tab: users::UserTab,
    /// Per-user message filter, as typed.
    users_msg_filter: String,
    /// Whole-archive message search, as typed.
    users_text_query: String,
    users_text_searched: bool,
    users_text_hits: Vec<crate::chat_index::MessageHit>,
    /// Take labels for `users_text_hits` (the index only returns ids).
    users_text_labels: HashMap<i64, crate::store::TakeLabel>,
    /// How many takes have a chat log at all — the denominator behind "N still
    /// to read", so an incomplete index never reads as an empty archive.
    users_takes_total: i64,
    /// Last thing that went wrong in the view, shown inline and dismissable.
    users_error: Option<String>,
    /// Cached chat-index counters for the Background tab: `(health, candidate
    /// count, when read)`. The Background view repaints continuously and these
    /// numbers only move once a minute — querying them per frame would put a
    /// database read on the render path.
    bg_index_stats: Option<(crate::chat_index::IndexHealth, i64, std::time::Instant)>,
    /// Channel picked for the on-demand "index this channel's chat logs" scan.
    users_scan_channel: Option<i64>,
    /// How many of that channel's most recent chat logs that scan reads.
    users_scan_count: i64,
    /// True while an on-demand scan is running, so a second can't overlap it.
    users_scan_running: bool,
    /// Finished on-demand scan's summary, posted from the background task and
    /// drained on the UI thread — same shape as `trash_action_done`.
    users_scan_done: Arc<Mutex<Option<String>>>,
    vod_info_popup_cache: HashMap<i64, (String, Recording)>,
    /// Deferred-viewport content for each open `vod_info_popups` entry.
    vod_info_popup_registry: PopupRegistry<i64, VodInfoContent>,
    /// Recording id whose remux-status popup is open, same caching shape as
    /// `vod_info_popup_cache`.
    remux_info_popups: Vec<i64>,
    remux_info_popup_cache: HashMap<i64, (String, Recording)>,
    /// Deferred-viewport content for each open `remux_info_popups` entry.
    remux_info_popup_registry: PopupRegistry<i64, VodInfoContent>,
    /// Lazy per-monitor upcoming-schedule detail, keyed by monitor id; cleared on
    /// reload. Backs the Next stream popup.
    schedule_cache: HashMap<i64, Vec<ScheduleSegment>>,
    /// Monitor id whose upcoming schedule is shown in a popup (None = closed).
    schedule_popups: Vec<i64>,
    /// Deferred-viewport content for each open `schedule_popups` entry.
    schedule_popup_registry: PopupRegistry<i64, SchedulePopupContent>,
    /// All upcoming scheduled streams (across every monitor), backing the Schedule
    /// calendar. Loaded lazily on first view + on refresh; see [`Self::spawn_reload_schedule`].
    schedule_all: Vec<UpcomingStream>,
    /// Whether [`Self::schedule_all`] has been loaded yet (lazy on first view).
    schedule_loaded: bool,
    /// Schedule calendar granularity (month / week / day).
    schedule_mode: ScheduleMode,
    /// The focused date the Schedule calendar is centered on; `None` until set to
    /// today on first view. Month view uses its year+month, week view the week
    /// containing it, day view the date itself.
    schedule_anchor: Option<chrono::NaiveDate>,
    /// Channel ids hidden from the Schedule calendar (sidebar filter). Tracking
    /// *hidden* (not visible) means newly-added channels default to visible.
    /// Persisted under [`crate::ui::schedule::K_SCHEDULE_HIDDEN_CHANNELS`] —
    /// unlike `schedule_hidden_segments` below, this is a deliberate per-channel
    /// preference (e.g. a channel whose schedule is a permanent dummy
    /// placeholder), not a soft per-event hide meant to reset.
    schedule_hidden: HashSet<i64>,
    /// Monitor (instance) ids hidden from the Schedule calendar — the same
    /// deliberate preference as `schedule_hidden`, one level down: some
    /// instances publish a permanent filler/dummy schedule (the same slots
    /// every day forever) while the channel's other instance has the real
    /// one. ANDed with the channel-level hide (either hides the event).
    /// Persisted under [`crate::ui::schedule::K_SCHEDULE_HIDDEN_MONITORS`];
    /// stale ids (deleted monitors) are harmless and simply never match.
    schedule_hidden_monitors: HashSet<i64>,
    /// Channels whose sidebar row is expanded to show per-instance
    /// checkboxes. Session-only.
    schedule_sidebar_open: HashSet<i64>,
    /// Live substring filter over the sidebar's channel list (case-insensitive,
    /// name match) — session-only, not persisted.
    schedule_channel_filter: String,
    /// The calendar's event filter bar (under the toolbar): case-insensitive
    /// substring over channel name / title / category / collaborators,
    /// narrowing every view (Month/Week/Day/Agenda). Session-only, like the
    /// sidebar filter above.
    schedule_event_filter: String,
    /// Individual segment IDs the user has soft-hidden (not tombstoned). Reset
    /// on app restart; use Delete for permanent suppression.
    schedule_hidden_segments: HashSet<i64>,
    /// When true, soft-hidden segments are shown dimmed instead of filtered out.
    schedule_show_hidden: bool,
    /// Whether to flag overlapping streams (time collisions) in the calendar.
    schedule_collisions: bool,
    /// Font/element zoom for the calendar body only (toolbar + sidebar stay
    /// normal size). 1.0 = 100%; Ctrl+0 resets. Session-only, like `schedule_mode`.
    schedule_zoom: f32,
    /// Per-channel display colour for every Schedule surface (event blocks,
    /// chips, stripes, sidebar legend) — the SAME resolution as the Streams
    /// list (custom colour > fetched Twitch broadcaster colour > palette),
    /// rebuilt each frame the Schedule view renders. Twitch colours are
    /// darkened for white-on-block readability (`block_safe_color`).
    schedule_chan_colors: HashMap<i64, egui::Color32>,
    /// Compact calendar events: collapse each Week/Day event block to a
    /// one-line chip at its start time (quick overview when many streams
    /// overlap). Persisted under [`K_SCHEDULE_COMPACT`].
    schedule_compact: bool,
    /// Draw a bigger channel-avatar picture in the body of each non-compact
    /// Week/Day event block (sized to fit; shrunk on a narrow block, never
    /// upscaled past the source image). Persisted under
    /// [`crate::ui::schedule::K_SCHEDULE_LARGE_AVATAR`].
    schedule_large_avatar: bool,
    /// Month view "Icons only": day cells show one avatar per scheduled
    /// channel, uniformly scaled so all of them fit, instead of chips +
    /// "+N more". Persisted under
    /// [`crate::ui::schedule::K_SCHEDULE_MONTH_ICONS`].
    schedule_month_icons: bool,
    /// Per-monitor lowercase (titles, categories) haystacks for the Streams
    /// grid's deep filter (`Store::monitor_meta_filter_texts`), cached against
    /// the `streams_cache_rev` it was fetched at — the recording history only
    /// changes when the grid data reloads, while the streams cache itself also
    /// rebuilds every second during an active capture.
    deep_filter_texts: Option<(u64, DeepFilterTexts)>,
    /// Each monitor's most recent `raid_out` event, cached against the
    /// `streams_cache_rev` it was fetched at — same reasoning (and same shape)
    /// as [`Self::deep_filter_texts`]: raids only arrive via EventSub, which
    /// reloads the grid (and bumps the rev), so re-running the query on the
    /// streams cache's *per-second* stamp was pure waste. It was also the
    /// single most expensive thing the UI thread did — see migration 87.
    raid_out_cache: Option<(u64, HashMap<i64, crate::models::StreamEventRow>)>,
    /// Per-monitor count of takes still counting down towards rolling
    /// auto-deletion, cached against `streams_cache_rev` — same shape and same
    /// reasoning as [`Self::raid_out_cache`]. Backs the 🕰 rollup badge.
    rolling_counts_cache: Option<(u64, HashMap<i64, i64>)>,
    /// Per-monitor sum of finished-take bytes, cached against
    /// `streams_cache_rev` — same shape and same reasoning as
    /// [`Self::rolling_counts_cache`]. Backs the Streams grid's "Disk use"
    /// column on channel/instance rows (a collapsed row has no per-take data
    /// loaded to sum itself — see `Store::monitor_disk_usage`'s doc comment);
    /// period/stream/take rows below them use `take_size_bytes` instead,
    /// which confirms each file still exists.
    disk_usage_cache: Option<(u64, HashMap<i64, i64>)>,
    /// An in-flight "🔄 Rescan disk usage" scan (channel/instance context menu,
    /// or the Streams toolbar for every monitor) — `None` while idle. The
    /// scan runs off-thread (one `exists_sync` per finished take can block on
    /// a stalled drive, same reasoning as `issues-missing-check`) and reports
    /// back the recording ids whose file is confirmed gone; `drain_rescan_disk_usage`
    /// clears their stored path (`Store::clear_recording_capture`) and forces
    /// a full Streams-grid refresh so the corrected total shows immediately.
    /// The manual fix for a file deleted outside the app, since nothing
    /// watches the filesystem for that on its own.
    rescan_disk_usage: Option<std::sync::mpsc::Receiver<Vec<i64>>>,
    /// Saved custom window layouts for the collab-play "Layout ▸" submenu,
    /// cached against `streams_cache_rev` — read once per grid rebuild instead
    /// of once per frame (it is a settings-table read, not a hot query, but
    /// the render path should hold no DB lock it doesn't have to).
    saved_layouts_cache: Option<(u64, Vec<crate::layout::CustomLayout>)>,
    /// The day whose full stream list is shown in a popup (None = closed).
    schedule_day_popup: Option<Arc<Mutex<ScheduleDayState>>>,
    /// Whether the "Schedule sources" dialog is open.
    show_schedule_sources: bool,
    /// Deferred-viewport state for the "Schedule sources" dialog (None = closed
    /// or not yet loaded).
    schedule_sources_popup: Option<Arc<Mutex<schedule::ScheduleSourcesPopupState>>>,
    /// Editable per-channel schedule-source configs shown in the Properties
    /// windows — one draft per open window, keyed by channel id.
    channel_cfg_drafts: HashMap<i64, crate::schedule_source::ChannelSourceConfig>,
    /// Editable per-channel schedule-source *scope* overrides (custom order +
    /// title-fill) shown in channel Properties, keyed by channel id.
    channel_scope_drafts: HashMap<i64, crate::schedule_source::SourceScopeConfig>,
    /// Editable per-instance (monitor) schedule-source *scope* overrides shown
    /// in instance Properties — one draft per open window, keyed by monitor id.
    instance_scope_drafts: HashMap<i64, crate::schedule_source::SourceScopeConfig>,
    /// Per-open channel-Properties trigger-word scope drafts (saved on change).
    channel_trigger_drafts: HashMap<i64, crate::triggers::TriggerScope>,
    /// Per-open instance-Properties trigger-word scope drafts (saved on change).
    instance_trigger_drafts: HashMap<i64, crate::triggers::TriggerScope>,
    /// Per-open channel-Properties BLACKLIST-trigger scope drafts (saved on change).
    channel_block_drafts: HashMap<i64, crate::triggers::TriggerScope>,
    /// Per-open instance-Properties BLACKLIST-trigger scope drafts (saved on change).
    instance_block_drafts: HashMap<i64, crate::triggers::TriggerScope>,
    /// Per-open instance-Properties chat-moderation history, newest first.
    /// Loaded once per window (it's history — it doesn't move while you read
    /// it) and dropped when the window closes, so a popup render pass never
    /// touches the database.
    instance_moderation: HashMap<i64, Vec<crate::models::StreamEventRow>>,
    /// Draft for the "Edit schedule item" dialog (None = closed). Saving converts
    /// the row to a protected `"manual"` source so refreshes don't overwrite it.
    edit_schedule: Option<Arc<Mutex<EditScheduleDraft>>>,
    /// Segment IDs selected in the schedule calendar (Ctrl+click multi-select).
    schedule_selected: HashSet<i64>,
    /// Open merge-preview dialog (None = closed).
    merge_preview: Option<Arc<Mutex<MergePreviewDraft>>>,
    /// Pending multi-delete confirmation for schedule segments (None = closed).
    confirm_delete_segments: Option<Arc<Mutex<ConfirmDialogState<Vec<i64>>>>>,
    /// Computed from `schedule_all`: primary segment_id → merge badge text.
    /// Built by [`Self::recompute_merge_state`]; drives the 🔀 indicator.
    schedule_merge_labels: HashMap<i64, String>,
    /// Computed from `schedule_all`: segment IDs that are auto-merge secondaries
    /// (hidden in favour of their primary). Built by [`Self::recompute_merge_state`].
    schedule_auto_secondary: HashSet<i64>,
    /// User-defined filename template presets loaded from the DB.
    custom_presets: Vec<(i64, String, String)>,
    /// Open "Save preset" naming dialog (None = closed).
    save_preset_dialog: Option<Arc<Mutex<SavePresetDraft>>>,
    /// Chat log viewer popup (None = closed).
    /// Open chat windows, one per monitor (each is its own OS viewport).
    chat_popups: Vec<Arc<Mutex<ChatPopup>>>,
    /// Platform favicons, uploaded to the GPU on first use (None until then).
    platform_tex: Option<PlatformTextures>,
    /// Chat/toolbar affordance icons, uploaded on first use (None until then).
    /// See [`UiTextures`] for why they aren't just emoji.
    ui_tex: Option<UiTextures>,
    /// App-wide UI font family by display name (`""` = egui's bundled
    /// default) — see [`K_APP_FONT_FAMILY`]. The chat's own font lives on the
    /// shared `ChatSettingsState` instead, so each chat window sees a change
    /// immediately.
    app_font: String,
    /// The font choice currently installed in egui. Compared against the live
    /// settings each frame so a change (from either font picker) is applied
    /// exactly once — `ctx.set_fonts` rebuilds the whole atlas and invalidates
    /// every cached galley, so it must never run per frame.
    installed_fonts: crate::fonts::FontChoice,
    /// Installed system fonts for the pickers, enumerated once on first use
    /// (~400 registry values plus an existence check each).
    system_fonts: Option<Vec<crate::fonts::SystemFont>>,
    /// Whether being named in chat raises a notification — see
    /// [`crate::chat_highlight::K_PINGABLE`]. Mirrors the setting so the
    /// checkbox has somewhere to live; the chat logger reads the store.
    chat_pingable: bool,

    /// Which monitor's Properties window is open (None = closed).
    properties_popups: Vec<i64>,
    /// Deferred-viewport state for each open instance-Properties window,
    /// keyed by monitor id — see [`properties::InstancePropsPopupState`].
    instance_props_registry: PopupRegistry<i64, properties::InstancePropsPopupState>,
    /// Open user-Properties windows, keyed by `(channel id, lowercased
    /// name)` — one per person per channel, since the same name is a
    /// different stranger in another channel's records.
    user_props_popups: Vec<(i64, String)>,
    /// Deferred-viewport state for each — see [`properties::UserPropsPopupState`].
    user_props_registry: PopupRegistry<(i64, String), properties::UserPropsPopupState>,
    /// Open channel-Properties windows (one per channel).
    channel_properties_popups: Vec<i64>,
    /// Deferred-viewport state for each open channel-Properties window,
    /// keyed by channel id — see [`properties::ChannelPropsPopupState`].
    channel_props_registry: PopupRegistry<i64, properties::ChannelPropsPopupState>,
    /// [`properties::PropsLoadingPlaceholderState`] for `drive_props_load`'s
    /// "Loading…" placeholder, keyed by the real window's own viewport id —
    /// shared by both instance- and channel-Properties loads.
    props_loading_registry: PopupRegistry<egui::ViewportId, properties::PropsLoadingPlaceholderState>,
    /// Open recording-properties windows, one per take (each carries its own
    /// notes draft, synced from the DB on open and written back per keystroke).
    rec_props_popups: Vec<Arc<Mutex<RecPropsPopup>>>,
    /// Open schedule-event-properties windows, one per event (each carries its
    /// own rescan model/effort draft). Opened by clicking an event tile in the
    /// Schedule calendar — see [`crate::ui::dialogs::EventPropsPopup`].
    event_props_popups: Vec<Arc<Mutex<EventPropsPopup>>>,
    /// Per-channel cached icon textures loaded from disk for the Properties window.
    /// A `None` value means the lookup was attempted but no icon file was found.
    channel_icons: HashMap<i64, Option<egui::TextureHandle>>,
    /// Pre-scaled (64 px) icon textures for the streams table avatar column.
    /// Separate from `channel_icons` so the small slot can use a properly
    /// Lanczos-downscaled thumbnail while Properties loads the full source.
    channel_icons_small: HashMap<i64, Option<egui::TextureHandle>>,
    /// Pre-scaled (64 px) per-INSTANCE icon textures for the instance rows of the
    /// streams table, keyed by monitor id — each instance shows the avatar fetched
    /// for its own account dir (GEEGA main vs alt). Same lifecycle as
    /// `channel_icons_small` (cleared on AssetFetch completion / channel rename).
    instance_icons_small: HashMap<i64, Option<egui::TextureHandle>>,
    /// Decoded + downscaled chat-emote frames, keyed by absolute image path. Shared
    /// with background decode tasks (`Arc<Mutex<…>>`). Animated GIF/WebP cycle; the
    /// frames are downscaled to render size to bound RAM, and the map is LRU-evicted
    /// against [`EMOTE_BUDGET_BYTES`] + cleared on asset refetch / popup close.
    emote_anim: Arc<Mutex<HashMap<std::path::PathBuf, crate::emote_anim::EmoteLoad>>>,
    /// Bumped whenever `emote_anim` is cleared; in-flight decode tasks capture it at
    /// spawn and skip their insert if it changed, so a decode finishing after a
    /// popup-close / asset-refetch can't resurrect a stale (leaked) cache entry.
    emote_epoch: Arc<AtomicU64>,
    /// Loaded mainpage image assets (icon + banner per platform) for the channel
    /// Properties thumbnail strip, keyed by channel id. Full-resolution textures
    /// (so Alt-preview is crisp); loaded on window open, dropped on close/refetch.
    channel_asset_thumbs: HashMap<i64, Vec<AssetThumb>>,
    /// Per-provider *viewable* emote counts for the open Properties window, keyed by
    /// channel id. Cached because the count is derived from the same enumeration the
    /// viewer uses (one `fs::metadata` per emote) — recomputing it every frame would
    /// be hundreds of stat calls per repaint. Invalidated wherever `channel_asset_thumbs`
    /// is (open/rename/refetch/close).
    channel_emote_counts: HashMap<i64, Vec<(AssetAccount, [(EmoteProvider, usize); 4])>>,
    /// Per-platform asset-status rows for the open Properties window, keyed by channel
    /// id. Cached for the same reason as `channel_emote_counts`: each row is built from
    /// blocking filesystem I/O (`read_dir` + per-file `metadata` + full JSON manifest
    /// parse), and the status grid is rebuilt every frame — so doing the I/O per frame is
    /// dozens of syscalls per repaint and can freeze the UI thread on slow/AV-scanned
    /// storage. Invalidated wherever `channel_asset_thumbs` is (open/rename/refetch/close).
    channel_asset_status: HashMap<i64, Vec<PlatformAssetStatus>>,
    /// Snapshot of the global schedule-source order for the open Properties window. Taken
    /// once on open so `scope_override_editor` reads it from memory instead of doing a
    /// settings DB read (store mutex) every frame.
    props_source_order: Vec<SourceEntry>,
    /// In-flight background load of the channel Properties window's per-open data (icon +
    /// asset-thumbnail decode/upload, per-platform asset enumeration, and the schedule
    /// -source config/scope/order DB reads). Run OFF the UI thread so a slow disk, an AV
    /// scan, or the store mutex being held by a background task can't freeze the GUI on
    /// open — the window shows a "Loading…" placeholder until the bundle lands. `None`
    /// when no load is running.
    props_loads: Vec<PropsLoad>,
    /// In-flight native file/folder picker (background thread). The OS dialog blocks
    /// until the user picks or cancels; running it off the UI thread keeps egui alive.
    /// At most one picker open at a time (a second Browse click replaces any existing).
    pending_browse: Option<PendingBrowse>,
    /// In-flight form save (background thread). The INSERT/UPDATE + reload queries can
    /// block on the store mutex when a detection pass holds it; running off the UI
    /// thread prevents a visible freeze on "Save".
    pending_save: Option<PendingSave>,
    /// In-flight F5 / manual reload (background thread). Same DB queries as
    /// `pending_save` but no write — avoids blocking the UI thread on the store
    /// mutex while a schedule-refresh Tokio task holds it.
    pending_reload: Option<std::sync::mpsc::Receiver<Option<SaveRows>>>,
    /// A reload was requested while one was already in flight. The in-flight
    /// thread may have read the DB *before* the change that triggered the new
    /// request, so drop-and-forget would leave the UI stale (until F5) — run
    /// one more reload as soon as the current one lands instead.
    reload_queued: bool,
    /// Unix time of the last timer-driven background reload. Routine polls
    /// update the DB (e.g. `last_checked_at`) without emitting an event, so a
    /// slow cadence reload keeps sorted columns correct without F5.
    last_auto_reload: i64,
    /// TTL cache for per-row filesystem probes (see [`FsProbes`]). `Arc<Mutex<>>`
    /// (not a plain field) so deferred-viewport popup closures — which must be
    /// `Send + Sync + 'static` and so cannot hold `&mut self` — can still reach
    /// it; `Mutex::lock` only needs `&self`, so this also sidesteps the old
    /// exclusive-borrow-of-`self` conflicts that came from `let fs = &mut
    /// self.fs_probes;` inside a closure that also touched other `self` fields.
    fs_probes: Arc<Mutex<FsProbes>>,
    /// When the Videos list was last re-read from the store. The tab shows
    /// live progress, but a 1s TTL replaces the old full SELECT every frame.
    videos_refreshed: Option<std::time::Instant>,
    /// Bumped whenever `self.videos` is reloaded — keys the sort-model cache.
    videos_rev: u64,
    /// Videos sort/filter model cache: (videos_rev, unix second, model).
    /// Second granularity keeps speed cells ticking without per-frame rebuild.
    videos_model_cache: Option<(u64, i64, Vec<Vec<Cell>>)>,
    /// Lowercased `settings_search`, kept in sync on edit — `section_shown`
    /// runs per section per frame and must not re-lowercase each call.
    settings_search_lc: String,
    /// Cached recovery CDN host count for the Settings label (5s TTL) — the
    /// old code re-read + re-parsed the host list from the store every frame.
    recovery_host_count: Option<(std::time::Instant, usize)>,
    /// Frame-invariant Streams-view data (see [`StreamsViewCache`]); rebuilt
    /// once per second or when `streams_cache_rev` bumps.
    streams_cache: Option<StreamsViewCache>,
    /// Bumped whenever data feeding the Streams view changes NOW (reload
    /// installed, expansion toggled, F5, settings saved) so the cache rebuilds
    /// immediately instead of waiting for the next second tick.
    streams_cache_rev: u64,
    /// Cached YouTube Data API quota for today and the configured daily cutoff.
    /// Updated by the background reload-rows thread; never read from DB on the
    /// render thread (which would block if the DB mutex is held elsewhere).
    yt_quota_today: i64,
    yt_quota_cutoff: i64,
    /// Daily search.list query count and its cutoff (separate from unit quota).
    yt_search_today: i64,
    yt_search_cutoff: i64,
    /// Per-endpoint unit breakdown of `yt_quota_today` — where the daily spend
    /// actually goes (search.list is 100u/call, videos.list and channels.list
    /// are 1u/call), so a spike is traceable instead of just an opaque total.
    yt_ep_search_today: i64,
    yt_ep_videos_today: i64,
    yt_ep_channels_today: i64,
    /// Keys of quota warning issues the user has dismissed this session.
    dismissed_quota_warnings: HashSet<String>,
    /// In-flight schedule calendar reload (background thread). `all_upcoming_schedule`
    /// can hold the DB mutex for several seconds when historical rows accumulate;
    /// running it off the UI thread prevents frame freezes and unblocks the delete action.
    pending_schedule: Option<std::sync::mpsc::Receiver<Option<Vec<UpcomingStream>>>>,
    /// Open emote-viewer windows (one per channel+provider). Reuse the shared
    /// `emote_anim` decode cache, so emotes animate on the chat-replay clock.
    emote_viewers: Vec<Arc<Mutex<EmoteViewer>>>,
    /// Open asset change-history popup (None = closed). Holds the channel's
    /// `asset_changes.jsonl` parsed + formatted once on open (newest first).
    asset_histories: Vec<Arc<Mutex<AssetHistoryView>>>,
    /// Open About-page viewers (one per channel + platform + account): the
    /// account's archived about versions with a picker + rendered content.
    about_views: Vec<Arc<Mutex<AboutView>>>,
    /// Channel Properties "About pages" rows: latest snapshot + version count
    /// per (platform, account), loaded off-thread with the props bundle.
    channel_about_latest: HashMap<i64, Vec<(crate::store::AboutSnapshotRow, i64)>>,
    /// GPU textures for the third-party emote-provider logos (7TV/BTTV), uploaded
    /// once on first use of the emote launcher buttons.
    provider_tex: Option<ProviderTextures>,
    /// Per-channel Twitch broadcaster name colour (from `name_color.txt`, fetched
    /// via Helix). `None` = looked up but the streamer set no colour / not Twitch.
    /// Tints the channel name in the Streams list; cleared with `channel_icons`.
    channel_twitch_colors: HashMap<i64, Option<egui::Color32>>,
    /// Sort + per-column filters for the Videos table.
    videos_sort: SortState,
    videos_filters: Vec<String>,
    /// Shared state of the interactive "Connect Twitch" device-code flow.
    twitch_flow: Arc<Mutex<AuthFlow>>,
    /// Shared state of the interactive "Connect YouTube" (Google) device-code flow.
    google_flow: Arc<Mutex<AuthFlow>>,
    /// Open "Import followed/subscriptions" confirmation dialog, if any.
    import_dialog: Option<Arc<Mutex<ImportDialog>>>,
    /// Stored collab history keyed by `(monitor_id, stream_id)` → partner
    /// names, preloaded on row reload — lets stream/take rows show which
    /// collab a past broadcast was without per-frame DB queries.
    collab_by_stream: HashMap<(i64, String), String>,
    /// Open "🤝 Collab history" popup: the channel id + its loaded sessions.
    collab_history: Option<Arc<Mutex<CollabHistoryState>>>,
    /// Open "which streams was this collab in" drill-down: the partner name
    /// and every session they appeared in, across all channels. Opened by
    /// clicking a partner's Sessions count in the App Stats Collabs table.
    partner_sessions: Option<Arc<Mutex<PartnerSessionsState>>>,
    /// Whether Streams rows show a status background tint (recording / ad / error).
    /// Toggled from the top bar; persisted under [`K_STATUS_BGCOLOR`]. Keyboard
    /// row selection is still highlighted regardless.
    status_bgcolor: bool,
    /// Whether the per-row Actions column (inline action buttons) is shown in the
    /// Streams + Videos tables. Off reclaims width; every action is also on the
    /// row's right-click context menu. Persisted under [`K_SHOW_ACTIONS`].
    show_actions: bool,
    /// Whether timestamp columns show a compact short format (e.g. `21/06 14:02`)
    /// instead of the full datetime. The full value appears in a tooltip. Persisted
    /// under [`K_SHORT_TIMESTAMPS`].
    shorten_timestamps: bool,
    /// Global chat-replay settings (render/animate emotes, unknown-emote CDN
    /// fetch, live usercard lookup, font size, colors) — shared by the
    /// Settings dialog's Display section and every open chat window's own
    /// ⚙ panel. See [`chat::ChatSettingsState`].
    chat_settings: Arc<Mutex<chat::ChatSettingsState>>,
    /// Set to true by the "⇔ Fit columns" button; consumed in `channels_view`
    /// to call `TableBuilder::reset()` so columns revert to content-fit widths.
    reset_streams_columns: bool,
    /// Persisted column order/visibility for every grid table (Streams, Videos,
    /// Background Active/Recent, Processes, Issues); see [`crate::grid_columns`].
    streams_grid: GridState,
    videos_grid: GridState,
    bg_active_grid: GridState,
    bg_recent_grid: GridState,
    /// Background view: whether the disk-gate queue list is expanded
    /// (session-only).
    bg_show_gate_queue: bool,
    processes_grid: GridState,
    issues_grid: GridState,
    backlog_grid: GridState,
    /// Sort + per-column filters for the 📥 Backlog table. Defaults to
    /// newest-first, which is the whole reason Backlog is its own view rather
    /// than a mode of Streams (Streams is a tree grouped under channels).
    backlog_sort: SortState,
    backlog_filters: Vec<String>,
    /// Backlog's 🕰 Rolling recordings section: also list broadcasts already
    /// kept, so Unkeep is reachable there. Session-only — the section is about
    /// what's at risk right now, and that's what it should open showing.
    backlog_show_kept: bool,
    /// Backing state for the "⇕ Reorder columns…" window (`None` = closed) —
    /// a working copy of one table's entries, only written back + persisted
    /// (and only forcing one table reset, not one per intermediate move) when
    /// the user hits Apply. See [`ReorderColumnsState`].
    reorder_columns: Option<Arc<Mutex<ReorderColumnsState>>>,
    /// Currently running background tasks (asset fetches, thumbnail downloads).
    background_tasks: Vec<crate::events::BackgroundTask>,
    /// Completed/failed background tasks (task, outcome, finished-at unix), newest
    /// first; capped at 100.
    finished_tasks: Vec<(crate::events::BackgroundTask, crate::events::TaskOutcome, i64)>,
    /// Enable/disable state for the periodic jobs (`events::TOGGLEABLE_JOBS`),
    /// mirrored from settings; edited via the Background "Scheduled" checkboxes.
    job_toggles: std::collections::HashMap<String, bool>,
    /// Debug view state — persisted across frames; fields are always present but
    /// only rendered when [`debug_view_enabled`] (debug build or `--debug`).
    debug_monitor_idx: usize,
    debug_test_title: String,
    debug_test_game: String,
    /// Format Designer: an interactive template preview/editor window.
    format_designer: Option<Arc<Mutex<FormatDesignerState>>>,
    /// Pending "Stop recordings & quit" confirmation (triggered by the tray
    /// item or the top-bar StreamArchiver ▾ menu).
    confirm_quit_stop: Option<Arc<Mutex<ConfirmDialogState<()>>>>,
    /// One-shot: the confirmation viewport got its focus raise this showing.
    confirm_quit_stop_raised: bool,
    /// Cached (ocr_stats, global_stats, poll_stats) for the Stats view; None = not yet loaded.
    stats_snapshot: Option<(OcrStats, GlobalStats, PollStats)>,
    /// App Stats "Capture health": lifetime totals + per-day trend, loaded
    /// with (and refreshed by) the same snapshot cycle as `stats_snapshot`.
    stats_capture_health:
        Option<(Vec<crate::store::AlertDailyStat>, crate::store::AlertHealthTotals)>,
    /// Per-day recording count/bytes series backing the Recordings
    /// Day/Week/Month/Year breakdown — loaded/refreshed with `stats_snapshot`.
    stats_recordings_daily: Option<Vec<DailyRecordingStat>>,
    /// Selected period for the Recordings breakdown (session-only).
    recordings_period: RecordingsPeriod,
    /// Cached 🤝 collab-partner overview (login, name, sessions, last seen)
    /// for the Stats view — loaded/refreshed together with `stats_snapshot`.
    stats_collabs: Vec<(String, String, i64, i64)>,
    /// Selected timespan for the Stats view's detection-history graphs
    /// (session-only, defaults to 24 h).
    stats_poll_span: PollSpan,
    /// Cached `poll_history` rows for the selected span; None = (re)query on
    /// next Stats render. Invalidated separately from `stats_snapshot` so
    /// flipping the span doesn't re-run the other stats queries.
    stats_history: Option<Vec<crate::models::PollBucket>>,
    /// Selected timespan for the Stats view's download graph (session-only,
    /// defaults to 24 h). Independent of `stats_poll_span` — the two sections
    /// are usually read at different zoom levels.
    stats_net_span: PollSpan,
    /// Cached `net_history` rows for `stats_net_span`; None = (re)query on the
    /// next Stats render.
    stats_net_history: Option<Vec<crate::models::NetBucket>>,
    /// Per-day downloaded bytes per traffic class backing the Network
    /// Day/Week/Month/Year breakdown — loaded/refreshed with `stats_snapshot`.
    stats_net_daily: Option<Vec<crate::models::DailyNetStat>>,
    /// Selected period for the Network breakdown (session-only).
    net_period: RecordingsPeriod,
    /// Live per-class download rates, refreshed at most 1×/s while the Stats
    /// tab is open (an `iomon::latest()` clone per frame would be wasteful).
    stats_net_live: Option<(std::time::Instant, NetLive)>,
    /// Channel Stats view: selected channel (`None` = all-channels overview).
    chstats_channel: Option<i64>,
    /// Channel Stats view: selected timespan (session-only, defaults 30 d).
    chstats_span: PollSpan,
    /// Channel Stats view: cached query results for (channel, span);
    /// `None` = (re)query on next render.
    chstats_data: Option<channel_stats::ChStatsData>,
    /// Channel Stats view: re-run the queries once a minute while the tab is
    /// open (new samples land at that cadence). Persisted as
    /// `chstats_auto_refresh`; default on.
    chstats_auto: bool,
    /// When `chstats_data` was last loaded (unix secs) — drives auto refresh.
    chstats_loaded_at: i64,
    /// Events-list filter text in the Channel Stats view (session-only).
    chstats_event_filter: String,
    /// 📈 viewer-stats popup window (single-instance, like collab history).
    viewer_stats_popup: Option<Arc<Mutex<channel_stats::ViewerStatsPopup>>>,
    /// Confirm hype trains via anonymous Twitch GQL polling. Persisted as
    /// `hype_gql` ([`crate::hype::K_HYPE_GQL`]); default on.
    hype_gql: bool,
    /// Cached copy of the global hype-train tuning blob for the Settings
    /// widgets (auto-tune also rewrites the stored blob in the background —
    /// the section's ⟳ reloads).
    hype_tuning: crate::hype::HypeTuning,
    /// "🚂 Mark hype train" dialog — `None` = closed. See [`HypeMarkDraft`].
    show_hype_mark: Option<Arc<Mutex<HypeMarkDraft>>>,
    /// Mark dialog: "minutes ago" start shortcut, remembered across opens
    /// (used to seed a fresh [`HypeMarkDraft`] — the channel and absolute
    /// time are NOT remembered, only these two "usually the same" values).
    hype_mark_mins_ago: i64,
    /// Mark dialog: train duration in minutes, remembered across opens.
    hype_mark_dur: i64,
    /// "⚙ Hype sensitivity" per-channel override editor (`None` = closed).
    /// See [`HypeOverrideState`].
    hype_override_for: Option<Arc<Mutex<HypeOverrideState>>>,
    /// Recent raw viewer samples per monitor for the 👁 column sparklines
    /// (last hour), refreshed at most once per minute while Streams renders.
    spark_data: std::collections::HashMap<i64, Vec<(i64, i64)>>,
    /// When `spark_data` was last refreshed (unix secs; 0 = never).
    spark_loaded_at: i64,
    /// I/O tab: cached sampler history + counters snapshot (refreshed ~1×/s
    /// while the tab is open — never cloned per frame).
    io_hist: Vec<crate::iomon::Sample>,
    io_snap: Option<crate::iomon::CountersSnapshot>,
    io_refreshed: Option<std::time::Instant>,
    /// I/O tab: which sub-tab is shown (Disks / Database).
    io_tab: IoTab,
    /// I/O tab: which series the rate graph shows.
    io_plot_metric: IoPlotMetric,
    /// I/O tab: recent-operations log filters.
    io_ops_cat: Option<crate::iomon::Cat>,
    io_ops_region: Option<crate::iomon::Region>,
    /// I/O tab: category-table sort (column index, ascending).
    io_cat_sort: (usize, bool),
    /// Files tab: off-thread path/drive scan (None = needs a (re)load).
    files_scan: Option<FilesScan>,
    files_scan_rx: Option<std::sync::mpsc::Receiver<FilesScan>>,
    /// Files tab: per-instance output-dir edit buffers (monitor id → draft).
    files_edit: std::collections::HashMap<i64, String>,
    /// Files tab: selected instances for batch actions.
    files_selected: std::collections::HashSet<i64>,
    /// Files tab: batch "set folder for selected" draft.
    files_batch_dir: String,
    /// Files tab: "Redirect all instances on drive" bar drafts (single
    /// letters, e.g. "A" / "G" — not full paths).
    files_redirect_from: String,
    files_redirect_to: String,
    /// Files tab: relocate-paths dialog drafts.
    files_reloc_from: String,
    files_reloc_to: String,
    files_reloc_monitors: bool,
    /// Files tab: last relocate preview (from-string, rec/video/monitor counts).
    files_reloc_preview: Option<(String, i64, i64, i64)>,
    files_status: String,
    /// Channel id to scroll into view on the next Streams render, after a save
    /// adds a new channel. Cleared once consumed. None = no pending scroll.
    scroll_to_channel: Option<i64>,
    /// Rename dialog: whether the dialog is open.
    show_rename_dialog: bool,
    /// Rename dialog: the recording id being renamed.
    rename_rec_id: Option<i64>,
    /// Rename dialog: the current template/stem string the user is editing.
    rename_draft: String,
    /// Rename dialog: live-expanded preview of `rename_draft`.
    rename_preview: String,
    /// Deferred-viewport state while the Rename dialog is open (None =
    /// closed) — see [`dialogs::RenameDialogState`].
    rename_dialog_popup: Option<Arc<Mutex<dialogs::RenameDialogState>>>,
}
/// Handle to the background thread loading a channel Properties window's per-open data.
/// Polled each frame the window is open until the [`PropsLoaded`] bundle arrives. See
/// the `props_loads` field for why this work is off the UI thread.
struct PropsLoad {
    /// The channel being loaded; lets us ignore a bundle that arrives after the user
    /// switched the window to a different channel.
    channel_id: i64,
    rx: std::sync::mpsc::Receiver<PropsLoaded>,
}

/// The fully-loaded per-open Properties data, produced on a background thread and
/// installed into the per-channel caches on the UI thread. Every field is the result of
/// blocking work (disk reads + image decode/upload, asset-dir enumeration, store-mutex
/// DB reads) that previously ran inline on the UI thread and could freeze the GUI.
struct PropsLoaded {
    channel_id: i64,
    /// `None` = no icon file found (a successful "no icon" result, not a failure).
    icon: Option<egui::TextureHandle>,
    thumbs: Vec<AssetThumb>,
    emote_counts: Vec<(AssetAccount, [(EmoteProvider, usize); 4])>,
    asset_status: Vec<PlatformAssetStatus>,
    cfg: crate::schedule_source::ChannelSourceConfig,
    source_order: Vec<SourceEntry>,
    scope: crate::schedule_source::SourceScopeConfig,
    /// Latest About snapshot + version count per (platform, account).
    about_latest: Vec<(crate::store::AboutSnapshotRow, i64)>,
}

/// In-flight native file/folder picker spawned on a background thread so the UI
/// thread is never blocked by the OS dialog. Polled each frame via `try_recv`.
struct PendingBrowse {
    rx: std::sync::mpsc::Receiver<Option<String>>,
    /// Called on the UI thread once the picker returns a path. Receives `&mut App`
    /// and the selected path; skipped when the user cancels (dialog returns `None`).
    /// `+ Send`: a `MonitorForm` can hold a `PendingBrowse` (`browse_req`) and
    /// `MonitorForm` itself needs `Send` to live behind `form_window`'s
    /// deferred-viewport `Arc<Mutex<>>`.
    apply: Box<dyn FnOnce(&mut StreamArchiverApp, String) + Send>,
}

/// Loaded rows returned by a background save-form thread; installed by
/// `drain_pending_save` once the thread completes.
struct SaveRows {
    rows: Vec<MonitorWithChannel>,
    channels: Vec<Channel>,
    next_streams: Vec<(i64, i64, String)>,
    yt_quota_today: i64,
    yt_quota_cutoff: i64,
    yt_search_today: i64,
    yt_search_cutoff: i64,
    yt_ep_search_today: i64,
    yt_ep_videos_today: i64,
    yt_ep_channels_today: i64,
    /// Id of a newly-INSERTED monitor (a fresh add, not an edit) — the UI fires
    /// an immediate asset/About fetch for it so a new channel isn't blank until
    /// the hourly sweep. `None` for an edit.
    new_monitor_id: Option<i64>,
}

/// In-flight form-save spawned on a background thread. The thread holds the store
/// mutex while doing the INSERT/UPDATE + reload queries, keeping the UI thread free.
struct PendingSave {
    rx: std::sync::mpsc::Receiver<Result<SaveRows, String>>,
}
/// Spawn a native folder picker on a background thread. The picker blocks until
/// the user chooses or cancels; keeping it off the UI thread lets egui keep
/// painting (and the watchdog heartbeat keep beating). Returns a [`PendingBrowse`]
/// that the caller stores in `app.pending_browse`; the `apply` closure is called
/// on the UI thread once the user confirms a selection.
fn spawn_browse_folder(
    current: &str,
    apply: impl FnOnce(&mut StreamArchiverApp, String) + Send + 'static,
) -> PendingBrowse {
    let (tx, rx) = std::sync::mpsc::channel();
    let current = current.to_string();
    std::thread::Builder::new()
        .name("browse-folder".into())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if !current.trim().is_empty() && crate::iomon::fs::exists_sync(crate::iomon::Cat::FsProbe, &current) {
                dialog = dialog.set_directory(&current);
            }
            let _ = tx.send(dialog.pick_folder().map(|p| p.to_string_lossy().to_string()));
        })
        .ok();
    PendingBrowse { rx, apply: Box::new(apply) }
}

/// Same as [`spawn_browse_folder`] but opens a file picker instead.
fn spawn_browse_file(
    current: &str,
    apply: impl FnOnce(&mut StreamArchiverApp, String) + Send + 'static,
) -> PendingBrowse {
    spawn_browse_file_impl(current, None, apply)
}

/// [`spawn_browse_file`] with an extension filter, e.g.
/// `("Images", &["png", "jpg"])`. The picker still offers an "All files"
/// escape hatch (rfd adds one on Windows).
fn spawn_browse_file_filtered(
    current: &str,
    filter: (&'static str, &'static [&'static str]),
    apply: impl FnOnce(&mut StreamArchiverApp, String) + Send + 'static,
) -> PendingBrowse {
    spawn_browse_file_impl(current, Some(filter), apply)
}

fn spawn_browse_file_impl(
    current: &str,
    filter: Option<(&'static str, &'static [&'static str])>,
    apply: impl FnOnce(&mut StreamArchiverApp, String) + Send + 'static,
) -> PendingBrowse {
    let (tx, rx) = std::sync::mpsc::channel();
    let current = current.to_string();
    std::thread::Builder::new()
        .name("browse-file".into())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some((name, exts)) = filter {
                dialog = dialog.add_filter(name, exts);
            }
            if let Some(parent) = std::path::Path::new(&current).parent() {
                if crate::iomon::fs::is_dir_sync(crate::iomon::Cat::FsProbe, parent) {
                    dialog = dialog.set_directory(parent);
                }
            }
            let _ = tx.send(dialog.pick_file().map(|p| p.to_string_lossy().to_string()));
        })
        .ok();
    PendingBrowse { rx, apply: Box::new(apply) }
}

impl StreamArchiverApp {
    /// Declare/update every popup window for this frame.
    ///
    /// **This must be called from [`eframe::App::logic`], never from
    /// [`eframe::App::ui`].** A `show_viewport_deferred` child only survives
    /// egui's end-of-pass viewport GC if it was re-declared during that pass
    /// (`ViewportImpl::used`), and eframe *skips `App::ui` entirely* whenever
    /// the root viewport reports itself invisible — which
    /// [`egui::ViewportInfo::visible`] derives from `minimized`/`occluded`, so
    /// it is `Some(false)` for a merely **minimized** window. `App::logic`
    /// keeps being called there, `App::ui` does not.
    ///
    /// Declaring these from `ui()` was therefore why every open popup's native
    /// window was destroyed the moment the main window was minimized, and
    /// reappeared with a fresh HWND (and default position) on restore — the
    /// exact symptom the deferred-viewport migration was supposed to fix, just
    /// moved one level up. Verified with a standalone eframe probe: declared
    /// from `ui()`, the child HWND vanishes from `EnumWindows` while the root
    /// is minimized; declared from `logic()`, the *same* HWND survives both a
    /// minimize and a hide-to-tray (`ViewportCommand::Visible(false)`).
    ///
    /// Side effect of the move: popups now open one frame after the click that
    /// requested them (this runs before `ui()` within the same pass, so it
    /// sees last frame's flags). Imperceptible, and they render on their own
    /// viewport schedule anyway.
    fn popup_windows(&mut self, ctx: &egui::Context) {
        self.form_window(ctx);
        self.channel_form_window(ctx);
        self.group_manager_window(ctx);
        self.add_to_recording_group_window(ctx);
        self.confirm_delete_window(ctx);
        self.confirm_delete_channel_window(ctx);
        self.drain_manual_delete_results();
        self.confirm_delete_file_window(ctx);
        self.confirm_delete_stream_files_window(ctx);
        self.move_instance_window(ctx);
        self.merge_channel_window(ctx);
        self.confirm_delete_segment_window(ctx);
        self.merge_preview_window(ctx);
        self.confirm_delete_segments_window(ctx);
        self.save_preset_window(ctx);
        self.format_probe_window(ctx);
        self.layout_editor_window(ctx);
        self.recover_vod_window(ctx);
        self.ad_popup_windows(ctx);
        self.meta_popup_windows(ctx);
        self.history_popup_windows(ctx);
        self.chapters_popup_windows(ctx);
        self.vod_info_popup_windows(ctx);
        self.remux_info_popup_windows(ctx);
        self.collab_history_window(ctx);
        self.partner_sessions_window(ctx);
        self.viewer_stats_window(ctx);
        self.hype_mark_window(ctx);
        self.hype_override_window(ctx);
        self.schedule_popup_windows(ctx);
        self.schedule_sources_window(ctx);
        self.schedule_day_window(ctx);
        self.edit_schedule_window(ctx);
        self.event_properties_windows(ctx);
        self.chat_popup_windows(ctx);
        self.instance_properties_windows(ctx);
        self.channel_properties_windows(ctx);
        self.user_properties_windows(ctx);
        self.emote_viewer_windows(ctx);
        self.rename_dialog_window(ctx);
        self.asset_history_windows(ctx);
        self.recording_properties_windows(ctx);
        self.processes_window(ctx);
        self.reorder_columns_window(ctx);
        self.scheduled_recordings_window(ctx);
        self.scheduled_recording_form_window(ctx);
        self.confirm_delete_scheduled_recording_window(ctx);
        self.confirm_permadelete_trash_window(ctx);
        self.issues_window(ctx);
        self.notifications_window(ctx);
        self.warnings_window(ctx);
        self.pot_server_log_window(ctx);
        self.log_view_window(ctx);
        self.posts_window(ctx);
        self.posts_excluded_window(ctx);
        self.format_designer_window(ctx);
        self.confirm_quit_stop_window(ctx);
        self.import_window(ctx);
        self.about_windows(ctx);
        self.inspector_window(ctx);
    }
}
impl StreamArchiverApp {
    /// Re-install the egui font stack when either font picker has changed.
    ///
    /// Called every frame, but only *does* anything when the choice actually
    /// differs from what is installed: `ctx.set_fonts` rebuilds the glyph
    /// atlas and invalidates every cached galley, so running it per frame
    /// would be ruinous. The comparison is two string compares.
    ///
    /// Reading the live values each frame (rather than having the pickers
    /// signal a change) is deliberate: the chat font can be edited from a
    /// deferred chat-window panel that has no `&mut self` to signal through,
    /// and this way there is one place that decides, not two.
    fn apply_font_settings(&mut self, ctx: &egui::Context) {
        let want = crate::fonts::FontChoice {
            app: self.app_font.clone(),
            chat: self.chat_settings.lock().unwrap().chat_font.clone(),
        };
        if want == self.installed_fonts {
            return;
        }
        crate::fonts::install_fonts(ctx, &want);
        self.installed_fonts = want;
    }
}

impl eframe::App for StreamArchiverApp {
    /// eframe's default is 30s, and egui state (scroll positions, window
    /// geometry) changes almost every interaction — so the default rewrites
    /// the whole ~260 KB `egui_state.ron` twice a minute for the app's entire
    /// uptime. State is also saved on exit, so a long interval loses nothing.
    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(300)
    }

    /// Non-drawing logic. eframe also calls this while the window is hidden when
    /// `request_repaint` was called — which is how the tray's "Open" wakes us.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── UI-freeze watchdog heartbeat ──────────────────────────────────────
        // Stamp "the UI thread is alive" at the start of every frame. A frame that
        // enters here and never returns — or whose subsequent egui paint hangs
        // (e.g. a GPU emote-texture stall) — stops beating, and the watchdog thread
        // surfaces a native dialog instead of a silent freeze. The ≥1 fps repaint
        // floor keeps a *healthy* idle (reactive) UI beating so it never
        // false-alarms; while minimised the OS legitimately stops delivering
        // frames, so we mark the heartbeat inactive to suppress the alarm there.
        self.heartbeat.beat();
        self.heartbeat.set_activity(crate::watchdog::Activity::Frame);
        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        self.heartbeat.set_active(!minimized);
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        self.apply_font_settings(ctx);

        // ── One-shot startup window-size self-heal ──────────────────────
        // A previous session that closed while minimized can leave a
        // degenerate (0×0) window size persisted (Windows reports a
        // minimized window's client area as zero) — eframe/winit only
        // floors that to a generic 64×64 on restore, not this app's real
        // usable minimum (`with_min_inner_size` in main.rs), so the window
        // opens as an unusable sliver until the user manually drags an
        // edge. Nothing this far below the real minimum can be a legitimate
        // interactive resize (the OS enforces that minimum while dragging),
        // so it's safe to correct unconditionally, once, as soon as the
        // backend reports real geometry (which can be `None` for the first
        // frame or two).
        if !self.startup_window_size_checked
            && let Some(rect) = ctx.input(|i| i.viewport().inner_rect)
        {
            self.startup_window_size_checked = true;
            if rect.width() < 300.0 || rect.height() < 200.0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(960.0, 600.0)));
                info!(
                    width = rect.width(),
                    height = rect.height(),
                    "startup: restored window size was degenerate — reset to default"
                );
            }
        }

        self.pump_messages(ctx);
        // Install filesystem-probe results the background worker finished
        // since last frame (never blocks — see `FsProbes`).
        self.fs_probes.lock().unwrap().drain_results();
        self.drain_rescan_disk_usage();
        self.drain_pending_browse();
        self.drain_pending_save();
        self.drain_pending_reload();
        self.drain_pending_schedule();

        // Slow-cadence background reload: routine polls update the DB (last
        // checked, recording metadata) without emitting an event, so sorted
        // columns would drift stale until F5. A 30s re-read keeps the grid —
        // and therefore its sort order — current without user action.
        let now = now_unix();
        if now - self.last_auto_reload >= 30 {
            self.last_auto_reload = now;
            self.spawn_pending_reload();
            // Bound the probe cache: age out entries no longer being rendered.
            // (Never clear() wholesale — that used to force every visible path
            // back through a probe in a single frame.)
            self.fs_probes.lock().unwrap().evict_unused();
        }

        // Keep repainting at 50ms while a background DB load is in-flight so
        // the result is shown as soon as it arrives, not after the 1s heartbeat.
        if self.pending_save.is_some()
            || self.pending_reload.is_some()
            || self.pending_schedule.is_some()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        // Close button hides to tray unless we're really quitting — or the OS
        // session is ending, where cancelling the close would hold up the
        // shutdown: let it through as a detach-quit instead.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            if crate::platform::session_ending() {
                self.request_quit_detach(ctx);
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        // Deliberately here and NOT in `ui()` — see `popup_windows`'s docs:
        // eframe skips `ui()` while the root window is minimized or hidden,
        // and a deferred viewport that isn't re-declared during a pass gets
        // its native window destroyed by egui's end-of-pass GC.
        self.popup_windows(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_shortcuts(ui.ctx());
        // Arm/disarm widget registration before anything draws, so pushes go
        // live the same frame F12 turns the inspector on.
        crate::inspector::set_enabled(self.show_inspector);

        egui::Panel::top("top")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    // The brand label doubles as an app menu with the two
                    // quit actions, so quitting never REQUIRES the tray icon
                    // (hard to reach mid notification storm).
                    ui.menu_button(egui::RichText::new("StreamArchiver").heading(), |ui| {
                        if ui
                            .button("⏻ Quit (keep recording)")
                            .on_hover_text(
                                "Close the app while active recordings, downloads and chat \
                                 sidecars keep running detached — the next launch re-attaches \
                                 to them. Same as the tray icon's Quit.",
                            )
                            .clicked()
                        {
                            ui.close();
                            self.request_quit_detach(ui.ctx());
                        }
                        if ui
                            .button("⏹ Quit & stop recordings")
                            .on_hover_text(
                                "Stop all active recordings (files are finalized), then close \
                                 the app. Asks for confirmation first. Same as the tray \
                                 icon's Quit & stop recordings.",
                            )
                            .clicked()
                        {
                            ui.close();
                            self.confirm_quit_stop = Some(ConfirmDialogState::open(()));
                        }
                    })
                    .response
                    .on_hover_text("Quit options — the same two actions as the tray icon menu.");
                    ui.separator();

                    // ── All view tabs, collapsing into » before they can ever
                    // reach the right-aligned status buttons. Icon-only —
                    // hover shows the full name (plus a description for the
                    // less-obvious ones) — so ten tabs cost roughly what a
                    // handful of words used to. ──
                    let mut all_tabs: Vec<(View, &str, &str, &str)> = vec![
                        (View::Streams, "📺", "Streams", ""),
                        (View::Videos, "🎬", "Videos", ""),
                        (View::Schedule, "🗓", "Schedule", ""),
                        (View::Posts, "📣", "Posts", ""),
                        (
                            View::Background,
                            "🎛",
                            "Background",
                            "Background jobs and periodic fetcher toggles.",
                        ),
                        (
                            View::Files,
                            "📁",
                            "Files",
                            "Recording file paths: drive mapping, batch output-directory \
                             edits, DB path relocation.",
                        ),
                        (
                            View::Backlog,
                            "📥",
                            "Backlog",
                            "Streams awaiting a watch decision: unwatched, started, or skipped.",
                        ),
                        (
                            View::StreamHistory,
                            "🗃",
                            "Stream History",
                            "Full cross-channel recording history — VOD, remux, and \
                             chapters status filters.",
                        ),
                        (
                            View::Trash,
                            "🗑",
                            "Trash",
                            "History of automatic media disposals — trash folder / Recycle \
                             Bin / permanent — grouped by channel. Restore or permanently \
                             delete a soft-deleted (trash-folder) file.",
                        ),
                        (
                            View::Users,
                            "👤",
                            "Users",
                            "Expanded user info.",
                        ),
                        (
                            View::ChannelStats,
                            "📈",
                            "Channel Stats",
                            "Per-channel viewer/follower history graphs, sub/bits/raid \
                             events, and collab overview.",
                        ),
                        (
                            View::Stats,
                            "📊",
                            "App Stats",
                            "App/system health: OCR usage, API quota, detection/poll \
                             health, recording totals, capture health. Per-channel stats \
                             live in Channel Stats.",
                        ),
                        (
                            View::IoMonitor,
                            "🖴",
                            "I/O monitor",
                            "Live disk & network I/O monitor (per-category attribution, \
                             gate queues).",
                        ),
                    ];
                    if debug_view_enabled() {
                        all_tabs.push((View::Debug, "🐞", "Debug", "Internal debug view."));
                    }
                    // Everything on this left-hand side (tabs, », 📖, ⚙,
                    // ⋯) renders at 2x the normal button font — the right-hand
                    // status cluster stays at its usual size, rendered further
                    // below in its own `with_layout`. `big_font` is what every
                    // `RichText` on this side uses; `item_w`'s galley
                    // measurement uses the SAME font so the overflow budget
                    // below matches what's actually painted.
                    let base_font = egui::TextStyle::Button.resolve(ui.style());
                    let big_font = egui::FontId::new(base_font.size * 2.0, base_font.family.clone());
                    // Approximate on-screen width of a button-like widget with
                    // `label` (galley + button padding + item spacing) — egui
                    // caches galleys, so this is cheap per frame.
                    let item_w = |ui: &egui::Ui, label: &str| -> f32 {
                        ui.painter()
                            .layout_no_wrap(label.to_string(), big_font.clone(), egui::Color32::WHITE)
                            .rect
                            .width()
                            + 2.0 * ui.spacing().button_padding.x
                            + ui.spacing().item_spacing.x
                    };
                    // Tab labels are normally just the icon, but Trash carries a
                    // count while files sit in a trash folder — those only leave
                    // by hand, so an unvisited Trash view is exactly how a
                    // drive quietly fills up. Computed once and used for BOTH
                    // the width budget and the paint, so a badge appearing
                    // can't desync the overflow calculation.
                    // Streams "<recording>/<live>" badge, Posts unread-post
                    // badge, and Videos active-download badge — all cheap
                    // in-memory counts (no DB call, unlike the Trash/bell
                    // badges above which need a synchronous SQLite read),
                    // computed once here and used for both the width budget
                    // and the paint below, same reasoning as Trash's.
                    let active_ids: HashSet<i64> =
                        self.core.active.lock().unwrap().keys().copied().collect();
                    let finalizing_ids: HashSet<i64> =
                        self.core.finalizing.lock().unwrap().keys().copied().collect();
                    let rec_now = active_ids.iter().filter(|id| !finalizing_ids.contains(id)).count();
                    let rows_ref: Vec<&MonitorWithChannel> = self.rows.iter().collect();
                    let live_now = channel_live_count(&rows_ref, &active_ids);
                    let videos_active =
                        self.videos.iter().filter(|v| v.status == "downloading").count();
                    let tab_label = |v: &View, icon: &str| -> String {
                        match v {
                            View::Trash if self.trash_badge > 0 => format!("{icon} {}", self.trash_badge),
                            View::Posts if self.posts_unread > 0 => format!("{icon} {}", self.posts_unread),
                            View::Streams if live_now > 0 => format!("{icon} {rec_now}/{live_now}"),
                            View::Videos if videos_active > 0 => format!("{icon} {videos_active}"),
                            _ => icon.to_string(),
                        }
                    };
                    let labels: Vec<String> =
                        all_tabs.iter().map(|(v, icon, ..)| tab_label(v, icon)).collect();
                    let widths: Vec<f32> = labels.iter().map(|l| item_w(ui, l)).collect();
                    let fixed_w: f32 = ["📖", "⚙", "⋯"].iter().map(|l| item_w(ui, l)).sum();
                    // The right cluster's width is only known from last frame
                    // (it renders after us); first frame reserves generously.
                    let right_reserved = if self.topbar.right_w > 0.0 {
                        self.topbar.right_w
                    } else {
                        600.0
                    };
                    let budget =
                        (ui.available_width() - right_reserved - fixed_w - 16.0).max(0.0);
                    let visible = partition_tabs(
                        &widths,
                        budget,
                        item_w(ui, "»"),
                        self.topbar.visible,
                        16.0,
                    );
                    self.topbar.visible = visible;

                    let mut switch: Option<View> = None;
                    for ((v, _, name, hover), label) in
                        all_tabs.iter().zip(labels.iter()).take(visible)
                    {
                        let mut hover_text =
                            if hover.is_empty() { name.to_string() } else { format!("{name}\n{hover}") };
                        if *v == View::Trash && self.trash_badge > 0 {
                            hover_text.push_str(&format!(
                                "\n\n⚠ {} file(s) are sitting in a trash folder — still taking up \
                                 space on the recordings drive until you restore or permanently \
                                 delete them here.",
                                self.trash_badge
                            ));
                        }
                        if *v == View::Posts && self.posts_unread > 0 {
                            hover_text.push_str(&format!(
                                "\n\n{} unread post(s) — cleared the same way as the 🔔 feed's \
                                 \"Mark all read\" (opening this tab alone doesn't clear it).",
                                self.posts_unread
                            ));
                        }
                        if *v == View::Streams && live_now > 0 {
                            hover_text.push_str(&format!(
                                "\n\n{rec_now} recording out of {live_now} currently live."
                            ));
                        }
                        if *v == View::Videos && videos_active > 0 {
                            hover_text.push_str(&format!("\n\n{videos_active} download(s) in progress."));
                        }
                        let text = egui::RichText::new(label).font(big_font.clone());
                        // Amber while the trash holds something, so it reads as
                        // "there is something to deal with" at a glance.
                        let text = if *v == View::Trash && self.trash_badge > 0 {
                            text.color(egui::Color32::from_rgb(220, 160, 60))
                        } else {
                            text
                        };
                        let resp = ui
                            .selectable_label(self.view == *v, text)
                            .on_hover_text(hover_text);
                        let resp = if *v == View::Streams {
                            resp.inspect("View tab: Streams", &[])
                        } else {
                            resp
                        };
                        if resp.clicked() {
                            switch = Some(*v);
                        }
                    }
                    if visible < all_tabs.len() {
                        ui.menu_button(egui::RichText::new("»").font(big_font.clone()), |ui| {
                            for ((v, _, name, _), label) in
                                all_tabs.iter().zip(labels.iter()).skip(visible)
                            {
                                if ui
                                    .selectable_label(self.view == *v, format!("{label} {name}"))
                                    .clicked()
                                {
                                    switch = Some(*v);
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .on_hover_text("Tabs that don't fit at this window width.");
                    }

                    // ── ⋯ Display: the two display toggles that used to share
                    // the Views ▾ menu with the view links above (now icon-only
                    // tabs in the main row). Stays open on toggle clicks. ──
                    ui.menu_button(egui::RichText::new("⋯").font(big_font.clone()), |ui| {
                        if ui
                            .checkbox(&mut self.status_bgcolor, "Status bgcolor")
                            .on_hover_text(
                                "Tint Streams rows by status (recording / ad playing / \
                                 failed). Row selection is still highlighted when this \
                                 is off.",
                            )
                            .changed()
                        {
                            let _ = self.core.store.set_setting(
                                K_STATUS_BGCOLOR,
                                if self.status_bgcolor { "1" } else { "0" },
                            );
                        }
                        if ui
                            .checkbox(&mut self.shorten_timestamps, "Short timestamps")
                            .on_hover_text(
                                "Show timestamps in a compact short format (e.g. \
                                 21/06 14:02) instead of the full datetime. Hover any \
                                 timestamp for the full value. The short format is \
                                 configurable in Settings → Display.",
                            )
                            .changed()
                        {
                            set_short_ts(self.shorten_timestamps);
                            let _ = self.core.store.set_setting(
                                K_SHORT_TIMESTAMPS,
                                if self.shorten_timestamps { "1" } else { "0" },
                            );
                        }
                    })
                    .response
                    .on_hover_text("Display options: row status coloring, timestamp format.");

                    // ── 📖 Help — About used to be a separate dropdown entry,
                    // but it's just the first page of the Help view's own
                    // sidebar (`selected == 0`), not a distinct destination —
                    // one icon button for both. ──
                    if ui
                        .selectable_label(
                            self.view == View::Help,
                            egui::RichText::new("📖").font(big_font.clone()),
                        )
                        .on_hover_text(
                            "Help\nThe full manual, in-app (works offline — it's embedded \
                             in the binary). Version/build info and data locations are the \
                             \"About\" page inside it.",
                        )
                        .clicked()
                    {
                        switch = Some(View::Help);
                    }

                    // ── ⚙ Settings ──
                    if ui
                        .selectable_label(
                            self.view == View::Settings,
                            egui::RichText::new("⚙").font(big_font.clone()),
                        )
                        .on_hover_text("Settings (Ctrl+,)")
                        .clicked()
                    {
                        switch = Some(View::Settings);
                    }

                    if let Some(v) = switch {
                        self.switch_view(v);
                    }

                    let right_w = ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Pinned far-right and shown on every view — the process
                        // manager is a global utility, not Background-specific.
                        {
                            // Count stays live even while the window is closed —
                            // `processes_window` refreshes it on a slower
                            // throttle regardless (see there).
                            let n = self.processes.len();
                            let label = if n > 0 { format!("🖥 {n}") } else { "🖥".to_string() };
                            if ui
                                .button(label)
                                .on_hover_text(
                                    "Process manager\nAll spawned download tool processes \
                                     (recordings, videos, chat) — PIDs, status, and manual \
                                     Stop / Kill.",
                                )
                                .clicked()
                            {
                                self.show_processes = true;
                                self.processes_refreshed = None; // force an immediate refresh
                            }
                        }
                        {
                            let quota_warnings = self.active_quota_warnings();
                            let n = self.issues_recs.len() + self.issues_missing.len()
                                + self.issues_errors.len() + self.issues_errors_no_file.len()
                                + self.issues_stuck.len() + self.issues_muted_vod.len()
                                + self.issues_unmerged.len() + self.issues_head_mismatch.len()
                                + self.issues_gap_splice.len()
                                + quota_warnings.len();
                            let label = if n > 0 { format!("⚠ {n}") } else { "⚠".to_string() };
                            let btn = egui::Button::new(label).small();
                            let btn = if n > 0 {
                                btn.fill(egui::Color32::from_rgb(160, 90, 10))
                            } else {
                                btn
                            };
                            if ui
                                .add(btn)
                                .on_hover_text(
                                    "Issues\nRecordings and quota warnings that need attention",
                                )
                                .clicked()
                            {
                                self.show_issues = true;
                                self.issues_refreshed = None;
                            }
                        }
                        {
                            // Capture warnings (🚨). Badge = unacked alerts
                            // scraped from the capture tools' own logs; red
                            // fill when any is an ERROR (lost data), yellow
                            // when only warnings. Counts cached on the same
                            // throttle style as the bell (see
                            // `warnings_window`).
                            let (errs, warns) = self.warn_badge;
                            let label = match (errs, warns) {
                                (0, 0) => "🚨".to_string(),
                                (0, w) => format!("🚨 {w}"),
                                (e, 0) => format!("🚨 {e}"),
                                (e, w) => format!("🚨 {e}+{w}"),
                            };
                            let btn = egui::Button::new(label).small();
                            let btn = if errs > 0 {
                                btn.fill(egui::Color32::from_rgb(140, 30, 30))
                            } else if warns > 0 {
                                btn.fill(egui::Color32::from_rgb(140, 110, 10))
                            } else {
                                btn
                            };
                            if ui
                                .add(btn)
                                .on_hover_text(
                                    "Warnings\nProblems reported by the capture tools' own \
                                     logs: lost segments / sequence gaps (errors — data is \
                                     missing from the capture), failed fetches, and tool \
                                     warnings. Red = unacknowledged errors, yellow = warnings \
                                     only.",
                                )
                                .clicked()
                            {
                                self.show_warnings = true;
                                self.warn_refreshed = None; // force an immediate refresh
                            }
                        }
                        {
                            // Notifications feed (bell). Mirrors the Issues button:
                            // the unread badge count is cached (refreshed on the
                            // Issues-style throttle in `notifications_window`, even
                            // while the window is closed) so it stays live.
                            let n = self.notif_unread;
                            let label = if n > 0 {
                                format!("🔔 {n}")
                            } else {
                                "🔔".to_string()
                            };
                            let btn = egui::Button::new(label).small();
                            let btn = if n > 0 {
                                btn.fill(egui::Color32::from_rgb(160, 90, 10))
                            } else {
                                btn
                            };
                            if ui
                                .add(btn)
                                .on_hover_text("Notifications: went-live, recordings, errors, schedule changes, YouTube posts")
                                .clicked()
                            {
                                self.show_notifications = true;
                                self.notif_refreshed = None; // force an immediate refresh
                            }
                        }
                        if ui
                            .button("🖹")
                            .on_hover_text(
                                "Log\nLive, filterable, colored view of the app's own tracing \
                                 output — search, minimum severity, platform filter. Same \
                                 events as the console/file log, without needing either open.",
                            )
                            .clicked()
                        {
                            self.show_log_view = true;
                        }
                        {
                            let n = self.scheduled_recordings.iter().filter(|r| r.rec.enabled).count();
                            let label = if n > 0 { format!("📅 {n}") } else { "📅".to_string() };
                            if ui
                                .button(label)
                                .on_hover_text(
                                    "Scheduled rec\nRecordings scheduled to force-start at a \
                                     specific time or on a weekly repeat, bypassing Auto — for \
                                     channels you don't want kept on Auto.",
                                )
                                .clicked()
                            {
                                self.show_scheduled_recordings = true;
                            }
                        }
                        // "📣🗗" (not bare 📣) — the left-hand Posts TAB already
                        // owns plain 📣; the trailing pop-out glyph is what
                        // keeps this a visually distinct button.
                        if ui
                            .button("📣🗗")
                            .on_hover_text("Pop out Posts\nOpens the YouTube posts feed in its own window")
                            .clicked()
                        {
                            self.show_posts_window = true;
                            self.posts_refreshed = None;
                        }
                        // Report the cluster's used width for next frame's
                        // tab-overflow budget (it renders after the tabs, so
                        // the current frame can only know last frame's value).
                        ui.min_rect().width()
                    }).inner;
                    self.topbar.right_w = right_w;
                });
            });

        // Streams-specific toolbar (add/group/filter/view controls) gets its
        // OWN row rather than competing with the tabs + global status icons
        // above for the same horizontal space — that packed single-row
        // layout used to silently overlap/clip its leftmost items on a
        // narrow window instead of reflowing. `horizontal_wrapped` further
        // guarantees no item is ever clipped: if even this row's full width
        // isn't enough, it wraps to a third row rather than overlapping
        // anything below it.
        if self.view == View::Streams {
            egui::Panel::top("streams_toolbar").resizable(false).show_inside(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("➕ Stream")
                        .on_hover_text(
                            "Add stream\nCreate a channel with its first instance \
                             (a URL to record)",
                        )
                        .clicked()
                    {
                        self.form = Some(Arc::new(Mutex::new(MonitorForm::new_channel(
                            &self.monitor_defaults,
                            &self.settings.default_output_dir,
                        ))));
                    }
                    if ui
                        .button("➕ Channel")
                        .on_hover_text(
                            "Add channel\nCreate an empty channel container; add \
                             instances to it afterwards",
                        )
                        .clicked()
                    {
                        self.channel_form = Some(Arc::new(Mutex::new(ChannelForm {
                            id: None,
                            name: String::new(),
                            color: String::new(),
                            vod_download: None,
                            vod_replace: None,
                            head_backfill_fetch: None,
                            head_backfill_replace: None,
                            join_cleanup: None,
                            disposal_method: None,
                            rolling: None,
                            rolling_ttl_hours: String::new(),
                            primary_platform_pref: None,
                            simulcast_pref: None,
                            simulcast_ad_free_pref: None,
                            chapters_enabled: None,
                            chapters_coalesce_secs: String::new(),
                            follow_my_raids: None,
                            record_me_as_raid_target: None,
                            follow_my_raids_play: None,
                            exclude_from_auto_play: None,
                            allow_delete: false,
                            primary_group: None,
                            groups: Default::default(),
                            do_save: false,
                            closed: false,
                            channel_groups: Vec::new(),
                        })));
                    }
                    if ui
                        .button("🏷 Groups")
                        .on_hover_text(
                            "Manage channel groups\nCreate/rename/delete groups; assign \
                             a channel's primary + secondary groups from its own \
                             Properties dialog.",
                        )
                        .clicked()
                    {
                        self.show_group_manager = true;
                    }
                    {
                        // Extracted local: a ComboBox iterating
                        // `self.channel_groups` while also mutating
                        // `self.streams_group_filter` inside the same
                        // closure is exactly the disjoint-field-borrow
                        // shape the channel/group-manager dialogs hit —
                        // see their own comments on the same pattern.
                        let groups = self.channel_groups.clone();
                        let selected_label = self
                            .streams_group_filter
                            .and_then(|gid| groups.iter().find(|g| g.id == gid))
                            .map(|g| g.name.as_str())
                            .unwrap_or("All channels");
                        egui::ComboBox::from_id_salt("streams_group_filter")
                            .selected_text(selected_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(self.streams_group_filter.is_none(), "All channels")
                                    .clicked()
                                {
                                    self.streams_group_filter = None;
                                    self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                                }
                                for g in &groups {
                                    if ui
                                        .selectable_label(
                                            self.streams_group_filter == Some(g.id),
                                            &g.name,
                                        )
                                        .clicked()
                                    {
                                        self.streams_group_filter = Some(g.id);
                                        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                                    }
                                }
                            })
                            .response
                            .on_hover_text(
                                "Narrow the Streams grid to one group's members \
                                 (primary or secondary) — the primary-group headers \
                                 above don't apply while this is set.",
                            );
                    }
                    {
                        let rgroups = self.recording_groups.clone();
                        let selected_label = self
                            .streams_recording_group_filter
                            .and_then(|gid| rgroups.iter().find(|g| g.id == gid))
                            .map(|g| g.name.as_str())
                            .unwrap_or("All streams");
                        egui::ComboBox::from_id_salt("streams_recording_group_filter")
                            .selected_text(selected_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        self.streams_recording_group_filter.is_none(),
                                        "All streams",
                                    )
                                    .clicked()
                                {
                                    self.streams_recording_group_filter = None;
                                    self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                                }
                                for g in &rgroups {
                                    if ui
                                        .selectable_label(
                                            self.streams_recording_group_filter == Some(g.id),
                                            &g.name,
                                        )
                                        .clicked()
                                    {
                                        self.streams_recording_group_filter = Some(g.id);
                                        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                                    }
                                }
                            })
                            .response
                            .on_hover_text(
                                "Narrow the Streams grid to one recording group's \
                                 streams (e.g. \"Numi Subathon 2025\") — channels/\
                                 instances with no matching stream are hidden, and the \
                                 ones that remain force-expand down to their matching \
                                 streams. Select streams (ctrl/shift-click) and use \
                                 \"➕ Add to group…\" to build one.",
                            );
                    }
                    if ui
                        .checkbox(&mut self.streams_group_visually, "Group")
                        .on_hover_text(
                            "Cluster channels under their primary-group headers \
                             (🏷 Groups). Off shows a flat list even for channels \
                             that have a group assigned — handy for a \"flat, \
                             sorted by last added\" view.",
                        )
                        .changed()
                    {
                        let _ = self.core.store.set_setting(
                            K_STREAMS_GROUP_VISUALLY,
                            if self.streams_group_visually { "1" } else { "0" },
                        );
                        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                    }
                    if ui
                        .checkbox(&mut self.streams_only_recorded, "Only stored")
                        .on_hover_text(
                            "Hide any channel/instance/stream with no take that \
                             actually has a file on disk — detected-but-never-\
                             recorded (Auto off) and failed/missed streams disappear. \
                             The ones that remain force-expand down to their stored \
                             takes, same as a Recording group filter.",
                        )
                        .changed()
                    {
                        let _ = self.core.store.set_setting(
                            K_STREAMS_ONLY_RECORDED,
                            if self.streams_only_recorded { "1" } else { "0" },
                        );
                        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                    }
                    if ui
                        .checkbox(
                            &mut self.streams_allow_delete,
                            egui::RichText::new("Allow deletion").color(grid::HL_ERROR_TEXT),
                        )
                        .on_hover_text(
                            "Master switch for the take-row \"🗑🔥 Delete file from \
                             disk\" action, which permanently removes a captured file \
                             (moved to trash/Recycle Bin, or deleted outright, per \
                             Settings → Automatic deletion) while keeping its history \
                             row. Off by default — this alone doesn't enable the \
                             action, it only ever UNBLOCKS it: the channel AND the \
                             instance each also need their own \"Allow delete\" \
                             setting on (Rename channel / Edit instance) before the \
                             menu item lights up for a given take. Deliberately three \
                             independent off-by-default gates, so this can't be \
                             triggered by an accidental click.",
                        )
                        .changed()
                    {
                        let _ = self.core.store.set_setting(
                            crate::manual_delete::K_STREAMS_ALLOW_DELETE,
                            if self.streams_allow_delete { "1" } else { "0" },
                        );
                    }
                    {
                        let selected_label =
                            self.streams_active_view.as_deref().unwrap_or("Views");
                        egui::ComboBox::from_id_salt("streams_view_select")
                            .selected_text(selected_label)
                            // ComboBox defaults to closing on ANY click inside
                            // its popup (egui::PopupCloseBehavior::CloseOnClick,
                            // meant for plain option lists) — this popup also
                            // holds a TextEdit and per-row action buttons the
                            // user needs to interact with repeatedly without the
                            // popup vanishing after the first click.
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                            .show_ui(ui, |ui| {
                                self.views_combo_popup(ui);
                            })
                            .response
                            .on_hover_text(
                                "Streams grid views — named presets of this grid's \
                                 sort, \"Group\" toggle, per-column filters, and \
                                 Group/Recording group selections. Open it to apply, \
                                 save, rename, or delete one.",
                            );
                    }
                    if ui
                        .button("⇔")
                        .on_hover_text("Auto-fit all columns to their content width")
                        .clicked()
                    {
                        self.reset_streams_columns = true;
                    }
                    if ui
                        .button("🔄 Rescan disk usage")
                        .on_hover_text(
                            "Check every stored take of every channel against disk and \
                             clear any whose file is gone (e.g. deleted outside the app) \
                             — the 💾 Disk use column otherwise keeps counting it. Runs \
                             in the background; a single channel/instance can be rescanned \
                             on its own from its right-click menu instead.",
                        )
                        .clicked()
                    {
                        let mids: Vec<i64> = self.rows.iter().map(|r| r.monitor.id).collect();
                        self.status = "Rescanning disk usage…".into();
                        self.start_rescan_disk_usage(mids);
                    }
                });
            });
        }

        egui::Panel::bottom("status")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(if self.status.is_empty() {
                        "Ready."
                    } else {
                        &self.status
                    });
                });
            });

        let panel_resp = egui::CentralPanel::default().show_inside(ui, |ui| match self.view {
            View::Streams => self.channels_view(ui),
            View::Videos => self.videos_view(ui),
            View::Schedule => self.schedule_view(ui),
            View::Posts => self.posts_view(ui),
            View::Background => self.background_view(ui),
            View::Files => self.files_view(ui),
            View::Backlog => self.backlog_view(ui),
            View::StreamHistory => self.stream_history_view(ui),
            View::Trash => self.trash_view(ui),
            View::Settings => self.settings_view(ui),
            View::ChannelStats => self.channel_stats_view(ui),
            View::Stats => self.stats_view(ui),
            View::IoMonitor => self.io_view(ui),
            View::Debug => self.debug_view(ui),
            View::Users => self.users_view(ui),
            View::Help => self.help_view(ui),
        });

        // ── Main-panel context menu (right-click on empty space) ──
        let view = self.view;
        let mut ctx_add_stream = false;
        let mut ctx_add_channel = false;
        let mut ctx_refresh_schedule = false;
        let mut ctx_open_proc_mgr = false;
        let mut ctx_save_settings = false;
        panel_resp.response.context_menu(|ui| {
            match view {
                View::Streams => {
                    if ui.button("➕  Add stream").clicked() {
                        ctx_add_stream = true;
                        ui.close();
                    }
                    if ui.button("➕  Add channel").clicked() {
                        ctx_add_channel = true;
                        ui.close();
                    }
                }
                View::Schedule => {
                    if ui.button("⟳  Fetch now").clicked() {
                        ctx_refresh_schedule = true;
                        ui.close();
                    }
                }
                View::Background => {
                    if ui.button("🖥  Process manager").clicked() {
                        ctx_open_proc_mgr = true;
                        ui.close();
                    }
                }
                View::Settings => {
                    if ui.button("💾  Save settings").clicked() {
                        ctx_save_settings = true;
                        ui.close();
                    }
                }
                View::Videos | View::ChannelStats | View::Stats | View::IoMonitor
                | View::Debug | View::Posts | View::Files | View::Help
                | View::Backlog | View::StreamHistory | View::Trash | View::Users => {}
            }
        });
        if ctx_add_stream {
            self.form = Some(Arc::new(Mutex::new(MonitorForm::new_channel(
                &self.monitor_defaults,
                &self.settings.default_output_dir,
            ))));
        }
        if ctx_add_channel {
            self.channel_form = Some(Arc::new(Mutex::new(ChannelForm {
                id: None,
                name: String::new(),
                color: String::new(),
                vod_download: None,
                vod_replace: None,
                head_backfill_fetch: None,
                head_backfill_replace: None,
                join_cleanup: None,
                disposal_method: None,
                rolling: None,
                rolling_ttl_hours: String::new(),
                primary_platform_pref: None,
                simulcast_pref: None,
                simulcast_ad_free_pref: None,
                chapters_enabled: None,
                chapters_coalesce_secs: String::new(),
                follow_my_raids: None,
                record_me_as_raid_target: None,
                follow_my_raids_play: None,
                exclude_from_auto_play: None,
                allow_delete: false,
                primary_group: None,
                groups: Default::default(),
                do_save: false,
                closed: false,
                channel_groups: Vec::new(),
            })));
        }
        if ctx_refresh_schedule {
            self.core.request_schedule_refresh();
            self.spawn_reload_schedule();
            self.status = "Fetching latest schedules…".into();
        }
        if ctx_open_proc_mgr {
            self.show_processes = true;
            self.processes_refreshed = None;
        }
        if ctx_save_settings {
            self.save_settings(ui.ctx());
        }

        draw_alt_image_preview(ui.ctx());

        // Must remain the FINAL statement of ui(): the child-viewport windows
        // above register their widgets after the root CentralPanel, so an
        // earlier drain would split one frame's widgets across two snapshots.
        self.inspector.lock().unwrap().end_frame(self.show_inspector);
    }
}
