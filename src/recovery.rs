//! Twitch VOD recovery — reconstruct muted / deleted VODs from CDN segments.
//!
//! Twitch DMCA-mutes and (on deletion) unpublishes VODs, but the underlying `.ts`
//! segments stay on Twitch's CDN for ~60 days. A VOD's HLS playlist URL is
//! **deterministically derivable** from three facts about the broadcast:
//! the streamer *login*, the numeric *broadcast id* (the Helix Get-Streams `id`,
//! == `Recording.stream_id`, **not** the `/videos/<vod_id>` archive id), and the
//! *stream start time* (UTC, second precision):
//!
//! ```text
//! for sec in 0..=window:                       // start time is imprecise
//!     base = "{login}_{broadcast_id}_{start_epoch + sec}"
//!     hash = sha1_hex(base)[..20]
//!     for host in CDN_HOSTS:
//!         candidate = "{host}{hash}_{base}/chunked/index-dvr.m3u8"
//! // HEAD-probe; the (single) 200 is the live playlist.
//! ```
//!
//! Only the *true* start second produces a real folder, so the first 200 wins.
//! The found `chunked` (source) playlist can be re-pointed to `1080p60`/… by
//! swapping the quality path component. Each segment line is rewritten to an
//! absolute CDN URL; DMCA-muted segments are **un-muted** by preferring the
//! pre-mute original `{n}.ts` (usually still on the CDN) over the silenced
//! `{n}-muted.ts`; segments gone entirely are dropped (partial recovery of a
//! deleted VOD). ffmpeg then muxes the surviving timeline into an MKV.
//!
//! The core (derive → probe → rewrite → mux) needs no Twitch auth. The
//! third-party-site [`scrape`] submodule is best-effort and *only* prefills a
//! start timestamp — its failure can never abort a recovery.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::events::{AppEvent, EventTx};
use crate::models::now_unix;
use crate::store::Store;

/// Twitch VOD CDN hosts (all https, trailing slash). The distributions rotate, so
/// this is only a *seed*: [`load_hosts`] unions it with hosts learned from GQL
/// (`seekPreviewsURL`) at runtime plus the user's override. The first block is the
/// set observed actively serving VODs (2026, sampled across ~110 global channels);
/// the rest are historical fallbacks (cheap DNS-fail if gone) that may still hold
/// older/deleted VODs.
pub const CDN_HOSTS: &[&str] = &[
    // Currently active (observed serving VODs).
    "https://d2nvs31859zcd8.cloudfront.net/",
    "https://d1m7jfoe9zdc1j.cloudfront.net/",
    "https://d3vd9lfkzbru3h.cloudfront.net/",
    "https://d2vi6trrdongqn.cloudfront.net/",
    "https://d3stzm2eumvgb4.cloudfront.net/",
    "https://d3fi1amfgojobc.cloudfront.net/",
    "https://dgeft87wbj63p.cloudfront.net/",
    // Historical / regional fallbacks.
    "https://vod-secure.twitch.tv/",
    "https://vod-metro.twitch.tv/",
    "https://vod-pop-secure.twitch.tv/",
    "https://d2e2de1etea730.cloudfront.net/",
    "https://dqrpb9wgowsf5.cloudfront.net/",
    "https://ds0h3roq6wcgc.cloudfront.net/",
    "https://d2aba1wr3818hz.cloudfront.net/",
    "https://d3c27h4odz752x.cloudfront.net/",
    "https://d3aqoihi2n8ty8.cloudfront.net/",
    "https://d2vjef5jvl6bfs.cloudfront.net/",
    "https://d1ymi26ma8va5x.cloudfront.net/",
    "https://d1mhjrowxxagfy.cloudfront.net/",
    "https://ddacn6pr5v0tl.cloudfront.net/",
];

/// Quality path components to probe, source-first.
const QUALITIES: &[&str] = &[
    "chunked", "1080p60", "1080p30", "936p60", "720p60", "720p30", "480p30", "360p30",
    "160p30", "audio_only",
];

/// Start-second search half-width (searched symmetrically, `-w..=w`): the true
/// folder second can sit on either side of the given time (Helix `started_at` is
/// exact → sec 0, but a scraped/VOD-created time can be a few seconds late while a
/// go-live time is a few seconds early). A detection-clock time (approximate) can
/// be off by minutes, so widen.
const WINDOW_EXACT: i64 = 60;
const WINDOW_APPROX: i64 = 180;

/// Default concurrent-HEAD cap (playlist probe + segment validation).
pub const DEFAULT_MAX_CONC: usize = 8;

// Settings keys (shared by the UI form and the auto-recover hook).
/// Auto-recover a Twitch VOD when the VOD checker finds it DMCA-muted.
pub const K_AUTO_RECOVER_MUTED: &str = "auto_recover_muted";
/// Auto-recover a Twitch VOD when the VOD checker finds it was never published.
pub const K_AUTO_RECOVER_DELETED: &str = "auto_recover_deleted";
/// Newline/comma-separated EXTRA CDN hosts (added to the built-in + learned sets).
pub const K_RECOVERY_CDN_HOSTS: &str = "recovery_cdn_hosts";
/// Persisted set of CDN hosts learned at runtime from GQL `seekPreviewsURL` — this
/// is how the host list stays current as Twitch rotates distributions.
pub const K_RECOVERY_LEARNED_HOSTS: &str = "recovery_learned_hosts";
/// Default recovery quality (`""`/`chunked` = source, else e.g. `720p60`).
pub const K_RECOVERY_QUALITY: &str = "recovery_default_quality";
/// Concurrent-HEAD cap override for the segment/playlist probes.
pub const K_RECOVERY_MAX_CONC: &str = "recovery_max_concurrency";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------- inputs / results ----------

/// Everything needed to derive a VOD's CDN URL. Bundled so `run_recovery` /
/// `ManualCommand` stay under the argument-count lint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryInputs {
    pub login: String,
    /// Helix Get-Streams `id` (== `Recording.stream_id`), NOT the `/videos/` id.
    pub broadcast_id: String,
    /// UTC start time, epoch seconds.
    pub start_epoch: i64,
    /// True when `start_epoch` came from the detection clock (widen the search).
    pub went_live_approx: bool,
    /// The `/videos/<id>` archive id, when known (a published/muted VOD). Enables
    /// the GQL fast-path (exact host+folder, no CDN host-list guessing). `None` for
    /// a deleted VOD — those fall back to the hash-derived host probe.
    pub vod_id: Option<String>,
}

/// Where a completed recovery is filed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverySink {
    /// Attach the MKV back onto a tracked recording + set its unique recovery status.
    Recording(i64),
    /// Add to the Videos list (reuse the download-history UI) for an untracked VOD.
    Standalone { output_dir: String, filename: String },
}

/// A live playlist located by [`find_live_playlist`].
#[derive(Clone, Debug)]
pub struct FoundPlaylist {
    /// Absolute `…/chunked/index-dvr.m3u8` that returned 200.
    pub url: String,
    pub host: String,
    /// `start_epoch + sec` that hashed to the live URL — the true go-live second.
    pub matched_epoch: i64,
}

/// A rewritten, muxable playlist plus recovery stats.
#[derive(Clone, Debug, Default)]
pub struct RecoveredPlaylist {
    pub text: String,
    /// Media segments in the source playlist.
    pub total: usize,
    /// Segments confirmed present on the CDN (or passed through for a muted VOD).
    pub present: usize,
    /// Muted segments whose pre-mute original `{n}.ts` survived (true un-mute).
    pub unmuted_recovered: usize,
    /// Segments that had to fall back to the constructed `-muted` copy — the
    /// pre-mute original didn't survive, so these ARE silent in the output.
    /// Unlike `unmuted_recovered`, a nonzero count here means real, audible
    /// mute damage in `text` (used by head-backfill's integrity gate before
    /// superseding an older take's head file).
    pub muted_used: usize,
    /// Segments dropped (neither original nor muted copy on the CDN).
    pub missing: usize,
}

