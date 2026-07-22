//! Channel asset/icon/about textures and helpers, emote viewer data,
//! alt-image preview.

use super::*;

/// Draw a small colored brand badge for the platform.
pub(super) fn platform_badge(ui: &mut egui::Ui, platform: Platform) -> egui::Response {
    use egui::{Color32, RichText};
    let (label, bg, fg) = match platform {
        Platform::Twitch => ("T", Color32::from_rgb(0x91, 0x46, 0xFF), Color32::WHITE),
        Platform::YouTube => ("▶", Color32::from_rgb(0xFF, 0x00, 0x00), Color32::WHITE),
        Platform::Kick => ("K", Color32::from_rgb(0x53, 0xFC, 0x18), Color32::BLACK),
        Platform::Nrk => ("N", Color32::from_rgb(0x00, 0x89, 0xE0), Color32::WHITE),
        Platform::Nebula => ("⬢", Color32::from_rgb(0x5E, 0x5C, 0xE6), Color32::WHITE),
        Platform::Generic => ("●", Color32::from_gray(0x80), Color32::WHITE),
    };
    ui.label(
        RichText::new(format!(" {label} "))
            .monospace()
            .strong()
            .color(fg)
            .background_color(bg),
    )
}

/// Platform favicons, decoded to raw 32×32 RGBA at build time (see build.rs) and
/// embedded here so no image decoder ships in the binary.
pub(super) const ICON_SRC: usize = 32;
/// On-screen icon size (favicons are designed for small sizes).
pub(super) const ICON_PX: f32 = 16.0;
pub(super) static TWITCH_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_twitch.rgba"));
pub(super) static YOUTUBE_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_youtube.rgba"));
pub(super) static KICK_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_kick.rgba"));
pub(super) static NRK_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_nrk.rgba"));
pub(super) static NEBULA_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/platform_nebula.rgba"));

/// GPU textures for the platform favicons, uploaded once and cheaply cloned
/// (each `TextureHandle` is reference-counted).
#[derive(Clone)]
pub(super) struct PlatformTextures {
    pub(super) twitch: egui::TextureHandle,
    pub(super) youtube: egui::TextureHandle,
    pub(super) kick: egui::TextureHandle,
    pub(super) nrk: egui::TextureHandle,
    pub(super) nebula: egui::TextureHandle,
}

impl PlatformTextures {
    pub(super) fn load(ctx: &egui::Context) -> PlatformTextures {
        let mk = |name: &str, rgba: &[u8]| {
            let image = egui::ColorImage::from_rgba_unmultiplied([ICON_SRC, ICON_SRC], rgba);
            ctx.load_texture(format!("platform_{name}"), image, egui::TextureOptions::LINEAR)
        };
        PlatformTextures {
            twitch: mk("twitch", TWITCH_ICON),
            youtube: mk("youtube", YOUTUBE_ICON),
            kick: mk("kick", KICK_ICON),
            nrk: mk("nrk", NRK_ICON),
            nebula: mk("nebula", NEBULA_ICON),
        }
    }

    /// The favicon for a platform, or `None` for `Generic` (no favicon → badge).
    pub(super) fn get(&self, p: Platform) -> Option<&egui::TextureHandle> {
        match p {
            Platform::Twitch => Some(&self.twitch),
            Platform::YouTube => Some(&self.youtube),
            Platform::Kick => Some(&self.kick),
            Platform::Nrk => Some(&self.nrk),
            Platform::Nebula => Some(&self.nebula),
            Platform::Generic => None,
        }
    }
}

/// Third-party emote-provider brand logos, rasterized from their SVGs to 64×64
/// straight-alpha RGBA at build time (see `decode_provider_logos` in build.rs) and
/// embedded so no SVG decoder ships in the binary. Used for the emote-viewer
/// launcher buttons (7TV blue, BTTV red). FFZ has no embedded logo (text button).
pub(super) const LOGO_SRC: usize = 64;
pub(super) static LOGO_7TV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo_7tv.rgba"));
pub(super) static LOGO_BTTV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo_bttv.rgba"));

/// GPU textures for the emote-provider logos, uploaded once and cheaply cloned.
#[derive(Clone)]
pub(super) struct ProviderTextures {
    pub(super) seventv: egui::TextureHandle,
    pub(super) bttv: egui::TextureHandle,
}

impl ProviderTextures {
    pub(super) fn load(ctx: &egui::Context) -> ProviderTextures {
        let mk = |name: &str, rgba: &[u8]| {
            let image = egui::ColorImage::from_rgba_unmultiplied([LOGO_SRC, LOGO_SRC], rgba);
            ctx.load_texture(format!("emote_logo_{name}"), image, egui::TextureOptions::LINEAR)
        };
        ProviderTextures {
            seventv: mk("7tv", LOGO_7TV),
            bttv: mk("bttv", LOGO_BTTV),
        }
    }
}

/// Which emote provider the emote viewer is showing. Twitch is first-party
/// (directory-listed, opaque ids, no codes); the rest read their JSON manifests.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EmoteProvider {
    Twitch,
    SevenTv,
    Bttv,
    Ffz,
}

impl EmoteProvider {
    pub(super) fn label(self) -> &'static str {
        match self {
            EmoteProvider::Twitch => "Twitch",
            EmoteProvider::SevenTv => "7TV",
            EmoteProvider::Bttv => "BetterTTV",
            EmoteProvider::Ffz => "FrankerFaceZ",
        }
    }
    pub(super) fn manifest(self) -> Option<&'static str> {
        match self {
            EmoteProvider::Twitch => Some("twitch.json"),
            EmoteProvider::SevenTv => Some("7tv.json"),
            EmoteProvider::Bttv => Some("bttv.json"),
            EmoteProvider::Ffz => Some("ffz.json"),
        }
    }
}

/// Open emote-viewer window: shows one provider's emotes for one channel. The emote
/// list is enumerated once on open (a dir-list / manifest parse) and split into
/// still-present (`active`) and gone-from-cache (`deprecated`) so the window can
/// repaint — e.g. for animation — without re-touching the disk each frame.
pub(super) struct EmoteViewer {
    pub(super) channel_name: String,
    /// Account slug of the instance whose emote cache is shown — a channel can
    /// hold several same-platform accounts (main + alt), each with its own set.
    pub(super) account: String,
    /// True when the channel has sibling accounts on the platform, so the
    /// window title should name the account.
    pub(super) has_siblings: bool,
    pub(super) provider: EmoteProvider,
    pub(super) active: Vec<ViewerEmote>,
    pub(super) deprecated: Vec<ViewerEmote>,
    pub(super) emote_properties: Option<ViewerEmote>,
    /// Set when this channel's assets were refetched while the window stayed
    /// open (the lists are an on-open snapshot).
    pub(super) stale: bool,
    /// Live filter text — case-insensitive substring match on the emote code.
    pub(super) filter: String,
    /// Current sort order for the grids.
    pub(super) sort: EmoteSort,
}

