//! Windows toast identity + COM activation: registers our own AppUserModelID
//! (`BluABK.StreamArchiver`) so toasts are branded "StreamArchiver" (name +
//! icon) instead of "Windows PowerShell", and runs an
//! `INotificationActivationCallback` COM local server so clicking a toast body
//! calls back into the app (focus/raise the window, or open the 🔔 feed) — or
//! relaunches the app to the tray when it isn't running.
//!
//! Everything is refreshed at startup from the running exe (no installer):
//! registry-based AUMID registration under
//! `HKCU\Software\Classes\AppUserModelId\<AUMID>` (`DisplayName`, `IconUri`,
//! `CustomActivator`) plus the activator CLSID's `LocalServer32` pointing at
//! `current_exe()` — rewritten every launch because dev builds move the exe.
//! The classic Start-Menu-shortcut route (a `.lnk` stamped with
//! `System.AppUserModel.ID`/`ToastActivatorCLSID`) is the fallback if registry
//! branding ever regresses; deliberately not built.
//!
//! Known benign race: a toast clicked while the app is still starting (factory
//! not yet registered) makes COM spawn a second `-Embedding` instance, which
//! loses the single-instance port guard and exits — that one click is lost.

use parking_lot::Mutex;

use crate::events::UiCommand;

/// What a foreground toast activation asks the app to do (parsed from the
/// toast's `launch`/action `arguments` string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastNav {
    /// Show and focus the main window.
    Focus,
    /// Show the window and open the 🔔 notifications feed.
    OpenNotifications,
}

/// Parse a toast activation argument string (`&`-separated `key=value` pairs,
/// e.g. `action=notifications`) into a [`ToastNav`]. Unknown or empty input
/// falls back to [`ToastNav::Focus`] — a toast click should always at least
/// raise the window.
pub fn parse_activation_args(args: &str) -> ToastNav {
    for pair in args.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k.trim() == "action"
        {
            return match v.trim() {
                "notifications" => ToastNav::OpenNotifications,
                _ => ToastNav::Focus,
            };
        }
    }
    ToastNav::Focus
}

/// The quoted `LocalServer32` command line for `exe` (COM appends `-Embedding`
/// itself when it launches the server).
#[cfg_attr(not(windows), allow(dead_code))]
fn localserver_command(exe: &std::path::Path) -> String {
    format!("\"{}\"", exe.display())
}

// ---------- UI sink: COM RPC thread -> egui ----------

/// The live UI's command channel + repaint handle, installed once eframe has
/// built the app ([`set_ui_sink`]).
static SINK: Mutex<Option<(std::sync::mpsc::Sender<UiCommand>, eframe::egui::Context)>> =
    Mutex::new(None);
/// An activation that arrived before the UI was up (the app was *launched by*
/// the toast click): stashed here and replayed from [`set_ui_sink`].
static PENDING: Mutex<Option<ToastNav>> = Mutex::new(None);

/// Install the UI command channel + egui context so toast activations can
/// reach the app, and replay any activation that arrived during startup.
pub fn set_ui_sink(tx: std::sync::mpsc::Sender<UiCommand>, ctx: eframe::egui::Context) {
    *SINK.lock() = Some((tx, ctx));
    if let Some(nav) = PENDING.lock().take() {
        deliver(nav);
    }
}

/// Route one activation to the UI, or stash it if the UI isn't up yet.
#[cfg_attr(not(windows), allow(dead_code))]
fn deliver(nav: ToastNav) {
    let guard = SINK.lock();
    match &*guard {
        Some((tx, ctx)) => {
            let cmd = match nav {
                ToastNav::Focus => UiCommand::ShowWindow,
                ToastNav::OpenNotifications => UiCommand::ShowNotifications,
            };
            if tx.send(cmd).is_ok() {
                ctx.request_repaint();
            }
        }
        None => {
            drop(guard);
            *PENDING.lock() = Some(nav);
        }
    }
}

