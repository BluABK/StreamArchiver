//! Rebuilding a clip that no longer exists upstream.
//!
//! # Three time frames
//!
//! Everything here is a conversion between three clocks, and mixing them up is
//! the bug this module exists to avoid:
//!
//! - **VOD time** — seconds into Twitch's published VOD. `clip.vod_offset_secs`
//!   is in this frame, and so is the DVR playlist (`index-dvr.m3u8`), which is
//!   why the CDN tier needs no conversion at all and is frame-exact.
//! - **Broadcast time** — seconds since `went_live_at`. Equal to VOD time for a
//!   VOD that was not trimmed.
//! - **Local-file time** — seconds into *our* recording, which starts when we
//!   joined, may have a backfilled head prepended, may contain ad filler the VOD
//!   omits, and may be missing segments we lost. Four corrections, below.
//!
//! # The ladder
//!
//! 1. `live` — the clip still exists: just download it (`fetch`).
//! 2. `cdn-vod` — frame-exact, but dies with the VOD (~60 days).
//! 3. `local-cut` — free, never expires, higher quality than the clip ever was,
//!    but approximate. Preferred over the CDN when the offset maths out exact.
//! 4. `cdn-clip` — the legacy standalone object; one cheap probe, the only hope
//!    for old clips with no recovery keys.
//! 5. `local-guess` — no keys at all: cut around `created_at` and say so.

use super::*;
use crate::models::Recording;
use std::path::Path;
use tracing::{debug, warn};

/// Padding around an exact cut — covers segment granularity and rounding.
const PAD_EXACT_SECS: f64 = 3.0;
/// Padding around an approximate cut. Deliberately large: an honest ±30 s clip
/// is a useful archive, a confidently-wrong one is worse than nothing.
const PAD_APPROX_SECS: f64 = 30.0;
/// Clips are usually created within a minute of the moment they capture, so a
/// keyless guess brackets `created_at` by this much.
const GUESS_BACK_SECS: f64 = 30.0;

/// How much to trust a reconstructed cut's position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// Every correction term is known and zero-risk.
    Exact,
    /// At least one term is a guess — the cut is padded and labelled.
    Approx,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Exact => "exact",
            Confidence::Approx => "approx",
        }
    }
    fn pad(self) -> f64 {
        match self {
            Confidence::Exact => PAD_EXACT_SECS,
            Confidence::Approx => PAD_APPROX_SECS,
        }
    }
}

/// A cut to make out of a local recording.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalCut {
    /// Seconds into the local file, already padded. Never negative.
    pub start: f64,
    /// Length of the cut, already padded.
    pub len: f64,
    pub confidence: Confidence,
}

/// Everything the offset maths needs about the take, gathered once so the
/// conversion itself stays pure and testable.
#[derive(Clone, Debug, Default)]
pub struct TakeFrame {
    pub started_at: i64,
    pub went_live_at: Option<i64>,
    /// A detection-clock guess rather than a real go-live time.
    pub went_live_approx: bool,
    /// True once head-backfill has prepended the missed intro, moving the
    /// file's t=0 back to the start of the broadcast.
    pub head_backfilled: bool,
    /// The take captured ad filler the published VOD does not contain, as
    /// `(broadcast_secs, duration_secs)`. Makes our file run LONGER than the
    /// VOD, so a given moment sits later in it.
    pub ad_breaks: Vec<(f64, f64)>,
    /// Segment losses never spliced back in, as `(start, end)` in broadcast
    /// time. Makes our file run SHORTER.
    pub unspliced_gaps: Vec<(f64, f64)>,
    /// The take's own recorded length, when known — used only to reject a cut
    /// that lands outside the file.
    pub duration_secs: Option<f64>,
}

impl TakeFrame {
    pub fn from_recording(rec: &Recording) -> TakeFrame {
        TakeFrame {
            started_at: rec.started_at,
            went_live_at: rec.went_live_at,
            went_live_approx: rec.went_live_approx,
            head_backfilled: rec.head_backfill_state == "done",
            ad_breaks: Vec::new(),
            unspliced_gaps: Vec::new(),
            duration_secs: rec
                .ended_at
                .map(|e| (e - rec.started_at).max(0) as f64),
        }
    }

