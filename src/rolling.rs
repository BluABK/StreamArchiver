//! Rolling recordings: captures made while an instance is in *rolling mode*
//! carry a TTL, and their file is disposed of once it elapses unless the user
//! pressed **Keep**.
//!
//! The per-take bookkeeping lives on the recording row itself
//! ([`crate::models::Rolling`], schema v88); the three-level on/off + TTL
//! settings ride along on [`crate::disposal::DisposalScope`], since "what
//! eventually happens to this take's file" is exactly what that scope already
//! answers. The TTL is resolved once at capture start
//! ([`crate::disposal::effective_rolling`]) and **frozen onto the take** the
//! same way `trigger_rule_json` is: turning the setting on never puts
//! already-recorded takes at risk, and turning it off never silently rescues
//! takes already counting down (those can still be Kept individually).
//!
//! Expiry disposes of the media and clears `output_path`, keeping the history
//! row — byte-for-byte what the manual "Delete file from disk" action does
//! (`crate::manual_delete`), so the take's title, stats, chat log, chapters and
//! notes all survive and only the video is gone.

use tracing::{info, warn};

use crate::events::{AppEvent, EventTx};
use crate::store::Store;

/// `app_settings` key holding the unix time of the last expiry sweep, so a
/// restart-heavy session doesn't re-scan on every scheduler tick.
const K_LAST_SWEEP: &str = "rolling_last_sweep";
/// Minimum gap between expiry sweeps. A rolling TTL is measured in hours, so
/// a minute of slack either way is irrelevant — this exists only to keep the
/// query off the every-tick path.
const SWEEP_INTERVAL_SECS: i64 = 60;

/// Dispose of every rolling take whose TTL has elapsed without being kept.
///
/// Self-throttled to [`SWEEP_INTERVAL_SECS`] — call it from the scheduler tick
/// alongside the log-retention and DB-backup sweeps and let it decide.
///
/// Each expiry is the same two steps the manual "Delete file from disk" action
/// performs (`ui::dialogs::spawn_manual_delete_file`): dispose of the media by
/// the configured method, then clear `output_path`. The history row is left
/// entirely alone, so the take keeps its title, stats, chat log, chapters and
/// notes and only loses the video.
///
/// A disposal failure is logged and **not** stamped as expired, so the next
/// sweep retries — matching this codebase's "disposal failures never escalate"
/// rule. A take whose file has already vanished is stamped without a disposal
/// attempt.
pub async fn maybe_sweep_rolling(store: &Store, events: &EventTx, now: i64) {
    let last = store
        .get_setting(K_LAST_SWEEP)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now - last < SWEEP_INTERVAL_SECS {
        return;
    }
    let _ = store.set_setting(K_LAST_SWEEP, &now.to_string());

    let due = match store.expired_rolling_recordings(now) {
        Ok(v) if v.is_empty() => return,
        Ok(v) => v,
        Err(e) => {
            warn!("rolling: expiry query failed: {e:#}");
            return;
        }
    };
    info!(count = due.len(), "rolling: disposing of expired recordings");
    for r in due {
        let path = std::path::PathBuf::from(&r.output_path);
        // Already gone (moved/deleted outside the app): nothing to dispose of,
        // but the take is still expired — stamp it so it stops being queried.
        if crate::iomon::fs::metadata(crate::iomon::Cat::CacheSweep, &path).await.is_err() {
            let _ = store.mark_rolling_expired(r.rec_id, now);
            let _ = store.update_recording_output_path(r.rec_id, "");
            let _ = events.send(AppEvent::RecordingUpdated { recording_id: r.rec_id });
            continue;
        }
        match crate::disposal::dispose_media(
            store,
            r.channel_id,
            r.monitor_id,
            &path,
            r.rec_id,
            "rolling recording expired",
        )
        .await
        {
            Ok(d) => {
                let _ = store.update_recording_output_path(r.rec_id, "");
                let _ = store.mark_rolling_expired(r.rec_id, now);
                let _ = events.send(AppEvent::RecordingUpdated { recording_id: r.rec_id });
                info!(rec_id = r.rec_id, path = %r.output_path, "rolling: {}", d.describe());
            }
            Err(e) => {
                // Left un-stamped on purpose: the next sweep tries again.
                warn!(rec_id = r.rec_id, path = %r.output_path, "rolling: disposal failed: {e}");
            }
        }
    }
}

