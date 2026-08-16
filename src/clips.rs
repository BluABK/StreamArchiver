//! Clip discovery: listing a channel's (and a VOD's) clips, and keeping the
//! keys that make a vanished clip recoverable.
//!
//! Two facts about Twitch's API shape the whole module, both measured against
//! the live Helix API on 2026-08-16 rather than assumed:
//!
//! 1. **The recovery keys are perishable.** `video_id` and `vod_offset` are
//!    present on 100% of clips up to 14 days old, 68% at 30 days, 19% at 90 and
//!    5% at a year — Twitch nulls them when the parent VOD expires. Indexing a
//!    clip inside its VOD's lifetime is the only chance to capture them, which
//!    is why the post-broadcast sweep exists and why [`Store::upsert_clip`]
//!    refuses to blank a key it already holds.
//!
//! 2. **A query returns at most ~1000 clips**, however far you paginate. Worse,
//!    Helix orders by view count *within the window*, so a capped window
//!    silently drops the **least-viewed** clips — exactly the obscure ones an
//!    archive exists to preserve. One channel measured 1,100 clips reachable by
//!    plain pagination but 7,588 via year windows, with 6 of 8 years still
//!    capped. Date-window bisection ([`bisect`]) is the only correct answer, and
//!    every truncation is logged rather than passed over in silence.
//!
//! Auth is the **app** access token ([`DetectContext::twitch_helix_auth`]), so
//! no new OAuth scope is needed and no existing grant is invalidated.

#![allow(dead_code)] // consumed by the sweep/UI phases; see `store::clips`

use crate::detectors::{DetectContext, parse_rfc3339};
use crate::models::{Clip, Platform};
use crate::store::Store;
use std::sync::Arc;
use tracing::{debug, warn};

mod fetch;
// `download_allowed` / `enqueue_clip_download` join this list when the Clips
// view gates its Download action on them; re-exporting now would only be unused
// imports (the queue drainer calls them from inside this module).
pub use fetch::{dispose_clip_media, download_master_on, drain_clip_queue};
mod sweep;
pub use sweep::{clips_enabled, maybe_sweep_post_broadcast, run_clip_sweep};

/// Master switch: index clip metadata for monitored channels. Cheap (tens of MB
/// for the whole archive) and it is what makes later recovery possible at all.
pub const K_CLIPS_ENABLED: &str = "clips_enabled";
/// Global download switch. **Off by default** — downloading every clip of every
/// monitored channel measured at roughly 200 GB per active channel.
pub const K_CLIPS_DOWNLOAD: &str = "clips_download";
/// Per-channel download opt-in (a bool scope map). ANDed with
/// [`K_CLIPS_DOWNLOAD`]; both must be on, neither inherits a default.
pub const K_CHANNEL_CLIPS_DOWNLOAD: &str = "channel_clips_download";
/// Allow the expensive historical window-bisection backfill. Off by default;
/// the daily and post-broadcast sweeps are cheap, this is thousands of requests.
pub const K_CLIPS_BACKFILL: &str = "clips_backfill";
/// Hours after a broadcast ends at which the two post-broadcast sweeps run.
pub const K_CLIPS_POST_OFFSETS: &str = "clips_post_broadcast_offsets";

/// Default post-broadcast sweep offsets: +2 h catches the burst made during and
/// right after the stream, +24 h catches the long tail — both comfortably
/// inside the parent VOD's lifetime, which is the point.
pub const DEFAULT_POST_OFFSETS: (i64, i64) = (2 * 3600, 24 * 3600);

/// Helix's per-page maximum.
const PAGE_SIZE: usize = 100;
/// 10 x 100 = the ~1000 the clips service will return for one query. Reaching
/// this with a cursor still in hand means the window was truncated.
const MAX_PAGES: usize = 10;
/// Never bisect below an hour: a single hour that still caps is a genuinely
/// extraordinary clip storm, and splitting further burns requests for nothing.
const MIN_WINDOW_SECS: i64 = 3600;
/// Minimum spacing between clip requests. The app token's quota is shared with
/// live detection, VOD polling and schedule refresh — a backfill must never be
/// the reason a recording starts late.
pub const REQUEST_PACE_MS: u64 = 250;

/// A half-open `[start, end)` range over clip **creation** time (unix seconds).
///
/// Note this filters on when the *clip* was made, not when the parent broadcast
/// happened — which is exactly what lets an incremental sweep pick up a clip
/// someone cut from a three-year-old VOD this morning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: i64,
    pub end: i64,
}

