//! Settings view and shared trigger/custom-tool editors.

use super::*;

/// Settings category tabs — the flat Settings page is grouped into these.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsTab {
    Accounts,
    Recording,
    Downloads,
    Schedule,
    Interface,
    System,
    Maintenance,
}

impl SettingsTab {
    const ALL: [SettingsTab; 7] = [
        SettingsTab::Accounts,
        SettingsTab::Recording,
        SettingsTab::Downloads,
        SettingsTab::Schedule,
        SettingsTab::Interface,
        SettingsTab::System,
        SettingsTab::Maintenance,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            SettingsTab::Accounts => "Accounts",
            SettingsTab::Recording => "Recording",
            SettingsTab::Downloads => "Downloads",
            SettingsTab::Schedule => "Schedule",
            SettingsTab::Interface => "Interface",
            SettingsTab::System => "System",
            SettingsTab::Maintenance => "Maintenance",
        }
    }

    /// Stable persisted id (the `K_SETTINGS_TAB` setting value).
    pub(super) fn id(self) -> &'static str {
        match self {
            SettingsTab::Accounts => "accounts",
            SettingsTab::Recording => "recording",
            SettingsTab::Downloads => "downloads",
            SettingsTab::Schedule => "schedule",
            SettingsTab::Interface => "interface",
            SettingsTab::System => "system",
            SettingsTab::Maintenance => "maintenance",
        }
    }

    pub(super) fn from_id(s: &str) -> SettingsTab {
        SettingsTab::ALL
            .into_iter()
            .find(|t| t.id() == s)
            .unwrap_or(SettingsTab::Accounts)
    }
}
/// Per-channel / per-instance schedule-source scope override editor: an
/// Inherit-vs-Custom source-order toggle (with an inline reorderable list when
/// Custom) plus a tri-state title-fill override (Inherit / On / Off). `global_order`
/// seeds a freshly-switched-on custom list. Returns true if `scope` changed.
pub(super) fn scope_override_editor(
    ui: &mut egui::Ui,
    scope: &mut crate::schedule_source::SourceScopeConfig,
    global_order: &[SourceEntry],
) -> bool {
    let mut changed = false;

    ui.label("Source order");
    let custom = scope.order.is_some();
    ui.horizontal(|ui| {
        if ui
            .radio(!custom, "Inherit global")
            .on_hover_text("Use the global source order from Settings → Schedule sources.")
            .clicked()
            && custom
        {
            scope.order = None;
            changed = true;
        }
        if ui
            .radio(custom, "Custom")
            .on_hover_text("Override the source order/enabled set just for this scope.")
            .clicked()
            && !custom
        {
            // Seed the custom list from the current global order.
            scope.order = Some(global_order.to_vec());
            changed = true;
        }
    });
    if let Some(order) = scope.order.as_mut() {
        if source_list_inline_editor(ui, order) {
            changed = true;
        }
    }

    ui.add_space(4.0);
    ui.label("Fill blank titles from next source");
    ui.horizontal(|ui| {
        if ui.radio(scope.title_fill.is_none(), "Inherit").clicked() && scope.title_fill.is_some() {
            scope.title_fill = None;
            changed = true;
        }
        if ui.radio(scope.title_fill == Some(true), "On").clicked()
            && scope.title_fill != Some(true)
        {
            scope.title_fill = Some(true);
            changed = true;
        }
        if ui.radio(scope.title_fill == Some(false), "Off").clicked()
            && scope.title_fill != Some(false)
        {
            scope.title_fill = Some(false);
            changed = true;
        }
    });

    changed
}

/// Editor for a list of trigger-word rules — one row per rule: enabled toggle,
/// field selector (Any/Title/Game), match type (Contains/Regex), the pattern
/// (validated live when regex), a per-rule "capture from start" override, and
/// remove. Returns true when anything changed (detected by value comparison so
/// combo selections and add/remove all count).
///
/// `with_actions: false` = blacklist mode: the per-rule start-action controls
/// (From start / Lead / Only while matching) are hidden — a veto has no
/// recording to act on, and the fields are ignored at match time.
pub(super) fn trigger_rules_editor(
    ui: &mut egui::Ui,
    rules: &mut Vec<crate::triggers::TriggerRule>,
    salt: &str,
    with_actions: bool,
) -> bool {
    use crate::triggers::{TriggerField, TriggerRule, pattern_error};
    let before = rules.clone();
    let mut remove: Option<usize> = None;
    for i in 0..rules.len() {
        let r = &mut rules[i];
        ui.horizontal(|ui| {
            ui.checkbox(&mut r.enabled, "").on_hover_text("Rule enabled");
            egui::ComboBox::from_id_salt((salt, "field", i))
                .selected_text(r.field.label())
                .width(86.0)
                .show_ui(ui, |ui| {
                    for f in TriggerField::ALL {
                        ui.selectable_value(&mut r.field, f, f.label());
                    }
                });
            egui::ComboBox::from_id_salt((salt, "match", i))
                .selected_text(if r.regex { "Regex" } else { "Contains" })
                .width(86.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut r.regex, false, "Contains")
                        .on_hover_text("Case-insensitive substring match.");
                    ui.selectable_value(&mut r.regex, true, "Regex")
                        .on_hover_text("Case-insensitive regular expression.");
                });
            let err = pattern_error(r);
            let mut edit = egui::TextEdit::singleline(&mut r.pattern)
                .hint_text(if r.regex { "unarchi(v|ve)d" } else { "karaoke" })
                .desired_width(150.0);
            if err.is_some() {
                edit = edit.text_color(HL_ERROR_TEXT);
            }
            let resp = ui.add(edit);
            match &err {
                Some(e) => {
                    resp.on_hover_text(format!("Invalid regex — this rule never matches:\n{e}"));
                }
                None => {
                    resp.on_hover_text(if r.regex {
                        "Case-insensitive regex (start the pattern with (?-i) for case-sensitive)."
                    } else {
                        "Case-insensitive substring — phrases like \"no vod\" match as a whole."
                    });
                }
            }
            if with_actions {
                ui.label("From start:").on_hover_text(
                    "Force the 'capture from start' flag for the recording this rule starts \
                     (unarchived streams usually warrant it). Inherit = the instance's own setting.",
                );
                tristate_combo(ui, &format!("{salt}_cfs_{i}"), &mut r.capture_from_start);
            }
            if ui.small_button("🗑").on_hover_text("Remove this rule").clicked() {
                remove = Some(i);
            }
        });
        if !with_actions {
            continue;
        }
        ui.horizontal(|ui| {
            ui.add_space(24.0); // roughly align under the row above
            ui.label("Lead:");
            ui.push_id((salt, "lead", i), |ui| {
                ui.add(egui::DragValue::new(&mut r.lead_secs).range(0..=600).suffix("s"))
            })
            .inner
            .on_hover_text(
                "Backfill this many seconds from the Twitch live VOD from before \
                 the match was detected, in case the title/game update landed a \
                 little late relative to when the segment actually started. \
                 0 = off. Reuses the head-backfill mechanism, so Twitch only.",
            );
            ui.checkbox(&mut r.stop_on_unmatch, "Only while matching").on_hover_text(
                "Stop this recording once the rule no longer matches, instead of \
                 recording until the stream ends — e.g. archiving just one game \
                 segment of a multi-day marathon. Checked on ~60s poll cycles, so \
                 small End delay values effectively round up to the next check.",
            );
            if r.stop_on_unmatch {
                ui.label("End delay:");
                ui.push_id((salt, "end_delay", i), |ui| {
                    ui.add(egui::DragValue::new(&mut r.end_delay_secs).range(0..=3600).suffix("s"))
                })
                .inner
                .on_hover_text(
                    "Keep recording this many seconds after the rule stops matching \
                     — a grace period in case the title/game flips back, or the \
                     update landed a little early. 0 = stop as soon as an unmatch \
                     is confirmed.",
                );
            }
        });
    }
    if let Some(i) = remove {
        rules.remove(i);
    }
    if ui.button("➕ Add trigger").clicked() {
        rules.push(TriggerRule::default());
    }
    *rules != before
}

/// An alias problem for the custom-tool row at `i` — empty, the reserved
/// `"sabr"` word, or a duplicate of another row's alias — shown inline so the
/// Videos-tab dropdown never has to disambiguate two identically-named tools.
pub(super) fn custom_tool_alias_error(tools: &[crate::downloader::CustomTool], i: usize) -> Option<&'static str> {
    let alias = tools[i].alias.trim();
    if alias.is_empty() {
        return Some("Alias can't be empty");
    }
    if alias.eq_ignore_ascii_case(crate::downloader::TOOL_BINARY_SABR) {
        return Some("\"sabr\" is reserved for the built-in SABR build");
    }
    if tools
        .iter()
        .enumerate()
        .any(|(j, t)| j != i && t.alias.trim().eq_ignore_ascii_case(alias))
    {
        return Some("Another custom tool already uses this alias");
    }
    None
}

