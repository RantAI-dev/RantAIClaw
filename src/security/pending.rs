//! Async approval queue for shell-command allowlist expansion.
//!
//! When a Supervised-mode tool call hits a basename that's not on the
//! boot allowlist, instead of hard-failing the tool returns "pending"
//! via [`PendingApprovals::request_decision`]. That future resolves
//! when:
//!
//! - a UI (TUI overlay, channel reply parser, gateway HTTP route, …)
//!   calls [`PendingApprovals::resolve`] with a [`Decision`], or
//! - the configured timeout elapses (auto-deny).
//!
//! The registry itself does **not** know about channels — it just
//! tracks pending requests and resolves futures. Notification of new
//! requests is delivered via a `tokio::sync::broadcast` so any number
//! of listeners (TUI, channel implementations) can render the prompt
//! concurrently. Only the first resolver wins; later resolves are
//! no-ops.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

/// User's response to a pending approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Allow this single execution; do not modify the allowlist.
    Once,
    /// Add the basename to the session-only runtime allowlist.
    Session,
    /// Add the basename to the runtime allowlist and persist to disk.
    Persist,
    /// Reject; the tool call fails with the original allowlist error.
    Deny,
}

/// A request awaiting decision. Cloneable snapshot — the live oneshot
/// sender stays inside the registry.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub id: Uuid,
    /// Single-token shell command basename (e.g. `"brew"`).
    pub basename: String,
    /// Full command string the agent attempted, for display context.
    pub full_command: String,
    /// Channel name that originated the request (e.g. `"tui"`, `"telegram"`).
    /// May be empty when the request didn't carry a channel hint.
    pub channel: String,
    /// Unix epoch seconds when the request was created.
    pub created_at: u64,
}

impl PendingRequest {
    fn new(basename: String, full_command: String, channel: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            basename,
            full_command,
            channel,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

/// Registry of pending approval requests.
///
/// Cheap to clone (`Arc` inside); the same registry handle should be
/// shared between the shell tool (producer) and the various UIs
/// (consumers).
#[derive(Clone)]
pub struct PendingApprovals {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PendingApprovals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingApprovals")
            .field("pending", &self.inner.snapshot.lock().len())
            .field("timeout", &self.inner.timeout)
            .finish()
    }
}

struct Inner {
    /// Oneshot senders awaiting resolution, keyed by request id.
    waiting: Mutex<HashMap<Uuid, oneshot::Sender<Decision>>>,
    /// Snapshot of all currently-pending requests (for UIs that render
    /// a queue).
    snapshot: Mutex<HashMap<Uuid, PendingRequest>>,
    /// New-request notifications. Listeners that miss a beat just see
    /// the snapshot next time they wake up.
    notify_tx: broadcast::Sender<PendingRequest>,
    /// How long to wait before auto-denying.
    timeout: Duration,
}

impl PendingApprovals {
    /// Create a registry with the given decision timeout.
    pub fn new(timeout: Duration) -> Self {
        let (notify_tx, _) = broadcast::channel(32);
        Self {
            inner: Arc::new(Inner {
                waiting: Mutex::new(HashMap::new()),
                snapshot: Mutex::new(HashMap::new()),
                notify_tx,
                timeout,
            }),
        }
    }

    /// Subscribe to new-request notifications. Returns a fresh
    /// `broadcast::Receiver` — each subscriber gets its own copy.
    pub fn subscribe(&self) -> broadcast::Receiver<PendingRequest> {
        self.inner.notify_tx.subscribe()
    }

    /// Snapshot of currently-pending requests.
    pub fn list(&self) -> Vec<PendingRequest> {
        let snap = self.inner.snapshot.lock();
        let mut v: Vec<PendingRequest> = snap.values().cloned().collect();
        v.sort_by_key(|r| r.created_at);
        v
    }

