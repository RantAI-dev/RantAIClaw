//! In-browser (web-modal) tool approval for the console SSE chat.
//!
//! The console runs the agent over an open SSE stream
//! (`api_v1::agent_chat_stream`). When a tool needs approval at the current
//! autonomy level, [`WebModalApprovalBackend`] — the SSE-surface
//! [`ApprovalBackend`] — emits an [`AgentEvent::ApprovalRequest`] over that
//! stream (so the browser renders an approve/deny modal) and then **blocks the
//! tool call** until the client resolves it via `POST /api/v1/approvals/{id}`
//! (see [`resolve`]) or the registry's deadline auto-denies.
//!
//! This is the web twin of the channel `ChatRelayApprovalBackend`: same async
//! [`crate::security::PendingApprovals`] await machinery, different transport
//! (SSE event out + HTTP POST in instead of a chat message round-trip). The id
//! lives in the registry's `basename` slot so resolution by id is unambiguous.
//!
//! Authority: the resolve endpoint is gated by the console's `check_auth`
//! (the API token is the approver). Absent a resolution, the request times out
//! to deny — secure by default.

use std::sync::Arc;

use crate::agent::{AgentEvent, AgentEventSender};
use crate::approval::{
    summarize_args, ApprovalBackend, ApprovalManager, ApprovalRequest, ApprovalResponse,
};
use crate::security::{Decision, PendingApprovals};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// SSE-surface approval backend: post a modal event, await the browser's reply.
pub struct WebModalApprovalBackend {
    /// Registry shared with the `POST /approvals/{id}` resolve handler.
    relay: Arc<PendingApprovals>,
    /// SSE event sink for this turn — carries the modal request to the browser.
    events: AgentEventSender,
    /// This turn's cancellation token (shared with the agent loop and the
    /// SSE-drop guard). A modal **Deny** fires it so the whole turn stops —
    /// parity with the TUI's N/Esc, where denying cancels the turn instead of
    /// letting the model retry an alternative tool.
    cancel: CancellationToken,
}

impl WebModalApprovalBackend {
    pub fn new(
        relay: Arc<PendingApprovals>,
        events: AgentEventSender,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            relay,
            events,
            cancel,
        }
    }
}

#[async_trait::async_trait]
impl ApprovalBackend for WebModalApprovalBackend {
    async fn decide(&self, _mgr: &ApprovalManager, request: &ApprovalRequest) -> ApprovalResponse {
        let id = Uuid::new_v4().to_string();
        // Tell the browser to show the modal. If the SSE receiver is gone the
        // client can't answer → fail closed (deny), never run the tool.
        if self
            .events
            .send(AgentEvent::ApprovalRequest {
                id: id.clone(),
                tool: request.tool_name.clone(),
                args: request.arguments.clone(),
            })
            .await
            .is_err()
        {
            return ApprovalResponse::No;
        }

        // Block this tool call until the client resolves `id` (via `resolve`)
        // or the registry deadline auto-denies. The id sits in the `basename`
        // slot so `resolve_by_basename(id, …)` is unambiguous.
        match self
            .relay
            .request_decision(id, summarize_args(&request.arguments), "console")
            .await
        {
            Decision::Once => ApprovalResponse::Yes,
            // "Always" from the modal → allowlist this tool for the session so
            // it stops prompting (the loop's `record_decision` folds an
            // `Always` response into `ApprovalManager`'s session allowlist).
            Decision::Session | Decision::Persist => ApprovalResponse::Always,
            Decision::Deny => {
                // Stop the whole turn on deny (TUI parity) — fire the shared
                // token so the loop unwinds cleanly at its next checkpoint
                // instead of returning the denial to the model to retry.
                self.cancel.cancel();
                ApprovalResponse::No
            }
        }
    }
}

/// Resolve a pending web-modal approval by id. `approve=false` denies; when
/// `approve=true`, `always=true` allowlists the tool for the session (no more
/// prompts) while `always=false` approves this one call. Returns `true` if a
/// request with that id was actually pending (and was resolved), `false`
/// otherwise (already resolved, timed out, or unknown id). Called by
/// `POST /api/v1/approvals/{id}`.
pub fn resolve(relay: &PendingApprovals, id: &str, approve: bool, always: bool) -> bool {
    let decision = if !approve {
        Decision::Deny
    } else if always {
        Decision::Session
    } else {
        Decision::Once
    };
    relay.resolve_by_basename(id, decision).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn manager() -> ApprovalManager {
        ApprovalManager::from_config(&crate::config::AutonomyConfig::default())
    }

    #[tokio::test]
    async fn emits_modal_event_and_yields_yes_on_approve() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        let backend = WebModalApprovalBackend::new(relay.clone(), tx, CancellationToken::new());
        let mgr = manager();
        let request = ApprovalRequest {
            tool_name: "web_search".into(),
            arguments: serde_json::json!({ "query": "rust" }),
        };

        let decide = tokio::spawn(async move { backend.decide(&mgr, &request).await });

        // The browser receives the modal request carrying an id…
        let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("event within deadline")
            .expect("event present");
        let id = match ev {
            AgentEvent::ApprovalRequest { id, tool, .. } => {
                assert_eq!(tool, "web_search");
                id
            }
            other => panic!("expected ApprovalRequest, got {other:?}"),
        };

        // …and approves it once by id.
        assert!(resolve(&relay, &id, true, false));
        assert_eq!(decide.await.unwrap(), ApprovalResponse::Yes);
    }

    #[tokio::test]
    async fn always_yields_always_response() {
        // "Approve · Always" from the modal must map to ApprovalResponse::Always
        // so the loop's record_decision allowlists the tool for the session.
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        let backend = WebModalApprovalBackend::new(relay.clone(), tx, CancellationToken::new());
        let mgr = manager();
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({ "command": "ls" }),
        };
        let decide = tokio::spawn(async move { backend.decide(&mgr, &request).await });
        let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let id = match ev {
            AgentEvent::ApprovalRequest { id, .. } => id,
            other => panic!("expected ApprovalRequest, got {other:?}"),
        };
        assert!(resolve(&relay, &id, true, true));
        assert_eq!(decide.await.unwrap(), ApprovalResponse::Always);
    }

    #[tokio::test]
    async fn yields_no_on_deny() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        let cancel = CancellationToken::new();
        let backend = WebModalApprovalBackend::new(relay.clone(), tx, cancel.clone());
        let mgr = manager();
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({ "command": "ls" }),
        };
        let decide = tokio::spawn(async move { backend.decide(&mgr, &request).await });
        let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let id = match ev {
            AgentEvent::ApprovalRequest { id, .. } => id,
            other => panic!("expected ApprovalRequest, got {other:?}"),
        };
        assert!(resolve(&relay, &id, false, false));
        assert_eq!(decide.await.unwrap(), ApprovalResponse::No);
        // Deny must cancel the turn (TUI parity), not just fail the one call.
        assert!(cancel.is_cancelled(), "a modal deny must cancel the turn");
    }

    #[tokio::test]
    async fn denies_when_client_disconnected() {
        // SSE receiver dropped → can't post the modal → fail closed.
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_millis(50))));
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        drop(rx);
        let backend = WebModalApprovalBackend::new(relay, tx, CancellationToken::new());
        let mgr = manager();
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({}),
        };
        assert_eq!(backend.decide(&mgr, &request).await, ApprovalResponse::No);
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_false() {
        let relay = PendingApprovals::new(Some(Duration::from_secs(10)));
        assert!(!resolve(&relay, "no-such-id", true, false));
    }
}
