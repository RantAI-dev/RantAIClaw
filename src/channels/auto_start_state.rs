//! Diagnostic state shared between the TUI's auto-start path and the
//! `/channels` (+ `/platforms`) command renderers.
//!
//! Pre-v0.6.6 the `/channels` table reported "polling" purely on the basis
//! of "we dispatched `tokio::spawn`". If `start_channels` errored mid-build
//! (bad token surfaced through the supervised listener, provider
//! validation 401, missing peripheral toolchain), the spawn task hit the
//! Err arm, logged a `tracing::warn!`, and the user — running the TUI —
//! never saw it. The status table kept lying.
//!
//! This module exposes a global `Mutex<AutoStartState>` so the spawned task
//! can mark Starting / Running / Failed / Terminated, and the command
//! renderers read the latest snapshot at display time.

use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default)]
pub enum AutoStartState {
    /// `start_channels` was never dispatched (no channels configured at
    /// TUI launch).
    #[default]
    NotDispatched,
    /// Dispatched and currently inside `start_channels` — provider build,
    /// listener spawn, dispatch loop entry, etc.
    Starting { since_unix: u64 },
    /// Dispatch loop exited cleanly (graceful shutdown). Listeners are
    /// no longer active.
    Terminated { at_unix: u64 },
    /// `start_channels` returned `Err`. Listeners never fully started or
    /// exited unexpectedly. The message is the formatted error chain.
    Failed { message: String, at_unix: u64 },
}

fn cell() -> &'static Mutex<AutoStartState> {
    static STATE: OnceLock<Mutex<AutoStartState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(AutoStartState::NotDispatched))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn mark_starting() {
    let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
    *g = AutoStartState::Starting {
        since_unix: now_unix(),
    };
    tracing::info!("auto-start: dispatched");
}

pub fn mark_terminated() {
    let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
    *g = AutoStartState::Terminated {
        at_unix: now_unix(),
    };
    tracing::info!("auto-start: terminated");
}

pub fn mark_failed(message: String) {
    let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
    *g = AutoStartState::Failed {
        message,
        at_unix: now_unix(),
    };
}

pub fn snapshot() -> AutoStartState {
    cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Best-effort check that listeners are at least past the build phase.
/// Returns true if the spawned task entered Starting > 5 seconds ago and
/// hasn't transitioned to Failed/Terminated. Five seconds is a generous
/// upper bound on `start_channels` build cost; if we haven't crashed by
/// then we're probably in the dispatch loop.
pub fn looks_running() -> bool {
    matches!(snapshot(), AutoStartState::Starting { since_unix }
        if now_unix().saturating_sub(since_unix) >= 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_not_dispatched() {
        assert!(matches!(
            AutoStartState::default(),
            AutoStartState::NotDispatched
        ));
    }

    #[test]
    fn transitions() {
        // Note: the global cell is process-wide, so this test runs in
        // isolation only when there's no concurrent test invoking the
        // module. Acceptable for a smoke check; full ordering coverage
        // would need an injected state holder.
        mark_starting();
        assert!(matches!(snapshot(), AutoStartState::Starting { .. }));

        mark_failed("test error".to_string());
        match snapshot() {
            AutoStartState::Failed { message, .. } => assert_eq!(message, "test error"),
            other => panic!("expected Failed, got {other:?}"),
        }

        mark_terminated();
        assert!(matches!(snapshot(), AutoStartState::Terminated { .. }));
    }
}

// Suppress dead-code warnings for the `Instant` import in case future
// instrumentation wants to use it for span timing without re-importing.
#[allow(dead_code)]
fn _instant_witness() -> Instant {
    Instant::now()
}
