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
/// mirrors `capture_cache_root`'s multi-root convention).
pub const K_TRASH_DIRS: &str = "disposal_trash_dirs";
/// Global default: what happens to the head/live parts once `full.mkv` lands.
pub const K_JOIN_CLEANUP: &str = "join_cleanup";
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
}

impl DisposalMethod {
    pub const ALL: [DisposalMethod; 3] =
        [DisposalMethod::Trash, DisposalMethod::Recycle, DisposalMethod::Delete];
    pub fn as_str(self) -> &'static str {
        match self {
            DisposalMethod::Trash => "trash",
            DisposalMethod::Recycle => "recycle",
            DisposalMethod::Delete => "delete",
        }
    }
    pub fn parse(s: &str) -> Option<DisposalMethod> {
        match s.trim() {
            "trash" => Some(DisposalMethod::Trash),
            "recycle" => Some(DisposalMethod::Recycle),
            "delete" => Some(DisposalMethod::Delete),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            DisposalMethod::Trash => "Trash folder",
            DisposalMethod::Recycle => "Recycle Bin",
            DisposalMethod::Delete => "Delete permanently",
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

// ---------- three-level scope config (clone of HeadBackfillScope) ----------

/// A channel- or monitor-level override. `None` on a field means "inherit the
/// level above"; `Some(v)` forces it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisposalScope {
    #[serde(default)]
    pub method: Option<DisposalMethod>,
    #[serde(default)]
    pub join_cleanup: Option<JoinCleanup>,
}

impl DisposalScope {
    /// True when this scope overrides nothing — persisted as a removal so the
    /// map only holds real overrides.
    pub fn is_inherit(&self) -> bool {
        self.method.is_none() && self.join_cleanup.is_none()
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
/// no same-drive root means the caller falls back to the Recycle Bin.
pub fn pick_trash_root(dirs: &str, path: &Path) -> Option<PathBuf> {
    let drive = crate::downloader::drive_of(path)?;
    dirs.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .find(|r| crate::downloader::drive_of(r) == Some(drive))
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

/// Dispose of a finished-media file per the effective (instance > channel >
/// global) method. On failure the file is left in place and an error returned —
/// disposal never escalates (a failed trash move or recycle NEVER falls
/// through to a permanent delete; trash does fall back to the Recycle Bin when
/// no same-drive trash root is configured, which is *less* destructive).
pub async fn dispose_media(
    store: &Store,
    channel_id: i64,
    monitor_id: i64,
    path: &Path,
) -> std::io::Result<Disposed> {
    let method = effective_method(store, channel_id, monitor_id);
    match method {
        DisposalMethod::Trash => {
            let dirs = store.get_setting(K_TRASH_DIRS).ok().flatten().unwrap_or_default();
            let Some(root) = pick_trash_root(&dirs, path) else {
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
    use super::*;

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
        let s = DisposalScope { method: Some(DisposalMethod::Trash), join_cleanup: Some(JoinCleanup::Both) };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"trash\"") && json.contains("\"both\""), "{json}");
        assert_eq!(serde_json::from_str::<DisposalScope>(&json).unwrap(), s);
        assert!(serde_json::from_str::<DisposalScope>("{}").unwrap().is_inherit());
    }

    #[test]
    fn precedence_monitor_over_channel_over_global() {
        let ch = DisposalScope { method: Some(DisposalMethod::Delete), join_cleanup: None };
        let mon = DisposalScope { method: Some(DisposalMethod::Trash), join_cleanup: Some(JoinCleanup::Head) };
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
            &DisposalScope { method: None, join_cleanup: Some(JoinCleanup::Keep) },
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
            pick_trash_root(dirs, Path::new(r"A:\streams\Ch\x.head.mkv")),
            Some(PathBuf::from(r"A:\streams\.sa-trash"))
        );
        assert_eq!(
            pick_trash_root(dirs, Path::new(r"g:\vods\Ch\x.mkv")),
            Some(PathBuf::from(r"G:\vods\.sa-trash"))
        );
        // No root on that drive → None (caller falls back to the Recycle Bin).
        assert_eq!(pick_trash_root(dirs, Path::new(r"D:\other\x.mkv")), None);
        assert_eq!(pick_trash_root("", Path::new(r"A:\x.mkv")), None);
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
