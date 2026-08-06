//! Throwaway probe: how do a root viewport and a deferred child viewport share
//! repaint scheduling when both want to repaint?
//!
//! `cargo run --example vp_repaint_probe` — a reporter thread prints
//! passes/second for each, so a starved viewport shows up even if it is the
//! one that stopped running. Env knobs:
//!
//! * `PROBE_CHILD_MS` (default 16)  — what the child asks for each pass.
//! * `PROBE_ROOT_MS`  (default 1000) — what the root asks for each pass.
//!
//! Animated emotes in the chat popup schedule themselves exactly this way
//! (`ctx.request_repaint_after(remaining)`), and the main window asks for its
//! 1 Hz heartbeat the same way, so this is the two of them competing.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use eframe::egui;

struct App {
    root_passes: Arc<AtomicU64>,
    child_passes: Arc<AtomicU64>,
    root_ms: u64,
    child_ms: u64,
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        self.root_passes.fetch_add(1, Ordering::Relaxed);
        ctx.request_repaint_after(Duration::from_millis(self.root_ms));

        let child_passes = self.child_passes.clone();
        let child_ms = self.child_ms;
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("repaint_child"),
            egui::ViewportBuilder::default()
                .with_title("REPAINT CHILD")
                .with_inner_size([320.0, 160.0]),
            move |ui, _class| {
                let n = child_passes.fetch_add(1, Ordering::Relaxed) + 1;
                ui.ctx().request_repaint_after(Duration::from_millis(child_ms));
                ui.label(format!("child passes: {n}"));
            },
        );
    }

    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        ui.label("repaint root");
    }
}

fn main() -> eframe::Result<()> {
    let root_ms = env_ms("PROBE_ROOT_MS", 1000);
    let child_ms = env_ms("PROBE_CHILD_MS", 16);
    let root_passes = Arc::new(AtomicU64::new(0));
    let child_passes = Arc::new(AtomicU64::new(0));

    // Report from a separate thread: whichever viewport is starved can't be
    // trusted to report on itself.
    {
        let (r, c) = (root_passes.clone(), child_passes.clone());
        std::thread::spawn(move || {
            println!("root asks {root_ms}ms, child asks {child_ms}ms");
            let t0 = Instant::now();
            let (mut pr, mut pc) = (0, 0);
            for _ in 0..6 {
                std::thread::sleep(Duration::from_secs(1));
                let (nr, nc) = (r.load(Ordering::Relaxed), c.load(Ordering::Relaxed));
                println!("  t={:.0}s  root +{}/s  child +{}/s", t0.elapsed().as_secs_f32(), nr - pr, nc - pc);
                (pr, pc) = (nr, nc);
            }
            std::process::exit(0);
        });
    }

    eframe::run_native(
        "REPAINT ROOT",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([420.0, 200.0]),
            ..Default::default()
        },
        Box::new(move |_cc| Ok(Box::new(App { root_passes, child_passes, root_ms, child_ms }))),
    )
}

fn env_ms(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
