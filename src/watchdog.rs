//! UI-freeze watchdog + panic dialog (Windows-focused, compile-safe elsewhere).
//!
//! Two safety nets that turn a *silent* GUI hang or a process-killing panic into
//! a *visible* native dialog:
//!
//! 1. [`install_panic_dialog`] — a `std::panic::set_hook` that pops a native
//!    `TaskDialog` with the panic message + location + scrollable stack trace /
//!    environment info before the default hook runs.
//!
//! 2. [`Heartbeat`] + [`start_watchdog`] — the UI thread stamps a monotonic
//!    timestamp every frame ([`Heartbeat::beat`]) and an optional coarse activity
//!    label ([`Heartbeat::set_activity`]) before risky sections. A dedicated
//!    background thread wakes every second; if the last beat is older than the
//!    threshold while the app is meant to be rendering, it shows a native dialog
//!    **off the UI thread**. Debounced so it fires once per hang.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Coarse "what was the UI thread doing" phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Activity {
    Idle = 0,
    Frame = 1,
    Properties = 2,
    EmoteViewerGrid = 3,
    EmoteDecodePump = 4,
    Chat = 5,
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
    last_beat_ms: AtomicU64,
    activity: AtomicU8,
    active: AtomicBool,
    start: Instant,
}

impl Heartbeat {
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

    #[inline]
    pub fn beat(&self) {
        let ms = self.inner.start.elapsed().as_millis() as u64;
        self.inner.last_beat_ms.store(ms, Ordering::Relaxed);
    }

    #[inline]
    pub fn set_activity(&self, a: Activity) {
        self.inner.activity.store(a as u8, Ordering::Relaxed);
    }

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

/// Spawn the watchdog thread (call ONCE at startup).
pub fn start_watchdog(hb: Heartbeat, threshold: Duration, exit_after_dialog: bool) {
    const REPROMPT_COOLDOWN: Duration = Duration::from_secs(20);
    std::thread::Builder::new()
        .name("ui-watchdog".into())
        .spawn(move || {
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
                        let summary = format!(
                            "StreamArchiver's UI thread has stopped responding.\n\n\
                             Last UI heartbeat:   {secs}s ago\n\
                             Last known activity: {doing}\n\n\
                             Choose \"Keep waiting\" to give it more time, or \
                             \"Force quit\" to close the app now.\n\n\
                             Background recordings are NOT affected — they run \
                             in separate processes.",
                        );
                        let detail = build_hang_detail(secs, doing);
                        let log_dir = crate::app_paths::logs_dir()
                            .to_string_lossy()
                            .into_owned();
                        let force_quit = show_detail_dialog(
                            "StreamArchiver — UI frozen",
                            "StreamArchiver — UI frozen",
                            &summary,
                            &detail,
                            &log_dir,
                            true,
                        );
                        if force_quit || exit_after_dialog {
                            std::process::exit(101);
                        }
                        last_dialog = Some(Instant::now());
                    }
                } else if age < threshold {
                    last_dialog = None;
                }
            }
        })
        .expect("spawn ui-watchdog thread");
}

/// Install a panic hook that shows a native TaskDialog with scrollable details
/// (panic message, location, thread, stack trace, environment), then chains to
/// the previous hook so the backtrace still hits stderr / the log.
pub fn install_panic_dialog() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
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

        let summary = format!(
            "StreamArchiver crashed (panic).\n\n\
             Thread:   {tname}\n\
             Location: {location}\n\n\
             {msg}\n\n\
             The application will now close.",
        );
        let detail = build_panic_detail(msg, tname, &location);
        let log_dir = crate::app_paths::logs_dir()
            .to_string_lossy()
            .into_owned();

        show_detail_dialog(
            "StreamArchiver crashed",
            "StreamArchiver crashed (panic)",
            &summary,
            &detail,
            &log_dir,
            false,
        );

        previous(info);
    }));
}

// ── detail text builders ──────────────────────────────────────────────────────

fn build_panic_detail(msg: &str, thread: &str, location: &str) -> String {
    let backtrace = std::backtrace::Backtrace::force_capture();
    let backtrace_str = backtrace.to_string();
    let backtrace_note = if backtrace_str.contains("<unknown>") || backtrace_str.trim().is_empty() {
        " (symbols stripped — release build shows raw addresses only)"
    } else {
        ""
    };

    let pid = std::process::id();
    let version = env!("CARGO_PKG_VERSION");
    let os = os_info_string();
    let log_dir = crate::app_paths::logs_dir().to_string_lossy().into_owned();
    let threads = enumerate_threads_text(pid);

    format!(
        "=== Panic ===\n\
         Message:  {msg}\n\
         Thread:   {thread}\n\
         Location: {location}\n\
         PID:      {pid}\n\n\
         === Environment ===\n\
         App version: {version}\n\
         OS:          {os}\n\
         Log dir:     {log_dir}\n\n\
         === Live threads ===\n\
         {threads}\n\n\
         === Stack trace{backtrace_note} ===\n\
         {backtrace_str}"
    )
}

