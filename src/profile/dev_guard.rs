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
///
/// A path containing `..` is never under the temp dir: the walk in
/// [`resolve_for_temp_dir_check`] cannot resolve `..` above a component that
/// does not exist, so the answer would be a lexical guess.
pub fn is_under_temp_dir(path: &Path) -> bool {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let temp_dir = std::env::temp_dir();
    let resolved_path = resolve_for_temp_dir_check(path);
    let resolved_temp_dir = resolve_for_temp_dir_check(&temp_dir);
    resolved_path.starts_with(&temp_dir) || resolved_path.starts_with(&resolved_temp_dir)
}

const DEV_HOME_PREFIX: &str = "rantaiclaw-dev-home-";

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// Whether `meta` belongs to `uid`. Without a uid concept the answer is yes.
#[cfg(unix)]
fn is_owned_by(meta: &std::fs::Metadata, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.uid() == uid
}

#[cfg(not(unix))]
fn is_owned_by(_meta: &std::fs::Metadata, _uid: u32) -> bool {
    true
}

/// Whether a process with this pid is running. A permission error still means
/// it exists. Non-unix builds never prune, so they report every pid as running.
#[cfg(unix)]
fn pid_is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    // `kill` with a pid of 0 or below signals a process group, never do that.
    if pid <= 0 {
        return true;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn pid_is_running(_pid: u32) -> bool {
    true
}

/// Create one new owner-only directory. Fails when the path already exists.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Whether `path` is a real directory (not a symlink) owned by `uid`.
fn is_real_dir_owned_by(path: &Path, uid: u32) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_dir() && is_owned_by(&meta, uid))
        .unwrap_or(false)
}

/// The private directory that holds this process's redirected root.
///
/// `<base>/rantaiclaw-dev-home-<pid>` is created owner-only. When that name is
/// already taken, it is reused only if it is a real directory owned by `uid`.
/// Anything else (a symlink, another user's directory) is not trusted, and a
/// fresh `<base>/rantaiclaw-dev-home-<pid>-<random>` directory is used instead.
fn prepare_dev_home(base: &Path, pid: u32, uid: u32) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(base)?;
    let preferred = base.join(format!("{DEV_HOME_PREFIX}{pid}"));
    match create_private_dir(&preferred) {
        Ok(()) => return Ok(preferred),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            if is_real_dir_owned_by(&preferred, uid) {
                return Ok(preferred);
            }
        }
        Err(err) => return Err(err),
    }
    let fallback = base.join(format!(
        "{DEV_HOME_PREFIX}{pid}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    create_private_dir(&fallback)?;
    Ok(fallback)
}

/// Remove `<base>/<prefix><pid>` and `<base>/<prefix><pid>-<random>`
/// directories left by processes that are no longer running.
///
/// Only real directories owned by the current user are removed, never a
/// symlink, never this process's own directory, and never a name whose part
/// after the prefix is not a plain pid. Errors are ignored: pruning is best
/// effort.
pub fn prune_dead_pid_dirs(base: &Path, prefix: &str) {
    prune_dead_pid_dirs_as(base, prefix, current_uid());
}

fn prune_dead_pid_dirs_as(base: &Path, prefix: &str, uid: u32) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(prefix)) else {
            continue;
        };
        let digits = rest.split('-').next().unwrap_or_default();
        // `parse` alone would accept a leading `+`.
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = digits.parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() || pid_is_running(pid) {
            continue;
        }
        let path = entry.path();
        if is_real_dir_owned_by(&path, uid) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// [`prepare_dev_home`], but an error stops the process.
///
/// The preferred `<base>/rantaiclaw-dev-home-<pid>` name may be a symlink or
/// another user's directory, which `prepare_dev_home` refuses. When the random
/// fallback cannot be created either, using the preferred name anyway would
/// write through that symlink or into that directory. This code only runs in
/// debug and test builds, so failing fast is the safe answer.
fn prepare_dev_home_or_panic(base: &Path, pid: u32, uid: u32) -> PathBuf {
    match prepare_dev_home(base, pid, uid) {
        Ok(dir) => dir,
        Err(err) => panic!(
            "rantaiclaw: could not create a private debug-build data root under {}: {err}. \
             Check the permissions of that directory and of the temp dir, or set \
             RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1 to use the real config tree.",
            base.display()
        ),
    }
}

/// The per-process root, decided once: prune dead siblings, prepare the
/// private directory, create the root inside it and print the notice.
fn dev_root() -> &'static Path {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let base = std::env::temp_dir();
        prune_dead_pid_dirs(&base, DEV_HOME_PREFIX);
        let pid = std::process::id();
        let dev_home = prepare_dev_home_or_panic(&base, pid, current_uid());
        let root = dev_home.join(".rantaiclaw");
        if let Err(err) = std::fs::create_dir_all(&root) {
            eprintln!(
                "rantaiclaw: could not create the debug-build data root {}: {err}",
                root.display()
            );
        }
        #[cfg(not(test))]
        eprintln!(
            "rantaiclaw: debug build, using {} instead of the real config tree. \
             Set RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1 to use the real one.",
            root.display()
        );
        root
    })
}