/// Sort orders for the emote-viewer grids.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EmoteSort {
    NameAsc,
    NameDesc,
    /// GIF/animated-WebP files first (by extension — a static WebP can slip
    /// in, but the cache stores animated variants with these extensions).
    AnimatedFirst,
}

impl EmoteSort {
    pub(super) const ALL: [EmoteSort; 3] =
        [EmoteSort::NameAsc, EmoteSort::NameDesc, EmoteSort::AnimatedFirst];

    pub(super) fn label(self) -> &'static str {
        match self {
            EmoteSort::NameAsc => "Name A→Z",
            EmoteSort::NameDesc => "Name Z→A",
            EmoteSort::AnimatedFirst => "Animated first",
        }
    }

    /// Sort `list` in place (ties always break alphabetically).
    pub(super) fn apply(self, list: &mut [ViewerEmote]) {
        match self {
            EmoteSort::NameAsc => list.sort_by_key(|e| e.name.to_lowercase()),
            EmoteSort::NameDesc => {
                list.sort_by_key(|e| std::cmp::Reverse(e.name.to_lowercase()))
            }
            EmoteSort::AnimatedFirst => list.sort_by_key(|e| {
                (!matches!(e.ext.as_str(), "gif" | "webp"), e.name.to_lowercase())
            }),
        }
    }
}

impl EmoteViewer {
    pub(super) fn new(
        channel_name: String,
        account: String,
        has_siblings: bool,
        provider: EmoteProvider,
    ) -> EmoteViewer {
        let (active, deprecated): (Vec<ViewerEmote>, Vec<ViewerEmote>) =
            enumerate_provider_emotes(&channel_name, &account, provider)
                .into_iter()
                .partition(|e| e.exists);
        EmoteViewer {
            channel_name,
            account,
            has_siblings,
            provider,
            active,
            deprecated,
            emote_properties: None,
            stale: false,
            filter: String::new(),
            sort: EmoteSort::NameAsc,
        }
    }
}

/// Open asset change-history popup: a channel's recorded asset changes (added /
/// removed emotes, icon / banner / name-colour replacements) across all its
/// platforms, read from each platform's `asset_changes.jsonl` and formatted once
/// on open into display lines (newest first). Mirrors [`EmoteViewer`]'s
/// load-once-on-open pattern so the popup never touches the disk per frame.
pub(super) struct AssetHistoryView {
    pub(super) channel_name: String,
    /// The channel's asset accounts (non-`Generic`), retained so the view can be
    /// reloaded in place when a background asset refetch lands while it's open.
    pub(super) accounts: Vec<AssetAccount>,
    pub(super) lines: Vec<String>,
}

impl AssetHistoryView {
    pub(super) fn new(channel_name: String, accounts: &[AssetAccount]) -> AssetHistoryView {
        let lines = load_asset_history_lines(&channel_name, accounts);
        AssetHistoryView { channel_name, accounts: accounts.to_vec(), lines }
    }

    /// Re-read the change logs from disk (newest first), keeping the window open.
    /// Called when an asset refetch for this channel completes so a live history
    /// view reflects the just-recorded changes without a manual reopen.
    pub(super) fn reload(&mut self) {
        self.lines = load_asset_history_lines(&self.channel_name, &self.accounts);
    }
}

/// One open About-page viewer: an account's archived about versions (newest
/// first), a version picker, and the selected version's parsed content.
/// Snapshots are queried once on open; [`AboutView::reload`] re-queries when a
/// background asset fetch for the channel lands.
pub(super) struct AboutView {
    pub(super) channel_id: i64,
    pub(super) channel_name: String,
    pub(super) platform: Platform,
    pub(super) account: String,
    /// Display label from [`AssetAccount::label`], e.g. `"Twitch (geega_alt)"`.
    pub(super) label: String,
    pub(super) snapshots: Vec<crate::store::AboutSnapshotRow>,
    /// Index into `snapshots` (0 = newest).
    pub(super) selected: usize,
    /// `panels_json`/`links_json` of the selected version, parsed once per
    /// selection change (not per frame).
    pub(super) panels: Vec<crate::assets::AboutPanel>,
    pub(super) links: Vec<crate::assets::AboutLink>,
    pub(super) md_cache: egui_commonmark::CommonMarkCache,
    /// Panel-image textures keyed by content hash. Deliberately NOT the shared
    /// `post_img_cache` — the posts feed's 200-entry cap would evict panel
    /// textures mid-frame.
    pub(super) img_cache: PostImageCache,
}

impl AboutView {
    pub(super) fn new(
        store: &crate::store::Store,
        channel_id: i64,
        channel_name: String,
        platform: Platform,
        account: String,
        label: String,
    ) -> AboutView {
        let mut view = AboutView {
            channel_id,
            channel_name,
            platform,
            account,
            label,
            snapshots: Vec::new(),
            selected: 0,
            panels: Vec::new(),
            links: Vec::new(),
            md_cache: egui_commonmark::CommonMarkCache::default(),
            img_cache: HashMap::new(),
        };
        view.reload(store);
        view
    }

    /// Re-query the snapshot list (newest first), keeping the selected version
    /// by snapshot id when it still exists (a new version shifts indices).
    pub(super) fn reload(&mut self, store: &crate::store::Store) {
        let keep_id = self.snapshots.get(self.selected).map(|s| s.id);
        self.snapshots = store
            .about_snapshots_for_account(self.channel_id, self.platform.as_str(), &self.account)
            .unwrap_or_default();
        let idx = keep_id
            .and_then(|id| self.snapshots.iter().position(|s| s.id == id))
            .unwrap_or(0);
        self.select(idx);
    }

    /// Change the displayed version and re-parse its panels/links JSON.
    pub(super) fn select(&mut self, idx: usize) {
        self.selected = idx;
        let Some(snap) = self.snapshots.get(idx) else {
            self.panels.clear();
            self.links.clear();
            return;
        };
        self.panels = serde_json::from_str(&snap.panels_json).unwrap_or_default();
        self.links = serde_json::from_str(&snap.links_json).unwrap_or_default();
    }
}

/// Short display label for an emote-provider manifest stem as stored in
/// [`crate::assets::AssetChange::provider`]. Falls back to the raw stem.
pub(super) fn provider_label_from_id(id: &str) -> &str {
    match id {
        "7tv" => "7TV",
        "bttv" => "BTTV",
        "ffz" => "FFZ",
        other => other,
    }
}

