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

/// A Win32 Job Object that kills the entire child process tree when dropped or
/// explicitly terminated — required because killing a tool (e.g. yt-dlp) would
/// otherwise orphan the ffmpeg child it spawned.
#[cfg(windows)]
pub struct ProcessJob {
    handle: windows::Win32::Foundation::HANDLE,
}

// A job-object HANDLE is process-global and safe to use from any thread; the
// supervisor holds one across `.await` points inside a spawned task.
#[cfg(windows)]
unsafe impl Send for ProcessJob {}
#[cfg(windows)]
unsafe impl Sync for ProcessJob {}

#[cfg(windows)]
impl ProcessJob {
    pub fn new() -> Result<ProcessJob> {
        use std::ffi::c_void;
        use std::mem::size_of;
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        unsafe {
            let handle = CreateJobObjectW(None, windows::core::PCWSTR::null())?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
            Ok(ProcessJob { handle })
        }
    }

    /// Assign a spawned child (and thus its future descendants) to this job.
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
impl Drop for ProcessJob {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE terminates the whole tree when the last handle closes.
        use windows::Win32::Foundation::CloseHandle;
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Non-Windows stub: relies on `kill_on_drop` for the direct child. (Process
/// groups for full-tree kill on Unix are a later enhancement.)
#[cfg(not(windows))]
pub struct ProcessJob;

#[cfg(not(windows))]
impl ProcessJob {
    pub fn new() -> Result<ProcessJob> {
        Ok(ProcessJob)
    }
    pub fn assign_child(&self, _child: &tokio::process::Child) -> Result<()> {
        Ok(())
    }
    pub fn kill(&self) {}
}

/// Forcefully kill a process and its entire child tree by PID.
///
/// Uses a Toolhelp snapshot to find descendants (by parent PID) and terminates
/// children before parents. This reliably reaches grandchildren (e.g. the ffmpeg
/// a yt-dlp launcher spawned) even when a Python console-script wrapper created
/// its child with `CREATE_BREAKAWAY_FROM_JOB`, which escapes a Job Object.
#[cfg(windows)]
pub fn kill_process_tree(pid: u32) {
    use std::mem::size_of;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    if pid == 0 {
        return;
    }
    // Snapshot (pid, parent_pid) for all processes.
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
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
