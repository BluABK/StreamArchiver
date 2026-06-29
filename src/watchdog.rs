//! UI-freeze watchdog + panic dialog (Windows-focused, compile-safe elsewhere).
//!
//! Two safety nets that turn a *silent* GUI hang or a process-killing panic into
//! a *visible* native dialog:
//!
//! 1. [`install_panic_dialog`] — a `std::panic::set_hook` that pops a native
//!    `MessageBox` with the panic message + location before the default hook
//!    runs (and, in release where `panic = "abort"`, before the process aborts).
//!
//! 2. [`Heartbeat`] + [`start_watchdog`] — the UI thread stamps a monotonic
//!    timestamp every frame ([`Heartbeat::beat`]) and an optional coarse activity
//!    label ([`Heartbeat::set_activity`]) before risky sections (e.g. emote GPU
//!    upload). A dedicated background thread wakes every second; if the last beat
//!    is older than the threshold while the app is meant to be rendering, it shows
//!    a native dialog **off the UI thread** (the UI thread is blocked, so it can't
//!    show anything itself). Debounced so it fires once per hang.
//!
//! Caveat: a dialog cannot *un-hang* the UI thread — that thread is stuck inside
//! some blocking call. The watchdog only *informs* and lets the user decide. See
//! [`start_watchdog`] for the wait-vs-kill policy.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Coarse "what was the UI thread doing" phases, stamped via [`Heartbeat::set_activity`]
/// right before a risky section so a freeze dialog can name the likely culprit.
///
/// `#[repr(u8)]` so the value round-trips through the `AtomicU8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Activity {
    Idle = 0,
    Frame = 1,
    Properties = 2,
    EmoteViewerGrid = 3,
    EmoteDecodePump = 4,
    Chat = 5,
    /// First-open work of the channel Properties window *before* its sub-window is
    /// created. The heavy load (image decode/upload, asset enumeration, schedule-source DB
    /// reads) now runs on a background `props-load` thread, so under this label the UI
    /// thread only does cheap in-memory work (platform list, cache lookups); a freeze here
    /// would mean a cache miss fell back to an inline load. Split from [`Activity::Properties`]
    /// (which covers the sub-window/placeholder build + paint) so the dialog still
    /// distinguishes pre-window work from the window itself.
    PropertiesLoad = 6,
}

impl Activity {
    fn from_u8(v: u8) -> Activity {
        match v {
            1 => Activity::Frame,
            2 => Activity::Properties,
            3 => Activity::EmoteViewerGrid,
            4 => Activity::EmoteDecodePump,
            5 => Activity::Chat,
            6 => Activity::PropertiesLoad,
            _ => Activity::Idle,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Activity::Idle => "idle",
            Activity::Frame => "rendering a frame",
            Activity::Properties => "drawing the channel properties window",
            Activity::EmoteViewerGrid => "drawing the emote viewer",
            Activity::EmoteDecodePump => "decoding / uploading emote images",
            Activity::Chat => "drawing the chat replay popup",
            Activity::PropertiesLoad => "loading the channel properties assets",
        }
    }
}

/// Shared, cheap, lock-free heartbeat. Clone freely (it's an `Arc` inside).
#[derive(Clone)]
pub struct Heartbeat {
    inner: Arc<Inner>,
}

struct Inner {
    /// Millis since `start` of the last [`beat`]. Read by the watchdog thread.
    last_beat_ms: AtomicU64,
    /// Current [`Activity`] phase as `u8`.
    activity: AtomicU8,
    /// True while the app wants to be actively rendering. When false (minimised
    /// to tray, shutting down) a stale heartbeat is expected and must NOT alarm.
    active: AtomicBool,
    /// Monotonic origin so we only ever store small `u64` millis (no `Instant`
    /// in an atomic, which isn't possible).
    start: Instant,
}

impl Heartbeat {
    /// Create a heartbeat that is immediately "fresh" and marked active.
    pub fn new() -> Heartbeat {
        let start = Instant::now();
        let hb = Heartbeat {
            inner: Arc::new(Inner {
                last_beat_ms: AtomicU64::new(0),
                activity: AtomicU8::new(Activity::Idle as u8),
                active: AtomicBool::new(true),
                start,
            }),
        };
        hb.beat();
        hb
    }

