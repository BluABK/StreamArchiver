//! Downloading clip media.
//!
//! Clips reuse the `video` row pipeline as a **disposable job ticket**: insert
//! one, let the existing supervisor run it (restart survival, the concurrency
//! semaphore, `--limit-rate`, progress maps, Stop/Retry and the stall watchdog
//! all come free), then copy the result onto the `clip` row and delete the
//! ticket. `delete_video` removes only the row, never the file, so the Videos
//! tab only ever shows the handful of clip jobs actually in flight rather than
//! tens of thousands of finished ones.
//!
//! No signed-URL handling lives here. yt-dlp's `twitch:clips` extractor performs
//! the `VideoAccessToken_Clip` handshake itself, so token expiry is its problem,
//! not ours — verified against a live clip before this was written.

use super::*;
use crate::events::ManualCommand;
use crate::models::now_unix;
use tokio::sync::mpsc;
use tracing::warn;

/// Concurrent clip downloads. Deliberately small and separate from the global
/// download semaphore: a ten-thousand-clip backlog must never delay a live
/// capture starting.
const DEFAULT_CONCURRENCY: i64 = 2;
/// Give up on a clip after this many failed attempts and leave it for a manual
/// retry, rather than re-queueing it forever.
const MAX_ATTEMPTS: i64 = 3;

/// Global download switch. Off by default — see [`download_allowed`].
pub fn download_master_on(store: &Store) -> bool {
    matches!(
        store.get_setting(K_CLIPS_DOWNLOAD).ok().flatten().as_deref(),
        Some("1") | Some("true")
    )
}

/// Per-channel opt-in.
pub fn channel_download_on(store: &Store, channel_id: i64) -> bool {
    crate::raid_follow::load_bool_scope(store, K_CHANNEL_CLIPS_DOWNLOAD, channel_id)
        .unwrap_or(false)
}

/// Whether clip media may be downloaded for a channel.
///
/// Two independent switches ANDed, **not** an inherit chain: there is no
/// sensible global default underneath a decision measured at ~200 GB per active
/// channel, so both must be turned on deliberately. Each half has its own reader
/// so the disabled-hover text can name the one that is missing — the same shape
/// as `manual_delete`'s deletion gates.
pub fn download_allowed(store: &Store, channel_id: i64) -> bool {
    download_master_on(store) && channel_download_on(store, channel_id)
}

/// How many clip downloads may be in flight at once.
pub fn max_concurrency(store: &Store) -> i64 {
    store
        .get_setting("clips_max_concurrent")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_CONCURRENCY)
        .min(8)
}