/// Render a stored TTL (seconds) for the hours-based text field the
/// channel/instance forms use. `None` (inherit) is an empty field.
pub fn secs_to_hours_field(secs: Option<i64>) -> String {
    match secs {
        Some(s) if s > 0 => {
            let hours = s as f64 / 3600.0;
            // Whole hours are by far the common case (and what the field
            // writes back); only show a fraction when the stored value really
            // isn't on an hour boundary — e.g. a short TTL set for testing.
            if (hours - hours.round()).abs() < 1e-9 {
                format!("{}", hours.round() as i64)
            } else {
                format!("{hours:.4}").trim_end_matches('0').trim_end_matches('.').to_string()
            }
        }
        _ => String::new(),
    }
}

/// Parse the hours-based text field back to seconds. Empty/garbage/non-positive
/// is `None` = "inherit the level above", which is also what the field shows
/// for it — so a typo can never be read as "expire immediately".
pub fn hours_field_to_secs(s: &str) -> Option<i64> {
    let hours: f64 = s.trim().parse().ok()?;
    let secs = (hours * 3600.0).round() as i64;
    (secs > 0).then_some(secs)
}

/// What the rolling takes under one row (an instance, or a channel summing its
/// instances) add up to: how many are counting down, and when the *first* of
/// them is due.
///
/// The soonest deadline is what a collapsed row has to show — "37 rolling
/// recordings" says nothing about whether the nearest one goes tonight or next
/// week, which is the only part that's actionable. `ttl_secs` is the TTL of
/// that same soonest take, so the countdown can be coloured by how much of its
/// life is left rather than by an absolute threshold — "1 day left" is most of
/// a 30 h window still to run and the last scrap of a 30 d one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RollingRollup {
    /// Takes still counting down (neither kept nor expired). Includes takes
    /// still recording, which have no deadline yet.
    pub count: i64,
    /// Unix time the first of them is disposed of, or `None` when every
    /// counting take is still recording (the clock starts at `ended_at`).
    pub soonest: Option<i64>,
    /// TTL of the take that owns `soonest`; `0` when there is none.
    pub ttl_secs: i64,
}

impl RollingRollup {
    /// Fold another row's rollup into this one — the channel row summing its
    /// instances, or a period row summing its broadcasts.
    pub fn merge(&mut self, other: &RollingRollup) {
        self.count += other.count;
        match (self.soonest, other.soonest) {
            (_, None) => {}
            (None, Some(_)) => {
                self.soonest = other.soonest;
                self.ttl_secs = other.ttl_secs;
            }
            (Some(mine), Some(theirs)) if theirs < mine => {
                self.soonest = other.soonest;
                self.ttl_secs = other.ttl_secs;
            }
            _ => {}
        }
    }

    /// Seconds left before the soonest deadline, or `None` when there isn't
    /// one yet (nothing counting, or every take still recording).
    pub fn remaining(&self, now: i64) -> Option<i64> {
        self.soonest.map(|d| d - now)
    }
}

/// How much of a rolling take's life is left, `1.0` = the full TTL still to
/// run, `0.0` = due now. Drives the countdown's colour ramp, so it's expressed
/// as a fraction of *that take's own TTL* rather than as an absolute number of
/// hours — see [`RollingRollup::ttl_secs`].
///
/// An unknown/nonsense TTL reads as `1.0` (calm) rather than `0.0`: a missing
/// denominator is our own bookkeeping gap, not evidence the file is about to
/// go.
pub fn remaining_frac(remaining: i64, ttl_secs: i64) -> f32 {
    if ttl_secs <= 0 {
        return 1.0;
    }
    (remaining as f32 / ttl_secs as f32).clamp(0.0, 1.0)
}

