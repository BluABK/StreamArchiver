//! Community-posts feed view.

use super::*;

/// Render a community post's body: if `links_json` (a `[{text,url}]` run array)
/// parses, render each run as a label or a clickable hyperlink (1:1 with the
/// source); otherwise fall back to the plain `body_text`.
pub(super) fn render_post_body(ui: &mut egui::Ui, links_json: &str, fallback: &str) {
    if let Ok(runs) = serde_json::from_str::<Vec<serde_json::Value>>(links_json)
        && !runs.is_empty()
    {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for run in &runs {
                let text = run.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                let url = run.get("url").and_then(|u| u.as_str()).unwrap_or("");
                if url.is_empty() {
                    ui.label(text);
                } else {
                    ui.hyperlink_to(text, url);
                }
            }
        });
        return;
    }
    if !fallback.is_empty() {
        ui.label(fallback);
    }
}

/// Decode an image file into an egui texture, returning the texture and its pixel
/// dimensions. `key` must be unique per logical image so textures never collide.
/// Returns `None` when the file is missing or undecodable.
pub(super) fn load_image_texture(
    path: &std::path::Path,
    ctx: &egui::Context,
    key: &str,
) -> Option<(egui::TextureHandle, (u32, u32))> {
    let bytes = crate::iomon::fs::read_sync(crate::iomon::Cat::AssetCache, path).ok()?;
    let img = decode_rgba_bounded(&bytes)?;
    let (w, h) = (img.width(), img.height());
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &img.into_raw());
    let tex = ctx.load_texture(format!("asset_{key}"), color_image, egui::TextureOptions::LINEAR);
    Some((tex, (w, h)))
}

/// Deferred-viewport state for `posts_window`. Mirrors the same session
/// fields the Posts tab uses (`self.posts_channel_filter`/etc) — synced IN
/// from `self` every wrapper call and back OUT after, so editing a filter in
/// either the tab or the pop-out window (both can be open at once) is
/// visible in the other within a frame, same as before this migration when
/// both literally read the same `self` fields. `posts`/`channels` are
/// snapshots (the deferred closure can't reach `self`); `open_excluded`/
/// `refresh` are actions the wrapper applies to the real `self` fields,
/// since opening the Excluded-channels window and forcing a DB reload both
/// need `self` beyond this popup's own state.
pub(super) struct PostsPopupState {
    pub(super) posts: Vec<crate::store::CommunityPostRow>,
    pub(super) channels: Vec<Channel>,
    pub(super) channel_filter: Option<i64>,
    pub(super) focus_post: Option<String>,
    pub(super) render_limit: usize,
    pub(super) search: String,
    pub(super) show_viewer: bool,
    pub(super) open_excluded: bool,
    pub(super) refresh: bool,
    pub(super) closed: bool,
}

impl StreamArchiverApp {
    /// The YouTube posts feed as a top-level tab. Shares [`Self::render_posts_feed`]
    /// with the pop-out posts window.
    pub(super) fn posts_view(&mut self, ui: &mut egui::Ui) {
        Self::posts_maybe_reload(ui.ctx(), &self.core, &mut self.posts, &mut self.posts_refreshed);
        let mut refresh = false;
        Self::render_posts_feed(
            ui,
            &self.channels,
            &self.posts,
            &mut self.posts_channel_filter,
            &mut self.posts_focus_post,
            &mut self.posts_render_limit,
            &mut self.posts_search,
            &mut self.posts_show_viewer,
            &mut self.show_posts_excluded,
            &mut refresh,
            &self.post_img_cache,
        );
        if refresh {
            self.posts_refreshed = None;
        }
    }

