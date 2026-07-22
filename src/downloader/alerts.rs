//! Capture-tool log scanning: classify streamlink/yt-dlp log lines into
//! persisted alerts (the 🚨 Warnings window) and derive the lost time ranges
//! the Twitch gap-recovery job re-fetches from the VOD CDN.
//!
//! Everything here is pure string/number work so the pattern table and the
//! range math are unit-testable against real log lines. The I/O half (tail
//! reads on the watchdog cadence, alert upserts, job spawning) lives in
//! `process.rs`/`gap_recover.rs`.

/// Twitch live/VOD segments are a fixed 2.0 s, and live media sequence 0 is
/// the broadcast start — so `position × 2` IS the VOD offset of a lost
/// segment. (Other platforms vary; gap recovery is Twitch-only.)
pub(super) const TWITCH_SEG_SECS: f64 = 2.0;
/// Safety padding on each side of a lost range (sequence→time mapping is
/// exact in theory, but a couple of segments of slack costs nothing).
pub(super) const GAP_PAD_SECS: f64 = 10.0;
/// Ranges closer than this merge into one fetch (one playlist + one mux beats
/// dozens of 16-second jobs when a bad patch produces clustered gaps).
pub(super) const GAP_COALESCE_SECS: f64 = 30.0;

/// What one matched log line means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LineHit {
    /// `sequence_gap` | `fetch_failed` | `tool_error` | `tool_warning`.
    pub kind: &'static str,
    /// `error` | `warning`.
    pub severity: &'static str,
    /// Lost live segments `(first_sequence, count)` — data-loss kinds only.
    pub lost: Option<(u64, u64)>,
}

/// Classify one tool-log line. `None` = not alert-worthy (the overwhelmingly
/// common case — INFO/progress/debug output).
pub(super) fn classify_line(line: &str) -> Option<LineHit> {
    let l = line.trim();
    if l.is_empty() {
        return None;
    }

    // streamlink `[stream.segmented][warning] Sequence gap of N segment(s) at
    // position P.` — the playlist window slid past segments that were never
    // downloaded. P..P+N-1 are the missing sequence numbers (verified against
    // streamlink's segmented.py: `size = segment.num - self.sequence`).
    if let Some(rest) = l.split("Sequence gap of ").nth(1)
        && let Some(n) = lead_u64(rest)
        && let Some(rest) = rest.split(" at position ").nth(1)
        && let Some(p) = lead_u64(rest)
    {
        return Some(LineHit { kind: "sequence_gap", severity: "error", lost: Some((p, n)) });
    }

    // streamlink `[stream.hls][error] Failed to fetch (map for) segment N: …`
    // — retries exhausted, the segment was skipped in the output. Same data
    // loss as a sequence gap, one segment at a time.
    for pat in ["Failed to fetch segment ", "Failed to fetch map for segment "] {
        if let Some(rest) = l.split(pat).nth(1)
            && let Some(p) = lead_u64(rest)
        {
            return Some(LineHit { kind: "fetch_failed", severity: "error", lost: Some((p, 1)) });
        }
    }

    // yt-dlp `[download] Skipping fragment N …` — a live fragment given up
    // on; data loss on the YouTube side (no auto-recovery — SABR/DVR rails
    // handle their own retries, this just surfaces that content was lost).
    if let Some(rest) = l.split("Skipping fragment ").nth(1)
        && let Some(p) = lead_u64(rest)
    {
        return Some(LineHit { kind: "sequence_gap", severity: "error", lost: Some((p, 1)) });
    }

    // yt-dlp hard errors.
    if l.starts_with("ERROR:") || l.contains("yt_dlp.utils.DownloadError") {
        return Some(LineHit { kind: "tool_error", severity: "error", lost: None });
    }

    // Remaining streamlink [error] lines and yt-dlp WARNING: lines — surfaced
    // as plain warnings, minus known-benign chatter (retries are owned by the
    // tools' own retry logic and the SABR in-flight retry; ad waits are
    // normal Twitch operation).
    let warnish = l.starts_with("WARNING:") || l.contains("][error]");
    if warnish {
        let benign = ["retry", "retrying", "waiting for", "will skip ad", "ad segment"];
        let lower = l.to_lowercase();
        if benign.iter().any(|b| lower.contains(b)) {
            return None;
        }
        return Some(LineHit { kind: "tool_warning", severity: "warning", lost: None });
    }
    None
}

