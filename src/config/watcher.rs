//! File-watcher for the active profile's `config.toml`.
//!
//! Wraps `notify` with the same Access-event filter + debounce that
//! `src/skills/watcher.rs` uses, so editor saves and `cat >> config.toml`
//! both arrive as a single tick.
//!
//! Consumers call `changed()` or `try_changed()` and reload the running
//! config on each tick: the TUI (`TuiApp::reload_config`, per-frame) and the
//! gateway (`run_gateway` swaps its shared `Config` so the web console
//! reflects the change). This closes the "edited config.toml directly —
//! running process still uses the old provider / MCP servers" gap on both
//! surfaces.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub struct ConfigWatcher {
    _watcher: notify::RecommendedWatcher,
    reload_rx: mpsc::UnboundedReceiver<()>,
}

/// How long to collapse a burst of events into a single reload tick. 500ms
/// matches the skills watcher's cadence; both are user-initiated so the latency
/// is fine.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Whether an event kind should trigger a reload. Access-only events (read
/// syscalls) are skipped: they create a feedback loop if `reload_config` later
/// reads the file (same lesson as the skills watcher, commit 8a45370). Pulled
/// out of the callback so it can be tested without synthesizing real filesystem
/// events.
fn is_actionable_event_kind(kind: notify::EventKind) -> bool {
    use notify::event::ModifyKind;
    use notify::EventKind;
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
    )
}

/// Collapse a burst of raw events into one reload tick: on the first event wait
/// `DEBOUNCE`, drain everything else that arrived, then emit a single tick.
/// Pulled out of `watch` so it can be driven with paused tokio time in tests.
async fn debounce_loop(
    mut raw_rx: mpsc::UnboundedReceiver<notify::Event>,
    reload_tx: mpsc::UnboundedSender<()>,
) {
    while raw_rx.recv().await.is_some() {
        tokio::time::sleep(DEBOUNCE).await;
        while raw_rx.try_recv().is_ok() {}
        if reload_tx.send(()).is_err() {
            break;
        }
    }
}