    /// The pop-out YouTube posts window (📣 header button). Renders the same feed
    /// as the Posts tab via [`Self::render_posts_feed`].
    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn posts_window(&mut self, ctx: &egui::Context) {
        if !self.show_posts_window {
            self.posts_popup = None;
            return;
        }
        Self::posts_maybe_reload(ctx, &self.core, &mut self.posts, &mut self.posts_refreshed);

        if self.posts_popup.is_none() {
            self.posts_popup = Some(Arc::new(Mutex::new(PostsPopupState {
                posts: Vec::new(),
                channels: Vec::new(),
                channel_filter: self.posts_channel_filter,
                focus_post: self.posts_focus_post.clone(),
                render_limit: self.posts_render_limit,
                search: self.posts_search.clone(),
                show_viewer: self.posts_show_viewer,
                open_excluded: false,
                refresh: false,
                closed: false,
            })));
        }
        let popup_state = self.posts_popup.clone().unwrap();
        // Refreshed every call — same session fields the tab reads directly.
        {
            let mut s = popup_state.lock().unwrap();
            s.posts = self.posts.clone();
            s.channels = self.channels.clone();
            s.channel_filter = self.posts_channel_filter;
            s.focus_post = self.posts_focus_post.clone();
            s.render_limit = self.posts_render_limit;
            s.search = self.posts_search.clone();
            s.show_viewer = self.posts_show_viewer;
        }

        let post_img_cache = self.post_img_cache.clone();
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("posts_vp"),
            egui::ViewportBuilder::default()
                .with_title("📣 YouTube posts")
                .with_inner_size([760.0, 640.0]),
            popup_state.clone(),
            shared,
            move |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    Self::render_posts_feed(
                        ui,
                        &s.channels,
                        &s.posts,
                        &mut s.channel_filter,
                        &mut s.focus_post,
                        &mut s.render_limit,
                        &mut s.search,
                        &mut s.show_viewer,
                        &mut s.open_excluded,
                        &mut s.refresh,
                        &post_img_cache,
                    );
                });
                // Child viewports draw their own copy of the Alt-hover
                // overlay — the main viewport's draw call can't reach here.
                draw_alt_image_preview(ctx);
            },
        );

        let (channel_filter, focus_post, render_limit, search, show_viewer, open_excluded, refresh, closed) = {
            let mut s = popup_state.lock().unwrap();
            let result = (
                s.channel_filter,
                s.focus_post.clone(),
                s.render_limit,
                s.search.clone(),
                s.show_viewer,
                s.open_excluded,
                s.refresh,
                s.closed,
            );
            // Consume the action flags — see commit 1f7a7a0.
            s.open_excluded = false;
            s.refresh = false;
            s.closed = false;
            result
        };
        self.posts_channel_filter = channel_filter;
        self.posts_focus_post = focus_post;
        self.posts_render_limit = render_limit;
        self.posts_search = search;
        self.posts_show_viewer = show_viewer;
        if refresh {
            self.posts_refreshed = None;
        }
        if open_excluded {
            self.show_posts_excluded = true;
        }
        if closed {
            self.show_posts_window = false;
            self.posts_popup = None;
        }
    }

    /// "🚫 Excluded channels…" management window (launched from the Posts
    /// feed toolbar): every channel with a checkbox for `Channel::posts_hidden`.
    /// Checking one hides it from the feed immediately — still fetched and
    /// archived normally either way.
    pub(super) fn posts_excluded_window(&mut self, ctx: &egui::Context) {
        if !self.show_posts_excluded {
            return;
        }
        let mut open = true;
        let mut channels = self.channels.clone();
        channels.sort_by_key(|c| c.name.to_lowercase());
        let mut search = std::mem::take(&mut self.posts_excluded_search);
        let mut toggled: Option<(i64, bool)> = None;
        egui::Window::new("🚫 Excluded channels")
            .collapsible(false)
            .resizable(true)
            .default_width(320.0)
            .default_height(420.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Checked channels' posts are hidden from the 📣 Posts feed — \
                         still fetched and archived normally, just not shown there.",
                    )
                    .small()
                    .weak(),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut search)
                        .hint_text("Filter…")
                        .desired_width(200.0),
                );
                ui.separator();
                let q = search.trim().to_lowercase();
                egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                    for c in channels.iter().filter(|c| q.is_empty() || c.name.to_lowercase().contains(&q))
                    {
                        let mut hidden = c.posts_hidden;
                        if ui.checkbox(&mut hidden, &c.name).changed() {
                            toggled = Some((c.id, hidden));
                        }
                    }
                    if channels.is_empty() {
                        ui.weak("No channels yet.");
                    }
                });
            });
        self.posts_excluded_search = search;
        if !open {
            self.show_posts_excluded = false;
        }
        if let Some((cid, hidden)) = toggled {
            let _ = self.core.store.set_channel_posts_hidden(cid, hidden);
            if let Some(c) = self.channels.iter_mut().find(|c| c.id == cid) {
                c.posts_hidden = hidden;
            }
        }
    }

    /// Render the YouTube community-posts feed (shared by the tab + the window):
    /// a throttle-loaded list of post cards (author, timestamp, body with links,
    /// all images 1:1), with a channel filter + text search. Post rows are moved
    /// out of `self` during render so the lazy image-texture cache (`self`) and
    /// the row data (local) don't alias.
    ///
    /// Only `posts_render_limit` of the filtered rows are actually laid out
    /// (see that field's doc comment) — a plain `ScrollArea` doesn't skip
    /// layout for off-screen content the way a virtualized table does, so
    /// laying out the full up-to-500-row feed every frame regardless of scroll
    /// position was the tab's main cost.
    /// Reload `posts` from the DB if the 5s cache has gone stale. Call once
    /// per frame from wherever this feed is shown (Posts tab, pop-out
    /// window) BEFORE `render_posts_feed`.
    pub(super) fn posts_maybe_reload(
        ctx: &egui::Context,
        core: &Arc<AppCore>,
        posts: &mut Vec<crate::store::CommunityPostRow>,
        posts_refreshed: &mut Option<std::time::Instant>,
    ) {
        let stale = posts_refreshed
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(5))
            .unwrap_or(true);
        // Held while text is selected — see `text_selection_hold`.
        if stale && !super::text_selection_hold(ctx) {
            *posts = core.store.list_community_posts(None, 500).unwrap_or_default();
            *posts_refreshed = Some(std::time::Instant::now());
        }
    }

    /// Render the feed body (toolbar/filters/scroll list). `posts` is a
    /// read-only snapshot — the caller reloads it via `posts_maybe_reload`
    /// first. Free function (not `&mut self`) so it's callable from both the
    /// Posts tab (`posts_view`) and the pop-out `posts_window`'s deferred
    /// closure. `refresh` is set when "⟳ Refresh" is clicked — the caller
    /// should clear its own `posts_refreshed` so the next `posts_maybe_reload`
    /// call re-fetches.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_posts_feed(
        ui: &mut egui::Ui,
        channels: &[Channel],
        posts: &[crate::store::CommunityPostRow],
        channel_filter: &mut Option<i64>,
        focus_post: &mut Option<String>,
        render_limit: &mut usize,
        search: &mut String,
        show_viewer: &mut bool,
        open_excluded: &mut bool,
        refresh: &mut bool,
        post_img_cache: &Mutex<PostImageCache>,
    ) {
        // Channels excluded via "🚫 Excluded channels…" — still fetched and
        // archived normally (see `Channel::posts_hidden`), just not shown
        // here (feed rows or the channel-filter dropdown below) by default.
        // A focused post (from the 🔔 feed's "View post") bypasses this like
        // every other filter, further below.
        let hidden_channels: std::collections::HashSet<i64> =
            channels.iter().filter(|c| c.posts_hidden).map(|c| c.id).collect();

        // ── Toolbar: channel filter + search + refresh ──
        ui.horizontal(|ui| {
            let sel_text = match *channel_filter {
                None => "All channels".to_string(),
                Some(cid) => posts
                    .iter()
                    .find(|p| p.channel_id == cid)
                    .map(|p| p.channel.clone())
                    .unwrap_or_else(|| "Channel".to_string()),
            };
            egui::ComboBox::from_id_salt("posts_channel_filter")
                .selected_text(sel_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(channel_filter, None, "All channels");
                    let mut chans: Vec<(i64, String)> = {
                        let mut seen = std::collections::HashSet::new();
                        posts
                            .iter()
                            .filter(|p| !hidden_channels.contains(&p.channel_id))
                            .filter(|p| seen.insert(p.channel_id))
                            .map(|p| (p.channel_id, p.channel.clone()))
                            .collect()
                    };
                    chans.sort_by_key(|a| a.1.to_lowercase());
                    for (cid, name) in chans {
                        ui.selectable_value(channel_filter, Some(cid), name);
                    }
                });
            if ui
                .button("🚫")
                .on_hover_text(
                    "Excluded channels — hide specific channels' posts from this feed \
                     (they're still fetched/archived normally, just not shown here).",
                )
                .clicked()
            {
                *open_excluded = true;
            }
            ui.add(
                egui::TextEdit::singleline(search)
                    .hint_text("Search…")
                    .desired_width(180.0),
            );
            if !search.is_empty() && ui.button("✕").on_hover_text("Clear search").clicked()
            {
                search.clear();
            }
            let viewer_n = posts
                .iter()
                .filter(|p| p.author_kind == "viewer")
                .filter(|p| channel_filter.is_none_or(|cid| p.channel_id == cid))
                .count();
            if viewer_n > 0 {
                ui.checkbox(
                    show_viewer,
                    format!("Show viewer posts ({viewer_n})"),
                )
                .on_hover_text(
                    "Include posts made by viewers in the channel's Community space \
                     (off by default — only the channel's own posts are shown)",
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("⟳ Refresh")
                    .on_hover_text("Reload the feed from the database")
                    .clicked()
                {
                    *refresh = true;
                }
            });
        });
        // Narrowed to one post by the 🔔 feed's "View post" button: say so, and
        // offer the way back. The banner sits above the list, under the toolbar,
        // so the filters it overrides stay visible and unchanged.
        if let Some(pid) = focus_post.clone() {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("🔎 Showing one post from a notification").weak())
                    .on_hover_text(format!(
                        "Post {pid} — the channel/search filters above are ignored while \
                         this is showing."
                    ));
                if ui
                    .small_button("✕ Show all")
                    .on_hover_text("Back to the whole feed, with the filters above applied again.")
                    .clicked()
                {
                    *focus_post = None;
                }
            });
        }
        ui.separator();

        let q = search.trim().to_lowercase();
        let cf = *channel_filter;
        let show_viewer_val = *show_viewer;
        let focus = focus_post.clone();
        let visible: Vec<usize> = posts
            .iter()
            .enumerate()
            .filter(|(_, p)| match &focus {
                // A focused post bypasses every other filter (that's the point).
                Some(pid) => &p.post_id == pid,
                None => {
                    !hidden_channels.contains(&p.channel_id)
                        && (show_viewer_val || p.author_kind != "viewer")
                        && cf.map(|c| p.channel_id == c).unwrap_or(true)
                        && (q.is_empty()
                            || p.author.to_lowercase().contains(&q)
                            || p.body_text.to_lowercase().contains(&q)
                            || p.channel.to_lowercase().contains(&q))
                }
            })
            .map(|(i, _)| i)
            .collect();

        if focus.is_some() && visible.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.weak("That post isn't in the feed.");
                ui.weak(
                    "Only the newest 500 posts are loaded — use “View on YouTube” \
                     on the notification instead.",
                );
                if ui.button("✕ Show all").clicked() {
                    *focus_post = None;
                }
            });
            return;
        }
        if posts.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.weak("No YouTube posts yet.");
                ui.weak("Posts are fetched periodically (Background → “YouTube posts refresh”).");
            });
            return;
        }
        if visible.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| ui.weak("No posts match the filter."));
            return;
        }

        let effective_render_limit = (*render_limit).max(POSTS_PAGE_SIZE);
        let mut open_url: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for &i in visible.iter().take(effective_render_limit) {
                    let p = &posts[i];
                    // Salt every widget this card creates by the post's own
                    // (stable) id instead of its position in the list — with a
                    // plain position-based id, an image finishing its async
                    // decode (changing that card's height) shifts every widget
                    // below it to a new screen rect on the very next frame,
                    // which egui's debug id-clash check (red outline + a
                    // "Widget rect ... changed id between passes" warning,
                    // debug builds only) flags as if it were a bug.
                    ui.push_id(p.id, |ui| {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // Header: avatar + author + timestamp + channel.
                        ui.horizontal(|ui| {
                            if !p.author_icon.is_empty() {
                                Self::show_post_avatar(ui, post_img_cache, &format!("avatar:{}", p.id), &p.author_icon);
                            }
                            ui.vertical(|ui| {
                                let name = if p.author.is_empty() {
                                    p.channel.as_str()
                                } else {
                                    p.author.as_str()
                                };
                                ui.label(egui::RichText::new(name).strong());
                                ui.horizontal(|ui| {
                                    if !p.published_text.is_empty() {
                                        let resp = ui.small(&p.published_text);
                                        if p.published_at > 0 {
                                            resp.on_hover_text(format!(
                                                "≈ {}",
                                                fmt_datetime_short(p.published_at)
                                            ));
                                        }
                                    }
                                    if !p.channel.is_empty() && p.channel != p.author {
                                        ui.small(format!("· {}", p.channel));
                                    }
                                    if p.author_kind == "viewer" {
                                        ui.small(egui::RichText::new("· viewer").weak())
                                            .on_hover_text(
                                                "A viewer's post in the channel's Community space",
                                            );
                                    }
                                });
                            });
                        });
                        // Body (runs with clickable links, else plain).
                        render_post_body(ui, &p.links_json, &p.body_text);
                        // Attachment images, 1:1, in order.
                        for m in p
                            .media
                            .iter()
                            .filter(|m| m.kind == "image" && !m.local_path.is_empty())
                        {
                            Self::show_post_image(ui, post_img_cache, &m.content_hash, &m.local_path, &m.image_url);
                        }
                        // Reshared/quoted original, as an indented quote card.
                        if !p.shared_json.is_empty()
                            && let Ok(sh) =
                                serde_json::from_str::<serde_json::Value>(&p.shared_json)
                        {
                            let s_author =
                                sh.get("author").and_then(|v| v.as_str()).unwrap_or("");
                            let s_time = sh
                                .get("published_text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let s_body =
                                sh.get("body_text").and_then(|v| v.as_str()).unwrap_or("");
                            let s_links = sh
                                .get("links_json")
                                .and_then(|v| v.as_str())
                                .unwrap_or("[]");
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                let mut head = format!("↪ {s_author}");
                                if !s_time.is_empty() {
                                    head.push_str(&format!(" · {s_time}"));
                                }
                                ui.label(egui::RichText::new(head).weak());
                                render_post_body(ui, s_links, s_body);
                                for m in p.media.iter().filter(|m| {
                                    m.kind == "shared_image" && !m.local_path.is_empty()
                                }) {
                                    Self::show_post_image(
                                        ui,
                                        post_img_cache,
                                        &m.content_hash,
                                        &m.local_path,
                                        &m.image_url,
                                    );
                                }
                            });
                        }
                        ui.horizontal(|ui| {
                            if !p.vote_count.is_empty() {
                                ui.small(format!("👍 {}", p.vote_count));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Open post ↗").clicked() {
                                        open_url = Some(format!(
                                            "https://www.youtube.com/post/{}",
                                            p.post_id
                                        ));
                                    }
                                },
                            );
                        });
                    });
                    });
                    ui.add_space(6.0);
                }
                if visible.len() > effective_render_limit {
                    ui.vertical_centered(|ui| {
                        if ui
                            .button(format!(
                                "Show {} more",
                                POSTS_PAGE_SIZE.min(visible.len() - effective_render_limit)
                            ))
                            .clicked()
                        {
                            *render_limit += POSTS_PAGE_SIZE;
                        }
                        ui.weak(format!("{} of {} shown", effective_render_limit, visible.len()));
                    });
                    ui.add_space(6.0);
                }
            });

        if let Some(url) = open_url {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
    }

    /// Render a small (fixed-size) post author avatar from disk, cached in
    /// `post_img_cache`. Avatars are few (one per post) so no visibility gating.
    /// Free function (not `&mut self`) so it's callable from both the Posts
    /// tab (`posts_view`) and the pop-out `posts_window`'s deferred closure —
    /// `post_img_cache` is a shared `Arc<Mutex<>>` either way.
    pub(super) fn show_post_avatar(
        ui: &mut egui::Ui,
        cache: &Mutex<PostImageCache>,
        key: &str,
        path: &str,
    ) {
        let cached = cache.lock().unwrap().get(key).cloned();
        match cached {
            Some(Some((tex, _))) => {
                ui.add(
                    egui::Image::from_texture(&tex)
                        .fit_to_exact_size(egui::vec2(28.0, 28.0))
                        .corner_radius(egui::CornerRadius::same(14)),
                );
            }
            Some(None) => {
                ui.add_space(28.0);
            }
            None => {
                let loaded = load_image_texture(std::path::Path::new(path), ui.ctx(), key);
                cache.lock().unwrap().insert(key.to_string(), loaded);
            }
        }
    }

    /// Render a post attachment image from disk at a bounded size, cached in
    /// `post_img_cache`. Off-screen images are NOT decoded (a fixed-height
    /// placeholder is reserved and `is_rect_visible` gates the load), so memory
    /// scales with what's scrolled, not the whole feed. A crude cap clears the
    /// cache if it grows large.
    ///
    /// Loaded images get the standard image affordances (matching the About
    /// panel / chat emotes): Alt-hover full-resolution preview, click to open
    /// the file, and a right-click menu (copy image / open file / open folder
    /// / copy the source URL).
    pub(super) fn show_post_image(
        ui: &mut egui::Ui,
        cache: &Mutex<PostImageCache>,
        hash: &str,
        path: &str,
        image_url: &str,
    ) {
        const MAX_W: f32 = 520.0;
        const MAX_H: f32 = 420.0;
        const PLACEHOLDER_H: f32 = 160.0;
        {
            let mut cache = cache.lock().unwrap();
            if cache.len() > 200 {
                cache.clear();
            }
        }
        let cached = cache.lock().unwrap().get(hash).cloned();
        match cached {
            Some(Some((tex, _))) => {
                let w = ui.available_width().min(MAX_W);
                let resp = ui.add(
                    egui::Image::from_texture(&tex)
                        .max_width(w)
                        .max_height(MAX_H)
                        .sense(egui::Sense::click()),
                );
                queue_alt_image_preview(ui.ctx(), &resp, &tex);
                let resp = resp.on_hover_text(
                    "Alt: preview full size · click: open file · right-click: more",
                );
                if resp.clicked() {
                    crate::platform::open_path(std::path::Path::new(path));
                }
                resp.context_menu(|ui| {
                    if ui.button("Copy Image").clicked() {
                        copy_emote_image_to_clipboard(std::path::Path::new(path));
                        ui.close();
                    }
                    if ui.button("Open File").clicked() {
                        crate::platform::open_path(std::path::Path::new(path));
                        ui.close();
                    }
                    if ui.button("Open Folder").clicked() {
                        if let Some(dir) = std::path::Path::new(path).parent() {
                            crate::platform::open_path(dir);
                        }
                        ui.close();
                    }
                    if !image_url.is_empty() && ui.button("Copy URL").clicked() {
                        ui.ctx().copy_text(image_url.to_string());
                        ui.close();
                    }
                });
            }
            Some(None) => {} // failed to decode — render nothing
            None => {
                let w = ui.available_width().min(MAX_W);
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(w, PLACEHOLDER_H), egui::Sense::hover());
                if ui.is_rect_visible(rect) {
                    let loaded = load_image_texture(std::path::Path::new(path), ui.ctx(), hash);
                    cache.lock().unwrap().insert(hash.to_string(), loaded);
                    ui.ctx().request_repaint();
                }
            }
        }
    }
}
