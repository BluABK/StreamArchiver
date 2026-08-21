//! Settings view and shared trigger/custom-tool editors.

use super::*;

/// Settings category tabs — the flat Settings page is grouped into these.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsTab {
    /// Credentials and authentication of every kind: detection API keys,
    /// platform OAuth accounts, WebSub, download cookies/tokens.
    Accounts,
    /// Capture-time behaviour: output defaults, monitor defaults, chat
    /// logging, ad-break detection, disk I/O limits.
    Recording,
    /// What starts, stops or fetches recordings on its own: trigger words,
    /// blacklist, follow-raid, head backfill, VOD download/recovery.
    Automation,
    /// What happens to files after capture: remux, chapters, file
    /// management, automatic deletion (incl. rolling recordings).
    PostProcessing,
    /// Download-tool plumbing: yt-dlp arguments, custom tools, SABR, the
    /// GVS PO token server.
    Downloads,
    Schedule,
    /// Channel-stats features: viewer-history retention, hype trains.
    Stats,
    Interface,
    System,
    /// Manual batch operations (re-remux all, thumbnails, reorganize, …).
    Maintenance,
}

impl SettingsTab {
    const ALL: [SettingsTab; 10] = [
        SettingsTab::Accounts,
        SettingsTab::Recording,
        SettingsTab::Automation,
        SettingsTab::PostProcessing,
        SettingsTab::Downloads,
        SettingsTab::Schedule,
        SettingsTab::Stats,
        SettingsTab::Interface,
        SettingsTab::System,
        SettingsTab::Maintenance,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            SettingsTab::Accounts => "Accounts",
            SettingsTab::Recording => "Recording",
            SettingsTab::Automation => "Automation",
            SettingsTab::PostProcessing => "Post-processing",
            SettingsTab::Downloads => "Downloads",
            SettingsTab::Schedule => "Schedule",
            SettingsTab::Stats => "Stats",
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
            SettingsTab::Automation => "automation",
            SettingsTab::PostProcessing => "postprocessing",
            SettingsTab::Downloads => "downloads",
            SettingsTab::Schedule => "schedule",
            SettingsTab::Stats => "stats",
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
/// One bound of a trigger rule's active period as an editable text field.
///
/// The rule stores a unix timestamp but the user edits text, so the
/// in-progress draft lives in egui temp memory keyed by `(salt, which, i)` —
/// a per-frame local would reset on every keystroke (see the ComboBox
/// persistence rule), and the rule structs deliberately carry no UI drafts.
/// The rule is only written on a successful parse; invalid text stays in the
/// field, tinted, changing nothing.
fn active_bound_field(
    ui: &mut egui::Ui,
    salt: &str,
    i: usize,
    which: &str,
    hint: &str,
    value: &mut Option<i64>,
) -> egui::Response {
    let id = egui::Id::new((salt, which, i));
    let mut draft: String = ui
        .data_mut(|d| d.get_temp(id))
        .unwrap_or_else(|| crate::triggers::format_active_bound(*value));
    let parsed = crate::triggers::parse_active_bound(&draft);
    let mut edit = egui::TextEdit::singleline(&mut draft).hint_text(hint).desired_width(130.0);
    if parsed.is_none() {
        edit = edit.text_color(HL_ERROR_TEXT);
    }
    let resp = ui.add(edit);
    if resp.changed() {
        if let Some(v) = crate::triggers::parse_active_bound(&draft) {
            *value = v;
        }
    }
    ui.data_mut(|d| d.insert_temp(id, draft));
    resp
}

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
            ui.push_id((salt, "label", i), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut r.label)
                        .hint_text("Label…")
                        .desired_width(120.0),
                )
            })
            .inner
            .on_hover_text(
                "Optional name for this rule (\"Deletion-flagged title\", \"Unarchived \
                 karaoke\") — shown in notifications and the ⚡ badge instead of leaving \
                 a long regex as the only identification.",
            );
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
        ui.horizontal(|ui| {
            ui.add_space(24.0); // roughly align under the row above
            ui.label("📝").on_hover_text("Note (free text, stays with the rule)");
            ui.push_id((salt, "note", i), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut r.note)
                        .hint_text("Note… (caveats, provenance — e.g. \"broad rule, watch for false positives\")")
                        .desired_width(460.0),
                )
            })
            .inner
            .on_hover_text(
                "Optional free-text note shown only here in the editor — record caveats or \
                 warnings (e.g. \"dangerously broad — blacklist per channel if it misfires\"). \
                 Not used for matching and not shown in notifications.",
            );
        });
        ui.horizontal(|ui| {
            ui.add_space(24.0); // roughly align under the row above
            let active_now = r.active_at(now_unix());
            ui.label(if active_now { "🕓" } else { "🕓💤" }).on_hover_text(if active_now {
                "Active period (optional): the rule only matches between these times — e.g. an \
                 event-scoped rule that should only fire during AGDQ/SGDQ week. Empty = no \
                 bound on that side; both empty = always active. The rule is currently IN its \
                 active period."
            } else {
                "Active period (optional) — the rule is currently OUTSIDE its active period, \
                 so it matches nothing right now. It stays here, ready for the next event, \
                 and the Schedule dry-run still previews it against each event's own time."
            });
            active_bound_field(ui, salt, i, "active_from", "From…", &mut r.active_from)
                .on_hover_text(
                    "Start of the active period, local time — \"2026-01-05 18:00\" or just \
                     \"2026-01-05\" (midnight). Empty = active since forever.",
                );
            ui.label("→");
            active_bound_field(ui, salt, i, "active_until", "Until…", &mut r.active_until)
                .on_hover_text(
                    "End of the active period (exclusive), local time — \"2026-01-12 06:00\" \
                     or just \"2026-01-12\" (midnight). Empty = never expires. For a rule \
                     with \"Only while matching\", the window closing also ends a recording \
                     it started, after the grace delay.",
                );
            if r.active_from.is_some_and(|f| r.active_until.is_some_and(|u| u <= f)) {
                ui.colored_label(HL_ERROR_TEXT, "empty window")
                    .on_hover_text("\"Until\" is not after \"From\" — this rule can never match.");
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
            ui.label("Deletion:");
            ui.push_id((salt, "disposal", i), |ui| {
                disposal_method_combo(ui, "trigger_disposal", &mut r.disposal_override)
            })
            .inner
            .on_hover_text(
                "Force the deletion method for every automatic disposal (post-join \
                 cleanup, gap-splice cleanup, superseded old head) of a recording THIS \
                 rule started — trigger words usually flag content that's easy to \
                 lose, so it can warrant stricter handling than the channel/instance \
                 is set to. Beats the channel/instance AND the all-triggers default \
                 below. Inherit = no special treatment for this rule's recordings.",
            );
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

/// The "Live" cell for a dynamic-mode disk-override row: current/ceiling for
/// both gate kinds, each a draggable value that pins a manual override on
/// change (see `io_gate::pin_dynamic_permits`), plus a 🔓 to release any
/// active pin back to the adjuster. `letter` is the row's (possibly still
/// being typed) drive-letter field.
/// Flag tokens an output-**folder** template won't expand.
///
/// A folder template is expanded once, when a channel is created, and stored
/// as a literal path — so an unsupported token doesn't fail, it becomes a
/// directory with braces in its name, and every channel that shares the
/// template lands in that same directory together. This is the only chance to
/// say so, since by the time it's visible the recordings are already there.
pub(super) fn dir_token_warning(ui: &mut egui::Ui, template: &str) {
    let bad = crate::downloader::unsupported_dir_tokens(template);
    if bad.is_empty() {
        return;
    }
    ui.colored_label(
        egui::Color32::from_rgb(200, 80, 80),
        format!("Not a folder token: {} — it would become part of the folder NAME", bad.join(" ")),
    )
    .on_hover_text(format!(
        "Folder templates expand only {}. Everything else stays literal, so this template \
         would create a directory with braces in its name and put every channel that uses \
         it in there together.",
        crate::downloader::DIR_TOKENS.join(" "),
    ));
}

fn dynamic_live_cell(ui: &mut egui::Ui, letter: &str, local_ceiling: u32, cdn_ceiling: u32) {
    let key = letter.trim().to_uppercase();
    if key.len() != 1 || !key.chars().all(|c| c.is_ascii_alphabetic()) {
        ui.weak("(drive letter?)");
        return;
    }
    let ch = key.chars().next().unwrap();
    let (pin_local, pin_cdn) = crate::io_gate::dynamic_pin_for(&key);
    let mut new_pin_local = pin_local;
    let mut new_pin_cdn = pin_cdn;
    ui.horizontal(|ui| {
        match crate::io_gate::local_dyn_status(ch) {
            Some(s) => {
                let mut v = pin_local.unwrap_or(s.current);
                let resp = ui
                    .add(egui::DragValue::new(&mut v).range(1..=local_ceiling.max(1)).prefix("L "))
                    .on_hover_text(format!(
                        "Local passes: the adjuster's LIVE permit count (not a setting). \
                         {} of {} in use, ceiling {}. Drag to pin a specific number — \
                         overrides the adjuster until cleared.",
                        s.in_use, s.current, s.ceiling
                    ));
                // Ceiling + busy count as visible text, not hover-only —
                // this cell is the live readout, and it should read like one.
                ui.weak(format!("/{} · {} busy", s.ceiling, s.in_use)).on_hover_text(format!(
                    "Ceiling {} (the configured max the adjuster grows toward) — {} of the \
                     {} live permits are running a pass right now.",
                    s.ceiling, s.in_use, s.current
                ));
                if resp.changed() {
                    new_pin_local = Some(v);
                }
            }
            None => {
                ui.weak("L —").on_hover_text("Not active yet — no local pass has run on this drive.");
            }
        }
        match crate::io_gate::cdn_dyn_status(ch) {
            Some(s) => {
                let mut v = pin_cdn.unwrap_or(s.current);
                let resp = ui
                    .add(egui::DragValue::new(&mut v).range(1..=cdn_ceiling.max(1)).prefix("C "))
                    .on_hover_text(format!(
                        "CDN muxes: the adjuster's LIVE permit count (not a setting). \
                         {} of {} in use, ceiling {}. Drag to pin a specific number — \
                         overrides the adjuster until cleared.",
                        s.in_use, s.current, s.ceiling
                    ));
                ui.weak(format!("/{} · {} busy", s.ceiling, s.in_use)).on_hover_text(format!(
                    "Ceiling {} (the configured max the adjuster grows toward) — {} of the \
                     {} live permits are running a mux right now.",
                    s.ceiling, s.in_use, s.current
                ));
                if resp.changed() {
                    new_pin_cdn = Some(v);
                }
            }
            None => {
                ui.weak("C —").on_hover_text("Not active yet — no CDN mux has run on this drive.");
            }
        }
        if (pin_local.is_some() || pin_cdn.is_some())
            && ui
                .small_button("🔓")
                .on_hover_text("Clear the manual override, resume auto-adjustment")
                .clicked()
        {
            new_pin_local = None;
            new_pin_cdn = None;
        }
    });
    if new_pin_local != pin_local || new_pin_cdn != pin_cdn {
        crate::io_gate::pin_dynamic_permits(&key, new_pin_local, new_pin_cdn);
    }
}

/// One font-family picker: a searchable dropdown of installed fonts, a live
/// preview in the chosen face, and a reset. Returns `true` when `current`
/// changed.
///
/// The reset button is not decoration — a user who picks a symbol font
/// otherwise cannot read the UI well enough to change it back — and the
/// preview is what stops them picking one by accident in the first place.
fn font_picker(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    current: &mut String,
    fonts: &[crate::fonts::SystemFont],
    hover: &str,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label).on_hover_text(hover);
        let shown = if current.is_empty() { "Default" } else { current.as_str() };
        egui::ComboBox::from_id_salt(id).selected_text(shown).width(220.0).show_ui(ui, |ui| {
            // A typical Windows box has 300+ installed faces, so: a filter,
            // and a real ScrollArea. `set_max_height` alone only caps the
            // BOX — without something to scroll inside it the list draws
            // straight past the bottom and over whatever is beneath it.
            let filter_id = egui::Id::new((id, "filter"));
            let mut filter: String = ui.data_mut(|d| d.get_temp(filter_id).unwrap_or_default());
            ui.add(
                egui::TextEdit::singleline(&mut filter)
                    .hint_text("filter…")
                    .desired_width(200.0),
            );
            let needle = filter.trim().to_lowercase();
            ui.data_mut(|d| d.insert_temp(filter_id, filter.clone()));

            if needle.is_empty()
                && ui.selectable_label(current.is_empty(), "Default").clicked()
                && !current.is_empty()
            {
                current.clear();
                changed = true;
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                let mut any = false;
                for f in fonts {
                    if !needle.is_empty() && !f.display.to_lowercase().contains(&needle) {
                        continue;
                    }
                    any = true;
                    let sel = current.eq_ignore_ascii_case(&f.display);
                    if ui.selectable_label(sel, &f.display).clicked() && !sel {
                        *current = f.display.clone();
                        changed = true;
                    }
                }
                if !any {
                    ui.weak("No font matches that.");
                }
            });
        });
        if !current.is_empty()
            && ui.button("Reset").on_hover_text("Back to the bundled default font.").clicked()
        {
            current.clear();
            changed = true;
        }
        if fonts.is_empty() {
            ui.weak("(no installed fonts found)");
        }
    });
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        // Rendered in the very family this picker controls, so what you pick
        // is what you see before committing the whole UI to it.
        let family = if id == "chat_font" {
            egui::FontFamily::Name(crate::fonts::CHAT_FAMILY.into())
        } else {
            egui::FontFamily::Proportional
        };
        ui.label(
            egui::RichText::new("The quick brown fox — 0123 — あいう")
                .font(egui::FontId::new(14.0, family))
                .weak(),
        );
    });
    changed
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
                    self.save_settings(ui.ctx());
                }
            });
        });
        if self.settings_search.trim().is_empty() {
            // Wrapped: ten tabs no longer reliably fit one row at narrow
            // window widths.
            ui.horizontal_wrapped(|ui| {
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
        // Sections render grouped by tab, in each tab's display order — only
        // one tab's group actually draws per frame (the `section_shown` gate),
        // but keeping the call order grouped means a SEARCH, which renders
        // matches across every tab, lists them in a coherent order too.
        egui::ScrollArea::vertical().show(ui, |ui| {
            // Accounts: detection keys → platform accounts → push/quota →
            // download cookies/tokens (authentication of every kind).
            self.settings_detection_credentials_section(ui);
            self.settings_twitch_account_section(ui);
            self.settings_google_account_section(ui);
            self.settings_websub_section(ui);
            self.settings_youtube_data_api_section(ui);
            self.settings_download_auth_section(ui);
            // Recording: capture-time behaviour.
            self.settings_defaults_section(ui);
            self.settings_monitor_defaults_section(ui);
            self.settings_chat_section(ui);
            self.settings_ad_probe_section(ui);
            self.settings_disk_io_section(ui);
            // Automation: what starts/stops/fetches on its own.
            self.settings_simulcast_section(ui);
            self.settings_trigger_words_section(ui);
            self.settings_blacklist_triggers_section(ui);
            self.settings_raid_follow_section(ui);
            self.settings_head_backfill_section(ui);
            self.settings_vod_download_section(ui);
            self.settings_vod_recovery_section(ui);
            // Post-processing: the file's life after capture, in the order
            // it actually happens — remux, chapters, organize, delete.
            self.settings_remux_section(ui);
            self.settings_chapters_section(ui);
            self.settings_file_management_section(ui);
            self.settings_disposal_section(ui);
            // Downloads: tool plumbing.
            self.settings_ytdlp_args_section(ui);
            self.settings_custom_tools_section(ui);
            self.settings_sabr_section(ui);
            self.settings_clips_section(ui);
            self.settings_pot_server_section(ui);
            // Schedule.
            self.settings_schedule_sources_section(ui);
            self.settings_discord_import_section(ui);
            // Stats.
            self.settings_stats_history_section(ui);
            self.settings_hype_trains_section(ui);
            // Interface.
            self.settings_display_section(ui);
            self.settings_table_columns_section(ui);
            self.settings_chat_highlights_section(ui);
            self.settings_notifications_section(ui);
            // System.
            self.settings_startup_section(ui);
            self.settings_shutdown_section(ui);
            self.settings_db_backup_section(ui);
            self.settings_chat_index_section(ui);
            self.settings_diagnostics_section(ui);
            // Maintenance (manual batch operations).
            self.settings_maintenance_section(ui);

            ui.add_space(16.0);
        });
    }

    fn settings_detection_credentials_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Accounts, "Detection credentials", &["twitch", "youtube", "kick", "client id", "secret", "api key", "credentials", "detection", "collab", "stream together", "shared chat", "eventsub", "mention"]) {
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

            ui.add_space(6.0);
            let mut collab_es = self.collab_eventsub;
            if ui
                .checkbox(&mut collab_es, "🤝 Collab updates via EventSub (conduit mode)")
                .on_hover_text(
                    "Also subscribe Twitch's shared-chat events \
                     (channel.shared_chat.begin/update/end) so \"Stream Together\" \
                     collabs show up within seconds instead of at the next poll. \
                     Only active in conduit mode (Client ID + Secret set): the \
                     direct-WebSocket fallback caps TOTAL subscriptions at cost 10, \
                     which the 3 extra types per channel would blow through. \
                     Polling keeps collabs working either way — this is just \
                     faster. Takes effect on the next EventSub (re)connect.",
                )
                .changed()
            {
                self.collab_eventsub = collab_es;
                let _ = self
                    .core
                    .store
                    .set_setting("collab_eventsub", if collab_es { "1" } else { "0" });
            }
            let mut raid_es = self.raid_eventsub;
            if ui
                .checkbox(&mut raid_es, "📈 Raids via EventSub (conduit mode)")
                .on_hover_text(
                    "Subscribe Twitch's channel.raid events (incoming AND outgoing) \
                     so raids land in the Channel Stats event history even while \
                     nothing is recording. Needs no extra scopes; only active in \
                     conduit mode (Client ID + Secret set) for the same \
                     subscription-cost reason as the collab checkbox. Incoming \
                     raids are also captured from chat while a recording with \
                     Chat log runs. Takes effect on the next EventSub (re)connect.",
                )
                .changed()
            {
                self.raid_eventsub = raid_es;
                let _ = self
                    .core
                    .store
                    .set_setting("raid_eventsub", if raid_es { "1" } else { "0" });
            }
            let mut collab_titles = self.collab_title_mentions;
            if ui
                .checkbox(&mut collab_titles, "@ Title-mention collabs")
                .on_hover_text(
                    "Also treat a `@handle` in the stream title as a (lower-confidence) \
                     collab partner, alongside confirmed Shared Chat/Stream Together \
                     partners. Never adds a channel that's already confirmed via Shared \
                     Chat or the collab group — this only fills in partners those two \
                     can't see (e.g. a collab announced in the title before Shared Chat \
                     starts). Default on.",
                )
                .changed()
            {
                self.collab_title_mentions = collab_titles;
                let _ = self.core.store.set_setting(
                    "collab_title_mentions",
                    if collab_titles { "1" } else { "0" },
                );
            }
            let mut collab_titles_in_name = self.collab_title_in_name;
            if ui
                .checkbox(&mut collab_titles_in_name, "@ Title-mention collabs in Name column")
                .on_hover_text(
                    "Also show title-mention collab partners in the Name cell's \" × Partner\" \
                     suffix (as \" × @Name\", `@`-prefixed to stay visually distinct from \
                     confirmed Shared Chat/group partners), not just in the 🤝 Collab column. \
                     Purely a display toggle — has no effect on detection. Default on.",
                )
                .changed()
            {
                self.collab_title_in_name = collab_titles_in_name;
                let _ = self.core.store.set_setting(
                    "collab_title_mentions_in_name",
                    if collab_titles_in_name { "1" } else { "0" },
                );
            }

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
                         path) and accept `--model <m> --add-dir <dir> -p <prompt>`. Default: claude. \
                         If a bare name isn't found (e.g. this app was already running when the CLI \
                         was installed, so it never saw the updated PATH), the default install \
                         location %USERPROFILE%\\.local\\bin\\<name>.exe is tried as a fallback before \
                         the call is marked failed.",
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
                    ui.small(
                        "Sending chat messages needs the newer 'user:write:chat' scope. Twitch \
                         can't widen an existing grant, so a connection made before that scope \
                         was added keeps working for everything else but must be reconnected \
                         once before the chat window will offer a send box.",
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
            if self.section_shown(SettingsTab::Recording, "Defaults", &["default", "output", "folder", "media player", "concurrent", "filename", "date", "timestamp", "chat", "logs", "sidecar"]) {
            ui.add_space(12.0);
            ui.heading("Defaults");
            egui::Grid::new("defaults_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Default output folder").on_hover_text(
                        "Seeds new instances' Output folder. Supports {name} (or its alias \
                         {channel}) and {platform}/{platform_short} as path segments, e.g. \
                         G:\\streams\\{platform}\\{name} — expanded once when the channel/\
                         instance is created (or its URL's platform changes), then stored as \
                         a fixed literal path; it does not re-expand later if you rename the \
                         channel. Only those identity tokens are supported — no \
                         {date}/{title}/etc., since a folder that silently changed meaning \
                         every time it was read would be far more surprising than a filename \
                         token that does. Anything else is flagged below rather than becoming \
                         a directory with braces in its name.",
                    );
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut self.settings.default_output_dir);
                            if ui.button("Browse…").clicked() {
                                self.pending_browse = Some(spawn_browse_folder(
                                    &self.settings.default_output_dir,
                                    |app, p| app.settings.default_output_dir = p,
                                ));
                            }
                        });
                        dir_token_warning(ui, &self.settings.default_output_dir);
                    });
                    ui.end_row();
                    ui.label("Default video download folder").on_hover_text(
                        "Seeds the Videos tab's per-platform Download defaults (and the manual \
                         Recover VOD form) — separate from Default output folder above, since \
                         on-demand video downloads aren't stream recordings. Only used to fill \
                         in a platform's output folder when it's still empty; each platform \
                         under Videos → Download defaults can be pointed elsewhere \
                         independently. Supports the same {name}/{platform}/{platform_short} \
                         tokens as Default output folder.",
                    );
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.settings.default_video_output_dir);
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_folder(
                                &self.settings.default_video_output_dir,
                                |app, p| app.settings.default_video_output_dir = p,
                            ));
                        }
                    });
                    ui.end_row();
                    ui.label("Chat logs folder (dedicated)").on_hover_text(
                        "Write chat sidecars to a dedicated folder — ideally on another, \
                         quieter drive — instead of next to the recordings, taking the \
                         constant small chat appends off the busy capture drives. The \
                         recordings' folder structure is mirrored under it with the drive \
                         letter as the top folder, so it can be re-merged by hand later: \
                         A:\\VODs\\GEEGA -> {root}\\A\\VODs\\GEEGA, and \
                         `robocopy {root}\\A\\ A:\\ /E` (one per drive folder) puts every \
                         chat log back beside its recording. Applies to all chat shapes — \
                         Twitch takes, YouTube live-chat sidecars, and chat-without-\
                         recording sessions. Renames (title in the filename), View chat, \
                         and Files-view relocation all follow it. Empty = chat logs stay \
                         next to the recordings (the default). Existing files don't move \
                         on their own — use \"Migrate chat logs\" under Maintenance.",
                    );
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.chat_log_root)
                                .hint_text(r"D:\ChatLogs"),
                        );
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_folder(
                                &self.settings.chat_log_root,
                                |app, p| app.settings.chat_log_root = p,
                            ));
                        }
                    });
                    ui.end_row();
                    ui.label("Media player path").on_hover_text(
                        "Path to the media player used by \"Play local recording (start)\" on recording rows. \
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
                    ui.label("Live-edge player title").on_hover_text(
                        "Window title for \"Play stream (live edge)\" — the URL/filename \
                         otherwise shown by default. Tokens: {channel}, {game}, \
                         {title_trimmed} (command-plug-stripped stream title), {pos} \
                         (current playback position, HH:MM:SS). {pos} ticks live in mpv \
                         (its own ${time-pos} keeps it updating). On Twitch, Streamlink \
                         opens the player and can only set a fixed title, so {pos} shows \
                         00:00:00 for the second or two before this app takes the title \
                         over via mpv's IPC socket — which needs \"Auto-update live \
                         title\" below on. Leave blank to restore the old behavior (no \
                         title override).",
                    );
                    ui.text_edit_singleline(&mut self.settings.live_title_template);
                    ui.end_row();
                    ui.label("Auto-update live title").on_hover_text(
                        "Push an updated title over mpv's IPC socket whenever this app \
                         detects the channel's title/game changed, for as long as the \
                         live-edge player stays open (default: on). Works on every \
                         launch path, including Twitch: Streamlink spawns the player \
                         there, so this asks it to hand mpv an IPC socket and then \
                         drives the title over that (also making {pos} tick, which \
                         Streamlink's own fixed title can't).\n\n\
                         It also covers windows opened for channels you DON'T track — a \
                         collab partner or a raid target. Those have no stored title or \
                         game, so theirs is fetched from the Twitch API just after the \
                         player opens (rather than before, which would delay tuning in) \
                         and then refreshed every 2 minutes — a slower cadence than a \
                         tracked channel's 20 s, because each refresh is a real API \
                         call. A closed window is noticed before the API is asked, so \
                         it never keeps spending calls on a player that's gone.\n\n\
                         Requires mpv as the configured media player; best-effort — if \
                         the socket never comes up, or the API doesn't answer, the title \
                         just stays as opened.",
                    );
                    ui.checkbox(&mut self.settings.live_title_auto_update, "");
                    ui.end_row();
                    ui.label("Dock chat to player").on_hover_text(
                        "Open the chat window automatically whenever ▷ Play \
                         stream (live edge) starts a player, docked to its side — \
                         video|chat as one unit, like the website. While docked, the \
                         pair moves, minimizes and restores together (drag either \
                         window), the chat matches the player\'s height, and quitting \
                         the player closes the chat with it; closing the chat leaves \
                         the player running. Dock or detach any chat manually with the \
                         🔗 toggle in its toolbar — that works with this \
                         off, too. The chat\'s docked width is remembered from the last \
                         time you resized it (drag its outer edge).",
                    );
                    ui.checkbox(&mut self.settings.chat_dock_on_play, "");
                    ui.end_row();
                    ui.label("Docked chat side").on_hover_text(
                        "Which side of the player a docked chat sticks to. Right is \
                         the website\'s own arrangement.",
                    );
                    egui::ComboBox::from_id_salt("chat_dock_side_combo")
                        .selected_text(if self.settings.chat_dock_side == "left" {
                            "Left of the player"
                        } else {
                            "Right of the player"
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.settings.chat_dock_side,
                                "right".to_string(),
                                "Right of the player",
                            );
                            ui.selectable_value(
                                &mut self.settings.chat_dock_side,
                                "left".to_string(),
                                "Left of the player",
                            );
                        });
                    ui.end_row();
                    ui.label("Mute collab instances").on_hover_text(
                        "Mute every OTHER angle opened by \"Play all collab instances \
                         (live edge)\" — the instance you actually right-clicked keeps its \
                         normal audio. Avoids several stream's worth of audio all playing \
                         at once. mpv only.",
                    );
                    ui.checkbox(&mut self.settings.mute_collab_instances, "");
                    ui.end_row();
                    ui.label("Untracked collab partner title").on_hover_text(
                        "Window title for a collab partner that ISN'T a channel you track \
                         (played via a synthetic instance borrowing this row's own tool/\
                         quality/auth settings) — kept separate from \"Live-edge player \
                         title\" above so these windows can be labelled differently. \
                         {game} and {title_trimmed} DO resolve here: with \"Auto-update \
                         live title\" on, the partner's title/game is fetched from the \
                         Twitch API just after the player opens (never before — tuning \
                         in is never held up by the API), pushed into the window over \
                         mpv's IPC socket, and refreshed every 2 minutes. Without that \
                         setting, or in a non-mpv player, only {channel} is filled in. \
                         Leave blank to fall back to the same template as a tracked \
                         instance.",
                    );
                    ui.text_edit_singleline(&mut self.settings.collab_untracked_title_template);
                    ui.end_row();
                    ui.label("Max concurrent downloads");
                    ui.text_edit_singleline(&mut self.settings.max_concurrent_downloads);
                    ui.end_row();
                    ui.label("Download rate limit").on_hover_text(
                        "yt-dlp --limit-rate for VOD-archive grabs and Videos-tab \
                         downloads. Moved to Settings → Recording → \"Disk I/O \
                         limits\" — configurable per target drive (default row = \
                         the old global value).",
                    );
                    ui.label(
                        egui::RichText::new("see Recording → Disk I/O limits").weak(),
                    );
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

                    ui.label("Filename token style").on_hover_text(
                        "Casing for the machine-value tokens {vcodec} {acodec} \
                         {platform} {platform_short} {tool} {mode}: \"As reported\" \
                         keeps the tools' lowercase values (h264, aac, twitch); \
                         \"Branded\" uses proper trademark/spec casing (H.264, AAC, \
                         Twitch, YouTube, SABR). Applies on Save to NEW names — \
                         existing files are never renamed.",
                    );
                    egui::ComboBox::from_id_salt("token_style_cb")
                        .selected_text(if self.settings.token_style_branded {
                            "Branded (H.264, AAC, YouTube)"
                        } else {
                            "As reported (h264, aac, youtube)"
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.settings.token_style_branded,
                                false,
                                "As reported (h264, aac, youtube)",
                            );
                            ui.selectable_value(
                                &mut self.settings.token_style_branded,
                                true,
                                "Branded (H.264, AAC, YouTube)",
                            );
                        });
                    ui.end_row();

                    ui.label("Token text overrides").on_hover_text(
                        "Custom text for individual token values, one per line: \
                         `value=Text` (matches that value in any token, \
                         case-insensitive) or `kind:value=Text` for one token only \
                         — e.g. `aac=AAC`, `h264=x264`, \
                         `platform_short:youtube=YT2`. Overrides win over the \
                         Branded style. Kinds: vcodec, acodec, platform, \
                         platform_short, tool, mode.",
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut self.settings.token_overrides)
                            .desired_rows(2)
                            .desired_width(300.0)
                            .hint_text("aac=AAC\nplatform_short:youtube=YT"),
                    );
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

                    ui.label("Default Schedule view").on_hover_text(
                        "Which calendar granularity the Schedule tab opens to. \
                         Applies on Save.",
                    );
                    let sm = &mut self.settings.schedule_default_view;
                    egui::ComboBox::from_id_salt("schedule_default_view_cb")
                        .selected_text(sm.label())
                        .show_ui(ui, |ui| {
                            for m in ScheduleMode::ALL {
                                ui.selectable_value(sm, m, m.label());
                            }
                        });
                    ui.end_row();
                });

            }
    }

    fn settings_display_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Interface, "Display", &["display", "actions", "emotes", "animate", "columns", "theme", "icon", "app icon", "tray", "branding", "unknown emotes", "cdn", "cross-channel"]) {
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

            // Enumerated once, lazily — this is the only place either picker
            // is drawn, and it's ~400 registry values plus an existence check
            // each, which is cheap but not per-frame cheap.
            let fonts = self
                .system_fonts
                .get_or_insert_with(crate::fonts::enumerate_system_fonts)
                .clone();
            let mut app_font = self.app_font.clone();
            if font_picker(
                ui,
                "app_font",
                "App font",
                &mut app_font,
                &fonts,
                "Font family for the app's own interface. Your pick goes in FRONT of egui's \
                 bundled default rather than replacing it, so the toolbar/button icon glyphs \
                 keep working, and the system CJK/emoji fallbacks still cover anything the \
                 font lacks. Note that a font collection (.ttc) loads its first face, so \
                 picking a specific weight of a collected family may give you the regular \
                 one. Applies immediately.",
            ) {
                self.app_font = app_font.clone();
                let _ = self.core.store.set_setting(K_APP_FONT_FAMILY, &app_font);
            }

            {
                let mut cs = self.chat_settings.lock().unwrap();
                if ui
                    .checkbox(&mut cs.render_emotes, "Render emotes in chat")
                    .on_hover_text(
                        "Show Twitch / BTTV / FFZ / 7TV emotes (and color emoji) as inline \
                         images in the chat replay. Off shows the emote code as plain text. \
                         Local/cross-channel-cached images need each channel's own \"Fetch chat \
                         assets\"; \"Fetch unknown emotes from Twitch\" below fetches anything \
                         still missing directly from Twitch's CDN. Applies immediately.",
                    )
                    .changed()
                {
                    let _ = self.core.store.set_setting(
                        K_RENDER_EMOTES,
                        if cs.render_emotes { "1" } else { "0" },
                    );
                }
                if ui
                    .add_enabled(
                        cs.render_emotes,
                        egui::Checkbox::new(&mut cs.fetch_unknown_emotes, "Fetch unknown emotes from Twitch"),
                    )
                    .on_hover_text(
                        "A chat message can use ANY Twitch subscriber emote, not just the ones \
                         belonging to a channel this app monitors — Twitch lets any subscriber \
                         use their sub emotes in any chat. When an emote id isn't cached under \
                         any monitored channel, fetch it directly from Twitch's public CDN by id \
                         (no login needed) into a shared cache, so it still renders 1:1 instead of \
                         showing as plain text. Off = only locally-cached emotes render. Applies \
                         immediately.",
                    )
                    .changed()
                {
                    let _ = self.core.store.set_setting(
                        K_FETCH_UNKNOWN_EMOTES,
                        if cs.fetch_unknown_emotes { "1" } else { "0" },
                    );
                }
                if ui
                    .checkbox(&mut cs.fetch_usercard_info, "Fetch live Twitch info for chat usercards")
                    .on_hover_text(
                        "When you click a username in the chat replay, also fetch their avatar \
                         and Twitch account-created date from Twitch's API — on top of the badges/ \
                         color/subscriber-months/message-count that always show from the local log. \
                         Off by default: unlike other asset fetches, this hits the network every \
                         time a usercard is opened, not just once per missing file. A failed lookup \
                         shows \"N/A\" and files a warning in the 🚨 Warnings window instead of \
                         blocking the rest of the card.",
                    )
                    .changed()
                {
                    let _ = self.core.store.set_setting(
                        K_FETCH_USERCARD_INFO,
                        if cs.fetch_usercard_info { "1" } else { "0" },
                    );
                }
                if ui
                    .add_enabled(
                        cs.render_emotes,
                        egui::Checkbox::new(&mut cs.animate_emotes, "Animate emotes"),
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
                        if cs.animate_emotes { "1" } else { "0" },
                    );
                }
                if ui
                    .checkbox(&mut cs.gigantify_enabled, "Click an emote to Gigantify it")
                    .on_hover_text(
                        "Click any emote in chat to show it much larger, right there in the \
                         row — a local echo of Twitch's Bits-powered Gigantify effect. Real \
                         Gigantify events aren't recoverable from an archived log (Twitch only \
                         signals them over the newer EventSub API, which the anonymous IRC \
                         capture here never receives), so this doesn't know which messages were \
                         actually gigantified live — it just lets you make any one big \
                         yourself. Click again to shrink it back. Applies immediately.",
                    )
                    .changed()
                {
                    let _ = self.core.store.set_setting(
                        K_CHAT_GIGANTIFY,
                        if cs.gigantify_enabled { "1" } else { "0" },
                    );
                }
                // Chat font lives beside the other chat settings so both this
                // dialog and each window's ⚙ panel see the same shared state.
                let mut chat_font = cs.chat_font.clone();
                if font_picker(
                    ui,
                    "chat_font",
                    "Chat font",
                    &mut chat_font,
                    &fonts,
                    "Font family for the chat replay. The chat renders in its own registered \
                     family, so this and the app font are independent, and either one still \
                     falls back to the system CJK/emoji fonts for glyphs it lacks. The \
                     bracketed stream-relative timestamp stays monospace regardless — it's a \
                     column, and a proportional face destroys the alignment. Applies \
                     immediately.",
                ) {
                    cs.chat_font = chat_font.clone();
                    let _ = self.core.store.set_setting(K_CHAT_FONT_FAMILY, &chat_font);
                }
                if ui
                    .checkbox(&mut cs.render_paints, "7TV gradient usernames")
                    .on_hover_text(
                        "Render a chatter's 7TV \"paint\" — the gradient some people have on                          their name. Approximated: egui colours text a run at a time, so a                          gradient is quantized into a few flat runs, only its horizontal                          component is expressible (a vertical gradient shows as one colour                          rather than pointing the wrong way), and animated paints render                          static. Fetched once per channel per day and cached. Applies to                          newly-opened chat windows.",
                    )
                    .changed()
                {
                    let _ = self.core.store.set_setting(
                        crate::cosmetics::K_RENDER_PAINTS,
                        if cs.render_paints { "1" } else { "0" },
                    );
                }
                if ui
                    .checkbox(&mut cs.show_hype_train, "Hype Train card in chat")
                    .on_hover_text(
                        "Show a broadcast's Hype Train above the chat log — a live progress bar \
                         while it runs, then \"ended\" for a few minutes, and a reached-level \
                         summary on archived takes. Off hides it everywhere; the 🚂 button on \
                         each chat window's toolbar collapses it in just that window (a new \
                         train re-opens that one, but never this). Applies immediately.",
                    )
                    .changed()
                {
                    let _ = self
                        .core
                        .store
                        .set_setting(K_CHAT_SHOW_HYPE, if cs.show_hype_train { "1" } else { "0" });
                }
                ui.horizontal(|ui| {
                    ui.label("Default chat timestamps").on_hover_text(
                        "What a chat window shows until you tell that one otherwise. The 🕒 \
                         button on each window's toolbar overrides it for that instance only; \
                         setting an instance back to this value clears its override, so it \
                         follows this default again if you change it later.",
                    );
                    let mut mode = cs.ts_mode;
                    egui::ComboBox::from_id_salt("chat_ts_mode_default")
                        .selected_text(match mode {
                            ChatTsMode::StreamRelative => "Time into the broadcast",
                            ChatTsMode::WallClock => "Wall-clock time",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut mode,
                                ChatTsMode::StreamRelative,
                                "Time into the broadcast",
                            )
                            .on_hover_text("[00:40:10] — what you need to seek a recording.");
                            ui.selectable_value(&mut mode, ChatTsMode::WallClock, "Wall-clock time")
                                .on_hover_text("19:30 — as Twitch's own chat shows.");
                        });
                    if mode != cs.ts_mode {
                        cs.ts_mode = mode;
                        let _ = self.core.store.set_setting(K_CHAT_TS_MODE, mode.as_str());
                    }
                });
                // Goal bar colour: a picker, a hex field, and "use the
                // channel's own colour". Twitch's own red is loud by design
                // on a live page; in a window open for hours it grates.
                ui.horizontal(|ui| {
                    let mut use_channel = cs.goal_color == GoalColor::Channel;
                    let mut fixed = match cs.goal_color {
                        GoalColor::Fixed(c) => c,
                        GoalColor::Channel => GOAL_COLOR,
                    };
                    ui.label("Goal bar colour").on_hover_text(
                        "Fill colour for the Creator Goal bar above the chat. Twitch's own red                          is tuned to catch the eye on a live page; in a chat window open for                          hours it reads as harsh, so the default here is a muted version of it.",
                    );
                    // Same idiom as the chat window's own colour rows.
                    let mut changed = ui
                        .add_enabled_ui(!use_channel, |ui| {
                            egui::color_picker::color_edit_button_srgba(
                                ui,
                                &mut fixed,
                                egui::color_picker::Alpha::Opaque,
                            )
                            .changed()
                        })
                        .inner;
                    let mut hex = hex_color_string(fixed);
                    if ui
                        .add_enabled(
                            !use_channel,
                            egui::TextEdit::singleline(&mut hex).desired_width(80.0),
                        )
                        .on_hover_text("#RRGGBB — type or paste.")
                        .changed()
                        && let Some(c) = parse_chat_hex_color(&hex)
                    {
                        fixed = c;
                        changed = true;
                    }
                    changed |= ui
                        .checkbox(&mut use_channel, "Use channel colour")
                        .on_hover_text(
                            "Fill the bar with the channel's own display colour — the same one                              the Streams grid and the notifications feed give it, so a channel                              reads consistently everywhere.",
                        )
                        .changed();
                    if changed {
                        cs.goal_color =
                            if use_channel { GoalColor::Channel } else { GoalColor::Fixed(fixed) };
                        let _ = self
                            .core
                            .store
                            .set_setting(K_CHAT_GOAL_COLOR, &cs.goal_color.as_setting());
                    }
                });
                // Send button colour: same "fixed / channel" idiom as the
                // goal bar just above.
                ui.horizontal(|ui| {
                    let mut use_channel = cs.send_button_color == SendButtonColor::Channel;
                    let mut fixed = match cs.send_button_color {
                        SendButtonColor::Fixed(c) => c,
                        SendButtonColor::Channel => SEND_BUTTON_COLOR,
                    };
                    ui.label("Send button colour").on_hover_text(
                        "Fill colour for the chat window's Send button. Defaults to Twitch's own                          send-button purple.",
                    );
                    let mut changed = ui
                        .add_enabled_ui(!use_channel, |ui| {
                            egui::color_picker::color_edit_button_srgba(
                                ui,
                                &mut fixed,
                                egui::color_picker::Alpha::Opaque,
                            )
                            .changed()
                        })
                        .inner;
                    let mut hex = hex_color_string(fixed);
                    if ui
                        .add_enabled(
                            !use_channel,
                            egui::TextEdit::singleline(&mut hex).desired_width(80.0),
                        )
                        .on_hover_text("#RRGGBB — type or paste.")
                        .changed()
                        && let Some(c) = parse_chat_hex_color(&hex)
                    {
                        fixed = c;
                        changed = true;
                    }
                    changed |= ui
                        .checkbox(&mut use_channel, "Use channel colour")
                        .on_hover_text(
                            "Fill the Send button with the channel's own display colour instead                              of a fixed one — the same option the goal bar above has.",
                        )
                        .changed();
                    if changed {
                        cs.send_button_color = if use_channel {
                            SendButtonColor::Channel
                        } else {
                            SendButtonColor::Fixed(fixed)
                        };
                        let _ = self.core.store.set_setting(
                            K_CHAT_SEND_BUTTON_COLOR,
                            &cs.send_button_color.as_setting(),
                        );
                    }
                });
                if ui
                    .checkbox(&mut cs.show_channel_info, "Channel info card in chat")
                    .on_hover_text(
                        "Show this broadcast's top supporters (gift subs and bits) above the \
                         chat log. Reconstructed from locally recorded chat events, so it won't \
                         match Twitch's own carousel exactly. Off hides it everywhere; the 🎁 \
                         toolbar button collapses it per window. Applies immediately.",
                    )
                    .changed()
                {
                    let _ = self.core.store.set_setting(
                        K_CHAT_SHOW_INFO,
                        if cs.show_channel_info { "1" } else { "0" },
                    );
                }
            }
            ui.label(
                egui::RichText::new(
                    "Color emoji use Twemoji (© Twitter/jdecked, CC-BY 4.0), downloaded on \
                     demand and cached for offline replay.",
                )
                .small()
                .weak(),
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Preferred platform when multiple instances are live:");
                let cur = self.primary_platform_pref;
                egui::ComboBox::from_id_salt("primary_platform_pref")
                    .selected_text(cur.map(Platform::label).unwrap_or("Earliest live wins"))
                    .show_ui(ui, |ui| {
                        let mut pick = |ui: &mut egui::Ui, val: Option<Platform>, label: &str| {
                            if ui.selectable_label(cur == val, label).clicked() && cur != val {
                                self.primary_platform_pref = val;
                                let _ = crate::platform_pref::set_global_primary_platform(
                                    &self.core.store, val,
                                );
                                self.streams_cache_rev = self.streams_cache_rev.wrapping_add(1);
                            }
                        };
                        pick(ui, None, "Earliest live wins (default)");
                        pick(ui, Some(Platform::Twitch), Platform::Twitch.label());
                        pick(ui, Some(Platform::YouTube), Platform::YouTube.label());
                        pick(ui, Some(Platform::Kick), Platform::Kick.label());
                    });
            })
            .response
            .on_hover_text(
                "When a channel has more than one instance (e.g. Twitch + YouTube) live at \
                 the same time, the channel row's Title/Game/Viewers/Went Live normally come \
                 from whichever instance went live earliest. Set a preferred platform to show \
                 that one's info instead whenever it's live — useful when one platform's \
                 metadata is richer (e.g. Twitch's game/category). Can be overridden per \
                 channel in Properties, or per instance with a pin (highest priority). \
                 DISPLAY only: it never changes which instance gets recorded — that's \
                 Settings → Automation → Simulcast dedup.",
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("App icon").on_hover_text(
                    "Path to an image (PNG/JPEG/WebP/GIF/ICO) replacing the built-in app \
                     icon everywhere it appears at runtime: the window title bar, the \
                     taskbar, the tray, and the attribution icon on desktop toasts. Square \
                     images of 64px or larger look best (downscaled to fit 256px; the tray \
                     gets a crisp 32px render). Empty = the built-in purple record-dot \
                     icon. Applies on Save — no restart needed. Does NOT change the exe's \
                     icon in Explorer, nor the crash/freeze dialog icon (that one is \
                     Settings → System → Diagnostics). If the file can't be read or \
                     decoded, the built-in icon is used and a warning is logged.",
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.app_icon)
                        .hint_text(r"C:\path\to\icon.png")
                        .desired_width(360.0),
                );
                if ui.button("Browse…").clicked() {
                    self.pending_browse = Some(spawn_browse_file_filtered(
                        &self.settings.app_icon,
                        ("Images", &["png", "jpg", "jpeg", "webp", "gif", "ico"]),
                        |app, p| app.settings.app_icon = p,
                    ));
                }
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button("📌 Add to Start Menu")
                    .on_hover_text(
                        "Create (or repair) a Start Menu shortcut to this exact binary —                          %APPDATA%\\…\\Start Menu\\Programs\\StreamArchiver.lnk, working                          directory set to the exe's folder. Purely a launcher: toasts and                          the taskbar identity never depended on a shortcut (the app                          registers its own AppUserModelID at startup). Safe to click again                          after moving or rebuilding the exe — the shortcut is overwritten                          to point at wherever the app is running from right now.",
                    )
                    .clicked()
                {
                    match crate::platform::create_start_menu_shortcut() {
                        Ok(p) => self.status = format!("Start Menu shortcut created: {}", p.display()),
                        Err(e) => self.status = format!("Start Menu shortcut failed: {e}"),
                    }
                }
            });

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
                let all_columns: [(GridTableId, &[GridCol]); 8] = [
                    (GridTableId::Streams, &STREAM_COLUMNS),
                    (GridTableId::Videos, &VIDEO_COLUMNS),
                    (GridTableId::BgActive, &BG_ACTIVE_COLUMNS),
                    (GridTableId::BgRecent, &BG_RECENT_COLUMNS),
                    (GridTableId::Processes, &PROCESSES_COLUMNS),
                    (GridTableId::Issues, &ISSUES_COLUMNS),
                    (GridTableId::Backlog, &BACKLOG_COLUMNS),
                    (GridTableId::Clips, &CLIP_COLUMNS),
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
            if self.section_shown(SettingsTab::Accounts, "Download authentication", &["download", "auth", "cookies", "browser", "token", "profile", "login"]) {
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

    fn settings_clips_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "Clips 🎞", &["clip", "clips", "twitch clips", "download clips"]) {
            ui.add_space(12.0);
            ui.heading("Clips 🎞");
            ui.label(
                "Indexing every clip of your monitored channels costs tens of megabytes for \
                 the whole archive, and is what preserves the keys that let a deleted clip be \
                 rebuilt later. Downloading the media is a different order of magnitude — a \
                 busy channel accumulates 7,500–12,000 clips, roughly 200 GB — so it is off \
                 until you turn it on, per channel.",
            );
            // These write straight to the store rather than through
            // `SettingsForm`: both are switches whose effect is immediate and
            // which the sweep loop re-reads every pass, so round-tripping them
            // through the form's save button would only add a way to forget.
            let store = &self.core.store;
            let mut index_on = crate::clips::clips_enabled(store);
            if ui
                .checkbox(&mut index_on, "Index clips (metadata only)")
                .on_hover_text(
                    "Catalogue every clip of your monitored channels. Cheap, and the only way \
                     to capture a clip's recovery keys — Twitch reports which VOD a clip came \
                     from only while that VOD still exists (100% of clips under two weeks old, \
                     5% at a year), so a clip indexed late can never be rebuilt.",
                )
                .changed()
            {
                let _ = store.set_setting(crate::clips::K_CLIPS_ENABLED, if index_on { "1" } else { "0" });
            }
            let mut dl_on = crate::clips::download_master_on(store);
            if ui
                .checkbox(&mut dl_on, "Download clip media (master switch)")
                .on_hover_text(
                    "Master switch for downloading. Each channel ALSO has to be enabled \
                     below — the two are independent switches rather than an inherit chain, \
                     because there is no sensible global default under a ~200 GB-per-channel \
                     decision.",
                )
                .changed()
            {
                let _ = store.set_setting(crate::clips::K_CLIPS_DOWNLOAD, if dl_on { "1" } else { "0" });
            }
            let mut auto_rec = store
                .get_setting("clips_auto_recover")
                .ok()
                .flatten()
                .as_deref()
                .is_none_or(|v| v != "0");
            if ui
                .checkbox(&mut auto_rec, "Try to rebuild clips that vanish")
                .on_hover_text(
                    "When a sweep finds a clip has been deleted upstream, attempt one rebuild \
                     immediately — from the parent VOD's CDN segments, or by cutting it out of \
                     our own recording of that broadcast. One attempt only; after that it's the \
                     row's right-click menu. On by default because the window in which a \
                     rebuild can still succeed closes when the parent VOD expires.",
                )
                .changed()
            {
                let _ = store.set_setting("clips_auto_recover", if auto_rec { "1" } else { "0" });
            }
            let mut backfill = matches!(
                store.get_setting(crate::clips::K_CLIPS_BACKFILL).ok().flatten().as_deref(),
                Some("1") | Some("true")
            );
            if ui
                .checkbox(&mut backfill, "Backfill each channel's full clip history")
                .on_hover_text(
                    "Walk every channel's history a month at a time to get past Twitch's \
                     ~1000-result cap — one channel measured 1,100 clips reachable normally \
                     versus 7,588 with windowing. Thousands of requests per channel, paced one \
                     window per visit over days, so it is off by default. The catalogue grows; \
                     whether the media is downloaded still depends on the switches above.",
                )
                .changed()
            {
                let _ = store.set_setting(
                    crate::clips::K_CLIPS_BACKFILL,
                    if backfill { "1" } else { "0" },
                );
            }
            ui.add_enabled_ui(dl_on, |ui| {
                ui.indent("clips_channels", |ui| {
                    ui.label(
                        egui::RichText::new("Download clips for these channels:")
                            .small()
                            .weak(),
                    );
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .id_salt("clips_channel_gates")
                        .show(ui, |ui| {
                            for c in &self.channels {
                                let mut on = crate::raid_follow::load_bool_scope(
                                    store,
                                    crate::clips::K_CHANNEL_CLIPS_DOWNLOAD,
                                    c.id,
                                )
                                .unwrap_or(false);
                                if ui
                                    .checkbox(&mut on, &c.name)
                                    .on_hover_text(
                                        "Download every clip of this channel as it is \
                                         discovered. The catalogue is kept either way.",
                                    )
                                    .changed()
                                {
                                    let _ = crate::raid_follow::save_bool_scope(
                                        store,
                                        crate::clips::K_CHANNEL_CLIPS_DOWNLOAD,
                                        c.id,
                                        Some(on).filter(|v| *v),
                                    );
                                }
                            }
                        });
                });
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

                    ui.label("📺 Capture public streams via tv client");
                    ui.checkbox(&mut self.settings.sabr_tv_primary, "").on_hover_text(
                        "Capture PUBLIC YouTube broadcasts via yt-dlp's 'tv' (TVHTML5) \
                         client, which has no GVS PO-token policy at all — the \
                         ATTESTATION_REQUIRED rejection waves that kill 'web' takes \
                         can't touch it (same formats, full-speed from-start, verified \
                         live). Members-only broadcasts always capture via 'web' + \
                         account cookies regardless, since entitlement lives on the \
                         account. When off, the preset's 'web' client is the primary \
                         and the PO-rejection fallback below carries the waves. \
                         Hand-written extractor-args override this for public streams.",
                    );
                    ui.end_row();

                    ui.label("🕶 Anonymous as a last resort");
                    ui.checkbox(&mut self.settings.yt_anon_public, "").on_hover_text(
                        "After three YouTube captures in a row fail WITH account cookies \
                         and nothing is captured, allow ONE attempt without them — on the \
                         chance that whatever is refusing the account will not refuse a \
                         stranger. Cookies are the normal path.\n\n\
                         It used to be the other way round: public YouTube always captured \
                         anonymously, to keep the account out of YouTube attestation \
                         experiments. That held until 2026-08-18, when YouTube began \
                         refusing every anonymous request from this network — both clients, \
                         measured — so anonymous-first meant capturing nothing at all.\n\n\
                         Skipped when the failures ARE the anonymous bot check (Sign in to \
                         confirm you are not a bot): that is a refusal of anonymity, so the \
                         attempt cannot help. Note that a --cookies-from-browser line in \
                         the yt-dlp user config applies to every run behind the app back — \
                         keep cookies out of that file and let the app attach them.",
                    );
                    ui.end_row();

                    ui.label("🎫 PO-rejection fallback (tv client)");
                    ui.checkbox(&mut self.settings.sabr_po_fallback, "").on_hover_text(
                        "When a take dies because YouTube rejected its GVS PO token \
                         (ATTESTATION_REQUIRED), retry promptly via yt-dlp's 'tv' client, \
                         which doesn't use PO tokens at all — rejection waves can't touch \
                         it. Mostly relevant when the tv-primary switch above is off (or \
                         for members-only captures, which run via 'web'): the fallback \
                         applies per-retry and the next successful capture returns to \
                         normal. When off, rejected takes instead wait out the escalating \
                         5-15 minute cooldown before retrying (footage at the live edge \
                         is lost for the wait's duration).",
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

    fn settings_pot_server_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Downloads, "GVS PO token server", &["pot", "po token", "gvs", "bgutil", "server", "node", "token"]) {
            ui.add_space(12.0);
            ui.heading("GVS PO token server 🎫");
            ui.label(
                "YouTube SABR captures die mid-stream without a GVS PO token, and the \
                 tokens come from the bgutil provider's local HTTP server. This manages \
                 that server so it's never silently missing: launched at startup, \
                 health-checked every 30s, restarted if it crashes, and started on \
                 demand when a capture fails for lack of a token. A server started \
                 outside the app is detected and used as-is (never killed).",
            );
            ui.add_space(4.0);
            // Live status + controls. Keep the section ticking while visible so
            // the status line tracks the watchdog without user interaction.
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
            let st = crate::pot_server::status();
            let ping_suffix = |p: &Option<crate::pot_server::PingInfo>| {
                p.as_ref()
                    .map(|p| {
                        let s = p.uptime_secs as u64;
                        format!(" · v{} · up {}:{:02}:{:02}", p.version, s / 3600, (s % 3600) / 60, s % 60)
                    })
                    .unwrap_or_default()
            };
            use crate::pot_server::PotMode;
            let (text, color, hover) = match &st.mode {
                PotMode::Managed { pid } => (
                    format!("● running (managed) · pid {pid}{}", ping_suffix(&st.last_ping)),
                    egui::Color32::from_rgb(0x39, 0xb0, 0x54),
                    "The app spawned this server and supervises it: if it crashes, the \
                     watchdog restarts it and posts a 🔔 notification. Uptime/version come \
                     from the server's own /ping endpoint."
                        .to_string(),
                ),
                PotMode::External => (
                    format!("● running (external){}", ping_suffix(&st.last_ping)),
                    egui::Color32::from_rgb(0x39, 0xb0, 0x54),
                    "A server someone else started is answering on the configured port. \
                     The app uses it as-is and won't restart it if it dies. To manage \
                     it anyway: ⏹ Stop external kills it, ⚡ Take control replaces it \
                     with an app-managed instance (watchdog, restarts, Stop button)."
                        .to_string(),
                ),
                PotMode::Starting => (
                    "◐ starting…".to_string(),
                    egui::Color32::from_rgb(0xd9, 0xa4, 0x06),
                    "Spawn issued; waiting up to 15s for the server's /ping to answer."
                        .to_string(),
                ),
                PotMode::Down if st.desired == crate::pot_server::Desired::ForcedOff => (
                    "○ stopped (by you)".to_string(),
                    egui::Color32::GRAY,
                    "You clicked Stop, so the watchdog won't restart it this session — \
                     click Start to bring it back (or restart the app)."
                        .to_string(),
                ),
                PotMode::Down => (
                    "○ down".to_string(),
                    egui::Color32::from_rgb(0xd9, 0x53, 0x4f),
                    "Not currently reachable; the watchdog will start it on its next check \
                     (within ~30s, sooner if a capture needs it)."
                        .to_string(),
                ),
                PotMode::Disabled => (
                    "○ not managed".to_string(),
                    egui::Color32::GRAY,
                    "Auto-launch is off and no server is being managed. SABR captures will \
                     still try the configured base URL — if nothing listens there, they \
                     fail with PO-token errors (a capture failure will still start the \
                     server on demand unless you stopped it explicitly)."
                        .to_string(),
                ),
                PotMode::Failed { reason } => (
                    format!("✗ failed: {reason}"),
                    egui::Color32::from_rgb(0xd9, 0x53, 0x4f),
                    "The last launch attempt failed (details in the server log and the \
                     app log). The watchdog retries with growing backoff; fix the \
                     directory/node settings below if they're wrong."
                        .to_string(),
                ),
            };
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(text).color(color)).on_hover_text(hover);
                ui.weak(format!("({})", st.base_url)).on_hover_text(
                    "The base URL the server is pinged/launched on — parsed from the \
                     'PO token extractor-args' setting above so the managed server and \
                     yt-dlp always agree on the port.",
                );
            });
            ui.horizontal(|ui| {
                let can_start = matches!(
                    st.mode,
                    PotMode::Down | PotMode::Disabled | PotMode::Failed { .. }
                );
                let can_stop = matches!(st.mode, PotMode::Managed { .. } | PotMode::Starting);
                if ui
                    .add_enabled(can_start, egui::Button::new("▶ Start"))
                    .on_hover_text(
                        "Start the server now and keep it running for this session, even \
                         with Auto-launch off. Takes effect within a second or two.",
                    )
                    .clicked()
                {
                    crate::pot_server::request_start();
                }
                if ui
                    .add_enabled(can_stop, egui::Button::new("⏹ Stop"))
                    .on_hover_text(
                        "Stop the managed server and keep it stopped for this session \
                         (the watchdog won't restart it until Start is clicked or the \
                         app restarts). For an external server use ⏹ Stop external / \
                         ⚡ Take control instead.",
                    )
                    .clicked()
                {
                    crate::pot_server::request_stop();
                }
                if st.mode == PotMode::External {
                    if ui
                        .button("⏹ Stop external")
                        .on_hover_text(
                            "Find the process listening on the configured port and kill \
                             it, then stay stopped for this session (Start brings up a \
                             managed instance instead). Caveat: for a server inside \
                             Docker/WSL the port is owned by the Docker/WSL proxy \
                             process — stop the container yourself instead.",
                        )
                        .clicked()
                    {
                        self.status = match crate::pot_server::stop_external() {
                            Ok(pid) => format!("External PO token server (pid {pid}) stopped."),
                            Err(e) => format!("Stop external failed: {e}"),
                        };
                    }
                    if ui
                        .button("⚡ Take control")
                        .on_hover_text(
                            "Kill the external server and immediately start an \
                             app-managed instance on the same port — from then on the \
                             watchdog supervises it (crash restarts, Stop button, pid \
                             re-adoption across app runs). Same Docker/WSL caveat as \
                             Stop external.",
                        )
                        .clicked()
                    {
                        self.status = match crate::pot_server::take_control() {
                            Ok(pid) => format!(
                                "Took control: external server (pid {pid}) replaced by a managed instance."
                            ),
                            Err(e) => format!("Take control failed: {e}"),
                        };
                    }
                }
                if ui
                    .button("📜 View log")
                    .on_hover_text(
                        "Open a live tail of the server's combined stdout+stderr — \
                         startup lines, token generations, and crash reasons land here.",
                    )
                    .clicked()
                {
                    self.show_pot_server_log = true;
                }
                if ui
                    .button("📂 Open log file")
                    .on_hover_text(format!(
                        "Open {} in the default viewer. Truncated at the first launch \
                         of each app run; restarts within a run append.",
                        crate::pot_server::log_path().display()
                    ))
                    .clicked()
                {
                    crate::platform::open_path(&crate::pot_server::log_path());
                }
            });
            ui.add_space(4.0);
            egui::Grid::new("pot_server_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Auto-launch at startup");
                    ui.checkbox(&mut self.settings.pot_server_autostart, "").on_hover_text(
                        "Launch the server when the app starts (skipped if one is already \
                         answering on the port) and restart it whenever it goes down. \
                         Off = only manual Start / on-demand starts triggered by a \
                         capture failing for lack of a token.",
                    );
                    ui.end_row();

                    ui.label("Server directory");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.pot_server_dir)
                                .hint_text(crate::pot_server::DEFAULT_SERVER_DIR)
                                .desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_folder(
                                &self.settings.pot_server_dir,
                                |app, p| app.settings.pot_server_dir = p,
                            ));
                        }
                    })
                    .response
                    .on_hover_text(
                        "The bgutil-ytdlp-pot-provider server's BUILD directory (the one \
                         containing main.js — usually server\\build in the clone, after \
                         `npx tsc`). Empty = the default path shown.",
                    );
                    ui.end_row();

                    ui.label("Node binary");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.pot_server_node)
                                .hint_text("(empty = node on PATH)")
                                .desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_file(
                                &self.settings.pot_server_node,
                                |app, p| app.settings.pot_server_node = p,
                            ));
                        }
                    })
                    .response
                    .on_hover_text(
                        "Node.js runs the server (needs Node ≥ 20). Leave empty to use \
                         `node` from PATH, or point at a specific node.exe.",
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
                ("  NRK",      Some(Platform::Nrk)),
                ("  Nebula",   Some(Platform::Nebula)),
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
                                DetectionMethod::WebSubSlow,
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
                                        "Tokens: {name} {channel} {date} {time} {timestamp} {year} {month} {day} {hour} {minute} {second} {title} {title_trimmed} {games} {video_id} {quality} {resolution} {height} {width} {fps} {vcodec} {acodec} {take} {tool} {mode} {platform} {platform_short} {went_live_date} {went_live_time}",
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
                                ).on_hover_text(
                                    "Tokens: {name} {platform} {platform_short} — expanded once \
                                     when the channel/instance is created, then stored as a \
                                     fixed literal path (see the global Default output folder's \
                                     tooltip for the full explanation of why only these two).",
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
                self.save_preset_dialog = Some(Arc::new(Mutex::new(SavePresetDraft {
                    kind: PresetKind::Filename,
                    template: tmpl,
                    name: String::new(),
                    error: String::new(),
                    do_save: false,
                    closed: false,
                })));
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
            if self.section_shown(SettingsTab::Interface, "Notifications", &["notifications", "desktop", "toast", "alerts", "dnd", "do not disturb", "quiet hours", "work hours"]) {
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

            ui.add_space(8.0);
            let mut dnd_on = self.dnd_enabled;
            if ui
                .checkbox(&mut dnd_on, "Do Not Disturb")
                .on_hover_text(
                    "Suppress desktop toasts right now (the in-app notifications feed and \
                     Background view still update — only the pop-up is silenced). \
                     Takes effect immediately.",
                )
                .changed()
            {
                self.dnd_enabled = dnd_on;
                let _ = self.core.store.set_setting(
                    crate::notifications::K_DND_ENABLED,
                    if dnd_on { "1" } else { "0" },
                );
                self.status = if dnd_on {
                    "Do Not Disturb enabled.".into()
                } else {
                    "Do Not Disturb disabled.".into()
                };
            }
            let mut dnd_sched_on = self.dnd_schedule_enabled;
            if ui
                .checkbox(&mut dnd_sched_on, "Automatically during a daily time range")
                .on_hover_text(
                    "Also suppress toasts every day during the window below — e.g. work \
                     hours, or overnight — independent of the toggle above.",
                )
                .changed()
            {
                self.dnd_schedule_enabled = dnd_sched_on;
                let _ = self.core.store.set_setting(
                    crate::notifications::K_DND_SCHEDULE_ENABLED,
                    if dnd_sched_on { "1" } else { "0" },
                );
                self.status = if dnd_sched_on {
                    "Do Not Disturb schedule enabled.".into()
                } else {
                    "Do Not Disturb schedule disabled.".into()
                };
            }
            if self.dnd_schedule_enabled {
                ui.indent("dnd_schedule_range", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("From");
                        let r1 = ui.add(
                            egui::TextEdit::singleline(&mut self.dnd_start)
                                .desired_width(50.0)
                                .hint_text("HH:MM"),
                        );
                        ui.label("to");
                        let r2 = ui.add(
                            egui::TextEdit::singleline(&mut self.dnd_end)
                                .desired_width(50.0)
                                .hint_text("HH:MM"),
                        );
                        ui.label("(a range like 22:00–08:00 spans midnight)");
                        // Validated with the exact "%H:%M" format `dnd_active`
                        // parses at runtime (not the looser `parse_time_of_day`,
                        // which also accepts seconds) — a value that passes
                        // here but fails there would silently never engage.
                        let strict_hhmm = |s: &str| {
                            chrono::NaiveTime::parse_from_str(s.trim(), "%H:%M").is_ok()
                        };
                        if r1.lost_focus() || r2.lost_focus() {
                            match (strict_hhmm(&self.dnd_start), strict_hhmm(&self.dnd_end)) {
                                (true, true) => {
                                    let _ = self
                                        .core
                                        .store
                                        .set_setting(crate::notifications::K_DND_START, &self.dnd_start);
                                    let _ = self
                                        .core
                                        .store
                                        .set_setting(crate::notifications::K_DND_END, &self.dnd_end);
                                    self.status = "Do Not Disturb schedule updated.".into();
                                }
                                _ => {
                                    self.status = "Time must be HH:MM.".into();
                                }
                            }
                        }
                    });
                });
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
            if self.section_shown(SettingsTab::PostProcessing, "Remux", &["remux", "mkv", "thumbnail", "title", "subtitles", "embed", "cover", "throttle", "readrate", "disk", "speed"]) {
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
                    ui.label("Template for the MKV title tag. Tokens: {title} {title_trimmed} {channel} {games} {date} {year} {month} {day} {name}");
                    ui.end_row();
                    ui.checkbox(&mut self.settings.remux_embed_subs, "Embed subtitle sidecars");
                    ui.label("Copy .srt/.ass/.vtt sidecar files as subtitle streams in the MKV.");
                    ui.end_row();
                    ui.label("Disk throttle");
                    setting_desc(
                        ui,
                        "Moved to \"Disk I/O limits\" below — the read throttle is now \
                         configurable per drive (default row = the old global value).",
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
                    // The underlying setting stays the ';'-joined string
                    // (that's the save/load and set_cache_root format) — this
                    // block only changes how it's EDITED: as one row per
                    // location instead of one cramped semicolon-packed field,
                    // parsed fresh each frame and rejoined on any change.
                    // Wrapped in one `ui.vertical` so it stays a single Grid
                    // cell (this Grid is 2 columns: control | description).
                    ui.vertical(|ui| {
                        ui.label("Capture cache location(s):");
                        let mut cache_rows: Vec<String> = self
                            .settings
                            .capture_cache_root
                            .split(';')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        if cache_rows.is_empty() {
                            ui.weak("No dedicated cache root set — each output folder gets its own .sa-cache subfolder.");
                        }
                        let mut rows_changed = false;
                        let mut remove_row: Option<usize> = None;
                        for (i, row) in cache_rows.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.weak(format!("{}.", i + 1));
                                if ui.add(egui::TextEdit::singleline(row).desired_width(400.0)).changed()
                                {
                                    rows_changed = true;
                                }
                                if ui
                                    .small_button("🗑")
                                    .on_hover_text("Remove this cache location.")
                                    .clicked()
                                {
                                    remove_row = Some(i);
                                }
                            });
                        }
                        if let Some(i) = remove_row {
                            cache_rows.remove(i);
                            rows_changed = true;
                        }
                        if rows_changed {
                            self.settings.capture_cache_root = cache_rows.join("; ");
                        }
                        if ui
                            .button("➕ Add folder…")
                            .on_hover_text(
                                "Pick a folder and add it as a new cache location — appended \
                                 to the list above, not a replacement for it.",
                            )
                            .clicked()
                        {
                            let first = cache_rows.first().cloned().unwrap_or_default();
                            self.pending_browse = Some(spawn_browse_folder(&first, |app, p| {
                                let mut rows: Vec<String> = app
                                    .settings
                                    .capture_cache_root
                                    .split(';')
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                rows.push(p);
                                app.settings.capture_cache_root = rows.join("; ");
                            }));
                        }
                    });
                    setting_desc(
                        ui,
                        "Central folder(s) for ALL in-progress capture files, one subfolder \
                         per channel — a single subtree per drive that backup tools can \
                         exclude by path (Backblaze has no wildcard rules). Recordings can \
                         span drives: one location per drive. Each only applies to output \
                         folders on ITS drive (finalizing must stay a same-volume rename); \
                         drives without one keep a per-folder .sa-cache. No locations at all \
                         = a .sa-cache subfolder inside each output folder. Existing files \
                         are found either way; takes started before a change finish under \
                         the old layout.",
                    );
                    ui.end_row();
                    ui.checkbox(&mut self.settings.iomon_sample_log, "I/O sample log");
                    setting_desc(
                        ui,
                        "Write the I/O monitor's 1s samples to a JSONL under the appdata \
                         logs folder (system drive) so drive stalls and disconnects can be \
                         analyzed after the fact. One file per session; a quiet idle session \
                         is small, but several concurrent captures can easily push it past \
                         5-10 MB/hour. Pruned after 14 days — checked at startup and at most \
                         once/day while running, so a long-lived session doesn't grow the \
                         logs folder unbounded between restarts.",
                    );
                    ui.end_row();
                });

            // ── File Management ────────────────────────────────────────────────
            }
    }

    fn settings_file_management_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::PostProcessing, "File Management", &["file", "management", "subdirectories", "organize", "split", "folders"]) {
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
            if self.section_shown(SettingsTab::Automation, "Post-stream VOD download", &["vod", "download", "archive", "replace", "post-stream", "published"]) {
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
            if self.section_shown(SettingsTab::Automation, "Head backfill on new takes", &["head", "backfill", "take", "retake", "reconnect", "capture from start"]) {
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

    fn settings_disposal_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::PostProcessing, "Automatic deletion", &["delete", "deletion", "cleanup", "trash", "recycle", "bin", "disposal", "join", "full", "parts", "head"]) {
            ui.add_space(12.0);
            ui.heading("Automatic deletion 🗑");
            ui.label(
                "What the app does when it deletes finished recordings on its own — the \
                 post-join parts cleanup below, superseded old heads, and a live capture \
                 replaced by its VOD. These are the GLOBAL defaults; override per-channel \
                 (channel Properties) or per-instance (edit instance). Transient working \
                 files (playlists, caches) are always plainly deleted.",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("After full.mkv join:");
                let v = &mut self.settings.join_cleanup;
                egui::ComboBox::from_id_salt("settings_join_cleanup")
                    .selected_text(v.label())
                    .show_ui(ui, |ui| {
                        for c in crate::disposal::JoinCleanup::ALL {
                            ui.selectable_value(v, c, c.label());
                        }
                    })
                    .response
                    .on_hover_text(
                        "Once a verified full.mkv (head + live capture losslessly joined) \
                         lands: keep both parts alongside it (safe, but a joined stream then \
                         occupies DOUBLE its size), delete just the head, or delete both \
                         parts — the take's main file then becomes the full. Cleanup only \
                         runs after the join passes its duration sanity check, and every \
                         removal uses the deletion method below.",
                    );
            });
            ui.horizontal(|ui| {
                ui.label("After gap splice:");
                let v = &mut self.settings.gap_splice_cleanup;
                egui::ComboBox::from_id_salt("settings_gap_splice_cleanup")
                    .selected_text(v.label())
                    .show_ui(ui, |ui| {
                        for c in crate::disposal::GapSpliceCleanup::ALL {
                            ui.selectable_value(v, c, c.label());
                        }
                    })
                    .response
                    .on_hover_text(
                        "Once a verified gapless splice (recovered lost-segment patches muxed \
                         into the take's main file) lands: keep the pre-splice original + \
                         patches alongside it (safe, but costs the extra space), delete just \
                         the consumed patches, or delete patches + the pre-splice original — \
                         the take's main file then becomes the gapless splice. Cleanup only \
                         runs after the splice passes its verification checks, and every \
                         removal uses the deletion method below.",
                    );
            });
            ui.checkbox(
                &mut self.settings.cache_drop_redundant,
                "Drop working-dir captures whose archive copy is verified",
            )
            .on_hover_text(
                "A capture is written to the hidden working folder first and moved out on \
                 success — but a crash, a failed remux or a re-attach can leave the original \
                 behind, and nothing ever cleans those up, because \"it looks stale\" is not \
                 evidence (a stale-looking .ts was once the only complete copy of a \
                 recording). With this on, the startup sweep may remove such a leftover, but \
                 ONLY after proving the take finished, its final file exists, and ffprobe says \
                 that file is at least as long as the leftover. Anything unprovable is kept \
                 untouched, and what is removed goes through the deletion method below, so \
                 it's still recoverable. Turn off to keep every working-dir capture forever.",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.settings.rolling_enabled, "Rolling recordings")
                    .on_hover_text(
                        "Global default: treat every capture as a rolling recording — \
                         automatically deleted a set time after it finishes, unless you press \
                         Keep on it in 📥 Backlog → Rolling recordings. The take's history row \
                         always survives (title, stats, chat log, chapters and notes are kept); \
                         only the video file goes, via the deletion method below. A channel or \
                         an individual instance can override this. Only captures started AFTER \
                         this is turned on are affected — nothing already recorded is put at \
                         risk, and turning it back off doesn't rescue takes already counting \
                         down (Keep those individually).",
                    );
                ui.label("Keep for (hours):");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.rolling_ttl_hours)
                        .hint_text("168")
                        .desired_width(60.0),
                )
                .on_hover_text(
                    "How long a rolling capture's file survives after its recording ends. \
                     Blank = one week. Each take freezes the value in force when it started, \
                     so changing this never re-times takes you already have.",
                );
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Deleted media goes to:");
                let v = &mut self.settings.disposal_method;
                let before = *v;
                egui::ComboBox::from_id_salt("settings_disposal_method")
                    .selected_text(v.label())
                    .show_ui(ui, |ui| {
                        for m in crate::disposal::DisposalMethod::ALL {
                            ui.selectable_value(v, m, m.label());
                        }
                    })
                    .response
                    .on_hover_text(
                        "Trash folder: instant same-drive move into the folder(s) below — \
                         you prune it yourself. Recycle Bin: the normal Windows bin \
                         (restorable; note that drives without a bin, e.g. some removable \
                         media, delete permanently instead). Delete permanently: gone \
                         immediately. A failed move/recycle always leaves the file in \
                         place — it is never escalated to a permanent delete.",
                    );
                // Picking "Trash folder" with nowhere to put trash silently
                // degrades every deletion to the Recycle Bin. Fill in the
                // per-drive default the moment it's selected so the working
                // configuration is the one you get by default; the warning
                // below covers it if this is then cleared by hand.
                if *v == crate::disposal::DisposalMethod::Trash
                    && before != *v
                    && self.settings.disposal_trash_dirs.trim().is_empty()
                    && self.settings.disposal_trash_default_root.trim().is_empty()
                {
                    self.settings.disposal_trash_default_root =
                        crate::disposal::TRASH_ROOT_SUGGESTION.to_string();
                    self.status =
                        "Trash folder selected — filled in a per-drive default trash root.".into();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Default trash folder:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.disposal_trash_default_root)
                        .hint_text(r"{drive}:\streams\.sa-trash")
                        .desired_width(240.0),
                )
                .on_hover_text(
                    "Fallback trash root applied to ANY drive not explicitly listed \
                     below — write it once with a '{drive}' token (e.g. \
                     '{drive}:\\streams\\.sa-trash') and every drive automatically \
                     gets its own trash folder in that shape, without moving files \
                     across disks. An explicit override below always wins for its \
                     drive. Blank here AND below is the one combination to avoid: \
                     the method still reads \"Trash folder\", but every deletion \
                     quietly goes to the Recycle Bin, which frees nothing on a \
                     recordings drive until you empty it yourself.",
                );
            });
            ui.horizontal(|ui| {
                ui.label("Trash folder overrides:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.disposal_trash_dirs)
                        .hint_text(r"A:\streams\.sa-trash; G:\vods\.sa-trash")
                        .desired_width(240.0),
                )
                .on_hover_text(
                    "Only used when \"Trash folder\" is the (effective) method. One \
                     folder per drive, ';'-separated — a trashed file is renamed into \
                     the folder on ITS OWN drive (a multi-GB \"delete\" must never \
                     become a cross-drive copy). Takes precedence over the default \
                     template above for any drive listed here; drives listed in \
                     neither fall back to the Recycle Bin. Name collisions get a \
                     \" (1)\" suffix.",
                );
                if ui
                    .button("Browse…")
                    .on_hover_text(
                        "Pick a folder — appended to the list (one folder per drive, \
                         ';'-separated).",
                    )
                    .clicked()
                {
                    let first = self
                        .settings
                        .disposal_trash_dirs
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    self.pending_browse = Some(spawn_browse_folder(&first, |app, p| {
                        let s = &mut app.settings.disposal_trash_dirs;
                        if s.trim().is_empty() {
                            *s = p;
                        } else {
                            *s = format!("{}; {}", s.trim().trim_end_matches(';'), p);
                        }
                    }));
                }
            });
            // "Trash folder" with neither field set is the trap that let 133 GB
            // pile up unnoticed in a Recycle Bin: the deletions all ran, none of
            // them freed a byte, and nothing in the UI said so. Say it loudly,
            // and offer the one-click fix rather than just complaining.
            if crate::disposal::trash_root_missing(
                self.settings.disposal_method,
                &self.settings.disposal_trash_dirs,
                &self.settings.disposal_trash_default_root,
            ) {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 90, 70),
                        "⚠ \"Trash folder\" is selected but no trash folder is configured — \
                         every automatic deletion will silently go to the Recycle Bin \
                         instead, which frees no space on the recordings drive until you \
                         empty it by hand.",
                    );
                    if ui
                        .button("Use the default")
                        .on_hover_text(format!(
                            "Set the default trash folder to {} — one folder per drive, so a \
                             deletion stays an instant same-drive move.",
                            crate::disposal::TRASH_ROOT_SUGGESTION
                        ))
                        .clicked()
                    {
                        self.settings.disposal_trash_default_root =
                            crate::disposal::TRASH_ROOT_SUGGESTION.to_string();
                    }
                });
            }
            }
    }

    /// "Simulcast dedup" — the global tier of [`crate::simulcast`].
    ///
    /// Written straight through on change (like the display-side platform
    /// preference) rather than waiting for a Save: these are one-click pickers
    /// with nothing to validate, and the next go-live reads them from the DB.
    fn settings_simulcast_section(&mut self, ui: &mut egui::Ui) {
        use crate::simulcast::SimulcastPref;
        if self.section_shown(
            SettingsTab::Automation,
            "Simulcast dedup",
            &["simulcast", "dedup", "duplicate", "multi", "platform", "instance", "preferred", "one copy", "ad-free", "sub", "failover", "standby"],
        ) {
            ui.add_space(12.0);
            ui.heading("Simulcast dedup ⇄");
            ui.label(
                "When one channel is live on several platforms at once, record only the \
                 preferred one instead of archiving the same broadcast twice. If the preferred \
                 platform ISN'T live, whatever is live records as normal — a platform exclusive \
                 is never skipped. The other instances stay armed: if the preferred one is live \
                 but never actually starts capturing (Auto off there, errors, a capture that \
                 dies), a sibling takes over.",
            );
            ui.add_space(6.0);
            let pick = |ui: &mut egui::Ui,
                            label: &str,
                            id: &str,
                            key: &'static str,
                            off_label: &str,
                            current: SimulcastPref,
                            hover: &str|
             -> Option<SimulcastPref> {
                let mut chosen = None;
                ui.horizontal(|ui| {
                    ui.label(label);
                    let text =
                        if current == SimulcastPref::Off { off_label } else { current.label() };
                    egui::ComboBox::from_id_salt(id)
                        .selected_text(text)
                        .show_ui(ui, |ui| {
                            for p in SimulcastPref::ALL {
                                let l = if p == SimulcastPref::Off { off_label } else { p.label() };
                                if ui.selectable_label(current == p, l).clicked() && current != p {
                                    let _ = self.core.store.set_setting(key, p.as_str());
                                    chosen = Some(p);
                                }
                            }
                        })
                        .response
                        .on_hover_text(hover);
                });
                chosen
            };
            if let Some(v) = pick(
                ui,
                "Simulcast: record only",
                "settings_simulcast_pref",
                crate::simulcast::K_SIMULCAST_PREF,
                "Off — record every live instance",
                self.simulcast_pref,
                "The platform to keep when a channel is live on more than one at the same \
                 time. Channel and instance Properties can override it — an instance set to \
                 Off is always recorded, even when a preferred sibling is live. Note this is \
                 NOT the same as Interface → Display's preferred platform, which only decides \
                 what the channel ROW shows.",
            ) {
                self.simulcast_pref = v;
            }
            if let Some(v) = pick(
                ui,
                "…but prefer this platform when it's ad-free",
                "settings_simulcast_ad_free_pref",
                crate::simulcast::K_SIMULCAST_AD_FREE_PREF,
                "No ad-free override",
                self.simulcast_ad_free_pref,
                "Overrides the choice above whenever the instance on THIS platform is ad-free \
                 for you — marked ad-free by hand, or a detected Twitch subscription. The point \
                 is ad-break hard cuts: prefer YouTube normally, but take Twitch on the \
                 channels you're subscribed to, where its stream has no ad breaks either. \
                 Ignored when that instance isn't live.",
            ) {
                self.simulcast_ad_free_pref = v;
            }
            ui.horizontal(|ui| {
                ui.label("Wait for the preferred platform for");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.simulcast_settle_mins)
                        .hint_text("minutes")
                        .desired_width(60.0),
                );
                ui.label("minutes");
                let changed = resp.changed();
                resp.on_hover_text(
                    "How long one broadcast counts as \"still settling\", which decides two \
                     things: how long a standing-by instance waits for the preferred one to \
                     actually start capturing before taking over as failover, and how early a \
                     capture that beat the preferred platform to it can still be switched over \
                     (later than that, the running capture is the intact copy and is left \
                     alone). Empty or 0 uses the default of 3 minutes.",
                );
                if changed {
                    // Minutes in the field, seconds in the DB — a blank or
                    // garbage entry stores nothing, so the default applies.
                    let secs = self
                        .simulcast_settle_mins
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|v| *v > 0.0)
                        .map(|v| (v * 60.0).round() as i64);
                    let _ = self.core.store.set_setting(
                        crate::simulcast::K_SIMULCAST_SETTLE_SECS,
                        &secs.map(|s| s.to_string()).unwrap_or_default(),
                    );
                }
            });
            ui.label(
                egui::RichText::new(
                    "Dedup is across platforms, not within one: two instances on the same \
                     preferred platform still both record.",
                )
                .small()
                .weak(),
            );
        }
    }

    fn settings_trigger_words_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Automation, "Trigger words", &["trigger", "word", "karaoke", "unarchived", "force", "auto", "title", "game", "regex", "delete", "deletion", "disposal"]) {
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
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Deletion for trigger-started takes:");
                disposal_method_combo(
                    ui,
                    "settings_trigger_disposal_default",
                    &mut self.settings.trigger_disposal_default,
                )
                .on_hover_text(
                    "Applies to every automatic disposal of a recording that a trigger word \
                     started (post-join cleanup, gap-splice cleanup, superseded old head), \
                     UNLESS the specific rule that started it sets its own override above — \
                     that always wins. Beats the channel/instance deletion method whenever it \
                     applies. Inherit = trigger-started recordings get no special treatment, \
                     same as any other take.",
                );
            });
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
            if self.section_shown(SettingsTab::Automation, "Blacklist triggers", &["blacklist", "block", "trigger", "prevent", "skip", "rerun", "veto", "title", "game", "regex"]) {
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

    fn settings_disk_io_section(&mut self, ui: &mut egui::Ui) {
        if self.section_shown(SettingsTab::Recording, "Disk I/O limits", &["disk", "io", "gate", "permit", "concurrent", "throttle", "readrate", "rate", "limit", "drive", "usb", "parallel"]) {
        ui.add_space(12.0);
        ui.heading("Disk I/O limits 🖴");
        ui.label(
            "How much of the app's own bulk I/O may hit each disk at once. Local passes \
             = full-file ffmpeg runs (finalize remux, split merge, head concat, embeds); \
             CDN muxes = network-fed writes (head backfills, VOD recoveries). The read \
             throttle and download rate limit here are the DEFAULTS (the same values as \
             the Remux disk throttle and the download rate limit); per-drive rows \
             override all four for recordings living on that drive. Permit changes take \
             effect immediately on Save — including for passes already queued behind the \
             old limit (a stuck backlog doesn't need a new pass to start before a raised \
             limit reaches it). A reduction still lets any pass already RUNNING finish \
             first; it only holds back the next one.",
        );
        ui.label(
            "Tick Dynamic on a drive to stop hand-tuning it: Local passes/CDN muxes become \
             a CEILING, and the live count adapts to the disk's actual queue depth instead \
             of holding a fixed number — grows slowly while the disk proves idle, backs off \
             immediately at the first sign of real contention. The ACTUAL live values appear \
             under the ticked Dynamic checkbox once bulk I/O has run on the drive: \
             \"L 2 /4 · 1 busy\" = 2 permits right now, ceiling 4, 1 in use (the Default \
             row lists one such line per un-overridden drive). Drag the number to pin a \
             manual override for now; 🔓 releases it back to auto. Read throttle and \
             download rate limit are unaffected by Dynamic.",
        );
        ui.add_space(6.0);
        let mut remove: Option<usize> = None;
        egui::Grid::new("disk_io_grid")
            .num_columns(8)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                ui.strong("Drive");
                ui.strong("Local passes").on_hover_text("Concurrent full-file ffmpeg passes on this disk (1 = one at a time). The ceiling when Dynamic is on.");
                ui.strong("CDN muxes").on_hover_text("Concurrent network-fed muxes writing to this disk. The ceiling when Dynamic is on.");
                ui.strong("Read throttle").on_hover_text("ffmpeg -readrate multiplier for local passes; 0 = unthrottled.");
                ui.strong("Download limit").on_hover_text("yt-dlp --limit-rate for VOD/video downloads landing on this disk (e.g. 4M, 500K); empty = unlimited. Never applied to live captures.");
                ui.strong("Dynamic").on_hover_text("Adapt the live permit count to actual disk activity instead of holding it fixed. See the note above.");
                ui.strong("Paused").on_hover_text(
                    "Emergency measure: block new local passes (concat/remux/embeds) on this \
                     drive so gap recovery, head-backfill fetches, VOD recovery, and live \
                     captures get the whole drive to themselves — those use a SEPARATE gate \
                     and are never paused by this. Doesn't touch a pass already running; only \
                     stops new ones from starting. Quicker to flip from the Background tab \
                     during an actual emergency — this checkbox is for a deliberate, \
                     restart-surviving pause.",
                );
                ui.label("");
                ui.end_row();

                // Default row (all drives without an override). No live/pin
                // readout here — "Default" isn't one physical disk, it's the
                // fallback for every un-overridden one, so there's no single
                // queue depth to show.
                ui.label("Default").on_hover_text("Applies to every drive without its own row below.");
                ui.add(egui::DragValue::new(&mut self.settings.disk_default_local).range(1..=8));
                ui.add(egui::DragValue::new(&mut self.settings.disk_default_cdn).range(1..=8));
                ui.add(
                    egui::DragValue::new(&mut self.settings.postproc_readrate)
                        .range(0.0..=1000.0)
                        .speed(1.0)
                        .suffix("×"),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.download_rate_limit)
                        .hint_text("off")
                        .desired_width(70.0),
                );
                ui.vertical(|ui| {
                    ui.checkbox(&mut self.settings.disk_default_dynamic, "");
                    if self.settings.disk_default_dynamic {
                        // "Default" spans every un-overridden drive, so the
                        // live readout is one line per real disk that has
                        // actually seen bulk I/O this session.
                        let overridden: Vec<String> = self
                            .settings
                            .disk_overrides
                            .iter()
                            .map(|(l, _)| l.trim().to_uppercase())
                            .collect();
                        let mut any = false;
                        for letter in crate::io_gate::active_gate_letters() {
                            if overridden.contains(&letter) {
                                continue;
                            }
                            any = true;
                            ui.horizontal(|ui| {
                                ui.weak(format!("{letter}:")).on_hover_text(format!(
                                    "Drive {letter}: has done bulk I/O this session and has no \
                                     override row, so the Default limits (and this Dynamic \
                                     setting) govern it. Live values shown to the right."
                                ));
                                dynamic_live_cell(
                                    ui,
                                    &letter,
                                    self.settings.disk_default_local,
                                    self.settings.disk_default_cdn,
                                );
                            });
                        }
                        if !any {
                            ui.weak("no bulk I/O yet")
                                .on_hover_text("Appears once a remux/backfill/recovery runs on a drive without its own row.");
                        }
                    }
                });
                ui.checkbox(&mut self.settings.disk_default_paused, "");
                ui.label("");
                ui.end_row();

                for i in 0..self.settings.disk_overrides.len() {
                    let (letter, lim) = &mut self.settings.disk_overrides[i];
                    ui.add(
                        egui::TextEdit::singleline(letter)
                            .hint_text("A")
                            .desired_width(24.0)
                            .char_limit(1),
                    )
                    .on_hover_text("Drive letter this override applies to.");
                    ui.add(egui::DragValue::new(&mut lim.local_permits).range(1..=8));
                    ui.add(egui::DragValue::new(&mut lim.cdn_permits).range(1..=8));
                    ui.add(
                        egui::DragValue::new(&mut lim.readrate)
                            .range(0.0..=1000.0)
                            .speed(1.0)
                            .suffix("×"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut lim.rate_limit)
                            .hint_text("off")
                            .desired_width(70.0),
                    );
                    ui.vertical(|ui| {
                        ui.checkbox(&mut lim.dynamic, "");
                        if lim.dynamic {
                            dynamic_live_cell(ui, letter, lim.local_permits, lim.cdn_permits);
                        }
                    });
                    ui.checkbox(&mut lim.paused, "");
                    if ui.small_button("🗑").on_hover_text("Remove this drive's override").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
        if let Some(i) = remove {
            self.settings.disk_overrides.remove(i);
        }
        if ui.button("➕ Add drive override").clicked() {
            self.settings.disk_overrides.push((
                String::new(),
                crate::io_gate::DiskLimits {
                    readrate: self.settings.postproc_readrate,
                    rate_limit: self.settings.download_rate_limit.clone(),
                    ..Default::default()
                },
            ));
        }
        ui.label(
            egui::RichText::new(
                "Live capture writes are never gated or throttled — these limits only \
                 bound the app's own post-processing and downloads.",
            )
            .small()
            .weak(),
        );
        }
    }

    fn settings_vod_recovery_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Automation, "Twitch VOD recovery", &["vod", "recovery", "muted", "deleted", "cdn", "recover", "unmute", "gap", "lost", "segment", "warnings"]) {
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
                    ui.checkbox(&mut self.settings.gap_recover, "Recover lost segments automatically")
                        .on_hover_text(
                            "When a live Twitch capture drops segments (streamlink logs a \
                             \"Sequence gap\" — that content is MISSING from the file), \
                             re-fetch the lost time ranges from the VOD CDN while the stream \
                             is still running (best chance before any post-stream DMCA \
                             muting) and save them as patch files next to the recording. \
                             Alerts appear under 🚨 Warnings either way.",
                        );
                    ui.label("Re-fetch sequence-gap losses from the VOD CDN into sibling patch files.");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.yt_gap_heal, "YouTube: auto-heal from the published VOD")
                        .on_hover_text(
                            "After a YouTube broadcast ends, compute what spans the local \
                             take files DON'T cover — a late join past the DVR window, gaps \
                             between takes (capture died mid-stream: PO-token wave, crash, \
                             platform suspension), or a tail the capture never resumed — and \
                             download JUST those sections from the published VOD \
                             (yt-dlp --download-sections), quality-matched to the capture, \
                             as .recovered-… patch files beside the take. The local capture \
                             stays the primary copy: a VOD that was trimmed/edited no longer \
                             lines up with the broadcast clock, so healing is refused with a \
                             ✂ Warnings row instead of splicing wrong footage. Streams with \
                             no published VOD simply never heal (the patches wait, then give \
                             up). The splice switch below applies to these patches too.",
                        );
                    ui.label("Download only the missing spans of a broadcast from its VOD as sibling patch files.");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.gap_splice, "Splice recovered gaps into a gapless file")
                        .on_hover_text(
                            "Once a take's recovered patches have all settled, try to mux them \
                             into the take's main file so there's one seamless, gapless \
                             recording instead of separate sibling patches. Every individual \
                             splice still passes its own safety checks first (matching codec \
                             parameters, a trustworthy PTS-derived splice point, a verified \
                             result) — this only gates whether the attempt happens at all. Any \
                             check that fails leaves the patches exactly as they are today; \
                             nothing is ever guessed. See \"After gap splice\" below for what \
                             happens to the originals once a splice succeeds.",
                        );
                    ui.label("Mux recovered gap patches into a gapless main file (see 🗑 Automatic deletion for cleanup).");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.auto_recover_muted, "Auto-recover muted VODs");
                    ui.label("When the VOD checker finds a DMCA-muted VOD, recover it automatically.");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.auto_recover_deleted, "Auto-recover deleted VODs");
                    ui.label("When a stream never publishes a VOD, try to recover it from the CDN automatically.");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.auto_backfill_missed, "Auto-backfill missed streams")
                        .on_hover_text(
                            "Off by default. Two things: (1) the moment a 👁 \"seen \
                             live, Auto was off\" row's broadcast ends, automatically \
                             try the published VOD, then (Twitch) CDN recovery if \
                             that's not published — same as clicking \"⏬ Backfill \
                             missed VOD\" yourself; (2) periodically scan each \
                             platform for broadcasts this app has no record of at \
                             all (wasn't running/monitoring at the time) and do the \
                             same for anything found. Either half can also be run \
                             on demand: the per-row button, or a channel's \"🔎 Scan \
                             for missed streams\" action.",
                        );
                    ui.label(
                        "Retroactively finish/discover streams before the platform \
                         prunes or removes them — see the Streams grid's 👁 rows.",
                    );
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

    fn settings_chapters_section(&mut self, ui: &mut egui::Ui) {
        if self.section_shown(
            SettingsTab::PostProcessing,
            "Chapters",
            &["chapters", "chapter", "title", "category", "game", "raid", "muted", "recovered", "bookmark", "coalesce", "window", "interval", "sync"],
        ) {
            ui.add_space(12.0);
            ui.heading("Chapters 📑");
            ui.label(
                "Embed chapter markers into a finalized recording once its file is stable \
                 (finished, head backfill settled, any gap-splice attempt resolved): one per \
                 title change, one per category/game change (merged into one chapter when both \
                 change together within the coalesce window below), one per raid past the \
                 viewer threshold below, and a bracketing pair around any gap-splice patch. \
                 These are the GLOBAL defaults; override per-channel (channel Properties) or \
                 per-instance (edit instance).",
            );
            ui.add_space(6.0);
            egui::Grid::new("chapters_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.checkbox(&mut self.settings.chapters_enabled, "Embed chapters")
                        .on_hover_text(
                            "Master on/off for chapter embedding. Every individual kind below \
                             can still be turned off independently; this only gates whether the \
                             feature runs at all. Purely additive metadata — a wrong chapter \
                             position is a minor cosmetic miss, never a risk to the recording \
                             itself.",
                        );
                    ui.label("Master on/off (default on).");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.chapters_title, "Title changes");
                    ui.label("One chapter per title change (per stream_meta_change history).");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.chapters_category, "Category/game changes");
                    ui.label("One chapter per category/game change.");
                    ui.end_row();

                    ui.label("Title/game coalesce window (s)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.chapters_coalesce_secs)
                            .desired_width(80.0)
                            .hint_text("30"),
                    )
                    .on_hover_text(
                        "How many seconds apart a title change and a category/game change may \
                         land and still merge into one combined chapter instead of two separate \
                         ones. Some streamers update both together instantly (a small window is \
                         fine); others update them minutes apart (raise this so they still merge). \
                         Overridable per channel (channel Properties) or per instance (edit \
                         instance).",
                    );
                    ui.end_row();

                    ui.checkbox(&mut self.settings.chapters_raid, "Raids");
                    ui.label("One chapter per raid at or above the viewer threshold.");
                    ui.end_row();

                    ui.label("Minimum raid viewers for a chapter");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.chapters_raid_min_viewers)
                            .desired_width(80.0)
                            .hint_text("50"),
                    )
                    .on_hover_text(
                        "Raids below this party size don't get their own chapter — keeps a \
                         string of 1-2-viewer raids from spamming the chapter list.",
                    );
                    ui.end_row();

                    ui.checkbox(&mut self.settings.chapters_recovered_segments, "Recovered gap-splice segments")
                        .on_hover_text(
                            "Brackets every successfully spliced lost-segment patch with \
                             \"Recovered segment start\"/\"Recovered segment end\" chapters, \
                             regardless of mute status — useful for spot-checking a recovery fix \
                             later. Only produced for takes where gap-splice actually completed.",
                        );
                    ui.label("\"Recovered segment start/end\" around every spliced patch.");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.chapters_muted_segments, "Muted gap-splice segments")
                        .on_hover_text(
                            "Independently brackets only the spliced patches whose recovery \
                             needed Twitch's muted-fallback copy, with \"Muted segment \
                             start\"/\"Muted segment end\" chapters — can coexist with \
                             \"Recovered segment\" markers on the same patch.",
                        );
                    ui.label("\"Muted segment start/end\" around patches with silenced audio.");
                    ui.end_row();

                    let chapters_all_running = self
                        .background_tasks
                        .iter()
                        .any(|t| t.kind == crate::events::BackgroundTaskKind::ReembedChaptersAll);
                    if ui
                        .add_enabled(!chapters_all_running, egui::Button::new("Re-embed chapters"))
                        .on_hover_text(
                            "Re-run chapter embedding across every eligible recording, \
                             regardless of whether it already has chapters — useful after \
                             changing which kinds are enabled, or to pick up recordings \
                             from before this feature existed without waiting for the next \
                             app restart's automatic sweep. Skips takes still resolving a \
                             gap-splice or still recording; safe to run any time.",
                        )
                        .clicked()
                    {
                        self.core.manual(ManualCommand::ReembedChaptersAll);
                        self.status = "Re-embedding chapters…".into();
                    }
                    ui.label("Re-embed chapters for every eligible recording, even ones that already have them.");
                    ui.end_row();
                });
        }
    }

    fn settings_raid_follow_section(&mut self, ui: &mut egui::Ui) {
        if self.section_shown(
            SettingsTab::Automation,
            "Follow raid",
            &["raid", "follow", "raid-follow", "raid target"],
        ) {
            ui.add_space(12.0);
            ui.heading("Follow raid 🏃");
            ui.label(
                "When a monitored Twitch channel raids out to another channel, tune into (or \
                 auto-record) the raid target. Needs conduit mode (Client ID + Secret, Settings \
                 → Accounts) and \"Raids via EventSub\" on — raid-out has no other detection \
                 path (chat only ever sees raids coming IN, never going out), so without both \
                 this feature never fires. These are the GLOBAL defaults for who FOLLOWS \
                 (source channel); override per-channel/instance (Properties / edit instance).",
            );
            ui.add_space(6.0);
            egui::Grid::new("raid_follow_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.checkbox(&mut self.settings.raid_follow_record, "Auto-record raid targets")
                        .on_hover_text(
                            "Master on/off: does raiding out ever trigger an auto-RECORD of the \
                             target at all? Off by default — unlike most toggles here, this \
                             creates new recordings of channels you didn't curate. Independent \
                             of \"Auto-play raid targets\" below — either, both, or neither can \
                             be on. The manual \"Follow raid\" play action (a channel's \
                             right-click menu) works regardless of either setting. Single-hop \
                             only: records until the raid target's own stream ends — Twitch has \
                             no formal \"raid end\" event, and following further raid chains \
                             isn't implemented yet.",
                        );
                    ui.label("Master on/off (default OFF). Single-hop only for now.");
                    ui.end_row();

                    ui.checkbox(&mut self.settings.raid_follow_play, "Auto-play raid targets")
                        .on_hover_text(
                            "Master on/off: does raiding out ever auto-OPEN the target at the \
                             live edge in your media player — no recording, the automatic \
                             equivalent of the manual \"▷🏃 Follow raid\" button? Off by \
                             default. Independent of \"Auto-record raid targets\" above. Unlike \
                             auto-record, this is never gated by the target's disabled state — \
                             only by its own \"Exclude from auto-play\" override (channel \
                             Properties / edit instance), since opening a player doesn't touch \
                             the target's recording/disk configuration at all.",
                        );
                    ui.label("Master on/off (default OFF). Never gated by the target's disabled state.");
                    ui.end_row();

                    ui.add_enabled_ui(self.settings.raid_follow_play, |ui| {
                        ui.checkbox(
                            &mut self.settings.raid_follow_play_only_watched,
                            "Only when watching the raider",
                        )
                        .on_hover_text(
                            "Auto-play the raid only if the RAIDING instance was open in a \
                             player this app launched (▷ live edge, ⏵ from start, collab \
                             angles…) — still open when the raid fires, or closed within the \
                             last 10 minutes (players often exit at end-of-stream moments \
                             before the raid event arrives). On by default: without it, every \
                             auto-play-enabled instance pops an unexplained player window \
                             whenever it raids out, watched or not. Players opened outside \
                             this app don't count — the app can't see them.",
                        );
                    });
                    ui.label("Skip the auto-play when you weren't watching the raiding stream.");
                    ui.end_row();

                    ui.label("Ad-hoc capture folder");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.settings.raid_follow_output_dir);
                        if ui.button("Browse…").clicked() {
                            self.pending_browse = Some(spawn_browse_folder(
                                &self.settings.raid_follow_output_dir,
                                |app, p| app.settings.raid_follow_output_dir = p,
                            ));
                        }
                    })
                    .response
                    .on_hover_text(
                        "Where a raid target that ISN'T one of your tracked channels gets \
                         captured — a plain file, no Streams-grid entry or history (a tracked \
                         target instead uses its own configured output folder like any other \
                         recording). Supports the {name} token (the raid target's display \
                         name). Required for untracked targets to record at all.",
                    );
                    ui.end_row();

                    ui.checkbox(&mut self.settings.raid_skip_disabled_targets, "Skip disabled raid targets")
                        .on_hover_text(
                            "Don't auto-record a TRACKED raid target that's currently disabled \
                             (its master switch off, at either channel or instance level) — on \
                             by default. Auto-record being off does NOT count as disabled here \
                             (same as Trigger Words) — a channel you've deliberately left in \
                             manual-only mode still gets recorded via a followed raid. A \
                             channel can override this either way via its own \"Record me when \
                             I'm a raid target\" setting (channel Properties / edit instance), \
                             which always wins over this default.",
                        );
                    ui.label("Default on — a channel/instance override always wins.");
                    ui.end_row();
                });
        }
    }

    fn settings_ad_probe_section(&mut self, ui: &mut egui::Ui) {
        if self.section_shown(
            SettingsTab::Recording,
            "Twitch ad-break detection",
            &["ad", "ads", "advertisement", "ad break", "ad probe", "manifest", "📢"],
        ) {
            ui.add_space(12.0);
            ui.heading("Twitch ad-break detection 📢");
            ui.label(
                "Streamlink already cuts Twitch ads out of the recording on its own — this only \
                 controls whether we ALSO log where those cuts happened (the 📢 Ads column, \
                 cut-list popup). Polls the live stream's own manifest directly (the same public \
                 access every Twitch player uses) every ~10s for ad markers; never affects the \
                 capture itself.",
            );
            ui.add_space(6.0);
            ui.checkbox(&mut self.settings.ad_probe, "Detect ad breaks from the live manifest")
                .on_hover_text(
                    "Reads the live Twitch playlist directly to find ad-break markers, in \
                     addition to streamlink's own log line (which needs extra metadata Twitch \
                     doesn't always send, and in practice misses almost every real break). \
                     Degrades soft on failure — a sustained problem shows up as an 🚨 Warnings \
                     alert instead of failing silently.",
                );
        }
    }

    /// ── Chat highlights & mentions ──
    fn settings_chat_highlights_section(&mut self, ui: &mut egui::Ui) {
        if !self.section_shown(
            SettingsTab::Interface,
            "Chat highlights",
            &["chat", "highlight", "mention", "ping", "notify", "regex", "@", "💬"],
        ) {
            return;
        }
        ui.add_space(12.0);
        ui.heading("Chat highlights & mentions 💬");
        ui.label(
            "Words to watch for in every monitored channel's live chat. Matching runs in the \
             chat logger itself, so it works with no chat window open — and for channels being \
             logged without a recording.",
        );
        ui.add_space(6.0);

        let mut pingable = self.chat_pingable;
        if ui
            .checkbox(&mut pingable, "Notify me when someone says my name")
            .on_hover_text(
                "Raise a desktop toast and a 🔔 feed row when a chatter names the connected \
                 Twitch account — either as @name or on its own. Off by default: this is the \
                 one setting that can make an unattended machine start talking to you. Do Not \
                 Disturb still suppresses the toast (the feed row is always recorded). At most \
                 one toast per channel per 10s, so a chat spamming your name can't spawn fifty. \
                 Requires a connected Twitch account. Applies to new chat connections.",
            )
            .changed()
        {
            self.chat_pingable = pingable;
            let _ = self
                .core
                .store
                .set_setting(crate::chat_highlight::K_PINGABLE, if pingable { "1" } else { "0" });
        }
        if pingable && self.connected_twitch_login().is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(200, 150, 60),
                "No Twitch account connected — nothing to be named as. Connect one under \
                 Accounts.",
            );
        }

        ui.add_space(6.0);
        ui.label(egui::RichText::new("Custom highlights").strong());
        ui.label(
            egui::RichText::new(
                "Each rule highlights matching messages in the chat window. Tick Notify to also \
                 raise a toast for it.",
            )
            .small()
            .weak(),
        );

        let mut rules = self.chat_settings.lock().unwrap().highlight_rules.clone();
        let mut changed = false;
        let mut remove: Option<usize> = None;
        for (i, r) in rules.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                changed |= ui
                    .checkbox(&mut r.enabled, "")
                    .on_hover_text("Off keeps the rule but stops it matching.")
                    .changed();
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut r.label)
                            .hint_text("name (optional)")
                            .desired_width(120.0),
                    )
                    .on_hover_text(
                        "Shown in the rule list and in notifications, so a long regex isn't the \
                         only thing identifying this rule.",
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut r.pattern)
                            .hint_text("word, phrase or regex")
                            .desired_width(220.0),
                    )
                    .changed();
                changed |= ui
                    .checkbox(&mut r.regex, "Regex")
                    .on_hover_text(
                        "Treat the pattern as a regular expression, case-insensitive unless it \
                         opts out with (?-i). Off matches it as plain text.",
                    )
                    .changed();
                changed |= ui
                    .add_enabled(!r.regex, egui::Checkbox::new(&mut r.whole_word, "Whole word"))
                    .on_hover_text(
                        "Only match on word boundaries, so \"art\" doesn't fire on \"start\". \
                         Regex rules express this themselves with \\b.",
                    )
                    .on_disabled_hover_text("Use \\b in the regex instead.")
                    .changed();
                changed |= ui
                    .checkbox(&mut r.notify, "Notify")
                    .on_hover_text(
                        "Also raise a toast for this rule, not just a highlighted row. Off by \
                         default — most people want a few words to stand out and only their own \
                         name to interrupt them.",
                    )
                    .changed();
                if ui.button("🗑").on_hover_text("Remove this rule.").clicked() {
                    remove = Some(i);
                }
            });
            if let Some(err) = crate::chat_highlight::pattern_error(r) {
                ui.colored_label(egui::Color32::from_rgb(200, 80, 80), format!("   regex: {err}"));
            }
        }
        if let Some(i) = remove {
            rules.remove(i);
            changed = true;
        }
        if ui.button("+ Add highlight").clicked() {
            rules.push(crate::chat_highlight::HighlightRule::default());
            changed = true;
        }
        if changed {
            crate::chat_highlight::save_rules(&self.core.store, &rules);
            // Into the SHARED state, so open chat windows pick it up on their
            // next frame rather than needing a reopen. The live chat logger
            // re-reads from the store on its own timer.
            self.chat_settings.lock().unwrap().highlight_rules = rules;
        }
    }

    /// The connected Twitch account's login, or `""`.
    fn connected_twitch_login(&self) -> String {
        self.core.store.get_setting(crate::oauth::K_LOGIN).ok().flatten().unwrap_or_default()
    }

    fn settings_chat_section(&mut self, ui: &mut egui::Ui) {
        if self.section_shown(
            SettingsTab::Recording,
            "Chat logging",
            &["chat", "chat log", "jsonl", "live_chat", "irc", "auto", "not recorded", "💬"],
        ) {
            ui.add_space(12.0);
            ui.heading("Chat logging 💬");
            ui.label(
                "Whether an instance logs chat at all is its own \"Log chat\" toggle — this \
                 decides whether that toggle keeps applying to broadcasts nobody is recording.",
            );
            ui.add_space(6.0);
            ui.checkbox(
                &mut self.settings.chat_log_without_recording,
                "Log chat even when the stream isn't being recorded",
            )
            .on_hover_text(
                "Auto-record off means \"don't spend the disk on this stream\", not \"don't \
                 archive it\" — so when a monitored channel goes live with Auto off, chat is \
                 still captured on its own. A chat log is a few MB where the video is tens of \
                 GB, and unlike the video it can't be fetched back later: Twitch publishes no \
                 transcript, and YouTube's chat replay dies with the stream. The sidecar is the \
                 same file a recorded take produces (Twitch: .chat.jsonl from the built-in \
                 anonymous logger; YouTube + yt-dlp: .live_chat.json), lands in the instance's \
                 output folder, and is opened from the \"seen live, not recorded\" 👁 take row \
                 via 💬 View chat. Turn OFF to capture chat only alongside an actual recording. \
                 Not applied when a blacklist trigger vetoed the recording or a Stop hold is \
                 active — those mean \"skip this broadcast\", not \"save the disk\".",
            );
            ui.label(
                "Default on. Still requires the instance's own \"Log chat\"; Twitch and \
                 YouTube (yt-dlp) only.",
            );
        }
    }

    /// Immediate-save for the viewer-history auto-compress setting (the
    /// checkbox/drag pair below saves on change, like the EventSub toggles).
    fn save_viewer_downsample_days(&self) {
        let _ = self.core.store.set_setting(
            crate::store::K_VH_DOWNSAMPLE_DAYS,
            &self.settings.viewer_downsample_days.to_string(),
        );
    }

    fn settings_stats_history_section(&mut self, ui: &mut egui::Ui) {
        if self.section_shown(
            SettingsTab::Stats,
            "Channel stats history",
            &["stats", "viewer", "history", "downsample", "compress", "retention", "graph"],
        ) {
            ui.add_space(12.0);
            ui.heading("Channel stats history 📈");
            ui.label(
                "Viewer/follower samples (one per minute while live) feed the Channel \
                 Stats graphs and are kept forever by default. Old samples can be \
                 compressed into 10-minute buckets — peaks and total airtime are \
                 preserved, only the fine detail goes.",
            );
            let (rows, oldest, raw_rows) = self
                .core
                .store
                .viewer_history_info()
                .unwrap_or((0, None, 0));
            ui.label(format!(
                "Currently stored: {rows} samples ({raw_rows} at full resolution){}",
                oldest
                    .map(|t| format!(", oldest from {}", fmt_datetime_short(t)))
                    .unwrap_or_default()
            ))
            .on_hover_text(
                "A sample row is ~30 bytes; a channel that's live 8 h/day adds \
                 ~480 rows/day at full resolution (48 once compressed).",
            );
            ui.horizontal(|ui| {
                let mut auto_on = self.settings.viewer_downsample_days > 0;
                if ui
                    .checkbox(&mut auto_on, "Auto-compress samples older than")
                    .on_hover_text(
                        "Once a day, rewrite viewer samples older than this many days \
                         into 10-minute buckets. Off = keep every minute-resolution \
                         sample forever.",
                    )
                    .changed()
                {
                    self.settings.viewer_downsample_days = if auto_on { 90 } else { 0 };
                    self.save_viewer_downsample_days();
                }
                let mut days = self.settings.viewer_downsample_days.max(1);
                if ui
                    .add_enabled(
                        auto_on,
                        egui::DragValue::new(&mut days).range(7..=3650).suffix(" days"),
                    )
                    .on_hover_text("Samples younger than this always stay full resolution")
                    .changed()
                    && auto_on
                {
                    self.settings.viewer_downsample_days = days;
                    self.save_viewer_downsample_days();
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .button("🗜 Compress now (older than 90 days)")
                    .on_hover_text(
                        "One-off compression of everything older than 90 days into \
                         10-minute buckets, regardless of the auto setting. Cannot \
                         be undone (the fine detail is gone), but graphs, peaks and \
                         airtime keep working.",
                    )
                    .clicked()
                {
                    let cut = now_unix() - 90 * 86_400;
                    match self.core.store.downsample_viewer_history(cut) {
                        Ok((before, after)) => {
                            self.status = format!(
                                "Viewer history compressed: {before} samples -> {after}"
                            );
                        }
                        Err(e) => self.status = format!("Compress failed: {e:#}"),
                    }
                }
            });

            }
    }

    fn settings_hype_trains_section(&mut self, ui: &mut egui::Ui) {
        if self.section_shown(
            SettingsTab::Stats,
            "Hype trains",
            &["hype", "train", "inference", "weights", "points", "gql", "auto-tune", "burst", "kickoff"],
        ) {
            ui.add_space(12.0);
            ui.heading("Hype trains 🚂");
            ui.label(
                "Twitch hype trains are captured two ways: the public train state is \
                 polled while a channel is live (confirmed trains, with level, points \
                 and top contributors), and the chat logger infers train-like bursts \
                 from subs/bits while recording (the fallback when polling is off or \
                 broken). Confirmed trains replace inferred ones and calibrate the \
                 inference below.",
            );
            let mut gql = self.hype_gql;
            if ui
                .checkbox(&mut gql, "Confirm trains via Twitch (recommended)")
                .on_hover_text(
                    "Ask Twitch's public GQL endpoint for the live hype-train state — \
                     one batched request per poll tick for live channels, one per \
                     minute per recording channel. Anonymous, no credentials or \
                     scopes; the same data every logged-out viewer sees. Confirmed \
                     trains supersede inferred bursts and feed the auto-tune.",
                )
                .changed()
            {
                self.hype_gql = gql;
                let _ = self
                    .core
                    .store
                    .set_setting(crate::hype::K_HYPE_GQL, if gql { "1" } else { "0" });
            }
            let mut auto = self.hype_tuning.auto_tune;
            if ui
                .checkbox(&mut auto, "Auto-tune the inference")
                .on_hover_text(
                    "Confirmed or manually-marked trains the chat inference missed \
                     LOOSEN the thresholds below; bursts Twitch never confirmed and \
                     inferred events you 🗑-delete TIGHTEN them. Every adjustment is \
                     listed in the tuning log.",
                )
                .changed()
            {
                self.hype_tuning.auto_tune = auto;
                crate::hype::save_tuning(&self.core.store, &self.hype_tuning);
            }
            ui.add_space(4.0);
            let mut tuning_changed = false;
            egui::Grid::new("hype_tuning_grid")
                .num_columns(4)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Burst window").on_hover_text(
                        "Contributions must fall within this sliding window to count \
                         toward one burst (Twitch's own train timer is 5 minutes).",
                    );
                    tuning_changed |= ui
                        .add(
                            egui::DragValue::new(&mut self.hype_tuning.window_secs)
                                .range(60..=900)
                                .suffix(" s"),
                        )
                        .on_hover_text("60–900 seconds")
                        .changed();
                    ui.label("Min points").on_hover_text(
                        "Summed contribution points needed in the window (weights \
                         below). 0 disables the points gate — the event/chatter \
                         counts alone decide.",
                    );
                    tuning_changed |= ui
                        .add(
                            egui::DragValue::new(&mut self.hype_tuning.min_points)
                                .range(0..=10_000)
                                .suffix(" pts"),
                        )
                        .on_hover_text("0 = points gate off")
                        .changed();
                    ui.end_row();
                    ui.label("Min contributions").on_hover_text(
                        "Number of separate sub/gift/bits/Hype Chat events needed in \
                         the window.",
                    );
                    tuning_changed |= ui
                        .add(egui::DragValue::new(&mut self.hype_tuning.min_events).range(1..=20))
                        .changed();
                    ui.label("Min chatters").on_hover_text(
                        "Distinct contributors needed — keeps a single whale's gift \
                         batch from counting as a train.",
                    );
                    tuning_changed |= ui
                        .add(egui::DragValue::new(&mut self.hype_tuning.min_actors).range(1..=10))
                        .changed();
                    ui.end_row();
                    ui.label("Points per bit").on_hover_text(
                        "Weight of one cheered bit (Twitch's own rate is 1).",
                    );
                    tuning_changed |= ui
                        .add(egui::DragValue::new(&mut self.hype_tuning.w_bits).range(0..=100))
                        .changed();
                    ui.label("Points per sub").on_hover_text(
                        "Weight of a tier-1 sub or resub (tier 2 counts double, \
                         tier 3 five-fold). Twitch's own rate is 500.",
                    );
                    tuning_changed |= ui
                        .add(egui::DragValue::new(&mut self.hype_tuning.w_sub).range(0..=5000))
                        .changed();
                    ui.end_row();
                    ui.label("Points per gifted sub").on_hover_text(
                        "Weight of each sub inside a gift batch (Twitch's rate: 500).",
                    );
                    tuning_changed |= ui
                        .add(egui::DragValue::new(&mut self.hype_tuning.w_gift).range(0..=5000))
                        .changed();
                    ui.label("Points per Hype Chat cent").on_hover_text(
                        "Weight of one currency minor unit (cent) of a paid pinned \
                         message — 1 makes a $5.00 Hype Chat worth one sub.",
                    );
                    tuning_changed |= ui
                        .add(egui::DragValue::new(&mut self.hype_tuning.w_dono).range(0..=100))
                        .changed();
                    ui.end_row();
                });
            if tuning_changed {
                crate::hype::save_tuning(&self.core.store, &self.hype_tuning);
            }
            ui.horizontal(|ui| {
                if ui
                    .button("↺ Defaults")
                    .on_hover_text(
                        "Reset window, gates and weights to the built-in defaults \
                         (300 s / 1000 pts / 3 events / 2 chatters, Twitch-rate \
                         weights). The tuning log is kept.",
                    )
                    .clicked()
                {
                    let log = std::mem::take(&mut self.hype_tuning.log);
                    let auto = self.hype_tuning.auto_tune;
                    self.hype_tuning = crate::hype::HypeTuning { log, auto_tune: auto, ..Default::default() };
                    crate::hype::save_tuning(&self.core.store, &self.hype_tuning);
                }
                if ui
                    .button("⟳ Reload")
                    .on_hover_text(
                        "Re-read the stored values — the auto-tune adjusts them in \
                         the background while this page is open.",
                    )
                    .clicked()
                {
                    self.hype_tuning = crate::hype::load_tuning(&self.core.store);
                }
            });
            egui::CollapsingHeader::new("Tuning log")
                .id_salt("hype_tuning_log")
                .show(ui, |ui| {
                    if self.hype_tuning.log.is_empty() {
                        ui.weak("No auto-tune adjustments yet.");
                    }
                    for line in &self.hype_tuning.log {
                        ui.label(line);
                    }
                })
                .header_response
                .on_hover_text(
                    "What the auto-tune changed and why, newest first (last 20). \
                     Per-channel sensitivity overrides are edited from the Channel \
                     Stats view's ⚙ button.",
                );

            }
    }

    fn settings_maintenance_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::Maintenance, "Maintenance", &["maintenance", "re-remux", "remux all", "thumbnails", "reorganize", "batch", "preset", "emote", "chat"]) {
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
            let join_cleanup_running  = self.background_tasks.iter().any(|t| t.kind == BTK::RerunJoinCleanup);
            let chat_migrate_running  = self.background_tasks.iter().any(|t| t.kind == BTK::MigrateChatLogs);
            let fetch_emotes_running  = self.background_tasks.iter().any(|t| t.kind == BTK::FetchMissingChatEmotes);
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

                    let chat_root_set = !self.settings.chat_log_root.trim().is_empty();
                    if ui
                        .add_enabled(chat_root_set && !chat_migrate_running, egui::Button::new("Migrate chat logs"))
                        .on_hover_text(
                            "One-shot sweep: move every EXISTING chat sidecar into the \
                             dedicated Chat logs folder (Recording → Defaults), mirroring \
                             each recording folder's structure there. Cross-drive, so each \
                             file is copied, size-verified, then deleted from the source — \
                             a failed verify leaves the original untouched. Sidecars of \
                             still-running sessions are skipped (run it again later), and \
                             every moved take keeps working in View chat. New takes \
                             already write to the chat folder directly; this is only the \
                             catch-up pass for files from before it was configured.",
                        )
                        .clicked()
                    {
                        self.core.manual(ManualCommand::MigrateChatLogs);
                    }
                    ui.label("Move existing chat sidecars into the dedicated chat logs folder.");
                    ui.end_row();

                    if ui
                        .add_enabled(!fetch_emotes_running, egui::Button::new("Fetch missing chat emotes"))
                        .on_hover_text(
                            "One-shot sweep: scan every archived Twitch chat log for first-party \
                             emote ids that don't render anywhere yet — not cached for that \
                             channel, not for any other monitored channel — and fetch each \
                             straight from Twitch's CDN by id, same as opening the chat would \
                             eventually do on its own. Useful for logs recorded before that \
                             on-demand fetch existed, or never opened since. Twitch only \
                             (YouTube chat has no first-party emote CDN to backfill from). \
                             Clicking this always fetches, regardless of the \"Fetch unknown \
                             emotes from Twitch\" display setting — that one only gates the \
                             passive per-chat-popup fetch; this button is its own explicit ask. \
                             A big archive can turn up hundreds of distinct missing ids in one \
                             run, so downloads are paced (150ms apart) and every miss across \
                             every log is deduplicated before any request goes out — one \
                             spammed emote across thousands of messages still costs exactly one \
                             fetch. Can take a while for months of logs; watch progress in the \
                             Background view.",
                        )
                        .clicked()
                    {
                        self.core.manual(ManualCommand::FetchMissingChatEmotes);
                    }
                    ui.label("Scan existing chat logs and fetch any first-party emotes still missing.");
                    ui.end_row();

                    if ui
                        .add_enabled(!join_cleanup_running, egui::Button::new("Re-run join cleanup"))
                        .on_hover_text(
                            "\"After full.mkv join\" is only applied at the moment a join lands, \
                             so changing it does nothing for streams joined earlier — those keep \
                             their head + live capture next to a full that already contains both, \
                             costing double the stream's size forever. This is the catch-up pass. \
                             Every take is re-verified first: the full.mkv is probed and must \
                             account for the parts still beside it, and anything that can't be \
                             verified is left completely alone. Parts go through the deletion \
                             method above, so they're recoverable from the Trash view.",
                        )
                        .clicked()
                    {
                        self.core.manual(ManualCommand::RerunJoinCleanup);
                    }
                    ui.label("Apply the current \"After full.mkv join\" setting to already-joined streams.");
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
                self.save_preset_dialog = Some(Arc::new(Mutex::new(SavePresetDraft {
                    kind: PresetKind::Filename,
                    template: tmpl,
                    name: String::new(),
                    error: String::new(),
                    do_save: false,
                    closed: false,
                })));
            }

            }
    }

    fn settings_db_backup_section(&mut self, ui: &mut egui::Ui) {
            if self.section_shown(SettingsTab::System, "Database backups", &["backup", "backups", "vacuum", "restore", "rolling", "snapshot", "incident"]) {
            ui.add_space(12.0);
            ui.heading("Database backups 🗄");
            ui.label(
                "Periodic, self-contained snapshots of the app database (channels, monitors, \
                 recording metadata, chapters, settings — not the video files themselves), so a \
                 destructive mistake or a corrupted database file has something recent to \
                 restore from instead of nothing.",
            );
            ui.add_space(6.0);
            ui.checkbox(&mut self.settings.db_backup_enabled, "Enable rolling backups")
                .on_hover_text(
                    "On by default. When off, no automatic backups are taken (existing ones \
                     are left alone — this only stops new ones).",
                );
            egui::Grid::new("db_backup_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Interval (hours)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.db_backup_interval_hours)
                            .desired_width(60.0),
                    )
                    .on_hover_text(format!(
                        "How often a new backup is taken. Empty or invalid defaults to {}.",
                        crate::db_backup::DEFAULT_INTERVAL_HOURS
                    ));
                    ui.end_row();

                    ui.label("Keep");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.db_backup_retention_count)
                            .desired_width(60.0),
                    )
                    .on_hover_text(format!(
                        "How many rolling backups to keep before the oldest is deleted. Empty \
                         or invalid defaults to {}.",
                        crate::db_backup::DEFAULT_RETENTION_COUNT
                    ));
                    ui.end_row();
                });
            ui.add_space(6.0);

            let last = crate::db_backup::last_run(&self.core.store);
            let now = crate::models::now_unix();
            let last_label = if last <= 0 {
                "never".to_string()
            } else {
                format!("{} ago", fmt_duration_secs((now - last).max(0)))
            };
            let backups = crate::db_backup::list_backups();
            let total_bytes: i64 = backups.iter().map(|(_, _, sz)| *sz as i64).sum();

            ui.horizontal(|ui| {
                if ui.button("Back up now").clicked() {
                    let keep = self
                        .settings
                        .db_backup_retention_count
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .filter(|v| *v > 0)
                        .unwrap_or(crate::db_backup::DEFAULT_RETENTION_COUNT);
                    match crate::db_backup::run_backup_now(now, keep) {
                        Ok(path) => {
                            self.status = format!("Backup written: {}", path.display());
                        }
                        Err(e) => self.status = format!("Backup failed: {e:#}"),
                    }
                }
                if ui.button("Open backups folder").clicked() {
                    crate::platform::open_path(&crate::app_paths::backups_dir());
                }
                ui.label(format!(
                    "Last backup: {last_label} · {} kept ({})",
                    backups.len(),
                    fmt_bytes(total_bytes)
                ));
            });
            }
    }

    /// Chat index: the switch, the pace, what it costs, and how to rebuild it.
    fn settings_chat_index_section(&mut self, ui: &mut egui::Ui) {
        if !self.section_shown(
            SettingsTab::System,
            "Chat index",
            &["chat", "index", "users", "chatter", "search", "fts", "messages", "presence"],
        ) {
            return;
        }
        ui.add_space(12.0);
        ui.heading("Chat index 👤");
        ui.label(
            "Reads finished chat logs in the background and records who chatted in which \
             stream, plus a full-text index of every message — the data behind the Users \
             view. Nothing is written while a stream is being captured: logs are only read \
             once a take has ended, behind the disk gate, so this never competes with a \
             recording.",
        );
        ui.add_space(6.0);
        ui.checkbox(&mut self.settings.chat_index_enabled, "Enable chat indexing")
            .on_hover_text(
                "On by default. Off stops all indexing immediately — reads, writes and the \
                 legacy-name lookups. Anything already indexed stays searchable; it just \
                 stops growing.",
            );

        let index = crate::chat_index::shared();
        let health = index.and_then(|i| i.health().ok());
        let total = self.core.store.chat_index_candidates().map(|v| v.len() as i64).unwrap_or(0);

        egui::Grid::new("chat_index_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Streams per sweep");
            ui.add(
                egui::TextEdit::singleline(&mut self.settings.chat_index_batch)
                    .desired_width(60.0),
            )
            .on_hover_text(format!(
                "How many chat logs one background pass reads (one pass a minute). Higher \
                 finishes the backlog sooner but puts more sustained load on the drive the \
                 logs live on. Empty or invalid defaults to 5; the ceiling is {}.",
                crate::chat_scan::INDEX_BATCH_MAX
            ));
            ui.end_row();

            if let Some(h) = &health {
                let done = h.takes_indexed + h.takes_failed;
                let remaining = total.saturating_sub(done);
                ui.label("Progress");
                if remaining > 0 {
                    ui.label(format!("{done} of {total} chat logs read — {remaining} to go"))
                        .on_hover_text(
                            "At the default pace this drains a large backlog over a few \
                             hours. Until it finishes, the Users view can be missing streams \
                             a chatter was really in, and says so.",
                        );
                } else if total > 0 {
                    ui.label(format!("all {total} chat logs read"));
                } else {
                    ui.label("no chat logs to read yet");
                }
                ui.end_row();

                ui.label("Contents");
                ui.label(format!(
                    "{} chatters · {} messages · {} appearances · {} on disk",
                    h.users,
                    h.messages,
                    h.presence_rows,
                    fmt_bytes(h.bytes_on_disk as i64)
                ))
                .on_hover_text(
                    "The index lives in its own database file (chat_index.sqlite3) — \
                     deliberately NOT inside the main database, so it never bloats the \
                     rolling backups and its writes can never block the app's own queries. \
                     It is rebuildable from the chat logs, so it is not backed up.",
                );
                ui.end_row();

                if h.takes_failed > 0 {
                    ui.label("Unreadable");
                    ui.label(
                        egui::RichText::new(format!("{} chat log(s) missing or unreadable", h.takes_failed))
                            .color(grid::HL_WARN_TEXT),
                    )
                    .on_hover_text(
                        "Chat logs that were deleted, moved, or written before chat logging \
                         existed. They are stamped so the queue can drain; those streams are \
                         simply not searchable.",
                    );
                    ui.end_row();
                }

                if h.unresolved_logins > 0 {
                    ui.label("Legacy names");
                    ui.label(format!("{} chatter(s) still keyed by name", h.unresolved_logins))
                        .on_hover_text(
                            "Twitch chat logs written before 2026-08-05 carry no account id, \
                             so those chatters are filed under their name. A background \
                             lookup folds them into real accounts 100 at a time. Until then \
                             — and for anyone since renamed — their history stays split.",
                        );
                    ui.end_row();
                }

                if h.slowest_ms > 0 {
                    ui.label("Slowest log");
                    ui.label(format!(
                        "{} ms (recording {})",
                        h.slowest_ms, h.slowest_rec_id
                    ))
                    .on_hover_text(
                        "The worst single chat log on record, start to finish. If this climbs \
                         into seconds, the app log has a line per indexed take with the parse \
                         and write split out.",
                    );
                    ui.end_row();
                }
            } else {
                ui.label("Status");
                ui.label(
                    egui::RichText::new("index unavailable — see the app log")
                        .color(grid::HL_ERROR_TEXT),
                );
                ui.end_row();
            }
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .button("Index all now")
                .on_hover_text(
                    "Raise the pace to the maximum so the backlog is read as fast as the \
                     disk gate allows, instead of a few logs a minute. Still yields to any \
                     running capture. Lower 'Streams per sweep' again to go back to a trickle.",
                )
                .clicked()
            {
                self.settings.chat_index_batch = crate::chat_scan::INDEX_BATCH_MAX.to_string();
                self.settings.chat_index_enabled = true;
                let ctx = ui.ctx().clone();
                self.save_settings(&ctx);
                self.status =
                    "Chat indexing set to full speed — it will still wait behind any capture."
                        .to_string();
            }
            if ui
                .button("Rebuild index")
                .on_hover_text(
                    "Throw the index away and read every chat log again from scratch. Useful \
                     if it is ever suspected of being wrong. Nothing else is affected — no \
                     recording, chat log or statistic is touched.",
                )
                .clicked()
            {
                match index {
                    Some(i) => match i.clear() {
                        Ok(()) => {
                            self.users_results.clear();
                            self.users_detail = None;
                            self.users_selected = None;
                            self.status =
                                "Chat index cleared — every chat log will be read again."
                                    .to_string();
                        }
                        Err(e) => self.status = format!("Could not clear the chat index: {e:#}"),
                    },
                    None => self.status = "The chat index is not available.".to_string(),
                }
            }
            if ui
                .button("👤 Open Users")
                .on_hover_text("Go to the Users view for expanded info.")
                .clicked()
            {
                self.view = View::Users;
            }
        });
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
                            // Async like every other Browse in the app — a
                            // picker run ON the UI thread blocks painting and
                            // the watchdog heartbeat.
                            self.pending_browse = Some(spawn_browse_file_filtered(
                                &self.settings.dialog_icon,
                                ("PNG images", &["png"]),
                                |app, p| app.settings.dialog_icon = p,
                            ));
                        }
                    });
                    ui.end_row();
                });
            } // end Diagnostics section guard
    }
}
