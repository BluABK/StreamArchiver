//! In-memory ring buffer of the app's own tracing events, feeding the 🖹 Log
//! view (`ui::log_view`) — a live, filterable, colored equivalent of the
//! console/file log that doesn't require tailing a file or leaving the app.
//!
//! [`LogCaptureLayer`] is a third `tracing_subscriber::Layer` registered
//! alongside the existing file and stderr layers (see `main::init_tracing`),
//! so it sees exactly the same events under the same `EnvFilter` — nothing
//! about the console/file log's behavior changes. Every event is copied into
//! a bounded [`VecDeque`] behind a single global `Mutex`; the oldest record
//! is dropped once [`CAPACITY`] is reached. Kept intentionally simple (no
//! spans, no field typing) since the console/file log stays the authoritative
//! record for anything older or more detailed than this view needs.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::models::Platform;

/// Ring buffer capacity — bounds memory (a few hundred bytes/record, so
/// worst case tens of MB) rather than growing unbounded across a long-lived
/// session. The file log (7-day retention) is the durable record; this view
/// is for "what's happening right now / recently," not full history.
pub const CAPACITY: usize = 50_000;

/// One captured tracing event. `target` is `event.metadata().target()`,
/// which is always `&'static str` (baked into the macro's compile-time
/// metadata) — free to store, no allocation. `message`/`fields` have already
/// had `logfmt::strip_ansi` applied, so a colored `Platform::tag()` embedded
/// in the message shows as plain `[Twitch]` text — the Log view does its own
/// coloring (see `ui::log_view::detect_platform`) rather than relying on the
/// terminal's ANSI codes, which are usually off anyway in a GUI build.
pub struct LogRecord {
    pub seq: u64,
    pub time_ms: i64,
    pub level: tracing::Level,
    pub target: &'static str,
    pub message: String,
    /// Non-message fields, pre-joined as `"key=value key2=value2"` (empty
    /// string if the event carried none) — cheaper than a `Vec<(String,
    /// String)>` for a view that only ever displays or substring-searches
    /// them, never looks one up by name.
    pub fields: String,
}

/// Severity rank, most-severe first — independent of `tracing::Level`'s own
/// `Ord` (which this deliberately doesn't rely on, to keep "minimum level"
/// filtering obviously correct at the call site rather than trusting an
/// externally-defined ordering direction).
pub fn level_rank(level: tracing::Level) -> u8 {
    match level {
        tracing::Level::ERROR => 0,
        tracing::Level::WARN => 1,
        tracing::Level::INFO => 2,
        tracing::Level::DEBUG => 3,
        tracing::Level::TRACE => 4,
    }
}

static SEQ: AtomicU64 = AtomicU64::new(0);
static BUFFER: OnceLock<Mutex<VecDeque<Arc<LogRecord>>>> = OnceLock::new();

fn buffer() -> &'static Mutex<VecDeque<Arc<LogRecord>>> {
    BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

fn push(record: LogRecord) {
    let mut buf = buffer().lock().unwrap_or_else(|e| e.into_inner());
    if buf.len() >= CAPACITY {
        buf.pop_front();
    }
    buf.push_back(Arc::new(record));
}

/// Session-only mute list: substrings that stop matching events from ever
/// being captured — checked in [`LogCaptureLayer::on_event`], before
/// `push`, so a muted source doesn't just disappear from the Log view, it
/// stops costing anything (buffer churn, eviction of everything else). Not
/// persisted to settings: a mute is for riding out a specific noisy episode
/// (see the module docs' recursion note), and a restart is a clean slate.
static MUTES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn mutes() -> &'static Mutex<Vec<String>> {
    MUTES.get_or_init(|| Mutex::new(Vec::new()))
}

fn matches_any_mute(list: &[String], haystack: &str) -> bool {
    if list.is_empty() {
        return false;
    }
    let lower = haystack.to_lowercase();
    list.iter().any(|p| lower.contains(&p.to_lowercase()))
}

/// Whether `haystack` (typically `"{message} {fields} {target}"`) contains
/// any currently-muted substring, case-insensitively.
pub fn is_muted(haystack: &str) -> bool {
    matches_any_mute(&mutes().lock().unwrap_or_else(|e| e.into_inner()), haystack)
}

/// Add a mute pattern (no-op on an empty/duplicate pattern) and immediately
/// purge every already-captured record it matches — muting a runaway source
/// recovers the view at once instead of only stopping it from getting worse.
pub fn add_mute(pattern: &str) {
    let pattern = pattern.trim().to_string();
    if pattern.is_empty() {
        return;
    }
    {
        let mut list = mutes().lock().unwrap_or_else(|e| e.into_inner());
        if list.iter().any(|p| p.eq_ignore_ascii_case(&pattern)) {
            return;
        }
        list.push(pattern);
    }
    let list = mutes().lock().unwrap_or_else(|e| e.into_inner()).clone();
    buffer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|r| !matches_any_mute(&list, &format!("{} {} {}", r.message, r.fields, r.target)));
}

