//! Regression probe for the "popups vanish when the main window is
//! minimized" bug — the one `ui::StreamArchiverApp::popup_windows` exists to
//! prevent. Not part of the app; run it by hand:
//!
//! ```text
//! cargo run --example vp_minimize_probe                     # expect PASS
//! PROBE_DECLARE_IN=ui cargo run --example vp_minimize_probe # reproduces the bug
//! ```
//!
//! It opens a root window plus one `show_viewport_deferred` child, minimizes
//! the root, and checks (via `EnumWindows`) whether the child's **HWND is
//! still the same one**. A destroyed-and-recreated child is exactly what the
//! user sees as "the popup minimized with the main window / came back in the
//! wrong place". Then it repeats the check for the hide-to-tray path
//! (`ViewportCommand::Visible(false)`), which the app also relies on.
//!
//! The distinction that matters: eframe skips `App::ui` whenever the root
//! viewport reports itself invisible — and `ViewportInfo::visible()` derives
//! that from `minimized`/`occluded`, so a merely minimized window counts.
//! `App::logic` keeps running. A deferred viewport not re-declared during a
//! pass is garbage-collected at the end of it, taking its native window with
//! it. Hence: declare popups from `logic()`, never from `ui()`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId,
};
use windows::core::BOOL;

/// HWNDs of this process's windows titled `PROBE CHILD`, filled by [`cb`].
static CHILD_HWNDS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let want = lparam.0 as u32;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == want {
        let mut buf = [0u16; 64];
        let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if String::from_utf16_lossy(&buf[..n as usize]) == "PROBE CHILD" {
            CHILD_HWNDS.lock().unwrap().push(hwnd.0 as isize);
        }
    }
    BOOL(1)
}

/// The child window's HWND right now, or `None` if it doesn't exist.
fn child_hwnd() -> Option<isize> {
    CHILD_HWNDS.lock().unwrap().clear();
    let _ = unsafe { EnumWindows(Some(cb), LPARAM(std::process::id() as isize)) };
    CHILD_HWNDS.lock().unwrap().first().copied()
}

struct App {
    t0: Instant,
    step: usize,
    /// `false` = declare the child from `logic()` (what the app does), `true` =
    /// from `ui()` (reproduces the bug).
    declare_in_ui: bool,
    baseline: Option<isize>,
    failures: Vec<String>,
    child_calls: Arc<Mutex<u64>>,
}

impl App {
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    fn declare_child(&self, ctx: &egui::Context) {
        let calls = self.child_calls.clone();
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("probe_child"),
            egui::ViewportBuilder::default()
                .with_title("PROBE CHILD")
                .with_inner_size([320.0, 200.0]),
            move |ui, _class| {
                *calls.lock().unwrap() += 1;
                egui::CentralPanel::default().show(ui.ctx(), |ui| {
                    ui.label("probe child");
                });
            },
        );
    }

    /// Compare the child's HWND against the one seen before the root was
    /// minimized/hidden. Same HWND = survived; anything else = destroyed.
    fn check(&mut self, what: &str) {
        let now = child_hwnd();
        let ok = now.is_some() && now == self.baseline;
        println!(
            "  {what}: child hwnd {:?} (baseline {:?}) -> {}",
            now,
            self.baseline,
            if ok { "SURVIVED" } else { "DESTROYED" }
        );
        if !ok {
            self.failures.push(what.to_string());
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        if !self.declare_in_ui {
            self.declare_child(ctx);
        }

        let secs = self.t0.elapsed().as_secs_f32();
        match (self.step, secs) {
            (0, s) if s > 1.5 => {
                self.baseline = child_hwnd();
                println!("baseline child hwnd: {:?}", self.baseline);
                println!("=> Minimized(true)");
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.step = 1;
            }
            (1, s) if s > 3.5 => {
                self.check("after minimize");
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                self.step = 2;
            }
            (2, s) if s > 4.5 => {
                println!("=> Visible(false) (hide-to-tray path)");
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.step = 3;
            }
            (3, s) if s > 6.5 => {
                self.check("after hide-to-tray");
                let mode = if self.declare_in_ui { "ui()" } else { "logic()" };
                if self.failures.is_empty() {
                    println!("PASS (declared in {mode}) — child window survived both");
                    std::process::exit(0);
                }
                println!("FAIL (declared in {mode}) — destroyed at: {:?}", self.failures);
                std::process::exit(1);
            }
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        ui.label("probe root");
        if self.declare_in_ui {
            let ctx = ui.ctx().clone();
            self.declare_child(&ctx);
        }
    }
}

fn main() -> eframe::Result<()> {
    let declare_in_ui = std::env::var("PROBE_DECLARE_IN").as_deref() == Ok("ui");
    eframe::run_native(
        "PROBE ROOT",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([420.0, 240.0]),
            ..Default::default()
        },
        Box::new(move |_cc| {
            Ok(Box::new(App {
                t0: Instant::now(),
                step: 0,
                declare_in_ui,
                baseline: None,
                failures: Vec::new(),
                child_calls: Arc::new(Mutex::new(0)),
            }))
        }),
    )
}
