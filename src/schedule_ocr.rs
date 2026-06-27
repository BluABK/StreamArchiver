//! Image → schedule OCR.
//!
//! Several schedule sources publish the week as an *image* (a Twitch offline
//! banner, a YouTube community post, a pinned tweet). We read those by shelling
//! out to an LLM CLI — by default the `claude` CLI — with a strict OCR→JSON
//! prompt, then map the returned events to [`ScheduleSegment`]s. This ports the
//! user's working `scripts/decode-schedule-img.ps1` into the app.
//!
//! Everything here fails *soft*: a missing CLI, an auth prompt, a non-zero exit,
//! or unparseable output all yield `None` so a transient OCR failure never wipes a
//! previously-stored schedule.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use chrono::Datelike;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::models::{
    K_OCR_COMMAND, K_OCR_FALLBACK_MODEL, K_OCR_MODEL, K_OCR_OFFSET, K_OCR_TIMEZONE, ScheduleSegment,
};
use crate::schedule_source::ChannelSourceConfig;
use crate::store::Store;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Built-in defaults when the corresponding setting is unset/empty.
pub const DEFAULT_COMMAND: &str = "claude";
pub const DEFAULT_MODEL: &str = "haiku";
pub const DEFAULT_FALLBACK_MODEL: &str = "sonnet";

/// Hard ceiling on one CLI invocation — a hung `claude` must not stall the
/// schedule refresh loop.
const OCR_TIMEOUT: Duration = Duration::from_secs(150);

/// Resolved options for one OCR run.
#[derive(Clone, Debug)]
pub struct OcrOpts {
    /// The LLM CLI executable (default `claude`).
    pub command: String,
    /// Primary model to try first.
    pub model: String,
    /// Stronger model to retry with if the primary returns invalid JSON.
    pub fallback_model: String,
    /// IANA timezone the banner times are assumed to be in (empty = let the model
    /// use whatever the banner shows).
    pub timezone: String,
    /// UTC offset string matching the timezone/season, e.g. `"+02:00"` (empty =
    /// derive from the machine's current local offset).
    pub offset: String,
    /// Year to assume for dates (banners rarely show it).
    pub year: i32,
}

/// Build [`OcrOpts`] from global settings, with per-channel overrides taking
/// precedence (model + timezone + offset).
pub fn ocr_opts_from_settings(store: &Store, cfg: &ChannelSourceConfig) -> OcrOpts {
    let get = |key: &str| store.get_setting(key).ok().flatten().unwrap_or_default();
    let non_empty = |s: String, default: &str| {
        if s.trim().is_empty() {
            default.to_string()
        } else {
            s
        }
    };
    let pick = |chan: &str, global: String| {
        if !chan.trim().is_empty() {
            chan.to_string()
        } else {
            global
        }
    };

    let offset = pick(&cfg.ocr_offset, get(K_OCR_OFFSET));
    let offset = if offset.trim().is_empty() {
        local_offset_string()
    } else {
        offset
    };

    OcrOpts {
        command: non_empty(get(K_OCR_COMMAND), DEFAULT_COMMAND),
        model: non_empty(pick(&cfg.ocr_model, get(K_OCR_MODEL)), DEFAULT_MODEL),
        fallback_model: non_empty(get(K_OCR_FALLBACK_MODEL), DEFAULT_FALLBACK_MODEL),
        timezone: pick(&cfg.ocr_timezone, get(K_OCR_TIMEZONE)),
        offset,
        year: chrono::Local::now().year(),
    }
}

