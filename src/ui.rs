//! The on-demand egui window: channel table, add/edit form, and settings.
//!
//! Runs reactive (repaints only on input/events). The tray thread wakes it via
//! `Context::request_repaint`. Closing the window hides it to the tray; the
//! tray "Quit" item triggers a real close.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use tracing::warn;
use tray_icon::TrayIcon;

use crate::app_core::AppCore;
use crate::events::{ManualCommand, UiCommand};
use crate::models::{
    AuthKind, Channel, Container, DetectionMethod, Monitor, MonitorWithChannel, Platform, Tool,
};
use crate::oauth::{self, AuthFlow};
use crate::platform::AutoStart;

const K_TWITCH_ID: &str = "twitch_client_id";
const K_TWITCH_SECRET: &str = "twitch_client_secret";
const K_YT_KEY: &str = "youtube_api_key";
const K_DEFAULT_OUT: &str = "default_output_dir";
const K_MAX_CONCURRENT: &str = "max_concurrent_downloads";
const K_DOWNLOAD_AUTH: &str = "download_auth_method";
const K_COOKIES_BROWSER: &str = "cookies_browser";

/// Browsers yt-dlp can read cookies from (for the Settings dropdown).
const COOKIE_BROWSERS: [&str; 8] = [
    "firefox", "chrome", "chromium", "edge", "brave", "opera", "vivaldi", "safari",
];

#[derive(PartialEq, Eq)]
enum View {
    Channels,
    Settings,
}

/// Backing state for the add/edit dialog.
struct MonitorForm {
    monitor_id: Option<i64>,
    channel_id: Option<i64>,
    name: String,
    url: String,
    tool: Tool,
    detection_method: DetectionMethod,
    poll_interval_secs: i64,
    quality: String,
    output_dir: String,
    filename_template: String,
    container: Container,
    capture_from_start: bool,
    enabled: bool,
    auth_kind: AuthKind,
    auth_value: String,
    extra_args: String,
}

impl MonitorForm {
    fn new_channel(default_output_dir: String) -> MonitorForm {
        MonitorForm {
            monitor_id: None,
            channel_id: None,
            name: String::new(),
            url: String::new(),
            tool: Tool::Streamlink,
            detection_method: DetectionMethod::GenericProbe,
            poll_interval_secs: 60,
            quality: "best".into(),
            output_dir: default_output_dir,
            filename_template: "{name}_{date}_{time}".into(),
            container: Container::Mkv,
            capture_from_start: true,
            enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            extra_args: String::new(),
        }
    }

    fn from_existing(row: &MonitorWithChannel) -> MonitorForm {
        let m = &row.monitor;
        MonitorForm {
            monitor_id: Some(m.id),
            channel_id: Some(row.channel.id),
            name: row.channel.name.clone(),
            url: row.channel.url.clone(),
            tool: m.tool,
            detection_method: m.detection_method,
            poll_interval_secs: m.poll_interval_secs,
            quality: m.quality.clone(),
            output_dir: m.output_dir.clone(),
            filename_template: m.filename_template.clone(),
            container: m.container,
            capture_from_start: m.capture_from_start,
            enabled: m.enabled,
            auth_kind: m.auth_kind,
            auth_value: m.auth_value.clone(),
            extra_args: m.extra_args.clone(),
        }
    }

    fn add_instance(channel: &Channel, default_output_dir: String) -> MonitorForm {
        let platform = channel.platform;
        MonitorForm {
            monitor_id: None,
            channel_id: Some(channel.id),
            name: channel.name.clone(),
            url: channel.url.clone(),
            tool: platform.default_tool(),
            detection_method: platform.default_detection(),
            poll_interval_secs: 60,
            quality: "best".into(),
            output_dir: default_output_dir,
            filename_template: "{name}_{date}_{time}".into(),
            container: Container::Mkv,
            capture_from_start: true,
            enabled: true,
            auth_kind: AuthKind::Inherit,
            auth_value: String::new(),
            extra_args: String::new(),
        }
    }
}

#[derive(Default)]
struct SettingsForm {
    twitch_client_id: String,
    twitch_client_secret: String,
    youtube_api_key: String,
    default_output_dir: String,
    max_concurrent_downloads: String,
    /// Global download-auth default: "none" or "cookies".
    download_auth_method: String,
    cookies_browser: String,
}