/// Leading unsigned integer of `s` (digits up to the first non-digit).
fn lead_u64(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Aggregate of one scan pass over newly appended log lines.
#[derive(Clone, Debug, Default)]
pub(super) struct ScanSummary {
    /// kind → (severity, lines matched, segments lost, last matching line).
    pub hits: Vec<(&'static str, &'static str, i64, i64, String)>,
    /// Every lost segment run `(first_sequence, count)` seen this pass.
    pub gaps: Vec<(u64, u64)>,
}

/// Run the pattern table over a chunk of log text (complete lines only — the
/// caller carries partial trailing lines between passes).
pub(super) fn scan_lines(text: &str) -> ScanSummary {
    let mut out = ScanSummary::default();
    for line in text.lines() {
        let Some(hit) = classify_line(line) else { continue };
        let lost = hit.lost.map(|(_, n)| n as i64).unwrap_or(0);
        if let Some((p, n)) = hit.lost {
            out.gaps.push((p, n));
        }
        match out.hits.iter_mut().find(|(k, ..)| *k == hit.kind) {
            Some((_, _, count, lost_tot, last)) => {
                *count += 1;
                *lost_tot += lost;
                *last = line.trim().to_string();
            }
            None => out.hits.push((hit.kind, hit.severity, 1, lost, line.trim().to_string())),
        }
    }
    out
}

/// Turn lost segment runs into padded, coalesced, broadcast-offset second
/// ranges ready for `replace_pending_gap_ranges`. Input order is arbitrary;
/// output is sorted and non-overlapping.
pub(super) fn gap_ranges_secs(gaps: &[(u64, u64)]) -> Vec<(f64, f64)> {
    let mut ranges: Vec<(f64, f64)> = gaps
        .iter()
        .map(|&(p, n)| {
            let start = (p as f64 * TWITCH_SEG_SECS - GAP_PAD_SECS).max(0.0);
            let end = (p + n) as f64 * TWITCH_SEG_SECS + GAP_PAD_SECS;
            (start, end)
        })
        .collect();
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match out.last_mut() {
            Some((_, prev_end)) if start - *prev_end <= GAP_COALESCE_SECS => {
                *prev_end = prev_end.max(end);
            }
            _ => out.push((start, end)),
        }
    }
    out
}

// ---------- scanner I/O (watchdog-cadence tail scanning) ----------

use super::*;

/// Per-cycle read cap: a tool spewing megabytes of retry noise must not turn
/// the 60 s watchdog tick into a bulk file read — beyond this the scanner
/// jumps to the tail and keeps going from there.
const SCAN_MAX_CYCLE_BYTES: u64 = 1024 * 1024;
/// Adopted (re-attach) logs are scanned FROM THE START when no bigger than
/// this, so an app restart mid-take still surfaces (and recovers) losses that
/// happened before the restart. Bigger logs start at the tail.
const ADOPT_FULL_SCAN_MAX: u64 = 4 * 1024 * 1024;
/// Sentinel offset: "not decided yet" — resolved on the first cycle per the
/// `ADOPT_FULL_SCAN_MAX` rule above.
pub(super) const SCAN_OFFSET_ADOPT: u64 = u64::MAX;
/// How far the live edge must be past a lost range's end before the VOD is
/// assumed to cover it (Twitch VODs trail the live stream by a few minutes).
const GAP_VOD_LAG_SECS: i64 = 240;

/// Rolling scanner state, owned by one capture's stall watchdog.
pub(super) struct LogScan {
    /// Tool program name for the alert's Source column (`""` = infer from the
    /// matched line — the adopt path doesn't know what it re-attached to).
    source: String,
    monitor_id: Option<i64>,
    went_live_at: Option<i64>,
    offset: u64,
    /// Partial trailing line carried between cycles.
    partial: String,
    /// Resolved once, on the first matching line: (display label, is a Twitch
    /// monitor — the gap-recovery gate).
    labels: Option<(String, bool)>,
    /// Every lost-segment run seen over the take's lifetime (the coalesced
    /// range set is re-derived from the full list each flush).
    all_gaps: Vec<(u64, u64)>,
}

