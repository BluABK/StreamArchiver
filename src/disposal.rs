//! How automatic media deletions are executed — **trash folder / Recycle Bin /
//! permanent** — plus the opt-in post-join parts cleanup ("after `full.mkv`
//! lands, delete the now-redundant head and/or live capture"). Both settings
//! use the same three-level inheritance chain as
//! [`crate::head_backfill::HeadBackfillScope`] (monitor override → channel
//! override → global default), stored as JSON scope-maps in `app_settings`
//! (no schema migration).
//!
//! Only deletions of **finished recording media** route through here (the
//! post-join cleanup, superseded-head removal, the live capture consumed by a
//! "replace with VOD" swap). Transient working files — playlists, `.state`,
//! cache leftovers — are junk, not media, and keep using plain deletes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::iomon::Cat;
use crate::store::Store;

// ---------- settings keys ----------

/// Global default: how automatic media deletions are executed.
pub const K_DISPOSAL_METHOD: &str = "disposal_method";
/// `;`-separated trash folder list, one per drive (same-drive moves only —
/// mirrors `capture_cache_root`'s multi-root convention). Takes precedence
/// over [`K_TRASH_DEFAULT_ROOT`] for any drive it explicitly lists.
pub const K_TRASH_DIRS: &str = "disposal_trash_dirs";
/// A `{drive}`-templated fallback trash root (e.g. `{drive}:\streams\.sa-trash`)
/// applied to any drive [`K_TRASH_DIRS`] doesn't explicitly cover — so a new
/// drive gets a trash folder automatically instead of falling back to the
/// Recycle Bin until the user adds an explicit entry for it. Empty = no
/// default (the pre-existing behavior: unlisted drives fall back to Recycle).
pub const K_TRASH_DEFAULT_ROOT: &str = "disposal_trash_default_root";
/// Global default: what happens to the head/live parts once `full.mkv` lands.
pub const K_JOIN_CLEANUP: &str = "join_cleanup";
pub const K_GAP_SPLICE_CLEANUP: &str = "gap_splice_cleanup";
/// Per-channel scope-config map (`{channel_id -> DisposalScope}`).
pub const K_CHANNEL_DISPOSAL_SCOPE: &str = "channel_disposal_scope";
/// Per-monitor scope-config map (`{monitor_id -> DisposalScope}`).
pub const K_MONITOR_DISPOSAL_SCOPE: &str = "monitor_disposal_scope";

// ---------- the two settings ----------

/// Where an automatic media deletion sends the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisposalMethod {
    /// Move into a configured trash folder on the same drive (instant rename,
    /// user prunes manually).
    Trash,
    /// Send to the OS Recycle Bin. The safe default: needs no configuration
    /// and survives a mis-fire. NB: on drives without a Recycle Bin
    /// (some removable media) Windows deletes permanently instead.
    #[default]
    Recycle,
    /// Delete permanently.
    Delete,
    /// The method used is genuinely unknown — only produced by the
    /// historical-import scan (`disposal_backfill`) for a disposal that
    /// predates the Trash view, where nothing records which of the three
    /// real methods above was actually configured at the time. Never
    /// returned by `effective_method`/produced by a live `dispose_media`
    /// call, and deliberately excluded from `ALL` so it can't appear as a
    /// choice in Settings.
    Unknown,
}

impl DisposalMethod {
    pub const ALL: [DisposalMethod; 3] =
        [DisposalMethod::Trash, DisposalMethod::Recycle, DisposalMethod::Delete];
    pub fn as_str(self) -> &'static str {
        match self {
            DisposalMethod::Trash => "trash",
            DisposalMethod::Recycle => "recycle",
            DisposalMethod::Delete => "delete",
            DisposalMethod::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Option<DisposalMethod> {
        match s.trim() {
            "trash" => Some(DisposalMethod::Trash),
            "recycle" => Some(DisposalMethod::Recycle),
            "delete" => Some(DisposalMethod::Delete),
            "unknown" => Some(DisposalMethod::Unknown),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            DisposalMethod::Trash => "Trash folder",
            DisposalMethod::Recycle => "Recycle Bin",
            DisposalMethod::Delete => "Delete permanently",
            DisposalMethod::Unknown => "Unknown (imported)",
        }
    }
}

/// What happens to the parts once a verified `full.mkv` join lands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinCleanup {
    /// Keep head + live capture alongside the full (the historical behavior —
    /// costs double the stream's size).
    #[default]
    Keep,
    /// Dispose of the head; keep the live capture as the take's main file.
    Head,
    /// Dispose of head AND live capture; the take's main file becomes the
    /// full (its `output_path` is re-pointed).
    Both,
}