// ---------- primitives ----------

/// First 20 hex chars of `SHA1(base)` — the CDN folder prefix.
pub fn sha1_hex20(base: &str) -> String {
    let digest = Sha1::digest(base.as_bytes());
    let mut s = String::with_capacity(40);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s.truncate(20);
    s
}

/// The base folder name for a given start second: `{login}_{broadcast_id}_{epoch}`.
fn base_name(inp: &RecoveryInputs, sec: i64) -> String {
    format!("{}_{}_{}", inp.login, inp.broadcast_id, inp.start_epoch + sec)
}

/// Every candidate `index-dvr.m3u8` URL for one start-second offset across `hosts`.
pub fn candidate_urls(inp: &RecoveryInputs, sec: i64, hosts: &[String]) -> Vec<String> {
    let base = base_name(inp, sec);
    let hash = sha1_hex20(&base);
    hosts
        .iter()
        .map(|h| format!("{h}{hash}_{base}/chunked/index-dvr.m3u8"))
        .collect()
}

/// A HEAD that resolves to `true` only on a 2xx.
async fn head_ok(client: &reqwest::Client, url: &str) -> bool {
    matches!(client.head(url).send().await, Ok(r) if r.status().is_success())
}

/// HEAD-probe every `(sec, host)` candidate; return the first 200 (the true start
/// second — only one exists, so the first hit is authoritative). `None` means the
/// VOD is past the ~60-day CDN window (or the inputs are wrong).
pub async fn find_live_playlist(
    client: &reqwest::Client,
    inp: &RecoveryInputs,
    hosts: &[String],
    max_conc: usize,
) -> Option<FoundPlaylist> {
    if inp.login.is_empty() || inp.broadcast_id.is_empty() {
        return None;
    }
    let window = if inp.went_live_approx { WINDOW_APPROX } else { WINDOW_EXACT };
    let sem = Arc::new(Semaphore::new(max_conc.max(1)));
    let mut set: JoinSet<Option<(String, i64)>> = JoinSet::new();
    for sec in -window..=window {
        for url in candidate_urls(inp, sec, hosts) {
            let client = client.clone();
            let sem = sem.clone();
            set.spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore");
                if head_ok(&client, &url).await { Some((url, sec)) } else { None }
            });
        }
    }
    while let Some(res) = set.join_next().await {
        if let Ok(Some((url, sec))) = res {
            set.abort_all();
            let host = hosts.iter().find(|h| url.starts_with(*h)).cloned().unwrap_or_default();
            return Some(FoundPlaylist { url, host, matched_epoch: inp.start_epoch + sec });
        }
    }
    None
}

/// Twitch's public web client-id (read-only GQL). Used to look up a published
/// VOD's exact CDN folder via `seekPreviewsURL` and to read public channel
/// panels for the About-page archive — no auth or user data involved.
pub(crate) const GQL_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

/// A published VOD's exact CDN location, resolved from Twitch GQL — no host-list
/// guessing. Also yields the login/broadcast/start the derivation would need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VodInfo {
    pub login: String,
    pub broadcast_id: String,
    pub start_epoch: i64,
    pub host: String,
    pub folder: String,
}

/// Split a `seekPreviewsURL` into `(host, folder)`, where folder is the
/// `{hash}_{login}_{broadcast}_{epoch}` path segment.
fn parse_seek_previews(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    let folder = path.split("/storyboards/").next()?.trim_matches('/');
    if host.is_empty() || folder.is_empty() {
        return None;
    }
    Some((host.to_string(), folder.to_string()))
}

/// Extract `(login, broadcast_id, epoch)` from a CDN folder name. The login can
/// itself contain underscores (e.g. `yuy_ix`), so peel the fixed head (hash) and
/// tail (broadcast, epoch) and re-join the middle.
fn folder_parts(folder: &str) -> Option<(String, String, i64)> {
    let parts: Vec<&str> = folder.split('_').collect();
    if parts.len() < 4 {
        return None;
    }
    let epoch = parts[parts.len() - 1].parse::<i64>().ok()?;
    let broadcast = parts[parts.len() - 2];
    if broadcast.is_empty() || !broadcast.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let login = parts[1..parts.len() - 2].join("_");
    if login.is_empty() {
        return None;
    }
    Some((login, broadcast.to_string(), epoch))
}

/// Look up a published VOD's exact CDN folder via Twitch GQL (`seekPreviewsURL`).
/// `vod_id` must be the numeric `/videos/<id>`. Errors for deleted/private VODs.
pub async fn gql_vod_info(client: &reqwest::Client, vod_id: &str) -> anyhow::Result<VodInfo> {
    if vod_id.is_empty() || !vod_id.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("vod id must be numeric");
    }
    let body = serde_json::json!({
        "query": format!("query{{video(id:\"{vod_id}\"){{seekPreviewsURL}}}}")
    });
    let resp = client
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", GQL_CLIENT_ID)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    let seek = v["data"]["video"]["seekPreviewsURL"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no seekPreviewsURL (VOD deleted, private, or sub-only?)"))?;
    let (host, folder) =
        parse_seek_previews(seek).ok_or_else(|| anyhow::anyhow!("unparseable seekPreviewsURL"))?;
    let (login, broadcast_id, start_epoch) =
        folder_parts(&folder).ok_or_else(|| anyhow::anyhow!("unexpected CDN folder shape"))?;
    Ok(VodInfo { login, broadcast_id, start_epoch, host, folder })
}

/// GQL fast-path: resolve a published VOD's live playlist by its `/videos/` id,
/// bypassing the CDN host-list entirely. `None` when GQL has nothing (deleted VOD)
/// or the derived playlist doesn't actually respond.
async fn resolve_via_gql(client: &reqwest::Client, vod_id: &str) -> Option<FoundPlaylist> {
    let info = gql_vod_info(client, vod_id).await.ok()?;
    let url = format!("https://{}/{}/chunked/index-dvr.m3u8", info.host, info.folder);
    head_ok(client, &url).await.then(|| FoundPlaylist {
        url,
        host: format!("https://{}/", info.host),
        matched_epoch: info.start_epoch,
    })
}

/// Locate a VOD's live playlist: try the GQL fast-path first when a `/videos/` id
/// is known (exact, host-list-independent — the robust path for muted-but-online
/// VODs), then fall back to hash-probing the CDN host list (for deleted VODs).
pub async fn resolve_playlist(
    client: &reqwest::Client,
    inputs: &RecoveryInputs,
    hosts: &[String],
    max_conc: usize,
) -> Option<FoundPlaylist> {
    let gql = match inputs.vod_id.as_deref().filter(|s| !s.is_empty()) {
        Some(vid) => resolve_via_gql(client, vid).await,
        None => None,
    };
    match gql {
        Some(found) => Some(found),
        None => find_live_playlist(client, inputs, hosts, max_conc).await,
    }
}

/// Swap the `chunked` quality PATH COMPONENT for `quality` (identity when
/// equal). Matches `/chunked/` with its slashes so a login that happens to
/// contain the word "chunked" (part of the preceding `{hash}_{login}_…` folder
/// segment) can't be corrupted — logins can't contain `/`.
fn swap_quality(chunked_url: &str, quality: &str) -> String {
    chunked_url.replacen("/chunked/", &format!("/{quality}/"), 1)
}

/// Re-point a found (source/`chunked`) playlist URL at `requested` quality,
/// verified against the renditions that actually exist on the CDN. An empty
/// request returns the found URL unchanged; an unavailable one falls back to
/// the best available (see [`resolve_quality`]). Used by the head backfill's
/// match-the-live-capture re-fetch — a head at source resolution can't be
/// `-c copy`-concatenated with a live capture that joined before the source
/// rendition appeared in Twitch's playlist.
pub async fn playlist_at_quality(
    client: &reqwest::Client,
    found: &FoundPlaylist,
    requested: &str,
    max_conc: usize,
) -> String {
    if requested.trim().is_empty() {
        return found.url.clone();
    }
    let quals = enumerate_qualities(client, found, max_conc).await;
    let chosen = resolve_quality(requested, &quals);
    swap_quality(&found.url, &chosen)
}

