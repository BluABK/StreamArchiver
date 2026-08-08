//! The chat popup window itself — toolbar, appearance panel, usercard,
//! users panel, info strips, and the virtualized message list.

use super::*;

impl StreamArchiverApp {
    // ── Chat log viewer ──────────────────────────────────────────────────────

    /// Open the chat popup for a monitor. `rec_id` picks a specific recording
    /// (a take/stream row's "View chat"); `None` falls back to the most recent
    /// recording that has a chat file.
    pub(in crate::ui) fn open_chat_popup(&mut self, monitor_id: i64, rec_id: Option<i64>, ctx: &egui::Context) {
        let row = self.rows.iter().find(|r| r.monitor.id == monitor_id);
        let monitor_name = row.map(|r| r.channel.name.clone()).unwrap_or_default();
        let platform = row.map(|r| r.monitor.platform());
        // The emote/badge cache is per-ACCOUNT: this monitor's URL names which
        // account's assets to use (a channel can hold a main + alt Twitch).
        let account = row
            .map(|r| asset_account(&r.monitor.url, r.monitor.platform()))
            .unwrap_or_default();
        // Twitch: build the third-party emote map (BTTV/FFZ/7TV) once and point at
        // the first-party emote dir, plus every OTHER cached channel's first-party
        // dir as a fallback (any subscriber can use their sub emotes in any
        // channel's chat — see `twitch_fallback_index`'s doc). YouTube/others:
        // empty map, no dir (emotes come inline in the runs / aren't word-matched).
        let (emote_map, twitch_emote_dir, twitch_fallback_index) = if platform == Some(Platform::Twitch) {
            let dir = twitch_emotes_dir(&monitor_name, &account).join("twitch");
            let fallback_dirs: Vec<_> =
                crate::assets::all_twitch_emote_dirs().into_iter().filter(|d| *d != dir).collect();
            let index = crate::assets::index_emote_stems(&fallback_dirs);
            (Arc::new(build_emote_map(&monitor_name, &account)), Some(dir), Arc::new(index))
        } else {
            (Arc::new(HashMap::new()), None, Arc::new(HashMap::new()))
        };
        let twitch_badge_dirs = Arc::new(if platform == Some(Platform::Twitch) {
            TwitchBadgeDirs {
                channel: Some(twitch_badge_dir(&monitor_name, &account)),
                global: twitch_global_badge_dir(),
            }
        } else {
            TwitchBadgeDirs { channel: None, global: twitch_global_badge_dir() }
        });

        let recs = self
            .core
            .store
            .recordings_for_monitor(monitor_id)
            .unwrap_or_default();
        let rec = rec_id
            .and_then(|id| recs.iter().find(|r| r.id == id))
            .or_else(|| recs.iter().rev().find(|r| chat_file_for_recording(r).is_some()))
            .or_else(|| recs.last())
            .cloned();

        // This take's recorded collab partners, keyed by Twitch broadcaster id
        // — resolves each message's `source_room_id` tag to a name for the
        // "which channel was this from" indicator during a Shared Chat
        // session. Twitch-only; empty when this take has no stream id or no
        // collab was ever recorded for it (messages then render with no
        // indicator, same as a pre-feature log).
        let source_partners: Arc<HashMap<String, crate::models::CollabPartner>> = Arc::new(
            if platform == Some(Platform::Twitch) {
                rec.as_ref()
                    .and_then(|r| r.stream_id.as_deref())
                    .filter(|sid| !sid.is_empty())
                    .and_then(|sid| self.core.store.collab_partners_for_stream(monitor_id, sid).ok())
                    .map(|partners| {
                        partners
                            .into_iter()
                            .filter(|p| !p.id.is_empty())
                            .map(|p| (p.id.clone(), p))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                HashMap::new()
            },
        );

        // This broadcast's top-supporters leaderboard + Hype Train summary —
        // local DB query, no network. Twitch-only (subgift/bits/hype_train
        // are all Twitch chat-event kinds).
        let stats = if platform == Some(Platform::Twitch) {
            rec.as_ref()
                .map(|r| {
                    let since = r.went_live_at.unwrap_or(r.started_at);
                    let until = r.ended_at.unwrap_or_else(crate::models::now_unix);
                    load_broadcast_stats(&self.core.store, monitor_id, since, until)
                })
                .unwrap_or_default()
        } else {
            BroadcastStats::default()
        };

        let (fetch_unknown_emotes, render_emotes) = {
            let cs = self.chat_settings.lock().unwrap();
            (cs.fetch_unknown_emotes, cs.render_emotes)
        };
        let state = Arc::new(Mutex::new(ChatLoadState::Loading));
        let loading = Arc::new(AtomicBool::new(false));
        if let Some(r) = &rec {
            self.core.rt.spawn(load_chat(
                state.clone(),
                loading.clone(),
                chat_file_for_recording(r),
                r.went_live_at.unwrap_or(r.started_at),
                emote_map.clone(),
                twitch_emote_dir.clone(),
                twitch_fallback_index.clone(),
                fetch_unknown_emotes,
                render_emotes,
                source_partners.clone(),
                twitch_badge_dirs.clone(),
                ctx.clone(),
            ));
        } else {
            *state.lock().unwrap() = ChatLoadState::NoFile;
        }
        let popup = ChatPopup {
            monitor_id,
            monitor_name,
            is_twitch: platform == Some(Platform::Twitch),
            recording: rec,
            all_recordings: recs,
            load_state: state,
            search: String::new(),
            full_view: false,
            hide_shared: false,
            highlight_login: None,
            show_appearance: false,
            ts_color_hex: String::new(),
            text_color_hex: String::new(),
            user_card: None,
            users_panel: None,
            stats,
            // Both cards start open; the feature switches in Settings decide
            // whether they're available at all.
            show_hype: true,
            show_info: true,
            hype_seen_id: String::new(),
            last_reload: std::time::Instant::now(),
            emote_map,
            twitch_emote_dir,
            twitch_fallback_index,
            twitch_badge_dirs,
            source_partners,
            fetch_unknown_emotes,
            loading,
            error_retries: 0,
            filter_cache: None,
            settings: self.chat_settings.clone(),
            closed: false,
            decode_misses: Vec::new(),
            usercard_click: None,
        };
        // One chat window per monitor: re-targeting an already-open window
        // (e.g. "View chat" on another take) replaces its content in place;
        // a different monitor gets its own window.
        match self.chat_popups.iter_mut().find(|p| p.lock().unwrap().monitor_id == monitor_id) {
            Some(slot) => *slot.lock().unwrap() = popup,
            None => self.chat_popups.push(Arc::new(Mutex::new(popup))),
        }
    }

    #[allow(deprecated)]
    /// Render every open chat window (one OS viewport per monitor).
    pub(in crate::ui) fn chat_popup_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        for idx in 0..self.chat_popups.len() {
            if self.chat_popup_window(ctx, idx) {
                closed.push(self.chat_popups[idx].lock().unwrap().monitor_id);
            }
        }
        if !closed.is_empty() {
            self.chat_popups.retain(|p| !closed.contains(&p.lock().unwrap().monitor_id));
            if self.chat_popups.is_empty() {
                // Free all decoded emote frame textures once the last chat
                // window is gone.
                self.clear_emote_cache();
            }
        }
    }

    /// Render one chat window; returns true when the user closed it.
    #[allow(deprecated)]
    pub(in crate::ui) fn chat_popup_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        const CHAT_RELOAD_SECS: u64 = 3;
        let popup_arc = self.chat_popups[idx].clone();
        let mut popup = popup_arc.lock().unwrap();
        // Watchdog: name this phase so a freeze dialog points at the chat popup.
        self.heartbeat.set_context(format!("Chat: {}", popup.monitor_name));
        self.heartbeat.set_activity(crate::watchdog::Activity::Chat);
        let title = format!("💬  Chat — {}", popup.monitor_name);
        let vp_id = egui::ViewportId::from_hash_of(("chat_popup_vp", popup.monitor_id));

        // Whether the selected recording is still in progress (chat file is growing).
        let rec_active = popup.recording.as_ref().map_or(false, |r| r.ended_at.is_none());
        // An errored load retries with a FULL sidecar re-read — back that off
        // exponentially (3s → 6 → … → capped ~3min) instead of hammering the
        // recordings drive every tick. Loaded resets the ladder; NoFile stays
        // on the fast tick (retrying a missing file is one cheap stat, and the
        // sidecar usually appears seconds into a recording).
        let errored = matches!(&*popup.load_state.lock().unwrap(), ChatLoadState::Error(_));
        if !errored && matches!(&*popup.load_state.lock().unwrap(), ChatLoadState::Loaded(_)) {
            popup.error_retries = 0;
        }
        let reload_after = if errored {
            std::time::Duration::from_secs((CHAT_RELOAD_SECS << popup.error_retries.min(6)).min(180))
        } else {
            std::time::Duration::from_secs(CHAT_RELOAD_SECS)
        };
        // Collect everything needed for a tail-reload before the `show` closure
        // borrows `popup` so we can act on it cleanly afterwards.
        type ReloadInfo = (
            std::path::PathBuf,
            i64,
            Arc<Mutex<ChatLoadState>>,
            Arc<HashMap<String, std::path::PathBuf>>,
            Option<std::path::PathBuf>,
            Arc<HashMap<String, std::path::PathBuf>>,
            bool,
            Arc<AtomicBool>,
            Arc<HashMap<String, crate::models::CollabPartner>>,
            Arc<TwitchBadgeDirs>,
        );
        let reload_info: Option<ReloadInfo> =
            if rec_active && popup.last_reload.elapsed() >= reload_after {
                // Sidecar located via the probe cache: this runs on the UI
                // thread every 3s per live popup, and a direct stat against
                // the recordings drive can block the frame for seconds.
                let mut fs_guard = self.fs_probes.lock().unwrap();
                let fs = &mut *fs_guard;
                popup.recording.as_ref().and_then(|r| {
                    chat_file_for_recording_cached(fs, r).map(|path| {
                        (
                            path,
                            r.went_live_at.unwrap_or(r.started_at),
                            popup.load_state.clone(),
                            popup.emote_map.clone(),
                            popup.twitch_emote_dir.clone(),
                            popup.twitch_fallback_index.clone(),
                            popup.fetch_unknown_emotes,
                            popup.loading.clone(),
                            popup.source_partners.clone(),
                            popup.twitch_badge_dirs.clone(),
                        )
                    })
                })
            } else {
                None
            };

        // The emote cache is shared (Arc<Mutex>), so the closure can use a clone
        // without borrowing `self`. Copy the render toggles out too.
        let anim_cache = self.emote_anim.clone();
        // Uploaded on first use and refcounted, so this clone is free. Moved
        // into the closure because the deferred render can't borrow `self`.
        let ui_tex = self.ui_tex.get_or_insert_with(|| UiTextures::load(ctx)).clone();
        let (render_emotes, animate_emotes, appearance) = {
            let cs = self.chat_settings.lock().unwrap();
            (
                cs.render_emotes,
                cs.animate_emotes,
                ChatAppearance {
                    font_pt: cs.font_pt,
                    emote_pt: cs.emote_pt,
                    ts_color: cs.ts_color,
                    text_color: cs.text_color,
                    font_id: font_name_key(&cs.chat_font),
                    ts_mode: cs.ts_mode,
                },
            )
        };
        // Animation clock for the LRU/decode bookkeeping the *wrapper* does
        // below. The emotes themselves must NOT use this one — see the
        // closure's own `now`.
        let wrapper_now = ctx.input(|i| i.time);

        // Release the lock before registering the deferred closure — it
        // takes its own lock on the SAME Arc each time it repaints, which
        // would deadlock against this one if it were still held.
        drop(popup);
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            vp_id,
            egui::ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size([480.0, 600.0]),
            popup_arc.clone(),
            shared,
            move |ctx, popup, shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    popup.closed = true;
                }
                // The global animation clock — all instances of an emote animate
                // in lockstep. Read HERE, inside the deferred closure, not in the
                // wrapper: this closure repaints on the popup viewport's own
                // schedule, so a captured value would be frozen at whatever time
                // the root's last frame had. That is exactly what made animated
                // emotes advance one frame per root repaint (≈1 fps idle, worse
                // under load) while the popup itself kept redrawing the same
                // frame at full speed — sluggish AND stuck.
                let now = ctx.input(|i| i.time);
                // Consumed at the end of this closure into `popup.decode_misses`/
                // `popup.usercard_click` — the deferred closure doesn't run
                // synchronously with the wrapper call, so these can't be plain
                // captured locals the wrapper reads back after the call returns.
                let mut decode_misses: Vec<std::path::PathBuf> = Vec::new();
                let mut usercard_click: Option<UserCardClick> = None;
                egui::CentralPanel::default().show(ctx, |ui| {
                    // ── Toolbar ──────────────────────────────────────────────
                    ui.horizontal(|ui| {
                        // Recording picker: only if >1 recording has a chat file.
                        // Probe-cache lookups: this filter re-runs EVERY FRAME
                        // over the monitor's whole take history (4 candidate
                        // paths each) — direct stats here were measured in the
                        // thousands per second against the recordings drive.
                        let recs_with_chat: Vec<_> = {
                            let mut fs_guard = shared.fs_probes.lock().unwrap();
                            popup
                                .all_recordings
                                .iter()
                                .filter(|r| chat_file_for_recording_cached(&mut fs_guard, r).is_some())
                                .collect()
                            // `fs_guard` dropped here — the rest of this closure
                            // (recording-switch handler, etc.) may take its own
                            // `self.fs_probes` lock elsewhere; a `std::sync::Mutex`
                            // is not reentrant.
                        };
                        if recs_with_chat.len() > 1 {
                            let cur_label = popup
                                .recording
                                .as_ref()
                                .map(fmt_recording_label)
                                .unwrap_or_default();
                            egui::ComboBox::from_id_salt("chat_rec_pick")
                                .selected_text(cur_label)
                                .show_ui(ui, |ui| {
                                    for rec in &recs_with_chat {
                                        let label = fmt_recording_label(rec);
                                        let selected = popup
                                            .recording
                                            .as_ref()
                                            .map(|r| r.id == rec.id)
                                            .unwrap_or(false);
                                        if ui.selectable_label(selected, &label).clicked() {
                                            let new_rec = (*rec).clone();
                                            let state = Arc::new(Mutex::new(ChatLoadState::Loading));
                                            let path = chat_file_for_recording(&new_rec);
                                            let start_ts =
                                                new_rec.went_live_at.unwrap_or(new_rec.started_at);
                                            let emap = popup.emote_map.clone();
                                            let tdir = popup.twitch_emote_dir.clone();
                                            let tfallback = popup.twitch_fallback_index.clone();
                                            let bdirs = popup.twitch_badge_dirs.clone();
                                            let funknown = popup.fetch_unknown_emotes;
                                            // A different recording is a
                                            // different broadcast — its
                                            // Shared Chat partners (if any)
                                            // aren't the same set.
                                            let source_partners: Arc<HashMap<String, crate::models::CollabPartner>> =
                                                Arc::new(
                                                    new_rec
                                                        .stream_id
                                                        .as_deref()
                                                        .filter(|sid| !sid.is_empty())
                                                        .and_then(|sid| {
                                                            shared
                                                                .core
                                                                .store
                                                                .collab_partners_for_stream(popup.monitor_id, sid)
                                                                .ok()
                                                        })
                                                        .map(|partners| {
                                                            partners
                                                                .into_iter()
                                                                .filter(|p| !p.id.is_empty())
                                                                .map(|p| (p.id.clone(), p))
                                                                .collect()
                                                        })
                                                        .unwrap_or_default(),
                                                );
                                            popup.source_partners = source_partners.clone();
                                            // A different recording is a different
                                            // broadcast — its leaderboard/Hype Train
                                            // history is scoped to its own time span.
                                            popup.stats = if popup.is_twitch {
                                                let since = start_ts;
                                                let until =
                                                    new_rec.ended_at.unwrap_or_else(crate::models::now_unix);
                                                load_broadcast_stats(&shared.core.store, popup.monitor_id, since, until)
                                            } else {
                                                BroadcastStats::default()
                                            };
                                            popup.load_state = state.clone();
                                            popup.recording = Some(new_rec);
                                            popup.last_reload = std::time::Instant::now();
                                            // Keyed on (query, count) only — a
                                            // different log with the same count
                                            // would reuse stale match indices.
                                            popup.filter_cache = None;
                                            shared.core.rt.spawn(load_chat(
                                                state,
                                                popup.loading.clone(),
                                                path,
                                                start_ts,
                                                emap,
                                                tdir,
                                                tfallback,
                                                funknown,
                                                render_emotes,
                                                source_partners,
                                                bdirs,
                                                ctx.clone(),
                                            ));
                                        }
                                    }
                                });
                            ui.separator();
                        }

                        // Search filter
                        ui_icon(ui, Some(&ui_tex), ICON_SEARCH, 14.0, ui.visuals().weak_text_color());
                        ui.add(
                            egui::TextEdit::singleline(&mut popup.search)
                                .hint_text("Filter…")
                                .desired_width(150.0),
                        );
                        if !popup.search.is_empty()
                            && icon_button(ui, Some(&ui_tex), ICON_CLOSE, 12.0, "Clear the filter.")
                                .clicked()
                        {
                            popup.search.clear();
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.toggle_value(&mut popup.full_view, "View full");
                            if icon_button(
                                ui,
                                Some(&ui_tex),
                                ICON_SETTINGS,
                                16.0,
                                "Chat appearance: font size and colors",
                            )
                            .clicked()
                            {
                                popup.show_appearance = !popup.show_appearance;
                                if popup.show_appearance {
                                    let cs = popup.settings.lock().unwrap();
                                    let (ts, tx) = (cs.ts_color, cs.text_color);
                                    drop(cs);
                                    popup.ts_color_hex = hex_color_string(ts);
                                    popup.text_color_hex = hex_color_string(tx);
                                }
                            }
                            let mut users_open = popup.users_panel.is_some();
                            if icon_toggle(
                                ui,
                                &mut users_open,
                                Some(&ui_tex),
                                ICON_USERS,
                                16.0,
                                "Users in chat (from this log)",
                            )
                            .changed()
                            {
                                popup.users_panel = if popup.users_panel.is_some() {
                                    None
                                } else {
                                    Some(UsersPanelState {
                                        filter: String::new(),
                                        entries: Vec::new(),
                                        built_at_count: 0,
                                    })
                                };
                            }
                            // Timestamp format. A toggle rather than a Settings
                            // field because both answers are right at different
                            // moments: the wall clock while watching a live
                            // broadcast, the stream-relative offset when you
                            // need to seek the local recording to a moment.
                            // (The other format is also on each timestamp's
                            // hover, so a one-off check needs no click at all.)
                            {
                                let cur = popup.settings.lock().unwrap().ts_mode;
                                let mut wall = cur == ChatTsMode::WallClock;
                                // Hover describes the CURRENT state and what a
                                // click does, so it must read `cur`, not the
                                // `wall` the toggle is about to write through.
                                let hint = if wall {
                                    "Showing wall-clock time. Click for time into the \
                                     broadcast, which is what you need to seek the recording."
                                } else {
                                    "Showing time into the broadcast. Click for wall-clock \
                                     time, as Twitch's own chat shows."
                                };
                                if icon_toggle(ui, &mut wall, Some(&ui_tex), ICON_CLOCK, 16.0, hint)
                                    .changed()
                                {
                                    let mode = if wall {
                                        ChatTsMode::WallClock
                                    } else {
                                        ChatTsMode::StreamRelative
                                    };
                                    popup.settings.lock().unwrap().ts_mode = mode;
                                    let _ = shared
                                        .core
                                        .store
                                        .set_setting(K_CHAT_TS_MODE, mode.as_str());
                                }
                            }
                            // The two info-card toggles. Rendered whenever this
                            // is a Twitch window and the card's feature switch
                            // is on, and DISABLED (rather than hidden) when the
                            // broadcast has nothing to put in that card — a
                            // toolbar that reflows every time a Hype Train
                            // starts or ends is worse than a greyed button.
                            if popup.is_twitch {
                                let (want_hype, want_info) = {
                                    let cs = popup.settings.lock().unwrap();
                                    (cs.show_hype_train, cs.show_channel_info)
                                };
                                if want_hype {
                                    let has = popup.stats.hype_train.is_some();
                                    ui.add_enabled_ui(has, |ui| {
                                        icon_toggle(
                                            ui,
                                            &mut popup.show_hype,
                                            Some(&ui_tex),
                                            ICON_TRAIN,
                                            16.0,
                                            if has {
                                                "Show this broadcast's Hype Train. A new train \
                                                 re-opens this even after you close it; turn the \
                                                 card off entirely in Settings → Interface."
                                            } else {
                                                "No Hype Train recorded for this broadcast."
                                            },
                                        );
                                    });
                                }
                                if want_info {
                                    let has = !popup.stats.top_gifters.is_empty()
                                        || !popup.stats.top_cheerers.is_empty();
                                    ui.add_enabled_ui(has, |ui| {
                                        icon_toggle(
                                            ui,
                                            &mut popup.show_info,
                                            Some(&ui_tex),
                                            ICON_GIFT,
                                            16.0,
                                            if has {
                                                "Show this broadcast's top supporters."
                                            } else {
                                                "No gift subs or bits recorded for this broadcast."
                                            },
                                        );
                                    });
                                }
                            }
                            ui.checkbox(&mut popup.hide_shared, "Hide shared")
                                .on_hover_text(
                                    "During an active Shared Chat session, hide messages that \
                                     came from another channel — show only this channel's own \
                                     messages. Useful when a merged chat is too noisy to follow.",
                                );
                        });
                    });
                    ui.separator();

                    if popup.show_appearance {
                        let (mut font_pt, mut emote_pt, mut ts_color, mut text_color) = {
                            let cs = popup.settings.lock().unwrap();
                            (cs.font_pt, cs.emote_pt, cs.ts_color, cs.text_color)
                        };
                        egui::Window::new("Chat Appearance")
                            .id(egui::Id::new(("chat_appearance_win", popup.monitor_id)))
                            .collapsible(false)
                            .resizable(false)
                            .default_pos(egui::pos2(120.0, 60.0))
                            .show(ctx, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Font size:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut font_pt)
                                                .range(8.0..=32.0)
                                                .suffix(" pt"),
                                        )
                                        .on_hover_text(
                                            "Exact point size for the timestamp, message text, \
                                             and username — applies to every open chat window.",
                                        )
                                        .changed()
                                    {
                                        popup.settings.lock().unwrap().font_pt = font_pt;
                                        let _ = shared.core.store.set_setting(
                                            K_CHAT_FONT_PT,
                                            &font_pt.to_string(),
                                        );
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Emote size:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut emote_pt)
                                                .range(12.0..=64.0)
                                                .suffix(" px"),
                                        )
                                        .on_hover_text(
                                            "Pixel size for emotes and emoji in the chat replay \
                                             — independent of the text font size, applies to \
                                             every open chat window.",
                                        )
                                        .changed()
                                    {
                                        popup.settings.lock().unwrap().emote_pt = emote_pt;
                                        let _ = shared.core.store.set_setting(
                                            K_CHAT_EMOTE_PT,
                                            &emote_pt.to_string(),
                                        );
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Timestamp color:");
                                    let wheel_changed = egui::color_picker::color_edit_button_srgba(
                                        ui,
                                        &mut ts_color,
                                        egui::color_picker::Alpha::Opaque,
                                    )
                                    .on_hover_text("Color of the [hh:mm:ss] timestamp prefix.")
                                    .changed();
                                    if wheel_changed {
                                        popup.ts_color_hex = hex_color_string(ts_color);
                                    }
                                    // Egui's color-wheel popup only offers a "copy"
                                    // button (RGB numbers, not hex) with no matching
                                    // paste target — this hex field is that missing
                                    // paste target: type or paste a `#RRGGBB` value
                                    // directly, applied as soon as it parses.
                                    let hex_changed = ui
                                        .add(
                                            egui::TextEdit::singleline(&mut popup.ts_color_hex)
                                                .desired_width(64.0)
                                                .hint_text("#RRGGBB"),
                                        )
                                        .on_hover_text(
                                            "Type or paste a hex color (e.g. #FFFFFF) — applies \
                                             as soon as it's a valid 6-digit hex value.",
                                        )
                                        .changed();
                                    if hex_changed
                                        && let Some(parsed) = parse_chat_hex_color(popup.ts_color_hex.trim())
                                    {
                                        ts_color = parsed;
                                    }
                                    let ts_color_was = popup.settings.lock().unwrap().ts_color;
                                    if wheel_changed || (hex_changed && ts_color != ts_color_was) {
                                        popup.settings.lock().unwrap().ts_color = ts_color;
                                        let _ = shared.core.store.set_setting(
                                            K_CHAT_TS_COLOR,
                                            &hex_color_string(ts_color),
                                        );
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Message color:");
                                    let wheel_changed = egui::color_picker::color_edit_button_srgba(
                                        ui,
                                        &mut text_color,
                                        egui::color_picker::Alpha::Opaque,
                                    )
                                    .on_hover_text("Color of the message body text.")
                                    .changed();
                                    if wheel_changed {
                                        popup.text_color_hex = hex_color_string(text_color);
                                    }
                                    let hex_changed = ui
                                        .add(
                                            egui::TextEdit::singleline(&mut popup.text_color_hex)
                                                .desired_width(64.0)
                                                .hint_text("#RRGGBB"),
                                        )
                                        .on_hover_text(
                                            "Type or paste a hex color (e.g. #FFFFFF) — applies \
                                             as soon as it's a valid 6-digit hex value.",
                                        )
                                        .changed();
                                    if hex_changed
                                        && let Some(parsed) = parse_chat_hex_color(popup.text_color_hex.trim())
                                    {
                                        text_color = parsed;
                                    }
                                    let text_color_was = popup.settings.lock().unwrap().text_color;
                                    if wheel_changed || (hex_changed && text_color != text_color_was) {
                                        popup.settings.lock().unwrap().text_color = text_color;
                                        let _ = shared.core.store.set_setting(
                                            K_CHAT_TEXT_COLOR,
                                            &hex_color_string(text_color),
                                        );
                                    }
                                });
                                ui.add_space(4.0);
                                if ui
                                    .button("Reset to defaults")
                                    .on_hover_text("Restore the default 14pt / 24px white/white appearance.")
                                    .clicked()
                                {
                                    {
                                        let mut cs = popup.settings.lock().unwrap();
                                        cs.font_pt = CHAT_FONT_PT_DEFAULT;
                                        cs.emote_pt = CHAT_EMOTE_PT_DEFAULT;
                                        cs.ts_color = egui::Color32::WHITE;
                                        cs.text_color = egui::Color32::WHITE;
                                    }
                                    popup.ts_color_hex = hex_color_string(egui::Color32::WHITE);
                                    popup.text_color_hex = hex_color_string(egui::Color32::WHITE);
                                    let _ = shared.core.store.set_setting(
                                        K_CHAT_FONT_PT,
                                        &CHAT_FONT_PT_DEFAULT.to_string(),
                                    );
                                    let _ = shared.core.store.set_setting(
                                        K_CHAT_EMOTE_PT,
                                        &CHAT_EMOTE_PT_DEFAULT.to_string(),
                                    );
                                    let _ = shared.core.store.set_setting(K_CHAT_TS_COLOR, "#FFFFFF");
                                    let _ = shared.core.store.set_setting(K_CHAT_TEXT_COLOR, "#FFFFFF");
                                }
                            });
                    }

                    // ── Usercard ─────────────────────────────────────────────
                    if let Some(card) = &popup.user_card {
                        let mut open = true;
                        egui::Window::new(format!("👤 {}", card.display_name))
                            .id(egui::Id::new(("chat_usercard_win", popup.monitor_id)))
                            .collapsible(false)
                            .resizable(true)
                            .default_width(340.0)
                            .open(&mut open)
                            .show(ctx, |ui| {
                                let banner_color = card
                                    .color
                                    .unwrap_or_else(|| twitch_username_color(&card.display_name));
                                paint_user_banner(ui, banner_color, 40.0);
                                ui.horizontal(|ui| {
                                    // Avatar (live lookup) — reuses the same
                                    // decode/GPU-upload cache as emotes/badges.
                                    let avatar_drawn = if let UserCardFetch::Loaded {
                                        avatar_path: Some(p), ..
                                    } = &*card.fetch.lock().unwrap()
                                    {
                                        draw_cached_emote(
                                            ui,
                                            &anim_cache,
                                            p,
                                            false,
                                            64.0,
                                            now,
                                            &mut decode_misses,
                                            ctx,
                                        )
                                        .is_some()
                                    } else {
                                        false
                                    };
                                    if !avatar_drawn {
                                        ui.allocate_ui(egui::vec2(64.0, 64.0), |ui| {
                                            ui.centered_and_justified(|ui| {
                                                ui_icon(
                                                    ui,
                                                    Some(&ui_tex),
                                                    ICON_USER,
                                                    40.0,
                                                    ui.visuals().weak_text_color(),
                                                )
                                            });
                                        });
                                    }
                                    ui.vertical(|ui| {
                                        let base = card.color.unwrap_or_else(|| {
                                            twitch_username_color(&card.display_name)
                                        });
                                        let color = readable_color(base, ui.visuals().panel_fill);
                                        ui.label(
                                            egui::RichText::new(&card.display_name)
                                                .strong()
                                                .size(16.0)
                                                .color(color),
                                        );
                                        ui.horizontal(|ui| {
                                            for (i, badge) in card.badges.iter().enumerate() {
                                                let icon =
                                                    card.badge_icons.get(i).and_then(|o| o.as_ref());
                                                let drawn = icon.and_then(|path| {
                                                    draw_cached_emote(
                                                        ui,
                                                        &anim_cache,
                                                        path,
                                                        false,
                                                        18.0,
                                                        now,
                                                        &mut decode_misses,
                                                        ctx,
                                                    )
                                                });
                                                if let Some((resp, _)) = drawn {
                                                    resp.on_hover_text(badge_label(badge));
                                                } else {
                                                    let (sym, c) =
                                                        badge_display(badge, &ChatPlatform::Twitch);
                                                    ui.label(egui::RichText::new(sym).color(c))
                                                        .on_hover_text(badge_label(badge));
                                                }
                                            }
                                        });
                                    });
                                });
                                ui.separator();
                                egui::Grid::new("usercard_grid").num_columns(2).show(ui, |ui| {
                                    if let Some((set_id, months)) = card
                                        .badge_info
                                        .split_once('/')
                                        .filter(|(s, _)| *s == "subscriber")
                                    {
                                        let _ = set_id;
                                        let tier = card
                                            .badges
                                            .iter()
                                            .find(|b| b.starts_with("subscriber/"))
                                            .and_then(|b| b.split('/').nth(1))
                                            .and_then(|v| v.parse::<i64>().ok())
                                            .map(|v| if v >= 3000 { 3 } else if v >= 2000 { 2 } else { 1 })
                                            .unwrap_or(1);
                                        ui.label("Subscriber:");
                                        ui.label(format!("Tier {tier} · {months} month(s)"));
                                        ui.end_row();
                                    }
                                    ui.label("Messages in this log:");
                                    ui.label(card.message_count.to_string());
                                    ui.end_row();
                                    if !card.user_id.is_empty() {
                                        let (label, tip) = match card.platform {
                                            ChatPlatform::Twitch => (
                                                "User ID:",
                                                "Twitch's numeric account id — stable across name changes.",
                                            ),
                                            ChatPlatform::YouTube => (
                                                "Channel ID:",
                                                "YouTube's channel id for this chatter — stable across name changes.",
                                            ),
                                        };
                                        ui.label(label).on_hover_text(tip);
                                        ui.label(&card.user_id);
                                        ui.end_row();
                                    }
                                    if let Some(secs) = card.first_seen_secs {
                                        ui.label("First seen:");
                                        ui.label(fmt_chat_ts(secs));
                                        ui.end_row();
                                    }
                                    // Twitch-only (Helix); a YouTube card never
                                    // makes a request, so the row would only
                                    // ever read "N/A".
                                    if card.platform == ChatPlatform::Twitch {
                                        ui.label("Account created:");
                                        let created = match &*card.fetch.lock().unwrap() {
                                            UserCardFetch::Loaded { created_at: Some(c), .. } => {
                                                chrono::DateTime::parse_from_rfc3339(c)
                                                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                                                    .unwrap_or_else(|_| c.clone())
                                            }
                                            UserCardFetch::Loading => "…".to_string(),
                                            _ => "N/A".to_string(),
                                        };
                                        ui.label(created);
                                        ui.end_row();
                                    }
                                });

                                // ── Moderation record ────────────────────────
                                usercard_moderation_section(ui, card);

                                // Cross-referenced against this channel's locally-recorded
                                // event history (bits/gifts/raids/timeouts) — see
                                // `summarize_user_events`'s doc. Local-only, no network.
                                if !card.channel_stats.is_empty() {
                                    ui.separator();
                                    ui.label(egui::RichText::new("This channel:").weak());
                                    for line in &card.channel_stats {
                                        ui.label(line);
                                    }
                                }

                                // A local "recent activity" feed — this user's own messages
                                // from the currently-loaded log, newest at the bottom.
                                if !card.recent_messages.is_empty() {
                                    ui.separator();
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Recent messages in this log ({}):",
                                            card.recent_messages.len()
                                        ))
                                        .weak(),
                                    );
                                    egui::ScrollArea::vertical()
                                        .id_salt("usercard_recent_messages")
                                        .max_height(150.0)
                                        .auto_shrink([false, true])
                                        .stick_to_bottom(true)
                                        .show(ui, |ui| {
                                            for (ts, text) in &card.recent_messages {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 3.0;
                                                    ui.label(
                                                        egui::RichText::new(fmt_chat_ts(*ts))
                                                            .monospace()
                                                            .small()
                                                            .weak(),
                                                    );
                                                    ui.label(egui::RichText::new(text).small());
                                                });
                                            }
                                        });
                                }

                                ui.separator();
                                ui.horizontal(|ui| {
                                    // Highlighting keys on the same identity
                                    // the log rows carry, so it works for a
                                    // YouTube chatter (no login) too.
                                    let key = if card.login.is_empty() {
                                        card.user_id.clone()
                                    } else {
                                        card.login.clone()
                                    };
                                    let highlighted =
                                        popup.highlight_login.as_deref() == Some(key.as_str());
                                    let mut hl = highlighted;
                                    if icon_toggle(
                                        ui,
                                        &mut hl,
                                        Some(&ui_tex),
                                        ICON_BELL,
                                        16.0,
                                        "Highlight messages of this user",
                                    )
                                    .changed()
                                    {
                                        popup.highlight_login =
                                            if highlighted { None } else { Some(key.clone()) };
                                    }
                                    let copy = if card.login.is_empty() {
                                        card.display_name.clone()
                                    } else {
                                        card.login.clone()
                                    };
                                    if ui
                                        .button("Copy username")
                                        .on_hover_text(
                                            "Copy this user's name to the clipboard (their login \
                                             on Twitch, their display name on YouTube)",
                                        )
                                        .clicked()
                                    {
                                        ctx.copy_text(copy);
                                    }
                                    match card.platform {
                                        ChatPlatform::Twitch => {
                                            if ui
                                                .button("Open Twitch profile")
                                                .on_hover_text("Open twitch.tv/{login} in your browser")
                                                .clicked()
                                            {
                                                crate::platform::open_url(&format!(
                                                    "https://twitch.tv/{}",
                                                    card.login
                                                ));
                                            }
                                        }
                                        ChatPlatform::YouTube => {
                                            if ui
                                                .add_enabled(
                                                    !card.user_id.is_empty(),
                                                    egui::Button::new("Open YouTube channel"),
                                                )
                                                .on_hover_text(
                                                    "Open this chatter's YouTube channel in your browser",
                                                )
                                                .clicked()
                                            {
                                                crate::platform::open_url(&format!(
                                                    "https://www.youtube.com/channel/{}",
                                                    card.user_id
                                                ));
                                            }
                                        }
                                    }
                                });
                            });
                        if !open {
                            popup.user_card = None;
                        }
                    }

                    // ── Users in chat ────────────────────────────────────────
                    if let Some(panel) = &mut popup.users_panel {
                        // Rebuild whenever the log has grown since the last
                        // build (a live tail-reload appended new messages) —
                        // cheap staleness check, not a per-frame rescan.
                        let count = match &*popup.load_state.lock().unwrap() {
                            ChatLoadState::Loaded(log) => log.messages.len(),
                            _ => 0,
                        };
                        if count != panel.built_at_count {
                            panel.entries = match &*popup.load_state.lock().unwrap() {
                                ChatLoadState::Loaded(log) => build_users_panel(log),
                                _ => Vec::new(),
                            };
                            panel.built_at_count = count;
                        }
                        let mut open = true;
                        let mut clicked: Option<UserCardClick> = None;
                        egui::Window::new("👥 Users in chat")
                            .id(egui::Id::new(("chat_users_panel_win", popup.monitor_id)))
                            .collapsible(false)
                            .resizable(true)
                            .default_width(220.0)
                            .default_height(400.0)
                            .open(&mut open)
                            .show(ctx, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut panel.filter)
                                        .hint_text("Filter…")
                                        .desired_width(f32::INFINITY),
                                )
                                .on_hover_text("Narrow the list by username (case-insensitive).");
                                ui.separator();
                                let q = panel.filter.to_lowercase();
                                egui::ScrollArea::vertical().auto_shrink([false, false]).show(
                                    ui,
                                    |ui| {
                                        let mut last_role: Option<&str> = None;
                                        for entry in panel.entries.iter().filter(|e| {
                                            q.is_empty() || e.click.display_name.to_lowercase().contains(&q)
                                        }) {
                                            if last_role != Some(entry.role) {
                                                ui.add_space(if last_role.is_some() { 6.0 } else { 0.0 });
                                                ui.label(egui::RichText::new(entry.role).weak().strong());
                                                last_role = Some(entry.role);
                                            }
                                            // Same contrast adjustment as the chat rows
                                            // themselves (`chat_username_color`) — an
                                            // unadjusted dark USERCOLOR (navy, dark green,
                                            // etc.) is hard to read on this panel's dark
                                            // background otherwise.
                                            let base = entry
                                                .click
                                                .color
                                                .unwrap_or_else(|| twitch_username_color(&entry.click.display_name));
                                            let color = readable_color(base, ui.visuals().panel_fill);
                                            if ui
                                                .add(
                                                    egui::Label::new(
                                                        egui::RichText::new(&entry.click.display_name)
                                                            .color(color),
                                                    )
                                                    .sense(egui::Sense::click()),
                                                )
                                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                .on_hover_text("Click for user info")
                                                .clicked()
                                            {
                                                clicked = Some(entry.click.clone());
                                            }
                                        }
                                    },
                                );
                            });
                        if let Some(c) = clicked {
                            usercard_click = Some(c);
                        }
                        if !open {
                            popup.users_panel = None;
                        }
                    }

                    // ── Info cards (channel info / Hype Train) ───────────────
                    // Twitch's own layout: a supporters strip and a Hype Train
                    // indicator sit above the message list. Built entirely from
                    // `stream_event` (see `load_broadcast_stats`'s doc) — no live
                    // carousel/train capture exists, so this is a local
                    // reconstruction: the leaderboard won't match Twitch's exact
                    // carousel (no follow/viewer-count data available to us).
                    //
                    // `live_view`: the auto-hide of a finished train applies only
                    // while following a still-running recording — see
                    // `hype_phase`'s doc for why an archived take must keep it.
                    let live_view = popup.recording.as_ref().is_some_and(|r| r.ended_at.is_none());
                    if chat_info_cards(
                        ui,
                        popup,
                        Some(&ui_tex),
                        live_view,
                        crate::models::now_unix(),
                    ) {
                        // A running countdown / an ended train's grace window.
                        // Requested HERE, inside the deferred closure, so it goes
                        // to this popup's viewport and not the root's.
                        ctx.request_repaint_after(std::time::Duration::from_secs(1));
                    }

                    // ── Content ──────────────────────────────────────────────
                    // Render straight from the mutex guard — the old code
                    // cloned the entire parsed log (every message + segments)
                    // every single frame.
                    let mut guard = popup.load_state.lock().unwrap();
                    match &mut *guard {
                        ChatLoadState::Loading => {
                            ui.horizontal(|ui| {
                                // Drives the repaints that poll the load too —
                                // an extra zero-delay `request_repaint` here
                                // would just free-run the viewport.
                                throttled_spinner(ui);
                                ui.label("Loading chat…");
                            });
                        }
                        ChatLoadState::NoFile => {
                            ui.add_space(8.0);
                            ui.label("No chat file found for this recording.");
                            ui.weak("Chat logging must be enabled and a recording must exist.");
                        }
                        ChatLoadState::Error(e) => {
                            ui.colored_label(egui::Color32::RED, format!("Failed to load: {e}"));
                        }
                        ChatLoadState::Loaded(log) => {
                            // Keep the height cache aligned with the message
                            // list: tail appends get estimates at the end; a
                            // shrink (recording switch) resets everything.
                            let n = log.messages.len();
                            if log.row_heights.len() > n {
                                log.row_heights.clear();
                            }
                            log.row_heights.resize(n, CHAT_ROW_EST);

                            // Search filter + "Hide shared" filter, recomputed only
                            // when the query, message count, or hide_shared changes
                            // — not every frame.
                            let q = popup.search.to_lowercase();
                            let hide_shared = popup.hide_shared;
                            if q.is_empty() && !hide_shared {
                                popup.filter_cache = None;
                            } else {
                                let stale = popup
                                    .filter_cache
                                    .as_ref()
                                    .is_none_or(|(cq, cn, ch, _)| *cq != q || *cn != n || *ch != hide_shared);
                                if stale {
                                    let idx: Vec<u32> = log
                                        .messages
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, m)| {
                                            (q.is_empty()
                                                || m.text.to_lowercase().contains(&q)
                                                || m.author.to_lowercase().contains(&q))
                                                && (!hide_shared || m.source_name.is_empty())
                                        })
                                        .map(|(i, _)| i as u32)
                                        .collect();
                                    popup.filter_cache = Some((q.clone(), n, hide_shared, idx));
                                }
                            }
                            let filtered: Option<&[u32]> =
                                popup.filter_cache.as_ref().map(|(_, _, _, v)| v.as_slice());
                            let count = filtered.map_or(n, |v| v.len());

                            ui.horizontal(|ui| {
                                ui.weak(format!("{count} messages"));
                                if log.loading_older {
                                    throttled_spinner(ui);
                                    ui.weak("loading older messages…");
                                }
                            });

                            let stick = q.is_empty() && !popup.full_view;
                            const GAP: f32 = 2.0;
                            const OVERSCAN: f32 = 300.0;
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .stick_to_bottom(stick)
                                .show_viewport(ui, |ui, viewport| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    // Wrapping depends on width, and row height
                                    // on the appearance settings — either
                                    // changing re-measures everything. See
                                    // `ChatLog::measured_key`.
                                    let w = ui.available_width();
                                    let key = (w, appearance.layout_key());
                                    if (w - log.measured_key.0).abs() > 0.5
                                        || key.1 != log.measured_key.1
                                    {
                                        log.measured_key = key;
                                        for h in &mut log.row_heights {
                                            *h = CHAT_ROW_EST;
                                        }
                                    }
                                    // One cheap pass over the cached heights
                                    // finds the on-screen window; only rows
                                    // within the viewport (± overscan) are
                                    // laid out — everything else is two
                                    // spacers, so a 6-hour log renders a few
                                    // dozen rows per frame, not all of them.
                                    // f64 accumulation: an f32 running sum
                                    // drifts past ~2M px (100k+ rows), which
                                    // desyncs offsets from rendered heights
                                    // and can retrigger repaints forever.
                                    let top = f64::from(viewport.min.y - OVERSCAN);
                                    let bottom = f64::from(viewport.max.y + OVERSCAN);
                                    let mut y = 0.0f64;
                                    let mut first = count;
                                    let mut offset = 0.0f64;
                                    let mut last = count;
                                    let mut last_y = 0.0f64;
                                    for di in 0..count {
                                        let mi = filtered.map_or(di, |v| v[di] as usize);
                                        let h = f64::from(log.row_heights[mi] + GAP);
                                        if first == count && y + h > top {
                                            first = di;
                                            offset = y;
                                        }
                                        if last == count && y > bottom {
                                            last = di;
                                            last_y = y;
                                        }
                                        y += h;
                                    }
                                    if last == count {
                                        last_y = y;
                                    }
                                    let total = y;
                                    ui.add_space(offset as f32);
                                    let mut mismeasured = false;
                                    for di in first..last {
                                        let mi = filtered.map_or(di, |v| v[di] as usize);
                                        let highlighted = popup
                                            .highlight_login
                                            .as_deref()
                                            .is_some_and(|hl| {
                                                let login = &log.messages[mi].login;
                                                !login.is_empty() && login == hl
                                            });
                                        // Fill + left-accent colour for this row —
                                        // the highlighted user, or the message's own
                                        // kind (first message, redemption, sub…).
                                        let decor =
                                            row_decor(&log.messages[mi], highlighted, ui.visuals());
                                        // Room for the accent bar on the left. Reserved
                                        // on EVERY row, not just accented ones, so text
                                        // doesn't shift sideways as notices scroll past.
                                        let r = egui::Frame::new()
                                            .fill(decor.fill)
                                            .inner_margin(egui::Margin {
                                                left: 7,
                                                right: 2,
                                                top: 1,
                                                bottom: 1,
                                            })
                                            .corner_radius(2.0)
                                            .show(ui, |ui| {
                                                ui.scope(|ui| {
                                                    render_chat_message(
                                                        ui,
                                                        &log.messages[mi],
                                                        &anim_cache,
                                                        render_emotes,
                                                        animate_emotes,
                                                        now,
                                                        &mut decode_misses,
                                                        Some(&ui_tex),
                                                        ctx,
                                                        &appearance,
                                                    )
                                                })
                                            });
                                        // egui's Frame has no per-side border, so the
                                        // Twitch-style accent is painted from the
                                        // frame's own rect after the fact. Doing it
                                        // here rather than inside keeps
                                        // `response.rect.height()` the measured row
                                        // height the virtualizer caches.
                                        if let Some(c) = decor.accent {
                                            let rect = r.response.rect;
                                            ui.painter().rect_filled(
                                                egui::Rect::from_min_size(
                                                    rect.min,
                                                    egui::vec2(3.0, rect.height()),
                                                ),
                                                1.0,
                                                c,
                                            );
                                        }
                                        if let Some(req) = r.inner.inner {
                                            usercard_click = Some(req);
                                        }
                                        let h = r.response.rect.height();
                                        if (h - log.row_heights[mi]).abs() > 0.5 {
                                            log.row_heights[mi] = h;
                                            mismeasured = true;
                                        }
                                        ui.add_space(GAP);
                                    }
                                    // Reserve the space of everything below
                                    // the rendered window so the scrollbar
                                    // spans the whole log.
                                    if total > last_y {
                                        ui.add_space((total - last_y) as f32);
                                    }
                                    if mismeasured {
                                        // Offsets were computed from estimates
                                        // — redo with real heights next frame.
                                        ctx.request_repaint();
                                    }
                                });
                        }
                    }
                });
                draw_alt_image_preview(ctx);
                popup.decode_misses.extend(decode_misses);
                if usercard_click.is_some() {
                    popup.usercard_click = usercard_click;
                }
            },
        );
        // Decode any newly-seen emotes off the UI thread, then LRU-evict the cache.
        let decode_misses = std::mem::take(&mut popup_arc.lock().unwrap().decode_misses);
        self.pump_emote_decodes(decode_misses, wrapper_now, ctx);

        // A username was clicked this frame: build the usercard. Local fields
        // (badges/color/sub-months) come straight from the click; session
        // stats are a fresh scan of the currently-loaded log (cheap — chat
        // logs are at most tens of thousands of messages, and this only runs
        // on a click, not per frame).
        let usercard_click = popup_arc.lock().unwrap().usercard_click.take();
        if let Some(req) = usercard_click {
            const RECENT_MESSAGES_CAP: usize = 50;
            let (message_count, first_seen_secs, recent_messages) = {
                let load_state = popup_arc.lock().unwrap().load_state.clone();
                let guard = load_state.lock().unwrap();
                if let ChatLoadState::Loaded(log) = &*guard {
                    let key = req.key();
                    let mut all: Vec<(f64, String)> = log
                        .messages
                        .iter()
                        .filter(|m| !m.system && m.purge_key() == key)
                        .map(|m| (m.timestamp_secs, m.text.clone()))
                        .collect();
                    let count = all.len();
                    let first = all.first().map(|(ts, _)| *ts);
                    if all.len() > RECENT_MESSAGES_CAP {
                        all.drain(0..all.len() - RECENT_MESSAGES_CAP);
                    }
                    (count, first, all)
                } else {
                    (0, None, Vec::new())
                }
            };
            let monitor_id = popup_arc.lock().unwrap().monitor_id;
            let channel_id =
                self.core.store.get_monitor_with_channel(monitor_id).ok().flatten().map(|m| m.channel.id);
            // Cross-reference this user's display name against the channel's
            // locally-recorded `stream_event` history — local DB query, no
            // network, so it's fine to run inline on the click.
            let channel_stats = channel_id
                .and_then(|cid| self.core.store.stream_events_range(cid, 0, crate::models::now_unix()).ok())
                .map(|events| summarize_user_events(&events, &req.display_name))
                .unwrap_or_default();
            // Their moderation record in this channel (both platforms; see
            // `Store::moderation_events_for_user` for how the two identities
            // are matched). Newest first, capped — a card is a summary, not an
            // audit log.
            const MODERATION_CAP: i64 = 50;
            let moderation = channel_id
                .and_then(|cid| {
                    self.core
                        .store
                        .moderation_events_for_user(cid, &req.display_name, &req.user_id, MODERATION_CAP)
                        .ok()
                })
                .unwrap_or_default();
            let mod_summary = crate::models::ModerationSummary::from_events(&moderation);
            // Live info exists only for Twitch (Helix); a YouTube card is
            // local-only, which is why the platform rides along on the click.
            let want_live = self.chat_settings.lock().unwrap().fetch_usercard_info
                && !req.user_id.is_empty()
                && req.platform == ChatPlatform::Twitch;
            let fetch = Arc::new(Mutex::new(if want_live {
                UserCardFetch::Loading
            } else {
                UserCardFetch::Disabled
            }));
            if want_live {
                if let Some(dctx) = self.core.detect_ctx() {
                    let fetch2 = fetch.clone();
                    let user_id = req.user_id.clone();
                    let login = req.login.clone();
                    let store = self.core.store.clone();
                    let events = self.core.events.clone();
                    self.core.rt.spawn(async move {
                        let result = async {
                            let (client_id, token) = dctx.twitch_helix_auth().await?;
                            crate::assets::fetch_usercard_info(&client_id, &token, &user_id).await
                        }
                        .await;
                        match result {
                            Ok(info) => {
                                *fetch2.lock().unwrap() = UserCardFetch::Loaded {
                                    avatar_path: info.avatar_path,
                                    created_at: info.created_at,
                                };
                            }
                            Err(e) => {
                                *fetch2.lock().unwrap() = UserCardFetch::Failed;
                                // File a warning through the same path capture-log
                                // alerts use, so a failed live lookup shows up in
                                // the 🚨 Warnings window / 🔔 feed instead of
                                // silently degrading to "N/A" with no trace.
                                let alert = crate::store::NewCaptureAlert {
                                    kind: "usercard_lookup_failed".to_string(),
                                    severity: "warning".to_string(),
                                    source: "chat_usercard".to_string(),
                                    take_key: format!("usercard:{login}"),
                                    monitor_id: Some(monitor_id),
                                    recording_id: None,
                                    video_id: None,
                                    channel: login.clone(),
                                    count: 1,
                                    lost_segments: 0,
                                    last_line: format!(
                                        "Twitch usercard lookup failed for {login}: {e:#}"
                                    ),
                                };
                                if let Ok((id, _)) = store.upsert_capture_alert(&alert) {
                                    let _ = events.send(crate::events::AppEvent::CaptureAlert {
                                        severity: "warning".to_string(),
                                        title: format!("Usercard lookup failed: {login}"),
                                        body: format!("{e:#}"),
                                        monitor_id: Some(monitor_id),
                                        channel: login,
                                        recording_id: None,
                                        ref_key: format!("usercard:{id}"),
                                    });
                                }
                            }
                        }
                    });
                } else {
                    *fetch.lock().unwrap() = UserCardFetch::Failed;
                }
            }
            popup_arc.lock().unwrap().user_card = Some(UserCardPopup {
                login: req.login,
                display_name: req.display_name,
                color: req.color,
                badges: req.badges,
                badge_icons: req.badge_icons,
                badge_info: req.badge_info,
                user_id: req.user_id,
                message_count,
                first_seen_secs,
                recent_messages,
                channel_stats,
                platform: req.platform,
                moderation,
                mod_summary,
                fetch,
            });
        }

        // Tail-reload: while the recording is live, parse only the bytes
        // appended since the last pass and push them onto the shown log —
        // the whole file is never re-read.
        if let Some((path, start_ts, state, emap, tdir, tfallback, funknown, loading, spartners, bdirs)) = reload_info {
            let mut p = popup_arc.lock().unwrap();
            p.last_reload = std::time::Instant::now();
            // Same cadence as the tail-reload: the leaderboard/Hype Train
            // rows keep changing while the broadcast is still live. Cheap
            // indexed local query — naturally empty for a non-Twitch monitor
            // (those event kinds are only ever written by the Twitch chat
            // parser), so no separate platform check is needed here.
            p.stats = load_broadcast_stats(
                &self.core.store,
                p.monitor_id,
                start_ts,
                crate::models::now_unix(),
            );
            if errored {
                p.error_retries = p.error_retries.saturating_add(1);
            }
            drop(p);
            self.core.rt.spawn(tail_chat(
                state,
                loading,
                path,
                start_ts,
                emap,
                tdir,
                tfallback,
                funknown,
                render_emotes,
                spartners,
                bdirs,
                ctx.clone(),
            ));
        }
        // Keep the UI alive while a live recording is open so the next
        // interval check fires automatically.
        if rec_active {
            ctx.request_repaint_after(std::time::Duration::from_secs(CHAT_RELOAD_SECS));
        }

        popup_arc.lock().unwrap().closed
    }

    /// Drop all decoded emote frames and bump the epoch so any in-flight decode
    /// task skips its insert (poison-safe).
    pub(in crate::ui) fn clear_emote_cache(&self) {
        self.emote_epoch.fetch_add(1, Ordering::SeqCst);
        self.emote_anim
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Decode newly-seen emotes off the UI thread, then enforce the LRU memory
    /// budget on the (now drawn) cache. Bounds how many decodes start per frame so
    /// opening a view with hundreds of distinct emotes doesn't spawn a blocking-
    /// thread storm; the over-cap ones revert to "unseen" and retry next frame. The
    /// epoch guard drops results whose cache was cleared (view closed / assets
    /// refetched) mid-decode. Shared by the chat replay popup and the emote viewer.
    pub(in crate::ui) fn pump_emote_decodes(
        &self,
        mut decode_misses: Vec<std::path::PathBuf>,
        now: f64,
        ctx: &egui::Context,
    ) {
        // Watchdog: the decode/upload/evict sweep is the most texture-churning phase.
        self.heartbeat.set_activity(crate::watchdog::Activity::EmoteDecodePump);
        const MAX_DECODE_PER_FRAME: usize = 64;
        if decode_misses.len() > MAX_DECODE_PER_FRAME {
            let mut g = self.emote_anim.lock().unwrap_or_else(|e| e.into_inner());
            for path in &decode_misses[MAX_DECODE_PER_FRAME..] {
                g.remove(path);
            }
            decode_misses.truncate(MAX_DECODE_PER_FRAME);
        }
        let epoch = self.emote_epoch.load(Ordering::SeqCst);
        for path in decode_misses {
            let cache = self.emote_anim.clone();
            let epoch_at = self.emote_epoch.clone();
            let ctx2 = ctx.clone();
            self.core.rt.spawn_blocking(move || {
                let decoded = crate::iomon::fs::read_sync(crate::iomon::Cat::AssetCache, &path).ok().and_then(|b| crate::emote_anim::decode(&b));
                let entry = match decoded {
                    Some((imgs, delays)) => crate::emote_anim::EmoteLoad::Decoded(imgs, delays),
                    None => crate::emote_anim::EmoteLoad::Failed,
                };
                let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
                if epoch_at.load(Ordering::SeqCst) == epoch {
                    g.insert(path, entry);
                    drop(g);
                    ctx2.request_repaint();
                }
            });
        }
        evict_emote_cache(&self.emote_anim, now);
    }
}