impl LogScan {
    pub(super) fn new(
        source: String,
        monitor_id: Option<i64>,
        went_live_at: Option<i64>,
        offset: u64,
    ) -> Self {
        LogScan { source, monitor_id, went_live_at, offset, partial: String::new(), labels: None, all_gaps: Vec::new() }
    }
}

/// Best-effort tool guess from a matched line's shape, for the Source column
/// when the spawn-time program name isn't known (adopted takes).
fn infer_source(line: &str) -> &'static str {
    if line.starts_with('[') && line.contains("][") {
        "streamlink"
    } else {
        "yt-dlp"
    }
}

impl Supervisor {
    /// One scan pass over a capture log's newly appended bytes: classify the
    /// new lines, grow the take's alerts (+ 🔔/toast on first occurrence per
    /// kind), refresh the Twitch lost-range queue, and kick the recovery job
    /// once the VOD should cover a pending range. Called from the stall
    /// watchdog every 60 s; never touches the capture itself.
    pub(super) async fn scan_log_cycle(
        &self,
        kind: DetachedKind,
        ref_id: i64,
        log_path: &Path,
        capture_path: &Path,
        st: &mut LogScan,
    ) {
        let Some(text) = read_new_log_text(log_path, st).await else {
            // No new bytes — a quiet cycle. Recovery kicks off only on quiet
            // cycles so a burst still in progress isn't fetched piecemeal.
            self.maybe_kick_gap_recovery(kind, ref_id, st).await;
            return;
        };
        let combined = format!("{}{}", st.partial, text);
        let (complete, partial) = match combined.rfind('\n') {
            Some(i) => combined.split_at(i + 1),
            None => ("", combined.as_str()),
        };
        st.partial = partial.to_string();
        let summary = scan_lines(complete);
        if summary.hits.is_empty() {
            self.maybe_kick_gap_recovery(kind, ref_id, st).await;
            return;
        }

        // Resolve display label + platform once, on first blood.
        if st.labels.is_none() {
            let resolved = st
                .monitor_id
                .and_then(|m| self.store.get_monitor_with_channel(m).ok().flatten())
                .map(|row| (row.channel.name.clone(), row.monitor.platform() == Platform::Twitch));
            st.labels = Some(resolved.unwrap_or_else(|| {
                let stem = capture_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (stem, false)
            }));
        }
        let (label, twitch) = st.labels.clone().unwrap_or_default();

        for (hit_kind, severity, count, lost, last_line) in &summary.hits {
            let alert = crate::store::NewCaptureAlert {
                kind: hit_kind.to_string(),
                severity: severity.to_string(),
                source: if st.source.is_empty() {
                    infer_source(last_line).to_string()
                } else {
                    st.source.clone()
                },
                take_key: log_path.to_string_lossy().into_owned(),
                monitor_id: st.monitor_id,
                recording_id: (kind == DetachedKind::Recording).then_some(ref_id),
                video_id: (kind == DetachedKind::Video).then_some(ref_id),
                channel: label.clone(),
                count: *count,
                lost_segments: *lost,
                last_line: last_line.clone(),
            };
            match self.store.upsert_capture_alert(&alert) {
                Ok((alert_id, was_new)) => {
                    // Log level mirrors the alert severity: an error alert
                    // means content is MISSING from the capture.
                    if *severity == "error" {
                        error!(
                            ref_id,
                            "capture alert [{label}]: {hit_kind} ×{count} (+{lost} lost segments) — {last_line}"
                        );
                    } else {
                        warn!(ref_id, "capture alert [{label}]: {hit_kind} ×{count} — {last_line}");
                    }
                    if was_new {
                        let (title, body) = alert_message(hit_kind, &label, *lost, last_line, twitch);
                        let _ = self.events.send(AppEvent::CaptureAlert {
                            severity: severity.to_string(),
                            title,
                            body,
                            monitor_id: st.monitor_id,
                            channel: label.clone(),
                            recording_id: (kind == DetachedKind::Recording).then_some(ref_id),
                            ref_key: format!("capalert:{alert_id}"),
                        });
                    }
                }
                Err(e) => warn!(ref_id, "capture alert upsert failed: {e:#}"),
            }
        }

        // Twitch recordings: turn the cumulative lost-segment list into the
        // pending recovery queue. (Ranges already being fetched / done are
        // preserved by `replace_pending_gap_ranges`.)
        if !summary.gaps.is_empty() {
            st.all_gaps.extend(summary.gaps.iter().copied());
            if kind == DetachedKind::Recording && twitch && gap_recover_enabled(&self.store) {
                let ranges = gap_ranges_secs(&st.all_gaps);
                if let Err(e) = self.store.replace_pending_gap_ranges(ref_id, &ranges) {
                    warn!(ref_id, "gap ranges: queue update failed: {e:#}");
                }
                if let Ok((total, done, muted)) = self.store.gap_range_progress(ref_id) {
                    let _ = self.store.set_alert_recovery(ref_id, total, done, muted);
                }
            }
        }
    }

