// In release builds, run as a GUI app (no console window). In debug, keep the
// console so tracing logs are visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_core;
mod app_paths;
mod detectors;
mod downloader;
mod events;
mod models;
mod platform;
mod scheduler;
mod store;
mod ui;

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use anyhow::{Context, Result};
use eframe::egui;
use tracing::info;
use tray_icon::TrayIcon;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};

use app_core::AppCore;
use detectors::{DetectContext, DetectItem};
use events::UiCommand;
use models::Platform;
use store::Store;

fn main() -> Result<()> {
    init_tracing();

    // Diagnostics that run and exit (also handy for scripting/headless use).
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--probe") {
        return run_probe(args.get(pos + 1).cloned().unwrap_or_default());
    }
    if let Some(pos) = args.iter().position(|a| a == "--add") {
        return run_add(&args, pos);
    }
    if args.iter().any(|a| a == "--list") {
        return run_list();
    }

    // Single-instance guard: hold the loopback bind for the process lifetime.
    let _instance_guard = match platform::acquire_single_instance() {
        Some(guard) => guard,
        None => {
            info!("another StreamArchiver instance is already running; exiting");
            return Ok(());
        }
    };

    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    let core = AppCore::new(Arc::new(store)).context("starting core runtime")?;
    core.start(); // launch the background poll scheduler

    // `--hidden` (used by the autostart entry) launches straight to the tray.
    let start_hidden = std::env::args().any(|a| a == "--hidden");
    info!(start_hidden, "core started; launching UI");

    let (rgba, w, h) = platform::app_icon_rgba();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("StreamArchiver")
            .with_inner_size([960.0, 600.0])
            .with_min_inner_size([680.0, 420.0])
            .with_visible(!start_hidden)
            .with_icon(egui::IconData {
                rgba,
                width: w,
                height: h,
            }),
        ..Default::default()
    };

    let core_for_app = core.clone();
    eframe::run_native(
        "StreamArchiver",
        native_options,
        Box::new(move |cc| {
            let (tray, ui_rx) = build_tray(cc.egui_ctx.clone())
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(Box::new(ui::StreamArchiverApp::new(
                core_for_app,
                tray,
                ui_rx,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed: {e}"))?;

    Ok(())
}

/// Create the system tray icon + menu and a background thread that forwards
/// menu events to the UI (waking the reactive egui loop via `request_repaint`).
fn build_tray(ctx: egui::Context) -> Result<(TrayIcon, Receiver<UiCommand>)> {
    let menu = Menu::new();
    let open_item = MenuItem::new("Open StreamArchiver", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append(&open_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("StreamArchiver")
        .with_icon(platform::tray_icon_image()?)
        .build()?;

    let (tx, rx) = std::sync::mpsc::channel::<UiCommand>();
    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    let menu_rx = MenuEvent::receiver().clone();

    std::thread::Builder::new()
        .name("tray-events".into())
        .spawn(move || {
            // Blocks until a tray menu event arrives, so it is idle (~0 CPU)
            // when nothing is happening.
            while let Ok(event) = menu_rx.recv() {
                let cmd = if event.id == open_id {
                    Some(UiCommand::ShowWindow)
                } else if event.id == quit_id {
                    Some(UiCommand::Quit)
                } else {
                    None
                };
                if let Some(cmd) = cmd {
                    if tx.send(cmd).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            }
        })
        .context("spawning tray event thread")?;

    // Keep the menu item handles alive for the life of the process so their ids
    // remain valid for event matching (the tray owns the Menu itself).
    std::mem::forget(open_item);
    std::mem::forget(quit_item);

    Ok((tray, rx))
}

/// One-shot detection for diagnostics. Uses scrape/probe (no credentials),
/// except Twitch which falls through to the generic streamlink probe.
fn run_probe(url: String) -> Result<()> {
    if url.is_empty() {
        anyhow::bail!("usage: streamarchiver --probe <url>");
    }
    let store = Arc::new(Store::open(&app_paths::db_path()).context("opening data store")?);
    let rt = tokio::runtime::Runtime::new()?;
    let ctx = DetectContext::new(store);
    let platform = Platform::detect(&url);
    let item = DetectItem {
        monitor_id: 0,
        url: url.clone(),
        platform,
    };
    let outcome = rt.block_on(ctx.detect_scrape(&item));
    println!(
        "url={url}\nplatform={platform:?}\nlive={}\nerror={}\ndetail={}",
        outcome.live, outcome.error, outcome.detail
    );
    Ok(())
}

/// `--add <name> <url> [method]` inserts a channel + one monitor.
fn run_add(args: &[String], pos: usize) -> Result<()> {
    let name = args.get(pos + 1).cloned().unwrap_or_default();
    let url = args.get(pos + 2).cloned().unwrap_or_default();
    if name.is_empty() || url.is_empty() {
        anyhow::bail!("usage: streamarchiver --add <name> <url> [detection_method]");
    }
    let platform = Platform::detect(&url);
    let method = args
        .get(pos + 3)
        .map(|s| models::DetectionMethod::parse(s))
        .unwrap_or_else(|| platform.default_detection());

    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    let channel_id = store.upsert_channel(&name, &url, platform)?;
    let monitor = models::Monitor {
        id: 0,
        channel_id,
        enabled: true,
        tool: platform.default_tool(),
        detection_method: method,
        poll_interval_secs: 30,
        quality: "best".into(),
        output_dir: app_paths::default_output_dir().to_string_lossy().to_string(),
        filename_template: "{author}_{time}".into(),
        container: models::Container::Mkv,
        extra_args: String::new(),
        max_concurrent: 1,
        last_checked_at: None,
        last_state: "idle".into(),
    };
    let monitor_id = store.insert_monitor(&monitor)?;
    println!("added monitor {monitor_id} (channel {channel_id}, {platform:?}, {method:?})");
    Ok(())
}

/// `--list` prints all monitors with their current detection state.
fn run_list() -> Result<()> {
    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    for r in store.list_monitors_with_channels()? {
        println!(
            "[{}] {:<20} {:<8} {:<14} state={:<8} checked={:?}",
            r.monitor.id,
            r.channel.name,
            r.channel.platform.as_str(),
            r.monitor.detection_method.as_str(),
            r.monitor.last_state,
            r.monitor.last_checked_at,
        );
    }
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,streamarchiver=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
