//! OS integration: single-instance guard, tray icon image, and autostart.

use std::net::TcpListener;

use anyhow::Result;
use auto_launch::{AutoLaunch, AutoLaunchBuilder};

/// Loopback port used purely as a cross-platform single-instance guard.
const SINGLE_INSTANCE_PORT: u16 = 47835;

/// Try to acquire the single-instance lock by binding a loopback port.
///
/// Returns `Some(listener)` if we are the first instance (keep it alive for the
/// process lifetime), or `None` if another instance already holds it.
pub fn acquire_single_instance() -> Option<TcpListener> {
    TcpListener::bind(("127.0.0.1", SINGLE_INSTANCE_PORT)).ok()
}

/// Build the tray/window icon: a purple tile with a red "record" dot.
pub fn app_icon_rgba() -> (Vec<u8>, u32, u32) {
    let size: u32 = 32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let (cx, cy, r) = (15.5f32, 15.5f32, 9.0f32);
    for y in 0..size {
        for x in 0..size {
            let idx = ((y * size + x) * 4) as usize;
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let (rr, gg, bb) = if dist <= r {
                (0xff, 0x3b, 0x30) // record dot
            } else {
                (0x6a, 0x3a, 0xcf) // background
            };
            rgba[idx] = rr;
            rgba[idx + 1] = gg;
            rgba[idx + 2] = bb;
            rgba[idx + 3] = 0xff;
        }
    }
    (rgba, size, size)
}

/// Build a [`tray_icon::Icon`] from the embedded image.
pub fn tray_icon_image() -> Result<tray_icon::Icon> {
    let (rgba, w, h) = app_icon_rgba();
    Ok(tray_icon::Icon::from_rgba(rgba, w, h)?)
}

// ===== Detached downloads: jobs that OUTLIVE the app, re-attachable on relaunch =====

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A **named** Win32 Job Object **without** `KILL_ON_JOB_CLOSE`.
/// Closing our handle (app exit) does **not** terminate members,
/// so an assigned download tool keeps running after we quit; the kernel keeps the
/// named job alive while any member runs, so a later launch can [`open`](Self::open)
/// it by name to terminate the whole tree. Identity/liveness of the individual
/// process is tracked separately via [`process_start_time`] + [`pid_alive`].
///
/// Note: we deliberately do **not** spawn members with `CREATE_BREAKAWAY_FROM_JOB`.
/// A normally-launched GUI app isn't inside a kill-on-close job, and on Win8+ our
/// no-kill-on-close job nests fine inside any ambient job; the breakaway flag would
/// risk a spawn failure if an ambient job disallowed breakaway, for no real gain.
#[cfg(windows)]
pub struct DetachedJob {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for DetachedJob {}
#[cfg(windows)]
unsafe impl Sync for DetachedJob {}

#[cfg(windows)]
impl DetachedJob {
    /// Create a named job with no limit flags (so app exit leaves members running).
    pub fn create(name: &str) -> Result<DetachedJob> {
        use windows::Win32::System::JobObjects::CreateJobObjectW;
        let wide = to_wide(name);
        unsafe {
            let handle = CreateJobObjectW(None, windows::core::PCWSTR(wide.as_ptr()))?;
            Ok(DetachedJob { handle })
        }
    }

    /// Re-open an existing named job after a restart (for [`kill`](Self::kill)).
    /// `None` if it no longer exists (no member is running).
    pub fn open(name: &str) -> Option<DetachedJob> {
        use windows::Win32::System::JobObjects::OpenJobObjectW;
        // JOB_OBJECT_ALL_ACCESS — enough to terminate.
        const JOB_OBJECT_ALL_ACCESS: u32 = 0x001F_001F;
        let wide = to_wide(name);
        unsafe {
            OpenJobObjectW(
                JOB_OBJECT_ALL_ACCESS,
                false,
                windows::core::PCWSTR(wide.as_ptr()),
            )
            .ok()
            .map(|handle| DetachedJob { handle })
        }
    }

    /// Assign a spawned child (and its future descendants) to this job.
    pub fn assign_child(&self, child: &tokio::process::Child) -> Result<()> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
        if let Some(raw) = child.raw_handle() {
            unsafe { AssignProcessToJobObject(self.handle, HANDLE(raw as *mut _))? };
        }
        Ok(())
    }