/// HEAD-probe which quality variants of a found playlist exist, source-first.
pub async fn enumerate_qualities(
    client: &reqwest::Client,
    found: &FoundPlaylist,
    max_conc: usize,
) -> Vec<String> {
    let sem = Arc::new(Semaphore::new(max_conc.max(1)));
    let mut set: JoinSet<Option<(usize, String)>> = JoinSet::new();
    for (rank, q) in QUALITIES.iter().enumerate() {
        let url = swap_quality(&found.url, q);
        let (client, sem, q) = (client.clone(), sem.clone(), q.to_string());
        set.spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore");
            if head_ok(&client, &url).await { Some((rank, q)) } else { None }
        });
    }
    let mut hits: Vec<(usize, String)> = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Some(hit)) = res {
            hits.push(hit);
        }
    }
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().map(|(_, q)| q).collect()
}

/// The absolute prefix a segment name hangs off: the playlist URL with its
/// `index-dvr.m3u8` filename stripped (keeps the trailing slash).
fn playlist_prefix(playlist_url: &str) -> String {
    match playlist_url.rfind('/') {
        Some(i) => playlist_url[..=i].to_string(),
        None => playlist_url.to_string(),
    }
}

/// Split a segment filename into its canonical (un-muted) form and whether the
/// source listed it as muted. `"123-muted.ts"` / `"123-unmuted.ts"` → `("123.ts",
/// true)`; `"123.ts"` → `("123.ts", false)`.
fn canonical_segment(name: &str) -> (String, bool) {
    let canonical = name.replace("-unmuted", "").replace("-muted", "");
    let marked = canonical != name;
    (canonical, marked)
}

/// True for a playlist line naming a media segment. Twitch VODs use either legacy
/// MPEG-TS (`123.ts`) or fMP4 (`123.mp4`) segments — handle both.
fn is_segment_line(t: &str) -> bool {
    !t.is_empty() && !t.starts_with('#') && (t.ends_with(".ts") || t.ends_with(".mp4"))
}

/// The silenced-copy URL for a canonical segment: `1404.mp4` → `…1404-muted.mp4`.
/// The playlist may point at a dead `-unmuted` name while only the `-muted` copy
/// survives on the CDN, so the muted variant must be *constructed*, not read from
/// the playlist line.
fn muted_variant(base: &str, canonical: &str) -> String {
    match canonical.rsplit_once('.') {
        Some((stem, ext)) => format!("{base}{stem}-muted.{ext}"),
        None => format!("{base}{canonical}-muted"),
    }
}

/// A media-segment line, keyed by its position among segments.
struct MediaSeg {
    pos: usize,
    canonical: String,
    marked: bool,
}

/// Fetch a quality's playlist and rewrite it into an absolute-URL, un-muted,
/// gaps-dropped playlist ready for ffmpeg.
///
/// `probe_all = false` (a merely *muted* VOD that still exists): only the
/// muted-marked segments are HEAD-probed; plain segments pass through verbatim —
/// the key win for long VODs. `probe_all = true` (a *deleted* VOD): every segment
/// is probed and dead ones dropped.
///
/// `max_secs` truncates the source playlist to its first N seconds before any
/// segment work (the head-backfill case: only the missed beginning is wanted,
/// and the source is a still-growing live playlist).
///
/// `skip_secs`, when set, additionally discards that many seconds from the
/// start before `max_secs` starts counting (the trigger lead-time case: a
/// bounded window ending at an arbitrary mid-playlist moment, not always
/// "from position zero" — see `truncate_playlist_window`). Ignored (as if
/// `None`) unless `max_secs` is also set.
pub async fn build_playlist(
    client: &reqwest::Client,
    playlist_url: &str,
    max_conc: usize,
    probe_all: bool,
    max_secs: Option<f64>,
    skip_secs: Option<f64>,
) -> anyhow::Result<RecoveredPlaylist> {
    let mut src = client.get(playlist_url).send().await?.error_for_status()?.text().await?;
    if let Some(cap) = max_secs {
        src = match skip_secs {
            Some(skip) => truncate_playlist_window(&src, skip, cap),
            None => truncate_playlist(&src, cap),
        };
    }
    let base = playlist_prefix(playlist_url);

    // Enumerate media-segment lines (position-keyed).
    let mut segs: Vec<MediaSeg> = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        if !is_segment_line(t) {
            continue;
        }
        let (canonical, marked) = canonical_segment(t);
        segs.push(MediaSeg { pos: segs.len(), canonical, marked });
    }

    // Pass 1 — resolve each segment to an absolute URL (or None) concurrently.
    // Tuple: (pos, url, was_unmute, was_muted_fallback).
    let sem = Arc::new(Semaphore::new(max_conc.max(1)));
    let mut set: JoinSet<(usize, Option<String>, bool, bool)> = JoinSet::new();
    for seg in &segs {
        let needs_probe = seg.marked || probe_all;
        let orig = format!("{base}{}", seg.canonical);
        if !needs_probe {
            // Plain segment of an existing VOD — trust it, no HEAD.
            let pos = seg.pos;
            set.spawn(async move { (pos, Some(orig), false, false) });
            continue;
        }
        // Prefer the pre-mute original (`{n}.ts`/`{n}.mp4`) — true un-mute; fall
        // back to the constructed `-muted` copy (silence beats a hole). The
        // playlist's own `-unmuted` name is often a dead pointer, so we never use it.
        let muted = muted_variant(&base, &seg.canonical);
        let (client, sem, pos, marked) = (client.clone(), sem.clone(), seg.pos, seg.marked);
        set.spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore");
            if head_ok(&client, &orig).await {
                (pos, Some(orig), marked, false) // original survived → un-muted audio
            } else if head_ok(&client, &muted).await {
                (pos, Some(muted), false, true) // silenced copy — continuity over a hole
            } else {
                (pos, None, false, false)
            }
        });
    }
    let mut resolved: HashMap<usize, String> = HashMap::new();
    let mut unmuted_recovered = 0usize;
    let mut muted_used = 0usize;
    while let Some(res) = set.join_next().await {
        if let Ok((pos, Some(u), was_unmute, was_muted_fallback)) = res {
            if was_unmute {
                unmuted_recovered += 1;
            }
            if was_muted_fallback {
                muted_used += 1;
            }
            resolved.insert(pos, u);
        }
    }

    // Pass 2 — reassemble in order, buffering the pending #EXTINF so a dropped
    // segment drops its duration too (no dangling tag).
    let mut out = String::new();
    let (mut total, mut present, mut missing) = (0usize, 0usize, 0usize);
    let mut seg_pos = 0usize;
    let mut pending_extinf: Option<String> = None;
    for line in src.lines() {
        let t = line.trim();
        if t.starts_with("#EXTINF") {
            pending_extinf = Some(line.to_string());
            continue;
        }
        if !is_segment_line(t) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // A media segment.
        total += 1;
        let pos = seg_pos;
        seg_pos += 1;
        match resolved.get(&pos) {
            Some(abs) => {
                if let Some(ex) = pending_extinf.take() {
                    out.push_str(&ex);
                    out.push('\n');
                }
                out.push_str(abs);
                out.push('\n');
                present += 1;
            }
            None => {
                pending_extinf = None;
                missing += 1;
            }
        }
    }

    Ok(RecoveredPlaylist { text: out, total, present, unmuted_recovered, muted_used, missing })
}