/// One display line for a recorded asset change, e.g.
/// `2026-06-29 14:30  Twitch · 7TV emote +PogU`, `… · Twitch icon replaced`, or
/// `… · Twitch name colour #9146FF → #00FF00`.
pub(super) fn fmt_asset_change_line(source_label: &str, c: &crate::assets::AssetChange) -> String {
    let when = fmt_datetime_short(c.at);
    let what = match c.kind.as_str() {
        "emote" => {
            let prov = provider_label_from_id(&c.provider);
            // U+2212 MINUS SIGN for removals to line up visually with '+'.
            let sign = if c.action == "removed" { "−" } else { "+" };
            format!("{prov} emote {sign}{}", c.name)
        }
        "icon" => "icon replaced".to_string(),
        "banner" => "banner replaced".to_string(),
        "name_color" => match c.action.as_str() {
            "added" => format!("name colour set {}", c.new),
            "removed" => format!("name colour cleared (was {})", c.old),
            _ => format!("name colour {} → {}", c.old, c.new),
        },
        other => format!("{other} {}", c.action),
    };
    format!("{when}  {source_label} · {what}")
}

/// Load and format a channel's recorded asset changes across all its asset
/// accounts (plus each platform's legacy pre-account dir), newest first. Empty
/// when nothing has been recorded yet.
pub(super) fn load_asset_history_lines(name: &str, accounts: &[AssetAccount]) -> Vec<String> {
    let mut all: Vec<(String, crate::assets::AssetChange)> = Vec::new();
    let mut legacy_seen: Vec<Platform> = Vec::new();
    for acc in accounts {
        let dir = channel_asset_dir(name, acc.platform, &acc.account);
        for c in crate::assets::read_asset_changes(&dir) {
            all.push((acc.label.clone(), c));
        }
        // Pre-migration entries live in the legacy platform dir — read it once
        // per platform, labelled without an account.
        if !legacy_seen.contains(&acc.platform) {
            legacy_seen.push(acc.platform);
            let legacy = crate::assets::legacy_platform_dir(name, acc.platform);
            for c in crate::assets::read_asset_changes(&legacy) {
                all.push((acc.platform.label().to_string(), c));
            }
        }
    }
    // Newest first. `sort_by_key` is stable, so changes sharing a timestamp keep
    // their per-source append order.
    all.sort_by_key(|c| std::cmp::Reverse(c.1.at));
    all.iter().map(|(l, c)| fmt_asset_change_line(l, c)).collect()
}

/// A loaded mainpage image asset (icon/banner) shown as a thumbnail in the channel
/// Properties window. Holds the full-resolution texture so the global Alt-hover
/// preview shows it at original size; the strip itself draws it small.
#[derive(Clone)]
pub(super) struct AssetThumb {
    /// Human label, e.g. "Twitch icon" / "YouTube banner".
    pub(super) label: String,
    /// Which account this asset belongs to (lets the instance Properties
    /// window filter the channel-wide strip down to its own account).
    pub(super) platform: Platform,
    pub(super) account: String,
    /// `"icon"` or `"banner"`.
    pub(super) kind: &'static str,
    /// Absolute path on disk (opened on click).
    pub(super) path: std::path::PathBuf,
    pub(super) tex: egui::TextureHandle,
    pub(super) dims: (u32, u32),
}

/// One row of the Properties window's per-platform asset-status grid. Every field is
/// derived from blocking filesystem I/O (`read_dir` for presence/variants, a recursive
/// depth-2 badge-dir count, a `read_dir` of the Twitch emote dir, a full JSON parse of
/// each third-party manifest, and a `read_to_string` of the fetch stamp). The grid is
/// rebuilt every frame the window is open, so these rows are computed once on open and
/// cached (see `channel_asset_status`) — doing the I/O per frame is dozens of syscalls
/// per repaint and can stall the UI thread for >10s on slow or AV-scanned storage.
#[derive(Clone)]
pub(super) struct PlatformAssetStatus {
    pub(super) account: AssetAccount,
    pub(super) icon_present: bool,
    pub(super) icon_variants: usize,
    pub(super) banner_present: bool,
    pub(super) banner_variants: usize,
    pub(super) badges: usize,
    pub(super) emotes: usize,
    pub(super) stamp: String,
}

/// One asset ACCOUNT of a channel container: a distinct (platform, account-slug)
/// among its instances. Two tools on one URL collapse to one entry; a main + alt
/// account on the same platform yield two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AssetAccount {
    pub(super) platform: Platform,
    /// [`crate::assets::account_slug`] of the owning instance's URL.
    pub(super) account: String,
    /// First monitor carrying this account — the Refetch dispatch target.
    pub(super) monitor_id: i64,
    /// Display label: `"Twitch"`, or `"Twitch (geega_alt)"` when the channel
    /// has sibling accounts on the platform.
    pub(super) label: String,
    /// True when another account of the same platform exists in the channel.
    pub(super) has_siblings: bool,
}

/// A channel's asset accounts: distinct (platform, account) pairs among its
/// instances, first-seen order, `Generic` skipped (no asset source).
pub(super) fn channel_asset_accounts(monitors: &[&MonitorWithChannel]) -> Vec<AssetAccount> {
    let mut out: Vec<AssetAccount> = Vec::new();
    for m in monitors {
        let platform = m.monitor.platform();
        if !platform.has_asset_fetcher() {
            continue;
        }
        let account = asset_account(&m.monitor.url, platform);
        if !out.iter().any(|a| a.platform == platform && a.account == account) {
            out.push(AssetAccount {
                platform,
                account,
                monitor_id: m.monitor.id,
                label: String::new(),
                has_siblings: false,
            });
        }
    }
    // Label pass: name the account only when the platform has siblings.
    for i in 0..out.len() {
        let siblings = out.iter().filter(|a| a.platform == out[i].platform).count() > 1;
        out[i].has_siblings = siblings;
        out[i].label = if siblings {
            format!("{} ({})", out[i].platform.label(), out[i].account)
        } else {
            out[i].platform.label().to_string()
        };
    }
    out
}
/// Build the Properties window's per-platform asset-status rows. Runs the blocking
/// filesystem I/O once on open (the result is cached in `channel_asset_status`); see
/// [`PlatformAssetStatus`] for why this must not run per frame. Logic mirrors what the
/// status grid used to compute inline: icon/banner presence + archived-variant counts,
/// a depth-2 badge-dir count, the Twitch emote-dir file count plus each third-party
/// manifest length, and the last-fetched stamp.
pub(super) fn build_platform_asset_status(name: &str, accounts: &[AssetAccount]) -> Vec<PlatformAssetStatus> {
    accounts
        .iter()
        .map(|acc| {
            // Read from the account dir; fall back to the legacy pre-account
            // dir so a not-yet-refetched channel still shows its assets.
            let dirs = crate::assets::asset_read_dirs(name, acc.platform, &acc.account);
            let pdir = if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &dirs[0]) { dirs[0].clone() } else { dirs[1].clone() };
            let mut emotes = prop_count_dir_files(&pdir.join("emotes").join("twitch"));
            for src in &["bttv", "ffz", "7tv"] {
                emotes += prop_read_manifest_count(&pdir.join("emotes").join(format!("{src}.json")));
            }
            PlatformAssetStatus {
                account: acc.clone(),
                icon_present: prop_find_first(&pdir, "icon.").is_some(),
                icon_variants: prop_variant_count(&pdir, "icon"),
                banner_present: prop_find_first(&pdir, "banner.").is_some(),
                banner_variants: prop_variant_count(&pdir, "banner"),
                badges: prop_count_nested_dirs(&pdir.join("badges"), 2),
                emotes,
                stamp: fmt_asset_stamp(&pdir),
            }
        })
        .collect()
}