    /// Terminate every process in the job immediately.
    pub fn kill(&self) {
        use windows::Win32::System::JobObjects::TerminateJobObject;
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for DetachedJob {
    fn drop(&mut self) {
        // No kill-on-close: closing our handle must NOT terminate the tree — that
        // is the whole point of detaching. The job persists while a member runs.
        use windows::Win32::Foundation::CloseHandle;
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// The OS creation time of `pid` (FILETIME 100ns ticks), or `None` if it can't be
/// opened. Stored alongside the PID so a re-attach can reject a recycled PID
/// (a different process now wearing the same number).
#[cfg(windows)]
pub fn process_start_time(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let res = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        res.ok()?;
        Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
    }
}

/// Whether `pid` names a process that's still running.
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    if pid == 0 {
        return false;
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code).is_ok();
        let _ = CloseHandle(handle);
        ok && code == STILL_ACTIVE
    }
}

/// Block until `pid` exits (polling every 200ms so `shutdown` can interrupt) and
/// return its exit code; `None` if `shutdown` fired first or the handle is
/// unusable. Run on a blocking thread (`spawn_blocking`) — it parks on a handle.
#[cfg(windows)]
pub fn wait_pid(pid: u32, shutdown: &std::sync::atomic::AtomicBool) -> Option<i64> {
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_ACCESS_RIGHTS,
        PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };
    const STILL_ACTIVE: u32 = 259;
    // SYNCHRONIZE (0x0010_0000) for the wait + query rights for the exit code.
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_ACCESS_RIGHTS(0x0010_0000);
    if pid == 0 {
        return None;
    }
    unsafe {
        let Ok(handle) = OpenProcess(access, false, pid) else {
            return None;
        };
        let read_exit = |h| {
            let mut code: u32 = 0;
            GetExitCodeProcess(h, &mut code).ok().map(|_| code as i64)
        };
        loop {
            if shutdown.load(Ordering::SeqCst) {
                let _ = CloseHandle(handle);
                return None;
            }
            let w = WaitForSingleObject(handle, 200);
            if w == WAIT_OBJECT_0 {
                let code = read_exit(handle);
                let _ = CloseHandle(handle);
                return code;
            }
            if w == WAIT_TIMEOUT {
                continue;
            }
            // WAIT_FAILED/abandoned: confirm via exit code, else give up.
            if let Some(code) = read_exit(handle) {
                if code != STILL_ACTIVE as i64 {
                    let _ = CloseHandle(handle);
                    return Some(code);
                }
            }
            let _ = CloseHandle(handle);
            return None;
        }
    }
}

#[cfg(not(windows))]
pub struct DetachedJob;

#[cfg(not(windows))]
impl DetachedJob {
    pub fn create(_name: &str) -> Result<DetachedJob> {
        Ok(DetachedJob)
    }
    pub fn open(_name: &str) -> Option<DetachedJob> {
        None
    }
    pub fn assign_child(&self, _child: &tokio::process::Child) -> Result<()> {
        Ok(())
    }
    pub fn kill(&self) {}
}

#[cfg(not(windows))]
pub fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

#[cfg(not(windows))]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn wait_pid(_pid: u32, _shutdown: &std::sync::atomic::AtomicBool) -> Option<i64> {
    None
}

/// Snapshot `(pid, parent_pid)` for every process on the system (Toolhelp).
/// Shared by [`kill_process_tree`] and the I/O monitor's child sampler (both
/// need to find the grandchildren a launcher like yt-dlp spawned).
#[cfg(windows)]
pub fn process_children_snapshot() -> Vec<(u32, u32)> {
    use std::mem::size_of;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut pairs: Vec<(u32, u32)> = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pairs;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    pairs
}

#[cfg(not(windows))]
pub fn process_children_snapshot() -> Vec<(u32, u32)> {
    Vec::new()
}

/// Cumulative I/O counters of one process (`GetProcessIoCounters`).
///
/// Note: these count **all** I/O the process issued, including sockets — for
/// a capture tool the read side is mostly CDN network traffic while the write
/// side is the file it records. Callers that care about disk load should lean
/// on the write counters (or the physical-disk stats) accordingly.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ProcIo {
    pub read_ops: u64,
    pub write_ops: u64,
    pub other_ops: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub other_bytes: u64,
}

#[cfg(windows)]
fn io_counters_from(handle: windows::Win32::Foundation::HANDLE) -> Option<ProcIo> {
    use windows::Win32::System::Threading::{GetProcessIoCounters, IO_COUNTERS};
    unsafe {
        let mut c = IO_COUNTERS::default();
        GetProcessIoCounters(handle, &mut c).ok()?;
        Some(ProcIo {
            read_ops: c.ReadOperationCount,
            write_ops: c.WriteOperationCount,
            other_ops: c.OtherOperationCount,
            read_bytes: c.ReadTransferCount,
            write_bytes: c.WriteTransferCount,
            other_bytes: c.OtherTransferCount,
        })
    }
}

/// Cumulative I/O counters of `pid`, or `None` if it can't be opened (an
/// exited process is the normal case — callers drop it silently).
#[cfg(windows)]
pub fn process_io_counters(pid: u32) -> Option<ProcIo> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let res = io_counters_from(handle);
        let _ = CloseHandle(handle);
        res
    }
}

