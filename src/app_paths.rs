//! Filesystem locations for config, database, and default recording output.

use std::path::PathBuf;

use directories::{ProjectDirs, UserDirs};

/// `create_dir_all` once per distinct path per session. These helpers are
/// called from hot paths (asset-path construction runs tens of times a
/// second while rendering), and unconditionally re-creating the same four
/// base dirs was ~30 create syscalls/second forever — invisible until the
/// I/O monitor counted ~11k of them in minutes. Deleting a base dir
/// mid-session now requires a restart to recreate it, which is fine: the
/// DB/logs live there, so that state is already unrecoverable in-session.
///
/// Manual I/O-monitor accounting instead of the `iomon::fs` facade:
/// `iomon::classify` resolves the data dir through these very helpers, so
/// routing them through the facade (which classifies) would re-enter that
/// lazy initialization.
#[allow(clippy::disallowed_methods)]
fn ensure_dir(dir: &std::path::Path) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static CREATED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    if CREATED
        .lock()
        .unwrap()
        .get_or_insert_with(HashSet::new)
        .contains(dir)
    {
        return;
    }
    let start = std::time::Instant::now();
    if std::fs::create_dir_all(dir).is_ok() {
        CREATED
            .lock()
            .unwrap()
            .get_or_insert_with(HashSet::new)
            .insert(dir.to_path_buf());
    }
    crate::iomon::record_region(
        crate::iomon::Cat::Startup,
        crate::iomon::Region::AppData,
        crate::iomon::OpKind::Create,
        0,
        start.elapsed(),
        true,
    );
}

/// Resolve (and create) the application data directory, e.g.
/// `%APPDATA%\StreamArchiver\data` on Windows.
pub fn data_dir() -> PathBuf {
    let dir = ProjectDirs::from("", "", "StreamArchiver")
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./streamarchiver-data"));
    ensure_dir(&dir);
    dir
}

/// Path to the SQLite database. Kept on local disk (WAL requires it).
///
/// The `STREAMARCHIVER_DB` environment variable overrides the location (useful
/// for tests and portable installs).
pub fn db_path() -> PathBuf {
    if let Some(p) = std::env::var_os("STREAMARCHIVER_DB") {
        return PathBuf::from(p);
    }
    data_dir().join("streamarchiver.sqlite3")
}

/// Default per-channel output directory: the user's Videos folder if available,
/// otherwise a `recordings` folder under the app data dir.
pub fn default_output_dir() -> PathBuf {
    let dir = UserDirs::new()
        .and_then(|u| u.video_dir().map(|p| p.join("StreamArchiver")))
        .unwrap_or_else(|| data_dir().join("recordings"));
    dir
}

/// Default output directory for on-demand **video downloads** (Videos tab /
/// Recover VOD) — distinct from [`default_output_dir`] (live stream
/// recordings): a `Downloads` subfolder alongside it, so the two are never
/// silently the same folder even before either setting is configured.
pub fn default_video_output_dir() -> PathBuf {
    default_output_dir().join("Downloads")
}

/// Root of the centralised asset cache (alongside the DB, under the data dir).
/// e.g. `%APPDATA%\StreamArchiver\data\asset-cache\` on Windows.
pub fn asset_cache_dir() -> PathBuf {
    let dir = data_dir().join("asset-cache");
    ensure_dir(&dir);
    dir
}

/// Platform-wide shared asset cache for deduplicated emote images and global badges.
/// e.g. `%APPDATA%\StreamArchiver\data\asset-cache\platform_assets\`.
pub fn platform_assets_dir() -> PathBuf {
    let dir = asset_cache_dir().join("platform_assets");
    ensure_dir(&dir);
    dir
}

/// Directory for rotating log files, e.g. `%APPDATA%\StreamArchiver\data\logs\`.
pub fn logs_dir() -> PathBuf {
    let dir = data_dir().join("logs");
    ensure_dir(&dir);
    dir
}