impl ConfigWatcher {
    /// Watch the directory containing `config.toml`. We watch the
    /// *directory* (non-recursive) rather than the file itself
    /// because atomic writes (editor save → rename) replace the
    /// inode, and watching the path directly stops firing after the
    /// first rename. Filtering inside the callback keeps us scoped
    /// to `config.toml`.
    pub fn watch(config_path: &Path) -> Result<Self> {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<notify::Event>();
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
                    if !is_actionable_event_kind(event.kind) {
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
        tokio::spawn(debounce_loop(raw_rx, reload_tx));

        Ok(Self {
            _watcher: watcher,
            reload_rx,
        })
    }

    /// Wait for the next debounced reload tick. `None` means the background
    /// task feeding this receiver has ended, which only happens once this
    /// watcher itself is dropped — so a caller can only observe it by first
    /// dropping the watcher out from under its own receiver, which the private
    /// field prevents from happening by accident.
    pub async fn changed(&mut self) -> Option<()> {
        self.reload_rx.recv().await
    }

    /// Non-blocking: drain every pending tick and report whether at least one
    /// was there. A burst of ticks (several writes in the debounce window)
    /// collapses to a single `true`, matching `changed`'s one-tick-per-burst
    /// behavior for a caller that polls instead of awaiting.
    pub fn try_changed(&mut self) -> bool {
        let mut any = false;
        while self.reload_rx.try_recv().is_ok() {
            any = true;
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_skips_access_events_and_accepts_writes() {
        use notify::event::{AccessKind, AccessMode};
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        use notify::EventKind;

        // Access (read) events must be skipped — reacting to them creates a
        // read→reload→read feedback loop. This is the half the filename-based
        // tests below never exercise.
        assert!(!is_actionable_event_kind(EventKind::Access(
            AccessKind::Any
        )));
        assert!(!is_actionable_event_kind(EventKind::Access(
            AccessKind::Read
        )));
        assert!(!is_actionable_event_kind(EventKind::Access(
            AccessKind::Open(AccessMode::Read)
        )));
        assert!(!is_actionable_event_kind(EventKind::Any));

        // Writes / creates / removes / renames are actionable.
        assert!(is_actionable_event_kind(EventKind::Create(CreateKind::Any)));
        assert!(is_actionable_event_kind(EventKind::Remove(RemoveKind::Any)));
        assert!(is_actionable_event_kind(EventKind::Modify(ModifyKind::Any)));
        assert!(is_actionable_event_kind(EventKind::Modify(
            ModifyKind::Name(notify::event::RenameMode::Any)
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_coalesces_a_burst_into_a_single_tick() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<notify::Event>();
        let (reload_tx, mut reload_rx) = mpsc::unbounded_channel::<()>();
        let handle = tokio::spawn(debounce_loop(raw_rx, reload_tx));

        let ev = || notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any));
        raw_tx.send(ev()).unwrap();
        raw_tx.send(ev()).unwrap();
        raw_tx.send(ev()).unwrap();

        // Let the loop consume the first event and enter its debounce sleep,
        // then advance virtual time past the window. Paused time makes this
        // deterministic — no real sleeping, no flakiness.
        tokio::task::yield_now().await;
        tokio::time::advance(DEBOUNCE + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        assert!(reload_rx.try_recv().is_ok(), "burst must produce a tick");
        assert!(
            reload_rx.try_recv().is_err(),
            "the burst must coalesce into exactly ONE tick"
        );

        drop(raw_tx);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn emits_reload_when_config_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, "initial = 1\n").expect("write initial");
        let mut watcher = ConfigWatcher::watch(&config_path).expect("watcher");

        // Modify the file.
        std::fs::write(&config_path, "initial = 2\n").expect("modify");

        tokio::time::timeout(Duration::from_secs(2), watcher.changed())
            .await
            .expect("reload within timeout")
            .expect("reload event");
    }

    /// A local `let ConfigWatcher { reload_rx, .. } = watcher;` at a
    /// function's own top level does NOT close the channel promptly: Rust
    /// only drops the un-bound remainder of a partially-moved local when
    /// *that local's own scope* ends, so `_watcher` stays alive for the rest
    /// of this test function regardless of what `reload_rx` does. The same
    /// extraction inside a short-lived scope (a helper that returns just the
    /// receiver, a `{ }` block) drops the remainder as soon as that scope
    /// exits — which is exactly the shape a caller reaches for when they
    /// want "just the receiver" out of the struct, and exactly what closes
    /// the channel at once.
    #[tokio::test]
    async fn extracting_the_receiver_in_a_short_lived_scope_closes_it_at_once() {
        fn reload_rx_only(watcher: ConfigWatcher) -> mpsc::UnboundedReceiver<()> {
            let ConfigWatcher { reload_rx, .. } = watcher;
            reload_rx
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, "initial = 1\n").expect("write initial");
        let watcher = ConfigWatcher::watch(&config_path).expect("watcher");
        let mut reload_rx = reload_rx_only(watcher);

        let received = tokio::time::timeout(Duration::from_secs(2), reload_rx.recv())
            .await
            .expect(
                "must not time out: the channel should close as soon as the watcher's \
                 scope ends, not hang",
            );

        assert_eq!(
            received, None,
            "extracting the receiver out of a short-lived scope must close the channel"
        );
    }

    #[tokio::test]
    async fn ignores_sibling_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, "initial = 1\n").expect("write initial");
        let mut watcher = ConfigWatcher::watch(&config_path).expect("watcher");

        // Touch a sibling — must NOT trigger a reload.
        std::fs::write(temp.path().join("other.toml"), "other = 1\n").expect("write sibling");

        let result = tokio::time::timeout(Duration::from_millis(900), watcher.changed()).await;
        assert!(
            result.is_err(),
            "sibling file changes must not trigger a reload"
        );
    }
}