impl Window {
    pub fn new(start: i64, end: i64) -> Window {
        Window { start, end }
    }
    pub fn secs(&self) -> i64 {
        (self.end - self.start).max(0)
    }
}

/// Split a capped window in half, or `None` when it can no longer be usefully
/// halved (the caller then accepts truncation — and must say so out loud).
pub fn bisect(w: Window) -> Option<(Window, Window)> {
    if w.secs() <= MIN_WINDOW_SECS {
        return None;
    }
    let mid = w.start + w.secs() / 2;
    Some((Window::new(w.start, mid), Window::new(mid, w.end)))
}

/// Did this query hit the service's result ceiling? Only true when we exhausted
/// our page budget *and* Helix still offered a cursor — a query that simply ran
/// out of clips is not truncated.
pub fn cap_hit(pages: usize, cursor_present: bool) -> bool {
    pages >= MAX_PAGES && cursor_present
}

/// Tile `[start, end)` newest-first in `stride`-second windows.
///
/// Newest-first matters: the newest windows are the ones whose clips still carry
/// recovery keys, so an interrupted backfill has already captured the valuable
/// part.
pub fn backfill_windows(start: i64, end: i64, stride: i64) -> Vec<Window> {
    let mut out = Vec::new();
    if end <= start || stride <= 0 {
        return out;
    }
    let mut cur = end;
    while cur > start {
        let lo = (cur - stride).max(start);
        out.push(Window::new(lo, cur));
        cur = lo;
    }
    out
}

/// Format unix seconds as the RFC3339 Helix wants for `started_at`/`ended_at`.
pub fn fmt_rfc3339(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("epoch is representable"))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// A clip identified only by platform + native id, before we know anything else
/// about it — what URL scraping (chat harvest) produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipRef {
    pub platform: Platform,
    /// Twitch slug / YouTube clip id.
    pub slug: String,
    /// The broadcaster login, when the URL form carried one. This is the *login*
    /// (a CDN folder component), not a display name, so it is worth keeping.
    pub login: Option<String>,
}

/// Recognise a clip URL.
///
/// Three forms exist and all appear in chat:
///   `clips.twitch.tv/<slug>`, `twitch.tv/<login>/clip/<slug>`,
///   `youtube.com/clip/<id>`.
/// Only the middle one reveals the broadcaster.
pub fn parse_clip_url(s: &str) -> Option<ClipRef> {
    let t = s.trim();
    let lower = t.to_lowercase();
    // Strip a trailing query/fragment: chat links routinely carry ?t= or
    // ?featured=, and utm noise from mobile shares.
    let cut = |v: &str| -> String { v.split(['?', '#']).next().unwrap_or("").trim_end_matches('/').to_string() };

    if let Some(i) = lower.find("clips.twitch.tv/") {
        let rest = cut(&t[i + "clips.twitch.tv/".len()..]);
        let slug = rest.split('/').next().unwrap_or("");
        return valid_slug(slug).then(|| ClipRef {
            platform: Platform::Twitch,
            slug: slug.to_string(),
            login: None,
        });
    }
    if let Some(i) = lower.find("youtube.com/clip/") {
        let rest = cut(&t[i + "youtube.com/clip/".len()..]);
        let slug = rest.split('/').next().unwrap_or("");
        return valid_slug(slug).then(|| ClipRef {
            platform: Platform::YouTube,
            slug: slug.to_string(),
            login: None,
        });
    }
    if let Some(i) = lower.find("twitch.tv/") {
        let rest = cut(&t[i + "twitch.tv/".len()..]);
        let mut parts = rest.split('/');
        let login = parts.next().unwrap_or("");
        // `/clip/` is the only path segment that makes this a clip; a bare
        // channel URL, /videos/, /about etc. must not match.
        if parts.next() != Some("clip") {
            return None;
        }
        let slug = parts.next().unwrap_or("");
        return (valid_slug(slug) && !login.is_empty()).then(|| ClipRef {
            platform: Platform::Twitch,
            slug: slug.to_string(),
            login: Some(login.to_lowercase()),
        });
    }
    None
}