impl JoinCleanup {
    pub const ALL: [JoinCleanup; 3] = [JoinCleanup::Keep, JoinCleanup::Head, JoinCleanup::Both];
    pub fn as_str(self) -> &'static str {
        match self {
            JoinCleanup::Keep => "keep",
            JoinCleanup::Head => "head",
            JoinCleanup::Both => "both",
        }
    }
    pub fn parse(s: &str) -> Option<JoinCleanup> {
        match s.trim() {
            "keep" => Some(JoinCleanup::Keep),
            "head" => Some(JoinCleanup::Head),
            "both" => Some(JoinCleanup::Both),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            JoinCleanup::Keep => "Keep parts",
            JoinCleanup::Head => "Delete head",
            JoinCleanup::Both => "Delete head + capture",
        }
    }
}

/// What happens to the pre-splice original + consumed gap patches once a
/// verified gapless splice lands — same 3-tier shape as [`JoinCleanup`],
/// named for this context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GapSpliceCleanup {
    /// Keep the pre-splice original + patch files alongside the gapless
    /// result (nothing auto-deleted — the default).
    #[default]
    Keep,
    /// Dispose of the consumed patch files; keep the pre-splice original.
    Patches,
    /// Dispose of patch files AND the pre-splice original; the take's main
    /// file becomes the gapless splice (its `output_path` is re-pointed).
    Both,
}

impl GapSpliceCleanup {
    pub const ALL: [GapSpliceCleanup; 3] =
        [GapSpliceCleanup::Keep, GapSpliceCleanup::Patches, GapSpliceCleanup::Both];
    pub fn as_str(self) -> &'static str {
        match self {
            GapSpliceCleanup::Keep => "keep",
            GapSpliceCleanup::Patches => "patches",
            GapSpliceCleanup::Both => "both",
        }
    }
    pub fn parse(s: &str) -> Option<GapSpliceCleanup> {
        match s.trim() {
            "keep" => Some(GapSpliceCleanup::Keep),
            "patches" => Some(GapSpliceCleanup::Patches),
            "both" => Some(GapSpliceCleanup::Both),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            GapSpliceCleanup::Keep => "Keep original + patches",
            GapSpliceCleanup::Patches => "Delete consumed patches",
            GapSpliceCleanup::Both => "Delete patches + pre-splice original",
        }
    }
}

// ---------- three-level scope config (clone of HeadBackfillScope) ----------

/// A channel- or monitor-level override. `None` on a field means "inherit the
/// level above"; `Some(v)` forces it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisposalScope {
    #[serde(default)]
    pub method: Option<DisposalMethod>,
    #[serde(default)]
    pub join_cleanup: Option<JoinCleanup>,
    #[serde(default)]
    pub gap_splice_cleanup: Option<GapSpliceCleanup>,
}

impl DisposalScope {
    /// True when this scope overrides nothing — persisted as a removal so the
    /// map only holds real overrides.
    pub fn is_inherit(&self) -> bool {
        self.method.is_none() && self.join_cleanup.is_none() && self.gap_splice_cleanup.is_none()
    }
}

