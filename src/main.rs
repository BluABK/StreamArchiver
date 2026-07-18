// In release builds, run as a GUI app (no console window). In debug, keep the
// console so tracing logs are visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_core;
mod app_paths;
mod assets;
mod browser_ua;
mod chat;
mod compat;
mod detectors;
mod downloader;
mod emoji;
mod emote_anim;
mod events;
mod eventsub;
mod fonts;
mod google_oauth;
mod grid_columns;
mod head_backfill;
mod hls_preview;
mod imports;
mod inspector;
mod io_gate;
mod iomon;
mod logfmt;
mod models;
mod notifications;
mod oauth;
mod platform;
mod platform_pref;
mod pot_server;
mod recovery;
mod schedule_ocr;
mod schedule_source;
mod scheduled_recordings;
mod scheduler;
mod store;
mod toast_activation;
mod triggers;
mod ui;
mod version;
mod vod_archive;
mod watchdog;
mod websub;

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

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
    let _tracing_guard = init_tracing();

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

    // GUI path only (the CLI diagnostic sub-commands above all returned already):
    // turn a process-killing panic into a visible native dialog before the runtime
    // aborts, so startup panics (store/core/tray/font setup) aren't silent crashes.
    watchdog::install_panic_dialog();

    // Toast identity + COM activation: register our AUMID (branded toasts) and
    // the toast-activator class factory. Before window/tray creation (the
    // explicit process AUMID is also the taskbar identity) and before
    // `core.start()` (the first toast needs the registration in place).
    toast_activation::init();

    let store = Store::open(&app_paths::db_path()).context("opening data store")?;

    // One-time asset-cache migration to the per-account layout
    // (channel_assets/{name}/{platform}/{account}/) — synchronous, before the
    // core starts so no fetch can race the file moves.
    assets::migrate_assets_to_account_dirs(&store);

    // Load the optional custom crash/freeze dialog icon (set in Settings → Diagnostics).
    let dialog_icon_path = store
        .get_setting(models::K_DIALOG_ICON)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    watchdog::set_dialog_icon(dialog_icon_path);

    // Per-disk I/O limits (gate permits, ffmpeg -readrate, yt-dlp
    // --limit-rate) from the persisted config; updated live on settings save.
    // The legacy global keys seed the defaults when the per-disk config has
    // never been saved.
    let disk_cfg = store
        .get_setting(io_gate::K_DISK_LIMITS)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str::<io_gate::DiskLimitsConfig>(&s).ok())
        .unwrap_or_else(|| {
            let mut c = io_gate::DiskLimitsConfig::default();
            c.default.readrate = store
                .get_setting(io_gate::K_POSTPROC_READRATE)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(io_gate::DEFAULT_READRATE);
            c.default.rate_limit = store
                .get_setting(io_gate::K_DOWNLOAD_RATE_LIMIT)
                .ok()
                .flatten()
                .unwrap_or_default();
            c
        });
    io_gate::set_disk_limits(disk_cfg);
    // yt-dlp postprocessor args (throttle for its internal ffmpeg passes).
    io_gate::set_ytdlp_ppa(
        &store
            .get_setting(io_gate::K_YTDLP_PPA)
            .ok()
            .flatten()
            .unwrap_or_default(),
    );

    // Crash recovery for on-demand downloads: any left mid-flight is stale.
    // (In-flight live recordings are handled in `core.start()` →
    // `Supervisor::resume_inflight`, which resumes SABR-resumable captures and
    // orphans the rest, then sweeps stale `.cache\` working files.)
    match store.mark_orphaned_videos(models::now_unix()) {
        Ok(n) if n > 0 => info!("marked {n} orphaned video download(s) from a previous run"),
        Ok(_) => {}
        Err(e) => tracing::warn!("video orphan recovery failed: {e:#}"),
    }
    // Retention: prune notifications older than 90 days on startup so the feed
    // table doesn't grow unbounded.
    match store.prune_notifications(90) {
        Ok(n) if n > 0 => info!("pruned {n} notification(s) older than 90 days"),
        Ok(_) => {}
        Err(e) => tracing::warn!("notification pruning failed: {e:#}"),
    }
    let core = AppCore::new(Arc::new(store)).context("starting core runtime")?;
    // Runtime handle registration + the dynamic disk-gate adjuster both live
    // in AppCore::new/start now, so every entry point gets them — see the
    // comments there.
    core.start(); // launch the background scheduler + download supervisor

    // `--hidden` (used by the autostart entry) launches straight to the tray.
    // `-Embedding` is COM launching us to deliver a toast click while we
    // weren't running — same deal: start to the tray; the delivered
    // activation then shows/focuses the window.
    let start_hidden = std::env::args().any(|a| a == "--hidden" || a == "-Embedding");
    info!(start_hidden, "core started; launching UI");

    let (rgba, w, h) = platform::app_icon_rgba();
    let native_options = eframe::NativeOptions {
        // Persist egui's Memory to disk so resized column widths, window
        // positions, and other UI state survive restarts.
        persistence_path: Some(crate::app_paths::data_dir().join("egui_state.ron")),
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

    // UI-freeze watchdog: the app stamps a heartbeat each frame; this background
    // thread pops a native dialog (off the UI thread) if the heartbeat goes stale
    // while the UI is meant to be rendering, so a hard GUI hang surfaces as an
    // error instead of a silent "Not Responding" freeze. Started right before the
    // UI loop so the gap to the first frame can't trip the threshold.
    let heartbeat = watchdog::Heartbeat::new();
    watchdog::start_watchdog(
        heartbeat.clone(),
        std::time::Duration::from_secs(10),
        false, // inform-only: downloads are detached, so don't auto-kill the UI
    );

    let core_for_app = core.clone();
    eframe::run_native(
        "StreamArchiver",
        native_options,
        Box::new(move |cc| {
            // Add OS CJK/Unicode fallback fonts so non-Latin channel names (e.g.
            // Japanese VTuber names, fullwidth 【】) render instead of tofu boxes.
            fonts::install_unicode_fonts(&cc.egui_ctx);
            let (tray, ui_rx, ui_tx) = build_tray(cc.egui_ctx.clone())
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            // Toast clicks feed the same command channel as the tray menu.
            toast_activation::set_ui_sink(ui_tx, cc.egui_ctx.clone());
            Ok(Box::new(ui::StreamArchiverApp::new(
                core_for_app,
                tray,
                ui_rx,
                heartbeat,
                cc.egui_ctx.clone(),
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed: {e}"))?;

    // The UI loop has exited (Quit). By default we DETACH: leave the tool process
    // trees running so the app can restart/rebuild without stopping downloads (the
    // next launch re-attaches). Only stop them when the user asked to (the
    // "Quit & stop recordings" tray item or the stop_downloads_on_quit setting).
    core.shutdown_on_exit();

    Ok(())
}

/// Create the system tray icon + menu and a background thread that forwards
/// menu events to the UI (waking the reactive egui loop via `request_repaint`).
/// Also hands back a `Sender` clone so other producers (toast activation) can
/// feed the same command channel.
fn build_tray(ctx: egui::Context) -> Result<(TrayIcon, Receiver<UiCommand>, Sender<UiCommand>)> {
    let menu = Menu::new();
    let open_item = MenuItem::new("Open StreamArchiver", true, None);
    // Default Quit detaches (downloads keep running); the second item force-stops.
    let quit_item = MenuItem::new("Quit (keep recording)", true, None);
    let quit_stop_item = MenuItem::new("Quit & stop recordings", true, None);
    menu.append(&open_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;
    menu.append(&quit_stop_item)?;

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
    let tx_out = tx.clone();
    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    let quit_stop_id = quit_stop_item.id().clone();
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
                } else if event.id == quit_stop_id {
                    Some(UiCommand::QuitAndStop)
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

    Ok((tray, rx, tx_out))
}

/// One-shot detection for diagnostics. Uses scrape/probe (no credentials),
/// except Twitch which falls through to the generic streamlink probe.
fn run_probe(url: String) -> Result<()> {
    if url.is_empty() {
        anyhow::bail!("usage: streamarchiver --probe <url>");
    }
    let store = Arc::new(Store::open(&app_paths::db_path()).context("opening data store")?);
    let rt = tokio::runtime::Runtime::new()?;
    let (events_tx, _) = tokio::sync::broadcast::channel(16);
    let ctx = DetectContext::new(store, events_tx);
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
        automation_enabled: true,
        tool,
        detection_method: method,
        poll_interval_secs: 30,
        quality: "best".into(),
        output_dir,
        filename_template: "{name}_{date}_{time}".into(),
        container: models::Container::Mkv,
        capture_from_start: std::env::var("STREAMARCHIVER_FROM_START").as_deref() != Ok("0"),
        dual_capture: false,
        ad_free: false,
        auth_kind: models::AuthKind::Inherit,
        auth_value: String::new(),
        audio_tracks: "all".into(),
        subtitle_tracks: "all".into(),
        chat_log: true,
        fetch_thumbnail: true,
        thumbnail_in_toast: false,
        fetch_chat_assets: true,
        extra_args: String::new(),
        max_concurrent: 1,
        last_checked_at: None,
        last_state: "idle".into(),
        last_live_since: None,
        last_live_since_approx: false,
        sabr_codec_pref: models::SabrCodecPref::Inherit,
        sabr_codec_custom: String::new(),
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
            color: String::new(),
            preferred_asset: None,
            enabled: true,
            automation_enabled: true,
        },
        monitor: models::Monitor {
            id: 0,
            channel_id: 0,
            url: url.clone(),
            enabled: true,
            automation_enabled: true,
            tool,
            detection_method: models::DetectionMethod::GenericProbe,
            poll_interval_secs: 60,
            quality: "best".into(),
            output_dir: out_dir,
            filename_template: "captest_{time}".into(),
            container: models::Container::Mkv,
            capture_from_start: false,
            dual_capture: false,
            ad_free: false,
            auth_kind: models::AuthKind::Inherit,
            auth_value: String::new(),
            audio_tracks: String::new(),
            subtitle_tracks: String::new(),
            chat_log: false,
            fetch_thumbnail: false,
            thumbnail_in_toast: false,
            fetch_chat_assets: false,
            extra_args: String::new(),
            max_concurrent: 1,
            last_checked_at: None,
            last_state: "idle".into(),
            last_live_since: None,
            last_live_since_approx: false,
            sabr_codec_pref: models::SabrCodecPref::Inherit,
            sabr_codec_custom: String::new(),
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
        last_recording_trigger: String::new(),
        ad_free_sub: None,
        recording_count: 0,
        next_stream_at: None,
        next_stream_title: String::new(),
        last_title: String::new(),
        last_game: String::new(),
        last_thumbnail_url: String::new(),
        last_viewers: -1,
    };
    let plan = downloader::build_plan(
        &row,
        models::now_unix(),
        &downloader::AuthSource::None,
        &[],
        None,
        "",
        None,
        0,
        &downloader::YtDlpBins::default(),
    );
    println!("plan: {} {:?}", plan.program, plan.args);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        if let Some(parent) = plan.capture_path.parent() {
            let _ = crate::iomon::fs::create_dir_all(crate::iomon::Cat::DirSetup, parent).await;
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

        let ts_len = crate::iomon::fs::metadata(crate::iomon::Cat::Other, &plan.capture_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        println!(
            "captured .ts: {} KB (survives the hard kill)",
            ts_len / 1024
        );
        if plan.remux_to_mkv && ts_len > 0 {
            match downloader::remux_ts_to_mkv(&plan.capture_path, &plan.final_path, None, &Default::default()).await {
                Ok(()) => {
                    let mkv = crate::iomon::fs::metadata(crate::iomon::Cat::Other, &plan.final_path)
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
/// Starts it, records for `<secs>`, and Stops it.
fn run_manual_test(id: i64, secs: u64) -> Result<()> {
    use events::ManualCommand;
    let store = Store::open(&app_paths::db_path()).context("opening data store")?;
    store.set_monitor_enabled(id, false)?; // prove the scheduler isn't doing it
    let core = AppCore::new(Arc::new(store)).context("starting core")?;
    core.start();
    info!("manual-test: Start({id})");
    core.manual(ManualCommand::Start { id, user_initiated: true });
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

/// Initialise tracing: stderr at the env-filter level **and** a daily-rotating
/// log file under `data_dir()/logs/streamarchiver.YYYY-MM-DD.log` (kept for 7
/// days via `tracing_appender::rolling::daily`; older files are not auto-pruned
/// by the crate, so we prune on startup).
///
/// Returns the worker-thread guard — drop it only when the process is about to
/// exit, otherwise buffered log lines may be lost.
fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,streamarchiver=debug"));

    // ── rotating file layer ───────────────────────────────────────────────────
    let log_dir = crate::app_paths::logs_dir();
    prune_old_logs(&log_dir, 7);
    // Per-download tool logs (see downloader::capture_log_path) — same retention.
    prune_old_logs(&log_dir.join("captures"), 7);
    // I/O-monitor JSONL sample logs (bigger files, longer post-mortem window).
    prune_old_logs(&log_dir.join("iomon"), iomon::SAMPLE_LOG_KEEP_DAYS);
    let file_appender = tracing_appender::rolling::daily(&log_dir, "streamarchiver.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // Strip ANSI escapes from message *text* too (colored platform tags) —
    // with_ansi(false) only disables the layer's own coloring. Sanitization
    // must be OFF so the real ESC bytes reach our stripping writer instead of
    // being rewritten to literal "\x1b" text first.
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_ansi_sanitization(false)
        .with_target(false)
        .with_writer(logfmt::StripAnsiMake(non_blocking));

    // ── stderr layer ─────────────────────────────────────────────────────────
    // Colored platform tags in message text only when stderr is a real
    // terminal (debug console runs); redirected stderr / release GUI = plain.
    // The tags are the only embedded ANSI (self-generated, never user data),
    // so the layer's injection-protection sanitization — which would print
    // them as literal "\x1b[38;…" — is safely disabled.
    {
        use std::io::IsTerminal;
        logfmt::set_color_enabled(std::io::stderr().is_terminal());
    }
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi_sanitization(false)
        .with_target(false)
        .with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    guard
}

/// Delete log files older than `keep_days` days from `dir`.
fn prune_old_logs(dir: &std::path::Path, keep_days: u64) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(keep_days * 86_400))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    use crate::iomon::Cat;
    let Ok(entries) = crate::iomon::fs::read_dir_sync(Cat::AppLog, dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // Match both plain `*.log` (capture tool logs) and the daily-rolled
        // `streamarchiver.log.YYYY-MM-DD` files, whose Path::extension is the
        // DATE — the old extension-only check never matched them, so rolled
        // app logs were in fact never pruned (found 11 days on disk).
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_log =
            name.ends_with(".log") || name.contains(".log.") || name.ends_with(".jsonl");
        if is_log
            && let Ok(meta) = crate::iomon::fs::metadata_sync(Cat::AppLog, &path)
            && meta.modified().map(|m| m < cutoff).unwrap_or(false)
        {
            let _ = crate::iomon::fs::remove_file_sync(Cat::AppLog, &path);
        }
    }
}