/// Truncate a playlist to its first `max_secs` seconds of media (by summed
/// `#EXTINF` durations, so the cut lands on a segment boundary — up to one
/// segment over), and terminate it with `#EXT-X-ENDLIST` if the source lacks
/// one. A still-growing live `index-dvr.m3u8` has no ENDLIST, and without it
/// ffmpeg treats the input as a live stream and waits at the end instead of
/// finishing.
fn truncate_playlist(src: &str, max_secs: f64) -> String {
    let mut out = String::new();
    let mut acc = 0.0f64;
    let mut pending_extinf: Option<&str> = None;
    for line in src.lines() {
        let t = line.trim();
        if t == "#EXT-X-ENDLIST" {
            break; // re-appended below
        }
        if t.starts_with("#EXTINF") {
            pending_extinf = Some(line);
            continue;
        }
        if is_segment_line(t) {
            if acc >= max_secs {
                break;
            }
            if let Some(ex) = pending_extinf.take() {
                acc += ex
                    .trim()
                    .strip_prefix("#EXTINF:")
                    .and_then(|rest| rest.split(',').next())
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(0.0);
                out.push_str(ex);
                out.push('\n');
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// Like [`truncate_playlist`], but first discards `skip_secs` of media from
/// the start of the playlist before keeping the next `window_secs` — a
/// bounded slice around an arbitrary mid-playlist moment (e.g. a fixed lead
/// time before a trigger match fired mid-broadcast) instead of always "from
/// position zero." Same segment-boundary rounding (a cut lands on a segment
/// boundary, up to one segment over on either edge) and same
/// `#EXT-X-ENDLIST` termination as `truncate_playlist`.
fn truncate_playlist_window(src: &str, skip_secs: f64, window_secs: f64) -> String {
    let mut out = String::new();
    let mut acc = 0.0f64; // total media time walked so far, skip phase included
    let mut kept = 0.0f64; // media time actually kept, once past the skip phase
    let mut pending_extinf: Option<&str> = None;
    for line in src.lines() {
        let t = line.trim();
        if t == "#EXT-X-ENDLIST" {
            break; // re-appended below
        }
        if t.starts_with("#EXTINF") {
            pending_extinf = Some(line);
            continue;
        }
        if is_segment_line(t) {
            let ex = pending_extinf.take();
            let dur = ex
                .and_then(|e| e.trim().strip_prefix("#EXTINF:"))
                .and_then(|rest| rest.split(',').next())
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(0.0);
            if acc < skip_secs {
                // Still skipping: discard this segment (and its #EXTINF)
                // entirely, but still count its duration toward the skip.
                acc += dur;
                continue;
            }
            if kept >= window_secs {
                break;
            }
            acc += dur;
            kept += dur;
            if let Some(e) = ex {
                out.push_str(e);
                out.push('\n');
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// Sum the `#EXTINF:<secs>,` durations in a playlist file (for progress %).
async fn playlist_duration_secs(path: &Path) -> Option<f64> {
    let text = crate::iomon::fs::read_to_string(crate::iomon::Cat::Recovery, path).await.ok()?;
    let mut total = 0.0f64;
    for line in text.lines() {
        if let Some(secs) = line
            .trim()
            .strip_prefix("#EXTINF:")
            .and_then(|rest| rest.split(',').next())
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            total += secs;
        }
    }
    (total > 0.0).then_some(total)
}

/// ffmpeg-mux a local rewritten playlist (referencing remote `.ts`) into an MKV.
/// The `-protocol_whitelist` must precede `-i` (why `remux_ts_to_mkv` can't be
/// reused). Emits `BackgroundTaskProgress` when a `(tx, task_id)` is given.
/// `max_secs` adds `-t` (output cap — the head-backfill trim to the exact
/// missed duration). `.kill_on_drop(true)` — recovery is ephemeral; quitting
/// reaps it.
pub async fn mux_playlist_to_mkv(
    playlist_path: &Path,
    dst: &Path,
    progress_tx: Option<(EventTx, u64)>,
    max_secs: Option<f64>,
    // What this mux is for ("head backfill" / "VOD recovery") — shown as the
    // purpose in the I/O monitor's per-process table.
    purpose: &str,
) -> anyhow::Result<()> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let total_us: Option<i64> = if progress_tx.is_some() {
        playlist_duration_secs(playlist_path).await.map(|s| (s * 1_000_000.0) as i64)
    } else {
        None
    };

    // At most 2 CDN-fed muxes write to the recordings drive at once — several
    // recoveries/head backfills triggering together (e.g. DMCA mutes landing
    // minutes after a shared stream end) must not stack on top of the live
    // captures and finalize passes already hitting the same disk (see io_gate).
    let _gate = crate::io_gate::cdn_mux("cdn-mux").await;

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-protocol_whitelist")
        .arg("file,http,https,tcp,tls,crypto")
        .arg("-i")
        .arg(playlist_path)
        .arg("-map")
        .arg("0:v?")
        .arg("-map")
        .arg("0:a?")
        .arg("-c")
        .arg("copy")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats");
    if let Some(secs) = max_secs {
        cmd.arg("-t").arg(format!("{secs:.3}"));
    }
    cmd.arg(dst)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn()?;
    let _io_guard = crate::iomon::track_tool(child.id(), "ffmpeg", purpose, dst);
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut lines: Vec<String> = Vec::new();
        while let Ok(Some(line)) = reader.next_line().await {
            lines.push(line);
        }
        lines
    });

    {
        let mut reader = BufReader::new(stdout).lines();
        let mut blk_speed = String::new();
        let mut blk_pos = String::new();
        let mut blk_us: Option<i64> = None;
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                match k {
                    "speed" => blk_speed = v.to_string(),
                    "out_time" => blk_pos = v.to_string(),
                    "out_time_ms" => blk_us = v.parse::<i64>().ok(),
                    "progress" => {
                        if let Some((ref tx, task_id)) = progress_tx {
                            let progress = blk_us.and_then(|us| {
                                total_us
                                    .filter(|&t| t > 0)
                                    .map(|t| (us as f64 / t as f64).clamp(0.0, 1.0) as f32)
                            });
                            let pos_short = blk_pos.split('.').next().unwrap_or(&blk_pos);
                            let _ = tx.send(AppEvent::BackgroundTaskProgress {
                                id: task_id,
                                progress,
                                info: format!("mux speed={blk_speed} pos={pos_short}"),
                            });
                        }
                        blk_speed.clear();
                        blk_pos.clear();
                        blk_us = None;
                    }
                    _ => {}
                }
            }
        }
    }

    let status = child.wait().await?;
    let stderr_lines = stderr_task.await.unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);
        let tail: String = stderr_lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(3)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        if tail.is_empty() {
            anyhow::bail!("ffmpeg recovery mux failed (exit {code})")
        } else {
            anyhow::bail!("ffmpeg recovery mux failed (exit {code}): {tail}")
        }
    }
}

/// Resolve the user's requested quality against what actually exists: an empty
/// request (or an unavailable one) falls back to the best available, else `chunked`.
fn resolve_quality(requested: &str, available: &[String]) -> String {
    let req = requested.trim();
    if !req.is_empty() && available.iter().any(|q| q == req) {
        return req.to_string();
    }
    available.first().cloned().unwrap_or_else(|| "chunked".to_string())
}

/// The concurrent-HEAD cap from the `recovery_max_concurrency` setting, clamped
/// to a sane range; defaults to [`DEFAULT_MAX_CONC`].
pub fn load_max_conc(store: &Store) -> usize {
    store
        .get_setting(K_RECOVERY_MAX_CONC)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, 64))
        .unwrap_or(DEFAULT_MAX_CONC)
}

/// Normalize a newline/comma-separated host blob into trailing-slash URLs.
fn parse_host_list(raw: &str) -> Vec<String> {
    raw.split(['\n', ','])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(normalize_host)
        .collect()
}

/// A host string as an `https://…/` URL (accepts a bare host or a full URL).
fn normalize_host(h: &str) -> String {
    let h = h.trim();
    let with_scheme =
        if h.starts_with("http://") || h.starts_with("https://") { h.to_string() } else { format!("https://{h}") };
    if with_scheme.ends_with('/') { with_scheme } else { format!("{with_scheme}/") }
}