fn build_hang_detail(frozen_secs: u64, activity: &str) -> String {
    let pid = std::process::id();
    let version = env!("CARGO_PKG_VERSION");
    let os = os_info_string();
    let log_dir = crate::app_paths::logs_dir().to_string_lossy().into_owned();
    let threads = enumerate_threads_text(pid);

    format!(
        "=== Freeze Diagnostics ===\n\
         Frozen for:    {frozen_secs}s\n\
         Last activity: {activity}\n\
         PID:           {pid}\n\n\
         === Environment ===\n\
         App version: {version}\n\
         OS:          {os}\n\
         Log dir:     {log_dir}\n\n\
         === Live threads ===\n\
         {threads}"
    )
}

fn os_info_string() -> String {
    // Use PowerShell to get a human-readable OS string without wrestling with
    // Win32 registry types; runs fast enough that it's imperceptible in a crash.
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_OperatingSystem).Caption + ' ' + \
             (Get-CimInstance Win32_OperatingSystem).OSArchitecture + \
             ' build ' + (Get-CimInstance Win32_OperatingSystem).BuildNumber",
        ])
        .output();
    if let Ok(o) = out {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_owned();
        if !s.is_empty() {
            return s;
        }
    }
    format!("Windows ({})", std::env::consts::ARCH)
}

fn enumerate_threads_text(pid: u32) -> String {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next,
            THREADENTRY32, TH32CS_SNAPTHREAD,
        };

        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
                return "(could not snapshot threads)".to_owned();
            };
            let mut te = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut lines: Vec<String> = Vec::new();
            if Thread32First(snap, &mut te).is_ok() {
                loop {
                    if te.th32OwnerProcessID == pid {
                        lines.push(format!(
                            "  TID {:6}  base-priority {}",
                            te.th32ThreadID, te.tpBasePri
                        ));
                    }
                    te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
                    if Thread32Next(snap, &mut te).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            if lines.is_empty() {
                "(no threads found)".to_owned()
            } else {
                lines.join("\n")
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = pid;
        "(not available on this platform)".to_owned()
    }
}

// ── TaskDialogIndirect-based dialog ───────────────────────────────────────────

/// Show a rich crash/hang dialog with a scrollable detail pane.
///
/// `warning` selects the warning vs error icon and adjusts the button set:
/// - panic (false): "Close" + "Copy details" + "Open log folder"
/// - hang  (true):  "Force quit" + "Keep waiting" + "Copy details" + "Open log folder"
///
/// Returns `true` only when the user chose **Force quit** (hang path).
fn show_detail_dialog(
    title: &str,
    heading: &str,
    summary: &str,
    detail: &str,
    log_dir: &str,
    warning: bool,
) -> bool {
    #[cfg(target_os = "windows")]
    return show_task_dialog_win32(title, heading, summary, detail, log_dir, warning);

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (heading, detail, log_dir, warning);
        let _ = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title(title)
            .set_buttons(rfd::MessageButtons::Ok)
            .set_description(summary)
            .show();
        false
    }
}

const BTN_FORCE_QUIT: i32 = 1001;
const BTN_COPY: i32 = 1002;
const BTN_OPEN_LOGS: i32 = 1003;