fn load_scope_map(store: &Store, key: &str) -> HashMap<String, DisposalScope> {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_scope(store: &Store, key: &str, id: i64, cfg: &DisposalScope) -> anyhow::Result<()> {
    let mut map = load_scope_map(store, key);
    if cfg.is_inherit() {
        map.remove(&id.to_string());
    } else {
        map.insert(id.to_string(), cfg.clone());
    }
    store.set_setting(key, &serde_json::to_string(&map)?)?;
    Ok(())
}

pub fn load_channel_disposal_scope(store: &Store, channel_id: i64) -> DisposalScope {
    load_scope_map(store, K_CHANNEL_DISPOSAL_SCOPE)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

pub fn save_channel_disposal_scope(
    store: &Store,
    channel_id: i64,
    cfg: &DisposalScope,
) -> anyhow::Result<()> {
    save_scope(store, K_CHANNEL_DISPOSAL_SCOPE, channel_id, cfg)
}

pub fn load_monitor_disposal_scope(store: &Store, monitor_id: i64) -> DisposalScope {
    load_scope_map(store, K_MONITOR_DISPOSAL_SCOPE)
        .remove(&monitor_id.to_string())
        .unwrap_or_default()
}

pub fn save_monitor_disposal_scope(
    store: &Store,
    monitor_id: i64,
    cfg: &DisposalScope,
) -> anyhow::Result<()> {
    save_scope(store, K_MONITOR_DISPOSAL_SCOPE, monitor_id, cfg)
}

// ---------- global readers + effective resolution ----------

pub fn global_method(store: &Store) -> DisposalMethod {
    store
        .get_setting(K_DISPOSAL_METHOD)
        .ok()
        .flatten()
        .and_then(|s| DisposalMethod::parse(&s))
        .unwrap_or_default()
}

pub fn global_join_cleanup(store: &Store) -> JoinCleanup {
    store
        .get_setting(K_JOIN_CLEANUP)
        .ok()
        .flatten()
        .and_then(|s| JoinCleanup::parse(&s))
        .unwrap_or_default()
}

pub fn global_gap_splice_cleanup(store: &Store) -> GapSpliceCleanup {
    store
        .get_setting(K_GAP_SPLICE_CLEANUP)
        .ok()
        .flatten()
        .and_then(|s| GapSpliceCleanup::parse(&s))
        .unwrap_or_default()
}

/// Monitor override over channel override over the global default.
pub fn effective_method_from(
    global: DisposalMethod,
    channel_scope: Option<&DisposalScope>,
    monitor_scope: Option<&DisposalScope>,
) -> DisposalMethod {
    monitor_scope
        .and_then(|s| s.method)
        .or_else(|| channel_scope.and_then(|s| s.method))
        .unwrap_or(global)
}

pub fn effective_join_cleanup_from(
    global: JoinCleanup,
    channel_scope: Option<&DisposalScope>,
    monitor_scope: Option<&DisposalScope>,
) -> JoinCleanup {
    monitor_scope
        .and_then(|s| s.join_cleanup)
        .or_else(|| channel_scope.and_then(|s| s.join_cleanup))
        .unwrap_or(global)
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_method(store: &Store, channel_id: i64, monitor_id: i64) -> DisposalMethod {
    let ch = load_channel_disposal_scope(store, channel_id);
    let mon = load_monitor_disposal_scope(store, monitor_id);
    effective_method_from(global_method(store), Some(&ch), Some(&mon))
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_join_cleanup(store: &Store, channel_id: i64, monitor_id: i64) -> JoinCleanup {
    let ch = load_channel_disposal_scope(store, channel_id);
    let mon = load_monitor_disposal_scope(store, monitor_id);
    effective_join_cleanup_from(global_join_cleanup(store), Some(&ch), Some(&mon))
}

pub fn effective_gap_splice_cleanup_from(
    global: GapSpliceCleanup,
    channel_scope: Option<&DisposalScope>,
    monitor_scope: Option<&DisposalScope>,
) -> GapSpliceCleanup {
    monitor_scope
        .and_then(|s| s.gap_splice_cleanup)
        .or_else(|| channel_scope.and_then(|s| s.gap_splice_cleanup))
        .unwrap_or(global)
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_gap_splice_cleanup(
    store: &Store,
    channel_id: i64,
    monitor_id: i64,
) -> GapSpliceCleanup {
    let ch = load_channel_disposal_scope(store, channel_id);
    let mon = load_monitor_disposal_scope(store, monitor_id);
    effective_gap_splice_cleanup_from(global_gap_splice_cleanup(store), Some(&ch), Some(&mon))
}

// ---------- disposal history (the Trash view) ----------

/// Current status of one logged disposal — drives the Trash view's actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisposalRecordState {
    /// A Trash-method disposal whose file still sits in its trash folder —
    /// the only state with real actions: restore, or permanently delete.
    SoftDeleted,
    /// Terminal: a Recycle/Delete-method disposal, or a soft-deleted row the
    /// user chose to permanently delete. Recycle Bin recovery is Windows'
    /// own job, not tracked or actioned here.
    Permanent,
    /// A soft-deleted row the user moved back to `original_path`.
    Restored,
}

impl DisposalRecordState {
    pub fn as_str(self) -> &'static str {
        match self {
            DisposalRecordState::SoftDeleted => "soft_deleted",
            DisposalRecordState::Permanent => "permanent",
            DisposalRecordState::Restored => "restored",
        }
    }
    pub fn parse(s: &str) -> Option<DisposalRecordState> {
        match s.trim() {
            "soft_deleted" => Some(DisposalRecordState::SoftDeleted),
            "permanent" => Some(DisposalRecordState::Permanent),
            "restored" => Some(DisposalRecordState::Restored),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            DisposalRecordState::SoftDeleted => "In trash",
            DisposalRecordState::Permanent => "Permanently deleted",
            DisposalRecordState::Restored => "Restored",
        }
    }
}

/// How a disposal record came to exist — lets the Trash view tell a
/// real-time-logged entry apart from a best-effort entry reconstructed after
/// the fact (see `disposal_backfill`) for disposals that predate this
/// feature, where the exact method/timestamp/path can't be known for sure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisposalConfidence {
    /// Logged by `log_disposal` at the moment `dispose_media` ran it — the
    /// method, path, and timestamp are all exactly what happened.
    Live,
    /// Reconstructed from a DB column that still held the exact original
    /// path (or a deterministic transform of one, following the same naming
    /// rule the app itself used to create the file) — verified absent from
    /// disk before import, but the method and timestamp are unknown/proxied.
    HistoricalExact,
    /// Reconstructed from a filename NAMING CONVENTION rather than a column
    /// that ever literally held this path — same absence verification, but
    /// the path itself is an educated guess, not a read-back value.
    HistoricalGuess,
}

impl DisposalConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            DisposalConfidence::Live => "live",
            DisposalConfidence::HistoricalExact => "historical_exact",
            DisposalConfidence::HistoricalGuess => "historical_guess",
        }
    }
    pub fn parse(s: &str) -> Option<DisposalConfidence> {
        match s.trim() {
            "live" => Some(DisposalConfidence::Live),
            "historical_exact" => Some(DisposalConfidence::HistoricalExact),
            "historical_guess" => Some(DisposalConfidence::HistoricalGuess),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            DisposalConfidence::Live => "Live",
            DisposalConfidence::HistoricalExact => "Historical (exact path)",
            DisposalConfidence::HistoricalGuess => "Historical (inferred path)",
        }
    }
}