    /// Spawn the recovery job for a recording whose pending ranges the VOD
    /// should now cover (quiet-cycle path). Cheap no-op in the common case.
    async fn maybe_kick_gap_recovery(&self, kind: DetachedKind, ref_id: i64, st: &LogScan) {
        if kind != DetachedKind::Recording
            || st.all_gaps.is_empty()
            || !matches!(st.labels, Some((_, true)))
            || !gap_recover_enabled(&self.store)
        {
            return;
        }
        let Some(went_live) = st.went_live_at else {
            // No go-live anchor to judge VOD coverage against in-flight — the
            // finalize sweep will run recovery instead.
            return;
        };
        let elapsed = now_unix() - went_live;
        let ready = self
            .store
            .gap_ranges_in_state(ref_id, "pending")
            .unwrap_or_default()
            .iter()
            .any(|r| (r.end_secs as i64) + GAP_VOD_LAG_SECS < elapsed);
        if ready {
            self.maybe_spawn_gap_recover(ref_id, false);
        }
    }
}

// ---------- retro sweep over past capture logs ----------

/// Retro sweep skips log files bigger than this (endless-retry spam; the live
/// scanner's tail cap covers those takes while they run).
const RETRO_SCAN_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Log-filename timestamp must land within this of a recording's `started_at`
/// to bind the log to that take.
const RETRO_MATCH_SLOP_SECS: i64 = 300;

/// Parse a capture-log filename ("Ebiko - 2026-07-22 02-50-48 - … -
/// [twitch 320438670041].ts.log") into `(platform_tag, stream_id,
/// local_start_epoch)`. `None` when there's no `[platform id]` bracket (chat
/// sidecars, odd names).
pub(super) fn parse_capture_log_name(name: &str) -> Option<(String, String, Option<i64>)> {
    // The platform bracket is the LAST `[...]` group (titles may contain
    // their own brackets, and `[games-tba]` precedes it).
    let (platform, id) = name
        .rmatch_indices('[')
        .find_map(|(i, _)| {
            let inner = &name[i + 1..name[i..].find(']')? + i];
            let mut parts = inner.split_whitespace();
            let tag = parts.next()?;
            let id = parts.next()?;
            (matches!(tag, "twitch" | "youtube" | "kick") && parts.next().is_none())
                .then(|| (tag.to_string(), id.to_string()))
        })?;
    // Take timestamp: the first "YYYY-MM-DD HH-MM-SS" in the name, local time
    // (that's how capture stems are built).
    let ts = regex_lite::Regex::new(r"(\d{4})-(\d{2})-(\d{2}) (\d{2})-(\d{2})-(\d{2})")
        .ok()
        .and_then(|re| {
            let c = re.captures(name)?;
            let get = |i: usize| c.get(i).unwrap().as_str().parse::<u32>().ok();
            let date = chrono::NaiveDate::from_ymd_opt(
                get(1)? as i32,
                get(2)?,
                get(3)?,
            )?;
            let time = chrono::NaiveTime::from_hms_opt(get(4)?, get(5)?, get(6)?)?;
            use chrono::TimeZone;
            match chrono::Local.from_local_datetime(&date.and_time(time)) {
                chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
                    Some(dt.timestamp())
                }
                chrono::LocalResult::None => None,
            }
        });
    Some((platform, id, ts))
}