    /// Stamp "the UI thread is alive right now". Call at the very top of the
    /// per-frame `logic()` AND at the end of `ui()` so the alive-window spans the
    /// whole frame (including painting, where emote uploads happen).
    #[inline]
    pub fn beat(&self) {
        let ms = self.inner.start.elapsed().as_millis() as u64;
        self.inner.last_beat_ms.store(ms, Ordering::Relaxed);
    }

    /// Mark the coarse activity phase. Set it right before a risky section and
    /// (optionally) reset to [`Activity::Frame`] after. It's just a hint for the
    /// dialog text; a stale value is harmless.
    #[inline]
    pub fn set_activity(&self, a: Activity) {
        self.inner.activity.store(a as u8, Ordering::Relaxed);
    }

    /// Toggle whether the UI is supposed to be rendering. Set `false` when hidden
    /// to the tray or when quitting, so an idle (legitimately stalled) UI thread
    /// doesn't trip the alarm; set `true` when shown again.
    #[inline]
    pub fn set_active(&self, active: bool) {
        self.inner.active.store(active, Ordering::Relaxed);
    }

    fn age(&self) -> Duration {
        let now = self.inner.start.elapsed().as_millis() as u64;
        let last = self.inner.last_beat_ms.load(Ordering::Relaxed);
        Duration::from_millis(now.saturating_sub(last))
    }

    fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::Relaxed)
    }

    fn activity(&self) -> Activity {
        Activity::from_u8(self.inner.activity.load(Ordering::Relaxed))
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the watchdog thread (call ONCE at startup). It owns a clone of the
/// heartbeat and never touches egui/the UI thread. `threshold` is how stale the
/// heartbeat must get before we declare a hang (8–10s is a good value: long
/// enough that a slow-but-progressing frame won't false-positive, short enough
/// to beat Windows' own "Not Responding" ghosting).
///
/// Policy note (`exit_after_dialog`):
/// - `false` (recommended default): don't *auto*-kill. The dialog still offers a
///   **Force quit** button so the user can exit a wedged UI without Task Manager;
///   choosing **Keep waiting** lets a transient stall recover, after which we re-arm
///   (and re-offer the escape hatch on a still-frozen UI once the cooldown passes).
/// - `true`: force `std::process::exit(101)` once the dialog is dismissed, regardless
///   of which button was chosen. Use only if a frozen UI is unrecoverable for your app
///   and you'd rather guarantee the process dies than leave a zombie window — choosing
///   **Force quit** in the dialog already exits, so this only adds exit-on-Keep-waiting.
///   Because downloads run in detached
///   child processes (per this project's design) and recordings survive app exit,
///   killing the UI is relatively safe here — but it's still a policy choice, so it's
///   off by default.
pub fn start_watchdog(hb: Heartbeat, threshold: Duration, exit_after_dialog: bool) {
    /// After the user picks "Keep waiting", stay quiet this long before offering the
    /// escape hatch again on a *still*-frozen UI — long enough not to spam dialogs,
    /// short enough that a persistent hang keeps offering a way out.
    const REPROMPT_COOLDOWN: Duration = Duration::from_secs(20);
    std::thread::Builder::new()
        .name("ui-watchdog".into())
        .spawn(move || {
            // When we last showed (and the user dismissed) a hang dialog. `None` =
            // armed: a hang may prompt immediately. After a "Keep waiting" we hold
            // off for `REPROMPT_COOLDOWN`; once the UI recovers we re-arm at once.
            let mut last_dialog: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let age = hb.age();
                let active = hb.is_active();

                if active && age >= threshold {
                    let cooled = last_dialog.is_none_or(|t| t.elapsed() >= REPROMPT_COOLDOWN);
                    if cooled {
                        let secs = age.as_secs();
                        let doing = hb.activity().label();
                        let msg = format!(
                            "StreamArchiver's UI thread has stopped responding.\n\n\
                             Last UI heartbeat: {secs}s ago\n\
                             Last known activity: {doing}\n\n\
                             The window is frozen. Choose \"Keep waiting\" to give it more \
                             time to recover, or \"Force quit\" to close the app now.\n\n\
                             Background recordings and downloads are NOT affected — they run \
                             in separate processes and keep going, so force-quitting is safe.",
                        );
                        // Blocks THIS (watchdog) thread until dismissed — fine, it's not
                        // the UI thread. MessageBoxW with a null parent is safe to call
                        // from any thread (see module docs).
                        let force_quit =
                            show_hang_dialog("StreamArchiver — UI frozen", &msg);
                        if force_quit || exit_after_dialog {
                            // The frozen UI thread can't tear itself down, so exit the
                            // whole process. Buffered logs are already on disk via
                            // tracing_appender; 101 mirrors the Rust panic/abort code.
                            std::process::exit(101);
                        }
                        // "Keep waiting": measure the cooldown from dismissal so a still
                        // -frozen UI re-prompts later instead of going silent forever.
                        last_dialog = Some(Instant::now());
                    }
                } else if age < threshold {
                    // UI is alive again (or went inactive): re-arm for the next hang.
                    last_dialog = None;
                }
            }
        })
        .expect("spawn ui-watchdog thread");
}