    /// Block until the user decides on this basename, or the configured
    /// timeout elapses (in which case we auto-deny).
    pub async fn request_decision(
        &self,
        basename: impl Into<String>,
        full_command: impl Into<String>,
        channel: impl Into<String>,
    ) -> Decision {
        let request = PendingRequest::new(basename.into(), full_command.into(), channel.into());
        let id = request.id;

        let (tx, rx) = oneshot::channel();
        {
            self.inner.waiting.lock().insert(id, tx);
            self.inner.snapshot.lock().insert(id, request.clone());
        }
        // Ignore send error: no live subscribers is fine.
        let _ = self.inner.notify_tx.send(request);

        let timeout = self.inner.timeout;
        let result = tokio::time::timeout(timeout, rx).await;
        // Always clean up before returning.
        self.inner.waiting.lock().remove(&id);
        self.inner.snapshot.lock().remove(&id);

        match result {
            Ok(Ok(decision)) => decision,
            // oneshot dropped or timed out → deny.
            _ => Decision::Deny,
        }
    }

    /// Resolve a pending request. Returns `true` if a sender was
    /// present and accepted the decision, `false` if the id was not
    /// pending (already resolved, timed out, or never existed).
    pub fn resolve(&self, id: Uuid, decision: Decision) -> bool {
        let tx = self.inner.waiting.lock().remove(&id);
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Resolve a pending request matched by basename. Useful for chat
    /// channels where users reply with a token (`y brew`) rather than
    /// a UUID. Returns the resolved request id if exactly one match
    /// existed.
    pub fn resolve_by_basename(&self, basename: &str, decision: Decision) -> Option<Uuid> {
        let id = {
            let snap = self.inner.snapshot.lock();
            let matches: Vec<Uuid> = snap
                .values()
                .filter(|r| r.basename == basename)
                .map(|r| r.id)
                .collect();
            if matches.len() != 1 {
                return None;
            }
            matches[0]
        };
        if self.resolve(id, decision) {
            Some(id)
        } else {
            None
        }
    }
}

impl Default for PendingApprovals {
    /// 5-minute default timeout matches the design doc.
    fn default() -> Self {
        Self::new(Duration::from_mins(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_returns_decision() {
        let registry = PendingApprovals::new(Duration::from_secs(10));
        let registry2 = registry.clone();

        let task = tokio::spawn(async move {
            registry2
                .request_decision("brew", "brew --version", "tui")
                .await
        });

        // Give the producer a chance to register.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let pending = registry.list();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].basename, "brew");

        assert!(registry.resolve(pending[0].id, Decision::Session));
        assert_eq!(task.await.unwrap(), Decision::Session);
        assert!(
            registry.list().is_empty(),
            "registry should clean up after resolve"
        );
    }

    #[tokio::test]
    async fn timeout_yields_deny() {
        let registry = PendingApprovals::new(Duration::from_millis(50));
        let decision = registry
            .request_decision("brew", "brew --version", "tui")
            .await;
        assert_eq!(decision, Decision::Deny);
        assert!(registry.list().is_empty());
    }

    #[tokio::test]
    async fn resolve_by_basename_unique_match() {
        let registry = PendingApprovals::new(Duration::from_secs(10));
        let r = registry.clone();
        let task =
            tokio::spawn(async move { r.request_decision("rg", "rg foo", "telegram").await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(registry
            .resolve_by_basename("rg", Decision::Persist)
            .is_some());
        assert_eq!(task.await.unwrap(), Decision::Persist);
    }

    #[tokio::test]
    async fn resolve_by_basename_ambiguous_is_none() {
        let registry = PendingApprovals::new(Duration::from_secs(10));
        let r1 = registry.clone();
        let r2 = registry.clone();
        let _t1 =
            tokio::spawn(async move { r1.request_decision("rg", "rg foo", "telegram").await });
        let _t2 =
            tokio::spawn(async move { r2.request_decision("rg", "rg bar", "telegram").await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Two pending `rg` requests → cannot disambiguate.
        assert!(registry.resolve_by_basename("rg", Decision::Once).is_none());
    }

    #[tokio::test]
    async fn subscribe_receives_new_requests() {
        let registry = PendingApprovals::new(Duration::from_secs(10));
        let mut rx = registry.subscribe();
        let r = registry.clone();
        let _t =
            tokio::spawn(async move { r.request_decision("brew", "brew --version", "tui").await });

        let received = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("notification within deadline")
            .expect("recv ok");
        assert_eq!(received.basename, "brew");
    }

    #[tokio::test]
    async fn resolve_unknown_id_returns_false() {
        let registry = PendingApprovals::new(Duration::from_secs(10));
        assert!(!registry.resolve(Uuid::new_v4(), Decision::Once));
    }
}
