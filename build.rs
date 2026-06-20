//! Build metadata embedded into the binary via `cargo:rustc-env`, so the exact
//! running build is identifiable in-app (window title / header / tray):
//! - `GIT_HASH`     short commit hash, with a `-dirty` suffix when the working
//!                  tree had uncommitted changes at build time.
//! - `BUILD_NUMBER` total commit count (a monotonically increasing build no.).
//! - `BUILD_UNIX`   compile time (unix seconds), shown so "latest build" is
//!                  obvious even between commits.
//! - `APP_VERSION`  the displayed version `<major>.<minor>.<patch>`. Major/minor
//!                  come from Cargo.toml; the patch (`z`) is a LOCAL build counter
//!                  (file `.build-counter`, git-ignored) bumped on every build.
//!                  When the counter file is absent (fresh checkout) it's seeded
//!                  from Cargo.toml's patch, so the displayed patch starts at the
//!                  manifest version and only ever grows from there. Delete the
//!                  file to reseed (e.g. after bumping major/minor in Cargo.toml).
//!
//! No `rerun-if-*` filter is emitted, so Cargo re-runs this whenever package
//! files change — i.e. on every real rebuild — keeping the counter/timestamp
//! fresh.

use std::path::PathBuf;
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

    let patch = next_build_patch();
    let major = env_or("CARGO_PKG_VERSION_MAJOR", "0");
    let minor = env_or("CARGO_PKG_VERSION_MINOR", "0");

    // Force a re-run on every build: we depend on `.build-counter`, which this
    // script rewrites each run, so Cargo always sees it as changed and re-runs
    // (keeping the counter, git hash, and timestamp fresh on every build).
    println!("cargo:rerun-if-changed=.build-counter");

    println!("cargo:rustc-env=GIT_HASH={hash}{dirty}");
    println!("cargo:rustc-env=BUILD_NUMBER={count}");
    println!("cargo:rustc-env=BUILD_UNIX={built}");
    println!("cargo:rustc-env=APP_VERSION={major}.{minor}.{patch}");
}

/// The patch (`z`) for this build: the local `.build-counter` value + 1, or — when
/// the file is absent — Cargo.toml's patch used as-is (the seed build). The new
/// value is written back so the next build increments again.
fn next_build_patch() -> u64 {
    let cargo_patch: u64 = env_or("CARGO_PKG_VERSION_PATCH", "0").parse().unwrap_or(0);
    let path = PathBuf::from(env_or("CARGO_MANIFEST_DIR", ".")).join(".build-counter");
    let patch = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        // Increment on every build...
        Some(prev) => prev.saturating_add(1),
        // ...except the seed build, which starts at the manifest's patch.
        None => cargo_patch,
    };
    let _ = std::fs::write(&path, patch.to_string());
    patch
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}
