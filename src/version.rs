//! Build identity, baked in at compile time by [`build.rs`].
//!
//! [`build_id`] is stamped into every detached download's registry row
//! (`detached_process.spawn_build`) so a later launch knows exactly which build
//! started an in-flight download and can apply per-build compat fixups when it
//! re-attaches (see [`crate::compat`]). The local patch counter in `APP_VERSION`
//! increments on every rebuild, so this distinguishes even successive dev builds
//! of the same commit.

/// A stable identifier for the running build, e.g. `0.1.42/a1b2c3d4e-dirty`.
/// `APP_VERSION` is `<major>.<minor>.<local-build-counter>`; `GIT_HASH` is the
/// short commit hash with a `-dirty` suffix when tracked files were modified.
pub fn build_id() -> &'static str {
    concat!(env!("APP_VERSION"), "/", env!("GIT_HASH"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn build_id_is_version_slash_git() {
        let id = super::build_id();
        let (version, git) = id
            .split_once('/')
            .unwrap_or_else(|| panic!("build_id should be <version>/<git>, got {id:?}"));
        // Version is dotted (major.minor.patch); both parts are non-empty.
        assert!(version.contains('.'), "version part: {version:?}");
        assert!(!git.is_empty(), "git part empty in {id:?}");
    }
}