/// `"+02:00"`-style string for the machine's current local UTC offset.
fn local_offset_string() -> String {
    use chrono::Offset;
    let secs = chrono::Local::now().offset().fix().local_minus_utc();
    let sign = if secs < 0 { '-' } else { '+' };
    let secs = secs.abs();
    format!("{sign}{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

/// One event object as emitted by the OCR prompt (mirrors the script's schema).
#[derive(Debug, Deserialize)]
struct OcrEvent {
    #[serde(default)]
    title: String,
    #[serde(default)]
    collab: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    time: Option<String>,
    /// ISO 8601 with offset, or null when there's no exact time.
    #[serde(default)]
    datetime: Option<String>,
}

/// OCR a schedule image into [`ScheduleSegment`]s. `Some(vec)` on a successful
/// decode (possibly empty if the image held nothing datable); `None` on any
/// failure (CLI missing, non-zero exit, unparseable output) so callers fall soft.
pub async fn ocr_schedule_image(image_path: &Path, opts: &OcrOpts) -> Option<Vec<ScheduleSegment>> {
    if !image_path.exists() {
        debug!("OCR: image not found: {}", image_path.display());
        return None;
    }
    let dir = image_path.parent().unwrap_or_else(|| Path::new("."));
    let img = image_path.to_string_lossy().replace('\\', "/");
    let dir_s = dir.to_string_lossy().replace('\\', "/");
    let prompt = build_prompt(&img, opts);

    // Try the cheap model first, then the stronger fallback on a parse miss.
    let mut events = match run_cli(&opts.command, &opts.model, &dir_s, &prompt).await {
        Some(raw) => parse_events(&raw),
        None => None,
    };
    if events.is_none() && opts.fallback_model != opts.model {
        debug!("OCR: retrying with fallback model {}", opts.fallback_model);
        if let Some(raw) = run_cli(&opts.command, &opts.fallback_model, &dir_s, &prompt).await {
            events = parse_events(&raw);
        }
    }
    let events = events?;
    Some(map_events(events, opts))
}

/// Substitute the per-image placeholders into the strict OCR→JSON prompt
/// (ported from `scripts/decode-schedule-img.ps1`).
fn build_prompt(image_path: &str, opts: &OcrOpts) -> String {
    let tz = if opts.timezone.trim().is_empty() {
        "the timezone shown on the banner"
    } else {
        opts.timezone.as_str()
    };
    format!(
        "You are an OCR-to-JSON extractor. Read the streamer schedule in {image_path} and output an array of event objects.\n\
\n\
RULES:\n\
- Output ONLY raw JSON. No markdown, no code fences, no backticks, no commentary, no leading or trailing text. The first character of your reply must be '[' and the last must be ']'.\n\
- Timezone: use exactly what the banner shows. The labels indicate {tz}, so timezone = '{tz}' and the UTC offset = {offset}. Do NOT convert to any other timezone. Do NOT use my local timezone.\n\
- The year is {year}.\n\
- Transcribe titles literally. 'w' or 'W' before a name means 'with' (a collaborator), e.g. 'FEARS TO FATHOM w CRELLY' -> title 'Fears to Fathom', collab 'Crelly'. Do not guess or 'correct' names.\n\
- Skip any card marked OFFLINE or with an unknown date ('????').\n\
- If a time is vague (e.g. 'Evening'), set time and datetime to null but keep the raw text in time_label.\n\
\n\
Each object has these fields:\n\
- title (string)\n\
- collab (string or null)\n\
- date (YYYY-MM-DD)\n\
- day (weekday name)\n\
- time (HH:MM 24-hour, or null)\n\
- time_label (raw time text from banner, e.g. '12.00 P.M.' or 'Evening')\n\
- timezone (IANA name)\n\
- datetime (ISO 8601 with offset, or null if no exact time)\n\
- source_image (set this to the filename: {image_path})",
        offset = opts.offset,
        year = opts.year,
    )
}

/// Run the LLM CLI for one image+model. Returns trimmed stdout on a zero exit
/// with non-empty output; `None` otherwise (spawn failure, timeout, non-zero
/// exit, empty output).
async fn run_cli(command: &str, model: &str, dir: &str, prompt: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(["--model", model, "--add-dir", dir, "-p", prompt])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!("OCR: failed to spawn '{command}': {e} (is the CLI installed/on PATH?)");
            return None;
        }
    };
    let out = match tokio::time::timeout(OCR_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            warn!("OCR: CLI wait failed: {e}");
            return None;
        }
        Err(_) => {
            warn!("OCR: CLI timed out after {}s", OCR_TIMEOUT.as_secs());
            return None;
        }
    };
    if !out.status.success() {
        warn!("OCR: CLI exited with {:?}", out.status.code());
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.is_empty() { None } else { Some(stdout) }
}

