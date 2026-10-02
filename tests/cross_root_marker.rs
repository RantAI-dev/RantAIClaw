//! End-to-end tests for the cross-root active-workspace-marker guard.
//!
//! The unit tests in `src/config/schema.rs::tests` cover the guard function
//! directly and the copy / move end-to-end paths. The snapshot path is the
//! one case that imports `crate::lifecycle::update_snapshot`, which the
//! library test module cannot reach from both `cargo test --lib` and the
//! bin's test build; this integration test exercises it instead.

use std::path::Path;

use rantaiclaw::config::Config;
use rantaiclaw::lifecycle::update_snapshot;

mod common;

const ACTIVE_WORKSPACE_STATE_FILE: &str = "active_workspace.toml";

async fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut entries = tokio::fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let ft = entry.file_type().await?;
        let target = dst.join(entry.file_name());
        if ft.is_dir() {
            Box::pin(copy_tree(&entry.path(), &target)).await?;
        } else if ft.is_file() {
            tokio::fs::copy(entry.path(), &target).await?;
        }
    }
    Ok(())
}

/// RAII guard that pins an env var to a value for its lifetime and restores
/// the prior value (or absence) on drop, so a panic in the middle of a
/// test does not leak the override into the next test.
struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }

    fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => std::env::set_var(self.key, prev),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Snapshot path: `update_snapshot::create` on A, copy the tree to B,
/// `update_snapshot::restore` with B as the root. Resolving with B must
/// still pick the local profile under B (the carried marker is set aside
/// because the local root has its own config for that profile).
#[tokio::test]
async fn restored_snapshot_resolves_inside_itself() {
    let _config_guard = common::ConfigDirGuard::acquire().await;

    let a_home = tempfile::TempDir::new().unwrap();
    let b_home = tempfile::TempDir::new().unwrap();
    let a_root = a_home.path().join(".rantaiclaw");
    let b_root = b_home.path().join(".rantaiclaw");

    tokio::fs::create_dir_all(a_root.join("profiles/default"))
        .await
        .unwrap();
    tokio::fs::write(
        a_root.join("profiles/default/config.toml"),
        "default_model = \"a-cfg\"\n",
    )
    .await
    .unwrap();

    let marker_state = format!(
        "config_dir = \"{}\"\n",
        a_root.join("profiles/default").to_string_lossy()
    );
    tokio::fs::write(a_root.join(ACTIVE_WORKSPACE_STATE_FILE), &marker_state)
        .await
        .unwrap();

    let snapshot = update_snapshot::create(&a_root, "0.0.0", "0.0.1", "default", None)
        .expect("snapshot create");
    assert!(
        snapshot
            .manifest
            .files
            .iter()
            .any(|f| f == ACTIVE_WORKSPACE_STATE_FILE),
        "snapshot must carry the marker"
    );

    // Copy A to B and restore the snapshot on top of B so B has the same
    // shape as A (same files, including the marker).
    copy_tree(&a_root, &b_root).await.unwrap();
    update_snapshot::restore(&snapshot, &b_root).expect("snapshot restore");

    // The marker is what the snapshot carried over.
    let restored_marker = tokio::fs::read_to_string(b_root.join(ACTIVE_WORKSPACE_STATE_FILE))
        .await
        .unwrap();
    assert!(
        restored_marker.contains(
            &a_root
                .join("profiles/default")
                .to_string_lossy()
                .to_string()
        ),
        "snapshot restore must preserve the marker, got: {restored_marker}"
    );

    // Resolve with B as the active home. `Config::resolve_active_paths`
    // goes through the same `resolve_runtime_config_dirs` the marker reader
    // feeds. Guards (not the test) own the env-var lifetime so a panic in
    // the middle of the test does not leave HOME pointed at a tempdir.
    let _g_home = EnvGuard::set("HOME", b_home.path());
    let _g_workspace = EnvGuard::unset("RANTAICLAW_WORKSPACE");
    let _g_config_dir = EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
    let _g_allow = EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

    let (config_path, _workspace_dir) = Config::resolve_active_paths()
        .await
        .expect("resolution succeeds");
    assert_eq!(
        config_path,
        b_root.join("profiles/default/config.toml"),
        "the local profile under B must win after a snapshot restore"
    );
}