/// One logged disposal event, as stored in / read from `disposal_record`
/// (schema v73/v74). `id` is `0` for a row not yet inserted.
#[derive(Clone, Debug)]
pub struct DisposalRecordRow {
    pub id: i64,
    /// The recording/take this file belonged to.
    pub rec_id: i64,
    /// Short human reason, e.g. "post-join cleanup: head" or "superseded old
    /// head" — free text, not an enum, since the call sites' phrasing already
    /// varies naturally and a closed set would just be re-parsed back to text
    /// for display anyway.
    pub reason: String,
    pub method: DisposalMethod,
    pub original_path: String,
    /// Where the file currently lives when `state == SoftDeleted` (empty for
    /// Recycle/Delete rows, which never had a trash-folder stop).
    pub trash_path: String,
    pub state: DisposalRecordState,
    pub disposed_at: i64,
    pub updated_at: i64,
    pub confidence: DisposalConfidence,
}

/// Record a completed disposal for the Trash view. Best-effort: a logging
/// failure must never undo or block the disposal itself, so callers only
/// warn on error. Always `DisposalConfidence::Live` — this runs at the exact
/// moment `dispose_media` acted; see `disposal_backfill` for the historical-
/// import path that produces the other two confidence levels.
pub fn log_disposal(store: &Store, rec_id: i64, reason: &str, original_path: &Path, disposed: &Disposed) {
    let now = crate::models::now_unix();
    let (method, trash_path, state) = match disposed {
        Disposed::Trashed(p) => {
            (DisposalMethod::Trash, p.to_string_lossy().into_owned(), DisposalRecordState::SoftDeleted)
        }
        Disposed::Recycled => (DisposalMethod::Recycle, String::new(), DisposalRecordState::Permanent),
        Disposed::Deleted => (DisposalMethod::Delete, String::new(), DisposalRecordState::Permanent),
    };
    let row = DisposalRecordRow {
        id: 0,
        rec_id,
        reason: reason.to_string(),
        method,
        original_path: original_path.to_string_lossy().into_owned(),
        trash_path,
        state,
        disposed_at: now,
        updated_at: now,
        confidence: DisposalConfidence::Live,
    };
    if let Err(e) = store.insert_disposal_record(&row) {
        warn!("disposal: failed to log history row for {}: {e:#}", original_path.display());
    }
}

/// Move a soft-deleted file back to its original path — the Trash view's
/// "Restore" action. Only valid while the file is still sitting in the trash
/// folder (`state == SoftDeleted`); fails closed on anything else so a
/// double-click can't clobber an already-restored/deleted row.
pub async fn restore_disposal_record(store: &Store, id: i64) -> anyhow::Result<()> {
    let row = store
        .get_disposal_record(id)?
        .ok_or_else(|| anyhow::anyhow!("no such disposal record"))?;
    if row.state != DisposalRecordState::SoftDeleted {
        anyhow::bail!("not in trash (already {})", row.state.label().to_lowercase());
    }
    let from = Path::new(&row.trash_path);
    let to = Path::new(&row.original_path);
    if let Some(parent) = to.parent() {
        crate::iomon::fs::create_dir_all(Cat::CacheSweep, parent).await?;
    }
    crate::iomon::fs::rename(Cat::CacheSweep, from, to).await?;
    store.set_disposal_record_state(
        id,
        DisposalRecordState::Restored,
        Some(""),
        crate::models::now_unix(),
    )?;
    Ok(())
}