impl Supervisor {
    /// One-shot startup sweep over past capture logs (`logs\captures\*.log`,
    /// 7-day retention): file alerts for takes that lost data BEFORE this
    /// feature existed (or while the app was down), and queue Twitch gap
    /// recovery for them — the CDN keeps the content ~60 days, so past
    /// streams are still repairable (muted fallback included). Idempotent:
    /// any existing alert for a log skips the file, and still-running takes
    /// are left to the live scanner.
    pub async fn retro_scan_capture_logs(&self) {
        use crate::iomon::Cat;
        use tokio::io::AsyncReadExt;
        let dir = crate::app_paths::logs_dir().join("captures");
        let Ok(mut rd) = crate::iomon::fs::read_dir(Cat::LogRead, &dir).await else { return };
        let (mut scanned, mut filed) = (0usize, 0usize);
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".log") || name.contains(".chat.") {
                continue;
            }
            let take_key = path.to_string_lossy().into_owned();
            if self.store.alert_exists_for_take(&take_key) {
                continue; // live-scanned or swept before — rescanning would double counters
            }
            let Some((platform_tag, stream_id, ts)) = parse_capture_log_name(&name) else {
                continue;
            };
            match crate::iomon::fs::metadata(Cat::LogRead, &path).await {
                Ok(m) if m.len() <= RETRO_SCAN_MAX_BYTES => {}
                _ => continue,
            }
            let Ok(mut f) = crate::iomon::fs::open(Cat::LogRead, &path).await else { continue };
            let mut buf = Vec::new();
            let read_start = std::time::Instant::now();
            if f.read_to_end(&mut buf).await.is_err() {
                continue;
            }
            crate::iomon::record(Cat::LogRead, &path, crate::iomon::OpKind::Read, buf.len() as u64, read_start.elapsed());
            scanned += 1;
            let summary = scan_lines(&String::from_utf8_lossy(&buf));
            if summary.hits.is_empty() {
                continue;
            }

            // Bind the log to its recording via stream id + take timestamp.
            let rec = self
                .store
                .recordings_by_stream_id(&stream_id)
                .unwrap_or_default()
                .into_iter()
                .find(|(_, _, started, _)| {
                    ts.map(|t| (started - t).abs() <= RETRO_MATCH_SLOP_SECS).unwrap_or(true)
                });
            if matches!(&rec, Some((_, _, _, status)) if status == "recording") {
                continue; // an active take — the live scanner owns its log
            }
            let (rec_id, monitor_id) = match &rec {
                Some((id, mid, _, _)) => (Some(*id), Some(*mid)),
                None => (None, None),
            };
            let row = monitor_id.and_then(|m| self.store.get_monitor_with_channel(m).ok().flatten());
            let twitch = row.as_ref().map(|r| r.monitor.platform() == Platform::Twitch).unwrap_or(false)
                && platform_tag == "twitch";
            let label = row
                .as_ref()
                .map(|r| r.channel.name.clone())
                // Fallback: the channel prefix of the log filename.
                .unwrap_or_else(|| name.split(" - ").next().unwrap_or_default().to_string());

            for (hit_kind, severity, count, lost, last_line) in &summary.hits {
                let alert = crate::store::NewCaptureAlert {
                    kind: hit_kind.to_string(),
                    severity: severity.to_string(),
                    source: infer_source(last_line).to_string(),
                    take_key: take_key.clone(),
                    monitor_id,
                    recording_id: rec_id,
                    video_id: None,
                    channel: label.clone(),
                    count: *count,
                    lost_segments: *lost,
                    last_line: last_line.clone(),
                };
                if let Ok((alert_id, was_new)) = self.store.upsert_capture_alert(&alert) {
                    filed += 1;
                    if *severity == "error" {
                        error!(
                            "retro log sweep [{label}]: {hit_kind} ×{count} (+{lost} lost segments) — {last_line}"
                        );
                    }
                    if was_new {
                        let (title, body) = alert_message(hit_kind, &label, *lost, last_line, twitch);
                        let _ = self.events.send(AppEvent::CaptureAlert {
                            severity: severity.to_string(),
                            title,
                            body,
                            monitor_id,
                            channel: label.clone(),
                            recording_id: rec_id,
                            ref_key: format!("capalert:{alert_id}"),
                        });
                    }
                }
            }