#[cfg(not(windows))]
pub fn process_io_counters(_pid: u32) -> Option<ProcIo> {
    None
}

/// Cumulative I/O counters of this process (pseudo-handle; nothing to close).
#[cfg(windows)]
pub fn self_io_counters() -> Option<ProcIo> {
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe { io_counters_from(GetCurrentProcess()) }
}

#[cfg(not(windows))]
pub fn self_io_counters() -> Option<ProcIo> {
    None
}

/// Forcefully kill a process and its entire child tree by PID.
///
/// Uses a Toolhelp snapshot to find descendants (by parent PID) and terminates
/// children before parents. This reliably reaches grandchildren (e.g. the ffmpeg
/// a yt-dlp launcher spawned) even when a Python console-script wrapper created
/// its child with `CREATE_BREAKAWAY_FROM_JOB`, which escapes a Job Object.
#[cfg(windows)]
pub fn kill_process_tree(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    if pid == 0 {
        return;
    }
    // Snapshot (pid, parent_pid) for all processes.
    let pairs = process_children_snapshot();

    // BFS to collect the target and all descendants.
    let mut tree = vec![pid];
    let mut i = 0;
    while i < tree.len() {
        let cur = tree[i];
        for &(p, parent) in &pairs {
            if parent == cur && !tree.contains(&p) {
                tree.push(p);
            }
        }
        i += 1;
    }

    // Terminate children before parents.
    for &p in tree.iter().rev() {
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, p) {
                let _ = TerminateProcess(handle, 1);
                let _ = CloseHandle(handle);
            }
        }
    }
}

#[cfg(not(windows))]
pub fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    // Best-effort on Unix; full process-group handling is a later enhancement.
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}

/// Mark a directory (or file) hidden. On Windows this sets
/// `FILE_ATTRIBUTE_HIDDEN`; elsewhere it's a no-op (a `.`-prefixed name already
/// hides it by Unix convention). Best-effort: failures are ignored.
#[cfg(windows)]
pub fn set_hidden(path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = SetFileAttributesW(
            windows::core::PCWSTR(wide.as_ptr()),
            FILE_ATTRIBUTE_HIDDEN,
        );
    }
}

#[cfg(not(windows))]
pub fn set_hidden(_path: &std::path::Path) {}

/// Reveal a path in the OS file manager (Explorer on Windows, Finder on macOS,
/// the default handler via `xdg-open` elsewhere).
///
/// Best-effort: spawn failures are ignored. Note that `explorer.exe` returns a
/// non-zero exit code even on success, so we never inspect the status.
pub fn open_path(path: &std::path::Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Manages launch-on-login via the OS autostart mechanism (HKCU Run key on
/// Windows), keyed to the current executable path.
pub struct AutoStart {
    inner: Option<AutoLaunch>,
}

impl AutoStart {
    pub fn new() -> AutoStart {
        let inner = std::env::current_exe().ok().and_then(|exe| {
            let exe = exe.to_string_lossy().to_string();
            AutoLaunchBuilder::new()
                .set_app_name("StreamArchiver")
                .set_app_path(&exe)
                // Launch-on-login starts hidden to the tray rather than popping a window.
                .set_args(&["--hidden"])
                .build()
                .ok()
        });
        AutoStart { inner }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner
            .as_ref()
            .and_then(|a| a.is_enabled().ok())
            .unwrap_or(false)
    }

    pub fn set(&self, enabled: bool) -> Result<()> {
        if let Some(a) = &self.inner {
            if enabled {
                a.enable()?;
            } else {
                a.disable()?;
            }
        }
        Ok(())
    }
}

impl Default for AutoStart {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn self_io_counters_nonzero_and_monotonic() {
        let before = self_io_counters().expect("self counters");
        // Do some real I/O so the counters must move.
        let path = std::env::temp_dir().join(format!("sa-procio-{}.tmp", std::process::id()));
        std::fs::write(&path, vec![0u8; 64 * 1024]).unwrap();
        let _ = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let after = self_io_counters().expect("self counters");
        assert!(after.write_bytes >= before.write_bytes + 64 * 1024);
        assert!(after.read_ops >= before.read_ops);
    }

    #[test]
    fn process_io_counters_work_for_own_pid() {
        let io = process_io_counters(std::process::id()).expect("own pid counters");
        assert!(io.read_ops + io.write_ops + io.other_ops > 0);
    }

    #[test]
    fn children_snapshot_contains_self() {
        let pairs = process_children_snapshot();
        let me = std::process::id();
        assert!(pairs.iter().any(|&(p, _)| p == me));
    }
}
