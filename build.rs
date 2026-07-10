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

// Build scripts run on the host, outside the app's I/O-monitor facade
// (clippy.toml disallowed-methods targets the app's runtime fs use).
#![allow(clippy::disallowed_methods)]

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

    decode_platform_icons();
    decode_provider_logos();

    // Embed a Windows application manifest requesting Common Controls v6 (required
    // for TaskDialogIndirect), DPI awareness, and visual styles. Without this,
    // Windows loads the old comctl32 v5 which lacks TaskDialogIndirect and the
    // process never reaches main() — an invisible "Entry Point Not Found" crash.
    embed_manifest::embed_manifest(embed_manifest::new_manifest("streamarchiver"))
        .expect("embed application manifest");

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

/// Decode each platform favicon (`assets/platform/<name>.png`) to a uniform
/// 32×32 RGBA buffer written to `OUT_DIR/platform_<name>.rgba`, which the app
/// embeds with `include_bytes!`. Decoding happens at build time so the `image`
/// crate never ships in the binary.
fn decode_platform_icons() {
    const SIZE: u32 = 32;
    let out = PathBuf::from(env_or("OUT_DIR", "."));
    for name in ["twitch", "youtube", "kick"] {
        let src = format!("assets/platform/{name}.png");
        println!("cargo:rerun-if-changed={src}");
        let img = image::open(&src)
            .unwrap_or_else(|e| panic!("decode {src}: {e}"))
            .resize_exact(SIZE, SIZE, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        std::fs::write(out.join(format!("platform_{name}.rgba")), img.as_raw())
            .unwrap_or_else(|e| panic!("write platform_{name}.rgba: {e}"));
    }
}

/// Rasterize the third-party emote-provider brand logos (`assets/emote/<name>.svg`)
/// to a uniform 64×64 straight-alpha RGBA buffer written to `OUT_DIR/logo_<name>.rgba`,
/// which the app embeds with `include_bytes!`. The logo is aspect-fit and centered in
/// the square canvas. Rasterizing at build time keeps the SVG stack (resvg/usvg/
/// tiny-skia) out of the shipped binary, mirroring `decode_platform_icons`.
fn decode_provider_logos() {
    use resvg::{tiny_skia, usvg};
    // Must match `LOGO_SRC` in src/ui.rs: it reads these buffers as `SIZE×SIZE` RGBA
    // and `ColorImage::from_rgba_unmultiplied` panics at startup if the dims disagree.
    const SIZE: u32 = 64;
    let out = PathBuf::from(env_or("OUT_DIR", "."));
    for name in ["7tv", "bttv"] {
        let src = format!("assets/emote/{name}.svg");
        println!("cargo:rerun-if-changed={src}");
        let svg = std::fs::read_to_string(&src).unwrap_or_else(|e| panic!("read {src}: {e}"));
        let tree = usvg::Tree::from_str(&svg, &usvg::Options::default())
            .unwrap_or_else(|e| panic!("parse {src}: {e}"));
        let ts = tree.size();
        // Aspect-fit the SVG's natural size into the square canvas, centered.
        let scale = (SIZE as f32 / ts.width()).min(SIZE as f32 / ts.height());
        let tx = (SIZE as f32 - ts.width() * scale) / 2.0;
        let ty = (SIZE as f32 - ts.height() * scale) / 2.0;
        let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);
        let mut pixmap =
            tiny_skia::Pixmap::new(SIZE, SIZE).unwrap_or_else(|| panic!("alloc pixmap for {src}"));
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        // tiny-skia stores premultiplied alpha; egui's `from_rgba_unmultiplied`
        // expects straight alpha, so demultiply each pixel.
        let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
        for px in pixmap.pixels() {
            let c = px.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        std::fs::write(out.join(format!("logo_{name}.rgba")), &rgba)
            .unwrap_or_else(|e| panic!("write logo_{name}.rgba: {e}"));
    }
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