/// Editor for the user-defined custom yt-dlp-compatible binaries (Settings →
/// Downloads). Each row is offered in the Videos-tab download form's Tool
/// dropdown alongside the system yt-dlp and the built-in SABR build. Returns
/// true on any change so the caller can persist immediately.
pub(super) fn custom_tools_editor(
    ui: &mut egui::Ui,
    tools: &mut Vec<crate::downloader::CustomTool>,
    pending_browse: &mut Option<PendingBrowse>,
) -> bool {
    let before = tools.clone();
    let mut remove: Option<usize> = None;
    for i in 0..tools.len() {
        let err = custom_tool_alias_error(tools, i);
        let t = &mut tools[i];
        ui.horizontal(|ui| {
            let mut alias_edit =
                egui::TextEdit::singleline(&mut t.alias).hint_text("alias").desired_width(120.0);
            if err.is_some() {
                alias_edit = alias_edit.text_color(HL_ERROR_TEXT);
            }
            let resp = ui.add(alias_edit);
            if let Some(e) = &err {
                resp.on_hover_text(*e);
            }
            ui.add(
                egui::TextEdit::singleline(&mut t.path)
                    .hint_text(r"e.g. C:\tools\my-yt-dlp\yt-dlp.exe")
                    .desired_width(340.0),
            );
            if ui.button("Browse…").clicked() {
                *pending_browse = Some(spawn_browse_file(&t.path, move |app, p| {
                    if let Some(row) = app.settings.custom_tools.get_mut(i) {
                        row.path = p;
                    }
                }));
            }
            if ui.small_button("🗑").on_hover_text("Remove this tool").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        tools.remove(i);
    }
    if ui.button("➕ Add custom tool").clicked() {
        tools.push(crate::downloader::CustomTool::default());
    }
    *tools != before
}

/// Inherit/Extend/Replace/Off editor for a channel- or instance-level trigger
/// scope (same structural idiom as [`scope_override_editor`]). Returns true on
/// any change so the caller can persist immediately.
pub(super) fn trigger_scope_editor(
    ui: &mut egui::Ui,
    scope: &mut crate::triggers::TriggerScope,
    salt: &str,
    with_actions: bool,
) -> bool {
    use crate::triggers::TriggerMode;
    let before = scope.clone();
    ui.horizontal(|ui| {
        for (mode, label, tip) in [
            (TriggerMode::Inherit, "Inherit", "Use the inherited trigger rules unchanged."),
            (TriggerMode::Extend, "Extend", "Inherited rules PLUS the extra rules below."),
            (
                TriggerMode::Replace,
                "Replace",
                "Ignore inherited rules; only the rules below apply here.",
            ),
            (
                TriggerMode::Off,
                "Off",
                "No trigger words here at all — inherited rules included.",
            ),
        ] {
            ui.radio_value(&mut scope.mode, mode, label).on_hover_text(tip);
        }
    });
    if matches!(scope.mode, TriggerMode::Extend | TriggerMode::Replace) {
        ui.add_space(2.0);
        trigger_rules_editor(ui, &mut scope.rules, salt, with_actions);
    }
    *scope != before
}

impl StreamArchiverApp {
    /// Whether a settings section should render: when the search box is empty, only
    /// the selected category tab's sections show; when searching, any section whose
    /// title or keywords match the query shows (across all categories).
    pub(super) fn section_shown(&self, tab: SettingsTab, title: &str, keywords: &[&str]) -> bool {
        // Runs per section per frame — the lowercased query is maintained on
        // edit (`settings_search_lc`), never recomputed here.
        let q = &self.settings_search_lc;
        if q.is_empty() {
            return self.settings_tab == tab;
        }
        title.to_lowercase().contains(q.as_str()) || keywords.iter().any(|k| k.contains(q.as_str()))
    }

    pub(super) fn settings_view(&mut self, ui: &mut egui::Ui) {
        // Fixed header (search + category tabs + always-visible Save) above the scroll.
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("🔎");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.settings_search)
                        .hint_text("Search settings…")
                        .desired_width(200.0),
                )
                .changed()
            {
                self.settings_search_lc = self.settings_search.trim().to_lowercase();
            }
            if !self.settings_search.is_empty() && ui.small_button("✕").clicked() {
                self.settings_search.clear();
                self.settings_search_lc.clear();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("💾 Save settings")
                    .inspect("Settings: Save button", &[])
                    .clicked()
                {
                    self.save_settings();
                }
            });
        });
        if self.settings_search.trim().is_empty() {
            ui.horizontal(|ui| {
                for tab in SettingsTab::ALL {
                    if ui
                        .selectable_value(&mut self.settings_tab, tab, tab.label())
                        .clicked()
                    {
                        let _ = self.core.store.set_setting(K_SETTINGS_TAB, tab.id());
                    }
                }
            });
        }
        ui.separator();
        // Each section below is gated by `section_shown(category, …)`: only the
        // active tab's sections render (or search matches). Inner code keeps its
        // original indentation to avoid a whole-file reflow.
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.settings_detection_credentials_section(ui);
            self.settings_youtube_data_api_section(ui);
            self.settings_discord_import_section(ui);
            self.settings_schedule_sources_section(ui);
            self.settings_twitch_account_section(ui);
            self.settings_google_account_section(ui);
            self.settings_websub_section(ui);
            self.settings_defaults_section(ui);
            self.settings_display_section(ui);
            self.settings_table_columns_section(ui);
            self.settings_download_auth_section(ui);
            self.settings_ytdlp_args_section(ui);
            self.settings_sabr_section(ui);
            self.settings_custom_tools_section(ui);
            self.settings_monitor_defaults_section(ui);
            self.settings_startup_section(ui);
            self.settings_notifications_section(ui);
            self.settings_shutdown_section(ui);
            self.settings_remux_section(ui);
            self.settings_file_management_section(ui);
            self.settings_vod_download_section(ui);
            self.settings_head_backfill_section(ui);
            self.settings_trigger_words_section(ui);
            self.settings_blacklist_triggers_section(ui);
            self.settings_vod_recovery_section(ui);
            self.settings_maintenance_section(ui);
            self.settings_diagnostics_section(ui);

            ui.add_space(16.0);
        });
    }

    fn settings_detection_credentials_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Accounts, "Detection credentials", &["twitch", "youtube", "kick", "client id", "secret", "api key", "credentials", "detection"]) {
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

            }
    }

    fn settings_youtube_data_api_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Accounts, "YouTube Data API usage", &["youtube", "data api", "quota", "search"]) {
            ui.add_space(12.0);
            ui.heading("YouTube Data API usage");
            let key_set = !self.settings.youtube_api_key.trim().is_empty();
            ui.label(
                "By default these YouTube features scrape public pages (free, but can break \
                 when YouTube changes them). With the YouTube API Key set above you can use the \
                 Data API instead for more reliable results — but each call spends quota (the \
                 free daily quota is ~10,000 units).",
            );
            if !key_set {
                ui.colored_label(
                    egui::Color32::from_rgb(0xe0, 0xb0, 0x6c),
                    "⚠ Set a YouTube API Key above to enable these.",
                );
            }
            ui.add_enabled_ui(key_set, |ui| {
                ui.checkbox(
                    &mut self.settings.youtube_api_detect,
                    "Live detection (instead of scraping /live)",
                )
                .on_hover_text(
                    "Use search.list for liveness on YouTube monitors whose detection method is \
                     'Scrape'. ~100 quota units per check — with a long poll interval. (Monitors \
                     already set to the 'YouTube Data API' method use it regardless.)",
                );
                ui.checkbox(
                    &mut self.settings.youtube_api_schedule,
                    "Upcoming schedule — exact times via videos.list",
                )
                .on_hover_text(
                    "Scraping /streams parses human-readable text so times are approximate. \
                     With this enabled, scheduled stream video IDs are collected during scraping \
                     and batched into a single videos.list call (~1 quota unit for ALL channels \
                     combined) to get exact scheduled start times from the API.",
                );
                if self.settings.youtube_api_schedule {
                    if ui
                        .button("Re-fetch missing video IDs")
                        .on_hover_text(
                            "Re-scrape YouTube channels whose schedule entries are missing video \
                             IDs (needed for exact times). Only fetches channels with gaps — \
                             others keep their cached schedules.",
                        )
                        .clicked()
                    {
                        self.core.request_yt_video_id_refetch();
                    }
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Daily quota limit (units)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.youtube_api_quota_cutoff)
                            .desired_width(80.0)
                            .hint_text("9000"),
                    )
                    .on_hover_text(
                        "Stop making YouTube Data API calls today once this many units are spent. \
                         The free tier allows 10,000 units/day; leaving a buffer prevents outages \
                         from unexpected bursts. Default: 9000.",
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Search query warning cutoff");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.youtube_search_quota_cutoff)
                            .desired_width(80.0)
                            .hint_text("90"),
                    )
                    .on_hover_text(
                        "Show a warning in Issues when today's search.list call count reaches this \
                         value. The free tier allows 100 search queries/day. Default: 90.",
                    );
                });
            });

            }
    }

    fn settings_discord_import_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Schedule, "Discord schedule import", &["discord", "schedule", "import", "token", "events"]) {
            ui.add_space(12.0);
            ui.heading("Discord schedule import");
            ui.label(
                "Import upcoming streams from Discord scheduled events in the servers you're in. \
                 Events whose location/description contains a monitored channel's stream URL are \
                 attached to it — useful for streamers who post their schedule on Discord but \
                 don't publish a Twitch/YouTube one.",
            );
            ui.colored_label(
                egui::Color32::from_rgb(0xe0, 0x6c, 0x6c),
                "⚠ This uses your personal Discord token. Automating a user token is against \
                 Discord's Terms of Service and could get your account banned — use at your own risk.",
            );
            ui.horizontal(|ui| {
                ui.label("Discord user token");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.discord_token)
                        .password(true)
                        .desired_width(280.0),
                );
            });
            let token_set = !self.settings.discord_token.trim().is_empty();
            if !token_set {
                ui.colored_label(
                    egui::Color32::from_rgb(0xe0, 0xb0, 0x6c),
                    "⚠ Paste your Discord token above to enable import.",
                );
            }
            ui.add_enabled_ui(token_set, |ui| {
                ui.checkbox(
                    &mut self.settings.discord_schedule,
                    "Import schedules from Discord events",
                )
                .on_hover_text(
                    "Sweeps your Discord servers a few hours apart (and on a manual reload), \
                     matching scheduled events to your monitors by stream URL. Discord events are \
                     only used for channels without a published Twitch/YouTube schedule.",
                );
            });

            }
    }

    fn settings_schedule_sources_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Schedule, "Schedule sources", &["schedule", "sources", "ocr", "banner", "twitter", "priority"]) {
            ui.add_space(12.0);
            ui.heading("Schedule sources");
            ui.label(
                "Schedules are fetched from several sources, tried in priority order per channel \
                 until one resolves. Some sources read the week off an image (a Twitch offline \
                 banner, a YouTube community post, a pinned tweet) via OCR — done by shelling out \
                 to an LLM CLI (the `claude` CLI by default; no API key needed).",
            );
            if ui
                .button("Configure source order…")
                .on_hover_text(
                    "Choose which sources to use and their priority. The first source that \
                     resolves an actual schedule for a channel wins.",
                )
                .clicked()
            {
                self.open_schedule_sources();
            }

            ui.add_space(6.0);
            ui.checkbox(
                &mut self.settings.schedule_title_fill,
                "Go to next schedule source when no title found",
            )
            .on_hover_text(
                "After a source resolves a schedule, if any of its events have a time but no \
                 title, keep querying the lower-priority sources to borrow titles (matched to \
                 the nearest event within ±2h). Useful when a Twitch schedule publishes times \
                 but no titles and a banner / community-post OCR source has them. Override per \
                 channel or per instance in Properties.",
            );
            ui.horizontal(|ui| {
                ui.label("YouTube community post backlog");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.youtube_community_max_posts)
                        .hint_text("5")
                        .desired_width(60.0),
                )
                .on_hover_text(
                    "How many recent YouTube community posts to scan for a schedule image \
                     (some channels post the week several posts back). Empty = 5. Clamped to \
                     1–20. Override per channel in Properties.",
                );
            });

            ui.add_space(6.0);
            ui.label("Image OCR (for banner / community / tweet sources)");
            egui::Grid::new("ocr_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("OCR CLI command");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ocr_command)
                            .hint_text("claude")
                            .desired_width(220.0),
                    )
                    .on_hover_text(
                        "Executable to shell out to for image OCR. Must be on PATH (or an absolute \
                         path) and accept `--model <m> --add-dir <dir> -p <prompt>`. Default: claude.",
                    );
                    ui.end_row();
                    ui.label("Model");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ocr_model)
                            .hint_text("haiku")
                            .desired_width(220.0),
                    )
                    .on_hover_text("Primary model passed to the CLI. Default: haiku.");
                    ui.end_row();
                    ui.label("Fallback model");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ocr_fallback_model)
                            .hint_text("sonnet")
                            .desired_width(220.0),
                    )
                    .on_hover_text(
                        "Stronger model retried once if the primary returns invalid JSON. \
                         Default: sonnet.",
                    );
                    ui.end_row();
                    ui.label("Primary timezone");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ocr_timezone)
                            .hint_text("(machine local)")
                            .desired_width(220.0),
                    )
                    .on_hover_text(
                        "The primary IANA timezone a schedule's day/date headers are written in \
                         (e.g. America/Los_Angeles). When an image lists several timezones for one \
                         stream, this anchors the date and is preferred for the conversion. Leave \
                         empty to use the machine's local timezone. Override per channel in \
                         Properties.",
                    );
                    ui.end_row();
                    ui.label("UTC offset");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ocr_offset)
                            .hint_text("(machine local)")
                            .desired_width(220.0),
                    )
                    .on_hover_text(
                        "UTC offset matching the timezone/season, e.g. +02:00. Leave empty to use \
                         the machine's current local offset.",
                    );
                    ui.end_row();
                    ui.label("Max budget (USD/call)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ocr_max_budget)
                            .hint_text("(no limit)")
                            .desired_width(120.0),
                    )
                    .on_hover_text(
                        "Hard cost cap per claude CLI call via --max-budget-usd (e.g. 0.05). \
                         The call is aborted and counted as a failure if the budget is hit. \
                         Leave empty for no cap.",
                    );
                    ui.end_row();
                    ui.label("Timeout (seconds)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ocr_timeout_secs)
                            .hint_text("150")
                            .desired_width(80.0),
                    )
                    .on_hover_text(
                        "Maximum seconds to wait for one claude CLI call before killing it and \
                         counting it as a failure. Default: 150 s.",
                    );
                    ui.end_row();
                    ui.label("Effort level");
                    egui::ComboBox::from_id_salt("ocr_effort_combo")
                        .selected_text(if self.settings.ocr_effort.is_empty() {
                            "default"
                        } else {
                            &self.settings.ocr_effort
                        })
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for level in &["", "low", "medium", "high", "xhigh", "max"] {
                                let label = if level.is_empty() { "default" } else { level };
                                ui.selectable_value(
                                    &mut self.settings.ocr_effort,
                                    level.to_string(),
                                    label,
                                );
                            }
                        })
                        .response
                        .on_hover_text(
                            "Effort level passed as --effort to the claude CLI. Lower effort = \
                             fewer tokens and lower cost, but may miss details. 'default' omits \
                             the flag entirely (claude chooses). 'low' is recommended for simple \
                             banner OCR.",
                        );
                    ui.end_row();
                });

            }
    }

    fn settings_twitch_account_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Accounts, "Twitch account (OAuth)", &["twitch", "account", "oauth", "connect", "login", "sub", "turbo"]) {
            ui.add_space(12.0);
            ui.heading("Twitch account (OAuth)");
            ui.label("Connect to use a user token for detection (Client Secret then optional).");
            let flow = self.twitch_flow.lock().unwrap().clone();
            match flow {
                AuthFlow::Connected { login } => {
                    ui.horizontal(|ui| {
                        if login.is_empty() {
                            ui.label("✅ Connected");
                        } else {
                            ui.label(format!("✅ Connected as {login}"));
                        }
                        if ui.button("Disconnect").clicked() {
                            let _ = oauth::disconnect(&self.core.store);
                            *self.twitch_flow.lock().unwrap() = AuthFlow::Idle;
                            // disconnect() clears the cached ad-free (sub) results;
                            // reload so the Streams column drops the stale badges now.
                            self.reload_rows();
                        }
                    });
                    ui.small(
                        "Tip: if you connected before the Ad-free / Import features, reconnect to \
                         grant the subscriptions + follows scopes.",
                    );
                    if ui
                        .button("📥 Import followed channels")
                        .on_hover_text(
                            "Add the channels this Twitch account follows as new streams \
                             (Auto off by default). Needs the 'follows' scope — reconnect if it \
                             was granted before this feature.",
                        )
                        .clicked()
                    {
                        self.open_import(Platform::Twitch, ui.ctx().clone());
                    }
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

            }
    }

    fn settings_google_account_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Accounts, "YouTube account (Google OAuth)", &["youtube", "google", "oauth", "account", "connect", "subscriptions"]) {
            ui.add_space(12.0);
            ui.heading("YouTube account (Google OAuth)");
            ui.label(
                "Connect a Google account to import your YouTube subscriptions. Needs a Google \
                 Cloud OAuth client of type \"TV and Limited Input devices\" with the YouTube \
                 Data API enabled.",
            );
            egui::Grid::new("google_creds_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Google Client ID");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.google_client_id)
                            .desired_width(320.0)
                            .hint_text("xxxxx.apps.googleusercontent.com"),
                    );
                    ui.end_row();
                    ui.label("Google Client Secret");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.google_client_secret)
                            .password(true),
                    );
                    ui.end_row();
                });
            let gflow = self.google_flow.lock().unwrap().clone();
            match gflow {
                AuthFlow::Connected { login } => {
                    ui.horizontal(|ui| {
                        if login.is_empty() {
                            ui.label("✅ Connected");
                        } else {
                            ui.label(format!("✅ Connected as {login}"));
                        }
                        if ui.button("Disconnect").clicked() {
                            let _ = google_oauth::disconnect(&self.core.store);
                            *self.google_flow.lock().unwrap() = AuthFlow::Idle;
                        }
                    });
                    if ui
                        .button("📥 Import subscriptions")
                        .on_hover_text(
                            "Add the channels this YouTube account subscribes to as new streams \
                             (Auto off by default).",
                        )
                        .clicked()
                    {
                        self.open_import(Platform::YouTube, ui.ctx().clone());
                    }
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
                    if ui.button("🔗 Connect YouTube").clicked() {
                        self.start_google_connect(ui.ctx().clone());
                    }
                }
                AuthFlow::Idle => {
                    if ui.button("🔗 Connect YouTube").clicked() {
                        self.start_google_connect(ui.ctx().clone());
                    }
                }
            }

            }
    }

    fn settings_websub_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Accounts, "YouTube WebSub (push via VPS)", &["youtube", "websub", "vps", "push", "relay", "pubsubhubbub"]) {
            ui.add_space(12.0);
            ui.heading("YouTube WebSub (push via VPS)");
            ui.label(
                "Optional. Point at a running yt-websub relay to get near-instant \
                 go-live triggers for YouTube channels set to the WebSub method.",
            );
            egui::Grid::new("websub_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("VPS base URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.websub_vps_url)
                            .desired_width(320.0)
                            .hint_text("https://hooks.example.com"),
                    )
                    .on_hover_text("The yt-websub server's HTTPS base URL (no trailing /api).");
                    ui.end_row();
                    ui.label("Bearer token");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.websub_token).password(true),
                    )
                    .on_hover_text("YTWEBSUB_BEARER_TOKEN configured on the VPS.");
                    ui.end_row();
                    ui.label("Poll interval (s)");
                    ui.add(egui::TextEdit::singleline(&mut self.settings.websub_poll_secs))
                        .on_hover_text("How often to pull new events from the VPS (min 5).");
                    ui.end_row();
                });

            }
    }

    fn settings_defaults_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Recording, "Defaults", &["default", "output", "folder", "media player", "concurrent", "filename", "date", "timestamp"]) {
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
                            self.pending_browse = Some(spawn_browse_folder(
                                &self.settings.default_output_dir,
                                |app, p| app.settings.default_output_dir = p,
                            ));
                        }
                    });
                    ui.end_row();
                    ui.label("Media player path").on_hover_text(
                        "Path to the media player used by \"Stream in player\" on recording rows. \
                         Passed the file path as the only argument (e.g. mpv.exe, vlc.exe). \
                         With mpv, in-progress recordings open with live-view flags that follow \
                         the growing file, and in-progress SABR captures (separate audio/video \
                         files) are playable too — other players only open finished or \
                         single-file captures.",
                    );
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.media_player_path)
                                .hint_text(r"C:\Progs\mpv\mpv.exe")
                                .desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_file(
                                &self.settings.media_player_path,
                                |app, p| app.settings.media_player_path = p,
                            ));
                        }
                    });
                    ui.end_row();
                    ui.label("Max concurrent downloads");
                    ui.text_edit_singleline(&mut self.settings.max_concurrent_downloads);
                    ui.end_row();
                    ui.label("Download rate limit").on_hover_text(
                        "yt-dlp --limit-rate for VOD-archive grabs and Videos-tab \
                         downloads (e.g. 4M, 500K). Empty = unlimited. Never applied \
                         to live captures — throttling the live edge loses data. \
                         Useful when post-stream VOD downloads write to the same \
                         drive as active recordings.",
                    );
                    ui.text_edit_singleline(&mut self.settings.download_rate_limit);
                    ui.end_row();
                    ui.label("Filename media info")
                        .on_hover_text(
                            "How the {resolution}/{height}/{width}/{fps}/{vcodec} filename \
                             variables get their values. Only applies when the filename \
                             template uses one of them.",
                        );
                    let mode = &mut self.settings.filename_media_info;
                    egui::ComboBox::from_id_salt("media_info_cb")
                        .selected_text(mode.label())
                        .show_ui(ui, |ui| {
                            for m in MediaInfoMode::ALL {
                                ui.selectable_value(mode, m, m.label())
                                    .on_hover_text(m.tooltip());
                            }
                        });
                    ui.end_row();

                    ui.label("Date format").on_hover_text(
                        "How dates and timestamps are shown throughout the app \
                         (the Polled / Went Live / Started On / Added columns, the \
                         history tree, etc.). Applies on Save.",
                    );
                    let df = &mut self.settings.date_fmt;
                    egui::ComboBox::from_id_salt("date_fmt_cb")
                        .selected_text(df.label())
                        .show_ui(ui, |ui| {
                            for f in DateFmt::ALL {
                                ui.selectable_value(df, f, f.label());
                            }
                        });
                    ui.end_row();

                    ui.label("Short timestamp format").on_hover_text(
                        "chrono pattern used when \"Short timestamps\" is on (top bar). \
                         Default: %d/%m %H:%M  (day/month + 24h time). \
                         Applies on Save.",
                    );
                    ui.text_edit_singleline(&mut self.settings.short_ts_fmt);
                    ui.end_row();
                });

            }
    }

    fn settings_display_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Interface, "Display", &["display", "actions", "emotes", "animate", "columns", "theme"]) {
            ui.add_space(12.0);
            ui.heading("Display");
            if ui
                .checkbox(&mut self.show_actions, "Show Actions column")
                .on_hover_text(
                    "Show the per-row Actions buttons column in the Streams and Videos \
                     tables. Turn it off to reclaim width — every action is also on each \
                     row's right-click context menu. Applies immediately.",
                )
                .changed()
            {
                let _ = self.core.store.set_setting(
                    K_SHOW_ACTIONS,
                    if self.show_actions { "1" } else { "0" },
                );
            }
            if ui
                .checkbox(&mut self.render_emotes, "Render emotes in chat")
                .on_hover_text(
                    "Show Twitch / BTTV / FFZ / 7TV emotes (and color emoji) as inline \
                     images in the chat replay. Off shows the emote code as plain text. \
                     Requires \"Fetch chat assets\" so the images are on disk. Applies \
                     immediately.",
                )
                .changed()
            {
                let _ = self.core.store.set_setting(
                    K_RENDER_EMOTES,
                    if self.render_emotes { "1" } else { "0" },
                );
            }
            if ui
                .add_enabled(
                    self.render_emotes,
                    egui::Checkbox::new(&mut self.animate_emotes, "Animate emotes"),
                )
                .on_hover_text(
                    "Play animated GIF / WebP emotes (Twitch, BTTV/FFZ, 7TV) and animated \
                     emoji. Off shows a static first frame — turn it off if a busy channel's \
                     animations use too much memory or CPU. Applies immediately.",
                )
                .changed()
            {
                let _ = self.core.store.set_setting(
                    K_ANIMATE_EMOTES,
                    if self.animate_emotes { "1" } else { "0" },
                );
            }
            ui.label(
                egui::RichText::new(
                    "Color emoji use Twemoji (© Twitter/jdecked, CC-BY 4.0), downloaded on \
                     demand and cached for offline replay.",
                )
                .small()
                .weak(),
            );

            }
    }

    fn settings_table_columns_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Interface, "Table columns", &["table", "columns", "reset", "grid", "sort"]) {
            ui.add_space(12.0);
            ui.heading("Table columns");
            ui.label(
                egui::RichText::new(
                    "Column visibility, order, and sort persist per table — right-click any \
                     table header to hide/show or reorder columns. These three buttons reset \
                     every table at once; they're kept here, away from the grids, so a stray \
                     click while customizing a table can't wipe it out by accident.",
                )
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                let all_columns: [(GridTableId, &[GridCol]); 6] = [
                    (GridTableId::Streams, &STREAM_COLUMNS),
                    (GridTableId::Videos, &VIDEO_COLUMNS),
                    (GridTableId::BgActive, &BG_ACTIVE_COLUMNS),
                    (GridTableId::BgRecent, &BG_RECENT_COLUMNS),
                    (GridTableId::Processes, &PROCESSES_COLUMNS),
                    (GridTableId::Issues, &ISSUES_COLUMNS),
                ];
                if ui
                    .button("Reset all columns")
                    .on_hover_text("Show every column, in its default order, on every table.")
                    .clicked()
                {
                    grid_columns::reset_all_columns(&self.core.store, &all_columns);
                    self.reload_all_grid_entries();
                    self.status = "Reset all table columns to default.".into();
                }
                if ui
                    .button("Reset column sort")
                    .on_hover_text("Clear sort on every table (Streams, Videos are the only sortable ones).")
                    .clicked()
                {
                    grid_columns::reset_all_sort(&self.core.store, &GridTableId::ALL);
                    self.streams_sort = SortState::default();
                    self.videos_sort = SortState::default();
                    self.status = "Reset table sort.".into();
                }
                if ui
                    .button("Reset all column positions")
                    .on_hover_text("Restore default column order on every table — keeps your show/hide choices.")
                    .clicked()
                {
                    grid_columns::reset_all_positions(&self.core.store, &all_columns);
                    self.reload_all_grid_entries();
                    self.status = "Reset all table column positions.".into();
                }
            });

            }
    }

    fn settings_download_auth_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "Download authentication", &["download", "auth", "cookies", "browser", "token", "profile", "login"]) {
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

                        ui.label("Profile / session");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.cookies_profile)
                                .hint_text("optional — e.g. dmrf6eed.YouTube"),
                        )
                        .on_hover_text(
                            "Which browser profile/session to read cookies from. Blank = the \
                             browser's default (most-recently-used) profile — which is why a \
                             dedicated login can be missed. For Firefox, use the profile folder \
                             name (the directory under …/Mozilla/Firefox/Profiles, e.g. \
                             dmrf6eed.YouTube) or an absolute path to it; find it at about:profiles.",
                        );
                        ui.end_row();
                    }
                });

            }
    }

    fn settings_ytdlp_args_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "yt-dlp default arguments", &["yt-dlp", "ytdlp", "arguments", "args", "binary", "path"]) {
            ui.add_space(12.0);
            ui.heading("yt-dlp default arguments");
            ui.label("Prepended to every yt-dlp invocation. Per-channel extra args are appended after and override these.");
            egui::Grid::new("ytdlp_args_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Extra args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.ytdlp_default_args)
                            .hint_text("e.g. --js-runtimes node --cookies-from-browser firefox:dmrf6eed.YouTube")
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(
                        "Shell-style space-separated arguments. Quoted strings are supported \
                         (e.g. \"value with spaces\"). Applied to all yt-dlp monitors; \
                         useful for --js-runtimes node, --cookies-from-browser, \
                         --concurrent-fragments, --throttled-rate, etc.",
                    );
                    ui.end_row();
                });

            }
    }

    fn settings_sabr_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "YouTube SABR (live-from-start)", &["youtube", "sabr", "live-from-start", "po token", "dash", "codec", "capture from start"]) {
            ui.add_space(12.0);
            ui.heading("YouTube SABR (live-from-start)");
            ui.label(
                "YouTube live capture-from-start needs the SABR protocol, which only the \
                 yt-dlp dev build provides. Point to that binary below; it is used ONLY for \
                 YouTube monitors with Capture-from-start. Chat, assets, VODs, and every other \
                 capture keep using the system yt-dlp.",
            );
            egui::Grid::new("sabr_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("System yt-dlp path");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.ytdlp_binary_path)
                                .hint_text("(empty = yt-dlp on PATH)")
                                .desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_file(
                                &self.settings.ytdlp_binary_path,
                                |app, p| app.settings.ytdlp_binary_path = p,
                            ));
                        }
                    });
                    ui.end_row();

                    ui.label("SABR build path");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.sabr_binary_path)
                                .hint_text(r"e.g. C:\git\yt-dlp-dev\dist\yt-dlp.exe")
                                .desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_file(
                                &self.settings.sabr_binary_path,
                                |app, p| app.settings.sabr_binary_path = p,
                            ));
                        }
                    })
                    .response
                    .on_hover_text(
                        "The yt-dlp dev fork with SABR support (bashonly's feat/youtube/sabr). \
                         A moving target — re-point this after rebuilding it. Empty = SABR off.",
                    );
                    ui.end_row();

                    ui.label("Use SABR for capture-from-start");
                    ui.checkbox(&mut self.settings.sabr_enabled, "").on_hover_text(
                        "When on (and a SABR build is set), YouTube monitors with \
                         Capture-from-start record via the SABR build.",
                    );
                    ui.end_row();

                    ui.label("SABR format");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.sabr_format)
                            .hint_text(crate::downloader::SABR_DEFAULT_FORMAT)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("SABR extractor-args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.sabr_extractor_args)
                            .hint_text(crate::downloader::SABR_DEFAULT_EXTRACTOR_ARGS)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Deep rewind (experimental)");
                    ui.checkbox(&mut self.settings.sabr_deep_rewind, "")
                        .on_hover_text(
                            "Appends enable_live_deep_rewind=true to the SABR extractor-args, \
                             letting capture-from-start rewind past YouTube's normal ~4h DVR \
                             window (so it can reach the start of a long-running stream instead \
                             of stalling). Requires a SABR dev build that supports it; a stock \
                             yt-dlp ignores it. Experimental and may be unstable. Has no effect \
                             when SABR manual args are set below.",
                        );
                    ui.end_row();

                    ui.label("SABR manual args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.sabr_raw_args)
                            .hint_text("(optional — overrides format + extractor-args above)")
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(
                        "When set, these raw args replace the SABR format + extractor-args \
                         preset entirely (put your own -f / --extractor-args here).",
                    );
                    ui.end_row();

                    ui.label("PO token extractor-args");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.sabr_pot_args)
                            .hint_text(crate::downloader::SABR_DEFAULT_POT_ARGS)
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(
                        "Passed as a SEPARATE --extractor-args entry on the SABR command \
                         (different extractor key than the format args above), for a GVS \
                         PO-token provider such as bgutil. Default points at the bgutil HTTP \
                         server on its standard port 4416. Leave empty to rely on the \
                         provider plugin's own auto-detection. Requires the provider plugin \
                         installed for the SABR build + its server running.",
                    );
                    ui.end_row();

                    ui.label("Video codec / quality");
                    egui::ComboBox::from_id_salt("settings_sabr_codec_pref")
                        .selected_text(self.settings.sabr_codec_pref.label())
                        .show_ui(ui, |ui| {
                            for &p in &SabrCodecPref::GLOBAL {
                                ui.selectable_value(
                                    &mut self.settings.sabr_codec_pref,
                                    p,
                                    p.label(),
                                );
                            }
                        });
                    ui.end_row();
                    if self.settings.sabr_codec_pref == SabrCodecPref::Custom {
                        ui.label("Custom -S sort");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.sabr_codec_custom)
                                .hint_text("res,fps,vcodec:h264")
                                .desired_width(f32::INFINITY),
                        )
                        .on_hover_text(
                            "Raw yt-dlp -S format-sort applied to the SABR selector. Lead with \
                             res,fps so resolution/fps win and codec/bitrate is only the tiebreak.",
                        );
                        ui.end_row();
                    }

                    ui.label("DASH companion format");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.dash_format)
                            .hint_text(crate::downloader::DASH_DEFAULT_FORMAT)
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text(
                        "Format selector for the DASH companion process when a monitor has \
                         Dual capture (SABR + DASH) enabled. Uses the system yt-dlp.",
                    );
                    ui.end_row();
                });

            }
    }

    fn settings_custom_tools_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "Custom download tools", &["custom", "tool", "binary", "alias", "yt-dlp", "fork"]) {
            ui.add_space(12.0);
            ui.heading("Custom download tools 🔧");
            ui.label(
                "Alternate yt-dlp-compatible binaries (e.g. a personal fork or another dev \
                 build) — each becomes selectable as its own \"Tool\" in the Videos tab's \
                 download form, alongside yt-dlp and yt-dlp-dev (SABR). Uses the same yt-dlp \
                 arguments; only the invoked binary differs.",
            );
            ui.add_space(6.0);
            custom_tools_editor(ui, &mut self.settings.custom_tools, &mut self.pending_browse);

            }
    }

    fn settings_monitor_defaults_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Recording, "Stream monitor defaults", &["monitor", "defaults", "platform", "quality", "tool", "container", "detection"]) {
            ui.add_space(12.0);
            ui.heading("Stream monitor defaults");
            ui.label(
                "Applied when creating a new monitor. Platform settings override the global; \
                 leave a field unset / empty to inherit from the global (or the built-in fallback).",
            );
            ui.add_space(4.0);

            // Work on a local clone to avoid borrow-checker issues (cross-field access
            // for hint text vs mutable edit access for the combo/text widgets).
            let mut md = self.monitor_defaults.clone();
            let custom_presets = self.custom_presets.as_slice();
            let mut mdef_preset_delete: Option<i64> = None;
            let mut mdef_preset_save_tmpl: Option<String> = None;

            for (label, platform_opt) in [
                ("🌐  Global", None),
                ("  Twitch",   Some(Platform::Twitch)),
                ("  YouTube",  Some(Platform::YouTube)),
                ("  Kick",     Some(Platform::Kick)),
                ("  Generic",  Some(Platform::Generic)),
            ] {
                let default_open = platform_opt.is_none();
                egui::CollapsingHeader::new(label)
                    .default_open(default_open)
                    .show(ui, |ui| {
                        let inherit = if platform_opt.is_some() { "Inherit" } else { "Not set" };

                        let methods: &[DetectionMethod] = match platform_opt {
                            None => &[
                                DetectionMethod::TwitchApi,
                                DetectionMethod::EventSubHelix,
                                DetectionMethod::YouTubeApi,
                                DetectionMethod::WebSub,
                                DetectionMethod::WebSubOnly,
                                DetectionMethod::Scrape,
                                DetectionMethod::KickApi,
                                DetectionMethod::GenericProbe,
                                DetectionMethod::Disabled,
                            ],
                            Some(p) => p.detection_methods(),
                        };

                        // Pre-compute hints from global for per-platform sections.
                        let q_hint: String = match platform_opt {
                            None => "best".to_string(),
                            Some(_) => md.global.quality.clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "best".to_string()),
                        };
                        let pi_hint: String = match platform_opt {
                            None => "60".to_string(),
                            Some(_) => md.global.poll_interval_secs
                                .unwrap_or(60)
                                .to_string(),
                        };
                        let ft_hint: String = match platform_opt {
                            None => "{name}_{date}_{time}".to_string(),
                            Some(_) => md.global.filename_template.clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "{name}_{date}_{time}".to_string()),
                        };
                        let od_hint: String = match platform_opt {
                            None => self.settings.default_output_dir.clone(),
                            Some(_) => md.global.output_dir.clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| self.settings.default_output_dir.clone()),
                        };
                        let fs_hint: String = if platform_opt.is_some() {
                            match md.global.from_start {
                                Some(true) => "Inherit (on)".to_string(),
                                Some(false) => "Inherit (off)".to_string(),
                                None => "Inherit (on)".to_string(),
                            }
                        } else {
                            "on".to_string()
                        };

                        let d = match platform_opt {
                            None => &mut md.global,
                            Some(p) => md.get_mut(p),
                        };

                        egui::Grid::new(format!("mdef_{label}"))
                            .num_columns(4)
                            .spacing([8.0, 6.0])
                            .show(ui, |ui| {
                                // Row 1: Tool, Detection
                                ui.label("Tool");
                                egui::ComboBox::from_id_salt(format!("mdef_tool_{label}"))
                                    .selected_text(match d.tool {
                                        None => inherit,
                                        Some(t) => t.label(),
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut d.tool, None, inherit);
                                        for &t in &Tool::ALL {
                                            ui.selectable_value(&mut d.tool, Some(t), t.label());
                                        }
                                    });
                                ui.label("Detection");
                                egui::ComboBox::from_id_salt(format!("mdef_det_{label}"))
                                    .selected_text(match d.detection_method {
                                        None => inherit,
                                        Some(m) => m.label(),
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut d.detection_method, None, inherit);
                                        for &m in methods {
                                            ui.selectable_value(&mut d.detection_method, Some(m), m.label());
                                        }
                                    });
                                ui.end_row();

                                // Row 2: Container, Quality
                                ui.label("Container");
                                egui::ComboBox::from_id_salt(format!("mdef_cont_{label}"))
                                    .selected_text(match d.container {
                                        None => inherit,
                                        Some(c) => c.label(),
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut d.container, None, inherit);
                                        for &c in &Container::ALL {
                                            ui.selectable_value(&mut d.container, Some(c), c.label());
                                        }
                                    });
                                ui.label("Quality");
                                let q_ref = d.quality.get_or_insert_with(String::new);
                                ui.add(
                                    egui::TextEdit::singleline(q_ref)
                                        .hint_text(q_hint)
                                        .desired_width(100.0),
                                );
                                ui.end_row();

                                // Row 3: Poll interval
                                ui.label("Poll interval (s)");
                                let mut pi_str = d.poll_interval_secs
                                    .map(|v| v.to_string())
                                    .unwrap_or_default();
                                if ui.add(
                                    egui::TextEdit::singleline(&mut pi_str)
                                        .hint_text(pi_hint)
                                        .desired_width(80.0),
                                ).changed() {
                                    d.poll_interval_secs = pi_str.trim().parse::<i64>().ok()
                                        .filter(|&v| v > 0);
                                }
                                ui.label("");
                                ui.label("");
                                ui.end_row();

                                // Row 4: Filename template
                                ui.label("Filename");
                                let ft_ref = d.filename_template.get_or_insert_with(String::new);
                                ui.horizontal(|ui| {
                                    let (del, save) = filename_preset_combo(
                                        ui,
                                        &format!("mdef_tmpl_{label}"),
                                        ft_ref,
                                        custom_presets,
                                    );
                                    if del.is_some() { mdef_preset_delete = del; }
                                    if save { mdef_preset_save_tmpl = Some(ft_ref.clone()); }
                                    ui.add(
                                        egui::TextEdit::singleline(ft_ref)
                                            .hint_text(&ft_hint)
                                            .desired_width(150.0),
                                    ).on_hover_text(
                                        "Tokens: {name} {date} {time} {timestamp} {year} {month} {day} {hour} {minute} {second} {title} {games} {video_id} {quality} {resolution} {height} {width} {fps} {vcodec} {acodec} {take} {tool} {mode} {platform} {went_live_date} {went_live_time}",
                                    );
                                });
                                ui.label("");
                                ui.label("");
                                ui.end_row();

                                // Row 5: Output directory
                                ui.label("Output dir");
                                let od_ref = d.output_dir.get_or_insert_with(String::new);
                                ui.add(
                                    egui::TextEdit::singleline(od_ref)
                                        .hint_text(od_hint)
                                        .desired_width(200.0),
                                );
                                ui.label("");
                                ui.label("");
                                ui.end_row();

                                // Row 6: Capture from start
                                ui.label("Capture from start")
                                    .on_hover_text(
                                        "yt-dlp --live-from-start / streamlink --hls-live-restart.\n\
                                         Default for new stream monitors on this platform.",
                                    );
                                egui::ComboBox::from_id_salt(format!("mdef_fs_{label}"))
                                    .selected_text(match d.from_start {
                                        None => inherit,
                                        Some(true) => "On",
                                        Some(false) => "Off",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut d.from_start, None, format!("{inherit} ({fs_hint})"));
                                        ui.selectable_value(&mut d.from_start, Some(true), "On");
                                        ui.selectable_value(&mut d.from_start, Some(false), "Off");
                                    });
                                ui.label("");
                                ui.label("");
                                ui.end_row();
                            });
                    });
            }

            // Write back the (possibly edited) clone.
            self.monitor_defaults = md;

            // Apply preset actions now that md borrow is released.
            if let Some(id) = mdef_preset_delete {
                if let Err(e) = self.core.store.delete_filename_preset(id) {
                    self.status = format!("Error deleting preset: {e:#}");
                } else {
                    self.custom_presets = self.core.store.get_filename_presets().unwrap_or_default();
                }
            }
            if let Some(tmpl) = mdef_preset_save_tmpl {
                self.save_preset_dialog = Some(SavePresetDraft {
                    template: tmpl,
                    name: String::new(),
                    error: String::new(),
                });
            }

            }
    }

    fn settings_startup_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::System, "Startup", &["startup", "start at login", "autostart", "boot"]) {
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

            }
    }

    fn settings_notifications_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Interface, "Notifications", &["notifications", "desktop", "toast", "alerts"]) {
            ui.add_space(12.0);
            ui.heading("Notifications");
            let mut notify_on = self.notifications_enabled;
            if ui
                .checkbox(&mut notify_on, "Show desktop notifications")
                .on_hover_text(
                    "Show a desktop toast when a recording starts, finishes, or errors. \
                     Uncheck to silence all pop-up alerts (the in-app status line and \
                     Background view still update). Takes effect immediately.",
                )
                .changed()
            {
                self.notifications_enabled = notify_on;
                let _ = self
                    .core
                    .store
                    .set_setting(
                        crate::notifications::K_NOTIFICATIONS,
                        if notify_on { "1" } else { "0" },
                    );
                self.status = if notify_on {
                    "Desktop notifications enabled.".into()
                } else {
                    "Desktop notifications disabled.".into()
                };
            }

            }
    }

    fn settings_shutdown_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::System, "Shutdown", &["shutdown", "quit", "close", "keep downloads", "exit"]) {
            ui.add_space(12.0);
            ui.heading("Shutdown");
            let mut keep = self.keep_downloads_on_quit;
            if ui
                .checkbox(&mut keep, "Keep downloads running when the app closes")
                .on_hover_text(
                    "Default. Quitting detaches the recording tools so they keep running and \
                     writing — the app re-attaches to them on the next launch, so you can \
                     restart or rebuild without stopping a recording. Uncheck to stop all \
                     downloads on quit instead. (The tray's \"Quit & stop recordings\" always \
                     stops them, regardless of this.)",
                )
                .changed()
            {
                self.keep_downloads_on_quit = keep;
                // Stored inverted: the setting names the opt-IN to stopping.
                let _ = self
                    .core
                    .store
                    .set_setting("stop_downloads_on_quit", if keep { "0" } else { "1" });
                self.status = if keep {
                    "Downloads will keep running when the app closes.".into()
                } else {
                    "Downloads will stop when the app closes.".into()
                };
            }

            ui.add_space(12.0);
            // ── Remux ──────────────────────────────────────────────────────────
            }
    }

    fn settings_remux_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Recording, "Remux", &["remux", "mkv", "thumbnail", "title", "subtitles", "embed", "cover", "throttle", "readrate", "disk", "speed"]) {
            ui.add_space(12.0);
            ui.heading("Remux");
            ui.label("Controls what gets embedded into MKV files when a recording is finalized (TS→MKV remux).");
            egui::Grid::new("remux_opts_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.checkbox(&mut self.settings.remux_embed_thumbnail, "Embed thumbnail as cover art");
                    ui.label("Attach the thumbnail sidecar (if present) as MKV cover art.");
                    ui.end_row();
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.settings.remux_embed_title, "Embed title tag");
                        ui.add_enabled(
                            self.settings.remux_embed_title,
                            egui::TextEdit::singleline(&mut self.settings.remux_title_template)
                                .hint_text("{title}")
                                .desired_width(200.0),
                        );
                    });
                    ui.label("Template for the MKV title tag. Tokens: {title} {channel} {games} {date} {year} {month} {day} {name}");
                    ui.end_row();
                    ui.checkbox(&mut self.settings.remux_embed_subs, "Embed subtitle sidecars");
                    ui.label("Copy .srt/.ass/.vtt sidecar files as subtitle streams in the MKV.");
                    ui.end_row();
                    ui.horizontal(|ui| {
                        ui.label("Disk throttle:");
                        ui.add(
                            egui::DragValue::new(&mut self.settings.postproc_readrate)
                                .range(0.0..=1000.0)
                                .speed(1.0)
                                .suffix("× realtime"),
                        );
                    });
                    setting_desc(
                        ui,
                        "Caps how fast finalize remuxes/joins/embeds read + write, so they \
                         can't starve live recordings on the same drive. 0 = unthrottled. \
                         30× ≈ a 5h stream finalizing in ~10 min. Needs ffmpeg 5.0+ \
                         (silently unthrottled on older builds).",
                    );
                    ui.end_row();
                    ui.horizontal(|ui| {
                        ui.label("yt-dlp ffmpeg throttle:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.ytdlp_ppa)
                                .hint_text("Merger+ffmpeg_i:-readrate 30")
                                .desired_width(240.0),
                        );
                    });
                    setting_desc(
                        ui,
                        "yt-dlp --postprocessor-args specs (separate several with ;;). \
                         The disk throttle above can't reach ffmpeg passes yt-dlp runs \
                         INTERNALLY — e.g. the post-stream SABR format merge reads + \
                         writes the whole multi-GB capture at full disk speed. \
                         \"Merger+ffmpeg_i:-readrate 30\" caps merges at 30× realtime \
                         (needs ffmpeg 5.0+). Empty = unthrottled.",
                    );
                    ui.end_row();
                    ui.horizontal(|ui| {
                        ui.label("Capture cache location(s):");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.capture_cache_root)
                                .hint_text(r"A:\streams\.sa-cache; G:\streams\.sa-cache")
                                .desired_width(240.0),
                        );
                        if ui
                            .button("Browse…")
                            .on_hover_text(
                                "Pick a folder — appended to the list (one location per \
                                 drive, ';'-separated).",
                            )
                            .clicked()
                        {
                            let first = self
                                .settings
                                .capture_cache_root
                                .split(';')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            self.pending_browse = Some(spawn_browse_folder(&first, |app, p| {
                                let s = &mut app.settings.capture_cache_root;
                                if s.trim().is_empty() {
                                    *s = p;
                                } else {
                                    *s = format!("{}; {}", s.trim().trim_end_matches(';'), p);
                                }
                            }));
                        }
                    });
                    setting_desc(
                        ui,
                        "Central folder(s) for ALL in-progress capture files, one subfolder \
                         per channel — a single subtree per drive that backup tools can \
                         exclude by path (Backblaze has no wildcard rules). Recordings can \
                         span drives: list one location per drive, separated by ';'. Each \
                         only applies to output folders on ITS drive (finalizing must stay \
                         a same-volume rename); drives without one keep a per-folder \
                         .sa-cache. Empty = a .sa-cache subfolder inside each output \
                         folder. Existing files are found either way; takes started before \
                         a change finish under the old layout.",
                    );
                    ui.end_row();
                    ui.checkbox(&mut self.settings.iomon_sample_log, "I/O sample log");
                    setting_desc(
                        ui,
                        "Write the I/O monitor's 1s samples to a JSONL under the appdata \
                         logs folder (system drive) so drive stalls and disconnects can be \
                         analyzed after the fact. ~2-5 MB/day, pruned after 14 days.",
                    );
                    ui.end_row();
                });

            // ── File Management ────────────────────────────────────────────────
            }
    }

    fn settings_file_management_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Recording, "File Management", &["file", "management", "subdirectories", "organize", "split", "folders"]) {
            ui.add_space(12.0);
            ui.heading("File Management");
            ui.label("Split captured files into per-type subdirectories under the monitor output directory.");
            egui::Grid::new("file_split_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.checkbox(&mut self.settings.file_split_enabled, "Enable subdirectory splitting");
                    ui.label("Move files into separate dirs (videos/, subs/, chat/, thumbs/, logs/).");
                    ui.end_row();

                    let enabled = self.settings.file_split_enabled;
                    ui.label("Videos dir");
                    ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.settings.file_split_videos).desired_width(140.0).hint_text("videos"));
                    ui.end_row();
                    ui.label("Subs dir");
                    ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.settings.file_split_subs).desired_width(140.0).hint_text("subs"));
                    ui.end_row();
                    ui.label("Chat dir");
                    ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.settings.file_split_chat).desired_width(140.0).hint_text("chat"));
                    ui.end_row();
                    ui.label("Thumbs dir");
                    ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.settings.file_split_thumbs).desired_width(140.0).hint_text("thumbs"));
                    ui.end_row();
                    ui.label("Logs dir");
                    ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.settings.file_split_logs).desired_width(140.0).hint_text("logs"));
                    ui.end_row();
                });

            // ── Post-stream VOD download ────────────────────────────────────────
            }
    }

    fn settings_vod_download_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "Post-stream VOD download", &["vod", "download", "archive", "replace", "post-stream", "published"]) {
            ui.add_space(12.0);
            ui.heading("Post-stream VOD download 📼");
            ui.label(
                "After a stream ends, download the platform's published (post-processed) VOD — \
                 Twitch/YouTube/Kick — alongside the live recording. These are the GLOBAL \
                 defaults; override per-channel (channel Properties) or per-instance (edit \
                 instance). A muted Twitch VOD is un-muted via recovery and never replaces the \
                 live copy.",
            );
            ui.add_space(6.0);
            ui.checkbox(
                &mut self.settings.vod_dl_enabled,
                "Download the published VOD after a stream ends",
            );
            ui.checkbox(
                &mut self.settings.vod_dl_replace,
                "Replace the live recording with the VOD when the download succeeds",
            )
            .on_hover_text(
                "Only when the download succeeds and (Twitch) the VOD isn't DMCA-muted. The live \
                 recording's chat/thumbnail sidecars are kept.",
            );

            // ── Post-stream VOD download ────────────────────────────────────────
            }
    }

    fn settings_head_backfill_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "Head backfill on new takes", &["head", "backfill", "take", "retake", "reconnect", "capture from start"]) {
            ui.add_space(12.0);
            ui.heading("Head backfill on new takes 🧩");
            ui.label(
                "Capture-from-start only: when a stream reconnects mid-broadcast (a new \
                 recording \"take\"), the gap since the previous take ended is lost the same way \
                 a missed intro is — and is just as recoverable from the still-growing live CDN \
                 playlist while the stream stays live. These are the GLOBAL defaults; override \
                 per-channel (channel Properties) or per-instance (edit instance).",
            );
            ui.add_space(6.0);
            ui.checkbox(
                &mut self.settings.quality_upgrade_restart,
                "Restart the take when a better quality appears (Twitch)",
            )
            .on_hover_text(
                "A capture that joins seconds after go-live often sees only transcodes — \
                 Twitch lists the source rendition late — and locks onto e.g. 720p60 while \
                 the stream is really 1080p60 (also why its head backfill, which is always \
                 source, can't be joined with it). With this on, `best`-quality streamlink \
                 captures re-check a few minutes in and restart once at the better \
                 rendition; the new take's head backfill covers the seam and joins into a \
                 complete full.mkv at the better quality.",
            );
            ui.checkbox(
                &mut self.settings.head_backfill_fetch_new_take,
                "Fetch new head backfill on new take",
            )
            .on_hover_text(
                "Fetch a fresh, full head backfill (go-live through this take's start) for every \
                 take, not just the stream's first. Off restores the original behavior: only the \
                 first take ever gets a head backfill.",
            );
            ui.checkbox(
                &mut self.settings.head_backfill_replace_old,
                "Replace old head (if new is undamaged)",
            )
            .on_hover_text(
                "Once a fresh head backfill passes its integrity checks (no muted segments, \
                 plausible duration), delete older takes' now-redundant head files for the same \
                 stream. Only takes effect when fetching a new head is also on; a fresh head \
                 that fails its checks is still kept, just never used to replace anything.",
            );
            ui.label(
                egui::RichText::new(
                    "The Streams grid also has a manual \"🧩 Backfill head\" action (on an \
                     instance — targets its latest recording — or on a specific take), enabled \
                     only while the channel is live. It always forces the fetch regardless of \
                     the \"fetch new head backfill on new take\" setting above (replace-old still \
                     follows the setting).",
                )
                .small()
                .weak(),
            );

            // ── Trigger words ──────────────────────────────────────────────────
            }
    }

    fn settings_trigger_words_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "Trigger words", &["trigger", "word", "karaoke", "unarchived", "force", "auto", "title", "game", "regex"]) {
            ui.add_space(12.0);
            ui.heading("Trigger words ⚡");
            ui.label(
                "Start recording when a live stream's title or game matches a rule — even when \
                 Auto-record is OFF. Meant for words like \"unarchived\" or \"karaoke\" that \
                 signal there will be no VOD (or a muted one). Checked at go-live and on every \
                 poll, so a mid-stream title change also fires. Each rule can force the \
                 'capture from start' flag for the recording it starts. These are the GLOBAL \
                 rules; channel/instance Properties can extend, replace, or disable them.",
            );
            ui.add_space(6.0);
            trigger_rules_editor(ui, &mut self.settings.trigger_rules, "settings_triggers", true);
            ui.label(
                egui::RichText::new(
                    "Note: EventSub-pushed go-lives fetch the title via a follow-up check; \
                     YouTube 'Data API' detection has no title — use the scrape method there.",
                )
                .small()
                .weak(),
            );
            }
    }

    fn settings_blacklist_triggers_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "Blacklist triggers", &["blacklist", "block", "trigger", "prevent", "skip", "rerun", "veto", "title", "game", "regex"]) {
            ui.add_space(12.0);
            ui.heading("Blacklist triggers 🚫");
            ui.label(
                "The inverse of trigger words: PREVENT automatic recording while the live \
                 title or game matches a rule — e.g. \"rerun\", \"24/7\", or a game you never \
                 want archived. A blacklist match vetoes both Auto-record and trigger-word \
                 starts; a manual ▶ Start always records. Checked at go-live and on every \
                 poll; a recording that is already running is NOT stopped by a mid-stream \
                 match. These are the GLOBAL rules; channel/instance Properties can extend, \
                 replace, or disable them.",
            );
            ui.add_space(6.0);
            trigger_rules_editor(ui, &mut self.settings.trigger_block_rules, "settings_block_triggers", false);

            // ── Twitch VOD recovery ────────────────────────────────────────────
            }
    }

    fn settings_vod_recovery_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "Twitch VOD recovery", &["vod", "recovery", "muted", "deleted", "cdn", "recover", "unmute"]) {
            ui.add_space(12.0);
            ui.heading("Twitch VOD recovery 🛟");
            ui.label(
                "Reconstruct deleted or DMCA-muted Twitch VODs from segments still on the CDN \
                 (~60-day window). Recovery is derived from a recording's broadcast id + go-live \
                 time — no Twitch login required.",
            );
            ui.add_space(6.0);
            egui::Grid::new("vod_recovery_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.checkbox(&mut self.settings.auto_recover_muted, "Auto-recover muted VODs");
                    ui.label("When the VOD checker finds a DMCA-muted VOD, recover it automatically.");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.auto_recover_deleted, "Auto-recover deleted VODs");
                    ui.label("When a stream never publishes a VOD, try to recover it from the CDN automatically.");
                    ui.end_row();

                    ui.label("Default quality");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.recovery_quality)
                            .desired_width(140.0)
                            .hint_text("chunked (source)"),
                    );
                    ui.end_row();

                    ui.label("Max concurrent probes");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.recovery_max_conc)
                            .desired_width(80.0)
                            .hint_text("8"),
                    );
                    ui.end_row();

                    ui.label("Extra CDN hosts");
                    ui.add(
                        egui::TextEdit::multiline(&mut self.settings.recovery_cdn_hosts)
                            .desired_rows(2)
                            .desired_width(360.0)
                            .hint_text("extra https hosts, one per line — added to the built-in + learned sets"),
                    );
                    ui.end_row();

                    let refresh_running = self
                        .background_tasks
                        .iter()
                        .any(|t| t.kind == crate::events::BackgroundTaskKind::RefreshCdnHosts);
                    if ui
                        .add_enabled(!refresh_running, egui::Button::new("Refresh CDN hosts"))
                        .on_hover_text("Harvest the current Twitch CDN hosts from your published VODs (via Twitch's public API) and remember any new ones. The host list also learns automatically from every successful recovery.")
                        .clicked()
                    {
                        self.core.manual(ManualCommand::RefreshCdnHosts);
                        self.status = "Refreshing CDN hosts…".into();
                    }
                    // Cached: re-reading + re-parsing the host list from the
                    // store every frame stalls rendering on the DB mutex.
                    let known = match self.recovery_host_count {
                        Some((at, n)) if at.elapsed() < std::time::Duration::from_secs(5) => n,
                        _ => {
                            let n = crate::recovery::load_hosts(&self.core.store).len();
                            self.recovery_host_count = Some((std::time::Instant::now(), n));
                            n
                        }
                    };
                    ui.label(format!("{known} CDN hosts known (built-in + learned + extra)."));
                    ui.end_row();

                    let scan_running = self
                        .background_tasks
                        .iter()
                        .any(|t| t.kind == crate::events::BackgroundTaskKind::RecoverVodScan);
                    if ui
                        .add_enabled(!scan_running, egui::Button::new("Recover deleted/muted VODs"))
                        .on_hover_text("Scan all recordings within the ~60-day window that are deleted or muted and recover each.")
                        .clicked()
                    {
                        let quality = self.settings.recovery_quality.trim().to_string();
                        self.core.manual(ManualCommand::ScanRecoverableVods { window_days: 60, quality });
                        self.status = "VOD recovery scan started — see the Background tab.".into();
                    }
                    ui.label("Bulk-recover every eligible recording (deleted or muted) inside the CDN retention window.");
                    ui.end_row();
                });

            // ── Maintenance ────────────────────────────────────────────────────
            }
    }

    fn settings_maintenance_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Maintenance, "Maintenance", &["maintenance", "re-remux", "remux all", "thumbnails", "reorganize", "batch", "preset"]) {
            ui.add_space(12.0);
            ui.heading("Maintenance 🔧");
            ui.label("One-time batch jobs — each runs in the background and reports progress in the Background tab.");
            ui.add_space(6.0);
            let mut maint_preset_delete: Option<i64> = None;
            let mut maint_preset_save_tmpl: Option<String> = None;
            let mut do_set_filename_default = false;
            let maint_custom_presets = self.custom_presets.clone();
            use crate::events::BackgroundTaskKind as BTK;
            let reremux_all_running   = self.background_tasks.iter().any(|t| t.kind == BTK::ReRemuxAll);
            let embed_thumb_running   = self.background_tasks.iter().any(|t| t.kind == BTK::EmbedMissingThumbnails);
            let fetch_thumb_running   = self.background_tasks.iter().any(|t| t.kind == BTK::FetchMissingThumbnails);
            let reorganize_running    = self.background_tasks.iter().any(|t| t.kind == BTK::ReorganizeAll);
            egui::Grid::new("maintenance_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    if ui.add_enabled(!reremux_all_running, egui::Button::new("Re-remux all")).clicked() {
                        self.core.manual(ManualCommand::ReRemuxAll);
                    }
                    ui.label("Re-run TS→MKV remux for any recording whose .ts source is still on disk.");
                    ui.end_row();

                    if ui.add_enabled(!embed_thumb_running, egui::Button::new("Embed missing thumbnails")).clicked() {
                        self.core.manual(ManualCommand::EmbedMissingThumbnails);
                    }
                    ui.label("Embed the thumbnail sidecar into MKV files that don't already have cover art.");
                    ui.end_row();

                    ui.horizontal(|ui| {
                        if ui.add_enabled(!fetch_thumb_running, egui::Button::new("Fetch missing thumbnails")).clicked() {
                            self.core.manual(ManualCommand::FetchMissingThumbnails { embed: self.settings.fetch_thumb_embed });
                        }
                        ui.checkbox(&mut self.settings.fetch_thumb_embed, "Embed after fetch");
                    });
                    ui.label("Download thumbnails for recordings that are missing a sidecar.");
                    ui.end_row();

                    if ui.add_enabled(!reorganize_running, egui::Button::new("Re-organize all files")).clicked() {
                        self.core.manual(ManualCommand::ReorganizeAll);
                    }
                    ui.label("Move files into/out of subdirectories based on current File Management settings.");
                    ui.end_row();

                    ui.horizontal(|ui| {
                        let (del, save) = filename_preset_combo(
                            ui,
                            "maint_filename_preset",
                            &mut self.settings.maintenance_filename_preset,
                            &maint_custom_presets,
                        );
                        if del.is_some() { maint_preset_delete = del; }
                        if save { maint_preset_save_tmpl = Some(self.settings.maintenance_filename_preset.clone()); }
                        let has_preset = !self.settings.maintenance_filename_preset.is_empty();
                        if ui.add_enabled(has_preset, egui::Button::new("Set as Default"))
                            .on_hover_text("Set this preset as the global filename template default for new monitors.")
                            .clicked()
                        {
                            do_set_filename_default = true;
                        }
                        ui.checkbox(&mut self.settings.maintenance_apply_all, "Apply to all existing");
                    });
                    ui.label("Set the global filename template default for new monitors; optionally apply it to all existing ones.");
                    ui.end_row();
                });
            if do_set_filename_default {
                let tmpl = self.settings.maintenance_filename_preset.clone();
                self.monitor_defaults.global.filename_template = Some(tmpl.clone());
                self.persist_monitor_defaults();
                if self.settings.maintenance_apply_all {
                    match self.core.store.set_all_filename_templates(&tmpl) {
                        Ok(n) => self.status = format!("Default set; updated {n} existing monitor(s)."),
                        Err(e) => self.status = format!("Error updating monitors: {e:#}"),
                    }
                } else {
                    self.status = "Default filename template updated.".into();
                }
            }
            if let Some(id) = maint_preset_delete {
                if let Err(e) = self.core.store.delete_filename_preset(id) {
                    self.status = format!("Error deleting preset: {e:#}");
                } else {
                    self.custom_presets = self.core.store.get_filename_presets().unwrap_or_default();
                }
            }
            if let Some(tmpl) = maint_preset_save_tmpl {
                self.save_preset_dialog = Some(SavePresetDraft {
                    template: tmpl,
                    name: String::new(),
                    error: String::new(),
                });
            }

            }
    }

    fn settings_diagnostics_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::System, "Diagnostics", &["diagnostics", "crash", "freeze", "dialog", "icon", "logs"]) {
            ui.add_space(12.0);

            // ── Diagnostics ────────────────────────────────────────────────────
            ui.heading("Diagnostics");
            ui.label(
                "Crash / freeze dialog icon — path to a PNG file shown as the main icon in \
                 error dialogs. Leave empty to use the standard Windows icon. Restart required \
                 to apply.",
            );
            egui::Grid::new("diag_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Dialog icon (PNG)");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.dialog_icon)
                                .hint_text("(standard icon)")
                                .desired_width(280.0),
                        )
                        .on_hover_text(
                            "Absolute path to a PNG file. Displayed as the main icon in \
                             crash and freeze dialogs. Falls back to the standard Windows \
                             icon if the file is missing or not a valid PNG.",
                        );
                        if ui.button("Browse…").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("PNG images", &["png"])
                                .pick_file()
                            {
                                self.settings.dialog_icon =
                                    path.to_string_lossy().into_owned();
                            }
                        }
                    });
                    ui.end_row();
                });
            } // end Diagnostics section guard
    }
}