// ---------- Windows: registration + COM server ----------

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::Context as _;
    use tracing::{info, warn};
    use windows::Win32::Foundation::CLASS_E_NOAGGREGATION;
    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoInitializeEx, CoRegisterClassObject,
        IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
    };
    use windows::Win32::UI::Notifications::{
        INotificationActivationCallback, INotificationActivationCallback_Impl,
        NOTIFICATION_USER_INPUT_DATA,
    };
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows::core::{GUID, Interface, PCWSTR, Ref, implement, w};

    use super::{deliver, localserver_command, parse_activation_args};

    /// Our registered AppUserModelID. Must stay in sync with the `w!(..)`
    /// literal in [`init`].
    pub const APP_AUMID: &str = "BluABK.StreamArchiver";

    /// The built-in PowerShell AppUserModelID — lets a Win32 app show toasts
    /// without any registration of its own. Kept as the fallback identity when
    /// [`register_registry`] fails (e.g. a locked-down registry) so toasts
    /// keep working, just unbranded.
    const POWERSHELL_AUMID: &str =
        r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

    /// The toast activator's COM class id (generated once, hard-coded — it
    /// only ever refers to this app on this machine via `LocalServer32`).
    const ACTIVATOR_CLSID: GUID = GUID::from_u128(0xA4E2B7D1_5C3F_4B8E_9A61_0D2C47F3E9B2);
    /// [`ACTIVATOR_CLSID`] in the braced-uppercase registry spelling.
    const CLSID_BRACED: &str = "{A4E2B7D1-5C3F-4B8E-9A61-0D2C47F3E9B2}";

    /// Set once [`register_registry`] succeeds; gates [`effective_aumid`].
    static AUMID_READY: AtomicBool = AtomicBool::new(false);

    /// The AUMID toasts should be shown under right now: ours when the
    /// registration succeeded this launch, else the PowerShell fallback.
    pub fn effective_aumid() -> &'static str {
        if AUMID_READY.load(Ordering::Acquire) { APP_AUMID } else { POWERSHELL_AUMID }
    }

    /// One-time startup registration. Must run before window/tray creation
    /// (the explicit process AUMID is also the taskbar identity) and before
    /// the first toast can fire.
    pub fn init() {
        // Explicit process AUMID: ties our windows and toasts to the same
        // identity. Must match APP_AUMID.
        unsafe {
            if let Err(e) = SetCurrentProcessExplicitAppUserModelID(w!("BluABK.StreamArchiver")) {
                warn!("SetCurrentProcessExplicitAppUserModelID failed: {e}");
            }
        }
        match register_registry() {
            Ok(()) => AUMID_READY.store(true, Ordering::Release),
            Err(e) => warn!(
                "toast AUMID registration failed — toasts stay under the PowerShell identity: {e:#}"
            ),
        }
        spawn_activator_thread();
    }

    /// Write (refresh) the HKCU registration: the AUMID's branding values and
    /// the activator CLSID's `LocalServer32` command line.
    fn register_registry() -> anyhow::Result<()> {
        let icon = write_icon_png().context("materializing toast icon")?;
        let root = windows_registry::CURRENT_USER;
        let k = root
            .create(format!(r"Software\Classes\AppUserModelId\{APP_AUMID}"))
            .context("creating AppUserModelId key")?;
        k.set_string("DisplayName", "StreamArchiver")?;
        k.set_string("IconUri", icon.to_string_lossy().as_ref())?;
        // Matches the app-icon tile purple so the icon plate blends in.
        k.set_string("IconBackgroundColor", "FF6A3ACF")?;
        k.set_string("CustomActivator", CLSID_BRACED)?;
        let ls = root
            .create(format!(r"Software\Classes\CLSID\{CLSID_BRACED}\LocalServer32"))
            .context("creating LocalServer32 key")?;
        let exe = std::env::current_exe().context("resolving current exe")?;
        ls.set_string("", localserver_command(&exe))?;
        Ok(())
    }

    /// Materialize the in-code app icon as a PNG at a path that survives dev
    /// rebuilds (the exe moves; `data_dir` doesn't). Rewritten only when
    /// missing or different so startups don't churn the file.
    fn write_icon_png() -> anyhow::Result<std::path::PathBuf> {
        use crate::iomon::{Cat, fs};
        let (rgba, w, h) = crate::platform::app_icon_rgba();
        let img = image::RgbaImage::from_raw(w, h, rgba)
            .ok_or_else(|| anyhow::anyhow!("icon buffer size mismatch"))?;
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        let dir = crate::app_paths::data_dir();
        // First launch runs before Store::open creates the data dir.
        fs::create_dir_all_sync(Cat::Startup, &dir)?;
        let path = dir.join("toast_icon.png");
        let unchanged =
            fs::read_sync(Cat::Startup, &path).map(|cur| cur == png).unwrap_or(false);
        if !unchanged {
            fs::write_sync(Cat::Startup, &path, &png)?;
        }
        Ok(path)
    }

    /// The COM object Windows Activates when a foreground toast is clicked.
    #[implement(INotificationActivationCallback)]
    struct ToastActivator;

    impl INotificationActivationCallback_Impl for ToastActivator_Impl {
        fn Activate(
            &self,
            _aumid: &PCWSTR,
            args: &PCWSTR,
            _data: *const NOTIFICATION_USER_INPUT_DATA,
            _count: u32,
        ) -> windows::core::Result<()> {
            let args = if args.is_null() {
                String::new()
            } else {
                unsafe { args.to_string() }.unwrap_or_default()
            };
            info!(args, "toast activated");
            deliver(parse_activation_args(&args));
            Ok(())
        }
    }

    #[implement(IClassFactory)]
    struct ActivatorFactory;

    impl IClassFactory_Impl for ActivatorFactory_Impl {
        fn CreateInstance(
            &self,
            outer: Ref<'_, windows::core::IUnknown>,
            riid: *const GUID,
            ppv: *mut *mut core::ffi::c_void,
        ) -> windows::core::Result<()> {
            if !outer.is_null() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let cb: INotificationActivationCallback = ToastActivator.into();
            unsafe { cb.query(riid, ppv).ok() }
        }
        fn LockServer(&self, _lock: windows::core::BOOL) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// Register the activator's class factory on a dedicated parked MTA
    /// thread. An MTA needs no message pump — `Activate` arrives on COM RPC
    /// worker threads — and parking keeps the apartment + factory registered
    /// for the process lifetime (never revoked).
    fn spawn_activator_thread() {
        let _ = std::thread::Builder::new().name("toast-activator".into()).spawn(|| unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let factory: IClassFactory = ActivatorFactory.into();
            match CoRegisterClassObject(
                &ACTIVATOR_CLSID,
                &factory,
                CLSCTX_LOCAL_SERVER,
                REGCLS_MULTIPLEUSE,
            ) {
                Ok(_cookie) => {
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => warn!("CoRegisterClassObject failed — toast clicks won't reach the app: {e}"),
            }
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn clsid_braced_matches_guid() {
            let g = ACTIVATOR_CLSID;
            let braced = format!(
                "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
                g.data1,
                g.data2,
                g.data3,
                g.data4[0],
                g.data4[1],
                g.data4[2],
                g.data4[3],
                g.data4[4],
                g.data4[5],
                g.data4[6],
                g.data4[7],
            );
            assert_eq!(braced, CLSID_BRACED);
        }
    }
}

#[cfg(windows)]
pub use win::{effective_aumid, init};

#[cfg(not(windows))]
pub fn init() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_maps_actions_and_defaults_to_focus() {
        assert_eq!(parse_activation_args(""), ToastNav::Focus);
        assert_eq!(parse_activation_args("action=focus"), ToastNav::Focus);
        assert_eq!(parse_activation_args("action=notifications"), ToastNav::OpenNotifications);
        // Unknown action values and garbage still raise the window.
        assert_eq!(parse_activation_args("action=whatever"), ToastNav::Focus);
        assert_eq!(parse_activation_args("garbage"), ToastNav::Focus);
        // The action key wins regardless of position among other pairs.
        assert_eq!(parse_activation_args("a=b&action=notifications"), ToastNav::OpenNotifications);
    }

    #[test]
    fn localserver_command_quotes_path() {
        let cmd = localserver_command(std::path::Path::new(r"C:\Program Files\SA\sa.exe"));
        assert_eq!(cmd, r#""C:\Program Files\SA\sa.exe""#);
    }
}