/// The full host set to probe: built-in seed ∪ runtime-learned ∪ user override,
/// deduped, active hosts first. This is what keeps recovery working as Twitch
/// rotates CDN distributions (learned hosts accrue from every GQL resolve).
pub fn load_hosts(store: &Store) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for h in CDN_HOSTS {
        if seen.insert((*h).to_string()) {
            out.push((*h).to_string());
        }
    }
    for key in [K_RECOVERY_LEARNED_HOSTS, K_RECOVERY_CDN_HOSTS] {
        let raw = store.get_setting(key).ok().flatten().unwrap_or_default();
        for h in parse_host_list(&raw) {
            if seen.insert(h.clone()) {
                out.push(h);
            }
        }
    }
    out
}

/// Remember a confirmed-live CDN host (from a successful recovery or a harvest) in
/// the persisted learned set. No-op if already known. Returns true if it was new.
pub fn record_learned_host(store: &Store, host: &str) -> bool {
    let host = normalize_host(host);
    if CDN_HOSTS.contains(&host.as_str()) {
        return false;
    }
    let raw = store.get_setting(K_RECOVERY_LEARNED_HOSTS).ok().flatten().unwrap_or_default();
    let mut hosts = parse_host_list(&raw);
    if hosts.iter().any(|h| h == &host) {
        return false;
    }
    hosts.push(host);
    let _ = store.set_setting(K_RECOVERY_LEARNED_HOSTS, &hosts.join("\n"));
    true
}

/// Harvest the CDN host of each published VOD via GQL and record any new ones.
/// Returns `(newly_learned, checked)`. Used by the "Refresh CDN hosts" action to
/// proactively learn hosts from the user's own recordings.
pub async fn harvest_hosts(store: &Store, client: &reqwest::Client, vod_ids: &[String]) -> (usize, usize) {
    let mut learned = 0usize;
    let mut checked = 0usize;
    for vid in vod_ids {
        if let Ok(info) = gql_vod_info(client, vid).await {
            checked += 1;
            if record_learned_host(store, &format!("https://{}/", info.host)) {
                learned += 1;
            }
        }
    }
    (learned, checked)
}

/// The Background-task label and the recording id (if any) a recovery is
/// tied to — `RecoverVod(Some(id))` is how the Streams grid's VodJob row
/// joins its live progress bar back to the take that started it.
fn recovery_task_label_and_id(sink: &RecoverySink) -> (String, Option<i64>) {
    match sink {
        RecoverySink::Recording(id) => (format!("VOD recovery · rec #{id}"), Some(*id)),
        RecoverySink::Standalone { filename, .. } => {
            (format!("VOD recovery · {filename}"), None)
        }
    }
}

/// End-to-end recovery: derive → probe → rewrite → mux → file the result.
/// Shared by the supervisor command, the bulk scan, and the auto-mute hook.
#[allow(clippy::too_many_arguments)]
pub async fn run_recovery(
    client: reqwest::Client,
    store: Arc<Store>,
    events: EventTx,
    inputs: RecoveryInputs,
    quality: String,
    sink: RecoverySink,
    probe_all: bool,
    task_id: u64,
) {
    let (label, rec_id) = recovery_task_label_and_id(&sink);
    let _ = events.send(AppEvent::BackgroundTaskStarted(crate::events::BackgroundTask {
        id: task_id,
        kind: crate::events::BackgroundTaskKind::RecoverVod(rec_id),
        label,
        detail: format!("{}_{}", inputs.login, inputs.broadcast_id),
        started_at: now_unix(),
        progress: Some(0.0),
        progress_info: None,
    }));
    if let RecoverySink::Recording(id) = &sink {
        let _ = store.set_recording_recovery_state(*id, "recovering");
        let _ = events.send(AppEvent::RecordingUpdated { recording_id: *id });
    }

    let finish_fail = |msg: String| {
        if let RecoverySink::Recording(id) = &sink {
            let state = if msg.contains("window") { "unavailable" } else { "failed" };
            let _ = store.set_recording_recovery_state(*id, state);
            let _ = events.send(AppEvent::RecordingUpdated { recording_id: *id });
        }
        let _ = events.send(AppEvent::BackgroundTaskFinished {
            id: task_id,
            outcome: crate::events::TaskOutcome::Failed(msg),
        });
    };

    let hosts = load_hosts(&store);
    let max_conc = load_max_conc(&store);
    let found = match resolve_playlist(&client, &inputs, &hosts, max_conc).await {
        Some(f) => f,
        None => {
            finish_fail("no live playlist found (past the ~60-day CDN window?)".into());
            return;
        }
    };
    // The host that served this VOD is confirmed-live — learn it so future probes
    // stay current as Twitch rotates CDN distributions.
    record_learned_host(&store, &found.host);

    let quals = enumerate_qualities(&client, &found, max_conc).await;
    let chosen = resolve_quality(&quality, &quals);
    let playlist_url = swap_quality(&found.url, &chosen);

    let recovered = match build_playlist(&client, &playlist_url, max_conc, probe_all, None, None).await {
        Ok(r) => r,
        Err(e) => {
            finish_fail(format!("playlist build failed: {e}"));
            return;
        }
    };
    if recovered.present == 0 {
        finish_fail("no segments survived on the CDN".into());
        return;
    }

    // Where to write: next to a tracked recording, or into the standalone dir.
    let (out_dir, base_stem): (PathBuf, String) = match &sink {
        RecoverySink::Recording(id) => match store.get_recording_paths(*id) {
            Ok(Some((_mid, path))) if !path.is_empty() => {
                let p = PathBuf::from(&path);
                let dir = p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
                let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| format!("rec_{id}"));
                (dir, format!("{stem}.recovered"))
            }
            _ => {
                finish_fail("recording has no output path to attach to".into());
                return;
            }
        },
        RecoverySink::Standalone { output_dir, filename } => {
            let dir = PathBuf::from(output_dir);
            let stem = Path::new(filename)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| filename.clone());
            (dir, stem)
        }
    };

    use crate::iomon::Cat;
    if let Err(e) = crate::iomon::fs::create_dir_all(Cat::Recovery, &out_dir).await {
        finish_fail(format!("cannot create output dir: {e}"));
        return;
    }
    let cache = crate::downloader::cache_dir(&out_dir);
    let _ = crate::iomon::fs::create_dir_all(Cat::Recovery, &cache).await;
    let temp_playlist = cache.join(format!("{base_stem}.m3u8"));
    if let Err(e) = crate::iomon::fs::write(Cat::Recovery, &temp_playlist, &recovered.text).await {
        finish_fail(format!("cannot write playlist: {e}"));
        return;
    }

    let final_stem = crate::downloader::unique_stem(&out_dir, &base_stem, "mkv", None);
    let dst = out_dir.join(format!("{final_stem}.mkv"));

    let mux = mux_playlist_to_mkv(
        &temp_playlist,
        &dst,
        Some((events.clone(), task_id)),
        None,
        "VOD recovery",
    )
    .await;
    let _ = crate::iomon::fs::remove_file(Cat::Recovery, &temp_playlist).await;

    match mux {
        Ok(()) => {
            let state = if recovered.missing == 0 { "recovered" } else { "partial" };
            let bytes = crate::iomon::fs::metadata(Cat::Recovery, &dst)
                .await
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            match &sink {
                RecoverySink::Recording(id) => {
                    let _ = store.set_recording_recovered(*id, &dst.to_string_lossy(), state);
                    let _ = events.send(AppEvent::RecordingUpdated { recording_id: *id });
                }
                RecoverySink::Standalone { output_dir, .. } => {
                    let video = standalone_video(&inputs, output_dir, &dst, &chosen, bytes);
                    match store.insert_video(&video) {
                        Ok(vid) => {
                            let _ = store.finish_video(
                                vid,
                                now_unix(),
                                bytes,
                                Some(0),
                                state,
                                &dst.to_string_lossy(),
                                "",
                            );
                        }
                        Err(e) => tracing::warn!("recovery: insert_video failed: {e:#}"),
                    }
                }
            }
            let note = format!(
                "{}/{} segments · {} un-muted · {} missing",
                recovered.present, recovered.total, recovered.unmuted_recovered, recovered.missing
            );
            let _ = events.send(AppEvent::BackgroundTaskFinished {
                id: task_id,
                outcome: crate::events::TaskOutcome::CompletedWithNote(note),
            });
        }
        Err(e) => finish_fail(format!("mux failed: {e}")),
    }
}