/// One emote row for the emote viewer: a code resolved to its on-disk image, plus
/// whether that image still exists (absent ⇒ shown in the "Deprecated" section).
#[derive(Clone)]
pub(super) struct ViewerEmote {
    pub(super) name: String,
    pub(super) id: String,
    pub(super) ext: String,
    pub(super) path: std::path::PathBuf,
    pub(super) exists: bool,
}

/// Draw one platform's icon (its favicon, or the colored badge for Generic),
/// returning the response so callers can attach the platform name on hover.
pub(super) fn platform_icon(ui: &mut egui::Ui, ptex: &PlatformTextures, platform: Platform) -> egui::Response {
    match ptex.get(platform) {
        Some(handle) => {
            let tex = egui::load::SizedTexture::new(handle.id(), egui::vec2(ICON_PX, ICON_PX));
            ui.image(tex)
        }
        None => platform_badge(ui, platform),
    }
}

/// Draw the platform icon(s) for a cell: one per distinct platform, side by side
/// (a channel may span several). Each shows the platform name on hover.
pub(super) fn platform_icons(ui: &mut egui::Ui, ptex: &PlatformTextures, platforms: &[Platform]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for &p in platforms {
            platform_icon(ui, ptex, p).on_hover_text(p.label());
        }
    });
}

/// Distinct platforms among a channel's instances, in first-seen order.
pub(super) fn channel_platforms(monitors: &[&MonitorWithChannel]) -> Vec<Platform> {
    let mut out: Vec<Platform> = Vec::new();
    for m in monitors {
        let p = m.monitor.platform();
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Tooltip for a failed row: the most meaningful captured error line, plus a
/// likely-cause hint for recognizable failures (network/DNS).
///
/// Tools print `ERROR: <the actual problem>` followed by a Python traceback
/// whose frames (`File "...", line N, in ...`) say nothing useful — the old
/// "last non-empty line" rule surfaced exactly such a frame (rec 653's DNS
/// failure hovered as `File "yt_dlp\extractor\youtube\_tab.py", line 925…`).
pub(super) fn fail_hover(log: &str) -> String {
    let lines: Vec<&str> = log.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let reason = lines
        .iter()
        .rev()
        .find(|l| {
            let low = l.to_ascii_lowercase();
            low.starts_with("error") || low.contains(" error: ")
        })
        // No explicit error line: last line that isn't a traceback frame.
        .or_else(|| lines.iter().rev().find(|l| !l.starts_with("File \"")))
        .copied();
    let mut out = match reason {
        Some(r) => format!("Failed: {r}"),
        None => "Failed (no captured output).".to_string(),
    };
    if let Some(hint) = network_failure_hint(log) {
        out.push_str("\n\n🌐 ");
        out.push_str(hint);
    }
    out
}
/// Version-picker entry: capture timestamp, `(current)` on the newest.
pub(super) fn about_version_label(snaps: &[crate::store::AboutSnapshotRow], i: usize) -> String {
    let s = &snaps[i];
    format!(
        "{}{}",
        fmt_datetime_short(s.fetched_at),
        if i == 0 { "  (current)" } else { "" }
    )
}

/// Render one About panel image from disk, lazily decoded only when visible
/// (mirrors `show_post_image`, but against the viewer's own cache).
pub(super) fn show_about_image(ui: &mut egui::Ui, cache: &mut PostImageCache, hash: &str, path: &str) {
    const MAX_W: f32 = 500.0;
    const MAX_H: f32 = 400.0;
    const PLACEHOLDER_H: f32 = 120.0;
    let cached = cache.get(hash).cloned();
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
            if resp.on_hover_text("Alt: preview full size · click: open file").clicked() {
                crate::platform::open_path(std::path::Path::new(path));
            }
        }
        Some(None) => {
            // Decode failed / file gone (e.g. channel renamed) — quiet marker.
            ui.weak("(image unavailable)");
        }
        None => {
            let w = ui.available_width().min(MAX_W);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(w, PLACEHOLDER_H), egui::Sense::hover());
            if ui.is_rect_visible(rect) {
                let key = format!("about_{hash}");
                let loaded = load_image_texture(std::path::Path::new(path), ui.ctx(), &key);
                cache.insert(hash.to_string(), loaded);
                ui.ctx().request_repaint();
            }
        }
    }
}

// ── Properties window helpers ────────────────────────────────────────────────

/// Per-account channel asset directory:
/// `…/channel_assets/{name}/{platform}/{account}/`. Thin alias for the shared
/// definition in [`crate::assets::channel_asset_dir`].
pub(super) fn channel_asset_dir(name: &str, platform: Platform, account: &str) -> std::path::PathBuf {
    crate::assets::channel_asset_dir(name, platform, account)
}

/// The account slug of a monitor URL — the last asset-path segment.
pub(super) fn asset_account(url: &str, platform: Platform) -> String {
    crate::assets::account_slug(url, platform)
}

/// Stable per-platform identity of a monitor's source URL, used to detect whether
/// an import candidate is already added (matches [`ImportCandidate::identity`]).
pub(super) fn monitor_import_identity(url: &str) -> String {
    match Platform::detect(url) {
        Platform::Twitch => crate::detectors::twitch_login(url).unwrap_or_default().to_lowercase(),
        Platform::Kick => crate::detectors::kick_slug(url).unwrap_or_default().to_lowercase(),
        Platform::YouTube => yt_channel_id(url).unwrap_or_else(|| url.to_lowercase()),
        // No account parser — the whole URL is the identity.
        Platform::Nrk | Platform::Nebula | Platform::Generic => url.to_lowercase(),
    }
}