    /// Seconds of the broadcast we missed before joining.
    fn join_estimate(&self) -> f64 {
        (self.started_at - self.went_live_at.unwrap_or(self.started_at)).max(0) as f64
    }

    /// How much earlier the file's t=0 sits once a head was backfilled — the
    /// same value `chapters::head_shift_for` computes, and for the same reason.
    fn head_shift(&self) -> f64 {
        if self.head_backfilled {
            self.join_estimate()
        } else {
            0.0
        }
    }
}

/// Total ad filler recorded before a point in broadcast time.
fn ads_before(frame: &TakeFrame, at: f64) -> f64 {
    frame
        .ad_breaks
        .iter()
        .filter(|(start, _)| *start < at)
        .map(|(_, dur)| *dur)
        .sum()
}

/// Total unspliced loss before a point in broadcast time. A gap straddling the
/// point counts only up to it.
fn gaps_before(frame: &TakeFrame, at: f64) -> f64 {
    frame
        .unspliced_gaps
        .iter()
        .filter(|(start, _)| *start < at)
        .map(|(start, end)| (end.min(at) - start).max(0.0))
        .sum()
}

/// Convert a VOD offset into a position in our local file.
///
/// ```text
/// local_t = vod_offset
///         − join_estimate      // we joined late; the file starts mid-broadcast
///         + head_shift         // …unless the head was backfilled back on
///         + ads_before         // we captured filler the VOD omits → file runs long
///         − gaps_before        // we lost segments → file runs short
/// ```
///
/// Returns `None` when the result falls outside the recording — a clip from
/// before we joined, or past where we stopped, cannot be cut from it.
pub fn local_position(frame: &TakeFrame, vod_offset: f64) -> Option<f64> {
    let t = vod_offset - frame.join_estimate() + frame.head_shift() + ads_before(frame, vod_offset)
        - gaps_before(frame, vod_offset);
    if t < 0.0 {
        return None;
    }
    if let Some(d) = frame.duration_secs
        && t > d
    {
        return None;
    }
    Some(t)
}

/// How much the position above can be trusted.
///
/// `Exact` requires every term to be known and safe: a real go-live time, no ad
/// filler, and no unspliced gap before the point. Anything else is `Approx` and
/// gets the wide pad — claiming precision we do not have would produce a
/// confidently-wrong file, which in an archive is worse than an obviously rough
/// one.
pub fn confidence_for(frame: &TakeFrame, vod_offset: f64) -> Confidence {
    if frame.went_live_approx {
        return Confidence::Approx;
    }
    if frame.went_live_at.is_none() {
        return Confidence::Approx;
    }
    if ads_before(frame, vod_offset) > 0.0 {
        return Confidence::Approx;
    }
    if gaps_before(frame, vod_offset) > 0.0 {
        return Confidence::Approx;
    }
    Confidence::Exact
}

/// Plan a local cut for a clip whose parent take we hold.
pub fn plan_local_cut(frame: &TakeFrame, vod_offset: f64, duration: f64) -> Option<LocalCut> {
    let start = local_position(frame, vod_offset)?;
    let confidence = confidence_for(frame, vod_offset);
    let pad = confidence.pad();
    Some(LocalCut {
        start: (start - pad).max(0.0),
        len: duration + pad * 2.0,
        confidence,
    })
}

/// Plan a cut for a clip with **no** recovery keys, from its creation time.
///
/// A clip is usually made within a minute of the moment it captures, so bracket
/// `created_at`. Always approximate, and labelled as such — this is a guess and
/// the UI must not present it as anything else.
pub fn plan_guess_cut(frame: &TakeFrame, created_at: i64, duration: f64) -> Option<LocalCut> {
    // created_at is wall-clock; convert to broadcast time first.
    let live = frame.went_live_at.unwrap_or(frame.started_at);
    let broadcast_t = (created_at - live) as f64;
    // The clip captures the moments *before* it was made.
    let start_guess = broadcast_t - duration - GUESS_BACK_SECS;
    let start = local_position(frame, start_guess.max(0.0))?;
    Some(LocalCut {
        start: (start - PAD_APPROX_SECS).max(0.0),
        len: duration + GUESS_BACK_SECS + PAD_APPROX_SECS * 2.0,
        confidence: Confidence::Approx,
    })
}

