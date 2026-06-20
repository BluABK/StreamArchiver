//! Build metadata embedded into the binary via `cargo:rustc-env`, so the exact
//! running build is identifiable in-app (window title / header / tray):
//! - `GIT_HASH`   short commit hash, with a `-dirty` suffix when the working
//!                tree had uncommitted changes at build time.
//! - `BUILD_NUMBER` total commit count (a monotonically increasing build no.).
//! - `BUILD_UNIX` compile time (unix seconds), shown so "latest build" is
//!                obvious even between commits.
//!
//! No `rerun-if-*` filter is emitted, so Cargo re-runs this whenever package
//! files change — i.e. on every real rebuild — keeping the timestamp fresh.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let hash = git(&["rev-parse", "--short=9", "HEAD"]).unwrap_or_else(|| "nogit".to_string());
    // Dirty = uncommitted changes to *tracked* files; ignore untracked files
    // (editor configs, build output, etc.) which don't change the built code.
    let dirty = match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(s) if !s.trim().is_empty() => "-dirty",
        _ => "",
    };
    let count = git(&["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "0".to_string());
    let built = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!("cargo:rustc-env=GIT_HASH={hash}{dirty}");
    println!("cargo:rustc-env=BUILD_NUMBER={count}");
    println!("cargo:rustc-env=BUILD_UNIX={built}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}