/// Delete a soft-deleted file for good — the Trash view's "Permanently
/// delete" action. Only valid for a `SoftDeleted` row (same fail-closed
/// reasoning as [`restore_disposal_record`]).
pub async fn permanently_delete_disposal_record(store: &Store, id: i64) -> anyhow::Result<()> {
    let row = store
        .get_disposal_record(id)?
        .ok_or_else(|| anyhow::anyhow!("no such disposal record"))?;
    if row.state != DisposalRecordState::SoftDeleted {
        anyhow::bail!("not in trash (already {})", row.state.label().to_lowercase());
    }
    crate::iomon::fs::remove_file(Cat::CacheSweep, Path::new(&row.trash_path)).await?;
    store.set_disposal_record_state(id, DisposalRecordState::Permanent, None, crate::models::now_unix())?;
    Ok(())
}

// ---------- executing a disposal ----------

/// What actually happened to the file (for logs / task notes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposed {
    Trashed(PathBuf),
    Recycled,
    Deleted,
}

impl Disposed {
    /// Short human phrase for task notes: "moved to trash" / "recycled" / "deleted".
    pub fn describe(&self) -> &'static str {
        match self {
            Disposed::Trashed(_) => "moved to trash",
            Disposed::Recycled => "sent to Recycle Bin",
            Disposed::Deleted => "deleted",
        }
    }
}

/// The configured trash root on the same drive as `path`, if any. Cross-drive
/// moves are never attempted (a multi-GB "delete" must not become a copy) —
/// no same-drive root and no usable default template means the caller falls
/// back to the Recycle Bin. `dirs` (explicit per-drive overrides) wins over
/// `default_template` (the `{drive}`-templated fallback) when both apply.
pub fn pick_trash_root(dirs: &str, default_template: &str, path: &Path) -> Option<PathBuf> {
    let drive = crate::downloader::drive_of(path)?;
    let explicit = dirs
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .find(|r| crate::downloader::drive_of(r) == Some(drive));
    explicit.or_else(|| expand_trash_default(default_template, drive))
}

/// Expands a `{drive}`-templated default trash root for one drive letter,
/// e.g. `expand_trash_default("{drive}:\\streams\\.sa-trash", 'A')` →
/// `A:\streams\.sa-trash`. Returns `None` for a blank template (the "no
/// default configured" case).
pub fn expand_trash_default(template: &str, drive: char) -> Option<PathBuf> {
    let template = template.trim();
    if template.is_empty() {
        return None;
    }
    Some(PathBuf::from(template.replace("{drive}", &drive.to_string())))
}