/// Build a completed `Video` row for a standalone (untracked) recovery.
fn standalone_video(
    inputs: &RecoveryInputs,
    output_dir: &str,
    dst: &Path,
    quality: &str,
    bytes: i64,
) -> crate::models::Video {
    crate::models::Video {
        id: 0,
        url: dst.to_string_lossy().into_owned(),
        title: format!("Recovered VOD · {} · {}", inputs.login, inputs.broadcast_id),
        channel: inputs.login.clone(),
        platform: crate::models::Platform::Twitch,
        tool: crate::models::Tool::Ffmpeg,
        tool_binary: String::new(),
        quality: quality.to_string(),
        output_dir: output_dir.to_string(),
        filename_template: String::new(),
        auth_kind: crate::models::AuthKind::Disabled,
        auth_value: String::new(),
        audio_tracks: String::new(),
        subtitle_tracks: String::new(),
        chat_log: false,
        extra_args: String::new(),
        auto_title: false,
        status: "completed".into(),
        output_path: dst.to_string_lossy().into_owned(),
        bytes,
        created_at: now_unix(),
        exit_code: Some(0),
        log_excerpt: String::new(),
        started_at: Some(now_unix()),
        ended_at: Some(now_unix()),
    }
}

// ---------- third-party URL parse + start-time scrape (best-effort, isolated) ----------

/// Best-effort scraping of TwitchTracker / StreamsCharts / SullyGnome to prefill a
/// recovery's start time. Everything returns `Result`; a failure only leaves the
/// timestamp blank for manual entry — it can never abort a recovery.
pub mod scrape {
    use chrono::NaiveDateTime;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Site {
        TwitchTracker,
        StreamsCharts,
        SullyGnome,
    }

