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

/// The pid of the process LISTENING on local TCP `port` (IPv4 or IPv6, any
/// interface), or `None` if nothing listens there. Used to take control of an
/// externally-started helper server (e.g. a manually-launched PO token
/// server): the app never spawned it, so this is the only way to learn who
/// owns the port. Note: for a server inside Docker/WSL the returned pid is
/// the *port proxy* (com.docker.backend / wslrelay), not the server itself.
#[cfg(windows)]
pub fn pid_listening_on(port: u16) -> Option<u32> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID,
        MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    // dwLocalPort holds the port in network byte order in its low 16 bits.
    let wanted = port.to_be() as u32;
    // One fetch per address family: the server binds `[::]` (dual-stack) with
    // an IPv4 `0.0.0.0` fallback, so either table may hold the listener.
    unsafe fn table_lookup(family: u32, wanted: u32) -> Option<u32> {
        unsafe {
            let mut size: u32 = 0;
            // First call sizes the buffer (returns ERROR_INSUFFICIENT_BUFFER).
            let _ = GetExtendedTcpTable(
                None,
                &mut size,
                false,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            );
            if size == 0 {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            let rc = GetExtendedTcpTable(
                Some(buf.as_mut_ptr().cast()),
                &mut size,
                false,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            );
            if rc != 0 {
                return None;
            }
            if family == AF_INET.0 as u32 {
                let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(
                    table.table.as_ptr(),
                    table.dwNumEntries as usize,
                );
                rows.iter()
                    .find(|r: &&MIB_TCPROW_OWNER_PID| r.dwLocalPort == wanted)
                    .map(|r| r.dwOwningPid)
            } else {
                let table = &*(buf.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(
                    table.table.as_ptr(),
                    table.dwNumEntries as usize,
                );
                rows.iter()
                    .find(|r: &&MIB_TCP6ROW_OWNER_PID| r.dwLocalPort == wanted)
                    .map(|r| r.dwOwningPid)
            }
        }
    }
    unsafe {
        table_lookup(AF_INET.0 as u32, wanted)
            .or_else(|| table_lookup(AF_INET6.0 as u32, wanted))
            .filter(|pid| *pid != 0)
    }
}

/// Which processes currently hold open handles to any of `paths`, via the
/// Restart Manager (the API behind Windows' "file in use by…" dialogs) —
/// e.g. `"bztransmit.exe (pid 4712, service)"`. Works unelevated for files
/// this process can access. Blocking (RM enumerates system state) — call
/// from `spawn_blocking` on async paths. Empty = no holder found (the lock
/// was already released, or RM couldn't see it).
#[cfg(windows)]
pub fn file_lock_holders(paths: &[std::path::PathBuf]) -> Vec<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::RestartManager::{
        CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
        RmStartSession, RmService,
    };
    use windows::core::PCWSTR;

    if paths.is_empty() {
        return Vec::new();
    }
    let wide: Vec<Vec<u16>> = paths
        .iter()
        .map(|p| p.as_os_str().encode_wide().chain(std::iter::once(0)).collect())
        .collect();
    let ptrs: Vec<PCWSTR> = wide.iter().map(|w| PCWSTR(w.as_ptr())).collect();
    let mut session = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    unsafe {
        if RmStartSession(&mut session, None, windows::core::PWSTR(key.as_mut_ptr())).is_err() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if RmRegisterResources(session, Some(&ptrs), None, None).is_ok() {
            let mut needed = 0u32;
            let mut count = 0u32;
            let mut reboot = 0u32;
            // First call sizes the array (ERROR_MORE_DATA when non-empty).
            let _ = RmGetList(session, &mut needed, &mut count, None, &mut reboot);
            if needed > 0 {
                let mut infos = vec![RM_PROCESS_INFO::default(); needed as usize];
                count = needed;
                if RmGetList(session, &mut needed, &mut count, Some(infos.as_mut_ptr()), &mut reboot)
                    .is_ok()
                {
                    for info in infos.iter().take(count as usize) {
                        let name_len =
                            info.strAppName.iter().position(|&c| c == 0).unwrap_or(0);
                        let name = String::from_utf16_lossy(&info.strAppName[..name_len]);
                        let name = if name.is_empty() { "<unknown>".into() } else { name };
                        let kind =
                            if info.ApplicationType == RmService { ", service" } else { "" };
                        out.push(format!(
                            "{name} (pid {}{kind})",
                            info.Process.dwProcessId
                        ));
                    }
                }
            }
        }
        let _ = RmEndSession(session);
        out
    }
}

