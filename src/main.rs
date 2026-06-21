// In release builds, run as a GUI app (no console window). In debug, keep the
// console so tracing logs are visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_core;
mod app_paths;
mod chat;
mod detectors;
mod downloader;
mod events;
mod eventsub;
mod models;
mod notifications;
mod oauth;
mod platform;
mod scheduler;
mod store;
mod ui;
mod websub;

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
    if args.iter().any(|a| a == "--recordings") {
        return run_recordings();
    }
    if args.iter().any(|a| a == "--twitch-login") {
        return run_twitch_login();
    }
    if let Some(pos) = args.iter().position(|a| a == "--set-setting") {
        let key = args.get(pos + 1).cloned().unwrap_or_default();
        let value = args.get(pos + 2).cloned().unwrap_or_default();
        let store = Store::open(&app_paths::db_path())?;
        store.set_setting(&key, &value)?;
        println!("set {key}");
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| a == "--capture-test") {
        return run_capture_test(&args, pos);
    }
    if let Some(pos) = args.iter().position(|a| a == "--run-for") {
        let secs: u64 = args.get(pos + 1).and_then(|s| s.parse().ok()).unwrap_or(30);
        return run_headless(secs);
    }
    if let Some(pos) = args.iter().position(|a| a == "--manual-test") {
        let id: i64 = args.get(pos + 1).and_then(|s| s.parse().ok()).unwrap_or(1);
        let secs: u64 = args.get(pos + 2).and_then(|s| s.parse().ok()).unwrap_or(12);
        return run_manual_test(id, secs);
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
    // Crash recovery: any recording/download left mid-flight by a previous run is stale.
    match store.mark_orphaned_recordings(models::now_unix()) {
        Ok(n) if n > 0 => info!("marked {n} orphaned recording(s) from a previous run"),
        Ok(_) => {}
        Err(e) => tracing::warn!("orphan recovery failed: {e:#}"),
    }
    match store.mark_orphaned_videos(models::now_unix()) {
        Ok(n) if n > 0 => info!("marked {n} orphaned video download(s) from a previous run"),
        Ok(_) => {}
        Err(e) => tracing::warn!("video orphan recovery failed: {e:#}"),
    }
    let core = AppCore::new(Arc::new(store)).context("starting core runtime")?;
    core.start(); // launch the background scheduler + download supervisor

    // `--hidden` (used by the autostart entry) launches straight to the tray.
    let start_hidden = std::env::args().any(|a| a == "--hidden");
    info!(start_hidden, "core started; launching UI");

    let (rgba, w, h) = platform::app_icon_rgba();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(concat!(
                "StreamArchiver v",
                env!("APP_VERSION"),
                " (",
                env!("GIT_HASH"),
                ")"
            ))
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

    // The UI loop has exited (Quit) — tear down any active recording trees so we
    // don't orphan streamlink/yt-dlp/ffmpeg processes.
    info!("shutting down; stopping active recordings");
    core.stop_all_recordings();

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
        .with_tooltip(concat!(
            "StreamArchiver v",
            env!("APP_VERSION"),
            " (",
            env!("GIT_HASH"),
            ")"
        ))
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

/// `--add <name> <url> [method] [tool]` inserts a channel + one monitor.
/// Output dir honors the `STREAMARCHIVER_OUT` env var (else the default).
fn run_add(args: &[String], pos: usize) -> Result<()> {
    let name = args.get(pos + 1).cloned().unwrap_or_default();
    let url = args.get(pos + 2).cloned().unwrap_or_default();
    if name.is_empty() || url.is_empty() {
        anyhow::bail!("usage: streamarchiver --add <name> <url> [detection_method] [tool]");
    }
    let platform = Platform::detect(&url);
    let method = args
        .get(pos + 3)
        .map(|s| models::DetectionMethod::parse(s))
        .unwrap_or_else(|| platform.default_detection());
    let tool = args
        .get(pos + 4)
        .map(|s| models::Tool::parse(s))
        .unwrap_or_else(|| platform.default_tool());
    let output_dir = std::env::var("STREAMARCHIVER_OUT").unwrap_or_else(|_| {
        app_paths::default_output_dir()
            .to_string_lossy()
            .to_string()
    });

    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    let channel_id = store.upsert_channel(&name, &url, platform)?;
    let monitor = models::Monitor {
        id: 0,
        channel_id,
        url: url.clone(),
        enabled: true,
        tool,
        detection_method: method,
        poll_interval_secs: 30,
        quality: "best".into(),
        output_dir,
        filename_template: "{name}_{date}_{time}".into(),
        container: models::Container::Mkv,
        capture_from_start: std::env::var("STREAMARCHIVER_FROM_START").as_deref() != Ok("0"),
        ad_free: false,
        auth_kind: models::AuthKind::Inherit,
        auth_value: String::new(),
        audio_tracks: "all".into(),
        subtitle_tracks: "all".into(),
        chat_log: true,
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
            "[{}] {:<20} {:<8} {:<14} state={:<8} {}",
            r.monitor.id,
            r.channel.name,
            r.monitor.platform().as_str(),
            r.monitor.detection_method.as_str(),
            r.monitor.last_state,
            r.monitor.url,
        );
    }
    Ok(())
}