pub struct StreamArchiverApp {
    core: Arc<AppCore>,
    _tray: TrayIcon,
    ui_rx: Receiver<UiCommand>,
    events_rx: crate::events::EventRx,
    autostart: AutoStart,
    autostart_on: bool,
    quitting: bool,

    view: View,
    rows: Vec<MonitorWithChannel>,
    form: Option<MonitorForm>,
    settings: SettingsForm,
    status: String,
    /// Shared state of the interactive "Connect Twitch" device-code flow.
    twitch_flow: Arc<Mutex<AuthFlow>>,
}

impl StreamArchiverApp {
    pub fn new(
        core: Arc<AppCore>,
        tray: TrayIcon,
        ui_rx: Receiver<UiCommand>,
    ) -> StreamArchiverApp {
        let events_rx = core.subscribe();
        let autostart = AutoStart::new();
        let autostart_on = autostart.is_enabled();

        let default_out = core
            .store
            .get_setting(K_DEFAULT_OUT)
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                crate::app_paths::default_output_dir()
                    .to_string_lossy()
                    .to_string()
            });

        let settings = SettingsForm {
            twitch_client_id: setting_or_empty(&core, K_TWITCH_ID),
            twitch_client_secret: setting_or_empty(&core, K_TWITCH_SECRET),
            youtube_api_key: setting_or_empty(&core, K_YT_KEY),
            default_output_dir: default_out,
            max_concurrent_downloads: core
                .store
                .get_setting(K_MAX_CONCURRENT)
                .ok()
                .flatten()
                .unwrap_or_else(|| "3".into()),
            download_auth_method: core
                .store
                .get_setting(K_DOWNLOAD_AUTH)
                .ok()
                .flatten()
                .unwrap_or_else(|| "none".into()),
            cookies_browser: setting_or_empty(&core, K_COOKIES_BROWSER),
        };

        let twitch_flow = Arc::new(Mutex::new(match oauth::connected_login(&core.store) {
            Some(login) => AuthFlow::Connected { login },
            None => AuthFlow::Idle,
        }));

        let mut app = StreamArchiverApp {
            core,
            _tray: tray,
            ui_rx,
            events_rx,
            autostart,
            autostart_on,
            quitting: false,
            view: View::Channels,
            rows: Vec::new(),
            form: None,
            settings,
            status: String::new(),
            twitch_flow,
        };
        app.reload_rows();
        app
    }

    fn reload_rows(&mut self) {
        match self.core.store.list_monitors_with_channels() {
            Ok(rows) => self.rows = rows,
            Err(e) => {
                warn!("failed to load monitors: {e:#}");
                self.status = format!("Error loading channels: {e}");
            }
        }
    }

    /// Handle tray commands and bus events; returns true if a repaint is needed.
    fn pump_messages(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.ui_rx.try_recv() {
            match cmd {
                UiCommand::ShowWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                UiCommand::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        let mut dirty = false;
        loop {
            match self.events_rx.try_recv() {
                Ok(crate::events::AppEvent::Error { context, message }) => {
                    self.status = format!("{context}: {message}");
                    dirty = true;
                }
                Ok(_) => dirty = true,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        if dirty {
            self.reload_rows();
        }
    }

    fn save_form(&mut self) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.name.trim().is_empty() || form.url.trim().is_empty() {
            self.status = "Name and URL are required.".into();
            return;
        }
        let platform = Platform::detect(&form.url);
        let channel_id =
            match form.channel_id {
                Some(id) => id,
                None => match self.core.store.upsert_channel(
                    form.name.trim(),
                    form.url.trim(),
                    platform,
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        self.status = format!("Error saving channel: {e}");
                        return;
                    }
                },
            };

        let monitor = Monitor {
            id: form.monitor_id.unwrap_or(0),
            channel_id,
            enabled: form.enabled,
            tool: form.tool,
            detection_method: form.detection_method,
            poll_interval_secs: form.poll_interval_secs.max(5),
            quality: form.quality.clone(),
            output_dir: form.output_dir.clone(),
            filename_template: form.filename_template.clone(),
            container: form.container,
            capture_from_start: form.capture_from_start,
            auth_kind: form.auth_kind,
            auth_value: form.auth_value.clone(),
            extra_args: form.extra_args.clone(),
            max_concurrent: 1,
            last_checked_at: None,
            last_state: "idle".into(),
        };

        let result = match form.monitor_id {
            Some(_) => self.core.store.update_monitor(&monitor),
            None => self.core.store.insert_monitor(&monitor).map(|_| ()),
        };
        match result {
            Ok(()) => {
                self.status = "Saved.".into();
                self.form = None;
                self.reload_rows();
            }
            Err(e) => self.status = format!("Error saving monitor: {e}"),
        }
    }

    fn save_settings(&mut self) {
        let s = &self.settings;
        let pairs = [
            (K_TWITCH_ID, s.twitch_client_id.trim()),
            (K_TWITCH_SECRET, s.twitch_client_secret.trim()),
            (K_YT_KEY, s.youtube_api_key.trim()),
            (K_DEFAULT_OUT, s.default_output_dir.trim()),
            (K_MAX_CONCURRENT, s.max_concurrent_downloads.trim()),
            (K_DOWNLOAD_AUTH, s.download_auth_method.trim()),
            (K_COOKIES_BROWSER, s.cookies_browser.trim()),
        ];
        for (k, v) in pairs {
            if let Err(e) = self.core.store.set_setting(k, v) {
                self.status = format!("Error saving settings: {e}");
                return;
            }
        }
        self.status = "Settings saved.".into();
    }
}

fn setting_or_empty(core: &AppCore, key: &str) -> String {
    core.store
        .get_setting(key)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Format a unix timestamp as a local `YYYY-MM-DD` date (empty if unset).
fn fmt_date(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

/// Compact local timestamp `MM-DD HH:MM:SS` (drops the year to save table width).
fn fmt_datetime_short(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// Format a duration in seconds as `HH:MM:SS`.
fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Columns derived from a monitor's latest recording.
struct RecordingCells {
    /// When *we* started recording.
    started_on: String,
    /// How long we've recorded (ticks while active; final length otherwise).
    duration: String,
    /// When the stream went live on the platform (`~`-prefixed if approximate).
    went_live: String,
    /// How much of the stream we missed = started_on - went_live.
    lost: String,
}

fn recording_cells(row: &MonitorWithChannel, now: i64) -> RecordingCells {
    let active = row.last_recording_status.as_deref() == Some("recording");
    let started = row.last_recording_started;
    let dur = match (started, row.last_recording_ended, active) {
        (Some(s), _, true) => Some(now - s),
        (Some(s), Some(e), false) => Some(e - s),
        _ => None,
    };
    let went_live = match row.last_recording_went_live {
        Some(w) => {
            let s = fmt_datetime_short(w);
            if row.last_recording_went_live_approx {
                format!("~{s}")
            } else {
                s
            }
        }
        None => String::new(),
    };
    let lost = match (started, row.last_recording_went_live) {
        (Some(s), Some(w)) => fmt_duration((s - w).max(0)),
        _ => String::new(),
    };
    RecordingCells {
        started_on: started.map(fmt_datetime_short).unwrap_or_default(),
        duration: dur.map(fmt_duration).unwrap_or_default(),
        went_live,
        lost,
    }
}

/// Draw a small colored brand badge for the platform.
fn platform_badge(ui: &mut egui::Ui, platform: Platform) {
    use egui::{Color32, RichText};
    let (label, bg, fg) = match platform {
        Platform::Twitch => ("T", Color32::from_rgb(0x91, 0x46, 0xFF), Color32::WHITE),
        Platform::YouTube => ("▶", Color32::from_rgb(0xFF, 0x00, 0x00), Color32::WHITE),
        Platform::Kick => ("K", Color32::from_rgb(0x53, 0xFC, 0x18), Color32::BLACK),
        Platform::Generic => ("●", Color32::from_gray(0x80), Color32::WHITE),
    };
    ui.label(
        RichText::new(format!(" {label} "))
            .monospace()
            .strong()
            .color(fg)
            .background_color(bg),
    );
}

/// Open a native folder picker, seeded at `current` if it exists.
fn browse_folder(current: &str) -> Option<String> {
    let mut dialog = rfd::FileDialog::new();
    if !current.trim().is_empty() && std::path::Path::new(current).exists() {
        dialog = dialog.set_directory(current);
    }
    dialog
        .pick_folder()
        .map(|p| p.to_string_lossy().to_string())
}

/// Open a native file picker (for a cookies.txt), seeded at `current`'s folder.
fn browse_file(current: &str) -> Option<String> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(parent) = std::path::Path::new(current).parent() {
        if parent.is_dir() {
            dialog = dialog.set_directory(parent);
        }
    }
    dialog.pick_file().map(|p| p.to_string_lossy().to_string())
}

impl eframe::App for StreamArchiverApp {
    /// Non-drawing logic. eframe also calls this while the window is hidden when
    /// `request_repaint` was called — which is how the tray's "Open" wakes us.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_messages(ctx);

        // Close button hides to tray unless we're really quitting.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("StreamArchiver");
                    ui.separator();
                    ui.selectable_value(&mut self.view, View::Channels, "Channels");
                    ui.selectable_value(&mut self.view, View::Settings, "Settings");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.view == View::Channels && ui.button("➕ Add channel").clicked() {
                            self.form = Some(MonitorForm::new_channel(
                                self.settings.default_output_dir.clone(),
                            ));
                        }
                    });
                });
            });

        egui::Panel::bottom("status")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(if self.status.is_empty() {
                        "Ready."
                    } else {
                        &self.status
                    });
                });
            });

        egui::CentralPanel::default().show_inside(ui, |ui| match self.view {
            View::Channels => self.channels_view(ui),
            View::Settings => self.settings_view(ui),
        });

        self.form_window(ui.ctx());
    }
}