#[cfg(not(windows))]
pub fn file_lock_holders(_paths: &[std::path::PathBuf]) -> Vec<String> {
    Vec::new()
}

/// Send a file to the OS Recycle Bin (`SHFileOperationW` + `FOF_ALLOWUNDO`).
/// Blocking — call from `spawn_blocking` on async paths. NB: on volumes
/// without a Recycle Bin (some removable media) Windows deletes permanently;
/// paths past the legacy 260-char limit can fail (shell API restriction) —
/// the caller keeps the file on failure.
#[cfg(windows)]
pub fn recycle_path(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        SHFILEOPSTRUCTW, SHFileOperationW,
    };
    use windows::core::PCWSTR;
    // pFrom is a double-NUL-terminated list of NUL-terminated paths.
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.push(0);
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0) as u16,
        ..Default::default()
    };
    let ret = unsafe { SHFileOperationW(&mut op) };
    if ret == 0 && !op.fAnyOperationsAborted.as_bool() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "SHFileOperationW failed (code {ret:#x}, aborted {})",
            op.fAnyOperationsAborted.as_bool()
        )))
    }
}

#[cfg(not(windows))]
pub fn recycle_path(_path: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::other("recycle bin is Windows-only"))
}

#[cfg(not(windows))]
pub fn pid_listening_on(_port: u16) -> Option<u32> {
    None
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

/// One process from a Toolhelp snapshot: pid, parent pid, and executable
/// name (lowercased, `.exe` stripped — for display).
#[derive(Clone, Debug)]
pub struct SnapProc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

/// Snapshot every process on the system (Toolhelp). Shared by
/// [`kill_process_tree`] and the I/O monitor's child sampler (both need to
/// find the grandchildren a launcher like yt-dlp spawned; the sampler also
/// shows the exe names so "what is this tree doing right now" is visible —
/// e.g. a finished SABR capture whose yt-dlp is running its ffmpeg merge).
#[cfg(windows)]
pub fn process_tree_snapshot() -> Vec<SnapProc> {
    use std::mem::size_of;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut procs: Vec<SnapProc> = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return procs;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let mut name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                if let Some(stripped) = name.strip_suffix(".exe") {
                    name.truncate(stripped.len());
                }
                procs.push(SnapProc {
                    pid: entry.th32ProcessID,
                    ppid: entry.th32ParentProcessID,
                    name,
                });
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    procs
}

#[cfg(not(windows))]
pub fn process_tree_snapshot() -> Vec<SnapProc> {
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

/// Cumulative physical-disk performance counters (`IOCTL_DISK_PERFORMANCE`).
/// The interesting live number is `queue_depth` — sustained depth on a USB
/// enclosure is the early-warning signal before it drops off the bus.
#[derive(Clone, Copy, Default, Debug)]
pub struct DiskPerf {
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub queue_depth: u32,
}

/// Query the physical disk backing drive `letter` (e.g. `'A'`).
///
/// Opens the volume with access 0 (no admin needed), resolves its physical
/// device number, then queries `\\.\PhysicalDriveN`. Returns `None` when the
/// disk-performance filter is unavailable (`ERROR_INVALID_FUNCTION` on some
/// stacks) or the drive doesn't exist — callers must degrade to "n/a".
/// Counters are disk-wide: other volumes on the same spindle are included
/// (that's the point — it's the spindle that saturates).
/// Open a device/volume path with access 0 (attribute query only, no admin
/// required) — shared by every raw disk IOCTL below.
#[cfg(windows)]
fn open_raw(path: &str) -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide = to_wide(path);
    unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .ok()
    }
}