    /// Login + broadcast id parsed from a supported stream URL's path.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ParsedVodUrl {
        pub login: String,
        pub broadcast_id: String,
        pub site: Site,
    }

    /// Extract the numeric `/videos/<id>` archive id from a Twitch VOD URL — the
    /// id the GQL fast-path (`gql_vod_info`) needs.
    pub fn twitch_vod_id(url: &str) -> Option<String> {
        let lower = url.to_lowercase();
        let pos = lower.find("twitch.tv/videos/")?;
        let rest = &url[pos + "twitch.tv/videos/".len()..];
        let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        (!id.is_empty()).then_some(id)
    }

    /// Extract `(login, broadcast_id, site)` from a stream URL's path. Accepts
    /// `…/streams/<id>` (TwitchTracker/StreamsCharts) and `…/stream/<id>`
    /// (SullyGnome); **rejects** `twitch.tv/videos/<vod_id>` (that's the archive
    /// id, not the broadcast id the CDN hash needs — use [`twitch_vod_id`] +
    /// [`super::gql_vod_info`] for those).
    pub fn parse_vod_url(url: &str) -> Option<ParsedVodUrl> {
        let u = url.trim();
        let lower = u.to_lowercase();
        if lower.contains("twitch.tv/videos/") {
            return None; // archive id, not broadcast id
        }
        let after = |host: &str| -> Option<Vec<String>> {
            let pos = lower.find(host)?;
            let rest = &u[pos + host.len()..];
            Some(
                rest.split(['?', '#'])
                    .next()
                    .unwrap_or("")
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect(),
            )
        };
        // StreamsCharts: /channels/<login>/streams/<id>
        if lower.contains("streamscharts.com") {
            let parts = after("streamscharts.com/")?;
            let i = parts.iter().position(|p| p == "channels")?;
            let login = parts.get(i + 1)?.clone();
            let j = parts.iter().position(|p| p == "streams")?;
            let id = parts.get(j + 1)?.clone();
            return valid(login, id, Site::StreamsCharts);
        }
        // SullyGnome: /channel/<login>/stream/<id>
        if lower.contains("sullygnome.com") {
            let parts = after("sullygnome.com/")?;
            let i = parts.iter().position(|p| p == "channel")?;
            let login = parts.get(i + 1)?.clone();
            let j = parts.iter().position(|p| p == "stream")?;
            let id = parts.get(j + 1)?.clone();
            return valid(login, id, Site::SullyGnome);
        }
        // TwitchTracker: /<login>/streams/<id>
        if lower.contains("twitchtracker.com") {
            let parts = after("twitchtracker.com/")?;
            let j = parts.iter().position(|p| p == "streams")?;
            let login = parts.get(j.wrapping_sub(1))?.clone();
            let id = parts.get(j + 1)?.clone();
            return valid(login, id, Site::TwitchTracker);
        }
        None
    }

    fn valid(login: String, id: String, site: Site) -> Option<ParsedVodUrl> {
        if login.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) || id.is_empty() {
            return None;
        }
        Some(ParsedVodUrl { login: login.to_lowercase(), broadcast_id: id, site })
    }

    /// Best-effort scrape of the broadcast's UTC start time (epoch secs). The only
    /// field not derivable from the URL. Fragile by nature — the sites block bots
    /// and change markup — so callers treat any error as "leave blank".
    pub async fn scrape_start_time(
        client: &reqwest::Client,
        p: &ParsedVodUrl,
    ) -> anyhow::Result<i64> {
        let url = match p.site {
            Site::TwitchTracker => {
                format!("https://twitchtracker.com/{}/streams/{}", p.login, p.broadcast_id)
            }
            Site::StreamsCharts => {
                format!("https://streamscharts.com/channels/{}/streams/{}", p.login, p.broadcast_id)
            }
            Site::SullyGnome => {
                format!("https://sullygnome.com/channel/{}/stream/{}", p.login, p.broadcast_id)
            }
        };
        // StreamsCharts is the most aggressive about blocking; give it a few tries.
        let retries = if p.site == Site::StreamsCharts { 5 } else { 1 };
        let mut last_err = anyhow::anyhow!("no attempt");
        for _ in 0..retries {
            match fetch_and_parse(client, &url, p.site).await {
                Ok(ts) => return Ok(ts),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    async fn fetch_and_parse(
        client: &reqwest::Client,
        url: &str,
        site: Site,
    ) -> anyhow::Result<i64> {
        let html = client.get(url).send().await?.error_for_status()?.text().await?;
        match site {
            Site::TwitchTracker => parse_twitchtracker(&html),
            Site::StreamsCharts => parse_streamscharts(&html),
            Site::SullyGnome => parse_sullygnome(&html),
        }
    }

    /// TwitchTracker renders `<div class="stream-timestamp-dt …">YYYY-MM-DD HH:MM:SS</div>`
    /// in UTC.
    fn parse_twitchtracker(html: &str) -> anyhow::Result<i64> {
        let inner = tag_inner(html, "stream-timestamp-dt")
            .ok_or_else(|| anyhow::anyhow!("timestamp element not found"))?;
        let dt = NaiveDateTime::parse_from_str(inner.trim(), "%Y-%m-%d %H:%M:%S")?;
        Ok(dt.and_utc().timestamp())
    }

    /// StreamsCharts renders `<time class="…">DD Mon YYYY HH:MM</time>` (UTC).
    fn parse_streamscharts(html: &str) -> anyhow::Result<i64> {
        let inner = between(html, "<time", "</time>")
            .and_then(|seg| seg.split('>').nth(1).map(str::to_string))
            .ok_or_else(|| anyhow::anyhow!("<time> element not found"))?;
        let cleaned = inner.replace(',', "");
        let stamp = format!("{}:00", cleaned.trim());
        let dt = NaiveDateTime::parse_from_str(&stamp, "%d %b %Y %H:%M:%S")?;
        Ok(dt.and_utc().timestamp())
    }

    /// SullyGnome renders the stream date without a year; assume the current UTC
    /// year (streams older than ~60 days are unrecoverable anyway).
    fn parse_sullygnome(html: &str) -> anyhow::Result<i64> {
        let inner = tag_inner(html, "MiddleSubHeaderItemValue")
            .ok_or_else(|| anyhow::anyhow!("date element not found"))?;
        let cleaned = strip_ordinals(inner.trim());
        // e.g. "Monday 15 January 18:30"
        let year = chrono::Utc::now().format("%Y").to_string();
        let stamp = format!("{year} {cleaned}:00");
        let dt = NaiveDateTime::parse_from_str(&stamp, "%Y %A %d %B %H:%M:%S")?;
        Ok(dt.and_utc().timestamp())
    }

    /// Inner text of the first element carrying `class="… <class_name> …"`.
    fn tag_inner(html: &str, class_name: &str) -> Option<String> {
        let idx = html.find(class_name)?;
        let close = html[idx..].find('>')? + idx + 1;
        let end = html[close..].find('<')? + close;
        Some(html[close..end].trim().to_string())
    }

    /// The substring from `start` up to and including `end` (inclusive of `start`).
    fn between<'a>(html: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let i = html.find(start)?;
        let j = html[i..].find(end)? + i;
        Some(&html[i..j])
    }

    /// Drop ordinal suffixes ("15th" → "15") so chrono can parse the day.
    fn strip_ordinals(s: &str) -> String {
        s.split_whitespace()
            .map(|w| {
                let trimmed = w.trim_end_matches(|c: char| c.is_ascii_alphabetic());
                if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
                    trimmed.to_string()
                } else {
                    w.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_task_carries_recording_id_for_recording_sink() {
        let (label, rec_id) = recovery_task_label_and_id(&RecoverySink::Recording(42));
        assert_eq!(rec_id, Some(42));
        assert!(label.contains("42"));
    }

    #[test]
    fn recovery_task_has_no_recording_id_for_standalone_sink() {
        let sink = RecoverySink::Standalone {
            output_dir: "out".into(),
            filename: "clip.mkv".into(),
        };
        let (label, rec_id) = recovery_task_label_and_id(&sink);
        assert_eq!(rec_id, None);
        assert!(label.contains("clip.mkv"));
    }

    #[test]
    fn sha1_known_vector() {
        // SHA1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        assert_eq!(sha1_hex20("abc"), "a9993e364706816aba3e");
        assert_eq!(sha1_hex20("abc").len(), 20);
        // A real Twitch CDN folder hash (VOD yuy_ix / 318355078359 / 1783199285).
        assert_eq!(
            sha1_hex20("yuy_ix_318355078359_1783199285"),
            "ec758f66a255b6df912f"
        );
    }

    #[test]
    fn candidate_url_shape() {
        let inp = RecoveryInputs {
            login: "streamer".into(),
            broadcast_id: "123456".into(),
            start_epoch: 1_700_000_000,
            went_live_approx: false,
            vod_id: None,
        };
        let hosts = vec!["https://vod-secure.twitch.tv/".to_string()];
        let urls = candidate_urls(&inp, 0, &hosts);
        assert_eq!(urls.len(), 1);
        let base = "streamer_123456_1700000000";
        let hash = sha1_hex20(base);
        assert_eq!(
            urls[0],
            format!("https://vod-secure.twitch.tv/{hash}_{base}/chunked/index-dvr.m3u8")
        );
        // A one-second offset changes the epoch (and thus the hash).
        assert_ne!(candidate_urls(&inp, 1, &hosts)[0], urls[0]);
    }

    #[test]
    fn canonical_segment_variants() {
        assert_eq!(canonical_segment("123.ts"), ("123.ts".to_string(), false));
        assert_eq!(canonical_segment("123-muted.ts"), ("123.ts".to_string(), true));
        assert_eq!(canonical_segment("123-unmuted.ts"), ("123.ts".to_string(), true));
        // fMP4 VODs (the modern default) name segments `.mp4`.
        assert_eq!(canonical_segment("1404-unmuted.mp4"), ("1404.mp4".to_string(), true));
    }

    #[test]
    fn segment_line_accepts_ts_and_mp4() {
        assert!(is_segment_line("123.ts"));
        assert!(is_segment_line("1404-unmuted.mp4"));
        assert!(!is_segment_line("#EXTINF:10.000,"));
        assert!(!is_segment_line(""));
        assert!(!is_segment_line("#EXT-X-ENDLIST"));
    }

    #[test]
    fn muted_variant_inserts_before_extension() {
        // Real case: playlist lists a dead `-unmuted` name, but the surviving copy
        // is the constructed `-muted` one.
        assert_eq!(
            muted_variant("https://h/x/chunked/", "1404.mp4"),
            "https://h/x/chunked/1404-muted.mp4"
        );
        assert_eq!(
            muted_variant("https://h/x/chunked/", "123.ts"),
            "https://h/x/chunked/123-muted.ts"
        );
    }

    #[test]
    fn truncate_playlist_stops_at_max_secs_and_appends_endlist() {
        // A live playlist: no #EXT-X-ENDLIST, 10 s segments.
        let src = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n\
                   #EXTINF:10.000,\n0.ts\n#EXTINF:10.000,\n1.ts\n\
                   #EXTINF:10.000,\n2.ts\n#EXTINF:10.000,\n3.ts\n";
        // 25 s cap → segments 0,1,2 (acc hits 30 ≥ 25 before segment 3).
        let out = truncate_playlist(src, 25.0);
        assert!(out.contains("2.ts"), "{out}");
        assert!(!out.contains("3.ts"), "{out}");
        assert!(out.ends_with("#EXT-X-ENDLIST\n"), "{out}");
        // Header lines survive.
        assert!(out.contains("#EXT-X-TARGETDURATION:10"), "{out}");
    }

    #[test]
    fn truncate_playlist_is_noop_when_shorter_than_cap() {
        let src = "#EXTM3U\n#EXTINF:10.000,\n0.ts\n#EXT-X-ENDLIST\n";
        let out = truncate_playlist(src, 300.0);
        assert!(out.contains("0.ts"), "{out}");
        // Exactly one terminator (the source's own is re-appended, not doubled).
        assert_eq!(out.matches("#EXT-X-ENDLIST").count(), 1, "{out}");
    }

    #[test]
    fn truncate_playlist_window_skips_then_bounds_the_window() {
        // 5 segments of 10s each (0..50s total).
        let src = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n\
                   #EXTINF:10.000,\n0.ts\n#EXTINF:10.000,\n1.ts\n\
                   #EXTINF:10.000,\n2.ts\n#EXTINF:10.000,\n3.ts\n\
                   #EXTINF:10.000,\n4.ts\n";
        // skip 20s (drops 0,1), keep a 15s window (acc 20->30 keeps 2, then
        // 30->40 keeps 3 since kept(10) < window(15), stops before 4).
        let out = truncate_playlist_window(src, 20.0, 15.0);
        assert!(!out.contains("0.ts") && !out.contains("1.ts"), "{out}");
        assert!(out.contains("2.ts") && out.contains("3.ts"), "{out}");
        assert!(!out.contains("4.ts"), "{out}");
        assert!(out.ends_with("#EXT-X-ENDLIST\n"), "{out}");
        assert!(out.contains("#EXT-X-TARGETDURATION:10"), "{out}");
    }

    #[test]
    fn truncate_playlist_window_skip_past_total_length_yields_nothing() {
        let src = "#EXTM3U\n#EXTINF:10.000,\n0.ts\n#EXTINF:10.000,\n1.ts\n#EXT-X-ENDLIST\n";
        let out = truncate_playlist_window(src, 100.0, 30.0);
        assert!(!out.contains("0.ts") && !out.contains("1.ts"), "{out}");
        assert_eq!(out, "#EXTM3U\n#EXT-X-ENDLIST\n");
    }

    #[test]
    fn truncate_playlist_window_zero_skip_matches_plain_truncate() {
        let src = "#EXTM3U\n#EXTINF:10.000,\n0.ts\n#EXTINF:10.000,\n1.ts\n#EXTINF:10.000,\n2.ts\n";
        assert_eq!(
            truncate_playlist_window(src, 0.0, 15.0),
            truncate_playlist(src, 15.0)
        );
    }

    #[test]
    fn playlist_prefix_strips_filename() {
        assert_eq!(
            playlist_prefix("https://host/abc_streamer_123/chunked/index-dvr.m3u8"),
            "https://host/abc_streamer_123/chunked/"
        );
    }

    #[test]
    fn swap_quality_replaces_first_chunked_only() {
        assert_eq!(
            swap_quality("https://h/x/chunked/index-dvr.m3u8", "720p60"),
            "https://h/x/720p60/index-dvr.m3u8"
        );
    }

    #[test]
    fn resolve_quality_prefers_request_then_best_then_chunked() {
        let avail = vec!["chunked".to_string(), "720p60".to_string()];
        assert_eq!(resolve_quality("720p60", &avail), "720p60");
        assert_eq!(resolve_quality("1080p60", &avail), "chunked"); // unavailable → best (first)
        assert_eq!(resolve_quality("", &avail), "chunked");
        assert_eq!(resolve_quality("720p60", &[]), "chunked");
    }

    #[test]
    fn host_normalization() {
        assert_eq!(normalize_host("d3stzm2eumvgb4.cloudfront.net"), "https://d3stzm2eumvgb4.cloudfront.net/");
        assert_eq!(normalize_host("https://d3stzm2eumvgb4.cloudfront.net"), "https://d3stzm2eumvgb4.cloudfront.net/");
        assert_eq!(normalize_host("  https://x/  "), "https://x/");
        assert_eq!(
            parse_host_list("a.net\nb.net, c.net\n\n"),
            vec!["https://a.net/", "https://b.net/", "https://c.net/"]
        );
    }

    #[test]
    fn seek_previews_and_folder_parse() {
        // Real seekPreviewsURL (login without underscore).
        let (host, folder) = parse_seek_previews(
            "https://d2nvs31859zcd8.cloudfront.net/d8727012be77965c38bc_camila_318354228567_1783193802/storyboards/2812222223-info.json",
        )
        .unwrap();
        assert_eq!(host, "d2nvs31859zcd8.cloudfront.net");
        assert_eq!(folder, "d8727012be77965c38bc_camila_318354228567_1783193802");
        assert_eq!(
            folder_parts(&folder),
            Some(("camila".to_string(), "318354228567".to_string(), 1_783_193_802))
        );
        // Login WITH an underscore must be re-joined (peel hash head + broadcast/epoch tail).
        assert_eq!(
            folder_parts("ec758f66a255b6df912f_yuy_ix_318355078359_1783199285"),
            Some(("yuy_ix".to_string(), "318355078359".to_string(), 1_783_199_285))
        );
    }

    /// End-to-end against real DMCA-muted Twitch VODs. Network-gated — run explicitly:
    /// `cargo test --bin streamarchiver -- --ignored --nocapture recovery_network`.
    /// Covers both segment formats (`.mp4`/`.ts`), the GQL fast-path (via vod_id),
    /// the hash-probe fallback (vod_id=None), and a VOD whose host needed adding.
    #[tokio::test]
    #[ignore = "hits the Twitch CDN + GQL"]
    async fn recovery_network_end_to_end() {
        // (login, broadcast_id, vod_id, createdAt, true_folder_epoch, muted-ext)
        let cases = [
            ("yuy_ix", "318355078359", "2812280160", 1_783_199_290i64, 1_783_199_285i64, "-muted.mp4"),
            ("camila", "318354228567", "2812222223", 1_783_193_806, 1_783_193_802, "-muted.ts"),
            ("vinesauce", "319375842272", "2812178289", 1_783_190_086, 1_783_190_081, "-muted.ts"),
        ];
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap();
        let hosts: Vec<String> = CDN_HOSTS.iter().map(|h| h.to_string()).collect();
        for (login, bid, vod_id, seeded, true_epoch, muted_ext) in cases {
            // First via the GQL fast-path (vod_id set), then the hash-probe fallback.
            for use_gql in [true, false] {
                let inputs = RecoveryInputs {
                    login: login.into(),
                    broadcast_id: bid.into(),
                    // Seed the VOD `createdAt` (a few secs after the true folder second)
                    // so the symmetric hash search must resolve the real start.
                    start_epoch: seeded,
                    went_live_approx: false,
                    vod_id: use_gql.then(|| vod_id.to_string()),
                };
                let found = resolve_playlist(&client, &inputs, &hosts, 16)
                    .await
                    .unwrap_or_else(|| panic!("{login} (gql={use_gql}): playlist should resolve"));
                assert_eq!(found.matched_epoch, true_epoch, "{login} (gql={use_gql}): true start");

                let recovered = build_playlist(&client, &found.url, 16, false, None, None).await.unwrap();
                eprintln!(
                    "{login} (gql={use_gql}): {}/{} present, {} un-muted, {} muted-fallback, {} missing",
                    recovered.present,
                    recovered.total,
                    recovered.unmuted_recovered,
                    recovered.muted_used,
                    recovered.missing
                );
                assert!(recovered.total > 0, "{login}: has media segments");
                assert_eq!(recovered.missing, 0, "{login}: every muted segment resolved");
                assert!(!recovered.text.contains("-unmuted"), "{login}: no dead -unmuted pointers");
                assert!(recovered.text.contains(muted_ext), "{login}: muted copies substituted");
                assert!(recovered.muted_used > 0, "{login}: at least one segment stayed silenced");
            }
        }
    }

    #[test]
    fn parse_vod_url_accepts_streams_rejects_videos() {
        use scrape::{parse_vod_url, Site};
        let tt = parse_vod_url("https://twitchtracker.com/streamer/streams/49135080904").unwrap();
        assert_eq!(tt.login, "streamer");
        assert_eq!(tt.broadcast_id, "49135080904");
        assert_eq!(tt.site, Site::TwitchTracker);

        let sc =
            parse_vod_url("https://streamscharts.com/channels/StreamER/streams/49135080904").unwrap();
        assert_eq!(sc.login, "streamer"); // lowercased
        assert_eq!(sc.site, Site::StreamsCharts);

        let sg = parse_vod_url("https://sullygnome.com/channel/streamer/stream/49135080904").unwrap();
        assert_eq!(sg.site, Site::SullyGnome);

        // Archive id is NOT a broadcast id.
        assert!(parse_vod_url("https://www.twitch.tv/videos/123456789").is_none());
        // Non-numeric id rejected.
        assert!(parse_vod_url("https://twitchtracker.com/streamer/streams/abc").is_none());
    }

    #[test]
    fn twitch_vod_id_extracts_numeric_archive_id() {
        use scrape::twitch_vod_id;
        assert_eq!(
            twitch_vod_id("https://www.twitch.tv/videos/2812178289?filter=archives&sort=time"),
            Some("2812178289".to_string())
        );
        assert_eq!(twitch_vod_id("twitch.tv/videos/123"), Some("123".to_string()));
        assert_eq!(twitch_vod_id("https://twitch.tv/streamer/streams/999"), None);
        assert_eq!(twitch_vod_id("https://twitchtracker.com/x/streams/999"), None);
    }
}