/// Which rung of the ladder to try for a clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Still alive upstream — an ordinary download.
    Live,
    /// Rebuild from the parent VOD's CDN segments. Frame-exact.
    CdnVod,
    /// Cut from our own recording. Free and permanent, but approximate unless
    /// every correction term is known.
    LocalCut,
    /// The legacy standalone clip object — one cheap probe.
    CdnClip,
    /// No keys: bracket the clip's creation time in our recording.
    LocalGuess,
    /// Nothing can be done.
    Unrecoverable,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Live => "live",
            Tier::CdnVod => "cdn-vod",
            Tier::LocalCut => "local-cut",
            Tier::CdnClip => "cdn-clip",
            Tier::LocalGuess => "local-guess",
            Tier::Unrecoverable => "",
        }
    }
}

/// Twitch keeps VODs for 14–60 days depending on the channel; past this the CDN
/// tier is pointless to attempt.
const CDN_WINDOW_SECS: i64 = 60 * 24 * 3600;

/// Pick the rung to try, given what we know.
///
/// Local-cut is preferred over the CDN **only when the offset maths out exact**
/// — then it is free, instant, permanent, and higher quality than the clip ever
/// was. Otherwise the CDN's frame-exactness wins while the VOD still lives.
pub fn choose_tier(
    gone: bool,
    has_keys: bool,
    local_cut: Option<Confidence>,
    vod_age_secs: i64,
) -> Tier {
    if !gone {
        return Tier::Live;
    }
    let cdn_possible = has_keys && vod_age_secs <= CDN_WINDOW_SECS;
    match (local_cut, cdn_possible) {
        (Some(Confidence::Exact), _) => Tier::LocalCut,
        (_, true) => Tier::CdnVod,
        (Some(Confidence::Approx), false) if has_keys => Tier::LocalCut,
        (Some(Confidence::Approx), false) => Tier::LocalGuess,
        (None, false) => Tier::CdnClip,
    }
}

/// The legacy standalone clip object. 403s for anything modern, but it is one
/// request and the only hope for the ~95% of old clips with no recovery keys.
pub fn legacy_clip_url(slug: &str) -> String {
    format!("https://clips-media-assets2.twitch.tv/AT-cm%7C{slug}.mp4")
}

/// Rebuild a clip from its parent VOD's CDN segments — the frame-exact tier.
///
/// `skip_secs = vod_offset` is directly correct with no conversion: the DVR
/// playlist *is* the VOD's own timeline. `truncate_playlist_window` trims the
/// playlist text before any segment work, so only the handful of segments the
/// clip actually covers are ever probed.
///
/// Resolution order matters for politeness: the cached `vod_cdn` folder first
/// (zero requests), then `gql_vod_info` (one). The generic host-probing
/// fallback is never used here — it costs ~2,400 HEADs, which is acceptable
/// once for a VOD and never per clip.
pub async fn rebuild_from_cdn(
    store: &Store,
    client: &reqwest::Client,
    clip: &Clip,
    dst: &Path,
    max_conc: usize,
) -> anyhow::Result<()> {
    let Some(offset) = clip.vod_offset_secs else {
        anyhow::bail!("no vod offset");
    };
    let folder = match store.get_vod_cdn(&clip.vod_id)? {
        Some(v) => (v.host, v.folder),
        None => {
            let info = crate::recovery::gql_vod_info(client, &clip.vod_id).await?;
            let _ = store.put_vod_cdn(&crate::store::VodCdnRow {
                vod_id: clip.vod_id.clone(),
                host: info.host.clone(),
                folder: info.folder.clone(),
                login: info.login,
                broadcast_id: info.broadcast_id,
                start_epoch: info.start_epoch,
                learned_at: crate::models::now_unix(),
            });
            (info.host, info.folder)
        }
    };
    let url = format!("https://{}/{}/chunked/index-dvr.m3u8", folder.0, folder.1);
    let dur = clip.duration_secs().max(1.0);
    let pl = crate::recovery::build_playlist(
        client,
        &url,
        max_conc,
        /* probe_all */ true,
        Some(dur),
        Some(offset as f64),
    )
    .await?;
    if pl.present == 0 {
        anyhow::bail!("no surviving segments for that window");
    }
    let tmp = dst.with_extension("clip.m3u8");
    crate::iomon::fs::write(crate::iomon::Cat::Recovery, &tmp, pl.text.as_bytes()).await?;
    let res = crate::recovery::mux_playlist_to_mkv(
        &tmp,
        dst,
        None,
        Some(dur),
        "clip recovery",
        None,
    )
    .await;
    let _ = crate::iomon::fs::remove_file(crate::iomon::Cat::Recovery, &tmp).await;
    res?;
    Ok(())
}

