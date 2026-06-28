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
use tracing::{debug, info, warn};

use crate::models::{
    K_OCR_COMMAND, K_OCR_EFFORT, K_OCR_FALLBACK_MODEL, K_OCR_MAX_BUDGET, K_OCR_MODEL,
    K_OCR_OFFSET, K_OCR_STATS, K_OCR_TIMEOUT_SECS, K_OCR_TIMEZONE, OcrModelStats, OcrStats,
    ScheduleSegment, now_unix,
};
use crate::schedule_source::ChannelSourceConfig;
use crate::store::Store;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Built-in defaults when the corresponding setting is unset/empty.
pub const DEFAULT_COMMAND: &str = "claude";
pub const DEFAULT_MODEL: &str = "haiku";
pub const DEFAULT_FALLBACK_MODEL: &str = "sonnet";

/// Default timeout for one CLI invocation when the setting is absent/zero.
const DEFAULT_TIMEOUT_SECS: u64 = 150;

/// Stats returned by one successful `claude` CLI call.
#[derive(Clone, Debug, Default)]
pub struct OcrCallStats {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_usd: f64,
}

/// Result of one `ocr_schedule_image` attempt (primary + optional fallback call).
pub struct OcrRunResult {
    /// Decoded segments — `None` if every CLI call failed or parse failed entirely.
    pub segments: Option<Vec<ScheduleSegment>>,
    /// Stats from each successful CLI call (0 = CLI failed, 1 = primary ok, 2 = fallback also ran).
    pub cli_calls: Vec<OcrCallStats>,
    /// CLI invocations that returned a non-zero exit / timed out / failed to spawn.
    pub cli_failures: u32,
    /// CLI calls that returned output but whose JSON couldn't be parsed as schedule events.
    pub parse_failures: u32,
}

/// JSON envelope returned by `claude --output-format json`.
#[derive(Deserialize)]
struct CliEnvelope {
    #[serde(default)]
    result: String,
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    usage: CliUsage,
}

#[derive(Default, Deserialize)]
struct CliUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

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
    /// `--max-budget-usd` value (empty = omit flag, no per-call USD cap).
    pub max_budget_usd: String,
    /// Process timeout in seconds (0 = use `DEFAULT_TIMEOUT_SECS`).
    pub timeout_secs: u64,
    /// `--effort` level (empty = omit flag; valid: low/medium/high/xhigh/max).
    pub effort: String,
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

    let timezone = pick(&cfg.ocr_timezone, get(K_OCR_TIMEZONE));
    let offset = pick(&cfg.ocr_offset, get(K_OCR_OFFSET));
    // Only fall back to the MACHINE's local offset when no timezone info was given
    // at all. If a primary timezone is named but the offset is blank, pairing that
    // zone with the machine offset would be a contradiction (the per-channel
    // override exists precisely for zones *other* than the machine's) and would
    // shift every event — so leave the offset empty and let the prompt have the
    // model derive the correct (DST-aware) offset from the named zone instead.
    let offset = if offset.trim().is_empty() && timezone.trim().is_empty() {
        local_offset_string()
    } else {
        offset
    };

    let timeout_secs = get(K_OCR_TIMEOUT_SECS)
        .trim()
        .parse::<u64>()
        .unwrap_or(0);

    OcrOpts {
        command: non_empty(get(K_OCR_COMMAND), DEFAULT_COMMAND),
        model: non_empty(pick(&cfg.ocr_model, get(K_OCR_MODEL)), DEFAULT_MODEL),
        fallback_model: non_empty(get(K_OCR_FALLBACK_MODEL), DEFAULT_FALLBACK_MODEL),
        timezone,
        offset,
        year: chrono::Local::now().year(),
        max_budget_usd: get(K_OCR_MAX_BUDGET),
        timeout_secs,
        effort: get(K_OCR_EFFORT),
    }
}

