//! Debug-build isolation of the operator's real config tree.
//!
//! The helpers are always compiled, because the binary crate built as a test
//! harness (`cargo bench`, `cargo test --release`) reaches this module through
//! the library, which is built without `cfg(test)` there. Every caller stays
//! gated under `cfg(any(test, debug_assertions))`, so a release build never
//! runs any of it. Unit tests, integration tests, `cargo run` and a binary a
//! test spawns are all debug builds and share these guards.

use std::path::{Path, PathBuf};

/// Whether the `RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1` escape hatch is set.
/// Every debug-build isolation guard in the crate (the config-resolver guard,
/// `Config::save`'s write-site guard, the legacy-layout-migration guard and the
/// root redirect in [`redirected_root`]) treats this the same way: a deliberate,
/// explicit opt-in for the one test or run that must exercise real-path
/// resolution.
pub fn allow_real_config_dir() -> bool {
    std::env::var_os("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR").as_deref()
        == Some(std::ffi::OsStr::new("1"))
}

/// Resolve `path` for a temp-dir comparison: canonicalize the nearest
/// existing ancestor and rejoin the rest, so a symlinked temp dir (`/tmp` ->
/// `/private/tmp` on macOS) still compares equal even though `path` itself
/// may not exist yet (e.g. `config.toml` before its first save).
fn resolve_for_temp_dir_check(path: &Path) -> PathBuf {
    let mut suffix = PathBuf::new();
    let mut ancestor = path;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            return if suffix.as_os_str().is_empty() {
                canonical
            } else {
                canonical.join(suffix)
            };
        }
        let Some(file_name) = ancestor.file_name() else {
            return path.to_path_buf();
        };
        suffix = PathBuf::from(file_name).join(suffix);
        let Some(parent) = ancestor.parent() else {
            return path.to_path_buf();
        };
        ancestor = parent;
    }
}

/// Whether `path` is under `std::env::temp_dir()`, canonicalizing both sides
/// so a symlinked temp dir doesn't produce a false negative. Shared by every
/// debug-build write-site / `HOME`-derived isolation guard in the crate.
pub fn is_under_temp_dir(path: &Path) -> bool {
    let temp_dir = std::env::temp_dir();
    let resolved_path = resolve_for_temp_dir_check(path);
    let resolved_temp_dir = resolve_for_temp_dir_check(&temp_dir);
    resolved_path.starts_with(&temp_dir) || resolved_path.starts_with(&resolved_temp_dir)
}

/// The root a debug build uses instead of `home/.rantaiclaw`, or `None` when
/// `home` is the real answer: the opt-out is set, or `home` is already under the
/// temp dir.
///
/// The root is `<temp>/rantaiclaw-dev-home-<pid>/.rantaiclaw`, one per process,
/// so it keeps the `.rantaiclaw` name the layout code expects. It is created
/// (owner-only) on first use. Outside `cfg(test)` the first redirect prints one
/// line to stderr, so a developer running `cargo run` knows where their data
/// went.
pub fn redirected_root(home: &Path) -> Option<PathBuf> {
    if allow_real_config_dir() || is_under_temp_dir(home) {
        return None;
    }
    let root = std::env::temp_dir()
        .join(format!("rantaiclaw-dev-home-{}", std::process::id()))
        .join(".rantaiclaw");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if let Err(err) = builder.create(&root) {
        eprintln!(
            "rantaiclaw: could not create the debug-build data root {}: {err}",
            root.display()
        );
    }
    #[cfg(not(test))]
    {
        static NOTICE: std::sync::Once = std::sync::Once::new();
        NOTICE.call_once(|| {
            eprintln!(
                "rantaiclaw: debug build, using {} instead of the real config tree. \
                 Set RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1 to use the real one.",
                root.display()
            );
        });
    }
    Some(root)
}