/// Extract a lowercased YouTube `UC…` channel id from a `/channel/UC…` URL.
pub(super) fn yt_channel_id(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    let idx = lower.find("/channel/")?;
    let rest = &lower[idx + "/channel/".len()..];
    let id = rest.split(['/', '?', '#']).next()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// Whether an import row matches the dialog's (already-lowercased) filter query.
pub(super) fn import_row_matches(row: &ImportRow, q: &str) -> bool {
    q.is_empty()
        || row.cand.name.to_lowercase().contains(q)
        || row.cand.identity.contains(q)
        || row.cand.id.to_lowercase().contains(q)
}
/// Decode encoded image `bytes` to RGBA8 under hard resource limits. A corrupt or
/// hostile image (absurd dimensions / a decompression bomb) would otherwise tie up the
/// UI thread for many seconds inside a synchronous decode — exactly the kind of single
/// blocking op that trips the freeze watchdog — or balloon into a multi-GB allocation
/// that, under the release `panic = "abort"` profile, kills the process outright.
///
/// Bounds: 16384×16384 px (the usual `GL_MAX_TEXTURE_SIZE`; anything larger could not be
/// uploaded as a texture anyway) and a 1 GiB peak decode allocation. A limit breach or a
/// decode error returns `None`, which every caller already treats as "no usable image".
pub(super) fn decode_rgba_bounded(bytes: &[u8]) -> Option<image::RgbaImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    // `Limits` is `#[non_exhaustive]`, so build via `default()` and set fields.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(1 << 30); // 1 GiB
    reader.limits(limits);
    Some(reader.decode().ok()?.to_rgba8())
}
/// Load a channel's mainpage image assets (the `icon.*` and `banner.*` of each
/// asset account — never emotes or badges) into textures for the Properties
/// thumbnail strip, in a stable account/asset order. Skips absent assets;
/// legacy pre-account dirs are consulted as a fallback per account.
pub(super) fn load_channel_asset_thumbs(
    channel: &Channel,
    accounts: &[AssetAccount],
    ctx: &egui::Context,
) -> Vec<AssetThumb> {
    let mut out: Vec<AssetThumb> = Vec::new();
    for acc in accounts {
        let dirs = crate::assets::asset_read_dirs(&channel.name, acc.platform, &acc.account);
        for (prefix, kind) in [("icon.", "icon"), ("banner.", "banner")] {
            let Some(path) = dirs.iter().find_map(|d| prop_find_first(d, prefix)) else {
                continue;
            };
            let key = format!(
                "thumb_{}_{}_{}_{kind}",
                channel.id,
                acc.platform.as_str(),
                acc.account
            );
            if let Some((tex, dims)) = load_image_texture(&path, ctx, &key) {
                out.push(AssetThumb {
                    label: format!("{} {kind}", acc.label),
                    platform: acc.platform,
                    account: acc.account.clone(),
                    kind,
                    path,
                    tex,
                    dims,
                });
            }
        }
    }
    out
}

/// Count the emotes per provider the viewer would list for a channel — its whole
/// *universe* of named entries (active + deprecated), derived from the very same
/// [`enumerate_provider_emotes`] the viewer enumerates. Using that one source means the
/// launcher button's count can never drift from what the viewer shows: an empty-code
/// manifest entry is excluded here exactly as the viewer excludes it, while a
/// missing/0-byte image still counts (it appears in the viewer's "Deprecated" section).
/// Counting the universe — not just the active ones — keeps the button (and thus the
/// Deprecated section) reachable even for a provider whose emotes were all removed
/// upstream. Order: Twitch, 7TV, BTTV, FFZ — the order the launcher buttons are drawn
/// in. This stats one file per emote, so callers cache the result rather than recompute
/// it per frame.
pub(super) fn emote_provider_counts(
    name: &str,
    accounts: &[AssetAccount],
) -> Vec<(AssetAccount, [(EmoteProvider, usize); 4])> {
    accounts
        .iter()
        .filter(|a| a.platform == Platform::Twitch) // emotes are a Twitch-side concept
        .map(|a| {
            let counts = [
                EmoteProvider::Twitch,
                EmoteProvider::SevenTv,
                EmoteProvider::Bttv,
                EmoteProvider::Ffz,
            ]
            .map(|p| (p, enumerate_provider_emotes(name, &a.account, p).len()));
            (a.clone(), counts)
        })
        .collect()
}