/// Human countdown for a rolling take's remaining life, e.g. `"6d 4h"`,
/// `"3h 12m"`, `"45m"`. Negative (already due, sweep hasn't run yet) reads as
/// `"due"` rather than a negative duration.
pub fn fmt_remaining(secs: i64) -> String {
    if secs <= 0 {
        return "due".to_string();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Rolling, RollingState};

    #[test]
    fn hours_field_roundtrips_and_rejects_nonsense() {
        assert_eq!(secs_to_hours_field(Some(7 * 24 * 3600)), "168");
        assert_eq!(secs_to_hours_field(Some(3600)), "1");
        assert_eq!(secs_to_hours_field(None), "");
        assert_eq!(secs_to_hours_field(Some(0)), "");
        // A sub-hour TTL (testing) survives the round trip rather than
        // rendering as "0" and being read back as inherit.
        assert_eq!(secs_to_hours_field(Some(900)), "0.25");
        assert_eq!(hours_field_to_secs("0.25"), Some(900));

        assert_eq!(hours_field_to_secs("168"), Some(7 * 24 * 3600));
        assert_eq!(hours_field_to_secs("  24 "), Some(86_400));
        // Anything unusable reads as inherit, never as "expire immediately".
        assert_eq!(hours_field_to_secs(""), None);
        assert_eq!(hours_field_to_secs("soon"), None);
        assert_eq!(hours_field_to_secs("0"), None);
        assert_eq!(hours_field_to_secs("-5"), None);
    }

    #[test]
    fn rolling_state_derivation() {
        // Not a rolling take at all.
        assert_eq!(Rolling::default().state(Some(100)), RollingState::None);

        let r = Rolling { ttl_secs: 60, ..Default::default() };
        // Still recording: counting down, but no deadline yet — the clock only
        // starts when the capture ends, so a long broadcast can't expire
        // mid-capture.
        assert_eq!(r.state(None), RollingState::Rolling { deadline: None });
        assert_eq!(r.deadline(None), None);
        assert_eq!(r.state(Some(1_000)), RollingState::Rolling { deadline: Some(1_060) });

        // Kept and expired both win over the countdown, expired first (a take
        // can't be un-deleted by keeping it afterwards).
        let kept = Rolling { ttl_secs: 60, kept_at: 5, ..Default::default() };
        assert_eq!(kept.state(Some(1_000)), RollingState::Kept { at: 5 });
        let both = Rolling { ttl_secs: 60, kept_at: 5, expired_at: 9, ..Default::default() };
        assert_eq!(both.state(Some(1_000)), RollingState::Expired { at: 9 });
    }

    #[test]
    fn unkeep_restarts_the_clock_rather_than_resuming_it() {
        // A take that ended long ago and is un-kept now must get a FULL fresh
        // TTL, not be immediately due — otherwise "Unkeep" reads like an undo
        // but deletes the file on the next sweep.
        let now = 1_000_000;
        let ended = 1;
        let unkept = Rolling { ttl_secs: 3600, from: now, ..Default::default() };
        assert_eq!(unkept.deadline(Some(ended)), Some(now + 3600));
        // Without the restart it would have been due since second 3601.
        let naive = Rolling { ttl_secs: 3600, ..Default::default() };
        assert_eq!(naive.deadline(Some(ended)), Some(3601));
    }

    #[test]
    fn rollup_merge_keeps_the_soonest_deadline_and_its_ttl() {
        let mut a = RollingRollup { count: 2, soonest: Some(500), ttl_secs: 3600 };
        // A later deadline adds to the count but must not become the headline.
        a.merge(&RollingRollup { count: 3, soonest: Some(900), ttl_secs: 7200 });
        assert_eq!(a, RollingRollup { count: 5, soonest: Some(500), ttl_secs: 3600 });
        // An earlier one takes over, bringing its OWN ttl (the colour ramp
        // divides by it, so the pair must never come from different takes).
        a.merge(&RollingRollup { count: 1, soonest: Some(100), ttl_secs: 60 });
        assert_eq!(a, RollingRollup { count: 6, soonest: Some(100), ttl_secs: 60 });
        // Still-recording takes count but never clear an existing deadline.
        a.merge(&RollingRollup { count: 4, soonest: None, ttl_secs: 0 });
        assert_eq!(a, RollingRollup { count: 10, soonest: Some(100), ttl_secs: 60 });
        // ...and are adopted wholesale by an empty rollup.
        let mut empty = RollingRollup::default();
        empty.merge(&RollingRollup { count: 1, soonest: None, ttl_secs: 0 });
        assert_eq!(empty.count, 1);
        assert_eq!(empty.soonest, None);
        empty.merge(&RollingRollup { count: 1, soonest: Some(7), ttl_secs: 9 });
        assert_eq!(empty, RollingRollup { count: 2, soonest: Some(7), ttl_secs: 9 });
    }

    #[test]
    fn remaining_frac_is_relative_to_the_takes_own_ttl() {
        assert_eq!(remaining_frac(3600, 3600), 1.0);
        assert_eq!(remaining_frac(1800, 3600), 0.5);
        assert_eq!(remaining_frac(0, 3600), 0.0);
        // Overdue clamps to 0, not a negative fraction.
        assert_eq!(remaining_frac(-9000, 3600), 0.0);
        // A longer TTL leaves the same absolute time looking calmer, which is
        // the whole point of the ratio.
        assert!(remaining_frac(86_400, 30 * 86_400) < remaining_frac(86_400, 2 * 86_400));
        // No TTL to divide by reads as calm, never as "due now".
        assert_eq!(remaining_frac(10, 0), 1.0);
    }

    #[test]
    fn remaining_reads_as_due_when_past() {
        assert_eq!(fmt_remaining(0), "due");
        assert_eq!(fmt_remaining(-90), "due");
        assert_eq!(fmt_remaining(45), "45s");
        assert_eq!(fmt_remaining(90 * 60), "1h 30m");
        assert_eq!(fmt_remaining(6 * 86_400 + 4 * 3600), "6d 4h");
    }
}
