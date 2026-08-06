//! Physical-display enumeration, for tiling player windows across monitors
//! (`crate::layout`, `crate::window_placement`). Distinct from
//! `crate::models::Monitor`, which is a tracked capture instance, not a
//! screen — an unfortunate name collision that predates this module.

/// An axis-aligned rectangle in absolute virtual-desktop pixel coordinates
/// (can be negative — a monitor to the left of or above the primary one has
/// negative `x`/`y`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PixelRect {
    pub fn geometry_arg(&self) -> String {
        format!("--geometry={}x{}+{}+{}", self.w, self.h, self.x, self.y)
    }
}

/// One connected physical display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalMonitor {
    /// Position within the `Vec` returned by [`enumerate_monitors`] for that
    /// call — stable for the lifetime of one enumeration, used as the
    /// persisted "which display" key in a saved [`crate::layout::LayoutSlot`].
    /// Not a stable OS identifier: monitors can be added/removed/reordered
    /// between calls, so a stale index is resolved leniently (falls back to
    /// monitor 0) wherever it's read back, same tolerance this codebase
    /// already applies to stale column/group ids elsewhere.
    pub index: usize,
    /// Full monitor bounds.
    pub rect: PixelRect,
    /// Bounds excluding the taskbar.
    pub work_rect: PixelRect,
    pub is_primary: bool,
    /// `GetMonitorInfoW`'s device name (e.g. `\\.\DISPLAY1`), for the Custom
    /// editor's display labels.
    pub name: String,
}

#[cfg(windows)]
pub fn enumerate_monitors() -> Vec<PhysicalMonitor> {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
    use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
    use windows::core::BOOL;

    fn to_rect(r: RECT) -> PixelRect {
        PixelRect { x: r.left, y: r.top, w: (r.right - r.left).max(0), h: (r.bottom - r.top).max(0) }
    }

    let mut out: Vec<PhysicalMonitor> = Vec::new();
    unsafe extern "system" fn callback(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let monitors = unsafe { &mut *(lparam.0 as *mut Vec<PhysicalMonitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(hmonitor, &mut info.monitorInfo) }.as_bool() {
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            let name = String::from_utf16_lossy(&info.szDevice[..len]);
            monitors.push(PhysicalMonitor {
                index: monitors.len(),
                rect: to_rect(info.monitorInfo.rcMonitor),
                work_rect: to_rect(info.monitorInfo.rcWork),
                is_primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
                name,
            });
        }
        true.into()
    }

    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(callback), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

#[cfg(not(windows))]
pub fn enumerate_monitors() -> Vec<PhysicalMonitor> {
    Vec::new()
}

/// The primary monitor, or a zero-sized placeholder if none could be
/// enumerated (headless CI, or the Win32 call failed) — callers doing rect
/// math should treat an all-zero `work_rect` as "nothing to draw", not crash.
pub fn primary_or_fallback(monitors: &[PhysicalMonitor]) -> PhysicalMonitor {
    monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .cloned()
        .unwrap_or(PhysicalMonitor {
            index: 0,
            rect: PixelRect::default(),
            work_rect: PixelRect::default(),
            is_primary: true,
            name: String::new(),
        })
}

/// Resolve a persisted monitor index against a live enumeration, falling
/// back to the primary monitor if the index is stale (monitor unplugged,
/// reordered, etc.) — same leniency as [`crate::grid_columns::resolve_sort`]
/// dropping an unknown column id rather than erroring.
pub fn resolve_monitor(monitors: &[PhysicalMonitor], index: usize) -> PhysicalMonitor {
    monitors.get(index).cloned().unwrap_or_else(|| primary_or_fallback(monitors))
}
