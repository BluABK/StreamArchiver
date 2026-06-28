//! Filesystem locations for config, database, and default recording output.

use std::path::PathBuf;

use directories::{ProjectDirs, UserDirs};

/// Resolve (and create) the application data directory, e.g.
/// `%APPDATA%\StreamArchiver\data` on Windows.
pub fn data_dir() -> PathBuf {
    let dir = ProjectDirs::from("", "", "StreamArchiver")
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./streamarchiver-data"));
    let _ = std::fs::create_dir_all(&dir);
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

/// Root of the centralised asset cache (alongside the DB).
/// e.g. `%APPDATA%\StreamArchiver\asset-cache\` on Windows.
pub fn asset_cache_dir() -> PathBuf {
    let dir = data_dir().join("asset-cache");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Platform-wide shared asset cache for deduplicated emote images and global badges.
/// e.g. `%APPDATA%\StreamArchiver\asset-cache\platform_assets\`.
pub fn platform_assets_dir() -> PathBuf {
    let dir = asset_cache_dir().join("platform_assets");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Directory for rotating log files, e.g. `%APPDATA%\StreamArchiver\data\logs\`.
pub fn logs_dir() -> PathBuf {
    let dir = data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}
