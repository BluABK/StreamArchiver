//! Recurrence math for [`crate::models::ScheduledRecording`] — pure
//! functions only. The tick loop that actually fires a due rule (calling
//! `Supervisor::try_begin`/`manual_stop`) lives on `Supervisor` itself in
//! `downloader.rs` (`scheduled_recordings_loop`), since firing needs private
//! Supervisor state; this module has no knowledge of recordings, only time.

use chrono::{Datelike, Local, NaiveDate, NaiveTime, TimeZone};

use crate::models::{RecurrenceKind, ScheduledRecording, dow_bit};

/// How many days ahead a `Weekly` scan looks for the next matching weekday
/// before giving up. One week is always enough to either find a match or
/// prove `days_of_week` is empty.
const WEEKLY_SCAN_DAYS: i64 = 8;

/// The next unix timestamp `rule` fires strictly after `after`, or `None` if
/// it never will again (a `Once` rule already past, a `Weekly` rule with no
/// days selected, or one whose next hit would be past its `until`).
pub fn compute_next_run(rule: &ScheduledRecording, after: i64) -> Option<i64> {
    match rule.kind {
        RecurrenceKind::Once => rule.start_at.filter(|&t| t > after),
        RecurrenceKind::Weekly => {
            let days = rule.days_of_week.unwrap_or(0);
            if days == 0 {
                return None;
            }
            let tod = rule.time_of_day_secs.unwrap_or(0).clamp(0, 86_399) as u32;
            let after_local = Local.timestamp_opt(after, 0).single()?;
            let start_date = after_local.date_naive();
            for add in 0..WEEKLY_SCAN_DAYS {
                let date = start_date + chrono::Duration::days(add);
                if days & dow_bit(date.weekday()) == 0 {
                    continue;
                }
                let Some(ts) = local_timestamp(date, tod) else {
                    continue; // DST spring-forward gap at this exact instant
                };
                if ts <= after {
                    continue;
                }
                if rule.until.is_some_and(|until| ts > until) {
                    return None;
                }
                return Some(ts);
            }
            None
        }
    }
}

/// Every occurrence of `rule` in `(range_start, range_end]`, for the calendar
/// month-grid badge. Bounded — a `Weekly` rule can't produce more than one hit
/// per selected weekday per week, so this can't loop unboundedly for any real
/// rule.
pub fn occurrences_in_range(rule: &ScheduledRecording, range_start: i64, range_end: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let mut after = range_start;
    // A rule with no `until` and every weekday selected could in principle
    // yield one hit per day forever; cap iterations generously so a
    // misconfigured rule can't hang the calendar render.
    for _ in 0..366 {
        match compute_next_run(rule, after) {
            Some(ts) if ts <= range_end => {
                out.push(ts);
                after = ts;
            }
            _ => break,
        }
    }
    out
}