/// Resolve drive `letter` to its backing `\\.\PhysicalDriveN` handle (open)
/// and device number — shared by every per-physical-disk query below (perf
/// counters, bus type, write-cache property, removal policy).
#[cfg(windows)]
fn physical_drive_handle(letter: char) -> Option<(windows::Win32::Foundation::HANDLE, u32)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER};

    if !letter.is_ascii_alphabetic() {
        return None;
    }
    unsafe {
        let vol = open_raw(&format!("\\\\.\\{}:", letter.to_ascii_uppercase()))?;
        let mut devnum = STORAGE_DEVICE_NUMBER::default();
        let mut returned = 0u32;
        let res = DeviceIoControl(
            vol,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some(&mut devnum as *mut _ as *mut _),
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            Some(&mut returned),
            None,
        );
        let _ = CloseHandle(vol);
        res.ok()?;
        let disk = open_raw(&format!("\\\\.\\PhysicalDrive{}", devnum.DeviceNumber))?;
        Some((disk, devnum.DeviceNumber))
    }
}

#[cfg(windows)]
pub fn disk_performance(letter: char) -> Option<DiskPerf> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{DISK_PERFORMANCE, IOCTL_DISK_PERFORMANCE};

    unsafe {
        let (disk, _) = physical_drive_handle(letter)?;
        let mut perf = DISK_PERFORMANCE::default();
        let mut returned = 0u32;
        let res = DeviceIoControl(
            disk,
            IOCTL_DISK_PERFORMANCE,
            None,
            0,
            Some(&mut perf as *mut _ as *mut _),
            std::mem::size_of::<DISK_PERFORMANCE>() as u32,
            Some(&mut returned),
            None,
        );
        let _ = CloseHandle(disk);
        res.ok()?;

        Some(DiskPerf {
            bytes_read: perf.BytesRead.max(0) as u64,
            bytes_written: perf.BytesWritten.max(0) as u64,
            queue_depth: perf.QueueDepth,
        })
    }
}

#[cfg(not(windows))]
pub fn disk_performance(_letter: char) -> Option<DiskPerf> {
    None
}

/// Physical bus a disk is attached over — the main signal for "what
/// tolerances can this drive letter actually handle": SATA/NVMe/SAS are
/// internally wired and don't share bandwidth with anything else, while USB
/// (and SD/MMC) run over a hub/enclosure/cable chain that can be shared,
/// flaky, or bandwidth-starved depending on what else is plugged in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiskBus {
    Usb,
    Sata,
    Nvme,
    Sas,
    Ata,
    Scsi,
    Raid,
    Sd,
    Mmc,
    Virtual,
    Ufs,
    #[default]
    Unknown,
}

impl DiskBus {
    /// Short label for table cells ("USB", "SATA", "NVMe", …).
    pub fn label(self) -> &'static str {
        match self {
            DiskBus::Usb => "USB",
            DiskBus::Sata => "SATA",
            DiskBus::Nvme => "NVMe",
            DiskBus::Sas => "SAS",
            DiskBus::Ata => "ATA",
            DiskBus::Scsi => "SCSI",
            DiskBus::Raid => "RAID",
            DiskBus::Sd => "SD",
            DiskBus::Mmc => "MMC",
            DiskBus::Virtual => "Virtual",
            DiskBus::Ufs => "UFS",
            DiskBus::Unknown => "?",
        }
    }

    /// Hot-pluggable external media (USB/SD/MMC) — the tier where enclosure,
    /// cable, and hub quality (and what else shares that hub) matter, as
    /// opposed to an internally wired SATA/SAS/NVMe/RAID connection.
    pub fn is_removable_bus(self) -> bool {
        matches!(self, DiskBus::Usb | DiskBus::Sd | DiskBus::Mmc)
    }
}

