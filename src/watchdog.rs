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

use std::sync::{Arc, Mutex};
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
    /// Free-form context label set by the UI thread alongside the activity
    /// (e.g. "Channel: MoriCalliope"). Shown in freeze reports.
    context: Mutex<String>,
    /// Thread ID of the UI thread captured at heartbeat creation. Used by the
    /// watchdog to suspend and read the UI thread's stack on freeze.
    ui_tid: u32,
    start: Instant,
}

impl Heartbeat {
    pub fn new() -> Heartbeat {
        let start = Instant::now();
        let ui_tid = current_thread_id();
        let hb = Heartbeat {
            inner: Arc::new(Inner {
                last_beat_ms: AtomicU64::new(0),
                activity: AtomicU8::new(Activity::Idle as u8),
                active: AtomicBool::new(true),
                context: Mutex::new(String::new()),
                ui_tid,
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

    /// Set a human-readable context label alongside the activity (e.g. which
    /// channel window is open). Shown in freeze reports to identify the instance.
    pub fn set_context(&self, ctx: impl Into<String>) {
        if let Ok(mut g) = self.inner.context.lock() {
            *g = ctx.into();
        }
    }

    /// Clear the context label (call when leaving a context-tagged section).
    #[allow(dead_code)]
    pub fn clear_context(&self) {
        if let Ok(mut g) = self.inner.context.lock() {
            g.clear();
        }
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

    fn context(&self) -> String {
        self.inner.context.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn ui_tid(&self) -> u32 {
        self.inner.ui_tid
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

fn current_thread_id() -> u32 {
    #[cfg(target_os = "windows")]
    unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
    #[cfg(not(target_os = "windows"))]
    { 0u32 }
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
                        let ctx = hb.context();
                        let summary = format!(
                            "StreamArchiver's UI thread has stopped responding.\n\n\
                             Last UI heartbeat:   {secs}s ago\n\
                             Last known activity: {doing}{}\n\n\
                             Choose \"Keep waiting\" to give it more time, or \
                             \"Force quit\" to close the app now.\n\n\
                             Background recordings are NOT affected — they run \
                             in separate processes.",
                            if ctx.is_empty() {
                                String::new()
                            } else {
                                format!("\n  Context:             {ctx}")
                            },
                        );
                        let detail = build_hang_detail(&hb, secs, doing);
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
                            Some((&hb, threshold)),
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
            None,
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
    let (thread_count, named_threads) = enumerate_threads_text(pid);

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
         === Live threads ({thread_count} total; named) ===\n\
         {named_threads}\n\n\
         === Stack trace{backtrace_note} ===\n\
         {backtrace_str}"
    )
}

fn build_hang_detail(hb: &Heartbeat, frozen_secs: u64, activity: &str) -> String {
    let pid = std::process::id();
    let ui_tid = hb.ui_tid();
    let ctx = hb.context();
    let version = env!("CARGO_PKG_VERSION");
    let os = os_info_string();
    let log_dir = crate::app_paths::logs_dir().to_string_lossy().into_owned();
    let (thread_count, named_threads) = enumerate_threads_text(pid);
    let stack = capture_ui_thread_stack(ui_tid);

    let context_line = if ctx.is_empty() {
        String::new()
    } else {
        format!("Context:       {ctx}\n")
    };

    format!(
        "=== Freeze Diagnostics ===\n\
         Frozen for:    {frozen_secs}s\n\
         Last activity: {activity}\n\
         {context_line}\
         PID:           {pid}\n\
         UI thread TID: {ui_tid}\n\n\
         === Environment ===\n\
         App version: {version}\n\
         OS:          {os}\n\
         Log dir:     {log_dir}\n\n\
         === Live threads ({thread_count} total; named) ===\n\
         {named_threads}\n\n\
         === UI thread stack (TID {ui_tid}) ===\n\
         {stack}"
    )
}

fn os_info_string() -> String {
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

/// Enumerate threads in this process. Returns (total_count, named_threads_text).
/// Uses GetThreadDescription (Win 10 1607+) to show only threads with names,
/// which is far more useful than a wall of TID+priority numbers.
fn enumerate_threads_text(pid: u32) -> (usize, String) {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next,
            THREADENTRY32, TH32CS_SNAPTHREAD,
        };
        use windows::Win32::System::Threading::{
            GetThreadDescription, OpenThread, THREAD_QUERY_INFORMATION,
        };

        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
                return (0, "(could not snapshot threads)".to_owned());
            };
            let mut te = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut total = 0usize;
            let mut named: Vec<String> = Vec::new();

            if Thread32First(snap, &mut te).is_ok() {
                loop {
                    if te.th32OwnerProcessID == pid {
                        total += 1;
                        // Try to get the thread's name via Win10 1607+ API.
                        if let Ok(thandle) = OpenThread(
                            THREAD_QUERY_INFORMATION,
                            false,
                            te.th32ThreadID,
                        ) {
                            // GetThreadDescription returns Result<PWSTR> in windows 0.62.
                            if let Ok(desc) = GetThreadDescription(thandle) {
                                if !desc.is_null() {
                                    let ptr = desc.0;
                                    let len = (0..)
                                        .take_while(|&i| *ptr.add(i) != 0)
                                        .count();
                                    if len > 0 {
                                        let slice = std::slice::from_raw_parts(ptr, len);
                                        let name = String::from_utf16_lossy(slice);
                                        named.push(format!(
                                            "  TID {:6}  {}",
                                            te.th32ThreadID, name
                                        ));
                                    }
                                    // Intentionally not freeing desc — acceptable in
                                    // a crash/freeze handler that runs at most once.
                                }
                            }
                            let _ = CloseHandle(thandle);
                        }
                    }
                    te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
                    if Thread32Next(snap, &mut te).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);

            let text = if named.is_empty() {
                "(no named threads — set thread names via Builder::name())".to_owned()
            } else {
                named.join("\n")
            };
            (total, text)
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = pid;
        (0, "(not available on this platform)".to_owned())
    }
}

/// Capture the UI thread's call stack by suspending it, reading its CONTEXT,
/// then walking frames with StackWalk64 and resolving symbols with SymFromAddr.
/// Returns a formatted multi-line string. Fails gracefully on any error.
fn capture_ui_thread_stack(tid: u32) -> String {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::AtomicBool;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::Debug::{
            GetThreadContext, StackWalk64, SymFromAddr, SymFunctionTableAccess64,
            SymGetModuleBase64, SymInitialize, SymSetOptions, ADDRESS_MODE,
            CONTEXT, CONTEXT_FLAGS, STACKFRAME64, SYMBOL_INFO,
            SYMOPT_DEFERRED_LOADS, SYMOPT_UNDNAME,
        };
        use windows::Win32::System::Threading::{
            GetCurrentProcess, OpenThread, ResumeThread, SuspendThread,
            THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
        };

        // AMD64 CONTEXT flags (raw values; avoids chasing crate constant names).
        const CTX_AMD64: u32 = 0x0010_0000;
        const CTX_FLAGS: u32 = CTX_AMD64 | 0x3; // CONTEXT_CONTROL | CONTEXT_INTEGER
        const MACHINE_AMD64: u32 = 0x8664;

        static SYM_INIT: AtomicBool = AtomicBool::new(false);

        unsafe {
            let Ok(thread) = OpenThread(
                THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
                false,
                tid,
            ) else {
                return format!("(could not open UI thread {tid})");
            };

            SuspendThread(thread);

            let mut ctx = std::mem::MaybeUninit::<CONTEXT>::zeroed().assume_init();
            ctx.ContextFlags = CONTEXT_FLAGS(CTX_FLAGS);
            let got_ctx = GetThreadContext(thread, &mut ctx).is_ok();

            if !got_ctx {
                ResumeThread(thread);
                let _ = CloseHandle(thread);
                return "(GetThreadContext failed)".to_string();
            }

            let proc = GetCurrentProcess();

            // SymInitialize is idempotent but call it once to avoid races.
            if !SYM_INIT.swap(true, Ordering::SeqCst) {
                SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS);
                let _ = SymInitialize(proc, None, true);
            }

            let mut frame = std::mem::MaybeUninit::<STACKFRAME64>::zeroed().assume_init();
            frame.AddrPC.Offset = ctx.Rip;
            frame.AddrPC.Mode = ADDRESS_MODE(3); // AddrModeFlat
            frame.AddrFrame.Offset = ctx.Rbp;
            frame.AddrFrame.Mode = ADDRESS_MODE(3);
            frame.AddrStack.Offset = ctx.Rsp;
            frame.AddrStack.Mode = ADDRESS_MODE(3);

            let mut ctx_walk = ctx;
            let mut lines: Vec<String> = Vec::new();

            for i in 0..48 {
                // The windows crate declares these helpers as `unsafe fn` (cdecl)
            // but StackWalk64 wants `extern "system"` fn pointers. Transmute the
            // calling convention — on x64 Windows they are identical in practice.
            use windows::Win32::Foundation::HANDLE;
            type FtaFn = unsafe extern "system" fn(HANDLE, u64) -> *mut std::ffi::c_void;
            type MbFn  = unsafe extern "system" fn(HANDLE, u64) -> u64;
            let fta: FtaFn = std::mem::transmute(SymFunctionTableAccess64 as *const ());
            let mb:  MbFn  = std::mem::transmute(SymGetModuleBase64 as *const ());

            let ok = StackWalk64(
                    MACHINE_AMD64,
                    proc,
                    thread,
                    &mut frame,
                    &mut ctx_walk as *mut CONTEXT as *mut _,
                    None,
                    Some(fta),
                    Some(mb),
                    None,
                );
                if !ok.as_bool() || frame.AddrPC.Offset == 0 {
                    break;
                }

                let addr = frame.AddrPC.Offset;

                // Resolve symbol. SYMBOL_INFO has a variable-length Name tail.
                const MAX_NAME: usize = 512;
                let sym_total = std::mem::size_of::<SYMBOL_INFO>() + MAX_NAME;
                let mut sym_buf: Vec<u8> = vec![0u8; sym_total];
                let sym = sym_buf.as_mut_ptr() as *mut SYMBOL_INFO;
                (*sym).SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
                (*sym).MaxNameLen = MAX_NAME as u32;

                let mut disp = 0u64;
                let label = if SymFromAddr(proc, addr, Some(&mut disp), sym).is_ok() {
                    let name_ptr = std::ptr::addr_of!((*sym).Name) as *const u8;
                    let name_len = ((*sym).NameLen as usize).min(MAX_NAME);
                    let bytes = std::slice::from_raw_parts(name_ptr, name_len);
                    let name = String::from_utf8_lossy(bytes);
                    format!("{name}+{disp}")
                } else {
                    format!("0x{addr:016x}")
                };
                lines.push(format!("  {i:2}: {label}"));
            }

            // Resume AFTER the full stack walk — thread must stay suspended
            // while StackWalk64 reads its stack memory.
            ResumeThread(thread);
            let _ = CloseHandle(thread);

            if lines.is_empty() {
                "(no frames captured — debug symbols may be stripped)".to_string()
            } else {
                lines.join("\n")
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = tid;
        "(stack capture not available on this platform)".to_string()
    }
}

// ── TaskDialogIndirect-based dialog ───────────────────────────────────────────

/// Show a rich crash/hang dialog with a scrollable detail pane.
///
/// `warning` selects the warning vs error icon and adjusts the button set:
/// - panic (false): "Close" + "Copy details" + "Open log folder"
/// - hang  (true):  "Force quit" + "Keep waiting" + "Copy details" + "Open log folder"
///
/// `hb_for_timer`: when `Some`, the dialog polls the heartbeat every ~200ms
/// and auto-dismisses with "Keep waiting" if the UI recovers. Pass `None` for
/// crash/panic dialogs where no heartbeat exists.
///
/// Returns `true` only when the user chose **Force quit** (hang path).
fn show_detail_dialog(
    title: &str,
    heading: &str,
    summary: &str,
    detail: &str,
    log_dir: &str,
    warning: bool,
    hb_for_timer: Option<(&Heartbeat, Duration)>,
) -> bool {
    #[cfg(target_os = "windows")]
    return show_task_dialog_win32(title, heading, summary, detail, log_dir, warning, hb_for_timer);

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (heading, detail, log_dir, warning, hb_for_timer);
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

// Thread-locals used by the TaskDialog callback. Set before each
// TaskDialogIndirect call; cleared immediately after it returns.
thread_local! {
    static CB_DETAIL: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
    static CB_LOG_DIR: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
    /// Raw `*const Inner` for the hang-dialog heartbeat check (0 = not a hang dialog).
    /// The Inner lives for the program's lifetime so this pointer is always valid
    /// while the dialog is open.
    static CB_HB_PTR: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static CB_THRESHOLD_MS: std::cell::Cell<u64> = std::cell::Cell::new(0);
}

/// Raw HICON handle (as `usize`) for the custom crash/freeze dialog icon.
/// Stored as `usize` so the static is `Send`. Set once at startup via
/// [`set_dialog_icon`]; `0` means not set, fall back to the built-in icon.
static DIALOG_ICON_HANDLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Set the PNG file to use as the main icon in crash and freeze dialogs.
/// Downscales with Lanczos3, caches as an ICO in appdata, then loads it via
/// `LoadImageW` so Windows handles pixel layout correctly.
/// Call once at startup after the settings store is opened.
/// A missing file or an unloadable PNG silently falls back to the standard icon.
pub fn set_dialog_icon(path: Option<std::path::PathBuf>) {
    #[cfg(target_os = "windows")]
    if let Some(p) = path {
        if let Some(hicon) = prepare_and_load_icon(&p) {
            let _ = DIALOG_ICON_HANDLE.set(hicon.0 as usize);
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = path;
}

/// Downscale `src` to 48×48 with Lanczos3, write an ICO to appdata, and load
/// it via `LoadImageW`. Returns `None` on any failure (bad PNG, save error, …).
/// Using LoadImageW with an ICO file avoids the manual BGRA flip and premultiply
/// dance required by CreateBitmap/CreateIconIndirect.
#[cfg(target_os = "windows")]
fn prepare_and_load_icon(
    src: &std::path::Path,
) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use image::imageops::FilterType;
    use windows::Win32::UI::WindowsAndMessaging::{
        LoadImageW, HICON, IMAGE_ICON, LR_LOADFROMFILE,
    };
    use windows::core::PCWSTR;

    const ICON_SIZE: u32 = 48;

    // High-quality downscale; avoids the aliasing Windows produces when given a
    // large PNG and asked to scale it to a 48×48 icon on its own.
    let img = image::open(src).ok()?.into_rgba8();
    let scaled = image::imageops::resize(&img, ICON_SIZE, ICON_SIZE, FilterType::Lanczos3);

    // Persist to appdata as an ICO so the result can be inspected and reused.
    let ico_path = crate::app_paths::data_dir().join("dialog_icon.ico");
    image::DynamicImage::ImageRgba8(scaled).save(&ico_path).ok()?;

    // LoadImageW handles pixel layout (orientation, alpha) correctly for ICO files.
    let path_w: Vec<u16> = ico_path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle = LoadImageW(
            None,
            PCWSTR(path_w.as_ptr()),
            IMAGE_ICON,
            ICON_SIZE as i32,
            ICON_SIZE as i32,
            LR_LOADFROMFILE,
        )
        .ok()?;
        Some(HICON(handle.0))
    }
}

// TDM_CLICK_BUTTON = WM_USER + 102 = 0x0466: simulates a button click on the
// task dialog. Used to auto-dismiss the hang dialog when the UI recovers.
const TDM_CLICK_BUTTON: u32 = 0x0466;

/// TaskDialog callback. Intercepts button clicks that should NOT close the
/// dialog (Copy details, Open log folder) and returns S_FALSE to keep it open.
/// For hang dialogs, also polls the heartbeat on every timer tick (≈200ms) and
/// auto-dismisses with "Keep waiting" if the UI thread becomes responsive again.
#[cfg(target_os = "windows")]
unsafe extern "system" fn task_dialog_cb(
    hwnd: windows::Win32::Foundation::HWND,
    msg: windows::Win32::UI::Controls::TASKDIALOG_NOTIFICATIONS,
    wparam: windows::Win32::Foundation::WPARAM,
    _lparam: windows::Win32::Foundation::LPARAM,
    _lp_ref: isize,
) -> windows::core::HRESULT {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Controls::{TDN_BUTTON_CLICKED, TASKDIALOG_NOTIFICATIONS};
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

    // TDN_TIMER fires every ~200ms when TDF_CALLBACK_TIMER is set.
    if msg == TASKDIALOG_NOTIFICATIONS(4) {
        let hb_ptr = CB_HB_PTR.with(|c| c.get());
        if hb_ptr != 0 {
            let threshold_ms = CB_THRESHOLD_MS.with(|c| c.get());
            // SAFETY: CB_HB_PTR holds a raw pointer to Inner from an Arc that lives
            // for the program's lifetime (owned by start_watchdog's closure).
            let inner = unsafe { &*(hb_ptr as *const Inner) };
            let now = inner.start.elapsed().as_millis() as u64;
            let last = inner.last_beat_ms.load(Ordering::Relaxed);
            if now.saturating_sub(last) < threshold_ms {
                // UI recovered — queue a "Keep waiting" click to dismiss.
                // SAFETY: hwnd is valid for the lifetime of the dialog callback.
                unsafe { let _ = PostMessageW(Some(hwnd), TDM_CLICK_BUTTON, WPARAM(2), LPARAM(0)); }
            }
        }
        return windows::core::HRESULT(0);
    }

    if msg == TDN_BUTTON_CLICKED {
        let btn = wparam.0 as i32;
        if btn == BTN_COPY {
            CB_DETAIL.with(|d| copy_to_clipboard(&d.borrow()));
            return windows::core::HRESULT(1); // S_FALSE — keep dialog open
        }
        if btn == BTN_OPEN_LOGS {
            CB_LOG_DIR.with(|d| {
                let _ = std::process::Command::new("explorer")
                    .arg(d.borrow().as_str())
                    .spawn();
            });
            return windows::core::HRESULT(1); // S_FALSE — keep dialog open
        }
    }
    windows::core::HRESULT(0) // S_OK — allow dialog to close
}

#[cfg(target_os = "windows")]
fn show_task_dialog_win32(
    title: &str,
    heading: &str,
    summary: &str,
    detail: &str,
    log_dir: &str,
    warning: bool,
    hb_for_timer: Option<(&Heartbeat, Duration)>,
) -> bool {
    use windows::Win32::Foundation::{HWND, HINSTANCE};
    use windows::Win32::UI::Controls::{
        TaskDialogIndirect, TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOG_FLAGS,
        TASKDIALOGCONFIG_0, TASKDIALOGCONFIG_1,
        TDF_ALLOW_DIALOG_CANCELLATION, TDF_SIZE_TO_CONTENT, TDF_USE_HICON_MAIN,
        TDCBF_CLOSE_BUTTON, TD_ERROR_ICON, TD_WARNING_ICON,
    };
    use windows::Win32::UI::WindowsAndMessaging::{HICON, IDCANCEL};
    use windows::core::PCWSTR;

    // TDF_CALLBACK_TIMER (0x0040): fires TDN_TIMER every ~200ms so we can poll
    // the heartbeat and auto-dismiss when the UI thread becomes responsive again.
    const TDF_CALLBACK_TIMER: TASKDIALOG_FLAGS = TASKDIALOG_FLAGS(0x0040);

    // Make detail and log_dir available to the callback via thread-locals.
    CB_DETAIL.with(|d| *d.borrow_mut() = detail.to_string());
    CB_LOG_DIR.with(|d| *d.borrow_mut() = log_dir.to_string());
    // For hang dialogs: expose the heartbeat so the timer callback can auto-close.
    if let Some((hb, threshold)) = hb_for_timer {
        CB_HB_PTR.with(|c| c.set(std::sync::Arc::as_ptr(&hb.inner) as usize));
        CB_THRESHOLD_MS.with(|c| c.set(threshold.as_millis() as u64));
    }

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

    // Try custom icon first; fall back to the built-in TD_WARNING/ERROR constants.
    let custom_hicon: Option<HICON> = DIALOG_ICON_HANDLE
        .get()
        .copied()
        .map(|raw| HICON(raw as *mut _));

    let (extra_flags, anon1) = match custom_hicon {
        Some(hicon) => (
            TDF_USE_HICON_MAIN,
            TASKDIALOGCONFIG_0 { hMainIcon: hicon },
        ),
        None => {
            let icon = if warning { TD_WARNING_ICON } else { TD_ERROR_ICON };
            (
                windows::Win32::UI::Controls::TASKDIALOG_FLAGS(0),
                TASKDIALOGCONFIG_0 { pszMainIcon: icon },
            )
        }
    };

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
        dwFlags: TDF_SIZE_TO_CONTENT | TDF_ALLOW_DIALOG_CANCELLATION | extra_flags
            | if hb_for_timer.is_some() { TDF_CALLBACK_TIMER } else { TASKDIALOG_FLAGS(0) },
        dwCommonButtons: TDCBF_CLOSE_BUTTON,
        pszWindowTitle: PCWSTR(title_w.as_ptr()),
        Anonymous1: anon1,
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
            hFooterIcon: HICON(std::ptr::null_mut()),
        },
        pszFooter: PCWSTR(footer_w.as_ptr()),
        pfCallback: Some(task_dialog_cb),
        lpCallbackData: 0,
        cxWidth: 0,
    };

    let mut btn: i32 = IDCANCEL.0 as i32;
    let ok = unsafe { TaskDialogIndirect(&config, Some(&mut btn), None, None) };
    // Clear heartbeat pointer so the callback never sees a dangling reference.
    CB_HB_PTR.with(|c| c.set(0));
    CB_THRESHOLD_MS.with(|c| c.set(0));

    if ok.is_err() {
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

    // BTN_COPY and BTN_OPEN_LOGS are handled by the callback (S_FALSE keeps the
    // dialog open so they never appear here). Only BTN_FORCE_QUIT reaches this.
    matches!(btn, BTN_FORCE_QUIT)
}

/// Copy `text` to the system clipboard via PowerShell (avoids wrestling with
/// Win32 memory handle types across crate versions).
fn copy_to_clipboard(text: &str) {
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
