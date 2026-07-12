//! Instance/channel properties, emote viewer, asset history and about
//! windows.

use super::*;

impl StreamArchiverApp {
    /// Instance (monitor) properties window — header + monitor-specific info.
    #[allow(deprecated)]
    /// Render every open instance-properties window (one per monitor).
    /// True if any open instance-Properties popup belongs to the given channel
    /// (those windows share the channel's per-open asset caches).
    pub(super) fn instance_props_open_for_channel(&self, cid: i64) -> bool {
        self.properties_popups
            .iter()
            .any(|&pm| self.rows.iter().any(|r| r.monitor.id == pm && r.channel.id == cid))
    }

    pub(super) fn instance_properties_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        // Snapshot: a window closed mid-load removes itself from the list
        // inside drive_props_load, so indexed iteration could skip/overrun.
        let ids: Vec<i64> = self.properties_popups.clone();
        for mid in ids {
            if !self.properties_popups.contains(&mid) {
                continue; // removed mid-loop (closed while loading)
            }
            if self.instance_properties_window(ctx, mid) {
                closed.push(mid);
            }
        }
        if !closed.is_empty() {
            self.properties_popups.retain(|m| !closed.contains(m));
            for mid in closed {
                self.instance_scope_drafts.remove(&mid);
                self.instance_trigger_drafts.remove(&mid);
                self.instance_block_drafts.remove(&mid);
                // Free the shared per-channel asset caches when this was the
                // last Properties window (channel or instance) showing them.
                let cid = self.rows.iter().find(|r| r.monitor.id == mid).map(|r| r.channel.id);
                if let Some(cid) = cid
                    && !self.channel_properties_popups.contains(&cid)
                    && !self.instance_props_open_for_channel(cid)
                {
                    self.channel_asset_thumbs.remove(&cid);
                    self.channel_emote_counts.remove(&cid);
                    self.channel_asset_status.remove(&cid);
                }
            }
        }
    }

    /// One instance-properties window; returns true when it should close.
    #[allow(deprecated)]
    pub(super) fn instance_properties_window(&mut self, ctx: &egui::Context, mid: i64) -> bool {
        let Some(row) = self.rows.iter().find(|r| r.monitor.id == mid).cloned() else {
            return true;
        };
        let ch = &row.channel;
        let m = &row.monitor;
        let cid = ch.id;

        let accounts = {
            let mons: Vec<&MonitorWithChannel> =
                self.rows.iter().filter(|r| r.channel.id == cid).collect();
            channel_asset_accounts(&mons)
        };
        // This instance's own asset account (None for Generic URLs — no asset
        // fetcher, so no assets section). Matched by (platform, slug) rather
        // than monitor id: two tools on one URL share the sibling's entry.
        let inst_account: Option<AssetAccount> = if m.platform() == Platform::Generic {
            None
        } else {
            let slug = asset_account(&m.url, m.platform());
            accounts
                .iter()
                .find(|a| a.platform == m.platform() && a.account == slug)
                .cloned()
        };

        // The assets shown below come from the same per-channel caches the
        // channel Properties window uses, filtered to this account — loaded
        // OFF the UI thread on first open (see drive_props_load).
        if inst_account.is_some()
            && !self.channel_asset_status.contains_key(&cid)
            && !self.drive_props_load(cid, ch, &accounts, Some(mid), ctx)
        {
            return false; // still loading — placeholder is on screen
        }

        let (thumbs, emote_counts, asset_status, icon_tex) =
            self.instance_props_cached_assets(cid, ch, &accounts, &inst_account, ctx);

        let mut refetch = false;
        let mut open_emote_viewer: Option<(EmoteProvider, AssetAccount)> = None;
        let mut open_asset_history = false;
        let mut open_about = false;

        self.instance_scope_drafts
            .entry(m.id)
            .or_insert_with(|| load_monitor_scope(&self.core.store, m.id));
        self.instance_trigger_drafts
            .entry(m.id)
            .or_insert_with(|| crate::triggers::load_monitor_trigger_scope(&self.core.store, m.id));
        self.instance_block_drafts
            .entry(m.id)
            .or_insert_with(|| crate::triggers::load_monitor_block_scope(&self.core.store, m.id));
        let global_order = load_source_order(&self.core.store);
        let mut scope_dirty = false;
        let mut trigger_dirty = false;
        let mut block_dirty = false;

        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("instance_props_vp", mid)),
            egui::ViewportBuilder::default()
                .with_title(format!("Instance — {}", ch.name))
                .with_inner_size([480.0, 560.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                // ── Header ──────────────────────────────────────────────
                self.instance_props_header(ui, ch, m, &icon_tex);

                ui.separator();

                egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {

                // ── Monitor (instance) ───────────────────────────────────
                Self::instance_props_monitor_section(ui, m, row.recording_count);

                // ── Assets (this instance's account) ─────────────────────
                // The same data the channel Properties window shows, filtered
                // to the account this instance's URL resolves to.
                if let Some(acc) = &inst_account {
                    self.instance_props_assets_section(
                        ui,
                        ch,
                        acc,
                        &thumbs,
                        &emote_counts,
                        &asset_status,
                        &mut refetch,
                        &mut open_emote_viewer,
                        &mut open_asset_history,
                        &mut open_about,
                    );
                }

                // ── Trigger words (this instance) ────────────────────────
                self.instance_props_triggers_section(ui, mid, &mut trigger_dirty, &mut block_dirty);

                // ── Schedule sources (this instance) ─────────────────────
                self.instance_props_sched_section(ui, mid, &global_order, &mut scope_dirty);

                }); // ScrollArea
                });
                draw_alt_image_preview(ctx);
            },
        );

        self.instance_props_apply_actions(
            mid,
            cid,
            ch,
            &inst_account,
            scope_dirty,
            trigger_dirty,
            block_dirty,
            refetch,
            open_emote_viewer,
            open_asset_history,
            open_about,
        );

        !open
    }

    /// Cached per-channel asset data filtered for the instance window (all
    /// empty when the instance has no asset account), plus the header icon.
    #[allow(clippy::type_complexity)]
    fn instance_props_cached_assets(
        &mut self,
        cid: i64,
        ch: &Channel,
        accounts: &[AssetAccount],
        inst_account: &Option<AssetAccount>,
        ctx: &egui::Context,
    ) -> (
        Vec<AssetThumb>,
        Vec<(AssetAccount, [(EmoteProvider, usize); 4])>,
        Vec<PlatformAssetStatus>,
        Option<egui::TextureHandle>,
    ) {
        let thumbs = if inst_account.is_some() {
            self.channel_asset_thumbs
                .entry(cid)
                .or_insert_with(|| load_channel_asset_thumbs(ch, accounts, ctx))
                .clone()
        } else {
            Vec::new()
        };
        let emote_counts = if inst_account.is_some() {
            self.channel_emote_counts
                .entry(cid)
                .or_insert_with(|| emote_provider_counts(&ch.name, accounts))
                .clone()
        } else {
            Vec::new()
        };
        let asset_status = if inst_account.is_some() {
            self.channel_asset_status
                .entry(cid)
                .or_insert_with(|| build_platform_asset_status(&ch.name, accounts))
                .clone()
        } else {
            Vec::new()
        };

        // Header icon: this instance's own account avatar when fetched; the
        // channel-level icon as the fallback (nothing fetched yet / Generic).
        let icon_tex = inst_account
            .as_ref()
            .and_then(|acc| {
                thumbs
                    .iter()
                    .find(|t| {
                        t.kind == "icon" && t.platform == acc.platform && t.account == acc.account
                    })
                    .map(|t| t.tex.clone())
            })
            .or_else(|| {
                self.channel_icons
                    .entry(cid)
                    .or_insert_with(|| resolve_channel_icon(ch, accounts, ctx))
                    .clone()
            });

        (thumbs, emote_counts, asset_status, icon_tex)
    }

    /// Header row: account avatar (or placeholder), channel name, and this
    /// instance's platform icon + source-URL link.
    fn instance_props_header(
        &mut self,
        ui: &mut egui::Ui,
        ch: &Channel,
        m: &Monitor,
        icon_tex: &Option<egui::TextureHandle>,
    ) {
        ui.horizontal(|ui| {
            if let Some(tex) = icon_tex {
                let resp = ui.add(
                    egui::Image::from_texture(tex)
                        .max_size(egui::vec2(96.0, 96.0))
                        .corner_radius(egui::CornerRadius::same(8)),
                );
                queue_alt_image_preview(ui.ctx(), &resp, tex);
            } else {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(96.0, 96.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    rect,
                    8.0,
                    ui.visuals().weak_text_color(),
                );
            }
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(4.0);
                ui.heading(&ch.name);
                // This instance's platform + source URL (not the
                // channel-wide platform list — that's channel Properties).
                ui.horizontal(|ui| {
                    let ptex = self
                        .platform_tex
                        .get_or_insert_with(|| PlatformTextures::load(ui.ctx()));
                    if let Some(t) = ptex.get(m.platform()) {
                        ui.add(
                            egui::Image::from_texture(t)
                                .max_size(egui::vec2(14.0, 14.0)),
                        );
                    }
                    if ui.link(instance_label(&m.url)).on_hover_text(&m.url).clicked()
                    {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(m.url.clone()));
                    }
                });
            });
        });
    }

    /// "Monitor (instance)" section — the read-only monitor-config grid.
    fn instance_props_monitor_section(ui: &mut egui::Ui, m: &Monitor, recording_count: i64) {
        egui::CollapsingHeader::new(egui::RichText::new("Monitor (instance)").strong())
            .id_salt("inst_props_sec_monitor")
            .default_open(true)
            .show(ui, |ui| {
        egui::Grid::new("props_mon")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                ui.label("DB monitor ID");
                ui.label(m.id.to_string());
                ui.end_row();
                ui.label("Detection");
                ui.label(m.detection_method.as_str());
                ui.end_row();
                ui.label("Tool");
                ui.label(format!("{:?}", m.tool));
                ui.end_row();
                ui.label("Poll interval");
                ui.label(format!("{}s", m.poll_interval_secs));
                ui.end_row();
                ui.label("Quality");
                ui.label(&m.quality);
                ui.end_row();
                ui.label("Max concurrent");
                ui.label(m.max_concurrent.to_string());
                ui.end_row();
                ui.label("Last state");
                ui.label(&m.last_state);
                ui.end_row();
                ui.label("Recordings");
                ui.label(recording_count.to_string());
                ui.end_row();
                ui.label("Output dir");
                ui.horizontal(|ui| {
                    ui.label(prop_truncate_path(&m.output_dir, 28));
                    if ui
                        .small_button("📂")
                        .on_hover_text("Open folder")
                        .clicked()
                    {
                        crate::platform::open_path(
                            std::path::Path::new(&m.output_dir),
                        );
                    }
                });
                ui.end_row();
                ui.label("Fetch thumbnail");
                ui.label(prop_bool(m.fetch_thumbnail));
                ui.end_row();
                ui.label("Fetch assets");
                ui.label(prop_bool(m.fetch_chat_assets));
                ui.end_row();
            });
            });
    }

    /// "Assets (this account)" section — refetch/folder/history/about buttons,
    /// this account's thumbnails, its status row and emote-viewer launchers.
    #[allow(deprecated)] // egui::ImageButton in the emote launchers
    #[allow(clippy::too_many_arguments)]
    fn instance_props_assets_section(
        &mut self,
        ui: &mut egui::Ui,
        ch: &Channel,
        acc: &AssetAccount,
        thumbs: &[AssetThumb],
        emote_counts: &[(AssetAccount, [(EmoteProvider, usize); 4])],
        asset_status: &[PlatformAssetStatus],
        refetch: &mut bool,
        open_emote_viewer: &mut Option<(EmoteProvider, AssetAccount)>,
        open_asset_history: &mut bool,
        open_about: &mut bool,
    ) {
        egui::CollapsingHeader::new(
            egui::RichText::new("Assets (this account)").strong(),
        )
        .id_salt("inst_props_sec_assets")
        .default_open(true)
        .show(ui, |ui| {
        ui.horizontal(|ui| {
            if ui
                .button("⟳ Refetch")
                .on_hover_text(format!(
                    "Fetch icon / banner / badges / emotes for {} now — \
                     ignores the 24h cache.",
                    acc.label,
                ))
                .clicked()
            {
                *refetch = true;
            }
            if ui
                .button("📂")
                .on_hover_text(
                    "Open this account's asset folder (icons, banners, and the \
                     history/ archive of older versions).",
                )
                .clicked()
            {
                let dir = channel_asset_dir(&ch.name, acc.platform, &acc.account);
                let target = if crate::iomon::fs::is_dir_sync(crate::iomon::Cat::AssetCache, &dir) {
                    dir
                } else {
                    // Nothing fetched yet — fall back to the channel root.
                    crate::app_paths::asset_cache_dir()
                        .join("channel_assets")
                        .join(crate::downloader::sanitize_filename(&ch.name))
                };
                crate::platform::open_path(&target);
            }
            if ui
                .button("🕑 History")
                .on_hover_text(
                    "Show recorded asset changes over time (added / removed \
                     emotes, icon / banner / name-colour replacements) for \
                     this account only.",
                )
                .clicked()
            {
                *open_asset_history = true;
            }
            if ui
                .button("ℹ About")
                .on_hover_text(
                    "Show this account's archived About page — description, \
                     panels, links — with a version picker. Captured with \
                     each asset fetch; a new version is stored only when the \
                     content actually changed.",
                )
                .clicked()
            {
                *open_about = true;
            }
        });

        // Thumbnails: this account's original icon/banner. Hover for
        // size, Alt to preview full-res, click to open the file.
        let own_thumbs: Vec<&AssetThumb> = thumbs
            .iter()
            .filter(|t| t.platform == acc.platform && t.account == acc.account)
            .collect();
        if !own_thumbs.is_empty() {
            ui.add_space(3.0);
            const THUMB_H: f32 = 56.0;
            ui.horizontal_wrapped(|ui| {
                for t in own_thumbs {
                    let (w, h) = t.dims;
                    let aspect = if h > 0 { w as f32 / h as f32 } else { 1.0 };
                    let width = (THUMB_H * aspect).min(THUMB_H * 4.0);
                    let resp = ui
                        .add(
                            egui::Image::from_texture(&t.tex)
                                .fit_to_exact_size(egui::vec2(width, THUMB_H))
                                .corner_radius(egui::CornerRadius::same(4))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(format!(
                            "{} — {w}×{h} px\nAlt: preview full size · click: open file",
                            t.label,
                        ));
                    queue_alt_image_preview(ui.ctx(), &resp, &t.tex);
                    if resp.clicked() {
                        crate::platform::open_path(&t.path);
                    }
                }
            });
        }

        // Status row (same columns as the channel window, minus the
        // per-row ⟳ — the header Refetch covers this one account).
        ui.add_space(4.0);
        egui::Grid::new("inst_props_assets")
            .num_columns(6)
            .spacing([12.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Source");
                ui.strong("Icon");
                ui.strong("Banner");
                ui.strong("Badges");
                ui.strong("Emotes");
                ui.strong("Updated");
                ui.end_row();
                for st in asset_status.iter().filter(|st| {
                    st.account.platform == acc.platform
                        && st.account.account == acc.account
                }) {
                    ui.horizontal(|ui| {
                        let ptex = self.platform_tex.get_or_insert_with(|| {
                            PlatformTextures::load(ui.ctx())
                        });
                        if let Some(t) = ptex.get(st.account.platform) {
                            ui.add(
                                egui::Image::from_texture(t)
                                    .max_size(egui::vec2(13.0, 13.0)),
                            );
                        }
                        ui.label(&st.account.label);
                    });
                    asset_status_cell(ui, st.icon_present, st.icon_variants);
                    asset_status_cell(ui, st.banner_present, st.banner_variants);
                    ui.label(if st.badges > 0 {
                        st.badges.to_string()
                    } else {
                        "—".into()
                    });
                    ui.label(if st.emotes > 0 {
                        st.emotes.to_string()
                    } else {
                        "—".into()
                    });
                    ui.label(&st.stamp);
                    ui.end_row();
                }
            });

        // Emote viewer launchers for this account only.
        if let Some((eacc, counts)) = emote_counts
            .iter()
            .find(|(a, _)| a.platform == acc.platform && a.account == acc.account)
            && counts.iter().any(|(_, n)| *n > 0)
        {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                    ui.strong("View emotes:");
                    let ptex = self
                        .provider_tex
                        .get_or_insert_with(|| ProviderTextures::load(ui.ctx()))
                        .clone();
                    for &(provider, count) in counts {
                        if count == 0 {
                            continue;
                        }
                        let resp = match provider {
                            EmoteProvider::SevenTv => ui.add(egui::ImageButton::new(
                                egui::Image::from_texture(&ptex.seventv)
                                    .fit_to_exact_size(egui::vec2(18.0, 18.0)),
                            )),
                            EmoteProvider::Bttv => ui.add(egui::ImageButton::new(
                                egui::Image::from_texture(&ptex.bttv)
                                    .fit_to_exact_size(egui::vec2(18.0, 18.0)),
                            )),
                            EmoteProvider::Twitch => ui.button("😀"),
                            EmoteProvider::Ffz => ui.button("FFZ"),
                        };
                        if resp
                            .on_hover_text(format!(
                                "View {} {} emote{} for {}",
                                count,
                                provider.label(),
                                if count == 1 { "" } else { "s" },
                                eacc.label,
                            ))
                            .clicked()
                        {
                            *open_emote_viewer = Some((provider, eacc.clone()));
                        }
                    }
                });
        }
        });
    }

    /// "Trigger words" section — this instance's trigger-scope editor.
    fn instance_props_triggers_section(
        &mut self,
        ui: &mut egui::Ui,
        mid: i64,
        trigger_dirty: &mut bool,
        block_dirty: &mut bool,
    ) {
        egui::CollapsingHeader::new(egui::RichText::new("Trigger words").strong())
            .id_salt("inst_props_sec_triggers")
            .default_open(true)
            .show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "Start recording when the live title/game matches — even with Auto \
                     off. Inherits the channel's rules, which inherit the global ones \
                     (Settings → Downloads → Trigger words).",
                )
                .small()
                .weak(),
            );
            if let Some(scope) = self.instance_trigger_drafts.get_mut(&mid)
                && trigger_scope_editor(ui, scope, "inst_triggers", true)
            {
                *trigger_dirty = true;
            }
            });
        egui::CollapsingHeader::new(egui::RichText::new("Blacklist triggers").strong())
            .id_salt("inst_props_sec_block_triggers")
            .default_open(false)
            .show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "PREVENT automatic recording while the live title/game matches — \
                     manual ▶ Start still records. Inherits the channel's blacklist, \
                     which inherits the global one (Settings → Downloads → Blacklist \
                     triggers).",
                )
                .small()
                .weak(),
            );
            if let Some(scope) = self.instance_block_drafts.get_mut(&mid)
                && trigger_scope_editor(ui, scope, "inst_block_triggers", false)
            {
                *block_dirty = true;
            }
            });
    }

    /// "Schedule sources (this instance)" section — the per-monitor scope
    /// override editor.
    fn instance_props_sched_section(
        &mut self,
        ui: &mut egui::Ui,
        mid: i64,
        global_order: &[SourceEntry],
        scope_dirty: &mut bool,
    ) {
        egui::CollapsingHeader::new(
            egui::RichText::new("Schedule sources (this instance)").strong(),
        )
        .id_salt("inst_props_sec_sched")
        .default_open(false)
        .show(ui, |ui| {
        ui.label(
            egui::RichText::new(
                "Overrides the global order/title-fill for this one instance, taking \
                 precedence over the channel's setting. Changes apply on the next \
                 schedule refresh.",
            )
            .small()
            .weak(),
        );
        if let Some(scope) = self.instance_scope_drafts.get_mut(&mid) {
            if scope_override_editor(ui, scope, global_order) {
                *scope_dirty = true;
            }
        }
        });
    }

    /// Post-render actions for the instance window: persist dirty scope/trigger
    /// drafts, dispatch ⟳ Refetch (outside the viewport closure) and open the
    /// emote-viewer / asset-history / About windows requested by button clicks.
    #[allow(clippy::too_many_arguments)]
    fn instance_props_apply_actions(
        &mut self,
        mid: i64,
        cid: i64,
        ch: &Channel,
        inst_account: &Option<AssetAccount>,
        scope_dirty: bool,
        trigger_dirty: bool,
        block_dirty: bool,
        refetch: bool,
        open_emote_viewer: Option<(EmoteProvider, AssetAccount)>,
        open_asset_history: bool,
        open_about: bool,
    ) {
        if scope_dirty {
            if let Some(scope) = self.instance_scope_drafts.get(&mid) {
                if let Err(e) = save_monitor_scope(&self.core.store, mid, scope) {
                    self.status = format!("Error saving instance sources: {e}");
                } else {
                    self.core.request_schedule_refresh();
                }
            }
        }
        if trigger_dirty
            && let Some(scope) = self.instance_trigger_drafts.get(&mid)
            && let Err(e) = crate::triggers::save_monitor_trigger_scope(&self.core.store, mid, scope)
        {
            self.status = format!("Error saving trigger words: {e}");
        }
        if block_dirty
            && let Some(scope) = self.instance_block_drafts.get(&mid)
            && let Err(e) = crate::triggers::save_monitor_block_scope(&self.core.store, mid, scope)
        {
            self.status = format!("Error saving blacklist triggers: {e}");
        }

        // ⟳ Refetch dispatches outside the viewport closure (same cache-drop set
        // as the channel window; channel_icons_small stays for the same
        // viewport-race reason — it refreshes on AssetFetch completion).
        if refetch && let Some(acc) = inst_account {
            self.core.manual(ManualCommand::RefetchAssets(mid));
            self.channel_icons.remove(&cid);
            self.channel_twitch_colors.remove(&cid);
            self.channel_asset_thumbs.remove(&cid);
            self.channel_emote_counts.remove(&cid);
            self.channel_asset_status.remove(&cid);
            self.channel_about_latest.remove(&cid);
            self.status = format!("Refetching assets for {}…", acc.label);
        }
        // A launcher button was clicked — open (or refresh) the emote viewer
        // for this channel+account+provider; other viewers stay open.
        if let Some((provider, acc)) = open_emote_viewer {
            let fresh = EmoteViewer::new(
                ch.name.clone(),
                acc.account.clone(),
                acc.has_siblings,
                provider,
            );
            match self.emote_viewers.iter_mut().find(|v| {
                v.channel_name == ch.name && v.account == acc.account && v.provider == provider
            }) {
                Some(slot) => *slot = fresh,
                None => self.emote_viewers.push(fresh),
            }
        }
        // 🕑 History — the asset-history popup scoped to this account only.
        if open_asset_history && let Some(acc) = inst_account {
            let fresh = AssetHistoryView::new(ch.name.clone(), std::slice::from_ref(acc));
            match self
                .asset_histories
                .iter_mut()
                .find(|h| h.channel_name == ch.name)
            {
                Some(slot) => *slot = fresh,
                None => self.asset_histories.push(fresh),
            }
        }
        // ℹ About — the archived About-page viewer for this account.
        if open_about && let Some(acc) = inst_account {
            self.open_about_view(cid, &ch.name, acc);
        }
    }

    /// Drive the off-UI-thread load of the channel Properties window's per-open data,
    /// drawing a "Loading…" placeholder until it lands. Returns `true` once the bundle has
    /// been installed into the per-channel caches (the caller then falls through and draws
    /// the real window *this* frame, no flicker); `false` while still loading (the caller
    /// returns — the placeholder is on screen). All the blocking work (disk reads, image
    /// decode/upload, asset-dir enumeration, store-mutex DB reads) happens on the spawned
    /// thread, so the UI thread can't freeze on a slow disk / AV scan / contended store.
    #[must_use]
    #[allow(deprecated)] // CentralPanel::show in an immediate viewport (matches the real window)
    /// `instance_mid` is `Some(monitor id)` when the caller is an *instance*
    /// Properties window (it shares the same per-channel caches): the loading
    /// placeholder then uses that window's viewport id/title, and closing it
    /// mid-load forgets the instance popup instead of the channel popup.
    pub(super) fn drive_props_load(
        &mut self,
        cid: i64,
        ch: &Channel,
        accounts: &[AssetAccount],
        instance_mid: Option<i64>,
        ctx: &egui::Context,
    ) -> bool {
        // Ensure a load for THIS channel is in flight (other windows' loads run
        // concurrently in their own entries).
        let need_spawn = !self.props_loads.iter().any(|pl| pl.channel_id == cid);
        if need_spawn {
            let (tx, rx) = std::sync::mpsc::channel();
            let ch_bg = ch.clone();
            let accounts_bg = accounts.to_vec();
            let ctx_bg = ctx.clone();
            let store = self.core.store.clone();
            debug!(channel_id = cid, "spawning props-load thread");
            let spawned = std::thread::Builder::new()
                .name("props-load".into())
                .spawn(move || {
                    let t = std::time::Instant::now();
                    // None of this touches `self`. egui's `load_texture` (inside
                    // `resolve_channel_icon` / `load_channel_asset_thumbs`) is thread-safe:
                    // it queues a `TexturesDelta` the paint thread uploads later.
                    let id = ch_bg.id;
                    let loaded = PropsLoaded {
                        channel_id: id,
                        icon: resolve_channel_icon(&ch_bg, &accounts_bg, &ctx_bg),
                        thumbs: load_channel_asset_thumbs(&ch_bg, &accounts_bg, &ctx_bg),
                        emote_counts: emote_provider_counts(&ch_bg.name, &accounts_bg),
                        asset_status: build_platform_asset_status(&ch_bg.name, &accounts_bg),
                        cfg: load_channel_cfg(&store, id),
                        source_order: load_source_order(&store),
                        scope: load_channel_scope(&store, id),
                        about_latest: store.about_latest_per_account(id).unwrap_or_default(),
                    };
                    debug!(elapsed_ms = t.elapsed().as_millis(), channel_id = id, "props-load done");
                    // Receiver gone (window closed) → the send is simply dropped.
                    let _ = tx.send(loaded);
                    // Wake the UI thread to install the bundle even if otherwise idle.
                    ctx_bg.request_repaint();
                });
            match spawned {
                Ok(_) => self.props_loads.push(PropsLoad { channel_id: cid, rx }),
                Err(e) => {
                    // Spawn failed (extremely unlikely). Fall back to a synchronous load so
                    // the window still opens rather than spinning forever.
                    warn!("props-load thread spawn failed ({e}); loading on the UI thread");
                    let id = ch.id;
                    let loaded = PropsLoaded {
                        channel_id: id,
                        icon: resolve_channel_icon(ch, accounts, ctx),
                        thumbs: load_channel_asset_thumbs(ch, accounts, ctx),
                        emote_counts: emote_provider_counts(&ch.name, accounts),
                        asset_status: build_platform_asset_status(&ch.name, accounts),
                        cfg: load_channel_cfg(&self.core.store, id),
                        source_order: load_source_order(&self.core.store),
                        scope: load_channel_scope(&self.core.store, id),
                        about_latest: self.core.store.about_latest_per_account(id).unwrap_or_default(),
                    };
                    self.install_props_loaded(loaded);
                    return true;
                }
            }
        }

        // Poll this channel's in-flight load without blocking.
        if let Some(i) = self.props_loads.iter().position(|pl| pl.channel_id == cid) {
            match self.props_loads[i].rx.try_recv() {
                Ok(loaded) => {
                    self.props_loads.remove(i);
                    self.install_props_loaded(loaded);
                    return true; // caller draws the real window this frame
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Loader vanished without sending (it never panics in practice —
                    // every step degrades to a default). Drop the handle so the next
                    // frame respawns; the placeholder stays up meanwhile.
                    self.props_loads.remove(i);
                }
                // Still loading → fall through to the placeholder.
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        // Still loading: draw the placeholder. Same viewport id as the real window so it
        // morphs in place when the bundle lands.
        self.heartbeat.set_context(format!("Properties: {}", ch.name));
        self.heartbeat.set_activity(crate::watchdog::Activity::Properties);
        let (vp_id, title, size) = match instance_mid {
            Some(mid) => (
                egui::ViewportId::from_hash_of(("instance_props_vp", mid)),
                format!("Instance — {}", ch.name),
                [480.0, 560.0],
            ),
            None => (
                egui::ViewportId::from_hash_of(("channel_props_vp", cid)),
                format!("Properties — {}", ch.name),
                [480.0, 640.0],
            ),
        };
        let mut open = true;
        ctx.show_viewport_immediate(
            vp_id,
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size(size),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(220.0);
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label(format!("Loading {} assets…", ch.name));
                    });
                });
            },
        );
        if !open {
            // Closed mid-load: forget the popup; drop the in-flight load only
            // when no other window (channel or sibling instance) still wants it.
            match instance_mid {
                Some(mid) => self.properties_popups.retain(|m| *m != mid),
                None => self.channel_properties_popups.retain(|c| *c != cid),
            }
            if !self.channel_properties_popups.contains(&cid)
                && !self.instance_props_open_for_channel(cid)
            {
                self.props_loads.retain(|pl| pl.channel_id != cid);
            }
        }
        // Keep frames coming while a load is in flight so we poll the loader
        // (the spinner animates too) — but NOT unconditionally: that busy-looped
        // the whole app at max FPS for as long as the window stayed open.
        if !self.props_loads.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        false
    }

    /// Install a finished [`PropsLoaded`] bundle into the per-channel Properties caches —
    /// but only if the window is still showing that channel (the user may have closed it or
    /// switched away while the background load ran). In-progress config/scope edits are
    /// preserved: the drafts are only seeded when they don't already belong to this channel.
    pub(super) fn install_props_loaded(&mut self, loaded: PropsLoaded) {
        if !self.channel_properties_popups.contains(&loaded.channel_id)
            && !self.instance_props_open_for_channel(loaded.channel_id)
        {
            return;
        }
        let id = loaded.channel_id;
        self.channel_icons.insert(id, loaded.icon);
        self.channel_asset_thumbs.insert(id, loaded.thumbs);
        self.channel_emote_counts.insert(id, loaded.emote_counts);
        self.channel_asset_status.insert(id, loaded.asset_status);
        self.channel_about_latest.insert(id, loaded.about_latest);
        self.props_source_order = loaded.source_order;
        self.channel_cfg_drafts.entry(id).or_insert(loaded.cfg);
        self.channel_scope_drafts.entry(id).or_insert(loaded.scope);
    }

    /// Render every open channel-Properties window (one per channel).
    pub(super) fn channel_properties_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<i64> = Vec::new();
        // Snapshot: a window closed mid-load removes itself from the list
        // inside drive_props_load, so indexed iteration could skip/overrun.
        let ids: Vec<i64> = self.channel_properties_popups.clone();
        for cid in ids {
            if !self.channel_properties_popups.contains(&cid) {
                continue; // removed mid-loop (closed while loading)
            }
            if self.channel_properties_window(ctx, cid) {
                closed.push(cid);
            }
        }
        for cid in closed {
            self.channel_properties_popups.retain(|c| *c != cid);
            self.channel_cfg_drafts.remove(&cid);
            self.channel_scope_drafts.remove(&cid);
            self.channel_trigger_drafts.remove(&cid);
            self.channel_block_drafts.remove(&cid);
            // Free the full-resolution thumbnail textures; they reload from disk
            // on the next open. Kept while an instance-Properties window of this
            // channel is still open — those share the same caches.
            if !self.instance_props_open_for_channel(cid) {
                self.channel_asset_thumbs.remove(&cid);
                self.channel_emote_counts.remove(&cid);
                self.channel_asset_status.remove(&cid);
            }
        }
    }

    /// Channel properties window — header + channel info, assets, and schedule-source config.
    /// Returns true when it should close.
    #[allow(deprecated)]
    pub(super) fn channel_properties_window(&mut self, ctx: &egui::Context, cid: i64) -> bool {
        let Some(ch) = self.channels.iter().find(|c| c.id == cid).cloned() else {
            return true;
        };
        // Watchdog: the synchronous first-open work below (icon/thumbnail image decode +
        // GPU upload and the per-platform asset enumeration) is the part that can block
        // the UI thread for seconds. Stamp it distinctly from the window paint so a freeze
        // dialog says whether we stalled *loading assets* or *inside the window*. Reset to
        // `Properties` right before the sub-window is created (below).
        self.heartbeat.set_context(format!("Properties: {}", ch.name));
        self.heartbeat.set_activity(crate::watchdog::Activity::PropertiesLoad);
        let (platforms, accounts) = {
            let mons: Vec<&MonitorWithChannel> =
                self.rows.iter().filter(|r| r.channel.id == cid).collect();
            (channel_platforms(&mons), channel_asset_accounts(&mons))
        };

        // First-open — and every reopen, since these per-open caches are dropped on
        // close — asset load runs OFF the UI thread: icon/thumbnail image decode + GPU
        // upload, the per-platform asset enumeration, and the schedule-source
        // config/scope/order DB reads. Those reads take the single store mutex, which a
        // background task (e.g. the asset-refresh loop's heavy `list_monitors_with_channels`
        // query) can hold long enough to freeze the GUI here. Until the bundle lands we
        // show a "Loading…" placeholder and stay responsive. Gated on `channel_asset_status`
        // because it is the cache dropped on every close (and on rename/refetch), so each
        // open re-loads through this off-thread path rather than blocking the UI thread.
        if !self.channel_asset_status.contains_key(&ch.id)
            && !self.drive_props_load(cid, &ch, &accounts, None, ctx)
        {
            return false; // still loading — placeholder is on screen
        }

        let (icon_tex, thumbs, emote_counts, asset_status, about_latest) =
            self.channel_props_cached_assets(&ch, &accounts, ctx);

        let mut pref_change: Option<Option<crate::models::PreferredAssetSource>> = None;
        let mut open_emote_viewer: Option<(EmoteProvider, AssetAccount)> = None;
        let mut open_asset_history = false;
        let mut open_about_account: Option<AssetAccount> = None;
        let mut refetch_monitor_ids: Vec<i64> = Vec::new();

        if !self.channel_cfg_drafts.contains_key(&ch.id) {
            self.channel_cfg_drafts
                .insert(ch.id, load_channel_cfg(&self.core.store, ch.id));
            // Snapshot the global source order once per open (read once here, not via a
            // settings DB read every frame inside `scope_override_editor`).
            self.props_source_order = load_source_order(&self.core.store);
        }
        if !self.channel_scope_drafts.contains_key(&ch.id) {
            self.channel_scope_drafts
                .insert(ch.id, load_channel_scope(&self.core.store, ch.id));
        }
        self.channel_trigger_drafts
            .entry(ch.id)
            .or_insert_with(|| crate::triggers::load_channel_trigger_scope(&self.core.store, ch.id));
        self.channel_block_drafts
            .entry(ch.id)
            .or_insert_with(|| crate::triggers::load_channel_block_scope(&self.core.store, ch.id));
        let global_order = self.props_source_order.clone();
        let mut cfg_dirty = false;
        let mut scope_dirty = false;
        let mut trigger_dirty = false;
        let mut block_dirty = false;

        // The first-open asset loads are done; everything from here is the sub-window
        // build + paint. Stamp that distinctly so a freeze here is attributed to the
        // window itself rather than to asset loading.
        self.heartbeat.set_activity(crate::watchdog::Activity::Properties);
        // context already set during PropertiesLoad above; refresh in case we skipped load

        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("channel_props_vp", cid)),
            egui::ViewportBuilder::default()
                .with_title(format!("Properties — {}", ch.name))
                .with_inner_size([480.0, 640.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                // ── Header ──────────────────────────────────────────────
                self.channel_props_header(ui, &ch, &platforms, &icon_tex);

                ui.separator();

                egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {

                // ── Assets ───────────────────────────────────────────────
                // Thumbnail overview of every original icon/banner across the
                // channel's accounts (hover for size, Alt to preview full-res,
                // click to open the file), then the refetch/history controls,
                // icon-source picker, per-account status grid and emote-viewer
                // launchers.
                self.channel_props_assets_section(
                    ui,
                    &ch,
                    &accounts,
                    &thumbs,
                    &emote_counts,
                    &asset_status,
                    &about_latest,
                    &mut pref_change,
                    &mut open_emote_viewer,
                    &mut open_asset_history,
                    &mut open_about_account,
                    &mut refetch_monitor_ids,
                );

                // ── Channel ─────────────────────────────────────────────
                Self::channel_props_channel_section(ui, &ch);

                // ── Trigger words (per-channel) ──────────────────────────
                self.channel_props_triggers_section(ui, cid, &mut trigger_dirty, &mut block_dirty);

                // ── Schedule sources (per-channel) ───────────────────────
                self.channel_props_sched_section(
                    ui,
                    cid,
                    &global_order,
                    &mut cfg_dirty,
                    &mut scope_dirty,
                );

                }); // ScrollArea

                // Apply an icon-source change picked in the combo above.
                self.channel_props_apply_pref_change(&ch, &mut pref_change);
                });
                draw_alt_image_preview(ctx);
            },
        );

        self.channel_props_apply_actions(
            cid,
            &ch,
            &accounts,
            refetch_monitor_ids,
            open_emote_viewer,
            open_asset_history,
            open_about_account,
            cfg_dirty,
            scope_dirty,
            trigger_dirty,
            block_dirty,
        );

        // Draft/texture cleanup for a closed window happens in the caller
        // (channel_properties_windows), which also drops it from the open list.
        !open
    }

    /// Per-open cached data for the channel window: header icon, asset
    /// thumbnails, emote counts, per-account status rows and About snapshots.
    #[allow(clippy::type_complexity)]
    fn channel_props_cached_assets(
        &mut self,
        ch: &Channel,
        accounts: &[AssetAccount],
        ctx: &egui::Context,
    ) -> (
        Option<egui::TextureHandle>,
        Vec<AssetThumb>,
        Vec<(AssetAccount, [(EmoteProvider, usize); 4])>,
        Vec<PlatformAssetStatus>,
        Vec<(crate::store::AboutSnapshotRow, i64)>,
    ) {
        let icon_tex = self
            .channel_icons
            .entry(ch.id)
            .or_insert_with(|| resolve_channel_icon(ch, accounts, ctx))
            .clone();

        // Mainpage asset thumbnails (icon + banner per account). Loaded full-res
        // once per open; the clone is cheap (Arc-backed texture handles).
        let thumbs = self
            .channel_asset_thumbs
            .entry(ch.id)
            .or_insert_with(|| load_channel_asset_thumbs(ch, accounts, ctx))
            .clone();
        // Viewable emote counts per account+provider — enumerated once per open
        // (one stat per emote) and cached, since this runs every frame.
        let emote_counts = self
            .channel_emote_counts
            .entry(ch.id)
            .or_insert_with(|| emote_provider_counts(&ch.name, accounts))
            .clone();
        // Per-platform asset-status rows — built once per open from blocking filesystem
        // I/O (read_dir + per-file metadata + full JSON manifest parse) and cached, so the
        // status grid (rebuilt every frame the window is open) reads from memory instead of
        // re-running dozens of syscalls per repaint (which can stall the UI thread on slow
        // or AV-scanned storage). The clone is cheap (a handful of small rows).
        let asset_status = self
            .channel_asset_status
            .entry(ch.id)
            .or_insert_with(|| build_platform_asset_status(&ch.name, accounts))
            .clone();
        // Latest About snapshot + version count per account — small indexed
        // rows, loaded once per open (or with the props bundle) and cached.
        let about_latest = self
            .channel_about_latest
            .entry(ch.id)
            .or_insert_with(|| {
                self.core.store.about_latest_per_account(ch.id).unwrap_or_default()
            })
            .clone();

        (icon_tex, thumbs, emote_counts, asset_status, about_latest)
    }

    /// Header row: channel icon (or placeholder), name, and the platform
    /// icon + label list across the channel's instances.
    fn channel_props_header(
        &mut self,
        ui: &mut egui::Ui,
        ch: &Channel,
        platforms: &[Platform],
        icon_tex: &Option<egui::TextureHandle>,
    ) {
        ui.horizontal(|ui| {
            if let Some(tex) = icon_tex {
                let resp = ui.add(
                    egui::Image::from_texture(tex)
                        .max_size(egui::vec2(96.0, 96.0))
                        .corner_radius(egui::CornerRadius::same(8)),
                );
                queue_alt_image_preview(ui.ctx(), &resp, tex);
            } else {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(96.0, 96.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    rect,
                    8.0,
                    ui.visuals().weak_text_color(),
                );
            }
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(4.0);
                ui.heading(&ch.name);
                ui.horizontal(|ui| {
                    let ptex = self
                        .platform_tex
                        .get_or_insert_with(|| PlatformTextures::load(ui.ctx()));
                    for &p in platforms {
                        if let Some(t) = ptex.get(p) {
                            ui.add(
                                egui::Image::from_texture(t)
                                    .max_size(egui::vec2(14.0, 14.0)),
                            );
                        }
                    }
                    let names: Vec<&str> =
                        platforms.iter().map(|p| p.label()).collect();
                    ui.label(if names.is_empty() {
                        "—".to_string()
                    } else {
                        names.join(" · ")
                    });
                });
            });
        });
    }

    /// "Assets" section — thumbnail strip, refetch/history controls, About-page
    /// rows, icon-source picker, per-account status grid and emote-viewer
    /// launchers.
    #[allow(clippy::too_many_arguments)]
    fn channel_props_assets_section(
        &mut self,
        ui: &mut egui::Ui,
        ch: &Channel,
        accounts: &[AssetAccount],
        thumbs: &[AssetThumb],
        emote_counts: &[(AssetAccount, [(EmoteProvider, usize); 4])],
        asset_status: &[PlatformAssetStatus],
        about_latest: &[(crate::store::AboutSnapshotRow, i64)],
        pref_change: &mut Option<Option<crate::models::PreferredAssetSource>>,
        open_emote_viewer: &mut Option<(EmoteProvider, AssetAccount)>,
        open_asset_history: &mut bool,
        open_about_account: &mut Option<AssetAccount>,
        refetch_monitor_ids: &mut Vec<i64>,
    ) {
        egui::CollapsingHeader::new(egui::RichText::new("Assets").strong())
            .id_salt("ch_props_sec_assets")
            .default_open(true)
            .show(ui, |ui| {
        Self::channel_props_asset_thumbs(ui, thumbs);

        {
            Self::channel_props_asset_actions(
                ui,
                ch,
                accounts,
                about_latest,
                open_asset_history,
                open_about_account,
                refetch_monitor_ids,
            );

            Self::channel_props_icon_source_picker(ui, ch, accounts, pref_change);

            self.channel_props_asset_status_grid(ui, asset_status, refetch_monitor_ids);

            self.channel_props_emote_launchers(ui, emote_counts, open_emote_viewer);
        }
            });
    }

    /// Thumbnail overview of every original icon/banner across the channel's
    /// accounts (hover for size, Alt to preview full-res, click to open).
    fn channel_props_asset_thumbs(ui: &mut egui::Ui, thumbs: &[AssetThumb]) {
        if !thumbs.is_empty() {
            ui.add_space(2.0);
            const THUMB_H: f32 = 56.0;
            ui.horizontal_wrapped(|ui| {
                for t in thumbs {
                    let (w, h) = t.dims;
                    let aspect = if h > 0 { w as f32 / h as f32 } else { 1.0 };
                    // Clamp very wide banners so one asset can't dominate.
                    let width = (THUMB_H * aspect).min(THUMB_H * 4.0);
                    let resp = ui
                        .add(
                            egui::Image::from_texture(&t.tex)
                                .fit_to_exact_size(egui::vec2(width, THUMB_H))
                                .corner_radius(egui::CornerRadius::same(4))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(format!(
                            "{} — {w}×{h} px\nAlt: preview full size · click: open file",
                            t.label,
                        ));
                    queue_alt_image_preview(ui.ctx(), &resp, &t.tex);
                    if resp.clicked() {
                        crate::platform::open_path(&t.path);
                    }
                }
            });
        }
    }

    /// Refetch / open-folder / history button row plus the per-account
    /// About-page rows.
    fn channel_props_asset_actions(
        ui: &mut egui::Ui,
        ch: &Channel,
        accounts: &[AssetAccount],
        about_latest: &[(crate::store::AboutSnapshotRow, i64)],
        open_asset_history: &mut bool,
        open_about_account: &mut Option<AssetAccount>,
        refetch_monitor_ids: &mut Vec<i64>,
    ) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if !accounts.is_empty()
                && ui
                    .button("⟳ Refetch")
                    .on_hover_text(
                        "Fetch icon / banner / badges / emotes for EVERY account of \
                         this channel now — ignores the 24h cache.",
                    )
                    .clicked()
            {
                refetch_monitor_ids.extend(accounts.iter().map(|a| a.monitor_id));
            }
            if ui
                .button("📂")
                .on_hover_text(
                    "Open the channel's asset folder (per-platform icons, banners, \
                     and the history/ archive of older versions).",
                )
                .clicked()
            {
                let root = crate::app_paths::asset_cache_dir()
                    .join("channel_assets")
                    .join(crate::downloader::sanitize_filename(&ch.name));
                crate::platform::open_path(&root);
            }
            if ui
                .button("🕑 History")
                .on_hover_text(
                    "Show this channel's recorded asset changes over time \
                     (added / removed emotes, icon / banner / name-colour \
                     replacements) across all its platforms.",
                )
                .clicked()
            {
                *open_asset_history = true;
            }
        });

        // About pages — one row per asset account, each opening the
        // archived About-page viewer (version picker + rendered
        // description/panels/links). Captured with every asset fetch.
        if !accounts.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("About pages").strong());
            for acc in accounts {
                ui.horizontal(|ui| {
                    if ui
                        .button(format!("ℹ {}", acc.label))
                        .on_hover_text(
                            "Show this account's archived About page with a \
                             version picker. A new version is stored only \
                             when the content actually changed.",
                        )
                        .clicked()
                    {
                        *open_about_account = Some(acc.clone());
                    }
                    match about_latest.iter().find(|(s, _)| {
                        s.platform == acc.platform.as_str() && s.account == acc.account
                    }) {
                        Some((s, n)) => ui.weak(format!(
                            "{n} version(s) · captured {} · checked {}",
                            fmt_datetime_short(s.fetched_at),
                            fmt_datetime_short(s.last_checked_at),
                        )),
                        None => ui.weak("never captured"),
                    };
                });
            }
        }
    }

    /// Icon source picker — one entry per asset ACCOUNT (a channel can hold a
    /// main + alt on one platform).
    fn channel_props_icon_source_picker(
        ui: &mut egui::Ui,
        ch: &Channel,
        accounts: &[AssetAccount],
        pref_change: &mut Option<Option<crate::models::PreferredAssetSource>>,
    ) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Icon source:");
            let cur = ch.preferred_asset.clone();
            // A legacy bare-platform preference matches that platform's
            // FIRST account row.
            let selected_idx = cur.as_ref().and_then(|p| {
                accounts.iter().position(|a| {
                    a.platform == p.platform
                        && p.account.as_deref().is_none_or(|acc| acc == a.account)
                })
            });
            egui::ComboBox::from_id_salt("pref_plat_cb")
                .selected_text(match selected_idx {
                    Some(i) => accounts[i].label.clone(),
                    None => "Auto (first available)".to_string(),
                })
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(cur.is_none(), "Auto (first available)")
                        .clicked()
                    {
                        *pref_change = Some(None);
                    }
                    for (i, a) in accounts.iter().enumerate() {
                        if ui
                            .selectable_label(selected_idx == Some(i), &a.label)
                            .clicked()
                        {
                            *pref_change =
                                Some(Some(crate::models::PreferredAssetSource {
                                    platform: a.platform,
                                    account: Some(a.account.clone()),
                                }));
                        }
                    }
                })
                .response
                .on_hover_text(
                    "Which account's profile pic represents this channel. \
                     Auto uses the first account that has a fetched icon.",
                );
        });
    }

    /// Per-account asset status grid (one row per platform ACCOUNT — a main +
    /// alt on one platform get separate rows), each with its own ⟳ refetch.
    fn channel_props_asset_status_grid(
        &mut self,
        ui: &mut egui::Ui,
        asset_status: &[PlatformAssetStatus],
        refetch_monitor_ids: &mut Vec<i64>,
    ) {
        ui.add_space(4.0);
        egui::Grid::new("props_assets")
            .num_columns(7)
            .spacing([12.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Source");
                ui.strong("Icon");
                ui.strong("Banner");
                ui.strong("Badges");
                ui.strong("Emotes");
                ui.strong("Updated");
                ui.strong("");
                ui.end_row();
                // Reads from the per-open `asset_status` cache — NO filesystem
                // I/O here (it would otherwise be dozens of syscalls per frame;
                // see `PlatformAssetStatus`).
                for st in asset_status {
                    ui.horizontal(|ui| {
                        let ptex = self.platform_tex.get_or_insert_with(|| {
                            PlatformTextures::load(ui.ctx())
                        });
                        if let Some(t) = ptex.get(st.account.platform) {
                            ui.add(
                                egui::Image::from_texture(t)
                                    .max_size(egui::vec2(13.0, 13.0)),
                            );
                        }
                        ui.label(&st.account.label);
                    });
                    asset_status_cell(ui, st.icon_present, st.icon_variants);
                    asset_status_cell(ui, st.banner_present, st.banner_variants);
                    ui.label(if st.badges > 0 {
                        st.badges.to_string()
                    } else {
                        "—".into()
                    });
                    ui.label(if st.emotes > 0 {
                        st.emotes.to_string()
                    } else {
                        "—".into()
                    });
                    ui.label(&st.stamp);
                    if ui
                        .small_button("⟳")
                        .on_hover_text(format!(
                            "Refetch assets for {} only (ignores the 24h cache).",
                            st.account.label,
                        ))
                        .clicked()
                    {
                        refetch_monitor_ids.push(st.account.monitor_id);
                    }
                    ui.end_row();
                }
            });
    }

    /// Emote viewers: one launcher per (account, provider) that has emotes.
    /// Twitch (first-party) uses a generic emote glyph; the third parties use
    /// their brand logo (7TV/BTTV) or a text badge (FFZ). Sibling accounts
    /// each get their own labelled row.
    #[allow(deprecated)] // egui::ImageButton
    fn channel_props_emote_launchers(
        &mut self,
        ui: &mut egui::Ui,
        emote_counts: &[(AssetAccount, [(EmoteProvider, usize); 4])],
        open_emote_viewer: &mut Option<(EmoteProvider, AssetAccount)>,
    ) {
        if emote_counts.iter().any(|(_, counts)| counts.iter().any(|(_, n)| *n > 0)) {
            ui.add_space(6.0);
            for (acc, counts) in emote_counts {
                if !counts.iter().any(|(_, n)| *n > 0) {
                    continue;
                }
                ui.horizontal(|ui| {
                    if acc.has_siblings {
                        ui.strong(format!("View emotes ({}):", acc.account));
                    } else {
                        ui.strong("View emotes:");
                    }
                    let ptex = self
                        .provider_tex
                        .get_or_insert_with(|| ProviderTextures::load(ui.ctx()))
                        .clone();
                    for &(provider, count) in counts {
                        if count == 0 {
                            continue;
                        }
                        let resp = match provider {
                            EmoteProvider::SevenTv => ui.add(egui::ImageButton::new(
                                egui::Image::from_texture(&ptex.seventv)
                                    .fit_to_exact_size(egui::vec2(18.0, 18.0)),
                            )),
                            EmoteProvider::Bttv => ui.add(egui::ImageButton::new(
                                egui::Image::from_texture(&ptex.bttv)
                                    .fit_to_exact_size(egui::vec2(18.0, 18.0)),
                            )),
                            EmoteProvider::Twitch => ui.button("😀"),
                            EmoteProvider::Ffz => ui.button("FFZ"),
                        };
                        if resp
                            .on_hover_text(format!(
                                "View {} {} emote{} for {}",
                                count,
                                provider.label(),
                                if count == 1 { "" } else { "s" },
                                acc.label,
                            ))
                            .clicked()
                        {
                            *open_emote_viewer = Some((provider, acc.clone()));
                        }
                    }
                });
            }
        }
    }

    /// "Channel" section — the read-only channel-identity grid.
    fn channel_props_channel_section(ui: &mut egui::Ui, ch: &Channel) {
        egui::CollapsingHeader::new(egui::RichText::new("Channel").strong())
            .id_salt("ch_props_sec_channel")
            .default_open(true)
            .show(ui, |ui| {
        egui::Grid::new("props_ch")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                ui.label("DB channel ID");
                ui.label(ch.id.to_string());
                ui.end_row();

                ui.label("URL");
                if ui.link(&ch.url).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(ch.url.clone()));
                }
                ui.end_row();

                if ch.platform == Platform::YouTube {
                    let yt_id = extract_yt_channel_id(&ch.url)
                        .unwrap_or_else(|| "— (handle URL, ID not in URL)".into());
                    ui.label("Channel ID");
                    ui.horizontal(|ui| {
                        ui.label(&yt_id);
                        if !yt_id.starts_with('—')
                            && ui.small_button("⧉").on_hover_text("Copy").clicked()
                        {
                            ui.ctx().copy_text(yt_id.clone());
                        }
                    });
                    ui.end_row();
                }
            });
            });
    }

    /// "Trigger words" section — the per-channel trigger-scope editor.
    fn channel_props_triggers_section(
        &mut self,
        ui: &mut egui::Ui,
        cid: i64,
        trigger_dirty: &mut bool,
        block_dirty: &mut bool,
    ) {
        egui::CollapsingHeader::new(egui::RichText::new("Trigger words").strong())
            .id_salt("ch_props_sec_triggers")
            .default_open(true)
            .show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "Start recording when the live title/game matches — even with Auto \
                     off — for every instance in this channel. Inherits the global rules \
                     (Settings → Downloads → Trigger words); instances can override again.",
                )
                .small()
                .weak(),
            );
            if let Some(scope) = self.channel_trigger_drafts.get_mut(&cid)
                && trigger_scope_editor(ui, scope, "ch_triggers", true)
            {
                *trigger_dirty = true;
            }
            });
        egui::CollapsingHeader::new(egui::RichText::new("Blacklist triggers").strong())
            .id_salt("ch_props_sec_block_triggers")
            .default_open(false)
            .show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "PREVENT automatic recording while the live title/game matches — for \
                     every instance in this channel; manual ▶ Start still records. \
                     Inherits the global blacklist (Settings → Downloads → Blacklist \
                     triggers); instances can override again.",
                )
                .small()
                .weak(),
            );
            if let Some(scope) = self.channel_block_drafts.get_mut(&cid)
                && trigger_scope_editor(ui, scope, "ch_block_triggers", false)
            {
                *block_dirty = true;
            }
            });
    }

    /// "Schedule sources (this channel)" section — per-channel source config
    /// (Twitter handle, OCR overrides, …) plus the scope-override editor.
    fn channel_props_sched_section(
        &mut self,
        ui: &mut egui::Ui,
        cid: i64,
        global_order: &[SourceEntry],
        cfg_dirty: &mut bool,
        scope_dirty: &mut bool,
    ) {
        if let Some(cfg) = self.channel_cfg_drafts.get_mut(&cid) {
            egui::CollapsingHeader::new(
                egui::RichText::new("Schedule sources (this channel)").strong(),
            )
            .id_salt("ch_props_sec_sched")
            .default_open(false)
            .show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "Used by the image/scrape sources you enable in Settings → Schedule \
                     sources. Changes apply on the next schedule refresh (or click ⟳ in \
                     the Schedule tab).",
                )
                .small()
                .weak(),
            );
            egui::Grid::new("props_sched_src")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Twitter/X handle");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut cfg.twitter_handle)
                                .hint_text("without @")
                                .desired_width(240.0),
                        )
                        .on_hover_text(
                            "Used by the 'Twitter/X pinned' source to read the schedule \
                             off the pinned tweet's image.",
                        )
                        .changed()
                    {
                        *cfg_dirty = true;
                    }
                    ui.end_row();

                    ui.label("Other image (path/URL)");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut cfg.other_image)
                                .hint_text("local path or https://…")
                                .desired_width(240.0),
                        )
                        .on_hover_text(
                            "Used by the 'Other image (OCR)' source — point it at any \
                             schedule image (a saved screenshot or a direct image URL).",
                        )
                        .changed()
                    {
                        *cfg_dirty = true;
                    }
                    ui.end_row();

                    ui.label("OCR model override");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut cfg.ocr_model)
                                .hint_text("(global default)")
                                .desired_width(240.0),
                        )
                        .changed()
                    {
                        *cfg_dirty = true;
                    }
                    ui.end_row();

                    ui.label("OCR primary timezone");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut cfg.ocr_timezone)
                                .hint_text("(global default)")
                                .desired_width(240.0),
                        )
                        .on_hover_text(
                            "Primary IANA timezone this channel's schedule images are \
                             written in (e.g. America/Los_Angeles). Anchors the date when \
                             an image lists multiple timezones for one stream.",
                        )
                        .changed()
                    {
                        *cfg_dirty = true;
                    }
                    ui.end_row();

                    ui.label("OCR UTC offset override");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut cfg.ocr_offset)
                                .hint_text("(global default, e.g. +02:00)")
                                .desired_width(240.0),
                        )
                        .changed()
                    {
                        *cfg_dirty = true;
                    }
                    ui.end_row();

                    ui.label("YouTube community backlog");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut cfg.max_community_posts)
                                .hint_text("(global default)")
                                .desired_width(80.0),
                        )
                        .on_hover_text(
                            "How many recent YouTube community posts to scan for this \
                             channel's schedule image. Empty = use the global setting. \
                             Clamped to 1–20.",
                        )
                        .changed()
                    {
                        *cfg_dirty = true;
                    }
                    ui.end_row();
                });

            // Per-channel source-order + title-fill override.
            ui.add_space(6.0);
            if let Some(scope) = self.channel_scope_drafts.get_mut(&cid) {
                if scope_override_editor(ui, scope, global_order) {
                    *scope_dirty = true;
                }
            }
            });
        }
    }

    /// Apply an icon-source change picked in the picker combo.
    fn channel_props_apply_pref_change(
        &mut self,
        ch: &Channel,
        pref_change: &mut Option<Option<crate::models::PreferredAssetSource>>,
    ) {
        if let Some(newp) = pref_change.take() {
            if let Err(e) =
                self.core.store.set_channel_preferred_asset(ch.id, newp.as_ref())
            {
                self.status = format!("Error: {e}");
            } else {
                self.channel_icons.remove(&ch.id);
                // channel_icons_small intentionally omitted — same viewport-race
                // reason as the Refetch button above. Refreshes on next AssetFetch.
                self.reload_rows();
            }
        }
    }

    /// Post-render actions for the channel window: dispatch the collected
    /// Refetch requests (outside the viewport closure), open the emote-viewer /
    /// asset-history / About windows requested by button clicks, and persist
    /// dirty config/scope/trigger drafts.
    #[allow(clippy::too_many_arguments)]
    fn channel_props_apply_actions(
        &mut self,
        cid: i64,
        ch: &Channel,
        accounts: &[AssetAccount],
        mut refetch_monitor_ids: Vec<i64>,
        open_emote_viewer: Option<(EmoteProvider, AssetAccount)>,
        open_asset_history: bool,
        open_about_account: Option<AssetAccount>,
        cfg_dirty: bool,
        scope_dirty: bool,
        trigger_dirty: bool,
        block_dirty: bool,
    ) {
        // The Refetch buttons (header = every account, per-row = one account)
        // dispatch outside the viewport closure.
        if !refetch_monitor_ids.is_empty() {
            refetch_monitor_ids.sort_unstable();
            refetch_monitor_ids.dedup();
            for mid in &refetch_monitor_ids {
                self.core.manual(ManualCommand::RefetchAssets(*mid));
            }
            self.channel_icons.remove(&ch.id);
            // channel_icons_small is NOT cleared here: this runs while a child
            // viewport is being painted; freeing the texture from the shared
            // painter before the main viewport paints the streams table causes
            // "Failed to find texture" warnings. The small icon reloads on the
            // next AssetFetch completion (logic() clears it before rendering).
            self.channel_twitch_colors.remove(&ch.id);
            self.channel_asset_thumbs.remove(&ch.id);
            self.channel_emote_counts.remove(&ch.id);
            self.channel_asset_status.remove(&ch.id);
            self.channel_about_latest.remove(&ch.id);
            self.status = format!("Refetching assets for {}…", ch.name);
        }
        // A launcher button was clicked above — open (or refresh) the emote
        // viewer for this channel+account+provider; other viewers stay open.
        if let Some((provider, acc)) = open_emote_viewer {
            let fresh = EmoteViewer::new(
                ch.name.clone(),
                acc.account.clone(),
                acc.has_siblings,
                provider,
            );
            match self.emote_viewers.iter_mut().find(|v| {
                v.channel_name == ch.name && v.account == acc.account && v.provider == provider
            }) {
                Some(slot) => *slot = fresh, // re-enumerated → stale flag reset
                None => self.emote_viewers.push(fresh),
            }
        }
        // The "🕑 History" button was clicked — load and open the asset-history popup.
        if open_asset_history {
            let fresh = AssetHistoryView::new(ch.name.clone(), accounts);
            match self
                .asset_histories
                .iter_mut()
                .find(|h| h.channel_name == ch.name)
            {
                Some(slot) => *slot = fresh,
                None => self.asset_histories.push(fresh),
            }
        }
        // ℹ — open the archived About-page viewer for one of the accounts.
        if let Some(acc) = open_about_account {
            self.open_about_view(ch.id, &ch.name, &acc);
        }

        if cfg_dirty {
            if let Some(cfg) = self.channel_cfg_drafts.get(&cid) {
                if let Err(e) = save_channel_cfg(&self.core.store, cid, cfg) {
                    self.status = format!("Error saving channel config: {e}");
                }
            }
        }
        if scope_dirty {
            if let Some(scope) = self.channel_scope_drafts.get(&cid) {
                if let Err(e) = save_channel_scope(&self.core.store, cid, scope) {
                    self.status = format!("Error saving channel sources: {e}");
                } else {
                    self.core.request_schedule_refresh();
                }
            }
        }
        if trigger_dirty
            && let Some(scope) = self.channel_trigger_drafts.get(&cid)
            && let Err(e) = crate::triggers::save_channel_trigger_scope(&self.core.store, cid, scope)
        {
            self.status = format!("Error saving trigger words: {e}");
        }
        if block_dirty
            && let Some(scope) = self.channel_block_drafts.get(&cid)
            && let Err(e) = crate::triggers::save_channel_block_scope(&self.core.store, cid, scope)
        {
            self.status = format!("Error saving blacklist triggers: {e}");
        }
    }

    /// Render every open emote-viewer window (one per channel+provider) — a
    /// grid of images with their codes; emotes whose image file is gone (the
    /// manifest still lists them, but the file was removed upstream) appear
    /// separately under "Deprecated". Reuses the shared `emote_anim` decode
    /// cache, so emotes animate against the same clock as chat replay.
    pub(super) fn emote_viewer_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<(String, String, EmoteProvider)> = Vec::new();
        for i in 0..self.emote_viewers.len() {
            let key = (
                self.emote_viewers[i].channel_name.clone(),
                self.emote_viewers[i].account.clone(),
                self.emote_viewers[i].provider,
            );
            if self.emote_viewer_window(ctx, i) {
                closed.push(key);
            }
        }
        if !closed.is_empty() {
            self.emote_viewers.retain(|v| {
                !closed.contains(&(v.channel_name.clone(), v.account.clone(), v.provider))
            });
            if self.emote_viewers.is_empty() {
                // Free decoded emote frames now the last viewer is closed
                // (mirrors the chat popup). An open chat replay re-decodes its
                // visible emotes next frame — cheap and bounded.
                self.clear_emote_cache();
            }
        }
    }

    /// One emote-viewer window; returns true when it should close.
    #[allow(deprecated)]
    pub(super) fn emote_viewer_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        // Watchdog: name this phase (REPRO 2 — emote viewer grid left open).
        self.heartbeat.set_activity(crate::watchdog::Activity::EmoteViewerGrid);
        // Render toggles + shared cache copied out before borrowing the viewer, so
        // the closure never has to touch `self`.
        let anim_cache = self.emote_anim.clone();
        let animate_emotes = self.animate_emotes;
        let now = ctx.input(|i| i.time);
        let mut decode_misses: Vec<std::path::PathBuf> = Vec::new();

        // Extract everything we need from the viewer up front, so the borrow ends
        // before we need to mutably borrow self later (for NLL borrow checker).
        let viewer = &self.emote_viewers[idx];
        // Were this channel's assets refetched while the window stayed open? The
        // lists below were enumerated once on open, so they no longer reflect disk.
        let stale = viewer.stale;
        let provider = viewer.provider;
        let channel_name = viewer.channel_name.clone();
        let account = viewer.account.clone();
        let title_channel = if viewer.has_siblings {
            format!("{channel_name} ({account})")
        } else {
            channel_name.clone()
        };
        let active = viewer.active.clone();
        let deprecated = viewer.deprecated.clone();
        let current_properties = viewer.emote_properties.clone();
        // viewer borrow ends here; all derived values are owned or Copy

        let mut open = true;
        let mut pending_properties: Option<ViewerEmote> = None;
        let mut clear_properties = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of((
                "emote_viewer_vp",
                &channel_name,
                &account,
                provider.label(),
            )),
            egui::ViewportBuilder::default()
                .with_title(format!("{} emotes — {title_channel}", provider.label()))
                .with_inner_size([560.0, 600.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if stale {
                        // Assets were refetched behind this window; the lists are a
                        // snapshot from when it opened. Amber, matching other
                        // "outdated info" hints elsewhere in the UI.
                        ui.colored_label(
                            egui::Color32::from_rgb(0xe0, 0xb0, 0x6c),
                            "⚠ Assets were refetched while this was open — close and \
                             reopen to see the latest.",
                        );
                        ui.separator();
                    }
                    ui.horizontal(|ui| {
                        ui.heading(provider.label());
                        // Active tally, plus the deprecated count when any — so the two
                        // reconcile with the launcher button (which counts the universe).
                        let mut tally = format!(
                            "· {} emote{}",
                            active.len(),
                            if active.len() == 1 { "" } else { "s" },
                        );
                        if !deprecated.is_empty() {
                            tally.push_str(&format!(" · {} deprecated", deprecated.len()));
                        }
                        ui.label(egui::RichText::new(tally).weak());
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        // Only claim "no emotes" when there is genuinely nothing to show.
                        // If `active` is empty but `deprecated` is not, the Deprecated
                        // section below carries the explanation — saying "no emotes
                        // available" above a populated list would contradict itself.
                        if active.is_empty() && deprecated.is_empty() {
                            ui.add_space(8.0);
                            ui.weak("No emotes available for this provider.");
                        } else if !active.is_empty() {
                            emote_viewer_grid(
                                ui,
                                &active,
                                &anim_cache,
                                animate_emotes,
                                now,
                                &mut decode_misses,
                                ctx,
                                false,
                                provider,
                                &mut pending_properties,
                            );
                        }
                        // The "Deprecated" section appears only when the manifest
                        // still references codes whose image is gone from the cache.
                        if !deprecated.is_empty() {
                            ui.add_space(12.0);
                            ui.separator();
                            ui.strong("Deprecated (no longer available)");
                            ui.label(
                                egui::RichText::new(
                                    "Still listed in this channel's emote manifest, but the image \
                                     is gone from the cache (removed upstream). Refetch assets to \
                                     prune them.",
                                )
                                .small()
                                .weak(),
                            );
                            ui.add_space(6.0);
                            emote_viewer_grid(
                                ui,
                                &deprecated,
                                &anim_cache,
                                animate_emotes,
                                now,
                                &mut decode_misses,
                                ctx,
                                true,
                                provider,
                                &mut pending_properties,
                            );
                        }
                    });
                });

                // Properties window (floats above the central panel)
                if let Some(ep) = &current_properties {
                    let url = emote_cdn_url(provider, &ep.id, &ep.ext);
                    // Probe cache: this runs every frame while the window is open.
                    let size_bytes = self.fs_probes.len(&ep.path);
                    let mut prop_open = true;
                    egui::Window::new("Emote Properties")
                        .collapsible(false)
                        .resizable(false)
                        .open(&mut prop_open)
                        .show(ctx, |ui| {
                            egui::Grid::new("ep_grid")
                                .num_columns(2)
                                .striped(true)
                                .show(ui, |ui| {
                                    ui.label("Name:");
                                    ui.label(&ep.name);
                                    ui.end_row();
                                    ui.label("ID:");
                                    ui.label(&ep.id);
                                    ui.end_row();
                                    ui.label("Provider:");
                                    ui.label(provider.label());
                                    ui.end_row();
                                    ui.label("Extension:");
                                    ui.label(&ep.ext);
                                    ui.end_row();
                                    ui.label("File:");
                                    ui.label(ep.path.to_string_lossy());
                                    ui.end_row();
                                    ui.label("Size:");
                                    ui.label(fmt_bytes(size_bytes as i64));
                                    ui.end_row();
                                    ui.label("URL:");
                                    ui.label(&url);
                                    ui.end_row();
                                });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui.button("Copy URL").clicked() {
                                    ui.ctx().copy_text(url.clone());
                                }
                                if ui.button("Open File").clicked() {
                                    open_path(&ep.path);
                                }
                                if ui.button("Open Folder").clicked() {
                                    if let Some(dir) = ep.path.parent() {
                                        open_path(dir);
                                    }
                                }
                            });
                        });
                    if !prop_open {
                        clear_properties = true;
                    }
                }

                draw_alt_image_preview(ctx);
            },
        );

        // Apply context-menu / properties-window state changes collected during render.
        if let Some(ep) = pending_properties {
            if let Some(viewer) = self.emote_viewers.get_mut(idx) {
                viewer.emote_properties = Some(ep);
            }
        } else if clear_properties {
            if let Some(viewer) = self.emote_viewers.get_mut(idx) {
                viewer.emote_properties = None;
            }
        }

        self.pump_emote_decodes(decode_misses, now, ctx);

        !open
    }

    /// Asset change-history popup: the recorded add/remove of emotes plus
    /// icon / banner / name-colour replacements for one channel, newest first,
    /// across all its platforms. Lines are built once on open (see
    /// [`AssetHistoryView::new`]); this just renders the snapshot. Mirrors
    /// [`Self::meta_popup_window`].
    /// Render every open asset-history window (one per channel).
    pub(super) fn asset_history_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<String> = Vec::new();
        for i in 0..self.asset_histories.len() {
            let name = self.asset_histories[i].channel_name.clone();
            if self.asset_history_window(ctx, i) {
                closed.push(name);
            }
        }
        if !closed.is_empty() {
            self.asset_histories.retain(|h| !closed.contains(&h.channel_name));
        }
    }

    /// One asset-history window; returns true when it should close.
    #[allow(deprecated)]
    pub(super) fn asset_history_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        let view = &self.asset_histories[idx];
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("asset_history_vp", &view.channel_name)),
            egui::ViewportBuilder::default()
                .with_title(format!("Asset history — {}", view.channel_name))
                .with_inner_size([560.0, 440.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if view.lines.is_empty() {
                        ui.label(
                            "No asset changes recorded yet. The first asset fetch is the \
                             baseline; changes appear here after a later refetch alters the \
                             channel's emotes, icon, banner, or name colour.",
                        );
                        return;
                    }
                    ui.label(format!(
                        "{} recorded change(s), newest first. Removed emotes are kept here \
                         even after they vanish from the channel's manifest.",
                        view.lines.len(),
                    ));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for line in &view.lines {
                            ui.label(egui::RichText::new(line).monospace());
                        }
                    });
                    ui.add_space(6.0);
                    if ui.button("📋  Copy").clicked() {
                        ui.ctx().copy_text(view.lines.join("\n"));
                    }
                });
            },
        );
        !open
    }

    /// Open (or refresh in place) the About-page viewer for one account.
    pub(super) fn open_about_view(&mut self, channel_id: i64, channel_name: &str, acc: &AssetAccount) {
        let fresh = AboutView::new(
            &self.core.store,
            channel_id,
            channel_name.to_string(),
            acc.platform,
            acc.account.clone(),
            acc.label.clone(),
        );
        match self.about_views.iter_mut().find(|v| {
            v.channel_id == channel_id && v.platform == acc.platform && v.account == acc.account
        }) {
            Some(slot) => *slot = fresh,
            None => self.about_views.push(fresh),
        }
    }

    /// Render every open About-page viewer (one per channel+platform+account).
    pub(super) fn about_windows(&mut self, ctx: &egui::Context) {
        let mut closed: Vec<usize> = Vec::new();
        for i in 0..self.about_views.len() {
            if self.about_window(ctx, i) {
                closed.push(i);
            }
        }
        for i in closed.into_iter().rev() {
            self.about_views.remove(i);
        }
    }

    /// One About-page viewer window; returns true when it should close.
    /// Version picker (newest first) + the selected version's description
    /// (markdown), panel cards (image / title / markdown body / link), and
    /// external links. Mirrors [`Self::asset_history_window`]'s lifecycle.
    #[allow(deprecated)]
    pub(super) fn about_window(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        let view = &mut self.about_views[idx];
        let mut open = true;
        let mut open_url: Option<String> = None;
        let mut select: Option<usize> = None;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of((
                "about_vp",
                view.channel_id,
                view.platform.as_str(),
                &view.account,
            )),
            egui::ViewportBuilder::default()
                .with_title(format!("About — {} · {}", view.channel_name, view.label))
                .with_inner_size([560.0, 640.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    if view.snapshots.is_empty() {
                        ui.label(
                            "No About page captured yet. It is archived with each channel \
                             asset fetch — use ⟳ Refetch in Properties to force one now.",
                        );
                        return;
                    }
                    let snap = view.snapshots[view.selected].clone();
                    ui.horizontal(|ui| {
                        ui.label("Version:");
                        egui::ComboBox::from_id_salt("about_ver")
                            .selected_text(about_version_label(&view.snapshots, view.selected))
                            .show_ui(ui, |ui| {
                                for i in 0..view.snapshots.len() {
                                    if ui
                                        .selectable_label(
                                            i == view.selected,
                                            about_version_label(&view.snapshots, i),
                                        )
                                        .clicked()
                                    {
                                        select = Some(i);
                                    }
                                }
                            });
                        ui.weak(format!(
                            "last checked {}",
                            fmt_datetime_short(snap.last_checked_at)
                        ));
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        if !snap.description.trim().is_empty() {
                            ui.push_id(("about_desc", snap.id), |ui| {
                                egui_commonmark::CommonMarkViewer::new().show(
                                    ui,
                                    &mut view.md_cache,
                                    &snap.description,
                                );
                            });
                            ui.add_space(6.0);
                        }
                        for (i, p) in view.panels.iter().enumerate() {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                if !p.image_path.is_empty() {
                                    show_about_image(ui, &mut view.img_cache, &p.image_hash, &p.image_path);
                                }
                                if !p.title.trim().is_empty() {
                                    ui.strong(&p.title);
                                }
                                if !p.description_md.trim().is_empty() {
                                    ui.push_id(("about_panel", snap.id, i), |ui| {
                                        egui_commonmark::CommonMarkViewer::new().show(
                                            ui,
                                            &mut view.md_cache,
                                            &p.description_md,
                                        );
                                    });
                                }
                                if !p.link.trim().is_empty()
                                    && ui.link(&p.link).on_hover_text("Open in browser").clicked()
                                {
                                    open_url = Some(p.link.clone());
                                }
                            });
                            ui.add_space(4.0);
                        }
                        if !view.links.is_empty() {
                            ui.add_space(4.0);
                            ui.strong("Links");
                            for l in &view.links {
                                let text = if l.title.trim().is_empty() {
                                    l.url.clone()
                                } else {
                                    format!("{} — {}", l.title, l.url)
                                };
                                if ui.link(text).on_hover_text(&l.url).clicked() {
                                    open_url = Some(l.url.clone());
                                }
                            }
                        }
                    });
                });
                draw_alt_image_preview(ctx);
            },
        );
        if let Some(i) = select {
            self.about_views[idx].select(i);
        }
        if let Some(url) = open_url {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
        !open
    }
}