#[cfg(windows)]
#[allow(non_upper_case_globals)] // matching windows-rs's PascalCase BusType* constants
fn disk_bus_from(bt: windows::Win32::Storage::FileSystem::STORAGE_BUS_TYPE) -> DiskBus {
    use windows::Win32::Storage::FileSystem::*;
    match bt {
        BusTypeUsb => DiskBus::Usb,
        BusTypeSata => DiskBus::Sata,
        BusTypeNvme => DiskBus::Nvme,
        BusTypeSas => DiskBus::Sas,
        BusTypeAta => DiskBus::Ata,
        BusTypeScsi | BusTypeAtapi => DiskBus::Scsi,
        BusTypeRAID => DiskBus::Raid,
        BusTypeSd => DiskBus::Sd,
        BusTypeMmc => DiskBus::Mmc,
        BusTypeVirtual | BusTypeFileBackedVirtual | BusTypeSpaces => DiskBus::Virtual,
        BusTypeUfs => DiskBus::Ufs,
        _ => DiskBus::Unknown,
    }
}

/// The two options on a disk's Device Manager "Policies" tab: removal
/// policy (top) and write-cache policy (bottom half — see [`DiskHwInfo`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalPolicy {
    /// "Quick removal" — no Windows-side write caching, safe to unplug any
    /// time without Safely Remove Hardware.
    QuickRemoval,
    /// "Better performance" — Windows caches writes to it; unplugging
    /// without Safely Remove Hardware first risks losing cached writes.
    BetterPerformance,
}

/// Physical connection + Device Manager "Policies" tab info for the disk
/// backing a drive letter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiskHwInfo {
    pub bus: DiskBus,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    /// `None` when the device doesn't report a write-cache property at all
    /// (some USB bridge chipsets don't).
    pub write_cache_enabled: Option<bool>,
    /// The device has its own power protection, so Windows can safely skip
    /// flushing its write cache on every write — Device Manager's "Turn off
    /// Windows write-cache buffer flushing" option.
    pub power_protected: bool,
    /// `None` when the policy was never explicitly changed from its
    /// bus-type default (Windows doesn't write the registry value until
    /// someone picks a policy in Device Manager).
    pub removal_policy: Option<RemovalPolicy>,
}

impl DiskHwInfo {
    /// Short "make/model (serial)" line for a table cell — empty pieces
    /// (common over some buses/bridges) are simply omitted.
    pub fn model_line(&self) -> String {
        let model = [&self.vendor, &self.product].into_iter().filter(|s| !s.is_empty())
            .cloned().collect::<Vec<_>>().join(" ");
        if self.serial.is_empty() {
            model
        } else if model.is_empty() {
            format!("serial {}", self.serial)
        } else {
            format!("{model} (serial {})", self.serial)
        }
    }

    /// Full multi-line hover text: what it's plugged into, why that bus's
    /// tolerances differ, and its Device Manager "Policies" tab state.
    pub fn hover_text(&self) -> String {
        let mut lines = vec![format!("Connection: {}", self.bus.label())];
        let model = self.model_line();
        if !model.is_empty() {
            lines.push(model);
        }
        lines.push(if self.bus.is_removable_bus() {
            "External/removable media — cable, hub, and enclosure bridge quality all matter, \
             and it can share bandwidth with anything else on the same USB controller/hub. \
             (Whether THIS drive happens to be on its own dedicated port isn't detected here — \
             check Device Manager's USB tree if that matters.)"
                .to_string()
        } else {
            "Internally wired — not subject to USB/enclosure/cable reliability concerns.".to_string()
        });
        lines.push(match self.write_cache_enabled {
            Some(true) if self.power_protected => {
                "Write caching: on, power-protected (safe — the device can flush its own cache \
                 on power loss)".to_string()
            }
            Some(true) => "Write caching: on (a power loss before it flushes can lose recent \
                 writes)"
                .to_string(),
            Some(false) => "Write caching: off".to_string(),
            None => "Write caching: not reported".to_string(),
        });
        lines.push(match self.removal_policy {
            Some(RemovalPolicy::QuickRemoval) => {
                "Removal policy: Quick removal (safe to unplug any time)".to_string()
            }
            Some(RemovalPolicy::BetterPerformance) => {
                "Removal policy: Better performance (use Safely Remove Hardware before unplugging)"
                    .to_string()
            }
            None => "Removal policy: not set (Windows applies its bus-type default)".to_string(),
        });
        lines.join("\n")
    }
}

