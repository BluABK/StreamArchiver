//! App plumbing: construction, data reloads, the message pump, form and
//! settings persistence, global shortcuts.

use super::*;

impl StreamArchiverApp {
    pub fn new(
        core: Arc<AppCore>,
        tray: TrayIcon,
        ui_rx: Receiver<UiCommand>,
        heartbeat: crate::watchdog::Heartbeat,
        egui_ctx: egui::Context,
    ) -> StreamArchiverApp {
        let events_rx = core.subscribe();
        let autostart = AutoStart::new();
        let autostart_on = autostart.is_enabled();
        // Detach-on-quit is the default; only `=="1"` opts into stopping downloads.
        let keep_downloads_on_quit = core
            .store
            .get_setting("stop_downloads_on_quit")
            .ok()
            .flatten()
            .as_deref()
            != Some("1");
        // Desktop notifications default on; only `=="0"` disables them.
        let notifications_enabled = core
            .store
            .get_setting(crate::notifications::K_NOTIFICATIONS)
            .ok()
            .flatten()
            .as_deref()
            != Some("0");
        // Collab (shared-chat) EventSub pushes default on; only "0" disables.
        let collab_eventsub = core
            .store
            .get_setting("collab_eventsub")
            .ok()
            .flatten()
            .as_deref()
            != Some("0");
        // Raid EventSub pushes (Channel Stats events) default on likewise.
        let raid_eventsub = core
            .store
            .get_setting("raid_eventsub")
            .ok()
            .flatten()
            .as_deref()
            != Some("0");
        // Channel Stats auto refresh defaults on likewise.
        let chstats_auto = core
            .store
            .get_setting("chstats_auto_refresh")
            .ok()
            .flatten()
            .as_deref()
            != Some("0");
        // Hype-train GQL confirmation defaults on; tuning blob for Settings.
        let hype_gql = crate::hype::gql_enabled(&core.store);
        let hype_tuning = crate::hype::load_tuning(&core.store);
        // Do Not Disturb defaults off in both dimensions.
        let dnd_enabled =
            setting_or_empty(&core, crate::notifications::K_DND_ENABLED) == "1";
        let dnd_schedule_enabled =
            setting_or_empty(&core, crate::notifications::K_DND_SCHEDULE_ENABLED) == "1";
        let dnd_start = setting_or_empty(&core, crate::notifications::K_DND_START);
        let dnd_start = if dnd_start.is_empty() { "22:00".to_string() } else { dnd_start };
        let dnd_end = setting_or_empty(&core, crate::notifications::K_DND_END);
        let dnd_end = if dnd_end.is_empty() { "08:00".to_string() } else { dnd_end };
        let primary_platform_pref = crate::platform_pref::global_primary_platform(&core.store);

        let schedule_compact = setting_or_empty(&core, K_SCHEDULE_COMPACT) == "1";

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
        let default_video_out = core
            .store
            .get_setting(K_VIDEO_DEFAULT_OUT)
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                crate::app_paths::default_video_output_dir()
                    .to_string_lossy()
                    .to_string()
            });

        let settings = SettingsForm {
            twitch_client_id: setting_or_empty(&core, K_TWITCH_ID),
            twitch_client_secret: setting_or_empty(&core, K_TWITCH_SECRET),
            google_client_id: setting_or_empty(&core, google_oauth::K_CLIENT_ID),
            google_client_secret: setting_or_empty(&core, google_oauth::K_CLIENT_SECRET),
            youtube_api_key: setting_or_empty(&core, K_YT_KEY),
            youtube_api_detect: setting_or_empty(&core, K_YT_API_DETECT) == "1",
            youtube_api_schedule: setting_or_empty(&core, K_YT_API_SCHEDULE) == "1",
            youtube_api_quota_cutoff: setting_or_empty(&core, K_YT_API_QUOTA_CUTOFF),
            youtube_search_quota_cutoff: setting_or_empty(&core, K_YT_SEARCH_QUOTA_CUTOFF),
            kick_client_id: setting_or_empty(&core, K_KICK_ID),
            kick_client_secret: setting_or_empty(&core, K_KICK_SECRET),
            default_output_dir: default_out,
            default_video_output_dir: default_video_out,
            max_concurrent_downloads: core
                .store
                .get_setting(K_MAX_CONCURRENT)
                .ok()
                .flatten()
                .unwrap_or_else(|| "3".into()),
            download_rate_limit: setting_or_empty(&core, crate::io_gate::K_DOWNLOAD_RATE_LIMIT),
            capture_cache_root: setting_or_empty(&core, crate::downloader::K_CACHE_ROOT),
            ytdlp_ppa: setting_or_empty(&core, crate::io_gate::K_YTDLP_PPA),
            download_auth_method: core
                .store
                .get_setting(K_DOWNLOAD_AUTH)
                .ok()
                .flatten()
                .unwrap_or_else(|| "none".into()),
            cookies_browser: split_browser_profile(&setting_or_empty(&core, K_COOKIES_BROWSER)).0,
            cookies_profile: split_browser_profile(&setting_or_empty(&core, K_COOKIES_BROWSER)).1,
            websub_vps_url: setting_or_empty(&core, K_WEBSUB_URL),
            websub_token: setting_or_empty(&core, K_WEBSUB_TOKEN),
            websub_poll_secs: core
                .store
                .get_setting(K_WEBSUB_POLL)
                .ok()
                .flatten()
                .unwrap_or_else(|| "15".into()),
            filename_media_info: MediaInfoMode::parse(&setting_or_empty(&core, K_FILENAME_MEDIA)),
            date_fmt: DateFmt::parse(&setting_or_empty(&core, K_DATE_FORMAT)),
            short_ts_fmt: {
                let v = setting_or_empty(&core, K_SHORT_TS_FMT);
                if v.is_empty() { "%d/%m %H:%M".to_string() } else { v }
            },
            schedule_default_view: ScheduleMode::parse(&setting_or_empty(&core, K_SCHEDULE_DEFAULT_VIEW)),
            ytdlp_default_args: setting_or_empty(&core, K_YTDLP_ARGS),
            ytdlp_binary_path: setting_or_empty(&core, K_YTDLP_BINARY),
            sabr_binary_path: setting_or_empty(&core, K_SABR_BINARY),
            // Absent ⇒ enabled by default; an explicit "0" disables it.
            sabr_enabled: setting_or_empty(&core, K_SABR_ENABLED) != "0",
            sabr_format: {
                let v = setting_or_empty(&core, K_SABR_FORMAT);
                if v.is_empty() {
                    crate::downloader::SABR_DEFAULT_FORMAT.to_string()
                } else {
                    v
                }
            },
            sabr_extractor_args: {
                let v = setting_or_empty(&core, K_SABR_EXTRACTOR_ARGS);
                if v.is_empty() {
                    crate::downloader::SABR_DEFAULT_EXTRACTOR_ARGS.to_string()
                } else {
                    v
                }
            },
            // Absent ⇒ off; an explicit "1" enables it.
            sabr_deep_rewind: setting_or_empty(&core, K_SABR_DEEP_REWIND) == "1",
            sabr_raw_args: setting_or_empty(&core, K_SABR_RAW_ARGS),
            // Absent ⇒ bgutil default; present (even empty) ⇒ honor it verbatim so
            // the user can disable it and rely on the plugin's auto-detection.
            sabr_pot_args: match core.store.get_setting(K_SABR_POT_ARGS) {
                Ok(Some(v)) => v,
                _ => crate::downloader::SABR_DEFAULT_POT_ARGS.to_string(),
            },
            // GLOBAL codec/quality default: unknown/absent ⇒ Auto (never Inherit —
            // there is nothing above the global to inherit from).
            sabr_codec_pref: match SabrCodecPref::parse(&setting_or_empty(&core, K_SABR_CODEC_PREF)) {
                SabrCodecPref::Inherit => SabrCodecPref::Auto,
                other => other,
            },
            sabr_codec_custom: setting_or_empty(&core, K_SABR_CODEC_CUSTOM),
            dash_format: {
                let v = setting_or_empty(&core, K_DASH_FORMAT);
                if v.is_empty() {
                    crate::downloader::DASH_DEFAULT_FORMAT.to_string()
                } else {
                    v
                }
            },
            // Absent ⇒ on: the managed server should just work out of the box.
            pot_server_autostart: setting_or_empty(&core, crate::pot_server::K_POT_SERVER_AUTOSTART)
                != "0",
            pot_server_dir: setting_or_empty(&core, crate::pot_server::K_POT_SERVER_DIR),
            pot_server_node: setting_or_empty(&core, crate::pot_server::K_POT_SERVER_NODE),
            discord_token: setting_or_empty(&core, K_DISCORD_TOKEN),
            discord_schedule: setting_or_empty(&core, K_DISCORD_SCHEDULE) == "1",
            ocr_command: setting_or_empty(&core, K_OCR_COMMAND),
            ocr_model: setting_or_empty(&core, K_OCR_MODEL),
            ocr_fallback_model: setting_or_empty(&core, K_OCR_FALLBACK_MODEL),
            ocr_timezone: setting_or_empty(&core, K_OCR_TIMEZONE),
            ocr_offset: setting_or_empty(&core, K_OCR_OFFSET),
            ocr_max_budget: setting_or_empty(&core, K_OCR_MAX_BUDGET),
            ocr_timeout_secs: setting_or_empty(&core, K_OCR_TIMEOUT_SECS),
            ocr_effort: setting_or_empty(&core, K_OCR_EFFORT),
            schedule_title_fill: setting_or_empty(&core, K_SCHEDULE_TITLE_FILL) == "1",
            youtube_community_max_posts: setting_or_empty(&core, K_YT_COMMUNITY_MAX_POSTS),
            dialog_icon: setting_or_empty(&core, K_DIALOG_ICON),
            remux_embed_thumbnail: core.store.get_setting(K_REMUX_EMBED_THUMBNAIL)
                .ok().flatten().map_or(true, |v| v != "0"),
            remux_embed_title: setting_or_empty(&core, K_REMUX_EMBED_TITLE) == "1",
            remux_title_template: {
                let v = setting_or_empty(&core, K_REMUX_TITLE_TEMPLATE);
                if v.is_empty() { "{title}".into() } else { v }
            },
            remux_embed_subs: setting_or_empty(&core, K_REMUX_EMBED_SUBS) == "1",
            postproc_readrate: setting_or_empty(&core, crate::io_gate::K_POSTPROC_READRATE)
                .parse::<f64>()
                .unwrap_or(crate::io_gate::DEFAULT_READRATE),
            iomon_sample_log: {
                let v = setting_or_empty(&core, crate::iomon::K_IOMON_LOG);
                if v.is_empty() { crate::iomon::SAMPLE_LOG_DEFAULT } else { v == "1" }
            },
            file_split_enabled: setting_or_empty(&core, K_FILE_SPLIT_ENABLED) == "1",
            file_split_videos: { let v = setting_or_empty(&core, K_FILE_SPLIT_VIDEOS); if v.is_empty() { "videos".into() } else { v } },
            file_split_subs:   { let v = setting_or_empty(&core, K_FILE_SPLIT_SUBS);   if v.is_empty() { "subs".into()   } else { v } },
            file_split_chat:   { let v = setting_or_empty(&core, K_FILE_SPLIT_CHAT);   if v.is_empty() { "chat".into()   } else { v } },
            file_split_thumbs: { let v = setting_or_empty(&core, K_FILE_SPLIT_THUMBS); if v.is_empty() { "thumbs".into() } else { v } },
            file_split_logs:   { let v = setting_or_empty(&core, K_FILE_SPLIT_LOGS);   if v.is_empty() { "logs".into()   } else { v } },
            fetch_thumb_embed: false,
            maintenance_filename_preset: String::new(),
            maintenance_apply_all: false,
            media_player_path: {
                let v = setting_or_empty(&core, K_MEDIA_PLAYER);
                if v.is_empty() { r"C:\Progs\mpv\mpv.exe".into() } else { v }
            },
            viewer_downsample_days: setting_or_empty(
                &core,
                crate::store::K_VH_DOWNSAMPLE_DAYS,
            )
            .trim()
            .parse()
            .unwrap_or(0),
            token_style_branded: setting_or_empty(&core, crate::downloader::K_TOKEN_STYLE)
                == "branded",
            token_overrides: setting_or_empty(&core, crate::downloader::K_TOKEN_OVERRIDES),
            gap_recover: setting_or_empty(&core, crate::downloader::K_GAP_RECOVER) != "0",
            gap_splice: setting_or_empty(&core, crate::downloader::K_GAP_SPLICE) != "0",
            gap_splice_cleanup: crate::disposal::GapSpliceCleanup::parse(&setting_or_empty(
                &core,
                crate::disposal::K_GAP_SPLICE_CLEANUP,
            ))
            .unwrap_or_default(),
            auto_recover_muted: setting_or_empty(&core, crate::recovery::K_AUTO_RECOVER_MUTED) == "1",
            auto_recover_deleted: setting_or_empty(&core, crate::recovery::K_AUTO_RECOVER_DELETED) == "1",
            recovery_cdn_hosts: setting_or_empty(&core, crate::recovery::K_RECOVERY_CDN_HOSTS),
            recovery_quality: setting_or_empty(&core, crate::recovery::K_RECOVERY_QUALITY),
            recovery_max_conc: setting_or_empty(&core, crate::recovery::K_RECOVERY_MAX_CONC),
            ad_probe: setting_or_empty(&core, crate::downloader::K_AD_PROBE) != "0",
            vod_dl_enabled: setting_or_empty(&core, crate::vod_archive::K_VOD_DL_ENABLED) == "1",
            vod_dl_replace: setting_or_empty(&core, crate::vod_archive::K_VOD_DL_REPLACE) == "1",
            // Default ON: missing key or anything but "0" ⇒ true.
            head_backfill_fetch_new_take: core
                .store
                .get_setting(crate::head_backfill::K_HEAD_BACKFILL_FETCH)
                .ok()
                .flatten()
                .is_none_or(|v| v != "0"),
            // Default ON, same convention.
            quality_upgrade_restart: core
                .store
                .get_setting(crate::downloader::K_QUALITY_UPGRADE)
                .ok()
                .flatten()
                .is_none_or(|v| v != "0"),
            head_backfill_replace_old: core
                .store
                .get_setting(crate::head_backfill::K_HEAD_BACKFILL_REPLACE)
                .ok()
                .flatten()
                .is_none_or(|v| v != "0"),
            join_cleanup: crate::disposal::global_join_cleanup(&core.store),
            disposal_method: crate::disposal::global_method(&core.store),
            disposal_trash_dirs: setting_or_empty(&core, crate::disposal::K_TRASH_DIRS),
            trigger_rules: crate::triggers::load_global_rules(&core.store),
            trigger_block_rules: crate::triggers::load_global_block_rules(&core.store),
            custom_tools: crate::downloader::load_custom_tools(&core.store),
            // Installed by main() before the UI starts, so this reflects the
            // persisted per-disk config (or the legacy-seeded defaults).
            disk_default_local: crate::io_gate::disk_limits_config().default.local_permits,
            disk_default_cdn: crate::io_gate::disk_limits_config().default.cdn_permits,
            disk_default_dynamic: crate::io_gate::disk_limits_config().default.dynamic,
            disk_default_paused: crate::io_gate::disk_limits_config().default.paused,
            disk_overrides: {
                let mut v: Vec<_> =
                    crate::io_gate::disk_limits_config().drives.into_iter().collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                v
            },
        };
        // Apply the loaded date format + short-timestamp pattern before the first render.
        set_active_date_fmt(settings.date_fmt);
        set_short_ts_pattern(&settings.short_ts_fmt);
        // `settings` is moved into the app struct below, before its own
        // `schedule_mode:` field would otherwise be able to read it.
        let schedule_mode = settings.schedule_default_view;

        let twitch_flow = Arc::new(Mutex::new(match oauth::connected_login(&core.store) {
            Some(login) => AuthFlow::Connected { login },
            None => AuthFlow::Idle,
        }));
        let google_flow = Arc::new(Mutex::new(if google_oauth::is_connected(&core.store) {
            AuthFlow::Connected {
                login: google_oauth::connected_identity(&core.store).unwrap_or_default(),
            }
        } else {
            AuthFlow::Idle
        }));

        // Status row tint defaults on; only an explicit "0" disables it.
        let status_bgcolor = core
            .store
            .get_setting(K_STATUS_BGCOLOR)
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true);
        // The Actions column defaults on; only an explicit "0" hides it.
        let show_actions = core
            .store
            .get_setting(K_SHOW_ACTIONS)
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true);
        // Short timestamps default off; only an explicit "1" enables.
        let shorten_timestamps = core
            .store
            .get_setting(K_SHORT_TIMESTAMPS)
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false);
        set_short_ts(shorten_timestamps);

        // Inline chat emotes default on; only an explicit "0" disables.
        let render_emotes = core
            .store
            .get_setting(K_RENDER_EMOTES)
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true);
        // Animated emotes default on; only an explicit "0" disables.
        let animate_emotes = core
            .store
            .get_setting(K_ANIMATE_EMOTES)
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true);

        let mut download_defaults = core
            .store
            .get_setting("download_defaults")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<DownloadDefaults>(&s).ok())
            .unwrap_or_else(|| DownloadDefaults::seeded(&settings.default_video_output_dir));
        // One-shot heal (marker-guarded, so re-choosing streamlink later
        // sticks): defaults persisted under the old seed gave Generic
        // downloads streamlink, which fails on plain video pages that yt-dlp
        // handles fine (2026-07-18: an NRK URL in the Videos tab died with
        // streamlink's "No plugin can handle URL").
        const K_GENERIC_TOOL_HEALED: &str = "download_defaults_generic_ytdlp_healed";
        if core.store.get_setting(K_GENERIC_TOOL_HEALED).ok().flatten().as_deref() != Some("1") {
            if download_defaults.heal_legacy_generic_tool() {
                tracing::info!(
                    "download defaults: generic on-demand tool healed streamlink → yt-dlp \
                     (streamlink can't download plain video pages)"
                );
                if let Ok(json) = serde_json::to_string(&download_defaults) {
                    let _ = core.store.set_setting("download_defaults", &json);
                }
            }
            let _ = core.store.set_setting(K_GENERIC_TOOL_HEALED, "1");
        }
        // Platforms added after the struct was first persisted (NRK/Nebula)
        // deserialize with an empty output dir — complete them from the
        // video-download default (not the recording default — these are
        // downloads, not recordings) before the Videos form ever sees them.
        download_defaults.fill_empty_output_dirs(&settings.default_video_output_dir);

        let monitor_defaults = core
            .store
            .get_setting(K_MONITOR_DEFAULTS)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<MonitorDefaults>(&s).ok())
            .unwrap_or_default();

        // Snapshot job enable/disable state before `core` is moved into the struct.
        let job_toggles: std::collections::HashMap<String, bool> =
            crate::events::TOGGLEABLE_JOBS
                .iter()
                .map(|(_, key)| (key.to_string(), core.store.job_enabled(key)))
                .collect();
        // Load user-defined filename presets before `core` is moved.
        let initial_custom_presets = core.store.get_filename_presets().unwrap_or_default();
        // Load every grid table's persisted column order/visibility + sort
        // before `core` is moved; see `crate::grid_columns`.
        let mut streams_grid = GridState::load(&core.store, GridTableId::Streams, &STREAM_COLUMNS);
        // "Scheduled rec" (schema v51) should start hidden — every other column
        // defaults to visible (grid_columns.rs's deliberate "new column id =
        // visible" rule), but this one is niche enough that showing it
        // unconditionally would clutter the grid for anyone who never uses
        // scheduled recordings. One-time seed: the first time this column
        // shows up in a fresh/older persisted list, hide it and persist
        // immediately; a settings marker stops this from re-hiding it once the
        // user has had a chance to turn it back on themselves.
        const K_SCHED_REC_COL_SEEDED: &str = "streams_scheduled_rec_col_seeded";
        if core.store.get_setting(K_SCHED_REC_COL_SEEDED).ok().flatten().is_none() {
            grid_columns::set_visible(&mut streams_grid.entries, "scheduled_rec", false);
            grid_columns::save_columns(&core.store, GridTableId::Streams, &streams_grid.entries);
            let _ = core.store.set_setting(K_SCHED_REC_COL_SEEDED, "1");
        }
        let streams_sort_persisted = grid_columns::load_sort(&core.store, GridTableId::Streams);
        let videos_grid = GridState::load(&core.store, GridTableId::Videos, &VIDEO_COLUMNS);
        let videos_sort_persisted = grid_columns::load_sort(&core.store, GridTableId::Videos);
        let bg_active_grid = GridState::load(&core.store, GridTableId::BgActive, &BG_ACTIVE_COLUMNS);
        let bg_recent_grid = GridState::load(&core.store, GridTableId::BgRecent, &BG_RECENT_COLUMNS);
        let processes_grid = GridState::load(&core.store, GridTableId::Processes, &PROCESSES_COLUMNS);
        let issues_grid = GridState::load(&core.store, GridTableId::Issues, &ISSUES_COLUMNS);
        let settings_tab = SettingsTab::from_id(&setting_or_empty(&core, K_SETTINGS_TAB));

        let mut app = StreamArchiverApp {
            core,
            _tray: tray,
            ui_rx,
            events_rx,
            autostart,
            autostart_on,
            keep_downloads_on_quit,
            notifications_enabled,
            collab_eventsub,
            raid_eventsub,
            dnd_enabled,
            dnd_schedule_enabled,
            dnd_start,
            dnd_end,
            primary_platform_pref,
            show_processes: false,
            processes: Vec::new(),
            processes_refreshed: None,
            processes_load: None,
            show_issues: false,
            issues_recs: Vec::new(),
            issues_missing: Vec::new(),
            issues_errors: Vec::new(),
            issues_errors_no_file: Vec::new(),
            issues_unmerged: Vec::new(),
            issues_head_mismatch: Vec::new(),
            issues_gap_splice: Vec::new(),
            issues_stale_recording: Vec::new(),
            issues_stuck: Vec::new(),
            issues_muted_vod: Vec::new(),
            issues_missing_load: None,
            issues_refreshed: None,
            issues_dirty: false,
            issues_confirm_clear: false,
            issues_error_view: None,
            show_notifications: false,
            notifications: Vec::new(),
            show_pot_server_log: false,
            pot_log_text: String::new(),
            pot_log_refreshed: None,
            notif_refreshed: None,
            notif_unread: 0,
            notif_search: String::new(),
            notif_kind_filter: None,
            show_warnings: false,
            warnings_rows: Vec::new(),
            rec_alert_badges: std::collections::HashMap::new(),
            warn_refreshed: None,
            warn_badge: (0, 0),
            warn_search: String::new(),
            warn_sev_filter: None,
            warn_hide_acked: false,
            show_posts_window: false,
            posts: Vec::new(),
            posts_refreshed: None,
            posts_search: String::new(),
            posts_channel_filter: None,
            posts_show_viewer: false,
            posts_render_limit: POSTS_PAGE_SIZE,
            post_img_cache: HashMap::new(),
            show_inspector: false,
            inspector: crate::inspector::InspectorState::default(),
            quitting: false,
            heartbeat,
            view: View::Streams,
            help: None,
            topbar: TopBarLayout::default(),
            rows: Vec::new(),
            channels: Vec::new(),
            videos: Vec::new(),
            form: None,
            video_form: VideoForm::new(),
            download_defaults,
            monitor_defaults,
            settings_tab,
            settings_search: String::new(),
            format_probe: Arc::new(Mutex::new(FormatProbe::Idle)),
            recover_form: None,
            recover_probe: Arc::new(Mutex::new(RecoverProbe::Idle)),
            recover_scrape: Arc::new(Mutex::new(RecoverScrape::Idle)),
            settings,
            status: String::new(),
            selected_monitor: None,
            confirm_delete: None,
            confirm_delete_channel: None,
            confirm_delete_segment: None,
            channel_form: None,
            show_scheduled_recordings: false,
            scheduled_recordings: Vec::new(),
            scheduled_recording_form: None,
            confirm_delete_scheduled_recording: None,
            sched_rec_add_monitor: 0,
            streams_sort: SortState {
                keys: grid_columns::resolve_sort(&STREAM_COLUMNS, &streams_sort_persisted)
                    .into_iter()
                    .map(|(col, ascending)| SortLevel { col, ascending })
                    .collect(),
            },
            streams_filters: vec![String::new(); STREAM_COLS],
            expanded_channels: HashSet::new(),
            expanded_instances: HashSet::new(),
            expanded_streams: HashSet::new(),
            period_toggles: HashSet::new(),
            rec_cache: HashMap::new(),
            ad_break_cache: HashMap::new(),
            ad_popups: Vec::new(),
            meta_change_cache: HashMap::new(),
            meta_popups: Vec::new(),
            history_change_cache: HashMap::new(),
            history_popups: Vec::new(),
            schedule_cache: HashMap::new(),
            schedule_popups: Vec::new(),
            schedule_all: Vec::new(),
            schedule_loaded: false,
            schedule_mode,
            schedule_anchor: None,
            schedule_hidden: HashSet::new(),
            schedule_hidden_segments: HashSet::new(),
            schedule_show_hidden: false,
            schedule_collisions: true,
            schedule_zoom: 1.0,
            schedule_chan_colors: HashMap::new(),
            schedule_compact,
            schedule_day_popup: None,
            show_schedule_sources: false,
            schedule_sources_draft: Vec::new(),
            schedule_sources_selected: None,
            channel_cfg_drafts: HashMap::new(),
            channel_scope_drafts: HashMap::new(),
            instance_scope_drafts: HashMap::new(),
            channel_trigger_drafts: HashMap::new(),
            instance_trigger_drafts: HashMap::new(),
            channel_block_drafts: HashMap::new(),
            instance_block_drafts: HashMap::new(),
            edit_schedule: None,
            schedule_selected: HashSet::new(),
            merge_preview: None,
            confirm_delete_segments: None,
            schedule_merge_labels: HashMap::new(),
            schedule_auto_secondary: HashSet::new(),
            custom_presets: initial_custom_presets,
            save_preset_dialog: None,
            chat_popups: Vec::new(),
            platform_tex: None,
            properties_popups: Vec::new(),
            channel_properties_popups: Vec::new(),
            rec_props_popups: Vec::new(),
            channel_icons: HashMap::new(),
            channel_icons_small: HashMap::new(),
            instance_icons_small: HashMap::new(),
            emote_anim: Arc::new(Mutex::new(HashMap::new())),
            emote_epoch: Arc::new(AtomicU64::new(0)),
            channel_asset_thumbs: HashMap::new(),
            channel_emote_counts: HashMap::new(),
            channel_asset_status: HashMap::new(),
            props_source_order: Vec::new(),
            props_loads: Vec::new(),
            pending_browse: None,
            pending_save: None,
            pending_reload: None,
            reload_queued: false,
            last_auto_reload: now_unix(),
            fs_probes: FsProbes::new(egui_ctx),
            videos_refreshed: None,
            videos_rev: 0,
            videos_model_cache: None,
            settings_search_lc: String::new(),
            recovery_host_count: None,
            streams_cache: None,
            streams_cache_rev: 0,
            yt_quota_today: 0,
            yt_quota_cutoff: 9000,
            yt_search_today: 0,
            yt_search_cutoff: 90,
            dismissed_quota_warnings: HashSet::new(),
            pending_schedule: None,
            emote_viewers: Vec::new(),
            asset_histories: Vec::new(),
            about_views: Vec::new(),
            channel_about_latest: HashMap::new(),
            provider_tex: None,
            channel_twitch_colors: HashMap::new(),
            videos_sort: SortState {
                keys: grid_columns::resolve_sort(&VIDEO_COLUMNS, &videos_sort_persisted)
                    .into_iter()
                    .map(|(col, ascending)| SortLevel { col, ascending })
                    .collect(),
            },
            videos_filters: vec![String::new(); VIDEO_COLS],
            twitch_flow,
            google_flow,
            import_dialog: None,
            collab_by_stream: HashMap::new(),
            collab_history: None,
            partner_sessions: None,
            status_bgcolor,
            show_actions,
            shorten_timestamps,
            render_emotes,
            animate_emotes,
            reset_streams_columns: false,
            streams_grid,
            videos_grid,
            bg_active_grid,
            bg_recent_grid,
            bg_show_gate_queue: false,
            processes_grid,
            issues_grid,
            reorder_columns: None,
            background_tasks: Vec::new(),
            finished_tasks: Vec::new(),
            job_toggles,
            format_designer: None,
            confirm_quit_stop: false,
            debug_monitor_idx: 0,
            debug_test_title: "Test Stream Title".into(),
            debug_test_game: "Just Chatting".into(),
            stats_snapshot: None,
            stats_capture_health: None,
            stats_collabs: Vec::new(),
            stats_poll_span: super::PollSpan::Day,
            stats_history: None,
            chstats_channel: None,
            chstats_span: super::PollSpan::Month,
            chstats_data: None,
            chstats_auto,
            chstats_loaded_at: 0,
            chstats_event_filter: String::new(),
            viewer_stats_popup: None,
            hype_gql,
            hype_tuning,
            show_hype_mark: false,
            hype_mark_channel: 0,
            hype_mark_mins_ago: 5,
            hype_mark_abs: String::new(),
            hype_mark_dur: 0,
            hype_override_for: None,
            hype_override_draft: crate::hype::HypeOverride::default(),
            spark_data: std::collections::HashMap::new(),
            spark_loaded_at: 0,
            io_hist: Vec::new(),
            io_snap: None,
            io_refreshed: None,
            io_tab: IoTab::Disks,
            io_plot_metric: IoPlotMetric::Write,
            io_ops_cat: None,
            io_ops_region: None,
            io_cat_sort: (2, false), // write B/s, descending
            files_scan: None,
            files_scan_rx: None,
            files_edit: std::collections::HashMap::new(),
            files_selected: std::collections::HashSet::new(),
            files_batch_dir: String::new(),
            files_redirect_from: String::new(),
            files_redirect_to: String::new(),
            files_reloc_from: String::new(),
            files_reloc_to: String::new(),
            files_reloc_monitors: false,
            files_reloc_preview: None,
            files_status: String::new(),
            scroll_to_channel: None,
            show_rename_dialog: false,
            rename_rec_id: None,
            rename_draft: String::new(),
            rename_preview: String::new(),
        };
        app.reload_rows();
        app.reload_videos();
        app.yt_quota_today = app.core.store.get_quota_today("youtube").unwrap_or(0);
        app.yt_quota_cutoff = app
            .core
            .store
            .get_setting(K_YT_API_QUOTA_CUTOFF)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(9000);
        app.yt_search_today = app.core.store.get_quota_today("youtube_search").unwrap_or(0);
        app.yt_search_cutoff = app
            .core
            .store
            .get_setting(K_YT_SEARCH_QUOTA_CUTOFF)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(90);
        app
    }

    pub(super) fn reload_rows(&mut self) {
        let _t = std::time::Instant::now();
        match self.core.store.list_monitors_with_channels() {
            Ok(rows) => self.rows = rows,
            Err(e) => {
                warn!("failed to load monitors: {e:#}");
                self.status = format!("Error loading channels: {e}");
            }
        }
        // Merge in each monitor's next upcoming scheduled stream (the row query
        // doesn't carry it — it's refreshed on a separate cadence).
        if let Ok(next) = self.core.store.next_scheduled_streams(now_unix()) {
            let by_mid: HashMap<i64, (i64, String)> =
                next.into_iter().map(|(mid, at, title)| (mid, (at, title))).collect();
            for row in &mut self.rows {
                if let Some((at, title)) = by_mid.get(&row.monitor.id) {
                    row.next_stream_at = Some(*at);
                    row.next_stream_title = title.clone();
                }
            }
        }
        // Load all containers (incl. empty ones) so they show in the tree.
        match self.core.store.list_channels() {
            Ok(chs) => self.channels = chs,
            Err(e) => warn!("failed to load channels: {e:#}"),
        }
        // Scheduled recordings (schema v51) — small table, cheap to reload
        // in full; drives the toolbar count and the grid column.
        match self.core.store.list_scheduled_recordings() {
            Ok(v) => self.scheduled_recordings = v,
            Err(e) => warn!("failed to load scheduled recordings: {e:#}"),
        }
        // Collab history map for the stream/take rows' 🤝 cells (small table,
        // one query; avoids any per-frame DB access from the grid).
        match self.core.store.collab_names_by_stream() {
            Ok(m) => self.collab_by_stream = m,
            Err(e) => warn!("failed to load collab sessions: {e:#}"),
        }
        // Drop expansion state for channels/monitors that no longer exist (avoids
        // an unbounded leak and "sticky" expansion if a row id is later reused).
        let live_channels: HashSet<i64> = self.channels.iter().map(|c| c.id).collect();
        let live_monitors: HashSet<i64> = self.rows.iter().map(|r| r.monitor.id).collect();
        // Evict dead monitors from the rec/ad/meta/schedule caches, but KEEP
        // entries for live monitors — clearing everything forces a synchronous
        // recordings_for_monitor() DB call for every expanded monitor on the very
        // next frame, which freezes the UI thread proportionally to the number of
        // expanded rows. Stale data (a just-finished recording) is refreshed at
        // the specific UiCommand::Reload sites that already know which monitor changed.
        self.rec_cache.retain(|k, _| live_monitors.contains(k));
        self.ad_break_cache.retain(|k, _| live_monitors.contains(k));
        self.meta_change_cache.retain(|k, _| live_monitors.contains(k));
        self.schedule_cache.retain(|k, _| live_monitors.contains(k));
        self.expanded_channels.retain(|id| live_channels.contains(id));
        self.expanded_instances.retain(|id| live_monitors.contains(id));
        // Stream keys are "s<mid>:…" / "t<mid>:…"; keep only live monitors'.
        self.expanded_streams
            .retain(|k| stream_key_monitor(k).is_some_and(|mid| live_monitors.contains(&mid)));
        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
        let elapsed_ms = _t.elapsed().as_millis();
        if elapsed_ms >= 100 {
            warn!(elapsed_ms, rows = self.rows.len(), "reload_rows slow");
        } else {
            debug!(elapsed_ms, rows = self.rows.len(), "reload_rows");
        }
    }

    pub(super) fn reload_videos(&mut self) {
        match self.core.store.list_videos() {
            Ok(v) => self.videos = v,
            Err(e) => warn!("failed to load videos: {e:#}"),
        }
        self.videos_refreshed = Some(std::time::Instant::now());
        self.videos_rev = self.videos_rev.wrapping_add(1);
    }

    /// Spawn a background thread to reload the Schedule calendar.  The result is
    /// installed by [`Self::drain_pending_schedule`] in the next update cycle.
    /// Loads 90 days of history plus all future events; the 90-day cutoff keeps
    /// the idx_schedule_canceled_start scan bounded.  If a reload is already in
    /// flight the new request is dropped — the in-flight one will land shortly.
    pub(super) fn spawn_reload_schedule(&mut self) {
        // Always clear the per-monitor popup cache immediately so an edit/delete
        // isn't shown stale in the Properties "Upcoming streams" list.
        self.schedule_cache.clear();
        if self.pending_schedule.is_some() {
            return;
        }
        let after = crate::models::now_unix() - 90 * 86_400;
        let store = self.core.store.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("reload-schedule".into())
            .spawn(move || {
                let result = (|| -> Option<Vec<UpcomingStream>> {
                    let mut v = store.all_upcoming_schedule(after).ok()?;
                    // Collapse cross-source duplicates that may have been stored before
                    // the per-timestamp eviction was in place. Two rows with the same
                    // (monitor_id, start_time) but different sources are the same event;
                    // keep the higher-priority source's version.
                    let source_priority = |s: &str| -> u8 {
                        match s {
                            "manual" => 0,
                            "platform" => 1,
                            "youtube_api" => 2,
                            "youtube" => 3,
                            "twitch_banner_ocr" | "youtube_community_ocr"
                            | "twitter_pinned" | "other_image_ocr" => 4,
                            "discord" => 5,
                            _ => 6,
                        }
                    };
                    v.sort_by(|a, b| {
                        a.monitor_id
                            .cmp(&b.monitor_id)
                            .then(a.start_time.cmp(&b.start_time))
                            .then(source_priority(&a.source).cmp(&source_priority(&b.source)))
                    });
                    v.dedup_by(|a, b| {
                        a.monitor_id == b.monitor_id && a.start_time == b.start_time
                    });
                    // Restore display order: soonest first, then channel name.
                    v.sort_by(|a, b| {
                        a.start_time
                            .cmp(&b.start_time)
                            .then(a.channel_name.to_lowercase().cmp(&b.channel_name.to_lowercase()))
                    });
                    Some(v)
                })();
                let _ = tx.send(result);
            })
            .ok();
        self.pending_schedule = Some(rx);
    }

    /// Install schedule calendar results from an in-flight background reload.
    /// Called every frame from the main update loop.
    pub(super) fn drain_pending_schedule(&mut self) {
        let recv = match &self.pending_schedule {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match recv {
            Ok(Some(v)) => {
                self.schedule_all = v;
                // Drop hide choices only for channels that no longer EXIST (deleted),
                // not ones merely without an upcoming stream right now.
                let live: HashSet<i64> = self.channels.iter().map(|c| c.id).collect();
                if !live.is_empty() {
                    self.schedule_hidden.retain(|id| live.contains(id));
                }
                self.recompute_merge_state();
                // Only latch on success so a transient error retries via lazy-load.
                self.schedule_loaded = true;
                self.pending_schedule = None;
            }
            Ok(None) => {
                warn!("reload-schedule thread produced no data");
                self.status = "Error loading schedule.".into();
                self.pending_schedule = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                warn!("reload-schedule thread disconnected without sending");
                self.pending_schedule = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Recompute `schedule_merge_labels` and `schedule_auto_secondary` from the
    /// current `schedule_all`. Call after any change to `schedule_all` or to
    /// `merged_into`/`auto_merge_excluded` on any segment.
    pub(super) fn recompute_merge_state(&mut self) {
        self.schedule_merge_labels.clear();
        self.schedule_auto_secondary.clear();

        // ── Auto-merge: group same-channel overlapping events ──────────────
        // Group non-excluded, non-manual-secondary segments by channel_id.
        let mut by_channel: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, s) in self.schedule_all.iter().enumerate() {
            if s.merged_into.is_some() { continue; } // manual secondary → skip
            by_channel.entry(s.channel_id).or_default().push(i);
        }
        for indices in by_channel.values() {
            // schedule_all is already sorted by start_time, so each channel's
            // list is time-ordered.
            let mut processed: HashSet<usize> = HashSet::new();
            for &pi in indices {
                if processed.contains(&pi) { continue; }
                let ps = &self.schedule_all[pi];
                if ps.auto_merge_excluded { continue; }
                let p_end = ps.end_time.unwrap_or(ps.start_time + 3600);
                let mut group: Vec<usize> = vec![pi];
                let mut group_end = p_end;
                for &si in indices {
                    if si == pi || processed.contains(&si) { continue; }
                    let ss = &self.schedule_all[si];
                    if ss.auto_merge_excluded { continue; }
                    let s_end = ss.end_time.unwrap_or(ss.start_time + 3600);
                    if ss.start_time < group_end && s_end > ps.start_time {
                        group.push(si);
                        group_end = group_end.max(s_end);
                    }
                }
                if group.len() > 1 {
                    // Pick primary: highest source priority (YouTube first)
                    let primary_pos = group
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, gi)| {
                            merge_source_priority(&self.schedule_all[**gi].source)
                        })
                        .map(|(pos, _)| pos)
                        .unwrap_or(0);
                    let primary_idx = group[primary_pos];
                    let primary_id = self.schedule_all[primary_idx].segment_id;
                    // Collect secondary info before mutating self.schedule_merge_labels.
                    let auto_sources: Vec<String> = group
                        .iter()
                        .enumerate()
                        .filter(|(pos, _)| *pos != primary_pos)
                        .map(|(_, si)| self.schedule_all[*si].source.clone())
                        .collect();
                    let secondary_sids: Vec<i64> = group
                        .iter()
                        .enumerate()
                        .filter(|(pos, _)| *pos != primary_pos)
                        .map(|(_, si)| self.schedule_all[*si].segment_id)
                        .collect();
                    for idx in &group {
                        processed.insert(*idx);
                    }
                    for sid in secondary_sids {
                        self.schedule_auto_secondary.insert(sid);
                    }
                    let n = auto_sources.len();
                    *self.schedule_merge_labels.entry(primary_id).or_default() =
                        format!("auto-merged {n} ({})", auto_sources.join(", "));
                }
            }
        }

        // ── Manual merge: collect primaries that have manual secondaries ───
        for s in &self.schedule_all {
            if let Some(primary_id) = s.merged_into {
                let src = s.source.clone();
                let entry = self.schedule_merge_labels.entry(primary_id).or_default();
                if !entry.is_empty() {
                    entry.push_str(&format!(", manual: {src}"));
                } else {
                    *entry = format!("manually merged ({})", src);
                }
            }
        }
    }

    pub(super) fn persist_download_defaults(&self) {
        match serde_json::to_string(&self.download_defaults) {
            Ok(json) => {
                let _ = self.core.store.set_setting("download_defaults", &json);
            }
            Err(e) => warn!("failed to serialize download defaults: {e:#}"),
        }
    }

    pub(super) fn persist_monitor_defaults(&self) {
        match serde_json::to_string(&self.monitor_defaults) {
            Ok(json) => {
                let _ = self.core.store.set_setting(K_MONITOR_DEFAULTS, &json);
            }
            Err(e) => warn!("failed to serialize monitor defaults: {e:#}"),
        }
    }

    /// Handle tray commands and bus events; returns true if a repaint is needed.
    pub(super) fn pump_messages(&mut self, ctx: &egui::Context) {
        // One-shot startup notice (e.g. detached downloads recovered on launch).
        if let Some(msg) = self.core.startup_notice.lock().unwrap().take() {
            self.status = msg;
        }
        while let Ok(cmd) = self.ui_rx.try_recv() {
            match cmd {
                UiCommand::ShowWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                UiCommand::ShowNotifications => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    // Mirror the 🔔 bell button: open, refresh, mark read.
                    self.show_notifications = true;
                    self.notif_refreshed = None;
                    let _ = self
                        .core
                        .store
                        .mark_notifications_read_before(crate::models::now_unix());
                    self.notif_unread = 0;
                }
                UiCommand::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                UiCommand::QuitAndStop => {
                    // Show confirmation before stopping active recordings.
                    self.confirm_quit_stop = true;
                    // Bring the window to the foreground so the dialog is visible.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
                Ok(crate::events::AppEvent::BackgroundTaskStarted(task)) => {
                    self.background_tasks.push(task);
                    dirty = true;
                }
                Ok(crate::events::AppEvent::BackgroundTaskProgress { id, progress, info }) => {
                    if let Some(task) = self.background_tasks.iter_mut().find(|t| t.id == id) {
                        task.progress = progress;
                        task.progress_info = Some(info);
                    }
                    dirty = true;
                }
                Ok(crate::events::AppEvent::BackgroundTaskFinished { id, outcome }) => {
                    if let Some(pos) = self.background_tasks.iter().position(|t| t.id == id) {
                        let task = self.background_tasks.remove(pos);
                        // A finished asset fetch may have produced a new channel
                        // icon or refreshed emote images — drop both caches so they
                        // reload from disk (emote files can change at a stable path).
                        if task.kind == crate::events::BackgroundTaskKind::AssetFetch {
                            // Texture caches are cleared here (in logic(), before any
                            // rendering) so new textures are allocated at the top of
                            // channels_view() this frame — safe because no paint commands
                            // have referenced them yet.
                            debug!(
                                task_label = task.label.as_str(),
                                "AssetFetch complete — clearing icon/emote texture caches"
                            );
                            self.channel_icons.clear();
                            self.channel_icons_small.clear();
                            self.instance_icons_small.clear();
                            self.channel_twitch_colors.clear();
                            // Banners/icons may have changed — drop the cached
                            // Properties thumbnails so they reload from disk.
                            self.channel_asset_thumbs.clear();
                            // Emote sets may have changed too — drop the cached counts
                            // so the launcher buttons re-enumerate.
                            self.channel_emote_counts.clear();
                            // Asset presence/variants/stamps moved — drop the cached
                            // status grid so it re-enumerates from disk on the next frame.
                            self.channel_asset_status.clear();
                            // Drop decoded emote frames so refreshed images re-decode
                            // from disk at their stable paths.
                            self.clear_emote_cache();
                            // If the emote viewer is open for this channel, its
                            // enumerated list was loaded once on open and is now
                            // stale — flag it so the window can show a re-open banner.
                            for v in &mut self.emote_viewers {
                                if v.channel_name == task.label {
                                    v.stale = true;
                                }
                            }
                            // The asset-history view reads the change logs once on
                            // open; if it's showing this channel, reload it in place
                            // so freshly-recorded changes appear without a reopen.
                            for h in &mut self.asset_histories {
                                if h.channel_name == task.label {
                                    h.reload();
                                }
                            }
                            // Same for open About viewers: re-query the snapshots
                            // (a new version may have landed) and drop their panel
                            // textures so changed images re-decode from disk.
                            for v in &mut self.about_views {
                                if v.channel_name == task.label {
                                    v.reload(&self.core.store);
                                    v.img_cache.clear();
                                }
                            }
                            // Channel-Properties About rows are stale too.
                            self.channel_about_latest.clear();
                        }
                        // Record a task_failed notification (the failing task
                        // carries its kind + label + error here; the event alone
                        // only has the id + outcome).
                        if let crate::events::TaskOutcome::Failed(err) = &outcome {
                            let mut body = task.label.clone();
                            if !err.is_empty() {
                                if !body.is_empty() {
                                    body.push_str(" — ");
                                }
                                body.push_str(err);
                            }
                            let _ = self.core.store.insert_notification(&crate::store::NewNotification {
                                kind: crate::models::NotificationKind::TaskFailed.id().to_string(),
                                severity: "error".to_string(),
                                title: format!("{} failed", task.kind.label()),
                                body,
                                channel: task.label.clone(),
                                ref_key: format!("taskfail:{}", task.id),
                                ..Default::default()
                            });
                            self.notif_refreshed = None; // surface promptly in the badge
                        }
                        self.finished_tasks.insert(0, (task, outcome, now_unix()));
                        self.finished_tasks.truncate(100);
                    }
                    dirty = true;
                }
                Ok(crate::events::AppEvent::RecordingUpdated { recording_id }) => {
                    // Recovery/VOD-archive updates change per-recording fields
                    // the reload's change-summary can't see (recovery_state,
                    // vod_dl_*), so drop the owning monitor's cached history —
                    // the 🛟/📼 badges refresh on the next rebuild instead of
                    // waiting for F5.
                    self.rec_cache
                        .retain(|_, recs| !recs.iter().any(|r| r.id == recording_id));
                    self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                    dirty = true;
                }
                Ok(_) => dirty = true,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        if dirty {
            self.spawn_pending_reload();
            self.reload_videos();
            // Keep the Schedule calendar in sync with background schedule fetches
            // (which emit a state event) — but only once it has been loaded, so we
            // don't pull it in before the user ever opens the tab.
            if self.schedule_loaded {
                self.spawn_reload_schedule();
            }
            // Any event that marks the UI dirty may affect recording statuses —
            // note it so the issues panel refreshes soon. Deliberately NOT an
            // immediate invalidation: during an event storm (startup re-attach
            // + first poll sweep of ~100 monitors) that re-ran the off-thread
            // missing-file sweep back-to-back — each pass holds the DB lock
            // for a 500-row query (convoying every poller with 50-250ms lock
            // waits) and stats up to 500 paths on the recordings drive.
            self.issues_dirty = true;
        }
    }

    pub(super) fn save_form(&mut self) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        // Validate before closing the form.
        if form.url.trim().is_empty() {
            self.status = "An instance URL is required.".into();
            return;
        }
        let new_channel_name: Option<String> = match form.channel_id {
            Some(_) => None, // existing container, no create needed
            None => {
                if form.name.trim().is_empty() {
                    self.status = "A channel name is required.".into();
                    return;
                }
                Some(form.name.trim().to_string())
            }
        };
        // Build the monitor value now; channel_id may be 0 when creating a new
        // container — the background thread overwrites it with the real id.
        let monitor = Monitor {
            id: form.monitor_id.unwrap_or(0),
            channel_id: form.channel_id.unwrap_or(0),
            url: form.url.trim().to_string(),
            enabled: form.enabled,
            automation_enabled: form.automation_enabled,
            tool: form.tool,
            detection_method: form.detection_method,
            poll_interval_secs: form.poll_interval_secs.max(5),
            quality: form.quality.clone(),
            // A no-op for the normal case (the field already holds the
            // resolved-default's literal value) — only actually re-expands
            // if the user hand-typed `{name}`/`{platform}` tokens straight
            // into the Output folder field themselves, so those never land
            // in the DB unexpanded.
            output_dir: crate::downloader::expand_dir_template(
                &form.output_dir,
                form.name.trim(),
                Platform::detect(&form.url).as_str(),
            ),
            filename_template: form.filename_template.clone(),
            container: form.container,
            capture_from_start: form.capture_from_start,
            dual_capture: form.dual_capture,
            ad_free: form.ad_free,
            auth_kind: form.auth_kind,
            auth_value: form.auth_value.clone(),
            audio_tracks: form.audio_tracks.trim().to_string(),
            subtitle_tracks: form.subtitle_tracks.trim().to_string(),
            chat_log: form.chat_log,
            fetch_thumbnail: form.fetch_thumbnail,
            thumbnail_in_toast: form.thumbnail_in_toast,
            fetch_chat_assets: form.fetch_chat_assets,
            extra_args: form.extra_args.clone(),
            max_concurrent: 1,
            last_checked_at: None,
            last_state: "idle".into(),
            last_live_since: None,
            last_live_since_approx: false,
            sabr_codec_pref: form.sabr_codec_pref,
            sabr_codec_custom: form.sabr_codec_custom.trim().to_string(),
        };
        let monitor_id = form.monitor_id;
        let vod_scope = crate::vod_archive::VodArchiveScope {
            download: form.vod_download,
            replace: form.vod_replace,
        };
        let head_backfill_scope = crate::head_backfill::HeadBackfillScope {
            fetch: form.head_backfill_fetch,
            replace: form.head_backfill_replace,
        };
        let disposal_scope = crate::disposal::DisposalScope {
            method: form.disposal_method,
            join_cleanup: form.join_cleanup,
            // No per-channel/instance gap-splice-cleanup override UI yet —
            // always inherits the global setting for now.
            gap_splice_cleanup: None,
        };
        let primary_pin = form.primary_pin;

        // Close the form immediately so the UI stays responsive while the DB
        // work runs. On a background-thread error the status bar shows the error;
        // the user can re-open Add/Edit to retry.
        self.form = None;
        self.status = "Saving…".into();

        let store = self.core.store.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        debug!("spawning save-monitor thread");
        std::thread::Builder::new()
            .name("save-monitor".into())
            .spawn(move || {
                let t = std::time::Instant::now();
                let result: Result<SaveRows, String> = (|| {
                    // Resolve the channel_id, creating a new container if needed. A
                    // brand-new channel has no other instances yet, so seed its
                    // Auto/Enabled switches from this first instance rather than
                    // leaving both at the schema default (true) — see
                    // `create_container_with_flags`.
                    let channel_id = match new_channel_name {
                        Some(name) => store
                            .create_container_with_flags(
                                &name,
                                monitor.enabled,
                                monitor.automation_enabled,
                            )
                            .map_err(|e| e.to_string())?,
                        None => monitor.channel_id,
                    };
                    let mut m = monitor;
                    m.channel_id = channel_id;
                    let (mid, new_monitor_id) = match monitor_id {
                        Some(id) => {
                            store.update_monitor(&m).map_err(|e| e.to_string())?;
                            (id, None)
                        }
                        None => {
                            let id = store.insert_monitor(&m).map_err(|e| e.to_string())?;
                            (id, Some(id))
                        }
                    };
                    let _ = crate::vod_archive::save_monitor_vod_scope(&store, mid, &vod_scope);
                    let _ = crate::head_backfill::save_monitor_head_backfill_scope(
                        &store,
                        mid,
                        &head_backfill_scope,
                    );
                    let _ = crate::disposal::save_monitor_disposal_scope(&store, mid, &disposal_scope);
                    let _ = crate::platform_pref::save_monitor_pin(&store, mid, primary_pin);
                    let rows = store.list_monitors_with_channels().map_err(|e| e.to_string())?;
                    let next_streams =
                        store.next_scheduled_streams(now_unix()).map_err(|e| e.to_string())?;
                    let channels = store.list_channels().map_err(|e| e.to_string())?;
                    let yt_quota_today = store.get_quota_today("youtube").unwrap_or(0);
                    let yt_quota_cutoff = store
                        .get_setting(K_YT_API_QUOTA_CUTOFF)
                        .ok()
                        .flatten()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(9000);
                    let yt_search_today = store.get_quota_today("youtube_search").unwrap_or(0);
                    let yt_search_cutoff = store
                        .get_setting(K_YT_SEARCH_QUOTA_CUTOFF)
                        .ok()
                        .flatten()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(90);
                    Ok(SaveRows { rows, channels, next_streams, yt_quota_today, yt_quota_cutoff, yt_search_today, yt_search_cutoff, new_monitor_id })
                })();
                debug!(elapsed_ms = t.elapsed().as_millis(), ok = result.is_ok(), "save-monitor done");
                let _ = tx.send(result);
            })
            .ok();
        self.pending_save = Some(PendingSave { rx });
    }

    /// Poll the in-flight file/folder picker. When the user confirms a selection
    /// the `apply` closure runs on the UI thread to install the chosen path.
    pub(super) fn drain_pending_browse(&mut self) {
        let recv = match &self.pending_browse {
            Some(pb) => pb.rx.try_recv(),
            None => return,
        };
        match recv {
            Ok(maybe_path) => {
                let pb = self.pending_browse.take().unwrap();
                if let Some(path) = maybe_path {
                    (pb.apply)(self, path);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_browse = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Poll the in-flight save-form thread. Installs the loaded rows on success
    /// or shows an error in the status bar on failure.
    pub(super) fn drain_pending_save(&mut self) {
        let recv = match &self.pending_save {
            Some(ps) => ps.rx.try_recv(),
            None => return,
        };
        match recv {
            Ok(Ok(save)) => {
                debug!(rows = save.rows.len(), channels = save.channels.len(), "save-monitor result installed");
                self.pending_save = None;
                self.status = "Saved.".into();
                self.install_save_rows(save);
            }
            Ok(Err(ref e)) => {
                warn!(error = %e, "save-monitor thread returned error");
                self.pending_save = None;
                self.status = format!("Error saving: {e}");
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                warn!("save-monitor thread disconnected without sending a result");
                self.pending_save = None;
                self.status = "Save failed (internal error).".into();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Spawn a background thread that reads the current rows/channels from the
    /// store without writing anything.  The result is drained by
    /// `drain_pending_reload` inside `logic()`.  If a reload is already in
    /// flight, one follow-up reload is queued instead (the in-flight thread may
    /// have read the DB before the change that triggered this request, so
    /// dropping it could leave the UI stale until the next event).
    pub(super) fn spawn_pending_reload(&mut self) {
        if self.pending_reload.is_some() {
            self.reload_queued = true;
            return;
        }
        let store = self.core.store.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // trace!, not debug!: this fires every ~30s for the app's whole uptime
        // (~12k lines/day dominating the daily log file at the debug default).
        tracing::trace!("spawning reload-rows thread");
        std::thread::Builder::new()
            .name("reload-rows".into())
            .spawn(move || {
                let t = std::time::Instant::now();
                let result = (|| -> Option<SaveRows> {
                    let rows = store.list_monitors_with_channels().ok()?;
                    let next_streams = store.next_scheduled_streams(crate::models::now_unix()).ok()?;
                    let channels = store.list_channels().ok()?;
                    let yt_quota_today = store.get_quota_today("youtube").unwrap_or(0);
                    let yt_quota_cutoff = store
                        .get_setting(K_YT_API_QUOTA_CUTOFF)
                        .ok()
                        .flatten()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(9000);
                    let yt_search_today = store.get_quota_today("youtube_search").unwrap_or(0);
                    let yt_search_cutoff = store
                        .get_setting(K_YT_SEARCH_QUOTA_CUTOFF)
                        .ok()
                        .flatten()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(90);
                    Some(SaveRows { rows, channels, next_streams, yt_quota_today, yt_quota_cutoff, yt_search_today, yt_search_cutoff, new_monitor_id: None })
                })();
                tracing::trace!(
                    elapsed_ms = t.elapsed().as_millis(),
                    ok = result.is_some(),
                    "reload-rows done"
                );
                let _ = tx.send(result);
            })
            .ok();
        self.pending_reload = Some(rx);
    }

    pub(super) fn drain_pending_reload(&mut self) {
        let recv = match &self.pending_reload {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match recv {
            Ok(Some(save)) => {
                tracing::trace!(rows = save.rows.len(), "reload-rows result installed");
                self.install_save_rows(save);
                // Only announce explicit refreshes (F5 sets "Refreshing…") —
                // background event/timer reloads shouldn't stomp the status line.
                if self.status == "Refreshing…" {
                    self.status = "Refreshed.".into();
                }
                self.pending_reload = None;
            }
            Ok(None) => {
                warn!("reload-rows thread produced no data");
                self.status = "Refresh failed (DB error).".into();
                self.pending_reload = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                warn!("reload-rows thread disconnected without sending");
                self.pending_reload = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        // A request arrived while the (now finished) reload was in flight: its
        // data may predate the triggering change, so run one more pass now.
        if self.pending_reload.is_none() && self.reload_queued {
            self.reload_queued = false;
            self.spawn_pending_reload();
        }
    }

    /// Install a completed save's rows into the UI, mirroring `reload_rows`.
    pub(super) fn install_save_rows(&mut self, save: SaveRows) {
        // Capture existing channel ids before replacing, to detect the new one.
        let old_channel_ids: HashSet<i64> = self.channels.iter().map(|c| c.id).collect();
        // A monitor whose latest-recording summary changed has new/updated rows
        // in its history — drop its cached recordings so expanded history rows
        // refresh on the next rebuild (they used to stay stale until F5).
        {
            let sig = |r: &MonitorWithChannel| {
                (
                    r.recording_count,
                    r.last_recording_started,
                    r.last_recording_ended,
                    r.last_recording_status.clone(),
                    r.last_recording_ad_count,
                    r.last_recording_meta_changes,
                )
            };
            let old: HashMap<i64, _> = self.rows.iter().map(|r| (r.monitor.id, sig(r))).collect();
            for r in &save.rows {
                if old.get(&r.monitor.id).is_some_and(|s| *s != sig(r)) {
                    self.rec_cache.remove(&r.monitor.id);
                }
            }
        }
        self.rows = save.rows;
        let by_mid: HashMap<i64, (i64, String)> = save
            .next_streams
            .into_iter()
            .map(|(mid, at, title)| (mid, (at, title)))
            .collect();
        for row in &mut self.rows {
            if let Some((at, title)) = by_mid.get(&row.monitor.id) {
                row.next_stream_at = Some(*at);
                row.next_stream_title = title.clone();
            }
        }
        self.channels = save.channels;
        self.yt_quota_today = save.yt_quota_today;
        self.yt_quota_cutoff = save.yt_quota_cutoff;
        self.yt_search_today = save.yt_search_today;
        self.yt_search_cutoff = save.yt_search_cutoff;
        // Scroll to the newly-added channel on the next render so it's visible
        // regardless of where it lands in the alphabetically-sorted list.
        if let Some(new_ch) = self.channels.iter().find(|c| !old_channel_ids.contains(&c.id)) {
            self.scroll_to_channel = Some(new_ch.id);
        }
        let live_channels: HashSet<i64> = self.channels.iter().map(|c| c.id).collect();
        let live_monitors: HashSet<i64> = self.rows.iter().map(|r| r.monitor.id).collect();
        self.rec_cache.retain(|k, _| live_monitors.contains(k));
        self.ad_break_cache.retain(|k, _| live_monitors.contains(k));
        self.meta_change_cache.retain(|k, _| live_monitors.contains(k));
        self.schedule_cache.retain(|k, _| live_monitors.contains(k));
        self.expanded_channels.retain(|id| live_channels.contains(id));
        self.expanded_instances.retain(|id| live_monitors.contains(id));
        self.expanded_streams
            .retain(|k| stream_key_monitor(k).is_some_and(|mid| live_monitors.contains(&mid)));
        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
        // A freshly-added instance: fetch its assets/About right away (icons,
        // banner, About page) instead of waiting for the hourly sweep. Live
        // state + title/game/viewers follow from the scheduler's next tick — a
        // new monitor is due immediately (no last_checked_at). Recording stays
        // manual, so no auto-Start here.
        if let Some(mid) = save.new_monitor_id {
            self.core.manual(crate::events::ManualCommand::RefetchAssets(mid));
        }
    }

    pub(super) fn save_settings(&mut self) {
        // Settings (e.g. the date format) feed the cached Streams-view model.
        self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
        let s = &self.settings;
        let postproc_readrate = format!("{}", s.postproc_readrate.clamp(0.0, 1000.0));
        // Discord import counts as on only when a token backs the toggle.
        let discord_on = s.discord_schedule && !s.discord_token.trim().is_empty();
        // Persist the browser + optional profile as one `browser:profile` value.
        let cookies_value = compose_browser_profile(&s.cookies_browser, &s.cookies_profile);
        let pairs = [
            (K_TWITCH_ID, s.twitch_client_id.trim()),
            (K_TWITCH_SECRET, s.twitch_client_secret.trim()),
            (google_oauth::K_CLIENT_ID, s.google_client_id.trim()),
            (google_oauth::K_CLIENT_SECRET, s.google_client_secret.trim()),
            (K_YT_KEY, s.youtube_api_key.trim()),
            (K_KICK_ID, s.kick_client_id.trim()),
            (K_KICK_SECRET, s.kick_client_secret.trim()),
            (K_DEFAULT_OUT, s.default_output_dir.trim()),
            (K_VIDEO_DEFAULT_OUT, s.default_video_output_dir.trim()),
            (K_MAX_CONCURRENT, s.max_concurrent_downloads.trim()),
            (crate::io_gate::K_DOWNLOAD_RATE_LIMIT, s.download_rate_limit.trim()),
            (crate::downloader::K_CACHE_ROOT, s.capture_cache_root.trim()),
            (crate::io_gate::K_YTDLP_PPA, s.ytdlp_ppa.trim()),
            (K_DOWNLOAD_AUTH, s.download_auth_method.trim()),
            (K_COOKIES_BROWSER, cookies_value.as_str()),
            (K_WEBSUB_URL, s.websub_vps_url.trim()),
            (K_WEBSUB_TOKEN, s.websub_token.trim()),
            (K_WEBSUB_POLL, s.websub_poll_secs.trim()),
            (K_FILENAME_MEDIA, s.filename_media_info.as_str()),
            (K_DATE_FORMAT, s.date_fmt.as_str()),
            (K_SHORT_TS_FMT, s.short_ts_fmt.trim()),
            (K_SCHEDULE_DEFAULT_VIEW, s.schedule_default_view.as_str()),
            (K_YT_API_DETECT, if s.youtube_api_detect { "1" } else { "0" }),
            (K_YT_API_SCHEDULE, if s.youtube_api_schedule { "1" } else { "0" }),
            (K_YT_API_QUOTA_CUTOFF, s.youtube_api_quota_cutoff.trim()),
            (K_YT_SEARCH_QUOTA_CUTOFF, s.youtube_search_quota_cutoff.trim()),
            (K_YTDLP_ARGS, s.ytdlp_default_args.trim()),
            (K_YTDLP_BINARY, s.ytdlp_binary_path.trim()),
            (K_SABR_BINARY, s.sabr_binary_path.trim()),
            (K_SABR_ENABLED, if s.sabr_enabled { "1" } else { "0" }),
            (K_SABR_FORMAT, s.sabr_format.trim()),
            (K_SABR_EXTRACTOR_ARGS, s.sabr_extractor_args.trim()),
            (K_SABR_DEEP_REWIND, if s.sabr_deep_rewind { "1" } else { "0" }),
            (K_SABR_RAW_ARGS, s.sabr_raw_args.trim()),
            (K_SABR_POT_ARGS, s.sabr_pot_args.trim()),
            (K_SABR_CODEC_PREF, s.sabr_codec_pref.id()),
            (K_SABR_CODEC_CUSTOM, s.sabr_codec_custom.trim()),
            (K_DASH_FORMAT, s.dash_format.trim()),
            (
                crate::pot_server::K_POT_SERVER_AUTOSTART,
                if s.pot_server_autostart { "1" } else { "0" },
            ),
            (crate::pot_server::K_POT_SERVER_DIR, s.pot_server_dir.trim()),
            (crate::pot_server::K_POT_SERVER_NODE, s.pot_server_node.trim()),
            (K_DISCORD_TOKEN, s.discord_token.trim()),
            // Only persist the import as on when a token actually backs it, so the
            // consent flag can't be left latched with no token.
            (
                K_DISCORD_SCHEDULE,
                if discord_on { "1" } else { "0" },
            ),
            (K_OCR_COMMAND, s.ocr_command.trim()),
            (K_OCR_MODEL, s.ocr_model.trim()),
            (K_OCR_FALLBACK_MODEL, s.ocr_fallback_model.trim()),
            (K_OCR_TIMEZONE, s.ocr_timezone.trim()),
            (K_OCR_OFFSET, s.ocr_offset.trim()),
            (K_OCR_MAX_BUDGET, s.ocr_max_budget.trim()),
            (K_OCR_TIMEOUT_SECS, s.ocr_timeout_secs.trim()),
            (K_OCR_EFFORT, s.ocr_effort.trim()),
            (
                K_SCHEDULE_TITLE_FILL,
                if s.schedule_title_fill { "1" } else { "0" },
            ),
            (
                K_YT_COMMUNITY_MAX_POSTS,
                s.youtube_community_max_posts.trim(),
            ),
            (K_DIALOG_ICON, s.dialog_icon.trim()),
            (K_REMUX_EMBED_THUMBNAIL, if s.remux_embed_thumbnail { "1" } else { "0" }),
            (K_REMUX_EMBED_TITLE,     if s.remux_embed_title     { "1" } else { "0" }),
            (K_REMUX_TITLE_TEMPLATE, s.remux_title_template.trim()),
            (K_REMUX_EMBED_SUBS,      if s.remux_embed_subs      { "1" } else { "0" }),
            (crate::io_gate::K_POSTPROC_READRATE, postproc_readrate.as_str()),
            (crate::iomon::K_IOMON_LOG, if s.iomon_sample_log { "1" } else { "0" }),
            (K_FILE_SPLIT_ENABLED,  if s.file_split_enabled { "1" } else { "0" }),
            (K_FILE_SPLIT_VIDEOS, s.file_split_videos.trim()),
            (K_FILE_SPLIT_SUBS,   s.file_split_subs.trim()),
            (K_FILE_SPLIT_CHAT,   s.file_split_chat.trim()),
            (K_FILE_SPLIT_THUMBS, s.file_split_thumbs.trim()),
            (K_FILE_SPLIT_LOGS,   s.file_split_logs.trim()),
            (K_MEDIA_PLAYER, s.media_player_path.trim()),
            (crate::downloader::K_TOKEN_STYLE, if s.token_style_branded { "branded" } else { "plain" }),
            (crate::downloader::K_TOKEN_OVERRIDES, s.token_overrides.trim()),
            (crate::downloader::K_GAP_RECOVER, if s.gap_recover { "1" } else { "0" }),
            (crate::downloader::K_GAP_SPLICE, if s.gap_splice { "1" } else { "0" }),
            (crate::disposal::K_GAP_SPLICE_CLEANUP, s.gap_splice_cleanup.as_str()),
            (crate::recovery::K_AUTO_RECOVER_MUTED, if s.auto_recover_muted { "1" } else { "0" }),
            (crate::recovery::K_AUTO_RECOVER_DELETED, if s.auto_recover_deleted { "1" } else { "0" }),
            (crate::recovery::K_RECOVERY_CDN_HOSTS, s.recovery_cdn_hosts.trim()),
            (crate::recovery::K_RECOVERY_QUALITY, s.recovery_quality.trim()),
            (crate::recovery::K_RECOVERY_MAX_CONC, s.recovery_max_conc.trim()),
            (crate::downloader::K_AD_PROBE, if s.ad_probe { "1" } else { "0" }),
            (crate::vod_archive::K_VOD_DL_ENABLED, if s.vod_dl_enabled { "1" } else { "0" }),
            (crate::vod_archive::K_VOD_DL_REPLACE, if s.vod_dl_replace { "1" } else { "0" }),
            (crate::head_backfill::K_HEAD_BACKFILL_FETCH, if s.head_backfill_fetch_new_take { "1" } else { "0" }),
            (crate::downloader::K_QUALITY_UPGRADE, if s.quality_upgrade_restart { "1" } else { "0" }),
            (crate::head_backfill::K_HEAD_BACKFILL_REPLACE, if s.head_backfill_replace_old { "1" } else { "0" }),
            (crate::disposal::K_JOIN_CLEANUP, s.join_cleanup.as_str()),
            (crate::disposal::K_DISPOSAL_METHOD, s.disposal_method.as_str()),
            (crate::disposal::K_TRASH_DIRS, s.disposal_trash_dirs.trim()),
        ];
        for (k, v) in pairs {
            if let Err(e) = self.core.store.set_setting(k, v) {
                self.status = format!("Error saving settings: {e}");
                return;
            }
        }
        // Trigger rules serialize to JSON, so they can't ride the &str pairs.
        if let Err(e) =
            crate::triggers::save_global_rules(&self.core.store, &self.settings.trigger_rules)
        {
            self.status = format!("Error saving trigger rules: {e}");
            return;
        }
        if let Err(e) = crate::triggers::save_global_block_rules(
            &self.core.store,
            &self.settings.trigger_block_rules,
        ) {
            self.status = format!("Error saving blacklist triggers: {e}");
            return;
        }
        // Custom tools serialize to JSON too.
        if let Err(e) =
            crate::downloader::save_custom_tools(&self.core.store, &self.settings.custom_tools)
        {
            self.status = format!("Error saving custom tools: {e}");
            return;
        }
        // If Discord import is now off (toggled off or token cleared), purge any
        // previously-imported Discord events so they don't linger on the calendar.
        if !discord_on {
            let _ = self.core.store.clear_schedule_source("discord");
            self.spawn_reload_schedule();
        }
        // Apply the (possibly changed) date format + short-timestamp pattern to the live UI.
        set_active_date_fmt(self.settings.date_fmt);
        set_short_ts_pattern(&self.settings.short_ts_fmt);
        // Re-install the filename token style so new plans/renames/previews
        // pick the change up without a restart.
        crate::downloader::set_global_token_style(crate::downloader::load_token_style(
            &self.core.store,
        ));
        // Per-disk I/O limits: the global throttle/rate-limit controls are the
        // defaults; the per-drive overrides come from the table. Permit
        // changes take effect immediately on save, including for a dynamic-
        // mode drive's ceiling (the live count itself stays under the
        // adjuster's control — see io_gate::resize_existing_gates).
        let disk_cfg = crate::io_gate::DiskLimitsConfig {
            default: crate::io_gate::DiskLimits {
                local_permits: self.settings.disk_default_local.max(1),
                cdn_permits: self.settings.disk_default_cdn.max(1),
                readrate: self.settings.postproc_readrate,
                rate_limit: self.settings.download_rate_limit.trim().to_string(),
                dynamic: self.settings.disk_default_dynamic,
                paused: self.settings.disk_default_paused,
            },
            drives: self
                .settings
                .disk_overrides
                .iter()
                .filter_map(|(letter, lim)| {
                    let l = letter.trim().to_uppercase();
                    (l.len() == 1 && l.chars().all(|c| c.is_ascii_alphabetic()))
                        .then(|| (l, lim.clone()))
                })
                .collect(),
        };
        match serde_json::to_string(&disk_cfg) {
            Ok(json) => {
                if let Err(e) = self.core.store.set_setting(crate::io_gate::K_DISK_LIMITS, &json) {
                    self.status = format!("Error saving disk I/O limits: {e}");
                    return;
                }
            }
            Err(e) => {
                self.status = format!("Error saving disk I/O limits: {e}");
                return;
            }
        }
        crate::io_gate::set_disk_limits(disk_cfg);
        crate::downloader::set_cache_root(&self.settings.capture_cache_root);
        crate::io_gate::set_ytdlp_ppa(&self.settings.ytdlp_ppa);
        // PO token server: re-read path/autostart/base-url and wake the
        // watchdog so a corrected config takes effect without a restart.
        crate::pot_server::apply_settings(&self.core.store);
        // I/O monitor: apply the sample-log toggle and re-register the
        // recordings roots (the default output dir may have changed).
        crate::iomon::set_sample_logging(self.settings.iomon_sample_log);
        refresh_iomon_roots(
            &self.core.store,
            &self.settings.default_output_dir,
            &self.settings.default_video_output_dir,
        );
        self.persist_monitor_defaults();
        self.status = "Settings saved.".into();
    }
    /// Single entry point for changing the active view — every switching path
    /// (top-bar tabs, the Views/Help menus, the » overflow menu, keyboard
    /// shortcuts, in-view jump links) goes through here so the per-view
    /// on-open side effects always run. Re-selecting the already-active view
    /// re-runs them too, which doubles as a manual refresh (matches the old
    /// tab `clicked()` behavior).
    pub(super) fn switch_view(&mut self, v: View) {
        self.view = v;
        match v {
            View::Posts => self.posts_refreshed = None, // force reload on open
            View::Files => self.files_scan = None,      // force rescan on open
            View::ChannelStats => {
                self.chstats_data = None; // force reload on open
                self.stats_collabs =
                    self.core.store.collab_partner_overview().unwrap_or_default();
            }
            View::Stats => {
                self.stats_snapshot = None; // force reload on open
                self.stats_history = None;
            }
            _ => {}
        }
    }

    /// Process global keyboard shortcuts once per frame, before drawing.
    ///
    /// While a modal (add/edit form or delete confirmation) is open, only `Esc`
    /// is handled — it dismisses the modal — and other shortcuts are suppressed.
    pub(super) fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        const ADD: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
        const SETTINGS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma);
        const REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F5);
        // Schedule-view zoom. Two bindings for zoom-in: `=` is the unshifted key
        // most keyboards use for zoom (matches browser/editor convention), `+`
        // covers keyboards/layouts that report the shifted key directly.
        const SCHED_ZOOM_IN_EQ: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Equals);
        const SCHED_ZOOM_IN_PLUS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Plus);
        const SCHED_ZOOM_OUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Minus);
        const SCHED_ZOOM_RESET: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Num0);

        // Widget inspector toggle — handled before the modal early-return so
        // it works even while a dialog is open.
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F12)) {
            self.show_inspector = !self.show_inspector;
        }

        // A modal is open: Esc closes it, everything else is swallowed.
        if self.form.is_some()
            || self.channel_form.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_delete_channel.is_some()
            || self.confirm_delete_segment.is_some()
        {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.form = None;
                self.channel_form = None;
                self.confirm_delete = None;
                self.confirm_delete_channel = None;
                self.confirm_delete_segment = None;
            }
            return;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&ADD)) {
            self.switch_view(View::Streams);
            self.form = Some(MonitorForm::new_channel(
                &self.monitor_defaults,
                &self.settings.default_output_dir,
            ));
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SETTINGS)) {
            self.switch_view(View::Settings);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&REFRESH)) {
            // Drop all per-monitor recording/ad/meta/schedule caches so the next
            // render re-reads them from DB. Normally these are preserved across
            // reload_rows to avoid per-frame DB queries, but F5 is an explicit
            // user request to see current state (e.g. after an external DB edit).
            self.rec_cache.clear();
            self.ad_break_cache.clear();
            self.meta_change_cache.clear();
            self.schedule_cache.clear();
            self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
            self.spawn_pending_reload();
            if self.view == View::Schedule {
                // Force a network re-fetch (not just a DB reload) + show current data.
                self.core.request_schedule_refresh();
                self.spawn_reload_schedule();
            }
            self.status = "Refreshing…".into();
        }

        // Schedule-view zoom (calendar body font/element size).
        if self.view == View::Schedule {
            let zoom_in = ctx.input_mut(|i| {
                i.consume_shortcut(&SCHED_ZOOM_IN_EQ) || i.consume_shortcut(&SCHED_ZOOM_IN_PLUS)
            });
            if zoom_in {
                self.schedule_zoom = (self.schedule_zoom + SCHEDULE_ZOOM_STEP).min(SCHEDULE_ZOOM_MAX);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&SCHED_ZOOM_OUT)) {
                self.schedule_zoom = (self.schedule_zoom - SCHEDULE_ZOOM_STEP).max(SCHEDULE_ZOOM_MIN);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&SCHED_ZOOM_RESET)) {
                self.schedule_zoom = 1.0;
            }
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
                        let mut mf = MonitorForm::from_existing(&self.rows[idx]);
                        let sc = crate::vod_archive::load_monitor_vod_scope(&self.core.store, self.rows[idx].monitor.id);
                        mf.vod_download = sc.download;
                        mf.vod_replace = sc.replace;
                        let hbsc = crate::head_backfill::load_monitor_head_backfill_scope(&self.core.store, self.rows[idx].monitor.id);
                        mf.head_backfill_fetch = hbsc.fetch;
                        mf.head_backfill_replace = hbsc.replace;
                        let dsc = crate::disposal::load_monitor_disposal_scope(&self.core.store, self.rows[idx].monitor.id);
                        mf.join_cleanup = dsc.join_cleanup;
                        mf.disposal_method = dsc.method;
                        mf.primary_pin = crate::platform_pref::monitor_is_pinned(&self.core.store, self.rows[idx].monitor.id);
                        self.form = Some(mf);
                    }
                }
            }
        }
    }
}