#[cfg(target_os = "windows")]
fn show_task_dialog_win32(
    title: &str,
    heading: &str,
    summary: &str,
    detail: &str,
    log_dir: &str,
    warning: bool,
) -> bool {
    use windows::Win32::Foundation::{HWND, HINSTANCE};
    use windows::Win32::UI::Controls::{
        TaskDialogIndirect, TASKDIALOG_BUTTON, TASKDIALOGCONFIG,
        TASKDIALOGCONFIG_0, TASKDIALOGCONFIG_1,
        TDF_ALLOW_DIALOG_CANCELLATION, TDF_SIZE_TO_CONTENT,
        TDCBF_CLOSE_BUTTON, TD_ERROR_ICON, TD_WARNING_ICON,
    };
    use windows::Win32::UI::WindowsAndMessaging::IDCANCEL;
    use windows::core::PCWSTR;

    let w = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };

    let title_w = w(title);
    let heading_w = w(heading);
    let summary_w = w(summary);
    let detail_w = w(detail);
    let footer_w = w(&format!("Log directory: {log_dir}"));
    let expand_w = w("Show details");
    let collapse_w = w("Hide details");

    let lbl_force = w("Force quit");
    let lbl_wait = w("Keep waiting");
    let lbl_close = w("Close");
    let lbl_copy = w("Copy details");
    let lbl_logs = w("Open log folder");

    let icon = if warning { TD_WARNING_ICON } else { TD_ERROR_ICON };

    // Re-show after "Copy" / "Open log folder" so the user can still dismiss.
    loop {
        let mut buttons: Vec<TASKDIALOG_BUTTON> = Vec::new();
        if warning {
            buttons.push(TASKDIALOG_BUTTON {
                nButtonID: BTN_FORCE_QUIT,
                pszButtonText: PCWSTR(lbl_force.as_ptr()),
            });
            buttons.push(TASKDIALOG_BUTTON {
                nButtonID: IDCANCEL.0 as i32,
                pszButtonText: PCWSTR(lbl_wait.as_ptr()),
            });
        } else {
            buttons.push(TASKDIALOG_BUTTON {
                nButtonID: IDCANCEL.0 as i32,
                pszButtonText: PCWSTR(lbl_close.as_ptr()),
            });
        }
        buttons.push(TASKDIALOG_BUTTON {
            nButtonID: BTN_COPY,
            pszButtonText: PCWSTR(lbl_copy.as_ptr()),
        });
        buttons.push(TASKDIALOG_BUTTON {
            nButtonID: BTN_OPEN_LOGS,
            pszButtonText: PCWSTR(lbl_logs.as_ptr()),
        });

        let config = TASKDIALOGCONFIG {
            cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
            hwndParent: HWND(std::ptr::null_mut()),
            hInstance: HINSTANCE(std::ptr::null_mut()),
            dwFlags: TDF_SIZE_TO_CONTENT | TDF_ALLOW_DIALOG_CANCELLATION,
            dwCommonButtons: TDCBF_CLOSE_BUTTON,
            pszWindowTitle: PCWSTR(title_w.as_ptr()),
            Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: icon },
            pszMainInstruction: PCWSTR(heading_w.as_ptr()),
            pszContent: PCWSTR(summary_w.as_ptr()),
            cButtons: buttons.len() as u32,
            pButtons: buttons.as_ptr(),
            nDefaultButton: IDCANCEL.0 as i32,
            cRadioButtons: 0,
            pRadioButtons: std::ptr::null(),
            nDefaultRadioButton: 0,
            pszVerificationText: PCWSTR::null(),
            pszExpandedInformation: PCWSTR(detail_w.as_ptr()),
            pszExpandedControlText: PCWSTR(collapse_w.as_ptr()),
            pszCollapsedControlText: PCWSTR(expand_w.as_ptr()),
            Anonymous2: TASKDIALOGCONFIG_1 {
                hFooterIcon: windows::Win32::UI::WindowsAndMessaging::HICON(std::ptr::null_mut()),
            },
            pszFooter: PCWSTR(footer_w.as_ptr()),
            pfCallback: None,
            lpCallbackData: 0,
            cxWidth: 0,
        };

        let mut btn: i32 = IDCANCEL.0 as i32;
        let ok = unsafe { TaskDialogIndirect(&config, Some(&mut btn), None, None) };

        if ok.is_err() {
            // TaskDialogIndirect unavailable; fall back to rfd.
            let level = if warning {
                rfd::MessageLevel::Warning
            } else {
                rfd::MessageLevel::Error
            };
            let _ = rfd::MessageDialog::new()
                .set_level(level)
                .set_title(title)
                .set_buttons(rfd::MessageButtons::Ok)
                .set_description(summary)
                .show();
            return false;
        }

        match btn {
            BTN_COPY => {
                copy_to_clipboard(detail);
                // Loop: re-show so the user can still dismiss.
            }
            BTN_OPEN_LOGS => {
                let _ = std::process::Command::new("explorer").arg(log_dir).spawn();
                // Loop: re-show so the user can still dismiss.
            }
            BTN_FORCE_QUIT => return true,
            _ => return false, // "Close" / "Keep waiting" / cancel (X button)
        }
    }
}

/// Copy `text` to the system clipboard via PowerShell (avoids wrestling with
/// Win32 memory handle types across crate versions).
fn copy_to_clipboard(text: &str) {
    // Write to a temp file so we don't need to escape anything for PowerShell.
    let tmp = std::env::temp_dir().join("streamarchiver_crash_detail.txt");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "Get-Content -Raw '{}' | Set-Clipboard",
                    tmp.display()
                ),
            ])
            .output();
    }
}