/// Read a NUL-terminated string out of a `STORAGE_DEVICE_DESCRIPTOR` result
/// buffer at a byte offset — an offset of 0 means the field isn't reported.
#[cfg(windows)]
fn read_cstr(buf: &[u8], offset: u32) -> String {
    let start = offset as usize;
    if offset == 0 || start >= buf.len() {
        return String::new();
    }
    let end = buf[start..].iter().position(|&b| b == 0).map_or(buf.len(), |p| start + p);
    String::from_utf8_lossy(&buf[start..end]).trim().to_string()
}

/// Query connection type, vendor/product/serial, and cache/removal policy
/// for the disk backing drive `letter`. Every query here uses access-0
/// handles or a registry read — no admin rights required.
#[cfg(windows)]
pub fn disk_hw_info(letter: char) -> Option<DiskHwInfo> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
        STORAGE_PROPERTY_QUERY, STORAGE_WRITE_CACHE_PROPERTY, StorageDeviceProperty,
        StorageDeviceWriteCacheProperty, WriteCacheDisabled, WriteCacheEnabled,
    };

    unsafe {
        let (disk, devnum) = physical_drive_handle(letter)?;

        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceProperty,
            QueryType: PropertyStandardQuery,
            ..Default::default()
        };
        let mut buf = [0u8; 1024];
        let mut returned = 0u32;
        let res = DeviceIoControl(
            disk,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const _),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut returned),
            None,
        );
        if res.is_err() {
            let _ = CloseHandle(disk);
            return None;
        }
        let desc = &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR);
        let bus = disk_bus_from(desc.BusType);
        let vendor = read_cstr(&buf, desc.VendorIdOffset);
        let product = read_cstr(&buf, desc.ProductIdOffset);
        let serial = read_cstr(&buf, desc.SerialNumberOffset);

        let wc_query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceWriteCacheProperty,
            QueryType: PropertyStandardQuery,
            ..Default::default()
        };
        let mut wc = STORAGE_WRITE_CACHE_PROPERTY::default();
        let wc_res = DeviceIoControl(
            disk,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&wc_query as *const _ as *const _),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(&mut wc as *mut _ as *mut _),
            std::mem::size_of::<STORAGE_WRITE_CACHE_PROPERTY>() as u32,
            Some(&mut returned),
            None,
        );
        let (write_cache_enabled, power_protected) = if wc_res.is_ok() {
            let enabled = if wc.WriteCacheEnabled == WriteCacheEnabled {
                Some(true)
            } else if wc.WriteCacheEnabled == WriteCacheDisabled {
                Some(false)
            } else {
                None
            };
            (enabled, wc.UserDefinedPowerProtection)
        } else {
            (None, false)
        };

        let removal_policy = removal_policy_for(devnum);
        let _ = CloseHandle(disk);

        Some(DiskHwInfo {
            bus,
            vendor,
            product,
            serial,
            write_cache_enabled,
            power_protected,
            removal_policy,
        })
    }
}

#[cfg(not(windows))]
pub fn disk_hw_info(_letter: char) -> Option<DiskHwInfo> {
    None
}