/// Build the output stem for a clip.
///
/// The clip id is appended deliberately: once the clip is deleted upstream the
/// filename is the only surviving provenance, and a title alone is neither
/// unique nor stable. `build_video_plan` applies `unique_stem` and the
/// sacrificial truncation on top, so long emoji-laden titles are already handled.
pub fn clip_stem(c: &Clip) -> String {
    let date = chrono::DateTime::from_timestamp(c.created_at, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown-date".into());
    let who = if c.broadcaster_login.is_empty() {
        "unknown".to_string()
    } else {
        c.broadcaster_login.clone()
    };
    let title = crate::downloader::sanitize_filename(&c.title);
    let title = title.trim();
    if title.is_empty() {
        format!("{date}_{who}_{}", c.slug)
    } else {
        format!("{date}_{who}_{title}_{}", c.slug)
    }
}

/// Queue one clip for download. Returns false when it could not be enqueued.
pub fn enqueue_clip_download(
    store: &Store,
    manual_tx: &mpsc::UnboundedSender<ManualCommand>,
    clip_id: i64,
) -> bool {
    let Ok(Some(c)) = store.get_clip(clip_id) else {
        return false;
    };
    if c.url.is_empty() {
        return false;
    }
    let Some(channel_id) = c.channel_id else {
        // A clip of a channel we don't monitor has no output directory to
        // belong to; it stays catalogued but undownloaded.
        return false;
    };
    let Ok(Some(mw)) = c
        .monitor_id
        .map(|m| store.get_monitor_with_channel(m))
        .transpose()
        .map(Option::flatten)
    else {
        return false;
    };
    let dir = clips_dir(&mw);
    let m = &mw.monitor;
    let video = crate::models::Video {
        id: 0,
        url: c.url.clone(),
        title: format!("Clip · {} · {}", mw.channel.name, c.title),
        channel: mw.channel.name.clone(),
        platform: c.platform,
        tool: crate::models::Tool::YtDlp,
        tool_binary: String::new(),
        quality: "best".into(),
        output_dir: dir,
        filename_template: clip_stem(&c),
        auth_kind: m.auth_kind,
        auth_value: m.auth_value.clone(),
        audio_tracks: String::new(),
        subtitle_tracks: String::new(),
        chat_log: false,
        extra_args: String::new(),
        auto_title: false,
        status: "queued".into(),
        output_path: String::new(),
        bytes: 0,
        created_at: now_unix(),
        exit_code: None,
        log_excerpt: String::new(),
        started_at: None,
        ended_at: None,
    };
    match store.insert_video(&video) {
        Ok(id) => {
            let _ = store.set_clip_download(clip_id, "downloading", Some(id));
            let _ = manual_tx.send(ManualCommand::StartVideo(id));
            let _ = channel_id; // gate already checked by the drainer
            true
        }
        Err(e) => {
            warn!(clip_id, "clips: insert_video failed: {e:#}");
            false
        }
    }
}

/// Where a channel's clips are written: a `clips` subfolder beside its
/// recordings, so they travel with the channel when its output dir moves.
fn clips_dir(mw: &crate::models::MonitorWithChannel) -> String {
    let base = mw.monitor.output_dir.trim();
    if base.is_empty() {
        return crate::app_paths::default_output_dir()
            .join("clips")
            .to_string_lossy()
            .into_owned();
    }
    std::path::Path::new(base)
        .join("clips")
        .to_string_lossy()
        .into_owned()
}

/// Top the in-flight clip downloads up to the configured concurrency.
///
/// Called once per sweep pass rather than enqueueing everything at once: a
/// channel can hold ten thousand pending clips and firing that many
/// `StartVideo` commands would swamp the supervisor's queue and the Videos tab
/// alike.
pub fn drain_clip_queue(store: &Store, manual_tx: &mpsc::UnboundedSender<ManualCommand>) -> usize {
    if !download_master_on(store) {
        return 0;
    }
    let cap = max_concurrency(store);
    let active = store.active_clip_download_count().unwrap_or(0);
    let slots = (cap - active).max(0);
    if slots == 0 {
        return 0;
    }
    // Over-fetch: most candidates will be filtered out by the per-channel gate,
    // and re-querying per rejection would be a query per clip.
    let candidates = store
        .pending_clip_downloads(slots * 20)
        .unwrap_or_default();
    let mut started = 0usize;
    for c in candidates {
        if started as i64 >= slots {
            break;
        }
        let Some(cid) = c.channel_id else { continue };
        if !channel_download_on(store, cid) {
            continue;
        }
        if c.dl_attempts >= MAX_ATTEMPTS {
            continue;
        }
        if enqueue_clip_download(store, manual_tx, c.id) {
            started += 1;
        }
    }
    started
}

/// Delete a clip's media, keeping the catalogue row.
///
/// Routes through [`crate::disposal::dispose_media`] so the file lands in Trash
/// (or the Recycle Bin, or is permanently removed) according to the same
/// resolved policy every other archived artifact obeys — the Videos tab
/// notably does *not* do this and simply orphans the file, which is the
/// mistake this avoids repeating.
///
/// The row survives with its recovery keys intact: that is the same
/// "row outlives the file" split rolling recordings use, and it is what lets the
/// clip be re-fetched while it still exists upstream.
pub async fn dispose_clip_media(store: &Store, clip_id: i64) -> bool {
    let Ok(Some(c)) = store.get_clip(clip_id) else {
        return false;
    };
    if c.output_path.is_empty() {
        return false;
    }
    let disposed = crate::disposal::dispose_media(
        store,
        c.channel_id.unwrap_or(0),
        c.monitor_id.unwrap_or(0),
        std::path::Path::new(&c.output_path),
        // `disposal_record` keys on a recording id; a clip linked to a local
        // take borrows its take's id so the Trash view can name the channel,
        // and an unlinked one records 0 (rendering channel-less there).
        c.recording_id.unwrap_or(0),
        "clip",
    )
    .await
    .is_ok();
    if disposed {
        let _ = store.clear_clip_output(clip_id);
    }
    disposed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Platform;

    fn clip() -> Clip {
        Clip {
            id: 7,
            platform: Platform::Twitch,
            slug: "GorgeousTasty-TupWG".into(),
            broadcaster_login: "laynalazar".into(),
            title: "You'll Never Be Able To Unsee It".into(),
            created_at: 1_786_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn a_clip_stem_carries_its_id_so_the_file_stays_identifiable() {
        // Once the clip is deleted upstream the filename is the only remaining
        // provenance, and a title is neither unique nor stable.
        let s = clip_stem(&clip());
        assert!(s.ends_with("GorgeousTasty-TupWG"), "{s}");
        assert!(s.contains("laynalazar"), "{s}");
        assert!(s.starts_with("2026-"), "{s}");
    }

    #[test]
    fn a_hostile_title_cannot_escape_the_stem() {
        let mut c = clip();
        c.title = r#"..\..\evil: "quoted" /slashes/ *stars*"#.into();
        let s = clip_stem(&c);
        for bad in ['\\', '/', ':', '"', '*'] {
            assert!(!s.contains(bad), "{bad:?} survived into {s}");
        }
        assert!(s.ends_with("GorgeousTasty-TupWG"));
    }

    #[test]
    fn a_titleless_clip_still_produces_a_usable_stem() {
        let mut c = clip();
        c.title = "   ".into();
        let s = clip_stem(&c);
        assert_eq!(s, "2026-08-06_laynalazar_GorgeousTasty-TupWG");
    }

    #[test]
    fn an_unknown_broadcaster_does_not_leave_a_dangling_separator() {
        let mut c = clip();
        c.broadcaster_login = String::new();
        assert!(clip_stem(&c).contains("_unknown_"));
    }

    #[test]
    fn both_download_gates_must_be_on_and_neither_defaults_to_yes() {
        // Independent AND-ed switches, not an inherit chain — there is no
        // sensible global default under a ~200 GB-per-channel decision.
        let store = Store::open_in_memory().unwrap();
        assert!(!download_master_on(&store), "master defaults off");
        assert!(!channel_download_on(&store, 1), "per-channel defaults off");
        assert!(!download_allowed(&store, 1));

        store.set_setting(K_CLIPS_DOWNLOAD, "1").unwrap();
        assert!(download_master_on(&store));
        assert!(!download_allowed(&store, 1), "channel gate still shut");

        crate::raid_follow::save_bool_scope(&store, K_CHANNEL_CLIPS_DOWNLOAD, 1, Some(true))
            .unwrap();
        assert!(download_allowed(&store, 1));
        // Turning the master back off closes it again, whatever the channel says.
        store.set_setting(K_CLIPS_DOWNLOAD, "0").unwrap();
        assert!(!download_allowed(&store, 1));
    }

    #[test]
    fn concurrency_is_bounded_even_when_the_setting_is_absurd() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(max_concurrency(&store), DEFAULT_CONCURRENCY);
        store.set_setting("clips_max_concurrent", "500").unwrap();
        assert_eq!(max_concurrency(&store), 8, "clamped so clips can't starve captures");
        store.set_setting("clips_max_concurrent", "0").unwrap();
        assert_eq!(max_concurrency(&store), DEFAULT_CONCURRENCY);
        store.set_setting("clips_max_concurrent", "nonsense").unwrap();
        assert_eq!(max_concurrency(&store), DEFAULT_CONCURRENCY);
    }
}
