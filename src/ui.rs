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
    AuthKind, Channel, Container, DetectionMethod, DownloadDefaults, Monitor, MonitorWithChannel,
    Platform, Tool, Video,
};
use crate::oauth::{self, AuthFlow};
use crate::platform::AutoStart;

const K_TWITCH_ID: &str = "twitch_client_id";
const K_TWITCH_SECRET: &str = "twitch_client_secret";
const K_YT_KEY: &str = "youtube_api_key";
const K_KICK_ID: &str = "kick_client_id";
const K_KICK_SECRET: &str = "kick_client_secret";
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
    Streams,
    Videos,
    Settings,
}

/// A self-mutating action picked from a row's right-click context menu. (URL /
/// folder / clipboard actions are handled inline in the menu and aren't here.)
enum MenuChoice {
    Start,
    Stop,
    Edit,
    AddInstance,
    Toggle,
    Delete,
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

/// Backing state for the always-visible "download a video" form on the Videos tab.
///
/// Fields are pre-filled from the detected platform's saved defaults whenever the
/// platform changes; the user can override any of them per download.
struct VideoForm {
    url: String,
    title: String,
    tool: Tool,
    quality: String,
    output_dir: String,
    filename_template: String,
    /// `None` = "Default (per-platform)": use the snapshotted platform-default
    /// auth below. `Some(kind)` overrides it with `auth_value` for this download.
    auth_override: Option<AuthKind>,
    auth_value: String,
    /// The platform default's auth, snapshotted at pre-fill (used when
    /// `auth_override` is `None`) so every field resolves from one snapshot.
    default_auth_kind: AuthKind,
    default_auth_value: String,
    extra_args: String,
    /// Platform the form is currently filled for; a change triggers a re-fill.
    last_platform: Option<Platform>,
}

impl VideoForm {
    fn new() -> VideoForm {
        VideoForm {
            url: String::new(),
            title: String::new(),
            tool: Tool::YtDlp,
            quality: "best".into(),
            output_dir: String::new(),
            filename_template: "{name}_{date}_{time}".into(),
            auth_override: None,
            auth_value: String::new(),
            default_auth_kind: AuthKind::Inherit,
            default_auth_value: String::new(),
            extra_args: String::new(),
            last_platform: None,
        }
    }
}

#[derive(Default)]
struct SettingsForm {
    twitch_client_id: String,
    twitch_client_secret: String,
    youtube_api_key: String,
    kick_client_id: String,
    kick_client_secret: String,
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
    videos: Vec<Video>,
    form: Option<MonitorForm>,
    video_form: VideoForm,
    /// Per-platform download defaults editable on the Videos tab (persisted JSON).
    download_defaults: DownloadDefaults,
    settings: SettingsForm,
    status: String,
    /// Monitor id of the currently selected row (target for keyboard shortcuts).
    selected_monitor: Option<i64>,
    /// Pending delete confirmation: (monitor id, channel name).
    confirm_delete: Option<(i64, String)>,
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
            kick_client_id: setting_or_empty(&core, K_KICK_ID),
            kick_client_secret: setting_or_empty(&core, K_KICK_SECRET),
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

        let download_defaults = core
            .store
            .get_setting("download_defaults")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<DownloadDefaults>(&s).ok())
            .unwrap_or_else(|| DownloadDefaults::seeded(&settings.default_output_dir));

        let mut app = StreamArchiverApp {
            core,
            _tray: tray,
            ui_rx,
            events_rx,
            autostart,
            autostart_on,
            quitting: false,
            view: View::Streams,
            rows: Vec::new(),
            videos: Vec::new(),
            form: None,
            video_form: VideoForm::new(),
            download_defaults,
            settings,
            status: String::new(),
            selected_monitor: None,
            confirm_delete: None,
            twitch_flow,
        };
        app.reload_rows();
        app.reload_videos();
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

    fn reload_videos(&mut self) {
        match self.core.store.list_videos() {
            Ok(v) => self.videos = v,
            Err(e) => warn!("failed to load videos: {e:#}"),
        }
    }