            // Queue + kick recovery for a matched, finished Twitch take.
            if let Some(rec_id) = rec_id
                && twitch
                && !summary.gaps.is_empty()
                && gap_recover_enabled(&self.store)
            {
                let ranges = gap_ranges_secs(&summary.gaps);
                let _ = self.store.replace_pending_gap_ranges(rec_id, &ranges);
                if let Ok((total, done, muted)) = self.store.gap_range_progress(rec_id) {
                    let _ = self.store.set_alert_recovery(rec_id, total, done, muted);
                }
                self.maybe_spawn_gap_recover(rec_id, true);
            }
        }
        if scanned > 0 {
            info!("retro log sweep: {scanned} past capture log(s) scanned, {filed} alert(s) filed");
        }
    }
}

/// First-occurrence 🔔/toast copy per alert kind.
fn alert_message(
    kind: &str,
    label: &str,
    lost: i64,
    last_line: &str,
    twitch: bool,
) -> (String, String) {
    match kind {
        "sequence_gap" | "fetch_failed" => {
            let secs = (lost as f64 * TWITCH_SEG_SECS) as i64;
            let recover = if twitch {
                " Lost ranges will be re-fetched from the VOD automatically."
            } else {
                ""
            };
            (
                format!("Capture losing data — {label}"),
                format!(
                    "{lost} segment(s) (~{secs}s) missing from the capture so far.{recover}\n{last_line}"
                ),
            )
        }
        "tool_error" => (format!("Capture tool error — {label}"), last_line.to_string()),
        _ => (format!("Capture tool warning — {label}"), last_line.to_string()),
    }
}

