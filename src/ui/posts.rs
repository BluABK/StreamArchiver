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

impl StreamArchiverApp {
    /// The YouTube posts feed as a top-level tab. Shares [`Self::render_posts_feed`]
    /// with the pop-out posts window.
    pub(super) fn posts_view(&mut self, ui: &mut egui::Ui) {
        self.render_posts_feed(ui);
    }

    /// The pop-out YouTube posts window (📣 header button). Renders the same feed
    /// as the Posts tab via [`Self::render_posts_feed`].
    #[allow(deprecated)] // CentralPanel::show inside a viewport (matches issues_window)
    pub(super) fn posts_window(&mut self, ctx: &egui::Context) {
        if !self.show_posts_window {
            return;
        }
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("posts_vp"),
            egui::ViewportBuilder::default()
                .with_title("📣 YouTube posts")
                .with_inner_size([760.0, 640.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.render_posts_feed(ui);
                });
            },
        );
        if !open {
            self.show_posts_window = false;
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
    pub(super) fn render_posts_feed(&mut self, ui: &mut egui::Ui) {
        use std::time::{Duration, Instant};
        let stale = self
            .posts_refreshed
            .map(|t| t.elapsed() >= Duration::from_secs(5))
            .unwrap_or(true);
        if stale {
            self.posts = self.core.store.list_community_posts(None, 500).unwrap_or_default();
            self.posts_refreshed = Some(Instant::now());
        }
        let posts = std::mem::take(&mut self.posts);

        // ── Toolbar: channel filter + search + refresh ──
        ui.horizontal(|ui| {
            let sel_text = match self.posts_channel_filter {
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
                    ui.selectable_value(&mut self.posts_channel_filter, None, "All channels");
                    let mut chans: Vec<(i64, String)> = {
                        let mut seen = std::collections::HashSet::new();
                        posts
                            .iter()
                            .filter(|p| seen.insert(p.channel_id))
                            .map(|p| (p.channel_id, p.channel.clone()))
                            .collect()
                    };
                    chans.sort_by_key(|a| a.1.to_lowercase());
                    for (cid, name) in chans {
                        ui.selectable_value(&mut self.posts_channel_filter, Some(cid), name);
                    }
                });
            ui.add(
                egui::TextEdit::singleline(&mut self.posts_search)
                    .hint_text("Search…")
                    .desired_width(180.0),
            );
            if !self.posts_search.is_empty() && ui.button("✕").on_hover_text("Clear search").clicked()
            {
                self.posts_search.clear();
            }
            let viewer_n = posts
                .iter()
                .filter(|p| p.author_kind == "viewer")
                .filter(|p| self.posts_channel_filter.is_none_or(|cid| p.channel_id == cid))
                .count();
            if viewer_n > 0 {
                ui.checkbox(
                    &mut self.posts_show_viewer,
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
                    self.posts_refreshed = None;
                }
            });
        });
        ui.separator();

        let q = self.posts_search.trim().to_lowercase();
        let cf = self.posts_channel_filter;
        let show_viewer = self.posts_show_viewer;
        let visible: Vec<usize> = posts
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                (show_viewer || p.author_kind != "viewer")
                    && cf.map(|c| p.channel_id == c).unwrap_or(true)
                    && (q.is_empty()
                        || p.author.to_lowercase().contains(&q)
                        || p.body_text.to_lowercase().contains(&q)
                        || p.channel.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect();

        if posts.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.weak("No YouTube posts yet.");
                ui.weak("Posts are fetched periodically (Background → “YouTube posts refresh”).");
            });
            self.posts = posts;
            return;
        }
        if visible.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| ui.weak("No posts match the filter."));
            self.posts = posts;
            return;
        }

        let render_limit = self.posts_render_limit.max(POSTS_PAGE_SIZE);
        let mut open_url: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for &i in visible.iter().take(render_limit) {
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
                                self.show_post_avatar(ui, &format!("avatar:{}", p.id), &p.author_icon);
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
                            self.show_post_image(ui, &m.content_hash, &m.local_path);
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
                                    self.show_post_image(ui, &m.content_hash, &m.local_path);
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
                if visible.len() > render_limit {
                    ui.vertical_centered(|ui| {
                        if ui
                            .button(format!(
                                "Show {} more",
                                POSTS_PAGE_SIZE.min(visible.len() - render_limit)
                            ))
                            .clicked()
                        {
                            self.posts_render_limit += POSTS_PAGE_SIZE;
                        }
                        ui.weak(format!("{} of {} shown", render_limit, visible.len()));
                    });
                    ui.add_space(6.0);
                }
            });

        if let Some(url) = open_url {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        self.posts = posts;
    }

    /// Render a small (fixed-size) post author avatar from disk, cached in
    /// `post_img_cache`. Avatars are few (one per post) so no visibility gating.
    pub(super) fn show_post_avatar(&mut self, ui: &mut egui::Ui, key: &str, path: &str) {
        let cached = self.post_img_cache.get(key).cloned();
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
                self.post_img_cache.insert(key.to_string(), loaded);
            }
        }
    }

    /// Render a post attachment image from disk at a bounded size, cached in
    /// `post_img_cache`. Off-screen images are NOT decoded (a fixed-height
    /// placeholder is reserved and `is_rect_visible` gates the load), so memory
    /// scales with what's scrolled, not the whole feed. A crude cap clears the
    /// cache if it grows large.
    pub(super) fn show_post_image(&mut self, ui: &mut egui::Ui, hash: &str, path: &str) {
        const MAX_W: f32 = 520.0;
        const MAX_H: f32 = 420.0;
        const PLACEHOLDER_H: f32 = 160.0;
        if self.post_img_cache.len() > 200 {
            self.post_img_cache.clear();
        }
        let cached = self.post_img_cache.get(hash).cloned();
        match cached {
            Some(Some((tex, _))) => {
                let w = ui.available_width().min(MAX_W);
                ui.add(egui::Image::from_texture(&tex).max_width(w).max_height(MAX_H));
            }
            Some(None) => {} // failed to decode — render nothing
            None => {
                let w = ui.available_width().min(MAX_W);
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(w, PLACEHOLDER_H), egui::Sense::hover());
                if ui.is_rect_visible(rect) {
                    let loaded = load_image_texture(std::path::Path::new(path), ui.ctx(), hash);
                    self.post_img_cache.insert(hash.to_string(), loaded);
                    ui.ctx().request_repaint();
                }
            }
        }
    }
}