/// Enumerate a provider's emotes for a channel, resolved to on-disk image paths.
/// Third parties read their manifest and resolve into the per-channel/shared caches
/// exactly like [`build_emote_map`]; an entry whose image is gone is kept but marked
/// `exists = false` (the viewer lists those under "Deprecated"). Twitch is
/// directory-listed by opaque id (no codes, so never deprecated). Sorted by code.
pub(super) fn enumerate_provider_emotes(name: &str, account: &str, provider: EmoteProvider) -> Vec<ViewerEmote> {
    use crate::assets::{EmoteManifestEntry, sanitize_emote_name};
    let emotes_dir = twitch_emotes_dir(name, account);
    let plat = crate::app_paths::platform_assets_dir();

    // Resolve an emote's on-disk path: try the new `{id}_{name}.{ext}` pattern
    // first (written by updated fetchers), fall back to old `{id}.{ext}` for
    // files downloaded before this change.
    let resolve_path = |base: std::path::PathBuf, e: &EmoteManifestEntry| -> std::path::PathBuf {
        let new_name = format!("{}_{}.{}", e.id, sanitize_emote_name(&e.name), e.ext);
        let new_path = base.join(&new_name);
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &new_path) {
            new_path
        } else {
            base.join(format!("{}.{}", e.id, e.ext))
        }
    };

    // For Twitch: use the manifest when available (written after a refetch under
    // the new code). Fall back to directory listing for channels not yet refetched
    // so existing `{id}.{ext}` files still appear, shown with the numeric id as name.
    if provider == EmoteProvider::Twitch {
        let manifest_path = emotes_dir.join("twitch.json");
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &manifest_path) {
            let entries: Vec<EmoteManifestEntry> =
                crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, &manifest_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
            let twitch_dir = emotes_dir.join("twitch");
            let mut out: Vec<ViewerEmote> = entries
                .into_iter()
                .filter(|e| !e.name.trim().is_empty())
                .map(|e| {
                    let path = resolve_path(twitch_dir.clone(), &e);
                    let exists = crate::iomon::fs::metadata_sync(crate::iomon::Cat::AssetCache, &path).map(|m| m.len() > 0).unwrap_or(false);
                    ViewerEmote { name: e.name, id: e.id, ext: e.ext, path, exists }
                })
                .collect();
            out.sort_by_key(|e| e.name.to_lowercase());
            return out;
        }
        // No manifest yet — dir-list fallback (shows numeric id as name)
        let mut out: Vec<ViewerEmote> = crate::iomon::fs::read_dir_sync(crate::iomon::Cat::AssetCache, emotes_dir.join("twitch"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| crate::iomon::fs::is_file_sync(crate::iomon::Cat::AssetCache, p))
            .map(|p| {
                let stem = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let ext = p
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                // In the old layout the stem is the numeric id
                ViewerEmote { name: stem.clone(), id: stem, ext, path: p, exists: true }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        return out;
    }

    let Some(manifest_file) = provider.manifest() else { return Vec::new() };
    let entries: Vec<EmoteManifestEntry> =
        crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, emotes_dir.join(manifest_file))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

    let resolve = |e: &EmoteManifestEntry| -> std::path::PathBuf {
        match provider {
            EmoteProvider::SevenTv => {
                resolve_path(plat.join("7tv").join("emotes"), e)
            }
            EmoteProvider::Ffz => {
                resolve_path(plat.join("ffz").join("emotes"), e)
            }
            EmoteProvider::Bttv => {
                let base = if e.shared {
                    plat.join("bttv").join("emotes")
                } else {
                    emotes_dir.join("bttv")
                };
                resolve_path(base, e)
            }
            EmoteProvider::Twitch => unreachable!("Twitch handled above"),
        }
    };

    let mut out: Vec<ViewerEmote> = entries
        .into_iter()
        .filter(|e| !e.name.trim().is_empty())
        .map(|e| {
            let path = resolve(&e);
            // Mirror `assets::asset_present`: a 0-byte file is treated as absent.
            let exists = crate::iomon::fs::metadata_sync(crate::iomon::Cat::AssetCache, &path).map(|m| m.len() > 0).unwrap_or(false);
            ViewerEmote { name: e.name, id: e.id, ext: e.ext, path, exists }
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}
/// Which asset account an explicit icon-source preference selects: matching
/// platform, and — for a legacy bare-platform preference — that platform's
/// first account.
pub(super) fn preferred_account_index(
    pref: &Option<crate::models::PreferredAssetSource>,
    accounts: &[AssetAccount],
) -> Option<usize> {
    let p = pref.as_ref()?;
    accounts.iter().position(|a| {
        a.platform == p.platform && p.account.as_deref().is_none_or(|acc| acc == a.account)
    })
}

/// Resolve a container's avatar — the chosen account's profile pic. An explicit
/// `preferred_asset` wins when it matches one of the container's asset accounts
/// (showing a placeholder until that account's icon is fetched); otherwise auto:
/// the first account (first-seen order) whose icon loads, then the legacy
/// per-platform and flat dirs. `None` when nothing has a fetched icon yet.
pub(super) fn resolve_channel_icon(
    channel: &Channel,
    accounts: &[AssetAccount],
    ctx: &egui::Context,
) -> Option<egui::TextureHandle> {
    let key = channel.id.to_string();
    let load = |a: &AssetAccount| {
        crate::assets::asset_read_dirs(&channel.name, a.platform, &a.account)
            .iter()
            .find_map(|d| load_channel_icon(d, ctx, &key))
    };
    if let Some(i) = preferred_account_index(&channel.preferred_asset, accounts) {
        return load(&accounts[i]);
    }
    accounts.iter().find_map(load).or_else(|| {
        // Legacy fallback (auto mode only): assets fetched before per-platform
        // namespacing lived in the flat channel_assets/{name}/ dir. Show that
        // icon until a refetch repopulates the namespaced dirs, so an existing
        // container's avatar doesn't go blank on upgrade.
        let flat = crate::app_paths::asset_cache_dir()
            .join("channel_assets")
            .join(crate::downloader::sanitize_filename(&channel.name));
        load_channel_icon(&flat, ctx, &key)
    })
}

/// Count archived historical variants of a singular asset (`history/{stem}_*`).
/// These are the older profile pics / banners kept when the channel changed them.
pub(super) fn prop_variant_count(asset_dir: &std::path::Path, stem: &str) -> usize {
    let prefix = format!("{stem}_");
    crate::iomon::fs::read_dir_sync(crate::iomon::Cat::AssetCache, asset_dir.join("history"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                .count()
        })
        .unwrap_or(0)
}

/// One asset-status cell: `—` (absent), `✔` (present), or `✔ +N` when `N` older
/// versions are archived. Hovering a `+N` cell explains the kept history.
pub(super) fn asset_status_cell(ui: &mut egui::Ui, present: bool, variants: usize) {
    let text = if !present {
        "—".to_string()
    } else if variants > 0 {
        format!("✔ +{variants}")
    } else {
        "✔".to_string()
    };
    let resp = ui.label(text);
    if variants > 0 {
        resp.on_hover_text(format!(
            "{variants} older version(s) archived under history/ — kept, never overwritten."
        ));
    }
}

/// Short "last fetched" label for an asset dir's `.assets_fetched_at` stamp.
/// Returns `"never"` when the stamp is missing/unparseable.
pub(super) fn fmt_asset_stamp(asset_dir: &std::path::Path) -> String {
    crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, asset_dir.join(".assets_fetched_at"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "never".into())
}

/// Load a channel icon from `asset_dir/icon.*` into an egui texture.
/// Returns `None` when no icon file is found or decoding fails.
pub(super) fn load_channel_icon(
    asset_dir: &std::path::Path,
    ctx: &egui::Context,
    key: &str,
) -> Option<egui::TextureHandle> {
    let entry = prop_find_first(asset_dir, "icon.")?;
    let bytes = crate::iomon::fs::read_sync(crate::iomon::Cat::AssetCache, &entry).ok()?;
    let img = decode_rgba_bounded(&bytes)?;
    let size = [img.width() as usize, img.height() as usize];
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied(size, &img.into_raw());
    Some(ctx.load_texture(
        format!("chan_icon_{key}"),
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}

/// Like [`load_channel_icon`] but loads the pre-scaled 64 px thumbnail
/// (`icon_64.png`) generated by [`crate::assets::ensure_scaled_icon`].
/// Used for the small avatar slots in the streams table where uploading a full
/// 300 px source and letting the GPU scale it to 18 px gives poor quality.
pub(super) fn load_channel_icon_small(
    asset_dir: &std::path::Path,
    ctx: &egui::Context,
    key: &str,
) -> Option<egui::TextureHandle> {
    let path = crate::assets::ensure_scaled_icon(asset_dir, 64)?;
    let bytes = crate::iomon::fs::read_sync(crate::iomon::Cat::AssetCache, &path).ok()?;
    let img = decode_rgba_bounded(&bytes)?;
    let size = [img.width() as usize, img.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &img.into_raw());
    Some(ctx.load_texture(
        format!("chan_icon_small_{key}"),
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}

/// Like [`resolve_channel_icon`] but uses the 64 px pre-scaled thumbnail for
/// the streams-table avatar slot.
pub(super) fn resolve_channel_icon_small(
    channel: &Channel,
    accounts: &[AssetAccount],
    ctx: &egui::Context,
) -> Option<egui::TextureHandle> {
    let key = channel.id.to_string();
    let load = |a: &AssetAccount| {
        crate::assets::asset_read_dirs(&channel.name, a.platform, &a.account)
            .iter()
            .find_map(|d| load_channel_icon_small(d, ctx, &key))
    };
    if let Some(i) = preferred_account_index(&channel.preferred_asset, accounts) {
        return load(&accounts[i]);
    }
    accounts.iter().find_map(load).or_else(|| {
        let flat = crate::app_paths::asset_cache_dir()
            .join("channel_assets")
            .join(crate::downloader::sanitize_filename(&channel.name));
        load_channel_icon_small(&flat, ctx, &key)
    })
}

/// Small avatar for a streams-table instance row: the icon fetched into that
/// instance's own account dir (so GEEGA main and alt each show their own face).
/// Falls back to the legacy per-platform dir for pre-migration trees.
pub(super) fn resolve_instance_icon_small(
    row: &MonitorWithChannel,
    ctx: &egui::Context,
) -> Option<egui::TextureHandle> {
    let platform = row.monitor.platform();
    if !platform.has_asset_fetcher() {
        return None; // no asset fetcher for this platform — nothing on disk
    }
    let account = asset_account(&row.monitor.url, platform);
    let key = format!("m{}", row.monitor.id);
    crate::assets::asset_read_dirs(&row.channel.name, platform, &account)
        .iter()
        .find_map(|d| load_channel_icon_small(d, ctx, &key))
}

// ---------- Alt-hover full-resolution image preview ----------

/// When the user holds Alt while hovering `resp`, queue `tex` for a
/// full-resolution floating preview shown by [`draw_alt_image_preview`].
/// Call immediately after `ui.add(egui::Image::from_texture(...))`.
pub(super) fn queue_alt_image_preview(ctx: &egui::Context, resp: &egui::Response, tex: &egui::TextureHandle) {
    if resp.hovered() && ctx.input(|i| i.modifiers.alt) {
        // Wrapped in Option so remove_temp (which needs T: Default) can use None as sentinel.
        ctx.data_mut(|d| {
            d.insert_temp(egui::Id::new("alt_img_preview"), Some(tex.clone()))
        });
    }
}

/// Draw the Alt-hover image preview queued this frame by [`queue_alt_image_preview`].
/// Call once at the end of each viewport's rendering pass so it floats on top.
/// Destructively consumes the queued texture, so the overlay vanishes the frame
/// the user stops hovering or releases Alt — no explicit clear needed.
pub(super) fn draw_alt_image_preview(ctx: &egui::Context) {
    // remove_temp::<Option<T>> works because Option implements Default (= None).
    let Some(Some(tex)) = ctx.data_mut(|d| {
        d.remove_temp::<Option<egui::TextureHandle>>(egui::Id::new("alt_img_preview"))
    }) else {
        return;
    };
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
        return;
    };

    let tex_size = tex.size_vec2();
    let label_text = format!("{} × {} px", tex_size.x as u32, tex_size.y as u32);
    // Measure the size label so [`alt_preview_layout`] reasons about the
    // overlay's true footprint (image + frame margins + label). If the real
    // footprint were bigger than computed, egui's own constrain-to-screen
    // would shove the area back over the cursor, which must never happen
    // (see the flicker note on [`alt_preview_layout`]).
    let label_size = {
        let font = egui::TextStyle::Small.resolve(&ctx.global_style());
        ctx.fonts_mut(|f| {
            f.layout_no_wrap(label_text.clone(), font, egui::Color32::PLACEHOLDER)
                .size()
        })
    };
    let spacing = ctx.global_style().spacing.item_spacing.y;
    let Some((overlay, scale)) =
        alt_preview_layout(tex_size, label_size, spacing, pos, ctx.content_rect())
    else {
        // Viewport too small to show a preview anywhere beside the cursor.
        return;
    };
    let preview_size = tex_size * scale;

    egui::Area::new(egui::Id::new("alt_img_preview_area"))
        .fixed_pos(overlay.min)
        .order(egui::Order::Tooltip)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(ui.visuals().window_fill)
                .stroke(ui.visuals().window_stroke)
                .inner_margin(egui::Margin::same(6))
                .corner_radius(egui::CornerRadius::same(6))
                .show(ui, |ui| {
                    ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(preview_size));
                    ui.label(egui::RichText::new(label_text).small().weak());
                });
        });
}

/// Place the Alt-hover preview overlay: returns the overlay's screen rect and
/// the scale to draw the image at, or `None` if the viewport is too small to
/// show a preview at all.
///
/// The overlay must never end up under the cursor: despite
/// `interactable(false)`, egui's hover hit-test still treats the preview area
/// as the top-most widget at the pointer, un-hovering the thumbnail that
/// queued it — the preview then vanishes, the thumbnail re-hovers, and the
/// overlay flickers on/off every other frame (worst in short viewports, where
/// the old clamp-to-edge placement pushed it back over the cursor).
pub(super) fn alt_preview_layout(
    tex_size: egui::Vec2,
    label_size: egui::Vec2,
    spacing: f32,
    pos: egui::Pos2,
    screen: egui::Rect,
) -> Option<(egui::Rect, f32)> {
    const MARGIN: f32 = 6.0;
    const GAP: f32 = 14.0;
    let chrome = egui::vec2(
        2.0 * MARGIN + 2.0, // +2 slack for frame stroke/pixel rounding
        2.0 * MARGIN + spacing + label_size.y + 2.0,
    );
    let overlay_size = |scale: f32| {
        egui::vec2(
            (tex_size.x * scale).max(label_size.x) + chrome.x,
            tex_size.y * scale + chrome.y,
        )
    };
    // Largest image scale whose overlay fits `avail` (0 = doesn't fit at all).
    let fit = |avail: egui::Vec2| -> f32 {
        let w = avail.x - chrome.x;
        let h = avail.y - chrome.y;
        if w < label_size.x || h <= 0.0 {
            return 0.0;
        }
        (w / tex_size.x.max(1.0))
            .min(h / tex_size.y.max(1.0))
            .min(1.0)
    };

    let avail = screen.shrink(GAP);

    // Cap at 85% of the viewport so large banners don't overflow off-screen.
    let mut scale = fit(screen.size() * 0.85);

    // Prefer bottom-right of the cursor; flip to the other side when that
    // would go off-screen.
    let mut size = overlay_size(scale);
    let mut tl = pos + egui::vec2(GAP, GAP);
    if tl.x + size.x > avail.right() {
        tl.x = (pos.x - size.x - GAP).max(avail.left());
    }
    if tl.y + size.y > avail.bottom() {
        tl.y = (pos.y - size.y - GAP).max(avail.top());
    }

    // When neither side of the cursor has room at full size, shrink into
    // whichever side fits it best instead of covering the cursor.
    if egui::Rect::from_min_size(tl, size)
        .expand(GAP / 2.0)
        .contains(pos)
    {
        let regions = [
            egui::Rect::from_min_max(egui::pos2(pos.x + GAP, avail.top()), avail.right_bottom()),
            egui::Rect::from_min_max(avail.left_top(), egui::pos2(pos.x - GAP, avail.bottom())),
            egui::Rect::from_min_max(egui::pos2(avail.left(), pos.y + GAP), avail.right_bottom()),
            egui::Rect::from_min_max(avail.left_top(), egui::pos2(avail.right(), pos.y - GAP)),
        ];
        let (region, region_scale) = regions
            .iter()
            .map(|r| (*r, fit(r.size())))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .filter(|(_, s)| *s > 0.0)?;
        scale = scale.min(region_scale);
        size = overlay_size(scale);
        tl = pos + egui::vec2(GAP, GAP);
        tl.x = tl.x.min(region.right() - size.x).max(region.left());
        tl.y = tl.y.min(region.bottom() - size.y).max(region.top());
    }
    Some((egui::Rect::from_min_size(tl, size), scale))
}

/// Try to extract a YouTube UC… channel ID from a channel URL.
pub(super) fn extract_yt_channel_id(url: &str) -> Option<String> {
    // Matches "/channel/UCxxxxxxxxx" style URLs.
    let idx = url.find("/channel/")?;
    let after = &url[idx + "/channel/".len()..];
    let id: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect();
    if id.starts_with("UC") && id.len() > 10 {
        Some(id)
    } else {
        None
    }
}

/// Find the first entry in `dir` whose filename starts with `prefix`.
pub(super) fn prop_find_first(dir: &std::path::Path, prefix: &str) -> Option<std::path::PathBuf> {
    crate::iomon::fs::read_dir_sync(crate::iomon::Cat::AssetCache, dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
}

/// Count files (non-recursive) in `dir`.
pub(super) fn prop_count_dir_files(dir: &std::path::Path) -> usize {
    crate::iomon::fs::read_dir_sync(crate::iomon::Cat::AssetCache, dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| crate::iomon::fs::is_file_sync(crate::iomon::Cat::AssetCache, e.path()))
        .count()
}

/// Count directories at exactly `depth` levels below `root`.
pub(super) fn prop_count_nested_dirs(root: &std::path::Path, depth: usize) -> usize {
    if depth == 0 || !crate::iomon::fs::is_dir_sync(crate::iomon::Cat::AssetCache, root) {
        return 0;
    }
    crate::iomon::fs::read_dir_sync(crate::iomon::Cat::AssetCache, root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            if depth == 1 {
                if crate::iomon::fs::is_dir_sync(crate::iomon::Cat::AssetCache, e.path()) { 1 } else { 0 }
            } else {
                prop_count_nested_dirs(&e.path(), depth - 1)
            }
        })
        .sum()
}

/// "yes" / "no" display for boolean fields.
pub(super) fn prop_bool(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

/// Truncate a long path string keeping its tail (for compact display).
pub(super) fn prop_truncate_path(p: &str, max_chars: usize) -> String {
    if p.len() <= max_chars {
        p.to_string()
    } else {
        format!("…{}", &p[p.len() - max_chars..])
    }
}

/// Read the length of a JSON-array manifest file (BTTV/FFZ/7TV emote manifests).
/// Returns 0 if the file is absent or unparseable.
pub(super) fn prop_read_manifest_count(path: &std::path::Path) -> usize {
    crate::iomon::fs::read_to_string_sync(crate::iomon::Cat::AssetCache, path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_array().map(|a| a.len()))
        .unwrap_or(0)
}


#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use std::path::PathBuf;

    /// The rec-653 hover: the real `ERROR:` line must win over the traceback
    /// frames that follow it, and a DNS failure gets the likely-cause hint.
    #[test]
    fn fail_hover_prefers_error_line_over_traceback_frames() {
        let log = "\
WARNING: [youtube:tab] Failed to resolve 'www.youtube.com' ([Errno 11001] getaddrinfo failed). Retrying (3/3)...
ERROR: [youtube:tab] @AnyaNyabyss: Playlists that require authentication may not extract correctly without a successful webpage download.
  File \"yt_dlp\\extractor\\common.py\", line 763, in extract
  File \"yt_dlp\\extractor\\youtube\\_tab.py\", line 925, in _report_playlist_authcheck";
        let hover = fail_hover(log);
        assert!(hover.starts_with("Failed: ERROR: [youtube:tab]"), "{hover}");
        assert!(!hover.contains("line 925"), "traceback frame leaked: {hover}");
        assert!(hover.contains("🌐"), "DNS hint missing: {hover}");
        // No error line at all → last non-frame line, no hint.
        let plain = fail_hover("something odd\n  File \"x.py\", line 1, in y");
        assert_eq!(plain, "Failed: something odd");
    }

    #[test]
    fn alt_preview_big_viewport_shows_full_size_beside_cursor() {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(2560.0, 1400.0));
        let pos = egui::pos2(400.0, 400.0);
        let (rect, scale) = alt_preview_layout(
            egui::vec2(320.0, 180.0),
            egui::vec2(80.0, 12.0),
            4.0,
            pos,
            screen,
        )
        .expect("plenty of room");
        assert_eq!(scale, 1.0, "small image should not be downscaled");
        assert_eq!(rect.min, pos + egui::vec2(14.0, 14.0), "bottom-right of cursor");
    }

    #[test]
    fn alt_preview_never_covers_the_cursor_even_in_short_viewports() {
        // Regression: on a viewport too short to hold the preview beside the
        // cursor, the old placement clamped it back over the pointer. The
        // preview's own (non-interactable) Area still wins egui's hover
        // hit-test, so covering the pointer un-hovers the thumbnail that
        // queued it — flickering the overlay on/off every other frame. The
        // overlay must therefore keep clear of the pointer at EVERY cursor
        // position, shrinking if that's the only way to fit.
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 400.0));
        let tex = egui::vec2(1920.0, 1080.0); // full-res banner, far bigger than the window
        for xi in 0..30 {
            for yi in 0..14 {
                let pos = egui::pos2(15.0 + xi as f32 * 30.0, 15.0 + yi as f32 * 27.0);
                let (rect, scale) =
                    alt_preview_layout(tex, egui::vec2(80.0, 12.0), 4.0, pos, screen)
                        .unwrap_or_else(|| panic!("no layout at {pos:?}"));
                assert!(scale > 0.0, "image visible at {pos:?}");
                assert!(
                    rect.distance_sq_to_pos(pos) >= 7.0 * 7.0,
                    "overlay {rect:?} too close to cursor {pos:?} (would steal hover)"
                );
            }
        }
    }

    #[test]
    fn alt_preview_gives_up_when_viewport_cannot_fit_one_beside_the_cursor() {
        // Absurdly small window: better to show nothing than a flickering
        // overlay glued to the pointer.
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(60.0, 40.0));
        let out = alt_preview_layout(
            egui::vec2(1920.0, 1080.0),
            egui::vec2(80.0, 12.0),
            4.0,
            egui::pos2(30.0, 20.0),
            screen,
        );
        assert!(out.is_none());
    }
    #[test]
    fn import_identity_matches_candidate_identity() {
        // Dedup keys must equal what the importer produces (lowercased login / UC id).
        assert_eq!(monitor_import_identity("https://twitch.tv/CoolStreamer"), "coolstreamer");
        assert_eq!(
            monitor_import_identity("https://www.youtube.com/channel/UCabcDEF"),
            "ucabcdef"
        );
        assert_eq!(
            yt_channel_id("https://youtube.com/channel/UCxyz/live"),
            Some("ucxyz".to_string())
        );
        // A handle URL has no /channel/UC… id → None (won't false-match a sub).
        assert_eq!(yt_channel_id("https://youtube.com/@handle"), None);
    }
}