/// Install a panic hook that shows a native dialog with the panic message +
/// location, THEN chains to the previous hook (which prints the backtrace and,
/// under `panic = "abort"`, lets the runtime abort the process).
///
/// Why this works under `panic = "abort"` (the release profile here): `abort`
/// changes *unwinding* behaviour, not hooks. The panic runtime still invokes the
/// registered hook, and only *after the hook returns* does it call `abort()`.
/// So the dialog is shown before the process dies. In debug (`panic = "unwind"`)
/// the hook runs too, then unwinding proceeds as normal.
pub fn install_panic_dialog() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Extract a human message from the panic payload.
        let payload = info.payload();
        let msg: &str = if let Some(s) = payload.downcast_ref::<&str>() {
            s
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.as_str()
        } else {
            "Box<dyn Any>"
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let thread = std::thread::current();
        let tname = thread.name().unwrap_or("<unnamed>");

        let dialog = format!(
            "StreamArchiver crashed (panic).\n\n\
             Thread: {tname}\n\
             Location: {location}\n\n\
             {msg}\n\n\
             The application will now close. See the log file under the data \
             directory's logs\\ folder for details.",
        );
        // Error-level box, single OK button. Null parent => callable from the
        // panicking thread (which may not be the UI thread).
        show_blocking_dialog("StreamArchiver crashed", &dialog, false);

        // Chain to the default hook so the backtrace still hits stderr / the log.
        previous(info);
    }));
}

// ── native dialog shim ────────────────────────────────────────────────────────

/// Show a blocking native message box. Uses rfd (which on Windows, without the
/// `common-controls-v6` feature, calls Win32 `MessageBoxW` with a null parent —
/// callable from ANY thread). `warning` picks the icon level.
///
/// On non-Windows this still works (rfd uses the platform backend); the project
/// is Windows-focused but this keeps the module portable/compile-safe.
fn show_blocking_dialog(title: &str, body: &str, warning: bool) {
    let level = if warning {
        rfd::MessageLevel::Warning
    } else {
        rfd::MessageLevel::Error
    };
    // No `.set_parent(..)`: we deliberately use a null owner window so the call
    // does not depend on (and cannot deadlock against) the frozen UI HWND.
    let _ = rfd::MessageDialog::new()
        .set_level(level)
        .set_title(title)
        .set_buttons(rfd::MessageButtons::Ok)
        .set_description(body)
        .show();
}

/// Like [`show_blocking_dialog`] but for the UI-freeze case: two buttons,
/// **Force quit** and **Keep waiting**. Returns `true` when the user chose to
/// force-quit. A plain OK can't *un-hang* the UI thread, so this gives the user a
/// real escape hatch (safe here — recordings/downloads run in detached processes).
///
/// Null owner window (no `.set_parent`) so the call cannot deadlock against the
/// frozen UI HWND; runs on the watchdog thread, never the UI thread.
fn show_hang_dialog(title: &str, body: &str) -> bool {
    let result = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title(title)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            "Force quit".to_owned(),
            "Keep waiting".to_owned(),
        ))
        .set_description(body)
        .show();
    matches!(result, rfd::MessageDialogResult::Custom(s) if s == "Force quit")
}