impl StreamArchiverApp {
    fn channels_view(&mut self, ui: &mut egui::Ui) {
        if self.rows.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No channels yet.");
                ui.label("Click “Add channel” to start monitoring a livestream.");
            });
            return;
        }

        // Deferred actions to avoid borrowing self mutably inside the table closure.
        let mut to_edit: Option<usize> = None;
        let mut to_add_instance: Option<usize> = None;
        let mut to_delete: Option<i64> = None;
        let mut toggle: Option<(i64, bool)> = None;
        let mut to_start: Option<i64> = None;
        let mut to_stop: Option<i64> = None;

        let now = crate::models::now_unix();
        let any_active = self
            .rows
            .iter()
            .any(|r| r.last_recording_status.as_deref() == Some("recording"));

        // Fill the available height so the horizontal scrollbar sits at the
        // bottom of the window rather than directly under the (short) row list.
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::auto().at_least(28.0)) // enabled
                    .column(Column::auto().at_least(100.0)) // name
                    .column(Column::auto().at_least(90.0)) // platform
                    .column(Column::auto().at_least(72.0)) // tool
                    .column(Column::auto().at_least(76.0)) // method
                    .column(Column::auto().at_least(44.0)) // interval
                    .column(Column::auto().at_least(64.0)) // state
                    .column(Column::auto().at_least(112.0)) // went live
                    .column(Column::auto().at_least(104.0)) // started on
                    .column(Column::auto().at_least(58.0)) // lost time
                    .column(Column::auto().at_least(58.0)) // duration
                    .column(Column::auto().at_least(80.0)) // added
                    .column(Column::remainder().at_least(140.0)) // actions
                    .header(20.0, |mut header| {
                        for title in [
                            "On",
                            "Name",
                            "Platform",
                            "Tool",
                            "Detection",
                            "Every",
                            "State",
                            "Went Live",
                            "Started On",
                            "Lost time",
                            "Duration",
                            "Added",
                            "Actions",
                        ] {
                            header.col(|ui| {
                                ui.strong(title);
                            });
                        }
                    })
                    .body(|mut body| {
                        for (i, row) in self.rows.iter().enumerate() {
                            let m = &row.monitor;
                            let rec = recording_cells(row, now);
                            body.row(24.0, |mut tr| {
                                tr.col(|ui| {
                                    let mut on = m.enabled;
                                    if ui.checkbox(&mut on, "").changed() {
                                        toggle = Some((m.id, on));
                                    }
                                });
                                tr.col(|ui| {
                                    ui.label(&row.channel.name).on_hover_text(&row.channel.url);
                                });
                                tr.col(|ui| {
                                    platform_badge(ui, row.channel.platform);
                                    ui.label(row.channel.platform.label());
                                });
                                tr.col(|ui| {
                                    ui.label(m.tool.label());
                                });
                                tr.col(|ui| {
                                    ui.label(m.detection_method.short_label())
                                        .on_hover_text(m.detection_method.label());
                                });
                                tr.col(|ui| {
                                    ui.label(format!("{}s", m.poll_interval_secs));
                                });
                                tr.col(|ui| {
                                    ui.label(&m.last_state);
                                });
                                tr.col(|ui| {
                                    ui.label(&rec.went_live);
                                });
                                tr.col(|ui| {
                                    ui.label(&rec.started_on);
                                });
                                tr.col(|ui| {
                                    ui.label(&rec.lost);
                                });
                                tr.col(|ui| {
                                    ui.label(&rec.duration);
                                });
                                tr.col(|ui| {
                                    ui.label(fmt_date(row.channel.created_at));
                                });
                                tr.col(|ui| {
                                    let recording =
                                        self.core.active.lock().unwrap().contains_key(&m.id);
                                    ui.push_id(m.id, |ui| {
                                        if recording {
                                            if ui
                                                .small_button("⏹")
                                                .on_hover_text("Stop / abort recording")
                                                .clicked()
                                            {
                                                to_stop = Some(m.id);
                                            }
                                        } else if ui
                                            .small_button("▶")
                                            .on_hover_text("Start recording now (checks if live)")
                                            .clicked()
                                        {
                                            to_start = Some(m.id);
                                        }
                                        if ui.small_button("✏").on_hover_text("Edit").clicked() {
                                            to_edit = Some(i);
                                        }
                                        if ui
                                            .small_button("➕")
                                            .on_hover_text("Add another tool instance")
                                            .clicked()
                                        {
                                            to_add_instance = Some(i);
                                        }
                                        if ui
                                            .small_button("🗑")
                                            .on_hover_text("Delete this instance")
                                            .clicked()
                                        {
                                            to_delete = Some(m.id);
                                        }
                                    });
                                });
                            });
                        }
                    });
            });

        // Tick the live Duration column ~1/sec while anything is recording.
        if any_active {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
        }

        if let Some(i) = to_edit {
            self.form = Some(MonitorForm::from_existing(&self.rows[i]));
        }
        if let Some(i) = to_add_instance {
            self.form = Some(MonitorForm::add_instance(
                &self.rows[i].channel,
                self.settings.default_output_dir.clone(),
            ));
        }
        if let Some((id, on)) = toggle {
            if let Err(e) = self.core.store.set_monitor_enabled(id, on) {
                self.status = format!("Error: {e}");
            }
            self.reload_rows();
        }
        if let Some(id) = to_delete {
            if let Err(e) = self.core.store.delete_monitor(id) {
                self.status = format!("Error: {e}");
            }
            self.reload_rows();
        }
        if let Some(id) = to_start {
            self.core.manual(ManualCommand::Start(id));
            self.status = "Checking channel… will record if live.".into();
        }
        if let Some(id) = to_stop {
            self.core.manual(ManualCommand::Stop(id));
            self.status = "Stopping recording…".into();
        }
    }

    /// Kick off the Twitch device-code flow on the async runtime, updating the
    /// shared `twitch_flow` state as it progresses and waking the UI.
    fn start_twitch_connect(&mut self, ctx: egui::Context) {
        let client_id = self.settings.twitch_client_id.trim().to_string();
        if client_id.is_empty() {
            self.status = "Enter and save a Twitch Client ID first.".into();
            return;
        }
        // Persist the Client ID so the flow + later refresh can read it.
        let _ = self.core.store.set_setting(K_TWITCH_ID, &client_id);

        let flow = self.twitch_flow.clone();
        let store = self.core.store.clone();
        *flow.lock().unwrap() = AuthFlow::Pending {
            user_code: String::new(),
            url: String::new(),
        };
        self.core.rt.spawn(async move {
            let http = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed {
                        message: e.to_string(),
                    };
                    ctx.request_repaint();
                    return;
                }
            };
            let dc = match oauth::start_device(&http, &client_id).await {
                Ok(dc) => dc,
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed {
                        message: e.to_string(),
                    };
                    ctx.request_repaint();
                    return;
                }
            };
            *flow.lock().unwrap() = AuthFlow::Pending {
                user_code: dc.user_code.clone(),
                url: dc.verification_uri.clone(),
            };
            ctx.request_repaint();
            match oauth::poll_token(&http, &client_id, &dc).await {
                Ok(tokens) => {
                    let login = oauth::fetch_login(&http, &client_id, &tokens.access)
                        .await
                        .unwrap_or_default();
                    let _ = oauth::store_tokens(&store, &tokens, &login);
                    *flow.lock().unwrap() = AuthFlow::Connected { login };
                }
                Err(e) => {
                    *flow.lock().unwrap() = AuthFlow::Failed {
                        message: e.to_string(),
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Detection credentials (optional)");
            ui.label("Used only by monitors set to an API detection method.");
            egui::Grid::new("creds_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Twitch Client ID");
                    ui.text_edit_singleline(&mut self.settings.twitch_client_id);
                    ui.end_row();
                    ui.label("Twitch Client Secret");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.twitch_client_secret)
                            .password(true),
                    );
                    ui.end_row();
                    ui.label("YouTube API Key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.youtube_api_key)
                            .password(true),
                    );
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.heading("Twitch account (OAuth)");
            ui.label("Connect to use a user token for detection (Client Secret then optional).");
            let flow = self.twitch_flow.lock().unwrap().clone();
            match flow {
                AuthFlow::Connected { login } => {
                    ui.horizontal(|ui| {
                        ui.label(format!("✅ Connected as {login}"));
                        if ui.button("Disconnect").clicked() {
                            let _ = oauth::disconnect(&self.core.store);
                            *self.twitch_flow.lock().unwrap() = AuthFlow::Idle;
                        }
                    });
                }
                AuthFlow::Pending { user_code, url } => {
                    ui.label("Authorize in your browser, then wait:");
                    if url.is_empty() {
                        ui.label("Requesting code…");
                    } else {
                        ui.hyperlink(&url);
                        ui.label(format!("Enter code: {user_code}"));
                    }
                }
                AuthFlow::Failed { message } => {
                    ui.colored_label(egui::Color32::from_rgb(0xE0, 0x6C, 0x6C), &message);
                    if ui.button("🔗 Connect Twitch").clicked() {
                        self.start_twitch_connect(ui.ctx().clone());
                    }
                }
                AuthFlow::Idle => {
                    if ui.button("🔗 Connect Twitch").clicked() {
                        self.start_twitch_connect(ui.ctx().clone());
                    }
                }
            }

            ui.add_space(12.0);
            ui.heading("Defaults");
            egui::Grid::new("defaults_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Default output folder");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.settings.default_output_dir);
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = browse_folder(&self.settings.default_output_dir) {
                                self.settings.default_output_dir = p;
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Max concurrent downloads");
                    ui.text_edit_singleline(&mut self.settings.max_concurrent_downloads);
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.heading("Download authentication");
            ui.label("Default for capturing sub-only / members-only / ad-reduced streams. Per-channel settings override this.");
            egui::Grid::new("auth_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Method");
                    let mut cookies = self.settings.download_auth_method == "cookies";
                    egui::ComboBox::from_id_salt("dl_auth_cb")
                        .selected_text(if cookies { "Browser cookies" } else { "None" })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut cookies, false, "None");
                            ui.selectable_value(&mut cookies, true, "Browser cookies");
                        });
                    self.settings.download_auth_method =
                        if cookies { "cookies".into() } else { "none".into() };
                    ui.end_row();

                    if cookies {
                        ui.label("Browser");
                        egui::ComboBox::from_id_salt("cookies_browser_cb")
                            .selected_text(if self.settings.cookies_browser.is_empty() {
                                "(choose)"
                            } else {
                                &self.settings.cookies_browser
                            })
                            .show_ui(ui, |ui| {
                                for b in COOKIE_BROWSERS {
                                    ui.selectable_value(
                                        &mut self.settings.cookies_browser,
                                        b.to_string(),
                                        b,
                                    );
                                }
                            });
                        ui.end_row();
                    }
                });

            ui.add_space(12.0);
            ui.heading("Startup");
            let mut on = self.autostart_on;
            if ui
                .checkbox(&mut on, "Start StreamArchiver at login")
                .changed()
            {
                match self.autostart.set(on) {
                    Ok(()) => {
                        self.autostart_on = on;
                        self.status = if on {
                            "Autostart enabled.".into()
                        } else {
                            "Autostart disabled.".into()
                        };
                    }
                    Err(e) => self.status = format!("Autostart error: {e}"),
                }
            }

            ui.add_space(16.0);
            if ui.button("💾 Save settings").clicked() {
                self.save_settings();
            }
        });
    }

    fn form_window(&mut self, ctx: &egui::Context) {
        if self.form.is_none() {
            return;
        }
        let mut open = true;
        let mut do_save = false;
        let mut do_cancel = false;

        let title = if self.form.as_ref().unwrap().monitor_id.is_some() {
            "Edit monitor"
        } else {
            "Add monitor"
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                let form = self.form.as_mut().unwrap();
                let editing_channel = form.channel_id.is_none();
                let platform = Platform::detect(&form.url);

                egui::Grid::new("form_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add_enabled(editing_channel, egui::TextEdit::singleline(&mut form.name));
                        ui.end_row();

                        ui.label("URL");
                        ui.add_enabled(
                            editing_channel,
                            egui::TextEdit::singleline(&mut form.url).desired_width(320.0),
                        );
                        ui.end_row();

                        ui.label("Platform");
                        ui.label(platform.label());
                        ui.end_row();

                        ui.label("Tool");
                        egui::ComboBox::from_id_salt("tool_cb")
                            .selected_text(form.tool.label())
                            .show_ui(ui, |ui| {
                                for t in Tool::ALL {
                                    ui.selectable_value(&mut form.tool, t, t.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Detection");
                        let methods = platform.detection_methods();
                        if !methods.contains(&form.detection_method) {
                            form.detection_method = platform.default_detection();
                        }
                        egui::ComboBox::from_id_salt("method_cb")
                            .selected_text(form.detection_method.label())
                            .show_ui(ui, |ui| {
                                for &dm in methods {
                                    ui.selectable_value(&mut form.detection_method, dm, dm.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Poll interval (s)");
                        ui.add(egui::DragValue::new(&mut form.poll_interval_secs).range(5..=86400));
                        ui.end_row();

                        ui.label("Quality");
                        ui.text_edit_singleline(&mut form.quality);
                        ui.end_row();

                        ui.label("Container");
                        egui::ComboBox::from_id_salt("container_cb")
                            .selected_text(form.container.label())
                            .show_ui(ui, |ui| {
                                for c in Container::ALL {
                                    ui.selectable_value(&mut form.container, c, c.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Capture from start");
                        ui.checkbox(&mut form.capture_from_start, "").on_hover_text(
                            "yt-dlp --live-from-start / streamlink --hls-live-restart",
                        );
                        ui.end_row();

                        ui.label("Enabled");
                        ui.checkbox(&mut form.enabled, "")
                            .on_hover_text("Monitor this channel for live streams");
                        ui.end_row();

                        ui.label("Auth");
                        egui::ComboBox::from_id_salt("auth_cb")
                            .selected_text(form.auth_kind.label())
                            .show_ui(ui, |ui| {
                                for k in AuthKind::ALL {
                                    ui.selectable_value(&mut form.auth_kind, k, k.label());
                                }
                            });
                        ui.end_row();

                        // Value field depends on the chosen auth kind.
                        match form.auth_kind {
                            AuthKind::CookiesBrowser => {
                                ui.label("Browser");
                                ui.text_edit_singleline(&mut form.auth_value)
                                    .on_hover_text("e.g. firefox, chrome, edge (blank = global)");
                                ui.end_row();
                            }
                            AuthKind::CookiesFile => {
                                ui.label("Cookies file");
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(&mut form.auth_value);
                                    if ui.button("Browse…").clicked() {
                                        if let Some(p) = browse_file(&form.auth_value) {
                                            form.auth_value = p;
                                        }
                                    }
                                });
                                ui.end_row();
                            }
                            AuthKind::Token => {
                                ui.label("Auth token");
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.auth_value).password(true),
                                )
                                .on_hover_text("Twitch OAuth token (streamlink)");
                                ui.end_row();
                            }
                            AuthKind::Inherit | AuthKind::Disabled => {}
                        }

                        ui.label("Output folder");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut form.output_dir);
                            if ui.button("Browse…").clicked() {
                                if let Some(p) = browse_folder(&form.output_dir) {
                                    form.output_dir = p;
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Filename template");
                        ui.text_edit_singleline(&mut form.filename_template);
                        ui.end_row();

                        ui.label("Extra args");
                        ui.text_edit_singleline(&mut form.extra_args);
                        ui.end_row();
                    });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        do_save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        do_cancel = true;
                    }
                });
            });

        if do_save {
            self.save_form();
        } else if do_cancel || !open {
            self.form = None;
        }
    }
}