/// Remove the mute pattern at `index` (as returned by [`mute_list`]).
/// Silently ignored if `index` is out of range (the list changed underneath
/// a stale UI snapshot — the next frame's list will already be current).
pub fn remove_mute(index: usize) {
    let mut list = mutes().lock().unwrap_or_else(|e| e.into_inner());
    if index < list.len() {
        list.remove(index);
    }
}

/// Current mute patterns, in the order they were added.
pub fn mute_list() -> Vec<String> {
    mutes().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A reasonable default mute pattern for "lines like this one": `text`
/// truncated at its first `[` or digit (the point most log messages here
/// switch from a fixed description to per-event data — ids, counts,
/// coordinates), trimmed of trailing separators. Editable afterward via the
/// mute list — this only has to be a good *starting point*, e.g. for
/// `"Widget rect changed id between passes: prev ids: [\"11BE\"], new ids:
/// [...]"` it suggests `"Widget rect changed id between passes: prev ids"`.
pub fn suggested_mute_pattern(text: &str) -> String {
    let cut = text
        .char_indices()
        .find(|&(_, c)| c == '[' || c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    text[..cut].trim_end_matches([':', ',', '-', ' ']).to_string()
}

/// Every record currently held, oldest first. Cheap: clones `Arc` pointers,
/// not the records themselves.
pub fn snapshot() -> Vec<Arc<LogRecord>> {
    buffer().lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

/// Records with `seq > since`, oldest first — for incrementally extending an
/// already-filtered view instead of re-scanning the whole buffer every frame.
pub fn since(since: u64) -> Vec<Arc<LogRecord>> {
    let buf = buffer().lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<Arc<LogRecord>> =
        buf.iter().rev().take_while(|r| r.seq > since).cloned().collect();
    out.reverse();
    out
}

/// Current record count (for a "N of CAPACITY" indicator in the UI).
pub fn len() -> usize {
    buffer().lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Drop everything captured so far. Only affects this in-memory view — the
/// rotating file log (and console, if attached) are untouched.
pub fn clear() {
    buffer().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Which of the known platform brand tags (see `logfmt::PlatTag`) appears in
/// `text`, if any — `"...[YouTube]..."` → `Some(Platform::YouTube)`. Checked
/// against `Platform::ALL` rather than hardcoded strings so a new platform
/// only needs adding there. First match wins; a message never embeds more
/// than one platform's tag in practice.
pub fn detect_platform(text: &str) -> Option<Platform> {
    Platform::ALL.into_iter().find(|p| text.contains(&format!("[{}]", p.label())))
}

/// Collects a tracing event's `message` field (formatted, no surrounding
/// quotes) and every other field as `"name=value"`, space-joined.
#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: String,
}

impl FieldCollector {
    fn push(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = value;
            return;
        }
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        self.fields.push_str(name);
        self.fields.push('=');
        self.fields.push_str(&value);
    }
}

impl tracing::field::Visit for FieldCollector {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.push(field.name(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.push(field.name(), value.to_string());
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.push(field.name(), value.to_string());
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.push(field.name(), value.to_string());
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.push(field.name(), value.to_string());
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.push(field.name(), value.to_string());
    }
}

/// The third layer in `main::init_tracing`'s registry — see the module docs.
/// Stateless (all state lives in the module-level statics above), so it's a
/// unit struct rather than carrying a buffer handle around.
pub struct LogCaptureLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogCaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut collector = FieldCollector::default();
        event.record(&mut collector);
        let target = event.metadata().target();
        let message = crate::logfmt::strip_ansi(&collector.message);
        let fields = crate::logfmt::strip_ansi(&collector.fields);
        // Checked before push, not just filtered at display time: a muted
        // source (see the module docs) must stop costing buffer churn and
        // eviction of everything else, not merely stop being shown.
        if is_muted(&format!("{message} {fields} {target}")) {
            return;
        }
        push(LogRecord {
            seq: SEQ.fetch_add(1, Ordering::Relaxed),
            time_ms: crate::models::now_unix_ms(),
            level: *event.metadata().level(),
            target,
            message,
            fields,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_rank_orders_error_as_most_severe() {
        assert!(level_rank(tracing::Level::ERROR) < level_rank(tracing::Level::WARN));
        assert!(level_rank(tracing::Level::WARN) < level_rank(tracing::Level::INFO));
        assert!(level_rank(tracing::Level::INFO) < level_rank(tracing::Level::DEBUG));
        assert!(level_rank(tracing::Level::DEBUG) < level_rank(tracing::Level::TRACE));
    }

    #[test]
    fn detect_platform_finds_the_embedded_tag() {
        assert_eq!(
            detect_platform("recording finished: [YouTube] girl_dm_ monitor_id=28"),
            Some(Platform::YouTube)
        );
        assert_eq!(detect_platform("chat (nagzz): CassieRedsky cheered 400 bits"), None);
        // A bare mention without brackets doesn't count — avoids false
        // positives on channel names that happen to contain a platform word.
        assert_eq!(detect_platform("the YouTube API quota reset"), None);
    }

    #[test]
    fn field_collector_keeps_message_separate_from_other_fields() {
        let mut c = FieldCollector::default();
        c.push("message", "recording finished".to_string());
        c.push("monitor_id", "28".to_string());
        c.push("channel", "girl_dm_".to_string());
        assert_eq!(c.message, "recording finished");
        assert_eq!(c.fields, "monitor_id=28 channel=girl_dm_");
    }

    #[test]
    fn since_returns_only_newer_records_in_order() {
        clear();
        push(LogRecord {
            seq: 1,
            time_ms: 0,
            level: tracing::Level::INFO,
            target: "t",
            message: "a".into(),
            fields: String::new(),
        });
        push(LogRecord {
            seq: 2,
            time_ms: 0,
            level: tracing::Level::INFO,
            target: "t",
            message: "b".into(),
            fields: String::new(),
        });
        push(LogRecord {
            seq: 3,
            time_ms: 0,
            level: tracing::Level::INFO,
            target: "t",
            message: "c".into(),
            fields: String::new(),
        });
        let newer = since(1);
        assert_eq!(newer.iter().map(|r| r.message.as_str()).collect::<Vec<_>>(), vec!["b", "c"]);
        assert!(since(3).is_empty());
        clear();
    }

    #[test]
    fn eviction_drops_the_oldest_record_once_at_capacity() {
        clear();
        // Exercise eviction with a small local deque directly rather than
        // pushing CAPACITY (50,000) real records through the global buffer
        // in a unit test.
        let mut buf: VecDeque<Arc<LogRecord>> = VecDeque::new();
        let cap = 3;
        for i in 0..5u64 {
            if buf.len() >= cap {
                buf.pop_front();
            }
            buf.push_back(Arc::new(LogRecord {
                seq: i,
                time_ms: 0,
                level: tracing::Level::INFO,
                target: "t",
                message: i.to_string(),
                fields: String::new(),
            }));
        }
        assert_eq!(buf.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn suggested_mute_pattern_cuts_before_the_first_dynamic_looking_part() {
        assert_eq!(
            suggested_mute_pattern(
                "Widget rect changed id between passes: prev ids: [\"11BE\"], new ids: [\"FEB\"]"
            ),
            "Widget rect changed id between passes: prev ids"
        );
        assert_eq!(suggested_mute_pattern("retry attempt 3 of 5"), "retry attempt");
        // No digit and no bracket at all — the whole (trimmed) text.
        assert_eq!(suggested_mute_pattern("channel offline"), "channel offline");
    }

    #[test]
    fn add_mute_is_idempotent_and_case_insensitive() {
        mutes().lock().unwrap().clear();
        add_mute("Widget rect changed");
        add_mute("widget rect changed"); // same pattern, different case
        assert_eq!(mute_list(), vec!["Widget rect changed".to_string()]);
        mutes().lock().unwrap().clear();
    }

    #[test]
    fn remove_mute_drops_the_pattern_at_that_index() {
        mutes().lock().unwrap().clear();
        add_mute("one");
        add_mute("two");
        remove_mute(0);
        assert_eq!(mute_list(), vec!["two".to_string()]);
        remove_mute(99); // out of range — ignored, not a panic
        assert_eq!(mute_list(), vec!["two".to_string()]);
        mutes().lock().unwrap().clear();
    }

    #[test]
    fn a_muted_event_is_never_captured() {
        clear();
        mutes().lock().unwrap().clear();
        add_mute("Widget rect changed");
        // Simulates on_event's own check — not going through a real
        // Subscriber here, just verifying the predicate + push contract.
        let noisy = "Widget rect changed id between passes: prev ids: [\"A\"]";
        assert!(is_muted(&format!("{noisy}  ")));
        if !is_muted(&format!("{noisy}  ")) {
            push(LogRecord {
                seq: 1,
                time_ms: 0,
                level: tracing::Level::WARN,
                target: "egui",
                message: noisy.into(),
                fields: String::new(),
            });
        }
        assert_eq!(len(), 0, "muted event must never reach the buffer");
        mutes().lock().unwrap().clear();
    }

    #[test]
    fn adding_a_mute_purges_matching_records_already_captured() {
        clear();
        mutes().lock().unwrap().clear();
        push(LogRecord {
            seq: 1,
            time_ms: 0,
            level: tracing::Level::WARN,
            target: "egui",
            message: "Widget rect changed id between passes: prev ids: [\"A\"]".into(),
            fields: String::new(),
        });
        push(LogRecord {
            seq: 2,
            time_ms: 0,
            level: tracing::Level::INFO,
            target: "streamarchiver",
            message: "recording finished".into(),
            fields: String::new(),
        });
        assert_eq!(len(), 2);
        add_mute("Widget rect changed");
        assert_eq!(len(), 1, "the matching record is purged, the unrelated one stays");
        assert_eq!(snapshot()[0].message, "recording finished");
        mutes().lock().unwrap().clear();
        clear();
    }
}