/// Read the log's newly appended bytes (from `st.offset`), honoring the
/// first-cycle adopt rule, the per-cycle cap, and truncation resets. `None`
/// when there's nothing new. Advances `st.offset`.
async fn read_new_log_text(path: &Path, st: &mut LogScan) -> Option<String> {
    use crate::iomon::Cat;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let len = crate::iomon::fs::metadata(Cat::LogRead, path).await.ok()?.len();
    if st.offset == SCAN_OFFSET_ADOPT {
        st.offset = if len <= ADOPT_FULL_SCAN_MAX { 0 } else { len.saturating_sub(64 * 1024) };
    }
    if len < st.offset {
        // Truncated/replaced — restart from the top.
        st.offset = 0;
        st.partial.clear();
    }
    if len == st.offset {
        return None;
    }
    if len - st.offset > SCAN_MAX_CYCLE_BYTES {
        debug!(
            "capture log scan: skipping {} bytes of runaway output in {}",
            len - st.offset - 64 * 1024,
            path.display()
        );
        st.offset = len.saturating_sub(64 * 1024);
        st.partial.clear();
    }
    let mut f = crate::iomon::fs::open(Cat::LogRead, path).await.ok()?;
    f.seek(std::io::SeekFrom::Start(st.offset)).await.ok()?;
    let mut buf = Vec::with_capacity((len - st.offset) as usize);
    let read_start = std::time::Instant::now();
    let n = f.take(len - st.offset).read_to_end(&mut buf).await.ok()?;
    crate::iomon::record(Cat::LogRead, path, crate::iomon::OpKind::Read, n as u64, read_start.elapsed());
    st.offset += n as u64;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_real_ebiko_lines() {
        // Verbatim lines from the 2026-07-22 Ebiko capture log.
        let gap = "[stream.segmented][warning] Sequence gap of 8 segments at position 3137. \
                   This is unsupported and will result in incoherent output data.";
        assert_eq!(
            classify_line(gap),
            Some(LineHit { kind: "sequence_gap", severity: "error", lost: Some((3137, 8)) })
        );
        let one = "[stream.segmented][warning] Sequence gap of 1 segment at position 11842. \
                   This is unsupported and will result in incoherent output data.";
        assert_eq!(classify_line(one).unwrap().lost, Some((11842, 1)));

        // Real streamlink fetch-failure shape (hls.py log.error).
        let ff = "[stream.hls][error] Failed to fetch segment 3200: Unable to open URL";
        assert_eq!(
            classify_line(ff),
            Some(LineHit { kind: "fetch_failed", severity: "error", lost: Some((3200, 1)) })
        );

        // yt-dlp shapes.
        assert_eq!(classify_line("ERROR: fragment 12 not found").unwrap().kind, "tool_error");
        assert_eq!(
            classify_line("[download] Skipping fragment 812 ...").unwrap().lost,
            Some((812, 1))
        );
        assert_eq!(
            classify_line("WARNING: [youtube] Some formats are missing").unwrap().severity,
            "warning"
        );

        // Benign / non-alert lines stay silent.
        assert_eq!(classify_line("[cli][info] Opening stream: 1080p60 (hls)"), None);
        assert_eq!(classify_line("[plugins.twitch][info] Will skip ad segments"), None);
        assert_eq!(classify_line("WARNING: Retrying (1/3) after connection reset"), None);
        assert_eq!(classify_line("[download] 1.2GiB at 5MiB/s"), None);
        assert_eq!(classify_line(""), None);
    }

    #[test]
    fn scan_aggregates_per_kind() {
        let text = "\
[stream.segmented][warning] Sequence gap of 8 segments at position 3137. This is unsupported and will result in incoherent output data.
[cli][info] noise
[stream.segmented][warning] Sequence gap of 21 segments at position 3159. This is unsupported and will result in incoherent output data.
[stream.hls][error] Failed to fetch segment 4000: Unable to open URL
";
        let s = scan_lines(text);
        assert_eq!(s.gaps, vec![(3137, 8), (3159, 21), (4000, 1)]);
        let gap = s.hits.iter().find(|(k, ..)| *k == "sequence_gap").unwrap();
        assert_eq!((gap.2, gap.3), (2, 29));
        assert!(gap.4.contains("position 3159"));
        let ff = s.hits.iter().find(|(k, ..)| *k == "fetch_failed").unwrap();
        assert_eq!((ff.2, ff.3), (1, 1));
    }

    #[test]
    fn parse_capture_log_names() {
        // The real Ebiko log filename (brackets in the games slot too).
        let (platform, id, ts) = parse_capture_log_name(
            "Ebiko - 2026-07-22 02-50-48 - title-tba [games-tba] (1080p60 live h264 aac) - \
             [twitch 320438670041].ts.log",
        )
        .unwrap();
        assert_eq!((platform.as_str(), id.as_str()), ("twitch", "320438670041"));
        // Local-time parse — just assert it resolved to a plausible epoch.
        assert!(ts.is_some_and(|t| t > 1_700_000_000));

        // YouTube SABR names (title may carry 【】 and its own brackets).
        let (platform, id, _) = parse_capture_log_name(
            "Dokibird - 2026-07-21 22-38-37 - 【PALWORLD】Making [pals] sloppy [games-tba] \
             (p sabr  ) - [youtube VVPVScN7pR0].mkv.log",
        )
        .unwrap();
        assert_eq!((platform.as_str(), id.as_str()), ("youtube", "VVPVScN7pR0"));

        // No platform bracket → not a capture log we can bind.
        assert!(parse_capture_log_name("random-notes.log").is_none());
    }

    #[test]
    fn gap_range_math_pads_coalesces_and_clamps() {
        // 8 lost segments at 3137 → 6274..6290 s, padded to 6264..6300.
        assert_eq!(gap_ranges_secs(&[(3137, 8)]), vec![(6264.0, 6300.0)]);
        // A gap at position 0 clamps to broadcast start.
        assert_eq!(gap_ranges_secs(&[(0, 3)]), vec![(0.0, 16.0)]);
        // 3137+8 ends at 6290+10=6300; 3159 starts at 6318-10=6308 — 8 s apart
        // → coalesced. The far-away run stays separate. Order-independent.
        let got = gap_ranges_secs(&[(9000, 5), (3137, 8), (3159, 21)]);
        assert_eq!(got, vec![(6264.0, 6370.0), (17990.0, 18020.0)]);
    }
}
