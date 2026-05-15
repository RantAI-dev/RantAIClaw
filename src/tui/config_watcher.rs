//! File-watcher for the active profile's `config.toml`.
//!
//! Wraps `notify` with the same Access-event filter + debounce that
//! `src/skills/watcher.rs` uses, so editor saves and `cat >> config.toml`
//! both arrive as a single tick.
//!
//! The receiver is drained each render frame by `TuiApp`; on tick it
//! calls `reload_config`, which already handles the full reload
//! pipeline (decrypt secrets, push `TurnRequest::Reload` to the agent
//! actor, refresh status-bar / `/model` picker / `/channels` snapshot).
//!
//! This is what closes the "user edited config.toml directly — agent
//! still uses old provider/MCP servers" gap. Wizard-driven reload was
//! already wired (`reload_config` is called when the setup overlay
//! closes); this watcher is the missing direct-edit half.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub struct ConfigWatcher {
    _watcher: notify::RecommendedWatcher,
    pub reload_rx: mpsc::UnboundedReceiver<()>,
}

impl ConfigWatcher {
    /// Watch the directory containing `config.toml`. We watch the
    /// *directory* (non-recursive) rather than the file itself
    /// because atomic writes (editor save → rename) replace the
    /// inode, and watching the path directly stops firing after the
    /// first rename. Filtering inside the callback keeps us scoped
    /// to `config.toml`.
    pub fn watch(config_path: &Path) -> Result<Self> {
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<notify::Event>();
        let (reload_tx, reload_rx) = mpsc::unbounded_channel::<()>();

        let parent = config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent: {}", config_path.display()))?
            .to_path_buf();
        let target_name = config_path
            .file_name()
            .ok_or_else(|| {
                anyhow::anyhow!("config path has no file name: {}", config_path.display())
            })?
            .to_os_string();

        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    use notify::EventKind;
                    // Skip Access-only events (read syscalls); they
                    // create a feedback loop if reload_config later
                    // reads the file. Same lesson as skills watcher
                    // (commit 8a45370).
                    let actionable = matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Modify(
                                notify::event::ModifyKind::Data(_)
                                    | notify::event::ModifyKind::Name(_)
                                    | notify::event::ModifyKind::Any
                            )
                    );
                    if !actionable {
                        return;
                    }
                    // Only react to events on config.toml itself, not
                    // sibling files in the profile dir.
                    let matches = event
                        .paths
                        .iter()
                        .any(|p| p.file_name().is_some_and(|n| n == target_name));
                    if !matches {
                        return;
                    }
                    let _ = raw_tx.send(event);
                }
            })?;

        watcher.watch(&parent, RecursiveMode::NonRecursive)?;

        // Debounce: collapse a burst of events into one reload tick.
        // 500ms matches the skills watcher's cadence; both are
        // user-initiated, so the latency is fine.
        tokio::spawn(async move {
            while raw_rx.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(500)).await;
                while raw_rx.try_recv().is_ok() {}
                if reload_tx.send(()).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            _watcher: watcher,
            reload_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_reload_when_config_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, "initial = 1\n").expect("write initial");
        let mut watcher = ConfigWatcher::watch(&config_path).expect("watcher");

        // Modify the file.
        std::fs::write(&config_path, "initial = 2\n").expect("modify");

        tokio::time::timeout(Duration::from_secs(2), watcher.reload_rx.recv())
            .await
            .expect("reload within timeout")
            .expect("reload event");
    }

    #[tokio::test]
    async fn ignores_sibling_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, "initial = 1\n").expect("write initial");
        let mut watcher = ConfigWatcher::watch(&config_path).expect("watcher");

        // Touch a sibling — must NOT trigger a reload.
        std::fs::write(temp.path().join("other.toml"), "other = 1\n").expect("write sibling");

        let result =
            tokio::time::timeout(Duration::from_millis(900), watcher.reload_rx.recv()).await;
        assert!(
            result.is_err(),
            "sibling file changes must not trigger a reload"
        );
    }
}
