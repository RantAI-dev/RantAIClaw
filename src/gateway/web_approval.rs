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
//! (SSE event out + HTTP POST in instead of a chat message round-trip). Each
//! request registers under a fresh UUID (the id the browser is handed) and is
//! scoped to the turn that raised it, so resolution by id is unambiguous and one
//! stream can neither see nor answer another's requests.
//!
//! Authority: the resolve endpoint is gated by the console's `check_auth`
//! (the API token is the approver). Absent a resolution, the request times out
//! to deny — secure by default.

use std::sync::Arc;

use crate::agent::{AgentEvent, AgentEventSender};
use crate::approval::{
    summarize_args, ApprovalBackend, ApprovalManager, ApprovalRequest, ApprovalResponse,
    DecisionBasis, Verdict,
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
    async fn decide(&self, mgr: &ApprovalManager, request: &ApprovalRequest) -> ApprovalResponse {
        self.decide_verdict(mgr, request).await.response
    }

    async fn decide_verdict(&self, mgr: &ApprovalManager, request: &ApprovalRequest) -> Verdict {
        let id = Uuid::new_v4();
        // Tell the browser to show the modal. If the SSE receiver is gone the
        // client can't answer → fail closed (deny), never run the tool. The
        // modal was never shown, so nobody could have answered.
        if self
            .events
            .send(AgentEvent::ApprovalRequest {
                id: id.to_string(),
                tool: request.tool_name.clone(),
                args: request.arguments.clone(),
                always_available: mgr.always_available_for(&request.tool_name),
            })
            .await
            .is_err()
        {
            return Verdict::no_approver();
        }

        // Register under the same UUID the browser holds, scoped to this turn so
        // only its own SSE stream sees and can resolve it. Inside the turn task
        // `current_turn_scope()` yields `("console", <turn_scope>)`.
        let (channel, reply_target) = crate::security::current_turn_scope();
        // Block this tool call until the client resolves `id` (via `resolve`)
        // or the registry deadline auto-denies.
        let outcome = self
            .relay
            .request_decision_in_outcome(
                id,
                id.to_string(),
                summarize_args(&request.arguments),
                channel,
                reply_target,
            )
            .await;
        match outcome.decision {
            Decision::Once => Verdict::answered(ApprovalResponse::Yes),
            // "Always" from the modal → allowlist this tool for the session so
            // it stops prompting (the loop's `record_decision` folds an
            // `Always` response into `ApprovalManager`'s session allowlist).
            Decision::Session | Decision::Persist => Verdict::answered(ApprovalResponse::Always),
            Decision::Deny if outcome.timed_out => {
                // The deadline auto-denied. Nobody clicked anything, so this is
                // not a person's No. Cancel the turn as a deny does, so the
                // loop does not retry the same modal on the next iteration.
                self.cancel.cancel();
                Verdict {
                    response: ApprovalResponse::No,
                    basis: DecisionBasis::NoAnswer,
                    args_shown_in_full: false,
                }
            }
            Decision::Deny => {
                // Stop the whole turn on deny (TUI parity) — fire the shared
                // token so the loop unwinds cleanly at its next checkpoint
                // instead of returning the denial to the model to retry.
                self.cancel.cancel();
                Verdict::answered(ApprovalResponse::No)
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
    // Web approvals are always addressed by their UUID (the id the browser was
    // handed in the `approval_request` event), never by basename — two turns
    // waiting on the same command must not be resolvable by a shared basename.
    match Uuid::parse_str(id) {
        Ok(uuid) => relay.resolve(uuid, decision),
        Err(_) => false,
    }
}

/// Per-session "Always" grants live in `crate::approval::session_grants` (moved
/// there so the autonomy tightening path can revoke them without `approval`
/// depending on `gateway`). Re-exported so existing gateway callers are unchanged.
pub use crate::approval::session_grants::{
    clear_all_session_grants, record_session_grants, session_granted_tools,
};

/// Exempt the `shell` tool from the Layer-A tool-gate on the web console. The
/// shell tool has its own command-level gate (Layer-B), which the console now
/// resolves via the modal, so gating `shell` at Layer-A too would mean two
/// modals for one command. Remove it from `always_ask` (which would otherwise
/// force a Layer-A prompt regardless of `auto_approve`) and add it to
/// `auto_approve`.
pub fn exempt_shell_from_tool_gate(autonomy: &mut crate::config::AutonomyConfig) {
    autonomy.always_ask.retain(|t| t != "shell");
    if !autonomy.auto_approve.iter().any(|t| t == "shell") {
        autonomy.auto_approve.push("shell".to_string());
    }
}

/// The narrowing a turn carries on top of the global autonomy config.
///
/// A turn's approval manager is built from an autonomy that is not always the
/// global one: the console exempts `shell` from the Layer-A gate, a webhook
/// carries the sender's already-approved tools. Pushing the global config
/// straight onto such a manager would WIDEN it (the console would gate `shell`
/// twice, the webhook would lose the carried-over approvals), so the
/// narrowing travels with the turn and is re-applied on every config change.
#[derive(Debug, Clone, Default)]
pub enum AutonomyOverlay {
    /// The turn runs the global config as it is.
    #[default]
    Global,
    /// The turn's `shell` tool is exempt from the Layer-A tool-gate.
    ShellExempt,
    /// The turn carries these tools as already approved.
    ExtraAutoApprove(Vec<String>),
}

impl AutonomyOverlay {
    /// The autonomy this turn's manager sees, given the global config.
    pub fn resolve(&self, global: &crate::config::AutonomyConfig) -> crate::config::AutonomyConfig {
        let mut autonomy = global.clone();
        match self {
            Self::Global => {}
            Self::ShellExempt => exempt_shell_from_tool_gate(&mut autonomy),
            Self::ExtraAutoApprove(extra) => {
                for name in extra {
                    if !autonomy.auto_approve.iter().any(|t| t == name) {
                        autonomy.auto_approve.push(name.clone());
                    }
                }
            }
        }
        autonomy
    }
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
    async fn modal_event_says_always_is_unavailable_when_the_tool_is_forced_to_prompt() {
        // Under Manual every tool is forced to prompt, so the Layer-A modal must
        // not claim an Always answer will hold.
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        let backend = WebModalApprovalBackend::new(relay.clone(), tx, CancellationToken::new());
        let mgr = ApprovalManager::from_config(&crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Supervised,
            always_ask: vec!["*".to_string()],
            ..crate::config::AutonomyConfig::default()
        });
        let request = ApprovalRequest {
            tool_name: "web_search".into(),
            arguments: serde_json::json!({ "query": "rust" }),
        };
        let decide = tokio::spawn(async move { backend.decide(&mgr, &request).await });
        let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("event within deadline")
            .expect("event present");
        let id = match ev {
            AgentEvent::ApprovalRequest {
                id,
                always_available,
                ..
            } => {
                assert!(!always_available, "Manual must say Always is unavailable");
                id
            }
            other => panic!("expected ApprovalRequest, got {other:?}"),
        };
        assert!(resolve(&relay, &id, true, false));
        assert_eq!(decide.await.unwrap(), ApprovalResponse::Yes);
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

    /// The SSE receiver is gone, so the modal was never shown and nobody could
    /// answer. That is a no-approver refusal, not an answered No.
    #[tokio::test]
    async fn dropped_sse_receiver_reports_no_approver() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_millis(50))));
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        drop(rx);
        let backend = WebModalApprovalBackend::new(relay, tx, CancellationToken::new());
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({}),
        };
        let verdict = backend.decide_verdict(&manager(), &request).await;
        assert_eq!(verdict.response, ApprovalResponse::No);
        assert_eq!(verdict.basis, DecisionBasis::NoApprover);
    }

    /// The modal was shown and the registry deadline passed with no answer.
    /// Nobody said No, so the basis is `NoAnswer`, not `Answered`.
    #[tokio::test]
    async fn elapsed_deadline_reports_no_answer() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_millis(30))));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        let backend = WebModalApprovalBackend::new(relay, tx, CancellationToken::new());
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({}),
        };
        // The modal is shown; nobody clicks it.
        let verdict = backend.decide_verdict(&manager(), &request).await;
        assert!(
            matches!(rx.try_recv(), Ok(AgentEvent::ApprovalRequest { .. })),
            "the modal must have been shown before the deadline"
        );
        assert_eq!(verdict.response, ApprovalResponse::No);
        assert_eq!(verdict.basis, DecisionBasis::NoAnswer);
    }

    /// A person clicking Deny is an answered No, and it still cancels the turn.
    #[tokio::test]
    async fn modal_deny_reports_answered() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        let backend = WebModalApprovalBackend::new(relay.clone(), tx, CancellationToken::new());
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({}),
        };
        let decide =
            tokio::spawn(async move { backend.decide_verdict(&manager(), &request).await });
        let id = match rx.recv().await.expect("event") {
            AgentEvent::ApprovalRequest { id, .. } => id,
            other => panic!("expected ApprovalRequest, got {other:?}"),
        };
        assert!(resolve(&relay, &id, false, false));
        let verdict = decide.await.unwrap();
        assert_eq!(verdict.response, ApprovalResponse::No);
        assert_eq!(verdict.basis, DecisionBasis::Answered);
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_false() {
        let relay = PendingApprovals::new(Some(Duration::from_secs(10)));
        assert!(!resolve(&relay, "no-such-id", true, false));
    }

    #[tokio::test]
    async fn resolve_by_basename_is_rejected_on_the_web_path() {
        // A shell approval with basename "git" must NOT be resolvable by posting
        // "git": the web path is UUID-only, so two turns waiting on the same
        // command can't cross-resolve. Only the UUID resolves it.
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let id = Uuid::new_v4();
        let r = relay.clone();
        let task = tokio::spawn(async move {
            r.request_decision_in(id, "git", "git status", "console", "turn-x")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !resolve(&relay, "git", true, false),
            "basename must not resolve on the web path"
        );
        assert!(
            resolve(&relay, &id.to_string(), true, false),
            "the UUID resolves it"
        );
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    #[test]
    fn exempt_shell_removes_it_from_layer_a_even_when_always_ask() {
        // Even if `shell` is on always_ask (which normally forces a Layer-A
        // prompt regardless of auto_approve), the console exemption must make
        // needs_approval("shell") false — otherwise the user sees two modals
        // (Layer-A tool + Layer-B command). Other tools stay gated.
        let mut autonomy = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Supervised,
            always_ask: vec!["shell".to_string()],
            ..crate::config::AutonomyConfig::default()
        };
        exempt_shell_from_tool_gate(&mut autonomy);
        let mgr = crate::approval::ApprovalManager::from_config(&autonomy);
        assert!(
            !mgr.needs_approval("shell"),
            "shell must be exempt from Layer-A"
        );
        assert!(
            mgr.needs_approval("http_request"),
            "other tools stay gated at Layer-A"
        );
    }

    // ── AutonomyOverlay ───────────────────────────────────────

    fn supervised_with_shell_asked() -> crate::config::AutonomyConfig {
        crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Supervised,
            always_ask: vec!["shell".to_string(), "ssh".to_string()],
            ..crate::config::AutonomyConfig::default()
        }
    }

    /// The plain overlay hands the global config over unchanged.
    #[test]
    fn the_global_overlay_resolves_to_the_global_config() {
        let global = supervised_with_shell_asked();
        let resolved = AutonomyOverlay::Global.resolve(&global);
        assert_eq!(resolved.level, global.level);
        assert_eq!(resolved.always_ask, global.always_ask);
        assert_eq!(resolved.auto_approve, global.auto_approve);
    }

    /// The console's narrowing must survive a config change: the Layer-A gate
    /// must not come back for `shell`, or one command shows two modals.
    #[test]
    fn the_shell_exempt_overlay_keeps_the_exemption() {
        let global = supervised_with_shell_asked();
        let resolved = AutonomyOverlay::ShellExempt.resolve(&global);
        assert!(
            !resolved.always_ask.iter().any(|t| t == "shell"),
            "shell must stay off always_ask"
        );
        assert!(
            resolved.auto_approve.iter().any(|t| t == "shell"),
            "shell must stay auto-approved"
        );
        assert!(
            resolved.always_ask.iter().any(|t| t == "ssh"),
            "the other entries stay"
        );
        let mgr = crate::approval::ApprovalManager::from_config(&resolved);
        assert!(
            !mgr.needs_approval("shell"),
            "the exemption must reach the gate"
        );
    }

    /// The webhook's carried-over approvals must survive a config change, and
    /// must not be added twice.
    #[test]
    fn the_extra_auto_approve_overlay_keeps_the_carried_over_tools() {
        let global = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Supervised,
            auto_approve: vec!["file_read".to_string()],
            ..crate::config::AutonomyConfig::default()
        };
        let resolved = AutonomyOverlay::ExtraAutoApprove(vec![
            "http_request".to_string(),
            "file_read".to_string(),
        ])
        .resolve(&global);
        assert_eq!(
            resolved.auto_approve,
            vec!["file_read".to_string(), "http_request".to_string()],
            "the carried-over tool is added once, after the global list"
        );
        let mgr = crate::approval::ApprovalManager::from_config(&resolved);
        assert!(!mgr.needs_approval("http_request"));
    }
}