/// Try the legacy standalone clip object. Cheap, usually fails, occasionally
/// saves an old clip nothing else could.
pub async fn try_legacy_object(
    client: &reqwest::Client,
    slug: &str,
    dst: &Path,
) -> anyhow::Result<()> {
    let url = legacy_clip_url(slug);
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("legacy clip object {}", resp.status());
    }
    let bytes = resp.bytes().await?;
    if bytes.len() < 1024 {
        anyhow::bail!("legacy clip object too small to be a video");
    }
    crate::iomon::fs::write(crate::iomon::Cat::Recovery, dst, &bytes).await?;
    debug!("clips: recovered {slug} from the legacy CDN object");
    Ok(())
}

/// Cut a range out of a local recording with a stream copy.
///
/// Gated on `io_gate::local_pass` like every other full-file local pass, so a
/// clip rebuild cannot compete with a remux or a finalize on the same disk.
pub async fn cut_local(src: &Path, dst: &Path, cut: LocalCut) -> anyhow::Result<()> {
    use tokio::process::Command;
    let _gate =
        crate::io_gate::local_pass(&crate::io_gate::gate_label("clip-cut", dst), dst).await;
    let mut cmd = Command::new("ffmpeg");
    // -ss before -i seeks the input (fast, keyframe-accurate); the pad either
    // side is what absorbs the keyframe rounding.
    cmd.arg("-y")
        .arg("-ss")
        .arg(format!("{:.3}", cut.start))
        .arg("-t")
        .arg(format!("{:.3}", cut.len))
        .arg("-i")
        .arg(src)
        .arg("-map")
        .arg("0:v?")
        .arg("-map")
        .arg("0:a?")
        .arg("-c")
        .arg("copy")
        .arg("-nostats")
        .arg("-loglevel")
        .arg("error")
        .arg(dst)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = cmd.output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg clip cut failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Run the ladder for one clip that has gone missing upstream.
///
/// Records which rung produced the file in `recovery_method`, and the cut's
/// `offset_confidence` — a reconstructed clip must never present itself as the
/// original, especially when the position was approximate.
pub async fn recover_clip(
    store: &Store,
    client: &reqwest::Client,
    clip_id: i64,
    max_conc: usize,
) -> bool {
    let Ok(Some(clip)) = store.get_clip(clip_id) else {
        return false;
    };
    let now = crate::models::now_unix();

    // What could a local cut give us, if anything?
    let take = clip
        .recording_id
        .and_then(|id| store.get_recording(id).ok().flatten())
        .filter(|r| !r.output_path.is_empty());
    let frame = take.as_ref().map(TakeFrame::from_recording);
    let planned = match (&frame, clip.vod_offset_secs) {
        (Some(f), Some(off)) => plan_local_cut(f, off as f64, clip.duration_secs()),
        (Some(f), None) => plan_guess_cut(f, clip.created_at, clip.duration_secs()),
        _ => None,
    };

    let tier = choose_tier(
        true,
        clip.has_recovery_keys(),
        planned.map(|c| c.confidence),
        now - clip.created_at,
    );

    let Some(dst) = rebuild_destination(&clip, take.as_ref()) else {
        return false;
    };
    let (ok, method, confidence) = match tier {
        Tier::LocalCut | Tier::LocalGuess => {
            let (Some(t), Some(cut)) = (take.as_ref(), planned) else {
                return false;
            };
            let src = Path::new(&t.output_path);
            match cut_local(src, &dst, cut).await {
                Ok(()) => (true, tier.as_str(), cut.confidence.as_str()),
                Err(e) => {
                    debug!("clips: local cut failed for {}: {e:#}", clip.slug);
                    (false, "", "")
                }
            }
        }
        Tier::CdnVod => match rebuild_from_cdn(store, client, &clip, &dst, max_conc).await {
            // Frame-exact: skip_secs IS the VOD's own clock, no conversion.
            Ok(()) => (true, tier.as_str(), Confidence::Exact.as_str()),
            Err(e) => {
                debug!("clips: CDN rebuild failed for {}: {e:#}", clip.slug);
                (false, "", "")
            }
        },
        Tier::CdnClip => match try_legacy_object(client, &clip.slug, &dst).await {
            Ok(()) => (true, tier.as_str(), ""),
            Err(e) => {
                debug!("clips: legacy object failed for {}: {e:#}", clip.slug);
                (false, "", "")
            }
        },
        Tier::Live | Tier::Unrecoverable => (false, "", ""),
    };

    if ok {
        let bytes = crate::iomon::fs::metadata(crate::iomon::Cat::Recovery, &dst)
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let _ = store.finish_clip(clip_id, &dst.to_string_lossy(), bytes, method, confidence);
        true
    } else {
        log_unrecoverable(&clip);
        let _ = store.fail_clip(clip_id, "recovery exhausted every route");
        false
    }
}

/// Where a rebuilt clip is written: beside the parent take's own clips folder
/// when we have one, else the channel's.
fn rebuild_destination(clip: &Clip, take: Option<&Recording>) -> Option<std::path::PathBuf> {
    let dir = take
        .and_then(|t| Path::new(&t.output_path).parent().map(|p| p.join("clips")))?;
    let stem = super::fetch::clip_stem(clip);
    Some(dir.join(format!("{stem}.rebuilt.mkv")))
}

/// Log what a failed recovery means, in terms of what is actually lost.
pub fn log_unrecoverable(clip: &Clip) {
    warn!(
        slug = %clip.slug,
        has_keys = clip.has_recovery_keys(),
        "clips: {} cannot be rebuilt — {}",
        clip.title,
        if clip.has_recovery_keys() {
            "the parent VOD's segments are gone from the CDN and we hold no local recording of it"
        } else {
            "its parent VOD expired before we indexed it, so Twitch never told us where it came from"
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> TakeFrame {
        TakeFrame {
            started_at: 1_000,
            went_live_at: Some(1_000),
            went_live_approx: false,
            head_backfilled: false,
            ad_breaks: Vec::new(),
            unspliced_gaps: Vec::new(),
            duration_secs: Some(10_000.0),
        }
    }

    #[test]
    fn a_clean_take_maps_vod_time_straight_through() {
        // Joined exactly at go-live, no ads, no gaps: the two clocks agree.
        assert_eq!(local_position(&frame(), 500.0), Some(500.0));
        assert_eq!(confidence_for(&frame(), 500.0), Confidence::Exact);
    }

    #[test]
    fn joining_late_shifts_the_local_position_earlier() {
        // We started 120s into the broadcast, so broadcast t=500 is only 380s
        // into our file.
        let mut f = frame();
        f.started_at = 1_120;
        assert_eq!(local_position(&f, 500.0), Some(380.0));
    }

    #[test]
    fn a_backfilled_head_cancels_the_join_offset() {
        // Head backfill prepends the missed intro, so the file's t=0 is the
        // broadcast's t=0 again — the same head_shift chapters computes.
        let mut f = frame();
        f.started_at = 1_120;
        f.head_backfilled = true;
        assert_eq!(local_position(&f, 500.0), Some(500.0));
    }

    #[test]
    fn ad_filler_pushes_a_moment_later_in_our_file() {
        // The VOD omits mid-rolls; we recorded them, so our file runs long and
        // the same broadcast moment sits later in it.
        let mut f = frame();
        f.ad_breaks = vec![(100.0, 30.0), (300.0, 60.0)];
        assert_eq!(local_position(&f, 500.0), Some(590.0));
        // A break AFTER the point must not count.
        f.ad_breaks.push((900.0, 60.0));
        assert_eq!(local_position(&f, 500.0), Some(590.0));
        assert_eq!(confidence_for(&f, 500.0), Confidence::Approx);
    }

    #[test]
    fn lost_segments_pull_a_moment_earlier() {
        let mut f = frame();
        f.unspliced_gaps = vec![(100.0, 140.0)];
        assert_eq!(local_position(&f, 500.0), Some(460.0));
        // A gap straddling the point counts only up to it.
        f.unspliced_gaps = vec![(480.0, 520.0)];
        assert_eq!(local_position(&f, 500.0), Some(480.0));
    }

    #[test]
    fn a_clip_from_before_we_joined_cannot_be_cut_from_our_file() {
        let mut f = frame();
        f.started_at = 1_600; // joined 600s in
        assert_eq!(local_position(&f, 100.0), None);
    }

    #[test]
    fn a_clip_past_the_end_of_our_take_is_refused() {
        let f = frame(); // 10_000s long
        assert_eq!(local_position(&f, 99_999.0), None);
    }

    #[test]
    fn an_approximate_go_live_time_can_never_be_exact() {
        // went_live_approx means the join estimate is a detection-clock guess,
        // which can be minutes out.
        let mut f = frame();
        f.went_live_approx = true;
        assert_eq!(confidence_for(&f, 500.0), Confidence::Approx);

        let mut f = frame();
        f.went_live_at = None;
        assert_eq!(confidence_for(&f, 500.0), Confidence::Approx);
    }

    #[test]
    fn an_approximate_cut_is_padded_ten_times_wider_than_an_exact_one() {
        // The pad is the honesty mechanism: a wide, obviously-rough clip beats
        // a narrow, confidently-wrong one.
        let exact = plan_local_cut(&frame(), 500.0, 20.0).unwrap();
        assert_eq!(exact.confidence, Confidence::Exact);
        assert_eq!(exact.start, 500.0 - PAD_EXACT_SECS);
        assert_eq!(exact.len, 20.0 + PAD_EXACT_SECS * 2.0);

        let mut f = frame();
        f.went_live_approx = true;
        let approx = plan_local_cut(&f, 500.0, 20.0).unwrap();
        assert_eq!(approx.confidence, Confidence::Approx);
        assert!(approx.len > exact.len * 3.0);
    }

    #[test]
    fn a_cut_never_starts_before_the_file_does() {
        // Padding must not produce a negative seek.
        let cut = plan_local_cut(&frame(), 1.0, 20.0).unwrap();
        assert_eq!(cut.start, 0.0);
    }

    #[test]
    fn a_keyless_guess_brackets_the_moments_before_the_clip_was_made() {
        // A clip made at broadcast t=600 captures roughly t=570..600.
        let f = frame();
        let cut = plan_guess_cut(&f, 1_600, 30.0).unwrap();
        assert_eq!(cut.confidence, Confidence::Approx, "always a guess");
        assert!(cut.start < 570.0, "starts before the clipped moment");
        assert!(cut.len >= 30.0);
    }

    #[test]
    fn a_live_clip_is_never_routed_through_recovery() {
        assert_eq!(choose_tier(false, true, None, 0), Tier::Live);
        assert_eq!(
            choose_tier(false, false, Some(Confidence::Exact), 999_999_999),
            Tier::Live
        );
    }

    #[test]
    fn an_exact_local_cut_beats_the_cdn_even_while_the_vod_lives() {
        // Free, instant, permanent, and higher quality than the clip ever was.
        assert_eq!(
            choose_tier(true, true, Some(Confidence::Exact), 0),
            Tier::LocalCut
        );
    }

    #[test]
    fn the_cdn_wins_when_the_local_cut_would_only_be_approximate() {
        // Frame-exactness beats free while the VOD is still there.
        assert_eq!(
            choose_tier(true, true, Some(Confidence::Approx), 0),
            Tier::CdnVod
        );
        // …but once the VOD ages out, the approximate local cut is all there is.
        assert_eq!(
            choose_tier(true, true, Some(Confidence::Approx), CDN_WINDOW_SECS + 1),
            Tier::LocalCut
        );
    }

    #[test]
    fn without_keys_or_a_local_file_only_the_legacy_object_is_left() {
        assert_eq!(choose_tier(true, false, None, 0), Tier::CdnClip);
    }

    #[test]
    fn without_keys_but_with_a_local_recording_we_guess() {
        assert_eq!(
            choose_tier(true, false, Some(Confidence::Approx), 0),
            Tier::LocalGuess
        );
    }

    #[test]
    fn the_legacy_url_encodes_the_pipe_twitch_uses() {
        // `AT-cm|<slug>.mp4` — the bar must be percent-encoded or the request
        // is malformed.
        let u = legacy_clip_url("Abc-123");
        assert!(u.contains("AT-cm%7CAbc-123.mp4"), "{u}");
        assert!(!u.contains('|'));
    }
}