/// Load the cumulative OCR stats from the settings store.
pub fn load_ocr_stats(store: &Store) -> OcrStats {
    store
        .get_setting(K_OCR_STATS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist cumulative OCR stats back to the settings store.
pub fn save_ocr_stats(store: &Store, stats: &OcrStats) {
    if let Ok(json) = serde_json::to_string(stats) {
        let _ = store.set_setting(K_OCR_STATS, &json);
    }
}

/// Fold one `OcrRunResult` into the persistent cumulative stats.
pub fn accumulate_ocr_stats(store: &Store, result: &OcrRunResult) {
    if result.cli_calls.is_empty() && result.cli_failures == 0 && result.parse_failures == 0 {
        return;
    }
    let mut stats = load_ocr_stats(store);
    stats.cli_failures += result.cli_failures as u64;
    stats.parse_failures += result.parse_failures as u64;
    for call in &result.cli_calls {
        stats.calls += 1;
        stats.input_tokens += call.input_tokens;
        stats.output_tokens += call.output_tokens;
        stats.cache_read_tokens += call.cache_read_tokens;
        stats.cache_creation_tokens += call.cache_creation_tokens;
        stats.cost_usd += call.cost_usd;
        stats.last_call_at = Some(now_unix());
        let m: &mut OcrModelStats = stats.by_model.entry(call.model.clone()).or_default();
        m.calls += 1;
        m.input_tokens += call.input_tokens;
        m.output_tokens += call.output_tokens;
        m.cost_usd += call.cost_usd;
    }
    save_ocr_stats(store, &stats);
}

/// Increment the cache-hit counter in the persistent stats.
pub fn record_ocr_cache_hit(store: &Store) {
    let mut stats = load_ocr_stats(store);
    stats.cache_hits += 1;
    save_ocr_stats(store, &stats);
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

/// OCR a schedule image. Always returns an `OcrRunResult` so callers can accumulate
/// stats even on failure; `result.segments` is `None` when nothing parseable came back.
pub async fn ocr_schedule_image(image_path: &Path, opts: &OcrOpts) -> OcrRunResult {
    if !image_path.exists() {
        debug!("OCR: image not found: {}", image_path.display());
        return OcrRunResult { segments: None, cli_calls: vec![], cli_failures: 0, parse_failures: 0 };
    }
    let dir = image_path.parent().unwrap_or_else(|| Path::new("."));
    let img = image_path.to_string_lossy().replace('\\', "/");
    let dir_s = dir.to_string_lossy().replace('\\', "/");
    let prompt = build_prompt(&img, opts);

    let mut cli_calls: Vec<OcrCallStats> = Vec::new();
    let mut cli_failures: u32 = 0;
    let mut parse_failures: u32 = 0;

    // Try the cheap model first, then the stronger fallback on a parse miss.
    let events = match run_cli(&opts.command, &opts.model, &dir_s, &prompt, opts).await {
        Some((raw, call_stats)) => {
            cli_calls.push(call_stats);
            match parse_events(&raw) {
                Some(ev) => Some(ev),
                None => {
                    parse_failures += 1;
                    None
                }
            }
        }
        None => {
            cli_failures += 1;
            None
        }
    };

    let events = if events.is_none() && opts.fallback_model != opts.model {
        debug!("OCR: retrying with fallback model {}", opts.fallback_model);
        match run_cli(&opts.command, &opts.fallback_model, &dir_s, &prompt, opts).await {
            Some((raw, call_stats)) => {
                cli_calls.push(call_stats);
                match parse_events(&raw) {
                    Some(ev) => Some(ev),
                    None => {
                        parse_failures += 1;
                        None
                    }
                }
            }
            None => {
                cli_failures += 1;
                None
            }
        }
    } else {
        events
    };

    let segments = events.map(|ev| map_events(ev, opts));
    OcrRunResult { segments, cli_calls, cli_failures, parse_failures }
}

/// Substitute the per-image placeholders into the strict OCR→JSON prompt
/// (ported from `scripts/decode-schedule-img.ps1`).
///
/// Timezones are the subtle part: image schedules print times in whatever zone(s)
/// the streamer chose — and some print the SAME stream in several zones at once
/// (e.g. PDT / JST / GMT), sometimes with a `*` "next day" mark when a conversion
/// rolls past midnight. We make the model resolve each stream block to ONE
/// absolute instant (RFC 3339 with its real offset) so multi-zone rows can't
/// inflate into duplicate events, anchoring date headers to the channel's primary
/// timezone when one is configured.
fn build_prompt(image_path: &str, opts: &OcrOpts) -> String {
    let primary = opts.timezone.trim();
    let offset = opts.offset.trim();
    let tz_rules = if !primary.is_empty() {
        // Only state a numeric offset if the user actually supplied one. Otherwise
        // tell the model to work out the correct (DST-aware) offset for the named
        // zone itself — never pair the zone with an unrelated machine offset.
        let zone_clause = if offset.is_empty() {
            format!("{primary} — work out its correct UTC offset for each date yourself, accounting for daylight saving")
        } else {
            format!("{primary} (UTC offset {offset})")
        };
        let no_label_clause = if offset.is_empty() {
            format!("assume {primary} (using its correct offset for that date)")
        } else {
            format!("assume {primary} with offset {offset}")
        };
        format!(
            "- Timezone: this schedule's day/date headers are written in the primary timezone {zone_clause}.\n\
- A single stream may print its start time in SEVERAL timezones at once (e.g. 'PDT / JST / GMT'). Those are the SAME moment shown more than once — NOT separate streams. Emit exactly ONE event per stream block.\n\
- To fix the instant: prefer the time labelled {primary}, combined with that row's date header (read in {primary}). If {primary} is not among the printed labels, use any one printed timezone with its real UTC offset; never multiply events.\n\
- A '*' or 'next day' mark beside a converted time only means that conversion lands on the following calendar day — it does not change the instant. Resolve everything to one absolute UTC instant and let the offset carry the date.\n\
- If NO timezone is printed anywhere, {no_label_clause}. Never convert to my machine's local timezone.",
        )
    } else {
        format!(
            "- Timezone: use exactly what the banner prints; if none is shown, assume UTC offset {offset}.\n\
- A single stream may print its start time in SEVERAL timezones at once (e.g. 'PDT / JST / GMT'). Those are the SAME moment shown more than once — NOT separate streams. Emit exactly ONE event per stream block, fixing the instant from the first/topmost printed timezone (with its real UTC offset).\n\
- A '*' or 'next day' mark beside a converted time only means that conversion lands on the following calendar day — it does not change the instant.\n\
- Never convert to my machine's local timezone.",
        )
    };
    format!(
        "You are an OCR-to-JSON extractor. Read the streamer schedule in {image_path} and output an array of event objects.\n\
\n\
RULES:\n\
- Output ONLY raw JSON. No markdown, no code fences, no backticks, no commentary, no leading or trailing text. The first character of your reply must be '[' and the last must be ']'.\n\
{tz_rules}\n\
- The year is {year}.\n\
- Transcribe titles literally. 'w' or 'W' before a name means 'with' (a collaborator), e.g. 'FEARS TO FATHOM w CRELLY' -> title 'Fears to Fathom', collab 'Crelly'. Do not guess or 'correct' names.\n\
- Skip any card marked OFFLINE, marked as a non-stream note (e.g. 'podcasting', 'break', 'TBD'), or with an unknown date ('????').\n\
- If a time is vague (e.g. 'Evening'), set time and datetime to null but keep the raw text in time_label.\n\
\n\
Each object has these fields:\n\
- title (string)\n\
- collab (string or null)\n\
- date (YYYY-MM-DD, in the timezone you used for datetime)\n\
- day (weekday name)\n\
- time (HH:MM 24-hour in the timezone you used, or null)\n\
- time_label (raw time text from banner, e.g. '12.00 P.M.' or 'Evening')\n\
- timezone (IANA name of the timezone you used)\n\
- datetime (full ISO 8601 instant WITH its UTC offset, e.g. 2026-06-21T13:00:00-07:00, or null if no exact time)\n\
- source_image (set this to the filename: {image_path})",
        year = opts.year,
    )
}

/// Run the LLM CLI for one image+model using `--output-format json`.
/// Returns `(result_text, stats)` on success, `None` on any failure.
/// If the output isn't a recognisable JSON envelope (older CLI), falls back to
/// treating stdout as raw text with zeroed stats so the parse step still runs.
async fn run_cli(command: &str, model: &str, dir: &str, prompt: &str, opts: &OcrOpts) -> Option<(String, OcrCallStats)> {
    let timeout = Duration::from_secs(if opts.timeout_secs > 0 { opts.timeout_secs } else { DEFAULT_TIMEOUT_SECS });
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(["--model", model, "--add-dir", dir, "-p", prompt, "--output-format", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let budget = opts.max_budget_usd.trim();
    if !budget.is_empty() {
        cmd.args(["--max-budget-usd", budget]);
    }
    let effort = opts.effort.trim();
    if !effort.is_empty() {
        cmd.args(["--effort", effort]);
    }
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut log_flags = format!("--model {model}");
    if !budget.is_empty() { log_flags.push_str(&format!(" --max-budget-usd {budget}")); }
    if !effort.is_empty() { log_flags.push_str(&format!(" --effort {effort}")); }
    info!("OCR: invoking '{command}' {log_flags} (timeout {timeout:?}, dir {dir})");
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!("OCR: failed to spawn '{command}': {e} (is the CLI installed/on PATH?)");
            return None;
        }
    };
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            warn!("OCR: CLI wait failed: {e}");
            return None;
        }
        Err(_) => {
            warn!("OCR: CLI timed out after {}s", timeout.as_secs());
            return None;
        }
    };
    if !out.status.success() {
        warn!("OCR: CLI exited with {:?}", out.status.code());
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.is_empty() {
        return None;
    }
    // Try to parse the JSON envelope (`--output-format json`).
    // On parse failure (older CLI without that flag), treat stdout as raw OCR text
    // and report zero tokens/cost.
    match serde_json::from_str::<CliEnvelope>(&stdout) {
        Ok(env) if !env.result.is_empty() => {
            let stats = OcrCallStats {
                model: model.to_string(),
                input_tokens: env.usage.input_tokens,
                output_tokens: env.usage.output_tokens,
                cache_read_tokens: env.usage.cache_read_input_tokens,
                cache_creation_tokens: env.usage.cache_creation_input_tokens,
                cost_usd: env.total_cost_usd,
            };
            info!(
                "OCR: {model} → {} in / {} out / ${:.4} (cache r={} c={})",
                env.usage.input_tokens,
                env.usage.output_tokens,
                env.total_cost_usd,
                env.usage.cache_read_input_tokens,
                env.usage.cache_creation_input_tokens,
            );
            Some((env.result, stats))
        }
        _ => {
            // Envelope parse failed — either the CLI doesn't support --output-format json
            // or the response was an error object. Fall back to raw text with no stats.
            debug!("OCR: output wasn't a CLI JSON envelope; treating as raw text");
            Some((stdout, OcrCallStats { model: model.to_string(), ..Default::default() }))
        }
    }
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
    // A channel streams one thing at a time, so two events at the SAME instant are
    // the same stream printed twice — typically multi-timezone rows the model
    // failed to merge into one, sometimes with a divergent title (collab tagged on
    // only one row, casing differences). The exact-title dedup above misses those,
    // so collapse any remaining same-instant rows, keeping the most informative
    // (longest) title.
    let mut i = 0;
    while i + 1 < out.len() {
        if out[i].start_time == out[i + 1].start_time {
            if out[i + 1].title.chars().count() > out[i].title.chars().count() {
                out[i].title = std::mem::take(&mut out[i + 1].title);
            }
            out.remove(i + 1);
        } else {
            i += 1;
        }
    }
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
            max_budget_usd: String::new(),
            timeout_secs: 0,
            effort: String::new(),
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
    fn multi_timezone_one_stream_collapses_to_single_event() {
        // girl_dm_-style row: the SAME 1:00 PM PDT stream printed in three zones —
        // PDT, GMT, and JST (next calendar day, the '*' marker). They denote one
        // moment, so map_events must collapse them to a single event by instant —
        // never one event per timezone label.
        let raw = r#"[
          {"title":"Art stream","collab":null,"datetime":"2026-06-21T13:00:00-07:00"},
          {"title":"Art stream","collab":null,"datetime":"2026-06-21T20:00:00+00:00"},
          {"title":"Art stream","collab":null,"datetime":"2026-06-22T05:00:00+09:00"}
        ]"#;
        let events = parse_events(raw).unwrap();
        let segs = map_events(events, &opts());
        assert_eq!(segs.len(), 1, "three timezone printouts are one stream");
        assert_eq!(segs[0].title, "Art stream");
        // 2026-06-21T13:00-07:00 == 2026-06-21T20:00Z == unix 1782072000.
        assert_eq!(segs[0].start_time, 1782072000);
    }

    #[test]
    fn multi_timezone_divergent_titles_collapse() {
        // Same as the multi-tz case, but the model tagged the collab on only one
        // row and varied casing — so the three rows resolve to ONE instant yet
        // carry different composed titles. Exact-title dedup can't merge them;
        // the same-instant collapse must, keeping the most informative title.
        let raw = r#"[
          {"title":"Art stream","collab":null,"datetime":"2026-06-21T13:00:00-07:00"},
          {"title":"Art stream","collab":"Guest","datetime":"2026-06-21T20:00:00+00:00"},
          {"title":"art STREAM","collab":null,"datetime":"2026-06-22T05:00:00+09:00"}
        ]"#;
        let events = parse_events(raw).unwrap();
        let segs = map_events(events, &opts());
        assert_eq!(segs.len(), 1, "same instant => one stream, regardless of title");
        assert_eq!(segs[0].title, "Art stream w/ Guest", "keep the most informative title");
        assert_eq!(segs[0].start_time, 1782072000);
    }

    #[test]
    fn named_zone_without_offset_defers_offset_to_model() {
        // Primary timezone set, offset left blank: the prompt must NOT assert a
        // numeric "(UTC offset …)" for the named zone (that would be the machine's
        // offset, contradicting the zone) — it must tell the model to derive it.
        let mut o = opts();
        o.timezone = "America/Los_Angeles".into();
        o.offset = String::new();
        let p = build_prompt("img.png", &o);
        assert!(p.contains("work out its correct UTC offset"), "must defer offset to model");
        assert!(
            !p.contains("UTC offset +") && !p.contains("UTC offset -"),
            "must not pair the named zone with a numeric offset: {p}"
        );
    }

    #[test]
    fn named_zone_with_explicit_offset_states_it() {
        let mut o = opts();
        o.timezone = "America/Los_Angeles".into();
        o.offset = "-07:00".into();
        let p = build_prompt("img.png", &o);
        assert!(p.contains("America/Los_Angeles (UTC offset -07:00)"));
    }

    #[test]
    fn parse_rejects_non_array() {
        assert!(parse_events("not json at all").is_none());
        assert!(parse_events("{\"title\":\"x\"}").is_none());
    }
}