fn local_timestamp(date: NaiveDate, secs_from_midnight: u32) -> Option<i64> {
    let time = NaiveTime::from_num_seconds_from_midnight_opt(secs_from_midnight, 0)?;
    let naive = date.and_time(time);
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        chrono::LocalResult::Ambiguous(dt, _) => Some(dt.timestamp()),
        chrono::LocalResult::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DOW_FRI, DOW_MON, DOW_SAT, DOW_SUN, DOW_THU, DOW_TUE, DOW_WED};

    fn rule(
        kind: RecurrenceKind,
        start_at: Option<i64>,
        days: Option<i64>,
        tod: Option<i64>,
        until: Option<i64>,
    ) -> ScheduledRecording {
        ScheduledRecording {
            id: 1,
            monitor_id: 1,
            label: String::new(),
            kind,
            start_at,
            days_of_week: days,
            time_of_day_secs: tod,
            until,
            duration_secs: None,
            enabled: true,
            next_run_at: None,
            last_fired_at: None,
            pending_stop_at: None,
            created_at: 0,
        }
    }

    fn local_ts(date: NaiveDate, secs_from_midnight: i64) -> i64 {
        local_timestamp(date, secs_from_midnight as u32).unwrap()
    }

    #[test]
    fn once_fires_only_strictly_in_the_future() {
        let r = rule(RecurrenceKind::Once, Some(1_000_000), None, None, None);
        assert_eq!(compute_next_run(&r, 999_999), Some(1_000_000));
        assert_eq!(compute_next_run(&r, 1_000_000), None, "not due again at its own instant");
        assert_eq!(compute_next_run(&r, 1_000_001), None, "already past");
    }

    #[test]
    fn once_with_no_start_at_never_fires() {
        let r = rule(RecurrenceKind::Once, None, None, None, None);
        assert_eq!(compute_next_run(&r, 0), None);
    }

    #[test]
    fn weekly_hits_every_day_of_week_bit() {
        // 2026-07-06 is a Monday; anchor `after` at that local midnight so
        // each single-day bitmask's next hit is deterministic days-ahead.
        let anchor_date = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        let after = local_ts(anchor_date, 0);
        for (bit, add_days) in [
            (DOW_MON, 0),
            (DOW_TUE, 1),
            (DOW_WED, 2),
            (DOW_THU, 3),
            (DOW_FRI, 4),
            (DOW_SAT, 5),
            (DOW_SUN, 6),
        ] {
            let r = rule(RecurrenceKind::Weekly, None, Some(bit), Some(3600), None);
            let got = compute_next_run(&r, after);
            let expected = local_ts(anchor_date + chrono::Duration::days(add_days), 3600);
            assert_eq!(got, Some(expected), "bit {bit} add_days {add_days}");
        }
    }

    #[test]
    fn weekly_multi_day_picks_the_soonest() {
        // Wednesday noon; Mon/Wed this week have passed, so Mon|Wed|Fri at
        // 08:00 should land on Friday.
        let wed_noon = NaiveDate::from_ymd_opt(2026, 7, 8).unwrap();
        let after = local_ts(wed_noon, 12 * 3600);
        let r = rule(
            RecurrenceKind::Weekly,
            None,
            Some(DOW_MON | DOW_WED | DOW_FRI),
            Some(8 * 3600),
            None,
        );
        let got = compute_next_run(&r, after);
        let expected = local_ts(wed_noon + chrono::Duration::days(2), 8 * 3600);
        assert_eq!(got, Some(expected));
    }

    #[test]
    fn weekly_no_days_selected_never_fires() {
        let r = rule(RecurrenceKind::Weekly, None, Some(0), Some(0), None);
        assert_eq!(compute_next_run(&r, 0), None);
        let r_none = rule(RecurrenceKind::Weekly, None, None, Some(0), None);
        assert_eq!(compute_next_run(&r_none, 0), None);
    }

    #[test]
    fn weekly_respects_until_boundary() {
        let anchor_date = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(); // Monday
        let after = local_ts(anchor_date, 0);
        let this_monday_1am = local_ts(anchor_date, 3600);

        let past_until = rule(
            RecurrenceKind::Weekly,
            None,
            Some(DOW_MON),
            Some(3600),
            Some(this_monday_1am - 1),
        );
        assert_eq!(compute_next_run(&past_until, after), None, "until before the hit");

        let exact_until = rule(
            RecurrenceKind::Weekly,
            None,
            Some(DOW_MON),
            Some(3600),
            Some(this_monday_1am),
        );
        assert_eq!(compute_next_run(&exact_until, after), Some(this_monday_1am));
    }

    #[test]
    fn occurrences_in_range_collects_multiple_weeks() {
        let anchor_date = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(); // Monday
        let range_start = local_ts(anchor_date, 0) - 1;
        let range_end = local_ts(anchor_date + chrono::Duration::days(21), 0);
        let r = rule(RecurrenceKind::Weekly, None, Some(DOW_MON), Some(0), None);
        let hits = occurrences_in_range(&r, range_start, range_end);
        assert_eq!(
            hits,
            vec![
                local_ts(anchor_date, 0),
                local_ts(anchor_date + chrono::Duration::days(7), 0),
                local_ts(anchor_date + chrono::Duration::days(14), 0),
                local_ts(anchor_date + chrono::Duration::days(21), 0),
            ]
        );
    }

    #[test]
    fn occurrences_in_range_empty_when_out_of_bounds() {
        let r = rule(RecurrenceKind::Once, Some(1_000_000), None, None, None);
        assert_eq!(occurrences_in_range(&r, 0, 999_999), Vec::<i64>::new());
        assert_eq!(occurrences_in_range(&r, 1_000_000, 2_000_000), Vec::<i64>::new(), "not strictly after range_start");
    }
}
