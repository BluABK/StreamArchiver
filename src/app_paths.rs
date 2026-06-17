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
pub fn db_path() -> PathBuf {
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
