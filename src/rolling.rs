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
    fn remaining_reads_as_due_when_past() {
        assert_eq!(fmt_remaining(0), "due");
        assert_eq!(fmt_remaining(-90), "due");
        assert_eq!(fmt_remaining(45), "45s");
        assert_eq!(fmt_remaining(90 * 60), "1h 30m");
        assert_eq!(fmt_remaining(6 * 86_400 + 4 * 3600), "6d 4h");
    }
}