/// Read a NUL-terminated wide string starting at `ptr`.
#[cfg(windows)]
unsafe fn wide_cstr(ptr: *const u16) -> String {
    let mut len = 0usize;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

/// Device Manager's "Policies" tab removal policy for the physical disk with
/// device number `devnum` (from [`physical_drive_handle`]).
///
/// There's no IOCTL for this — it's a registry value under the disk's own
/// device-instance key. Getting there means walking the disk device
/// interfaces via SetupAPI to find the one whose device number matches,
/// reading its instance ID, then reading
/// `HKLM\SYSTEM\CurrentControlSet\Enum\<instance id>\Device Parameters\Classpnp\UserRemovalPolicy`
/// (3 = Quick removal, 2 = Better performance; missing = never changed from
/// the bus-type default).
#[cfg(windows)]
fn removal_policy_for(devnum: u32) -> Option<RemovalPolicy> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
        SetupDiGetDeviceInterfaceDetailW, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
    };
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
    };

    let instance_id = unsafe {
        let set = SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_DISK),
            windows::core::PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .ok()?;

        let mut idx = 0u32;
        let found = loop {
            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(set, None, &GUID_DEVINTERFACE_DISK, idx, &mut iface)
                .is_err()
            {
                break None;
            }
            idx += 1;

            let mut needed = 0u32;
            // Expected to "fail" here — this call is only a size probe.
            let _ =
                SetupDiGetDeviceInterfaceDetailW(set, &iface, None, 0, Some(&mut needed), None);
            if needed == 0 {
                continue;
            }
            let mut raw = vec![0u8; needed as usize];
            let header = raw.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*header).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut devinfo = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiGetDeviceInterfaceDetailW(
                set,
                &iface,
                Some(header),
                needed,
                None,
                Some(&mut devinfo),
            )
            .is_err()
            {
                continue;
            }
            // DevicePath is the struct's trailing flexible u16 array, right
            // after the leading `cbSize: u32`.
            let path = wide_cstr((header as *const u8).add(4) as *const u16);

            let Some(vol) = open_raw(&path) else { continue };
            let mut num = STORAGE_DEVICE_NUMBER::default();
            let mut returned = 0u32;
            let matched = DeviceIoControl(
                vol,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                None,
                0,
                Some(&mut num as *mut _ as *mut _),
                std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                Some(&mut returned),
                None,
            )
            .is_ok()
                && num.DeviceNumber == devnum;
            let _ = CloseHandle(vol);
            if !matched {
                continue;
            }

            let mut id_buf = [0u16; 512];
            if SetupDiGetDeviceInstanceIdW(set, &devinfo, Some(&mut id_buf), None).is_err() {
                break None;
            }
            let end = id_buf.iter().position(|&c| c == 0).unwrap_or(id_buf.len());
            break Some(String::from_utf16_lossy(&id_buf[..end]));
        };
        let _ = SetupDiDestroyDeviceInfoList(set);
        found
    }?;

    let key_path = format!(r"SYSTEM\CurrentControlSet\Enum\{instance_id}\Device Parameters\Classpnp");
    let key = windows_registry::LOCAL_MACHINE.open(key_path).ok()?;
    match key.get_u32("UserRemovalPolicy").ok()? {
        3 => Some(RemovalPolicy::QuickRemoval),
        2 => Some(RemovalPolicy::BetterPerformance),
        _ => None,
    }
}

/// Free / total bytes of a drive (e.g. 'A'), or `None` when the drive is
/// offline or unmapped — the Files view degrades to "offline".
#[cfg(windows)]
pub fn disk_space(letter: char) -> Option<(u64, u64)> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let wide = to_wide(&format!("{}:\\", letter.to_ascii_uppercase()));
    let mut free = 0u64;
    let mut total = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(
            windows::core::PCWSTR(wide.as_ptr()),
            Some(&mut free),
            Some(&mut total),
            None,
        )
        .ok()?;
    }
    Some((free, total))
}