/// Clip ids/slugs are `[A-Za-z0-9_-]`. Hyphens are load-bearing — every modern
/// Twitch slug has one before its random suffix.
fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Map one Helix clip object onto a [`Clip`].
///
/// `login` is threaded in from the monitor being swept because **Helix does not
/// return `broadcaster_login`** — only `broadcaster_name`, the display name,
/// which is not always the login lowercased (a Japanese or styled display name
/// is not a CDN folder component). Falling back to the lowercased display name
/// is better than nothing but is marked by the caller as such.
pub fn parse_helix_clip(v: &serde_json::Value, login: Option<&str>) -> Option<Clip> {
    let slug = v["id"].as_str().filter(|s| !s.is_empty())?;
    let created_at = v["created_at"].as_str().and_then(parse_rfc3339).unwrap_or(0);
    // Helix reports a float ("43.90"); milliseconds so the recovery window is
    // never silently rounded to the wrong second.
    let duration_ms = v["duration"].as_f64().map(|d| (d * 1000.0).round() as i64).unwrap_or(0);
    let broadcaster_login = login
        .map(str::to_string)
        .or_else(|| v["broadcaster_name"].as_str().map(str::to_lowercase))
        .unwrap_or_default();

    Some(Clip {
        platform: Platform::Twitch,
        slug: slug.to_string(),
        broadcaster_id: v["broadcaster_id"].as_str().unwrap_or_default().to_string(),
        broadcaster_login,
        creator_login: v["creator_name"].as_str().unwrap_or_default().to_string(),
        title: v["title"].as_str().unwrap_or_default().to_string(),
        // Helix returns `game_id`; the display name is resolved later from the
        // games cache, so the raw id is what is stored for now.
        game: v["game_id"].as_str().unwrap_or_default().to_string(),
        language: v["language"].as_str().unwrap_or_default().to_string(),
        view_count: v["view_count"].as_i64().unwrap_or(0),
        duration_ms,
        created_at,
        url: v["url"].as_str().unwrap_or_default().to_string(),
        thumbnail_url: v["thumbnail_url"].as_str().unwrap_or_default().to_string(),
        // The perishable pair. Absent, null and empty-string all mean the same
        // thing here: the parent VOD is gone and Twitch has stopped telling us.
        vod_id: v["video_id"].as_str().unwrap_or_default().to_string(),
        vod_offset_secs: v["vod_offset"].as_i64(),
        source: "helix".into(),
        ..Default::default()
    })
}

/// Who we are sweeping. Bundled because every field travels together through
/// the window/bisect recursion, and the four of them plus context, store, window
/// and clock is past the point where positional arguments stay readable.
#[derive(Clone, Debug)]
pub struct SweepTarget {
    pub channel_id: i64,
    pub monitor_id: i64,
    /// Twitch login (lowercase) — also the CDN folder component, so it is worth
    /// carrying rather than re-deriving from a display name.
    pub login: String,
    /// Helix `broadcaster_id`.
    pub user_id: String,
}

/// Outcome of sweeping one window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepResult {
    /// Clips seen (upserted), including ones we already knew.
    pub seen: usize,
    /// Slugs returned, for the liveness diff.
    pub slugs: Vec<String>,
    /// The window was truncated by the ~1000 cap and needs bisecting.
    pub capped: bool,
}

/// Page one window of `GET /helix/clips` for a broadcaster.
///
/// Returns `Err` for any transport/HTTP failure. The caller **must not** treat
/// that as "no clips" — per the house contract (`detectors.rs:1750`), `Err`
/// means we weren't watching, and advancing the sweep high-water mark past a
/// failed window would leave a permanent hole in the archive.
pub async fn sweep_window(
    ctx: &Arc<DetectContext>,
    store: &Arc<Store>,
    t: &SweepTarget,
    w: Window,
    now: i64,
) -> anyhow::Result<SweepResult> {
    let (client_id, token) = ctx.twitch_helix_auth().await?;
    let client = ctx.http_client();
    let mut out = SweepResult::default();
    let mut cursor: Option<String> = None;
    let (start, end) = (fmt_rfc3339(w.start), fmt_rfc3339(w.end));
    let mut pages = 0usize;

    while pages < MAX_PAGES {
        let mut query: Vec<(&str, &str)> = vec![
            ("broadcaster_id", t.user_id.as_str()),
            ("first", "100"),
            ("started_at", start.as_str()),
            ("ended_at", end.as_str()),
        ];
        if let Some(c) = &cursor {
            query.push(("after", c.as_str()));
        }
        let resp = client
            .get("https://api.twitch.tv/helix/clips")
            .header("Client-Id", &client_id)
            .bearer_auth(&token)
            .query(&query)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 429 {
            // Shared quota: back off the whole sweep rather than grinding.
            anyhow::bail!("helix clips rate limited (429)");
        }
        if !status.is_success() {
            anyhow::bail!("helix clips {status}");
        }
        let v: serde_json::Value = resp.json().await?;
        let data = v["data"].as_array().cloned().unwrap_or_default();
        if data.is_empty() {
            cursor = None;
            break;
        }
        for item in &data {
            let Some(mut c) = parse_helix_clip(item, Some(&t.login)) else {
                continue;
            };
            c.channel_id = Some(t.channel_id);
            c.monitor_id = Some(t.monitor_id);
            out.slugs.push(c.slug.clone());
            if let Err(e) = store.upsert_clip(&c, now) {
                warn!("clips: upsert {} failed: {e:#}", c.slug);
            } else {
                out.seen += 1;
            }
        }
        pages += 1;
        cursor = v["pagination"]["cursor"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        if cursor.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(REQUEST_PACE_MS)).await;
    }

    out.capped = cap_hit(pages, cursor.is_some());
    if out.capped {
        debug!(
            "clips: window {}..{} for {} hit the ~1000 cap ({} seen) — bisecting",
            start, end, t.login, out.seen
        );
    }
    Ok(out)
}

