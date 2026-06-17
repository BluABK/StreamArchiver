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