    fn persist_download_defaults(&self) {
        match serde_json::to_string(&self.download_defaults) {
            Ok(json) => {
                let _ = self.core.store.set_setting("download_defaults", &json);
            }
            Err(e) => warn!("failed to serialize download defaults: {e:#}"),
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
            self.reload_videos();
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
            (K_KICK_ID, s.kick_client_id.trim()),
            (K_KICK_SECRET, s.kick_client_secret.trim()),
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

/// Human-readable byte size (B / KB / MB / GB).
fn fmt_bytes(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}

/// Theme color for a video download status string.
fn video_status_color(status: &str) -> egui::Color32 {
    use egui::Color32;
    match status {
        "downloading" => Color32::from_rgb(0x4d, 0x9b, 0xff),
        "completed" => Color32::from_rgb(0x57, 0xc7, 0x57),
        "failed" => Color32::from_rgb(0xe0, 0x6c, 0x6c),
        _ => Color32::from_gray(0xa0), // queued / stopped / orphaned
    }
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
        self.handle_shortcuts(ui.ctx());

        egui::Panel::top("top")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("StreamArchiver");
                    ui.separator();
                    ui.selectable_value(&mut self.view, View::Streams, "Streams");
                    ui.selectable_value(&mut self.view, View::Videos, "Videos");
                    ui.selectable_value(&mut self.view, View::Settings, "Settings");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.view == View::Streams && ui.button("➕ Add stream").clicked() {
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
            View::Streams => self.channels_view(ui),
            View::Videos => self.videos_view(ui),
            View::Settings => self.settings_view(ui),
        });

        self.form_window(ui.ctx());
        self.confirm_delete_window(ui.ctx());
    }
}

impl StreamArchiverApp {
    /// Process global keyboard shortcuts once per frame, before drawing.
    ///
    /// While a modal (add/edit form or delete confirmation) is open, only `Esc`
    /// is handled — it dismisses the modal — and other shortcuts are suppressed.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        const ADD: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
        const SETTINGS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma);
        const REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F5);

        // A modal is open: Esc closes it, everything else is swallowed.
        if self.form.is_some() || self.confirm_delete.is_some() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.form = None;
                self.confirm_delete = None;
            }
            return;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&ADD)) {
            self.view = View::Streams;
            self.form = Some(MonitorForm::new_channel(
                self.settings.default_output_dir.clone(),
            ));
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SETTINGS)) {
            self.view = View::Settings;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&REFRESH)) {
            self.reload_rows();
            self.status = "Refreshed.".into();
        }

        // Row-targeted keys only fire on the channel list when not typing.
        if self.view == View::Streams && !ctx.egui_wants_keyboard_input() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete)) {
                if let Some(id) = self.selected_monitor {
                    if let Some(row) = self.rows.iter().find(|r| r.monitor.id == id) {
                        self.confirm_delete = Some((id, row.channel.name.clone()));
                    }
                }
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                if let Some(id) = self.selected_monitor {
                    if let Some(idx) = self.rows.iter().position(|r| r.monitor.id == id) {
                        self.form = Some(MonitorForm::from_existing(&self.rows[idx]));
                    }
                }
            }
        }
    }

    /// Modal confirmation for deleting a monitor (the only destructive action).
    fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some((id, name)) = self.confirm_delete.clone() else {
            return;
        };
        let mut open = true;
        let mut do_delete = false;
        let mut do_cancel = false;

        egui::Window::new("Delete monitor")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Delete this capture instance for “{name}”?"));
                ui.label("Removes the monitor and its settings. Recorded files are kept.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        do_delete = true;
                    }
                    if ui.button("Cancel").clicked() {
                        do_cancel = true;
                    }
                });
            });

        if do_delete {
            match self.core.store.delete_monitor(id) {
                Ok(()) => self.status = "Deleted.".into(),
                Err(e) => self.status = format!("Error: {e}"),
            }
            if self.selected_monitor == Some(id) {
                self.selected_monitor = None;
            }
            self.confirm_delete = None;
            self.reload_rows();
        } else if do_cancel || !open {
            self.confirm_delete = None;
        }
    }

    /// The "Videos" tab: a list of on-demand downloads with an always-visible
    /// "paste a URL" form pinned to the bottom.
    fn videos_view(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("video_add_panel")
            .resizable(true)
            .default_size(300.0)
            .show_inside(ui, |ui| {
                // Per-platform defaults on the right; download form on the left.
                egui::Panel::right("video_defaults_panel")
                    .resizable(true)
                    .default_size(360.0)
                    .show_inside(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("video_defaults_scroll")
                            .show(ui, |ui| {
                                self.video_defaults_editor(ui);
                            });
                    });
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("video_form_scroll")
                        .show(ui, |ui| {
                            self.video_add_form(ui);
                        });
                });
            });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.videos_list(ui);
        });
    }

    /// Collapsible per-platform download defaults editor (Twitch / YouTube /
    /// Kick / Generic). Edits persist immediately; the download form below
    /// pre-fills from these per detected platform.
    fn video_defaults_editor(&mut self, ui: &mut egui::Ui) {
        let mut dirty = false;
        // Borrow the defaults (not `self`) so the nested egui closures don't
        // alias `self`; persist afterwards.
        let defs = &mut self.download_defaults;

        ui.add_space(6.0);
        ui.strong("⚙  Per-platform defaults");
        ui.label(
            egui::RichText::new(
                "Downloads pre-fill from these by platform; override per download.",
            )
            .small()
            .color(egui::Color32::from_gray(0x90)),
        );
        ui.add_space(4.0);

        for platform in Platform::ALL {
            egui::CollapsingHeader::new(platform.label())
                .id_salt(("dl_def", platform.as_str()))
                .show(ui, |ui| {
                    let d = defs.get_mut(platform);
                    egui::Grid::new(("dl_def_grid", platform.as_str()))
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Tool").on_hover_text(d.tool.tooltip());
                            egui::ComboBox::from_id_salt(("dl_tool", platform.as_str()))
                                .selected_text(d.tool.label())
                                .show_ui(ui, |ui| {
                                    for t in Tool::ALL {
                                        if ui
                                            .selectable_value(&mut d.tool, t, t.label())
                                            .on_hover_text(t.tooltip())
                                            .changed()
                                        {
                                            dirty = true;
                                        }
                                    }
                                });
                            ui.end_row();

                            ui.label("Quality");
                            if ui.text_edit_singleline(&mut d.quality).changed() {
                                dirty = true;
                            }
                            ui.end_row();

                            ui.label("Auth");
                            egui::ComboBox::from_id_salt(("dl_auth", platform.as_str()))
                                .selected_text(d.auth_kind.label())
                                .show_ui(ui, |ui| {
                                    for k in AuthKind::ALL {
                                        if ui
                                            .selectable_value(&mut d.auth_kind, k, k.label())
                                            .changed()
                                        {
                                            dirty = true;
                                        }
                                    }
                                });
                            ui.end_row();

                            match d.auth_kind {
                                AuthKind::CookiesBrowser => {
                                    ui.label("Browser");
                                    if ui
                                        .text_edit_singleline(&mut d.auth_value)
                                        .on_hover_text("e.g. firefox, chrome, edge")
                                        .changed()
                                    {
                                        dirty = true;
                                    }
                                    ui.end_row();
                                }
                                AuthKind::CookiesFile => {
                                    ui.label("Cookies file");
                                    ui.horizontal(|ui| {
                                        if ui.text_edit_singleline(&mut d.auth_value).changed() {
                                            dirty = true;
                                        }
                                        if ui.button("Browse…").clicked() {
                                            if let Some(p) = browse_file(&d.auth_value) {
                                                d.auth_value = p;
                                                dirty = true;
                                            }
                                        }
                                    });
                                    ui.end_row();
                                }
                                AuthKind::Token => {
                                    ui.label("Auth token");
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(&mut d.auth_value)
                                                .password(true),
                                        )
                                        .changed()
                                    {
                                        dirty = true;
                                    }
                                    ui.end_row();
                                }
                                AuthKind::Inherit | AuthKind::Disabled => {}
                            }

                            ui.label("Output folder");
                            ui.horizontal(|ui| {
                                if ui.text_edit_singleline(&mut d.output_dir).changed() {
                                    dirty = true;
                                }
                                if ui.button("Browse…").clicked() {
                                    if let Some(p) = browse_folder(&d.output_dir) {
                                        d.output_dir = p;
                                        dirty = true;
                                    }
                                }
                            });
                            ui.end_row();

                            ui.label("Filename template");
                            if ui.text_edit_singleline(&mut d.filename_template).changed() {
                                dirty = true;
                            }
                            ui.end_row();

                            ui.label("Extra args");
                            if ui.text_edit_singleline(&mut d.extra_args).changed() {
                                dirty = true;
                            }
                            ui.end_row();
                        });
                });
        }
        if dirty {
            self.persist_download_defaults();
        }
    }

    fn videos_list(&mut self, ui: &mut egui::Ui) {
        // Reflect background progress whenever the list is shown.
        self.reload_videos();

        if self.videos.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No videos yet.");
                ui.label("Paste a URL in the box below to download a video or VOD.");
            });
            return;
        }

        let mut to_stop: Option<i64> = None;
        let mut to_retry: Option<i64> = None;
        let mut to_delete: Option<i64> = None;
        let any_active = self.videos.iter().any(|v| v.is_active());

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::auto().at_least(200.0)) // video
                    .column(Column::auto().at_least(86.0)) // platform
                    .column(Column::auto().at_least(72.0)) // tool
                    .column(Column::auto().at_least(90.0)) // status
                    .column(Column::auto().at_least(72.0)) // size
                    .column(Column::auto().at_least(80.0)) // added
                    .column(Column::remainder().at_least(150.0)) // actions
                    .header(20.0, |mut header| {
                        for title in [
                            "Video", "Platform", "Tool", "Status", "Size", "Added", "Actions",
                        ] {
                            header.col(|ui| {
                                ui.strong(title);
                            });
                        }
                    })
                    .body(|mut body| {
                        for v in &self.videos {
                            body.row(24.0, |mut tr| {
                                tr.col(|ui| {
                                    let label = if v.title.trim().is_empty() {
                                        v.url.as_str()
                                    } else {
                                        v.title.as_str()
                                    };
                                    ui.label(label).on_hover_text(&v.url);
                                });
                                tr.col(|ui| {
                                    platform_badge(ui, v.platform);
                                    ui.label(v.platform.label());
                                });
                                tr.col(|ui| {
                                    ui.label(v.tool.label());
                                });
                                tr.col(|ui| {
                                    ui.colored_label(video_status_color(&v.status), &v.status);
                                });
                                tr.col(|ui| {
                                    if v.bytes > 0 {
                                        ui.label(fmt_bytes(v.bytes));
                                    }
                                });
                                tr.col(|ui| {
                                    ui.label(fmt_date(v.created_at));
                                });
                                tr.col(|ui| {
                                    ui.push_id(v.id, |ui| {
                                        if v.is_active() {
                                            if ui
                                                .small_button("⏹")
                                                .on_hover_text("Stop download")
                                                .clicked()
                                            {
                                                to_stop = Some(v.id);
                                            }
                                        } else if ui
                                            .small_button("↻")
                                            .on_hover_text("Retry download")
                                            .clicked()
                                        {
                                            to_retry = Some(v.id);
                                        }
                                        let dir_ok = std::path::Path::new(&v.output_dir).is_dir();
                                        if ui
                                            .add_enabled(dir_ok, egui::Button::new("📂").small())
                                            .on_hover_text("Open output folder")
                                            .clicked()
                                        {
                                            crate::platform::open_path(std::path::Path::new(
                                                &v.output_dir,
                                            ));
                                        }
                                        let file_ok = !v.output_path.is_empty()
                                            && std::path::Path::new(&v.output_path).is_file();
                                        if ui
                                            .add_enabled(file_ok, egui::Button::new("▶").small())
                                            .on_hover_text("Open file")
                                            .clicked()
                                        {
                                            crate::platform::open_path(std::path::Path::new(
                                                &v.output_path,
                                            ));
                                        }
                                        if ui.small_button("📋").on_hover_text("Copy URL").clicked()
                                        {
                                            ui.ctx().copy_text(v.url.clone());
                                        }
                                        if ui
                                            .small_button("🗑")
                                            .on_hover_text("Delete from list (keeps the file)")
                                            .clicked()
                                        {
                                            to_delete = Some(v.id);
                                        }
                                    });
                                });
                            });
                        }
                    });
            });

        // Tick while a download is queued/running so status + size update live.
        if any_active {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
        }

        if let Some(id) = to_stop {
            self.core.manual(ManualCommand::StopVideo(id));
            self.status = "Stopping download…".into();
        }
        if let Some(id) = to_retry {
            match self.core.store.reset_video_for_retry(id) {
                Ok(()) => {
                    self.core.manual(ManualCommand::StartVideo(id));
                    self.status = "Re-queued download.".into();
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
            self.reload_videos();
        }
        if let Some(id) = to_delete {
            if let Err(e) = self.core.store.delete_video(id) {
                self.status = format!("Error: {e}");
            }
            self.reload_videos();
        }
    }

    /// The bottom "paste a URL + settings + Download" form on the Videos tab.
    fn video_add_form(&mut self, ui: &mut egui::Ui) {
        let platform = Platform::detect(&self.video_form.url);

        // Re-fill the form from this platform's saved defaults whenever the
        // detected platform changes; the user can then override any field.
        if self.video_form.last_platform != Some(platform) {
            let d = self.download_defaults.get(platform).clone();
            let vf = &mut self.video_form;
            vf.tool = d.tool;
            vf.quality = d.quality;
            vf.output_dir = d.output_dir;
            vf.filename_template = d.filename_template;
            vf.extra_args = d.extra_args;
            vf.auth_override = None; // "Default (per-platform)"
            vf.auth_value = String::new();
            // Snapshot the default auth too, so a later edit to the default
            // doesn't desync from the other (already snapshotted) fields.
            vf.default_auth_kind = d.auth_kind;
            vf.default_auth_value = d.auth_value;
            vf.last_platform = Some(platform);
        }

        let mut do_download = false;

        {
            let vf = &mut self.video_form;

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Download a video / VOD");
                ui.label(
                    egui::RichText::new("→ MKV")
                        .small()
                        .color(egui::Color32::from_gray(0x90)),
                );
            });

            egui::Grid::new("video_form_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("URL");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut vf.url)
                                .desired_width(360.0)
                                .hint_text(
                                    "YouTube video, Twitch VOD, or any streamlink/yt-dlp URL",
                                ),
                        );
                        platform_badge(ui, platform);
                        ui.label(platform.label());
                    });
                    ui.end_row();

                    ui.label("Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut vf.title)
                            .hint_text("optional — used for the filename (default: video)"),
                    );
                    ui.end_row();

                    ui.label("Tool").on_hover_text(vf.tool.tooltip());
                    egui::ComboBox::from_id_salt("video_tool_cb")
                        .selected_text(vf.tool.label())
                        .show_ui(ui, |ui| {
                            for t in Tool::ALL {
                                ui.selectable_value(&mut vf.tool, t, t.label())
                                    .on_hover_text(t.tooltip());
                            }
                        });
                    ui.end_row();

                    ui.label("Quality");
                    ui.add(
                        egui::TextEdit::singleline(&mut vf.quality)
                            .hint_text("best, 1080p, or a yt-dlp -f selector"),
                    );
                    ui.end_row();

                    ui.label("Auth");
                    let auth_text = match vf.auth_override {
                        None => "Default (per-platform)".to_string(),
                        Some(k) => k.label().to_string(),
                    };
                    egui::ComboBox::from_id_salt("video_auth_cb")
                        .selected_text(auth_text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut vf.auth_override,
                                None,
                                "Default (per-platform)",
                            );
                            for k in AuthKind::ALL {
                                ui.selectable_value(&mut vf.auth_override, Some(k), k.label());
                            }
                        });
                    ui.end_row();

                    match vf.auth_override {
                        Some(AuthKind::CookiesBrowser) => {
                            ui.label("Browser");
                            ui.text_edit_singleline(&mut vf.auth_value)
                                .on_hover_text("e.g. firefox, chrome, edge");
                            ui.end_row();
                        }
                        Some(AuthKind::CookiesFile) => {
                            ui.label("Cookies file");
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(&mut vf.auth_value);
                                if ui.button("Browse…").clicked() {
                                    if let Some(p) = browse_file(&vf.auth_value) {
                                        vf.auth_value = p;
                                    }
                                }
                            });
                            ui.end_row();
                        }
                        Some(AuthKind::Token) => {
                            ui.label("Auth token");
                            ui.add(egui::TextEdit::singleline(&mut vf.auth_value).password(true))
                                .on_hover_text("Twitch OAuth token (streamlink)");
                            ui.end_row();
                        }
                        // Default (None), Global (Inherit), and None (Disabled) need no value.
                        _ => {}
                    }

                    ui.label("Output folder");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut vf.output_dir);
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = browse_folder(&vf.output_dir) {
                                vf.output_dir = p;
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Filename template");
                    ui.text_edit_singleline(&mut vf.filename_template);
                    ui.end_row();

                    ui.label("Extra args");
                    ui.text_edit_singleline(&mut vf.extra_args);
                    ui.end_row();
                });

            ui.add_space(6.0);
            let can_download = !vf.url.trim().is_empty();
            if ui
                .add_enabled(can_download, egui::Button::new("⬇  Download"))
                .clicked()
            {
                do_download = true;
            }
            ui.add_space(6.0);
        }

        if do_download {
            self.start_video_download();
        }
    }

    /// Insert the form's video as a queued download and kick off the supervisor.
    fn start_video_download(&mut self) {
        let url = self.video_form.url.trim().to_string();
        if url.is_empty() {
            return;
        }
        let platform = Platform::detect(&url);
        // "Default" auth uses the snapshotted platform default; an explicit
        // choice overrides it. (All fields resolve from the same snapshot.)
        let (auth_kind, auth_value) = match self.video_form.auth_override {
            Some(kind) => (kind, self.video_form.auth_value.clone()),
            None => (
                self.video_form.default_auth_kind,
                self.video_form.default_auth_value.clone(),
            ),
        };
        let video = Video {
            id: 0,
            platform,
            url,
            title: self.video_form.title.trim().to_string(),
            tool: self.video_form.tool,
            quality: self.video_form.quality.clone(),
            output_dir: self.video_form.output_dir.clone(),
            filename_template: self.video_form.filename_template.clone(),
            auth_kind,
            auth_value,
            extra_args: self.video_form.extra_args.clone(),
            status: "queued".into(),
            output_path: String::new(),
            bytes: 0,
            exit_code: None,
            created_at: 0,
            started_at: None,
            ended_at: None,
        };
        match self.core.store.insert_video(&video) {
            Ok(id) => {
                self.core.manual(ManualCommand::StartVideo(id));
                self.status = "Queued video download.".into();
                // Clear the URL/name; re-fill defaults for the next download.
                self.video_form.url.clear();
                self.video_form.title.clear();
                self.video_form.last_platform = None;
                self.reload_videos();
            }
            Err(e) => self.status = format!("Error: {e}"),
        }
    }

    fn channels_view(&mut self, ui: &mut egui::Ui) {
        if self.rows.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label("No streams yet.");
                ui.label("Click “Add stream” to start monitoring a channel for live broadcasts.");
            });
            return;
        }

        // Deferred actions to avoid borrowing self mutably inside the table closure.
        let mut to_edit: Option<usize> = None;
        let mut to_add_instance: Option<usize> = None;
        let mut to_delete: Option<(i64, String)> = None;
        let mut toggle: Option<(i64, bool)> = None;
        let mut to_start: Option<i64> = None;
        let mut to_stop: Option<i64> = None;
        let mut to_select: Option<i64> = None;

        let selected_monitor = self.selected_monitor;
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
                // Labels are selectable by default, which makes them sense clicks
                // (for text selection) and swallow right-clicks over their text —
                // breaking the row context menu. Turn it off for the table so the
                // row's click sense wins (the menu offers "Copy URL" instead).
                ui.style_mut().interaction.selectable_labels = false;
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    // Make rows sense clicks so they can be selected and carry a
                    // right-click context menu.
                    .sense(egui::Sense::click())
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
                            let recording = self.core.active.lock().unwrap().contains_key(&m.id);
                            let is_selected = selected_monitor == Some(m.id);
                            body.row(24.0, |mut tr| {
                                // Tint active (recording) rows and the user-selected
                                // row with the theme accent (blue).
                                tr.set_selected(recording || is_selected);

                                // Reusable context-menu body (a `Fn`), attached below
                                // to both the row and each inline action button so a
                                // right-click anywhere on the row opens it. Self-mutating
                                // picks go through `menu_choice`; URL/folder/clipboard
                                // actions are handled inline (they only need `ctx`).
                                let mut menu_choice: Option<MenuChoice> = None;
                                let add_menu =
                                    |ui: &mut egui::Ui, choice: &mut Option<MenuChoice>| {
                                        ui.set_min_width(180.0);
                                        if recording {
                                            if ui.button("⏹  Stop recording").clicked() {
                                                *choice = Some(MenuChoice::Stop);
                                                ui.close();
                                            }
                                        } else if ui.button("▶  Start recording").clicked() {
                                            *choice = Some(MenuChoice::Start);
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("🔗  Open channel URL").clicked() {
                                            ui.ctx().open_url(egui::OpenUrl::new_tab(
                                                row.channel.url.clone(),
                                            ));
                                            ui.close();
                                        }
                                        let folder_exists =
                                            std::path::Path::new(&m.output_dir).is_dir();
                                        if ui
                                            .add_enabled(
                                                folder_exists,
                                                egui::Button::new("📂  Open output folder"),
                                            )
                                            .clicked()
                                        {
                                            crate::platform::open_path(std::path::Path::new(
                                                &m.output_dir,
                                            ));
                                            ui.close();
                                        }
                                        if ui.button("📋  Copy URL").clicked() {
                                            ui.ctx().copy_text(row.channel.url.clone());
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("✏  Edit…").clicked() {
                                            *choice = Some(MenuChoice::Edit);
                                            ui.close();
                                        }
                                        if ui.button("➕  Add tool instance").clicked() {
                                            *choice = Some(MenuChoice::AddInstance);
                                            ui.close();
                                        }
                                        let toggle_label = if m.enabled {
                                            "⏸  Disable"
                                        } else {
                                            "✔  Enable"
                                        };
                                        if ui.button(toggle_label).clicked() {
                                            *choice = Some(MenuChoice::Toggle);
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("🗑  Delete").clicked() {
                                            *choice = Some(MenuChoice::Delete);
                                            ui.close();
                                        }
                                    };

                                tr.col(|ui| {
                                    let mut on = m.enabled;
                                    let cb = ui.checkbox(&mut on, "");
                                    if cb.changed() {
                                        toggle = Some((m.id, on));
                                    }
                                    // The checkbox senses clicks too, so give it the menu.
                                    cb.context_menu(|ui| add_menu(ui, &mut menu_choice));
                                });
                                tr.col(|ui| {
                                    ui.label(&row.channel.name).on_hover_text(&row.channel.url);
                                });
                                tr.col(|ui| {
                                    platform_badge(ui, row.channel.platform);
                                    ui.label(row.channel.platform.label());
                                });
                                tr.col(|ui| {
                                    ui.label(m.tool.label()).on_hover_text(m.tool.tooltip());
                                });
                                tr.col(|ui| {
                                    ui.label(m.detection_method.short_label()).on_hover_text(
                                        format!(
                                            "{}\n\n{}",
                                            m.detection_method.label(),
                                            m.detection_method.tooltip()
                                        ),
                                    );
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
                                    ui.push_id(m.id, |ui| {
                                        let mut btns: Vec<egui::Response> = Vec::with_capacity(4);
                                        if recording {
                                            let b = ui
                                                .small_button("⏹")
                                                .on_hover_text("Stop / abort recording");
                                            if b.clicked() {
                                                to_stop = Some(m.id);
                                            }
                                            btns.push(b);
                                        } else {
                                            let b = ui.small_button("▶").on_hover_text(
                                                "Start recording now (checks if live)",
                                            );
                                            if b.clicked() {
                                                to_start = Some(m.id);
                                            }
                                            btns.push(b);
                                        }
                                        let b = ui.small_button("✏").on_hover_text("Edit");
                                        if b.clicked() {
                                            to_edit = Some(i);
                                        }
                                        btns.push(b);
                                        let b = ui
                                            .small_button("➕")
                                            .on_hover_text("Add another tool instance");
                                        if b.clicked() {
                                            to_add_instance = Some(i);
                                        }
                                        btns.push(b);
                                        let b = ui
                                            .small_button("🗑")
                                            .on_hover_text("Delete this instance");
                                        if b.clicked() {
                                            to_delete = Some((m.id, row.channel.name.clone()));
                                        }
                                        btns.push(b);
                                        // Buttons sense clicks and would otherwise swallow
                                        // the right-click, so give each the row menu too.
                                        for b in &btns {
                                            b.context_menu(|ui| add_menu(ui, &mut menu_choice));
                                        }
                                    });
                                });

                                // Left-click selects the row; right-click anywhere on it
                                // (cells or buttons) opens the context menu, which also
                                // selects it. `response()` is the union of all cells.
                                let row_resp = tr.response();
                                if row_resp.clicked() || row_resp.secondary_clicked() {
                                    to_select = Some(m.id);
                                }
                                row_resp.context_menu(|ui| add_menu(ui, &mut menu_choice));

                                match menu_choice {
                                    Some(MenuChoice::Start) => to_start = Some(m.id),
                                    Some(MenuChoice::Stop) => to_stop = Some(m.id),
                                    Some(MenuChoice::Edit) => to_edit = Some(i),
                                    Some(MenuChoice::AddInstance) => to_add_instance = Some(i),
                                    Some(MenuChoice::Toggle) => toggle = Some((m.id, !m.enabled)),
                                    Some(MenuChoice::Delete) => {
                                        to_delete = Some((m.id, row.channel.name.clone()))
                                    }
                                    None => {}
                                }
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
        if let Some(id) = to_select {
            self.selected_monitor = Some(id);
        }
        if let Some((id, on)) = toggle {
            if let Err(e) = self.core.store.set_monitor_enabled(id, on) {
                self.status = format!("Error: {e}");
            }
            self.reload_rows();
        }
        if let Some((id, name)) = to_delete {
            self.confirm_delete = Some((id, name));
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
                    ui.label("Kick Client ID");
                    ui.text_edit_singleline(&mut self.settings.kick_client_id);
                    ui.end_row();
                    ui.label("Kick Client Secret");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.kick_client_secret)
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

                        ui.label("Tool").on_hover_text(form.tool.tooltip());
                        egui::ComboBox::from_id_salt("tool_cb")
                            .selected_text(form.tool.label())
                            .show_ui(ui, |ui| {
                                for t in Tool::ALL {
                                    ui.selectable_value(&mut form.tool, t, t.label())
                                        .on_hover_text(t.tooltip());
                                }
                            });
                        ui.end_row();

                        ui.label("Detection")
                            .on_hover_text(form.detection_method.tooltip());
                        let methods = platform.detection_methods();
                        if !methods.contains(&form.detection_method) {
                            form.detection_method = platform.default_detection();
                        }
                        egui::ComboBox::from_id_salt("method_cb")
                            .selected_text(form.detection_method.label())
                            .show_ui(ui, |ui| {
                                for &dm in methods {
                                    ui.selectable_value(&mut form.detection_method, dm, dm.label())
                                        .on_hover_text(dm.tooltip());
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