#[cfg(not(windows))]
pub fn disk_space(_letter: char) -> Option<(u64, u64)> {
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
    let procs = process_tree_snapshot();

    // BFS to collect the target and all descendants.
    let mut tree = vec![pid];
    let mut i = 0;
    while i < tree.len() {
        let cur = tree[i];
        for p in &procs {
            if p.ppid == cur && !tree.contains(&p.pid) {
                tree.push(p.pid);
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
    fn disk_performance_smoke() {
        // Must not panic; when the diskperf filter answers (normal on Win11),
        // a system drive that has booted the OS has non-zero traffic.
        if let Some(p) = disk_performance('C') {
            assert!(p.bytes_read > 0 || p.bytes_written > 0);
        }
        assert!(disk_performance('!').is_none());
    }

    #[test]
    fn disk_hw_info_smoke() {
        // Must not panic; a booted system drive is never on an unresolved
        // bus, and a bogus letter must not resolve to anything.
        if let Some(hw) = disk_hw_info('C') {
            assert_ne!(hw.bus, DiskBus::Unknown);
        }
        assert!(disk_hw_info('!').is_none());
    }

    #[test]
    #[ignore = "prints real hardware info for every present drive letter — run with --ignored --nocapture"]
    fn print_disk_hw_info_all_drives() {
        for letter in 'A'..='Z' {
            if disk_space(letter).is_none() {
                continue;
            }
            match disk_hw_info(letter) {
                Some(hw) => println!(
                    "{letter}: bus={} vendor={:?} product={:?} serial={:?} \
                     write_cache_enabled={:?} power_protected={} removal_policy={:?}",
                    hw.bus.label(),
                    hw.vendor,
                    hw.product,
                    hw.serial,
                    hw.write_cache_enabled,
                    hw.power_protected,
                    hw.removal_policy
                ),
                None => println!("{letter}: disk_hw_info returned None"),
            }
        }
    }

    #[test]
    fn tree_snapshot_contains_self_with_name() {
        let procs = process_tree_snapshot();
        let me = std::process::id();
        let mine = procs.iter().find(|p| p.pid == me).expect("own pid in snapshot");
        assert!(!mine.name.is_empty());
        assert!(!mine.name.ends_with(".exe"));
    }

    #[test]
    fn pid_listening_on_finds_own_listener() {
        // Bind an ephemeral IPv4 listener in-process: the table lookup must
        // attribute its port to this test process, and a port nobody uses
        // (the one freed by dropping the listener) must return None.
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        assert_eq!(pid_listening_on(port), Some(std::process::id()));
        drop(l);
        assert_eq!(pid_listening_on(port), None);

        // IPv6 listeners are found too (the POT server prefers [::]).
        if let Ok(l6) = std::net::TcpListener::bind("[::1]:0") {
            let port6 = l6.local_addr().expect("addr6").port();
            assert_eq!(pid_listening_on(port6), Some(std::process::id()));
        }
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // test fixture I/O, not app I/O
    fn file_lock_holders_names_self() {
        // Hold a file open in-process; the Restart Manager must attribute the
        // handle to this very process (that's the whole culprit-logging
        // premise). An unheld file reports no holders.
        let dir = std::env::temp_dir();
        let p = dir.join(format!("sa-rm-selftest-{}.txt", std::process::id()));
        std::fs::write(&p, b"x").expect("write fixture");
        {
            let _held = std::fs::File::open(&p).expect("hold open");
            let holders = file_lock_holders(std::slice::from_ref(&p));
            let me = std::process::id().to_string();
            assert!(
                holders.iter().any(|h| h.contains(&format!("pid {me}"))),
                "self not reported as holder: {holders:?}"
            );
        }
        let free = file_lock_holders(std::slice::from_ref(&p));
        assert!(free.is_empty(), "no-holder file reported holders: {free:?}");
        let _ = std::fs::remove_file(&p);
    }
}