/// `--capture-test <tool> <url> <secs>` records for a fixed time, kills the tree
/// (taskkill /T), and remuxes — exercising the real capture lifecycle end-to-end.
fn run_capture_test(args: &[String], pos: usize) -> Result<()> {
    use std::process::Stdio;
    let tool = models::Tool::parse(&args.get(pos + 1).cloned().unwrap_or_default());
    let url = args.get(pos + 2).cloned().unwrap_or_default();
    let secs: u64 = args.get(pos + 3).and_then(|s| s.parse().ok()).unwrap_or(15);
    if url.is_empty() {
        anyhow::bail!("usage: streamarchiver --capture-test <tool> <url> <secs>");
    }
    let out_dir = std::env::var("STREAMARCHIVER_OUT").unwrap_or_else(|_| {
        app_paths::default_output_dir()
            .to_string_lossy()
            .to_string()
    });
    let row = models::MonitorWithChannel {
        channel: models::Channel {
            id: 0,
            name: "captest".into(),
            url: url.clone(),
            platform: Platform::detect(&url),
            created_at: 0,
        },
        monitor: models::Monitor {
            id: 0,
            channel_id: 0,
            url: url.clone(),
            enabled: true,
            tool,
            detection_method: models::DetectionMethod::GenericProbe,
            poll_interval_secs: 60,
            quality: "best".into(),
            output_dir: out_dir,
            filename_template: "captest_{time}".into(),
            container: models::Container::Mkv,
            capture_from_start: false,
            ad_free: false,
            auth_kind: models::AuthKind::Inherit,
            auth_value: String::new(),
            audio_tracks: String::new(),
            subtitle_tracks: String::new(),
            chat_log: false,
            extra_args: String::new(),
            max_concurrent: 1,
            last_checked_at: None,
            last_state: "idle".into(),
        },
        last_recording_started: None,
        last_recording_ended: None,
        last_recording_status: None,
        last_recording_went_live: None,
        last_recording_went_live_approx: false,
        last_recording_lost_secs: None,
        last_recording_ad_count: 0,
        last_recording_ad_secs: 0,
        last_recording_meta_changes: 0,
        last_recording_title: String::new(),
        last_recording_category: String::new(),
        last_recording_log: String::new(),
        ad_free_sub: None,
        recording_count: 0,
        next_stream_at: None,
        next_stream_title: String::new(),
    };
    let plan =
        downloader::build_plan(&row, models::now_unix(), &downloader::AuthSource::None, None, None);
    println!("plan: {} {:?}", plan.program, plan.args);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        if let Some(parent) = plan.capture_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let mut cmd = tokio::process::Command::new(&plan.program);
        cmd.args(&plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("spawn tool")?;
        let pid = child.id().unwrap_or(0);
        println!(
            "spawned {} pid={pid}; recording {secs}s -> {}",
            plan.program,
            plan.capture_path.display()
        );
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;

        platform::kill_process_tree(pid);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let ts_len = tokio::fs::metadata(&plan.capture_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        println!(
            "captured .ts: {} KB (survives the hard kill)",
            ts_len / 1024
        );
        if plan.remux_to_mkv && ts_len > 0 {
            match downloader::remux_ts_to_mkv(&plan.capture_path, &plan.final_path).await {
                Ok(()) => {
                    let mkv = tokio::fs::metadata(&plan.final_path)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);
                    println!(
                        "remuxed -> {} ({} KB)",
                        plan.final_path.display(),
                        mkv / 1024
                    );
                }
                Err(e) => println!("remux failed: {e:#}"),
            }
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

/// `--run-for <secs>` runs the full core (scheduler + supervisor) headlessly for
/// a fixed time, then gracefully stops — for testing the real pipeline and as a
/// no-UI daemon mode.
fn run_headless(secs: u64) -> Result<()> {
    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    let _ = store.mark_orphaned_recordings(models::now_unix());
    let _ = store.mark_orphaned_videos(models::now_unix());
    let core = AppCore::new(Arc::new(store)).context("starting core runtime")?;
    core.start();
    info!("headless: running core for {secs}s");
    std::thread::sleep(std::time::Duration::from_secs(secs));
    info!("headless: stopping");
    core.stop_all_recordings();
    Ok(())
}

/// `--manual-test <monitor_id> <secs>` exercises the on-demand Start/Stop path:
/// it DISABLES the monitor (so the scheduler won't touch it), then manually
/// Starts it, records for <secs>, and Stops it.
fn run_manual_test(id: i64, secs: u64) -> Result<()> {
    use events::ManualCommand;
    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    store.set_monitor_enabled(id, false)?; // prove the scheduler isn't doing it
    let core = AppCore::new(Arc::new(store)).context("starting core")?;
    core.start();
    info!("manual-test: Start({id})");
    core.manual(ManualCommand::Start(id));
    std::thread::sleep(std::time::Duration::from_secs(secs));
    info!("manual-test: Stop({id})");
    core.manual(ManualCommand::Stop(id));
    std::thread::sleep(std::time::Duration::from_secs(5));
    core.stop_all_recordings();
    Ok(())
}

/// `--twitch-login` runs the Twitch device-code OAuth flow interactively.
fn run_twitch_login() -> Result<()> {
    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    let client_id = store.get_setting("twitch_client_id")?.unwrap_or_default();
    if client_id.is_empty() {
        anyhow::bail!("set a Twitch Client ID in Settings first");
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        let dc = oauth::start_device(&http, &client_id).await?;
        println!(
            "\n  Open: {}\n  Enter code: {}\n",
            dc.verification_uri, dc.user_code
        );
        println!("Waiting for authorization…");
        let tokens = oauth::poll_token(&http, &client_id, &dc).await?;
        match oauth::fetch_user(&http, &client_id, &tokens.access).await {
            Ok((login, user_id)) => {
                oauth::store_tokens(&store, &tokens, &login)?;
                store.set_setting(oauth::K_USER_ID, &user_id)?;
                println!("Connected as {login}.");
            }
            // Keep the valid tokens (detection only needs the token); the account
            // lookup failed, so ad-free sub detection stays off until a reconnect.
            Err(e) => {
                oauth::store_tokens(&store, &tokens, "")?;
                println!("Connected (couldn't read account: {e}). Reconnect later to enable ad-free sub detection.");
            }
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

/// `--recordings` prints the recent recording log.
fn run_recordings() -> Result<()> {
    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    for r in store.recent_recordings(50)? {
        let went = match r.went_live_at {
            Some(w) => format!("{}{}", if r.went_live_approx { "~" } else { "" }, w),
            None => "-".into(),
        };
        let lost = match r.went_live_at {
            Some(w) => format!("{}s", (r.started_at - w).max(0)),
            None => "-".into(),
        };
        println!(
            "rec[{}] monitor={} status={:<10} bytes={:<9} started={} went_live={} lost={} {}",
            r.id, r.monitor_id, r.status, r.bytes, r.started_at, went, lost, r.output_path
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