/// A non-clobbering target name inside the trash dir: `name`, else
/// `stem (n).ext` for the first free `n`. `exists` is injected for testability.
pub fn unique_trash_target(dir: &Path, name: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !exists(&first) {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s, Some(e)),
        None => (name, None),
    };
    for n in 1u32.. {
        let candidate = match ext {
            Some(e) => dir.join(format!("{stem} ({n}).{e}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !exists(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 exhausted finding a free trash name");
}

/// The default trash root offered when the user picks the Trash method with
/// nothing configured — one folder per drive, alongside the capture cache, so
/// a trashed file is always an instant same-drive rename.
pub const TRASH_ROOT_SUGGESTION: &str = r"{drive}:\streams\.sa-trash";

/// True when "Trash folder" is selected but **nothing says where the trash
/// folder is** — no per-drive entry and no `{drive}` default template.
///
/// This combination is not an error, but it is a trap: `execute_disposal`
/// quietly degrades to the Recycle Bin, which on a recordings drive frees
/// *nothing* until the bin is emptied by hand. It read as "deletions are
/// configured" while 133 GB accumulated in `G:\$RECYCLE.BIN` (2026-07-31).
/// Settings uses this to refuse to leave the pair blank silently.
///
/// Takes the raw field values rather than the store so the Settings form can
/// call it on unsaved edits.
pub fn trash_root_missing(method: DisposalMethod, trash_dirs: &str, default_root: &str) -> bool {
    method == DisposalMethod::Trash
        && trash_dirs.trim().is_empty()
        && default_root.trim().is_empty()
}

/// Dispose of a finished-media file per the effective (instance > channel >
/// global) method. On failure the file is left in place and an error returned —
/// disposal never escalates (a failed trash move or recycle NEVER falls
/// through to a permanent delete; trash does fall back to the Recycle Bin when
/// no same-drive trash root is configured, which is *less* destructive).
/// `rec_id`/`reason` are only for the Trash view's history log (`log_disposal`)
/// — they don't affect what gets done, only how it's later explained.
pub async fn dispose_media(
    store: &Store,
    channel_id: i64,
    monitor_id: i64,
    path: &Path,
    rec_id: i64,
    reason: &str,
) -> std::io::Result<Disposed> {
    let result = execute_disposal(store, channel_id, monitor_id, path).await;
    if let Ok(disposed) = &result {
        log_disposal(store, rec_id, reason, path, disposed);
    }
    result
}

/// The disposal itself, split out from [`dispose_media`] purely so the history
/// log above it is **unbypassable**. It used to be one function whose tail did
/// the logging, and one `return` in the middle — the "no trash folder on this
/// drive, degrade to the Recycle Bin" path — jumped straight over it. Every
/// disposal on a Trash-method setup with no configured root was therefore
/// executed but never recorded: `disposal_record` stayed empty, the Trash view
/// showed nothing to restore or prune, and 133 GB of "trashed" captures piled
/// up invisibly in the Recycle Bin (found 2026-07-31). With the log in the
/// caller, no early return in here can lose an entry again.
async fn execute_disposal(
    store: &Store,
    channel_id: i64,
    monitor_id: i64,
    path: &Path,
) -> std::io::Result<Disposed> {
    let method = effective_method(store, channel_id, monitor_id);
    match method {
        DisposalMethod::Trash => {
            let dirs = store.get_setting(K_TRASH_DIRS).ok().flatten().unwrap_or_default();
            let default_root =
                store.get_setting(K_TRASH_DEFAULT_ROOT).ok().flatten().unwrap_or_default();
            let Some(root) = pick_trash_root(&dirs, &default_root, path) else {
                warn!(
                    "disposal: no trash folder configured on {}'s drive — sending to the Recycle Bin instead",
                    path.display()
                );
                return recycle(path).await;
            };
            crate::iomon::fs::create_dir_all(Cat::CacheSweep, &root).await?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| std::io::Error::other("path has no file name"))?;
            let target = unique_trash_target(&root, &name, |p| {
                crate::iomon::fs::exists_sync(Cat::CacheSweep, p)
            });
            match crate::iomon::fs::rename(Cat::CacheSweep, path, &target).await {
                Ok(()) => Ok(Disposed::Trashed(target)),
                Err(e) => {
                    // Same-drive renames shouldn't fail; whatever this is
                    // (locked file, exotic path), degrade to the less
                    // destructive option rather than giving up entirely.
                    warn!(
                        "disposal: trash move of {} failed ({e:#}) — sending to the Recycle Bin instead",
                        path.display()
                    );
                    recycle(path).await
                }
            }
        }
        DisposalMethod::Recycle => recycle(path).await,
        DisposalMethod::Delete => {
            crate::iomon::fs::remove_file(Cat::CacheSweep, path).await?;
            Ok(Disposed::Deleted)
        }
        DisposalMethod::Unknown => {
            unreachable!("Unknown is only ever constructed by disposal_backfill, never resolved by effective_method")
        }
    }
}

async fn recycle(path: &Path) -> std::io::Result<Disposed> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::platform::recycle_path(&p))
        .await
        .map_err(std::io::Error::other)??;
    Ok(Disposed::Recycled)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures, not app I/O paths
    use super::*;

    /// Every successful disposal lands a history row, whichever branch of
    /// `execute_disposal` produced it. The regression this guards: the
    /// no-trash-root Recycle fallback used to `return` past the logging, so a
    /// Trash-method setup with no configured root disposed hundreds of GB
    /// without a single `disposal_record` row (see `execute_disposal`).
    #[tokio::test]
    async fn every_successful_disposal_is_logged() {
        let dir = std::env::temp_dir().join(format!("sa-disp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1. Trash WITH a configured root: soft-deleted, restorable.
        let store = Store::open_in_memory().unwrap();
        let trash = dir.join("trash");
        store.set_setting(K_DISPOSAL_METHOD, "trash").unwrap();
        store.set_setting(K_TRASH_DIRS, &trash.to_string_lossy()).unwrap();
        let victim = dir.join("a.mkv");
        std::fs::write(&victim, b"x").unwrap();
        let d = dispose_media(&store, 1, 1, &victim, 42, "post-join cleanup: head").await.unwrap();
        assert!(matches!(d, Disposed::Trashed(_)), "{d:?}");
        let rows = store.list_disposal_records().unwrap();
        assert_eq!(rows.len(), 1, "the trash move must be logged");
        assert_eq!(rows[0].row.rec_id, 42);
        assert_eq!(rows[0].row.method, DisposalMethod::Trash);
        assert_eq!(rows[0].row.state, DisposalRecordState::SoftDeleted);
        assert_eq!(rows[0].row.reason, "post-join cleanup: head");
        assert!(!rows[0].row.trash_path.is_empty(), "restore needs the new path");
        assert!(!victim.exists() && std::fs::read_dir(&trash).unwrap().count() == 1);

        // 2. Permanent delete: a different branch, still logged.
        let store = Store::open_in_memory().unwrap();
        store.set_setting(K_DISPOSAL_METHOD, "delete").unwrap();
        let victim = dir.join("b.mkv");
        std::fs::write(&victim, b"x").unwrap();
        let d = dispose_media(&store, 1, 1, &victim, 7, "post-join cleanup: live capture").await.unwrap();
        assert!(matches!(d, Disposed::Deleted));
        let rows = store.list_disposal_records().unwrap();
        assert_eq!(rows.len(), 1, "the permanent delete must be logged");
        assert_eq!(rows[0].row.state, DisposalRecordState::Permanent);
        assert!(!victim.exists());

        // 3. A FAILED disposal logs nothing (the file is still there — a row
        //    would offer a restore for something that was never removed).
        let store = Store::open_in_memory().unwrap();
        store.set_setting(K_DISPOSAL_METHOD, "delete").unwrap();
        assert!(dispose_media(&store, 1, 1, &dir.join("nope.mkv"), 9, "x").await.is_err());
        assert!(store.list_disposal_records().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact condition that silently routes a "Trash" disposal to the
    /// Recycle Bin: no explicit entry for the file's drive AND no `{drive}`
    /// default template. Settings warns about this loudly — see
    /// `trash_root_missing`.
    #[test]
    fn no_trash_root_configured_is_what_triggers_the_recycle_fallback() {
        let p = Path::new(r"G:\streams\x.mkv");
        assert!(pick_trash_root("", "", p).is_none(), "blank config = fallback");
        assert!(pick_trash_root(r"A:\streams\.sa-trash", "", p).is_none(), "other drive only");
        assert!(pick_trash_root(r"G:\streams\.sa-trash", "", p).is_some());
        assert!(pick_trash_root("", r"{drive}:\streams\.sa-trash", p).is_some(), "template covers it");
    }

    #[test]
    fn enum_strings_roundtrip() {
        for m in DisposalMethod::ALL {
            assert_eq!(DisposalMethod::parse(m.as_str()), Some(m));
        }
        for c in JoinCleanup::ALL {
            assert_eq!(JoinCleanup::parse(c.as_str()), Some(c));
        }
        assert_eq!(DisposalMethod::parse("bogus"), None);
        assert_eq!(JoinCleanup::parse(""), None);
        // Scope JSON keeps the lowercase strings (what the settings blob stores).
        let s = DisposalScope {
            method: Some(DisposalMethod::Trash),
            join_cleanup: Some(JoinCleanup::Both),
            gap_splice_cleanup: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"trash\"") && json.contains("\"both\""), "{json}");
        assert_eq!(serde_json::from_str::<DisposalScope>(&json).unwrap(), s);
        assert!(serde_json::from_str::<DisposalScope>("{}").unwrap().is_inherit());
    }

    #[test]
    fn precedence_monitor_over_channel_over_global() {
        let ch = DisposalScope {
            method: Some(DisposalMethod::Delete),
            join_cleanup: None,
            gap_splice_cleanup: None,
        };
        let mon = DisposalScope {
            method: Some(DisposalMethod::Trash),
            join_cleanup: Some(JoinCleanup::Head),
            gap_splice_cleanup: None,
        };
        assert_eq!(
            effective_method_from(DisposalMethod::Recycle, Some(&ch), Some(&mon)),
            DisposalMethod::Trash
        );
        // Channel wins when the monitor inherits that field.
        assert_eq!(
            effective_method_from(DisposalMethod::Recycle, Some(&ch), Some(&DisposalScope::default())),
            DisposalMethod::Delete
        );
        // Global when both inherit; fields resolve independently.
        assert_eq!(
            effective_join_cleanup_from(JoinCleanup::Keep, Some(&ch), None),
            JoinCleanup::Keep
        );
        assert_eq!(
            effective_join_cleanup_from(JoinCleanup::Keep, Some(&ch), Some(&mon)),
            JoinCleanup::Head
        );
    }

    #[test]
    fn defaults_are_safe_when_unset() {
        let store = Store::open_in_memory().unwrap();
        // Opt-in cleanup: default keeps parts; deletes default to the Recycle Bin.
        assert_eq!(global_join_cleanup(&store), JoinCleanup::Keep);
        assert_eq!(global_method(&store), DisposalMethod::Recycle);
        assert_eq!(effective_join_cleanup(&store, 1, 1), JoinCleanup::Keep);
        store.set_setting(K_JOIN_CLEANUP, "both").unwrap();
        assert_eq!(effective_join_cleanup(&store, 1, 1), JoinCleanup::Both);
        save_monitor_disposal_scope(
            &store,
            1,
            &DisposalScope { method: None, join_cleanup: Some(JoinCleanup::Keep), gap_splice_cleanup: None },
        )
        .unwrap();
        assert_eq!(effective_join_cleanup(&store, 1, 1), JoinCleanup::Keep);
        assert_eq!(effective_method(&store, 1, 1), DisposalMethod::Recycle);
    }

    #[test]
    fn trash_root_same_drive_only() {
        use std::path::Path;
        let dirs = r"A:\streams\.sa-trash; G:\vods\.sa-trash";
        assert_eq!(
            pick_trash_root(dirs, "", Path::new(r"A:\streams\Ch\x.head.mkv")),
            Some(PathBuf::from(r"A:\streams\.sa-trash"))
        );
        assert_eq!(
            pick_trash_root(dirs, "", Path::new(r"g:\vods\Ch\x.mkv")),
            Some(PathBuf::from(r"G:\vods\.sa-trash"))
        );
        // No root on that drive and no default → None (falls back to Recycle Bin).
        assert_eq!(pick_trash_root(dirs, "", Path::new(r"D:\other\x.mkv")), None);
        assert_eq!(pick_trash_root("", "", Path::new(r"A:\x.mkv")), None);
    }

    #[test]
    fn trash_root_default_template_fills_unlisted_drives() {
        use std::path::Path;
        let dirs = r"A:\streams\.sa-trash"; // explicit override for A: only
        let default = r"{drive}:\streams\.sa-trash";
        // A: keeps its explicit override, not the templated default.
        assert_eq!(
            pick_trash_root(dirs, default, Path::new(r"A:\streams\Ch\x.mkv")),
            Some(PathBuf::from(r"A:\streams\.sa-trash"))
        );
        // D: has no explicit entry → falls through to the expanded template.
        assert_eq!(
            pick_trash_root(dirs, default, Path::new(r"D:\vods\Ch\x.mkv")),
            Some(PathBuf::from(r"D:\streams\.sa-trash"))
        );
        // Blank default template → still None for an unlisted drive.
        assert_eq!(pick_trash_root(dirs, "", Path::new(r"D:\vods\Ch\x.mkv")), None);
    }

    #[test]
    fn expand_trash_default_substitutes_drive_letter_and_handles_blank() {
        assert_eq!(
            expand_trash_default(r"{drive}:\streams\.sa-trash", 'a'),
            Some(PathBuf::from(r"a:\streams\.sa-trash"))
        );
        assert_eq!(expand_trash_default("  ", 'A'), None);
        assert_eq!(expand_trash_default("", 'A'), None);
        // No token at all — used verbatim (edge case, documented as such).
        assert_eq!(expand_trash_default(r"D:\fixed-trash", 'A'), Some(PathBuf::from(r"D:\fixed-trash")));
    }

    #[test]
    fn unique_trash_target_dedupes() {
        let dir = Path::new(r"A:\t");
        // Free name used as-is.
        assert_eq!(
            unique_trash_target(dir, "x.full.mkv", |_| false),
            dir.join("x.full.mkv")
        );
        // Collision → " (n)" before the (last) extension.
        let taken = [dir.join("x.full.mkv"), dir.join("x.full (1).mkv")];
        assert_eq!(
            unique_trash_target(dir, "x.full.mkv", |p| taken.contains(&p.to_path_buf())),
            dir.join("x.full (2).mkv")
        );
        // Extensionless names still dedupe.
        assert_eq!(
            unique_trash_target(dir, "noext", |p| p == dir.join("noext")),
            dir.join("noext (1)")
        );
    }
}