/// The root a debug build uses instead of `home/.rantaiclaw`, or `None` when
/// `home` is the real answer: the opt-out is set, or `home` is already under the
/// temp dir.
///
/// The root is `<temp>/rantaiclaw-dev-home-<pid>/.rantaiclaw`, one per process,
/// so it keeps the `.rantaiclaw` name the layout code expects. It is created
/// (owner-only) on first use, and the choice is cached for the process; see
/// [`prepare_dev_home`] for when a random suffix replaces the plain pid.
/// Outside `cfg(test)` the first redirect prints one line to stderr, so a
/// developer running `cargo run` knows where their data went.
pub fn redirected_root(home: &Path) -> Option<PathBuf> {
    if allow_real_config_dir() || is_under_temp_dir(home) {
        return None;
    }
    Some(dev_root().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lexical `starts_with(temp_dir)` check accepts a path that climbs out of
    /// the temp dir through `..` when the first component after the temp dir does
    /// not exist, because the walk in `resolve_for_temp_dir_check` never
    /// canonicalizes it.
    #[test]
    fn is_under_temp_dir_rejects_a_path_that_climbs_out_with_parent_dir() {
        let escaping = std::env::temp_dir()
            .join("rantaiclaw-no-such-dir")
            .join("..")
            .join("..")
            .join("home")
            .join("rantaiclaw_user")
            .join(".rantaiclaw");
        assert!(
            !is_under_temp_dir(&escaping),
            "{} climbs out of the temp dir and must not count as under it",
            escaping.display()
        );
    }
}

#[cfg(all(test, unix))]
mod dev_home_tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    /// A pid no process can hold: above Linux's `pid_max` and macOS's limit,
    /// and still a positive `i32` so `kill(pid, 0)` is well defined.
    const DEAD_PID: u32 = i32::MAX as u32;

    fn dev_home_name(pid: u32) -> String {
        format!("{DEV_HOME_PREFIX}{pid}")
    }

    fn make_dir_with_file(path: &Path) {
        std::fs::create_dir_all(path.join(".rantaiclaw")).expect("create fake dev home");
        std::fs::write(path.join(".rantaiclaw").join("marker"), "x").expect("write marker");
    }

    #[test]
    fn prepare_dev_home_creates_an_owner_only_directory() {
        let base = tempfile::tempdir().expect("base");
        let dir = prepare_dev_home(base.path(), 4242, current_uid()).expect("prepare");
        assert_eq!(dir, base.path().join(dev_home_name(4242)));
        let mode = std::fs::metadata(&dir)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "the dev home must be owner-only");
    }

    #[test]
    fn prepare_dev_home_reuses_an_existing_directory_owned_by_the_current_user() {
        let base = tempfile::tempdir().expect("base");
        let existing = base.path().join(dev_home_name(4242));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&existing)
            .expect("pre-create");
        let dir = prepare_dev_home(base.path(), 4242, current_uid()).expect("prepare");
        assert_eq!(dir, existing);
    }

    #[test]
    fn prepare_dev_home_refuses_a_symlink_and_uses_a_random_directory() {
        let base = tempfile::tempdir().expect("base");
        let target = tempfile::tempdir().expect("symlink target");
        let link = base.path().join(dev_home_name(4242));
        std::os::unix::fs::symlink(target.path(), &link).expect("symlink");

        let dir = prepare_dev_home(base.path(), 4242, current_uid()).expect("prepare");

        assert_ne!(dir, link, "a symlink must not be used as the dev home");
        let name = dir.file_name().and_then(|n| n.to_str()).expect("name");
        assert!(
            name.starts_with(&format!("{}-", dev_home_name(4242))),
            "the fallback keeps the pid so it can be pruned later, got {name}"
        );
        let meta = std::fs::symlink_metadata(&dir).expect("fallback exists");
        assert!(
            meta.file_type().is_dir(),
            "the fallback is a real directory"
        );
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::read_dir(target.path())
                .expect("read target")
                .count(),
            0,
            "nothing may be written through the symlink"
        );
    }

    #[test]
    fn prepare_dev_home_refuses_a_directory_owned_by_another_user() {
        let base = tempfile::tempdir().expect("base");
        let existing = base.path().join(dev_home_name(4242));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&existing)
            .expect("pre-create");
        let owner = std::fs::metadata(&existing).expect("metadata").uid();

        let dir = prepare_dev_home(base.path(), 4242, owner.wrapping_add(1)).expect("prepare");

        assert_ne!(
            dir, existing,
            "a directory of another uid must not be reused"
        );
        assert!(dir.is_dir());
    }

    #[test]
    fn dev_home_is_never_the_refused_name_when_no_fallback_can_be_made() {
        if current_uid() == 0 {
            eprintln!("skipped: root ignores directory modes");
            return;
        }
        let base = tempfile::tempdir().expect("base");
        let target = tempfile::tempdir().expect("symlink target");
        let link = base.path().join(dev_home_name(4242));
        std::os::unix::fs::symlink(target.path(), &link).expect("symlink");
        // With the base read-only, the random fallback cannot be created.
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o500))
            .expect("make the base read-only");

        let outcome = std::panic::catch_unwind(|| {
            prepare_dev_home_or_panic(base.path(), 4242, current_uid())
        });

        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore the base permissions");
        let payload = outcome.expect_err(
            "a refused name with no possible fallback must stop the process, not return the refused path",
        );
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .expect("panic message");
        assert!(
            message.contains(&base.path().display().to_string())
                && message.contains("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR"),
            "the message must name the base path and the escape hatch, got: {message}"
        );
        assert_eq!(
            std::fs::read_dir(target.path())
                .expect("read target")
                .count(),
            0,
            "nothing may be written through the symlink"
        );
    }

    #[test]
    fn prune_removes_the_directory_of_a_dead_pid() {
        let base = tempfile::tempdir().expect("base");
        let dead = base.path().join(dev_home_name(DEAD_PID));
        let dead_random = base
            .path()
            .join(format!("{}-a1b2c3", dev_home_name(DEAD_PID)));
        make_dir_with_file(&dead);
        make_dir_with_file(&dead_random);

        prune_dead_pid_dirs_as(base.path(), DEV_HOME_PREFIX, current_uid());

        assert!(!dead.exists(), "the dead pid's directory must be removed");
        assert!(!dead_random.exists(), "its random-suffix sibling too");
    }

    #[test]
    fn prune_keeps_the_directory_of_a_running_process() {
        let base = tempfile::tempdir().expect("base");
        let parent_pid = std::os::unix::process::parent_id();
        let alive = base.path().join(dev_home_name(parent_pid));
        make_dir_with_file(&alive);

        prune_dead_pid_dirs_as(base.path(), DEV_HOME_PREFIX, current_uid());

        assert!(alive.exists(), "a running process's directory must stay");
    }

    #[test]
    fn prune_keeps_the_directory_of_the_current_process() {
        let base = tempfile::tempdir().expect("base");
        let own = base.path().join(dev_home_name(std::process::id()));
        make_dir_with_file(&own);

        prune_dead_pid_dirs_as(base.path(), DEV_HOME_PREFIX, current_uid());

        assert!(own.exists());
    }

    #[test]
    fn prune_keeps_a_symlink_named_like_a_dead_pid() {
        let base = tempfile::tempdir().expect("base");
        let target = tempfile::tempdir().expect("symlink target");
        make_dir_with_file(target.path());
        let link = base.path().join(dev_home_name(DEAD_PID));
        std::os::unix::fs::symlink(target.path(), &link).expect("symlink");

        prune_dead_pid_dirs_as(base.path(), DEV_HOME_PREFIX, current_uid());

        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "the symlink itself must stay"
        );
        assert!(
            target.path().join(".rantaiclaw").join("marker").exists(),
            "the symlink target must not be deleted"
        );
    }

    #[test]
    fn prune_keeps_a_directory_owned_by_another_user() {
        let base = tempfile::tempdir().expect("base");
        let dead = base.path().join(dev_home_name(DEAD_PID));
        make_dir_with_file(&dead);
        let owner = std::fs::metadata(&dead).expect("metadata").uid();

        prune_dead_pid_dirs_as(base.path(), DEV_HOME_PREFIX, owner.wrapping_add(1));

        assert!(dead.exists(), "another user's directory must stay");
    }

    #[test]
    fn prune_keeps_names_whose_suffix_is_not_a_plain_pid() {
        let base = tempfile::tempdir().expect("base");
        let names = [
            format!("{DEV_HOME_PREFIX}abc"),
            format!("{DEV_HOME_PREFIX}{DEAD_PID}x"),
            format!("{DEV_HOME_PREFIX}-{DEAD_PID}"),
            format!("{DEV_HOME_PREFIX}0"),
            format!("{DEV_HOME_PREFIX}+{DEAD_PID}"),
            DEV_HOME_PREFIX.to_string(),
            format!("other-prefix-{DEAD_PID}"),
        ];
        for name in &names {
            make_dir_with_file(&base.path().join(name));
        }

        prune_dead_pid_dirs_as(base.path(), DEV_HOME_PREFIX, current_uid());

        for name in &names {
            assert!(base.path().join(name).exists(), "{name} must stay");
        }
    }
}