/// Clean and parse the CLI's reply into events: strip any stray markdown fences,
/// trim to the outermost JSON array, then deserialize. `None` on invalid JSON.
fn parse_events(raw: &str) -> Option<Vec<OcrEvent>> {
    let clean = raw.replace("```json", "").replace("```", "");
    // Trim to the outermost JSON array, in case of leading/trailing prose.
    let start = clean.find('[')?;
    let end = clean.rfind(']')?;
    if end <= start {
        return None;
    }
    let slice = &clean[start..=end];
    match serde_json::from_str::<Vec<OcrEvent>>(slice) {
        Ok(v) => Some(v),
        Err(e) => {
            debug!("OCR: invalid JSON ({e})");
            None
        }
    }
}

/// Map decoded events to schedule segments, dropping any without a resolvable
/// exact start time (vague "Evening" cards carry no datetime).
fn map_events(events: Vec<OcrEvent>, opts: &OcrOpts) -> Vec<ScheduleSegment> {
    let mut out: Vec<ScheduleSegment> = events
        .into_iter()
        .filter_map(|e| {
            let start = event_start(&e, opts)?;
            let title = match e.collab.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(c) if !e.title.trim().is_empty() => format!("{} w/ {}", e.title.trim(), c),
                _ => e.title.trim().to_string(),
            };
            if title.is_empty() {
                return None;
            }
            Some(ScheduleSegment {
                id: 0,
                monitor_id: 0,
                start_time: start,
                end_time: None,
                title,
                category: String::new(),
                canceled: false,
                video_id: None,
            })
        })
        .collect();
    out.sort_by(|a, b| a.start_time.cmp(&b.start_time).then_with(|| a.title.cmp(&b.title)));
    out.dedup_by(|a, b| a.start_time == b.start_time && a.title == b.title);
    out
}

/// Resolve an event's start time (unix seconds): prefer the ISO `datetime`, else
/// combine `date` + `time` + the assumed offset. `None` when no exact time.
fn event_start(e: &OcrEvent, opts: &OcrOpts) -> Option<i64> {
    if let Some(dt) = e.datetime.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(dt) {
            return Some(parsed.timestamp());
        }
    }
    // Fallback: date (YYYY-MM-DD) + time (HH:MM) + assumed offset → RFC3339.
    let date = e.date.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    let time = e.time.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    let offset = if opts.offset.trim().is_empty() {
        "+00:00"
    } else {
        opts.offset.trim()
    };
    let composed = format!("{date}T{time}:00{offset}");
    chrono::DateTime::parse_from_rfc3339(&composed)
        .ok()
        .map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> OcrOpts {
        OcrOpts {
            command: "claude".into(),
            model: "haiku".into(),
            fallback_model: "sonnet".into(),
            timezone: "Europe/Oslo".into(),
            offset: "+02:00".into(),
            year: 2026,
        }
    }

    #[test]
    fn parse_strips_fences_and_trims_prose() {
        let raw = "Here you go:\n```json\n[{\"title\":\"A\",\"datetime\":\"2026-06-18T20:00:00+02:00\"}]\n```\nDone.";
        let events = parse_events(raw).expect("should parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "A");
    }

    #[test]
    fn map_uses_datetime_and_collab_and_drops_timeless() {
        let raw = r#"[
          {"title":"Fears to Fathom","collab":"Crelly","datetime":"2026-06-18T20:00:00+02:00"},
          {"title":"Cozy night","collab":null,"date":"2026-06-19","time":"21:30","datetime":null},
          {"title":"Maybe stream","collab":null,"time":null,"datetime":null}
        ]"#;
        let events = parse_events(raw).unwrap();
        let segs = map_events(events, &opts());
        // Third (no time) is dropped; first two kept and sorted by start.
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].title, "Fears to Fathom w/ Crelly");
        assert_eq!(segs[1].title, "Cozy night");
        // 2026-06-18T20:00+02:00 == 2026-06-18T18:00Z == 1781805600
        assert_eq!(segs[0].start_time, 1781805600);
    }

    #[test]
    fn parse_rejects_non_array() {
        assert!(parse_events("not json at all").is_none());
        assert!(parse_events("{\"title\":\"x\"}").is_none());
    }
}