/// Sweep a window, bisecting recursively whenever the ~1000 cap truncates it.
///
/// Uses an explicit stack rather than recursion so the depth is bounded and
/// loggable. A window that caps but can no longer be halved is reported at
/// `warn!` — silent truncation in an archival tool is data loss that looks like
/// success.
pub async fn sweep_window_deep(
    ctx: &Arc<DetectContext>,
    store: &Arc<Store>,
    t: &SweepTarget,
    root: Window,
    now: i64,
) -> anyhow::Result<SweepResult> {
    let mut stack = vec![root];
    let mut total = SweepResult::default();
    while let Some(w) = stack.pop() {
        let r = sweep_window(ctx, store, t, w, now).await?;
        total.seen += r.seen;
        total.slugs.extend(r.slugs);
        if r.capped {
            match bisect(w) {
                Some((a, b)) => {
                    stack.push(b);
                    stack.push(a);
                }
                None => {
                    total.capped = true;
                    warn!(
                        "clips: {} window {}..{} still caps at {} s — the least-viewed clips \
                         in it cannot be reached and are NOT archived",
                        t.login,
                        fmt_rfc3339(w.start),
                        fmt_rfc3339(w.end),
                        w.secs()
                    );
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(REQUEST_PACE_MS)).await;
    }
    Ok(total)
}

/// Ask Helix about specific clips by id (up to 100 per call).
///
/// Returns the ids that still exist. **Absence is the deletion signal** — which
/// is why this returns `Err` rather than an empty set on failure: marking clips
/// gone because a request failed would be a lie, and would trigger a pointless
/// recovery attempt on a clip that is fine.
pub async fn hydrate_clip_ids(
    ctx: &Arc<DetectContext>,
    ids: &[String],
) -> anyhow::Result<Vec<serde_json::Value>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (client_id, token) = ctx.twitch_helix_auth().await?;
    let client = ctx.http_client();
    let mut out = Vec::new();
    for chunk in ids.chunks(PAGE_SIZE) {
        let query: Vec<(&str, &str)> = chunk.iter().map(|i| ("id", i.as_str())).collect();
        let resp = client
            .get("https://api.twitch.tv/helix/clips")
            .header("Client-Id", &client_id)
            .bearer_auth(&token)
            .query(&query)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("helix clips by id {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        out.extend(v["data"].as_array().cloned().unwrap_or_default());
        tokio::time::sleep(std::time::Duration::from_millis(REQUEST_PACE_MS)).await;
    }
    Ok(out)
}

/// Parse the `"2,24"` post-broadcast offsets setting into seconds.
///
/// Garbage degrades to the default rather than disabling the sweep — losing the
/// only window in which the recovery keys exist would be a silent, permanent
/// loss, and a typo in a settings box should not cost that.
pub fn parse_post_offsets(s: &str) -> (i64, i64) {
    let parts: Vec<i64> = s
        .split(',')
        .filter_map(|p| p.trim().parse::<i64>().ok())
        .filter(|h| *h >= 0)
        .collect();
    match parts.as_slice() {
        [a, b, ..] if b > a => (a * 3600, b * 3600),
        [a] => (a * 3600, DEFAULT_POST_OFFSETS.1.max(a * 3600 + 3600)),
        _ => DEFAULT_POST_OFFSETS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bisect_halves_a_window_and_refuses_below_the_floor() {
        let day = Window::new(0, 86_400);
        let (a, b) = bisect(day).unwrap();
        assert_eq!(a, Window::new(0, 43_200));
        assert_eq!(b, Window::new(43_200, 86_400));
        // The halves tile the parent exactly — no clip can fall between them.
        assert_eq!(a.start, day.start);
        assert_eq!(b.end, day.end);
        assert_eq!(a.end, b.start);

        assert!(bisect(Window::new(0, MIN_WINDOW_SECS)).is_none());
        assert!(bisect(Window::new(0, 0)).is_none());
    }

    #[test]
    fn cap_is_only_hit_when_pages_are_exhausted_with_a_cursor_left() {
        assert!(cap_hit(MAX_PAGES, true));
        // Ran out of clips before running out of pages: complete, not truncated.
        assert!(!cap_hit(MAX_PAGES, false));
        assert!(!cap_hit(3, true));
    }

    #[test]
    fn backfill_windows_tile_newest_first_without_gaps_or_overlap() {
        let ws = backfill_windows(0, 250, 100);
        assert_eq!(
            ws,
            vec![Window::new(150, 250), Window::new(50, 150), Window::new(0, 50)]
        );
        // Newest first, so an interrupted backfill has already covered the span
        // where clips still carry recovery keys.
        assert!(ws[0].start > ws[1].start);
        // Contiguous, and clamped to the requested range.
        for pair in ws.windows(2) {
            assert_eq!(pair[0].start, pair[1].end);
        }
        assert_eq!(ws.last().unwrap().start, 0);
        assert_eq!(ws.first().unwrap().end, 250);

        assert!(backfill_windows(100, 100, 10).is_empty());
        assert!(backfill_windows(0, 100, 0).is_empty());
    }

    #[test]
    fn rfc3339_round_trips_through_the_helix_format() {
        assert_eq!(fmt_rfc3339(0), "1970-01-01T00:00:00Z");
        let t = 1_786_000_000;
        assert_eq!(parse_rfc3339(&fmt_rfc3339(t)), Some(t));
    }

    #[test]
    fn parses_all_three_clip_url_forms() {
        let a = parse_clip_url("https://clips.twitch.tv/GorgeousTastyCheddar-TupWG").unwrap();
        assert_eq!(a.platform, Platform::Twitch);
        assert_eq!(a.slug, "GorgeousTastyCheddar-TupWG");
        assert_eq!(a.login, None);

        let b = parse_clip_url("https://www.twitch.tv/laynalazar/clip/RefinedYawning-HtHK").unwrap();
        assert_eq!(b.slug, "RefinedYawning-HtHK");
        assert_eq!(b.login.as_deref(), Some("laynalazar"));

        let c = parse_clip_url("https://www.youtube.com/clip/UgyEdErFafvCHgVC8o94AaABCQ").unwrap();
        assert_eq!(c.platform, Platform::YouTube);
        assert_eq!(c.slug, "UgyEdErFafvCHgVC8o94AaABCQ");
    }

    #[test]
    fn clip_urls_survive_the_noise_chat_actually_carries() {
        // Query tails, fragments, trailing slashes and mobile hosts.
        for u in [
            "https://clips.twitch.tv/AbcDef-123?featured=true",
            "https://clips.twitch.tv/AbcDef-123#t=10",
            "https://clips.twitch.tv/AbcDef-123/",
            "clips.twitch.tv/AbcDef-123",
        ] {
            assert_eq!(parse_clip_url(u).unwrap().slug, "AbcDef-123", "{u}");
        }
        // Hyphens are load-bearing: every modern slug has one.
        assert_eq!(
            parse_clip_url("https://clips.twitch.tv/Powerful-Ao_vogXyWLDzcSLR")
                .unwrap()
                .slug,
            "Powerful-Ao_vogXyWLDzcSLR"
        );
    }

    #[test]
    fn non_clip_urls_are_rejected() {
        // A bare channel URL is the one that would do real damage if matched.
        assert!(parse_clip_url("https://twitch.tv/laynalazar").is_none());
        assert!(parse_clip_url("https://twitch.tv/laynalazar/videos").is_none());
        assert!(parse_clip_url("https://www.youtube.com/watch?v=abc").is_none());
        assert!(parse_clip_url("https://youtube.com/clip/").is_none());
        assert!(parse_clip_url("just some chat text").is_none());
    }

    #[test]
    fn helix_clip_maps_a_live_response_including_the_recovery_keys() {
        // Shape copied from a real /helix/clips response.
        let v = serde_json::json!({
            "id": "GorgeousTastyCheddarKappaClaus-TupWG-zLvKzDKp2N",
            "url": "https://www.twitch.tv/laynalazar/clip/GorgeousTastyCheddarKappaClaus-TupWG-zLvKzDKp2N",
            "broadcaster_id": "123", "broadcaster_name": "LaynaLazar",
            "creator_id": "9", "creator_name": "SomeClipper",
            "video_id": "2840712897", "game_id": "509658", "language": "en",
            "title": "You'll Never Be Able To Unsee It",
            "view_count": 4211, "created_at": "2026-08-06T20:04:52Z",
            "thumbnail_url": "https://example/thumb.jpg",
            "duration": 15.6, "vod_offset": 10565, "is_featured": false
        });
        let c = parse_helix_clip(&v, Some("laynalazar")).unwrap();
        assert_eq!(c.slug, "GorgeousTastyCheddarKappaClaus-TupWG-zLvKzDKp2N");
        assert_eq!(c.broadcaster_login, "laynalazar");
        assert_eq!(c.vod_id, "2840712897");
        assert_eq!(c.vod_offset_secs, Some(10565));
        assert!(c.has_recovery_keys());
        // A float duration must not be truncated to whole seconds.
        assert_eq!(c.duration_ms, 15_600);
        assert_eq!(c.created_at, parse_rfc3339("2026-08-06T20:04:52Z").unwrap());
        assert_eq!(c.view_count, 4211);
    }

    #[test]
    fn helix_clip_tolerates_an_expired_vod_in_all_three_shapes() {
        // Once the parent VOD expires Twitch stops reporting the keys. The
        // field may be absent, JSON null, or an empty string; all three mean
        // "unrecoverable from the VOD", and none may panic or fabricate a zero.
        let base = serde_json::json!({
            "id": "Old-Clip", "title": "t", "duration": 30.0,
            "created_at": "2024-01-01T00:00:00Z", "broadcaster_name": "Layna"
        });
        let c = parse_helix_clip(&base, None).unwrap();
        assert_eq!(c.vod_id, "");
        assert_eq!(c.vod_offset_secs, None);
        assert!(!c.has_recovery_keys(), "must not offer VOD recovery");
        // Display-name fallback when the sweep didn't supply a login.
        assert_eq!(c.broadcaster_login, "layna");

        let mut nulled = base.clone();
        nulled["video_id"] = serde_json::Value::Null;
        nulled["vod_offset"] = serde_json::Value::Null;
        let c = parse_helix_clip(&nulled, None).unwrap();
        assert!(!c.has_recovery_keys());

        let mut empty = base.clone();
        empty["video_id"] = serde_json::json!("");
        let c = parse_helix_clip(&empty, None).unwrap();
        assert!(!c.has_recovery_keys());
    }

    #[test]
    fn helix_clip_without_an_id_is_dropped_not_defaulted() {
        assert!(parse_helix_clip(&serde_json::json!({"title": "x"}), None).is_none());
        assert!(parse_helix_clip(&serde_json::json!({"id": ""}), None).is_none());
    }

    #[test]
    fn a_supplied_login_beats_the_display_name() {
        // Display names are not always the login lowercased, and it is the
        // login that is a CDN folder component.
        let v = serde_json::json!({"id": "x", "broadcaster_name": "がうる・ぐら"});
        let c = parse_helix_clip(&v, Some("gawrgura")).unwrap();
        assert_eq!(c.broadcaster_login, "gawrgura");
    }

    #[test]
    fn post_offsets_parse_and_fall_back_to_the_default_on_garbage() {
        assert_eq!(parse_post_offsets("2,24"), (2 * 3600, 24 * 3600));
        assert_eq!(parse_post_offsets(" 1 , 12 "), (3600, 12 * 3600));
        // Losing this sweep loses the only window where the keys exist, so a
        // typo must degrade to the default rather than disable it.
        assert_eq!(parse_post_offsets(""), DEFAULT_POST_OFFSETS);
        assert_eq!(parse_post_offsets("nonsense"), DEFAULT_POST_OFFSETS);
        assert_eq!(parse_post_offsets("-5,-1"), DEFAULT_POST_OFFSETS);
        // Out of order is not a valid pair of stages.
        assert_eq!(parse_post_offsets("24,2"), DEFAULT_POST_OFFSETS);
        // A single value still yields a distinct, later second stage.
        let (a, b) = parse_post_offsets("48");
        assert_eq!(a, 48 * 3600);
        assert!(b > a);
    }
}
