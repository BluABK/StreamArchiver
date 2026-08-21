//! Applying a [`crate::layout`]-resolved [`PixelRect`] to a real OS window.
//!
//! Two paths, chosen by the caller based on the configured player
//! (`crate::ui::player::player_is_mpv`):
//! - **mpv**: [`mpv_geometry_args`] — pass `--geometry=WxH+X+Y` at spawn time
//!   (absolute virtual-desktop pixel coordinates, so there's no mpv-vs-Win32
//!   monitor-index mismatch to worry about). No process/window matching
//!   needed, no race.
//! - **anything else**: [`place_window_for_pid_tree`] — spawn a short poll
//!   loop that waits for the target process (or one of its descendants —
//!   Streamlink spawns the actual player as a *child*, so the window we want
//!   usually doesn't belong to the pid we spawned) to open a visible
//!   top-level window, then moves/resizes it with `SetWindowPos`. Best
//!   effort: playback is never gated on this succeeding.

use std::time::Duration;

use crate::display::PixelRect;

/// How long [`place_window_for_pid_tree`] polls before giving up.
pub const PLACEMENT_TIMEOUT: Duration = Duration::from_secs(8);

/// mpv CLI args that put its window at `rect` immediately at launch.
pub fn mpv_geometry_args(rect: PixelRect) -> Vec<String> {
    vec![rect.geometry_arg()]
}

/// Every pid in `root_pid`'s process tree (itself + all descendants),
/// re-derived fresh from a Toolhelp snapshot — needed because a launcher
/// like Streamlink spawns its player as a child, sometimes a moment after
/// `root_pid` itself started.
#[cfg(windows)]
fn tracked_pids(root_pid: u32) -> std::collections::HashSet<u32> {
    let procs = crate::platform::process_tree_snapshot();
    let mut tree = vec![root_pid];
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
    tree.into_iter().collect()
}

#[cfg(windows)]
pub(crate) fn find_top_level_window_for_pid_tree(
    root_pid: u32,
) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindow, GetWindowThreadProcessId, IsWindowVisible, GW_OWNER,
    };
    use windows::core::BOOL;

    let pids = tracked_pids(root_pid);
    if pids.is_empty() {
        return None;
    }

    struct Ctx {
        pids: std::collections::HashSet<u32>,
        found: Option<HWND>,
    }
    let mut ctx = Ctx { pids, found: None };

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return true.into();
            }
            // Only top-level, unowned windows — skips tooltips/owned popups
            // a player's own UI may create alongside its main window.
            if GetWindow(hwnd, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false) {
                return true.into();
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if ctx.pids.contains(&pid) {
                ctx.found = Some(hwnd);
                return false.into(); // stop enumeration, we found one
            }
        }
        true.into()
    }

    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.found
}

#[cfg(windows)]
fn apply_rect(hwnd: windows::Win32::Foundation::HWND, rect: PixelRect) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, HWND_TOP, SWP_NOACTIVATE, SWP_NOZORDER, SW_RESTORE,
    };
    unsafe {
        // A maximized/minimized window ignores SetWindowPos's size until
        // restored first.
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Spawn a background thread that waits (up to [`PLACEMENT_TIMEOUT`]) for
/// `root_pid` or a descendant to open a visible top-level window, then moves
/// it to `rect`. Fire-and-forget, matching this codebase's other watcher
/// threads (e.g. `spawn_logged`'s own child-exit reaper) — logs a warning
/// and gives up silently on timeout rather than affecting playback.
#[cfg(windows)]
pub fn place_window_for_pid_tree(root_pid: u32, rect: PixelRect, timeout: Duration) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(hwnd) = find_top_level_window_for_pid_tree(root_pid) {
                apply_rect(hwnd, rect);
                return;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    root_pid,
                    ?timeout,
                    "layout: gave up waiting for a player window to tile"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    });
}

#[cfg(not(windows))]
pub fn place_window_for_pid_tree(_root_pid: u32, _rect: PixelRect, _timeout: Duration) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpv_geometry_args_shape() {
        let rect = PixelRect { x: -100, y: 50, w: 800, h: 600 };
        assert_eq!(mpv_geometry_args(rect), vec!["--geometry=800x600+-100+50".to_string()]);
    }
}
