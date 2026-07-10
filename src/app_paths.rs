//! Filesystem locations for config, database, and default recording output.

use std::path::PathBuf;

use directories::{ProjectDirs, UserDirs};

/// `create_dir_all` with manual I/O-monitor accounting instead of the
/// `iomon::fs` facade: `iomon::classify` resolves the data dir through these
/// very helpers, so routing them through the facade (which classifies) would
/// re-enter that lazy initialization.
#[allow(clippy::disallowed_methods)]
fn ensure_dir(dir: &std::path::Path) {
    let start = std::time::Instant::now();
    let _ = std::fs::create_dir_all(dir);
    crate::iomon::record_region(
        crate::iomon::Cat::Startup,
        crate::iomon::Region::AppData,
        crate::iomon::OpKind::Create,
        0,
        start.elapsed(),
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
