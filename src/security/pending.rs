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
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::audit::ApprovalOutcome;

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

/// What an answer to a request may hold for.
///
/// The scope an answer has must equal the scope its surface states: a request
/// carries the scope of the gate that raised it, so every surface can say what
/// the answer really does instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    /// The answer covers this run only. Nothing holds.
    None,
    /// The answer holds for this command basename for the rest of the session —
    /// the shell unallowed-command gate.
    Command,
    /// The answer holds for this command basename **at medium risk** for the rest
    /// of the session — the shell risk gate. A high-risk command never holds.
    MediumRisk,
    /// The answer covers this run only: a high-risk command always asks again.
    HighRisk,
}

impl SessionScope {
    /// True when an answer to a request with this scope can hold for the rest of
    /// the session. High risk is excluded on purpose: it always asks again.
    #[must_use]
    pub fn holds_for_session(self) -> bool {
        matches!(self, Self::Command | Self::MediumRisk)
    }
}

/// A decision together with whether the registry deadline produced it.
///
/// `decision` is exactly what the matching `request_decision*` call returns.
/// `timed_out` is true only when the deadline elapsed, so `decision` is the
/// auto-deny and nobody answered. A person's Deny has `timed_out: false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingOutcome {
    pub decision: Decision,
    pub timed_out: bool,
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
    /// Chat/room the request can be answered from, alongside `channel`.
    ///
    /// Empty means **unscoped**: the registering surface had no message context
    /// to attach — a direct TUI or CLI run. An unscoped request is deliberately
    /// NOT resolvable by a bare `ok`/`y`; it needs its request handle. See
    /// `resolve_by_basename_in`.
    ///
    /// `ShellTool` used to be permanently in that state, because it is a `Tool`
    /// and the trait carries no originating message. It now reads the turn's
    /// chat from [`TURN_SCOPE`], so a shell approval raised by a channel turn
    /// names the chat it came from.
    pub reply_target: String,
    /// Unix epoch seconds when the request was created.
    pub created_at: u64,
    /// What an answer to this request may hold for. The shell unallowed-command
    /// gate offers [`SessionScope::Command`], the shell risk gate
    /// [`SessionScope::MediumRisk`] or [`SessionScope::HighRisk`] depending on
    /// the risk it classified, and every other prompt
    /// [`SessionScope::None`] — so a surface never has to claim a scope the
    /// code does not give.
    pub session_scope: SessionScope,
}

/// A resolution event, broadcast to `subscribe_resolved` listeners when a pending
/// request is answered or auto-denied. Lets a surface (the web console) close a
/// modal it raised instead of leaving dead approve/deny buttons on screen.
#[derive(Debug, Clone)]
pub struct ResolvedInfo {
    pub id: Uuid,
    pub decision: Decision,
    /// The `reply_target` the request carried, so a listener can tell whether the
    /// resolution belongs to the turn it is serving.
    pub reply_target: String,
    /// True when the resolution was the deadline auto-deny rather than a user
    /// decision.
    pub timed_out: bool,
}

tokio::task_local! {
    /// The chat the current turn came from: `(channel, reply_target)`.
    ///
    /// `ShellTool` is a `Tool`, and the trait carries no originating message —
    /// so a shell approval used to register with both fields empty, which made
    /// it **unscoped**: answerable only by naming the command (`allow brew`),
    /// never by a bare `ok`. That was a real limitation, not a safety property
    /// on its own: `resolve_by_basename_in` already refuses when more than one
    /// request matches, so two parallel `curl` calls cannot be answered by a
    /// guess either way.
    ///
    /// A task-local rather than a `Tool` trait parameter: the tool registry is
    /// built **once** and shared across every channel and every turn, so a
    /// constructor field would be wrong for all but one caller, and widening a
    /// public trait for one tool's benefit is worse than ambient context that
    /// the one tool reads. Set by the channel dispatch around the tool loop; the
    /// loop does not spawn between there and `Tool::execute`, so it survives.
    ///
    /// Absent on the TUI and CLI paths, where there is no chat — those keep
    /// today's unscoped behaviour.
    pub static TURN_SCOPE: (String, String);

    /// The human answer a tool received while it ran, if any.
    ///
    /// The agent loop installs one of these around every tool's `execute`, so
    /// a tool that asks a human itself (shell's two gates, `pty`, `ssh`) can
    /// report the answer; the loop reads it back and writes the honest verdict
    /// into the audit trail. Without it the trail could only report what the
    /// OUTER `ApprovalManager` gate saw, and a call a human approved from
    /// inside the tool read as `not_required` — nobody had approved anything,
    /// as far as the record could tell.
    ///
    /// The value is a shared handle rather than the answer itself: a task-local
    /// is only visible while the task is being polled (`tokio`'s `scope` swaps
    /// the value in around each poll of the inner future and swaps it back
    /// afterwards), so the tool and the loop must reach the same cell through
    /// one interior-mutable handle. See [`ToolApprovalCell`].
    pub static TOOL_APPROVAL: ToolApprovalCell;

    /// Whether the current turn belongs to a guest — a sender who is not an
    /// owner.
    ///
    /// The security layer owns this marker instead of reading the approval
    /// module's `GUEST_TURN`: the dependency runs approval → security, so a
    /// security-side reader of an approval-side marker would invert it. The
    /// channel dispatch sets both from the same owner decision, so they cannot
    /// disagree.
    ///
    /// `false` outside a turn is the safe answer: the TUI, the CLI and the
    /// gateway's own operator paths are the operator's own turns.
    pub static TURN_IS_GUEST: bool;

    /// The turn's cancellation token, while a tool runs.
    ///
    /// A tool that asks a human itself (shell's two gates, `pty`, `ssh`) has to
    /// end the turn when the human says no — the model must not get to try the
    /// same call, or an alternative one, behind their back. A `Tool` carries no
    /// cancellation token, and widening that trait for one tool's benefit is
    /// worse than ambient context the shared decision path reads.
    ///
    /// The agent loop installs one of these around every tool's `execute`, next
    /// to [`TOOL_APPROVAL`], so a tool that asks inherits the behaviour without
    /// knowing about it. Absent outside a turn (a unit test, a cron path), and
    /// the shared path then does nothing.
    pub static TURN_CANCEL: CancellationToken;
}

/// End the turn after a human refused the request.
///
/// A person said no, or the deadline gave the no in their place: both end the
/// turn. A refusal nobody gave — the registry shut down under the request —
/// is not a human's no and leaves the turn alone.
///
/// A no-op when no turn is in scope: a prompt raised outside a turn has no
/// turn to end.
pub fn cancel_turn_on_human_deny() {
    let _ = TURN_CANCEL.try_with(CancellationToken::cancel);
}

/// Shared handle to the human answer one tool call received.
///
/// `None` means no tool-internal prompt was answered. The loop keeps its own
/// clone and reads the cell after the tool's future completes; the tool writes
/// into it through [`record_tool_approval`].
pub type ToolApprovalCell = Arc<Mutex<Option<ApprovalOutcome>>>;

/// A fresh cell for one tool call: no human answer yet.
#[must_use]
pub fn tool_approval_cell() -> ToolApprovalCell {
    Arc::new(Mutex::new(None))
}

/// Record the human answer a tool received while the running call was in
/// flight.
///
/// A no-op when no cell is installed: a tool can run outside the agent loop
/// (a unit test, a cron path), and there is no trail to write there.
///
/// The merge is sticky. A call may prompt more than once — a chained shell
/// command asks about each blocked basename, and the risk gate asks again
/// after that — and a refusal outranks a grant: the call records `denied`
/// when any of its prompts was refused, `granted` when they were all
/// answered yes.
pub fn record_tool_approval(outcome: ApprovalOutcome) {
    let _ = TOOL_APPROVAL.try_with(|cell| {
        let mut slot = cell.lock();
        match outcome {
            ApprovalOutcome::Denied => *slot = Some(ApprovalOutcome::Denied),
            ApprovalOutcome::Granted => {
                if *slot != Some(ApprovalOutcome::Denied) {
                    *slot = Some(ApprovalOutcome::Granted);
                }
            }
            // Nobody was asked. That is not a human answer, so it is not
            // recorded — the cell keeps saying "no prompt was answered".
            ApprovalOutcome::NotRequired => {}
        }
    });
}

/// A cell for a call whose outer gate has already decided.
///
/// When the outer `ApprovalManager` gate put the call in front of a human, got
/// a yes, and the prompt showed every argument in full, the cell starts out
/// holding that answer, so a tool that asks again can see it was asked about
/// already. A yes to a prompt that cut the arguments covers only what it
/// showed, so it leaves the cell empty, as does any other outer outcome.
#[must_use]
pub fn tool_approval_cell_after(
    outer: ApprovalOutcome,
    args_shown_in_full: bool,
) -> ToolApprovalCell {
    let answer = match outer {
        ApprovalOutcome::Granted if args_shown_in_full => Some(ApprovalOutcome::Granted),
        _ => None,
    };
    Arc::new(Mutex::new(answer))
}

/// True when a human has already approved the call that is running.
///
/// A tool that asks a human itself (`pty`, `ssh`) reads this before it asks,
/// and does not ask again when the outer gate has already put the same call in
/// front of one. A refusal never reaches the tool, so a grant is the only
/// answer this can hold.
#[must_use]
pub fn call_already_approved() -> bool {
    TOOL_APPROVAL
        .try_with(|cell| *cell.lock() == Some(ApprovalOutcome::Granted))
        .unwrap_or(false)
}

/// The human answer recorded in `cell`, if any.
#[must_use]
pub fn tool_approval_outcome(cell: &ToolApprovalCell) -> Option<ApprovalOutcome> {
    *cell.lock()
}

/// What the audit trail says about who let this call through.
///
/// Two gates can ask a human: the OUTER `ApprovalManager` (the caller knows
/// its verdict and passes it in) and the tool itself (shell's basename and
/// risk gates, `pty`, `ssh` — the tool writes its answer into the cell). A
/// refusal inside the tool outranks everything: the call did not run, so no
/// outer grant can make it read as approved. A grant inside the tool means a
/// person really answered, so the line says `granted` even when the outer
/// gate never asked. A call nobody was asked about keeps the outer outcome.
///
/// The rule lives beside the cell rather than in the agent loop, so the loop
/// keeps exactly one place that may write `Granted`: the branch that asked a
/// human (`only_the_branch_that_asked_a_human_may_record_a_granted_approval`
/// in `src/agent/loop_.rs`). This function names `Granted` only when the cell
/// holds it, and only a tool that asked a human can put it there.
#[must_use]
pub fn merge_tool_approval_outcome(
    outer: ApprovalOutcome,
    cell: &ToolApprovalCell,
) -> ApprovalOutcome {
    match tool_approval_outcome(cell) {
        Some(ApprovalOutcome::Denied) => ApprovalOutcome::Denied,
        Some(ApprovalOutcome::Granted) => ApprovalOutcome::Granted,
        _ => outer,
    }
}

/// The current turn's `(channel, reply_target)`, or empty strings when there is
/// none — a direct CLI or TUI run.
#[must_use]
pub fn current_turn_scope() -> (String, String) {
    TURN_SCOPE
        .try_with(|(channel, reply_target)| (channel.clone(), reply_target.clone()))
        .unwrap_or_default()
}

/// True when the current task runs a guest's turn. See [`TURN_IS_GUEST`].
#[must_use]
pub fn current_turn_is_guest() -> bool {
    TURN_IS_GUEST
        .try_with(|is_guest| *is_guest)
        .unwrap_or(false)
}

impl PendingRequest {
    fn new_with_id(
        id: Uuid,
        basename: String,
        full_command: String,
        channel: String,
        reply_target: String,
    ) -> Self {
        Self {
            id,
            basename,
            full_command,
            channel,
            reply_target,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            session_scope: SessionScope::None,
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
    /// Resolution notifications: a pending request was answered or auto-denied.
    resolved_tx: broadcast::Sender<ResolvedInfo>,
    /// Optional auto-deny timeout. `None` waits forever — matches CC's
    /// pause semantics for the TUI surface. Tests + channels that
    /// genuinely want a deadline pass `Some(Duration::…)` via
    /// `PendingApprovals::new`.
    timeout: Option<Duration>,
}

/// RAII cleanup for one pending request. Removes its `waiting` + `snapshot`
/// entries on drop, so a request whose awaiting future is cancelled (dropped
/// mid-`.await`, e.g. by a caller's `tokio::select!`) never leaks a phantom
/// entry — which would otherwise break `resolve_by_basename`'s uniqueness
/// check. Also covers the normal-return path.
struct Cleanup {
    inner: Arc<Inner>,
    id: Uuid,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // Two separate lock statements — the guards never overlap, so this
        // can't deadlock against `resolve`/`resolve_by_basename`.
        self.inner.waiting.lock().remove(&self.id);
        self.inner.snapshot.lock().remove(&self.id);
    }
}

impl PendingApprovals {
    /// Create a registry with the given decision timeout. `None` waits
    /// indefinitely for an explicit decision.
    pub fn new(timeout: Option<Duration>) -> Self {
        let (notify_tx, _) = broadcast::channel(32);
        let (resolved_tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(Inner {
                waiting: Mutex::new(HashMap::new()),
                snapshot: Mutex::new(HashMap::new()),
                notify_tx,
                resolved_tx,
                timeout,
            }),
        }
    }

    /// Subscribe to new-request notifications. Returns a fresh
    /// `broadcast::Receiver` — each subscriber gets its own copy.
    pub fn subscribe(&self) -> broadcast::Receiver<PendingRequest> {
        self.inner.notify_tx.subscribe()
    }

    /// Subscribe to resolution notifications (answered or auto-denied). Each
    /// subscriber gets its own `broadcast::Receiver`.
    pub fn subscribe_resolved(&self) -> broadcast::Receiver<ResolvedInfo> {
        self.inner.resolved_tx.subscribe()
    }

    /// Snapshot of currently-pending requests.
    pub fn list(&self) -> Vec<PendingRequest> {
        let snap = self.inner.snapshot.lock();
        let mut v: Vec<PendingRequest> = snap.values().cloned().collect();
        v.sort_by_key(|r| r.created_at);
        v
    }

    /// True when `id` is still awaiting a decision.
    ///
    /// `resolve` removes the request from the waiting map at once, while the
    /// snapshot entry survives until the awaiting future unwinds — so the
    /// snapshot can still report a request that was already answered. This
    /// reads the waiting map, which is the authoritative "still unanswered"
    /// state.
    pub fn is_pending(&self, id: Uuid) -> bool {
        self.inner.waiting.lock().contains_key(&id)
    }

    /// Block until the user decides on this basename. When `timeout`
    /// is `Some`, auto-denies after the deadline; when `None`, waits
    /// indefinitely (CC-style — the prompt sits until the user acts
    /// or the process shuts down).
    pub async fn request_decision(
        &self,
        basename: impl Into<String>,
        full_command: impl Into<String>,
        channel: impl Into<String>,
    ) -> Decision {
        self.request_decision_in(
            Uuid::new_v4(),
            basename,
            full_command,
            channel,
            String::new(),
        )
        .await
    }

    /// As [`request_decision`](Self::request_decision), but records the chat the
    /// request can be answered from so a bare `ok` in a *different* chat cannot
    /// resolve it.
    /// The caller supplies `id` so it can show the request's handle in the
    /// prompt it posts *before* awaiting the decision. The prompt has to name
    /// the request it is about, and it is sent first.
    pub async fn request_decision_in(
        &self,
        id: Uuid,
        basename: impl Into<String>,
        full_command: impl Into<String>,
        channel: impl Into<String>,
        reply_target: impl Into<String>,
    ) -> Decision {
        self.request_decision_in_outcome(id, basename, full_command, channel, reply_target)
            .await
            .decision
    }

    /// As [`request_decision_in`](Self::request_decision_in), but also reports
    /// whether the deadline produced the decision. Callers that must tell "no
    /// answer in time" from a person's Deny use this; the decision is the same.
    pub async fn request_decision_in_outcome(
        &self,
        id: Uuid,
        basename: impl Into<String>,
        full_command: impl Into<String>,
        channel: impl Into<String>,
        reply_target: impl Into<String>,
    ) -> PendingOutcome {
        self.request_decision_with(
            id,
            basename,
            full_command,
            channel,
            reply_target,
            SessionScope::None,
        )
        .await
    }

    /// As [`request_decision_in`](Self::request_decision_in), for the shell
    /// unallowed-command gate only: its "Always" answer is recorded as a command
    /// grant and holds for the rest of the conversation, so the request carries
    /// [`SessionScope::Command`]. Every other caller keeps the deny default.
    pub async fn request_decision_in_always(
        &self,
        id: Uuid,
        basename: impl Into<String>,
        full_command: impl Into<String>,
        channel: impl Into<String>,
        reply_target: impl Into<String>,
    ) -> Decision {
        self.request_decision_with(
            id,
            basename,
            full_command,
            channel,
            reply_target,
            SessionScope::Command,
        )
        .await
        .decision
    }

    /// As [`request_decision_in`](Self::request_decision_in), but the request
    /// states what an answer to it may hold for. The shell risk gate uses this:
    /// a medium-risk command may hold for the session, a high-risk one never
    /// does, so the scope is decided by the gate that raises the request and
    /// every surface can report it truthfully.
    pub async fn request_decision_in_scope(
        &self,
        id: Uuid,
        basename: impl Into<String>,
        full_command: impl Into<String>,
        channel: impl Into<String>,
        reply_target: impl Into<String>,
        scope: SessionScope,
    ) -> Decision {
        self.request_decision_with(id, basename, full_command, channel, reply_target, scope)
            .await
            .decision
    }

    async fn request_decision_with(
        &self,
        id: Uuid,
        basename: impl Into<String>,
        full_command: impl Into<String>,
        channel: impl Into<String>,
        reply_target: impl Into<String>,
        scope: SessionScope,
    ) -> PendingOutcome {
        let mut request = PendingRequest::new_with_id(
            id,
            basename.into(),
            full_command.into(),
            channel.into(),
            reply_target.into(),
        );
        request.session_scope = scope;
        let id = request.id;
        // Kept for the resolution broadcast on the auto-deny path (the snapshot
        // entry is gone by the time the deadline fires and cleanup runs).
        let reply_target_for_resolve = request.reply_target.clone();

        let (tx, rx) = oneshot::channel();
        {
            self.inner.waiting.lock().insert(id, tx);
            self.inner.snapshot.lock().insert(id, request.clone());
        }
        // Remove both entries when we leave this scope — on a normal return AND
        // when the future is dropped by a caller cancelling us mid-wait.
        let _cleanup = Cleanup {
            inner: Arc::clone(&self.inner),
            id,
        };
        // Ignore send error: no live subscribers is fine.
        let _ = self.inner.notify_tx.send(request);

        // `_cleanup` drops after this value is produced, removing both entries.
        //
        // `human_deny` says who produced a no: a person answered, or the
        // deadline gave the no in a person's place. A no the registry produced
        // on its own (it shut down under the request) is nobody's no — it
        // leaves the turn alone.
        let (outcome, human_deny) = match self.inner.timeout {
            Some(d) => match tokio::time::timeout(d, rx).await {
                Ok(Ok(decision)) => (
                    PendingOutcome {
                        decision,
                        timed_out: false,
                    },
                    decision == Decision::Deny,
                ),
                // Sender dropped (registry shut down) — deny, no broadcast.
                Ok(Err(_)) => (
                    PendingOutcome {
                        decision: Decision::Deny,
                        timed_out: false,
                    },
                    false,
                ),
                // Deadline elapsed — auto-deny AND tell resolved-subscribers so a
                // web modal can close instead of leaving dead buttons.
                Err(_elapsed) => {
                    let _ = self.inner.resolved_tx.send(ResolvedInfo {
                        id,
                        decision: Decision::Deny,
                        reply_target: reply_target_for_resolve.clone(),
                        timed_out: true,
                    });
                    (
                        PendingOutcome {
                            decision: Decision::Deny,
                            timed_out: true,
                        },
                        true,
                    )
                }
            },
            None => match rx.await {
                Ok(decision) => (
                    PendingOutcome {
                        decision,
                        timed_out: false,
                    },
                    decision == Decision::Deny,
                ),
                // Oneshot sender dropped (registry shut down) — deny.
                Err(_) => (
                    PendingOutcome {
                        decision: Decision::Deny,
                        timed_out: false,
                    },
                    false,
                ),
            },
        };
        if human_deny {
            // A person said no, or the deadline gave the no in their place:
            // end the turn, so the model does not get to try the same call
            // again. The tool still returns its failed result — the model sees
            // the call as denied, it just never gets to act on that.
            cancel_turn_on_human_deny();
        }
        outcome
    }

    /// Resolve a pending request. Returns `true` if a sender was
    /// present and accepted the decision, `false` if the id was not
    /// pending (already resolved, timed out, or never existed).
    pub fn resolve(&self, id: Uuid, decision: Decision) -> bool {
        let tx = self.inner.waiting.lock().remove(&id);
        match tx {
            Some(tx) => {
                let ok = tx.send(decision).is_ok();
                if ok {
                    // The snapshot entry is still present (cleanup runs after the
                    // awaiting future returns), so read the reply_target for the
                    // resolution broadcast.
                    let reply_target = self
                        .inner
                        .snapshot
                        .lock()
                        .get(&id)
                        .map(|r| r.reply_target.clone())
                        .unwrap_or_default();
                    let _ = self.inner.resolved_tx.send(ResolvedInfo {
                        id,
                        decision,
                        reply_target,
                        timed_out: false,
                    });
                }
                ok
            }
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

    /// The scope of the pending request matched by basename, when exactly one
    /// request carries that basename.
    ///
    /// A surface that answers by basename has to know what the answer will hold
    /// for *before* it resolves — the scope is gone once the request is
    /// resolved. More than one match is `None`, the same refusal
    /// [`resolve_by_basename`](Self::resolve_by_basename) makes: a surface must
    /// never claim a scope for a request it cannot name.
    pub fn scope_by_basename(&self, basename: &str) -> Option<SessionScope> {
        let snap = self.inner.snapshot.lock();
        let mut scopes = snap
            .values()
            .filter(|r| r.basename == basename)
            .map(|r| r.session_scope);
        let first = scopes.next()?;
        if scopes.next().is_some() {
            return None;
        }
        Some(first)
    }

    /// Resolve a pending request matched by basename **within one chat**.
    ///
    /// This is what a bare `ok`/`y` from a channel goes through. Matching on the
    /// basename alone let an approval posted into chat A be answered from chat B
    /// on another channel entirely — no identity spoofing needed, because
    /// resolution consulted neither the request id nor the origin the request
    /// already carried.
    ///
    /// A request with an empty `reply_target` is **unscoped** and never matches
    /// here: it has no chat to compare against, so the caller must name it by
    /// handle instead of guessing.
    pub fn resolve_by_basename_in(
        &self,
        basename: &str,
        channel: &str,
        reply_target: &str,
        decision: Decision,
    ) -> Option<Uuid> {
        if reply_target.is_empty() {
            return None;
        }
        let id = {
            let snap = self.inner.snapshot.lock();
            let matches: Vec<Uuid> = snap
                .values()
                .filter(|r| {
                    r.basename == basename
                        && r.channel == channel
                        && !r.reply_target.is_empty()
                        && r.reply_target == reply_target
                })
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

    /// Resolve the request whose id starts with `handle` (the short form shown
    /// in the approval prompt). Returns the full id when exactly one matched.
    ///
    /// Scoped resolution cannot help a request that has no chat to compare
    /// against, so naming it explicitly is the answer for those.
    pub fn resolve_by_handle(&self, handle: &str, decision: Decision) -> Option<Uuid> {
        let handle = handle.trim().to_ascii_lowercase();
        if handle.len() < 4 {
            return None;
        }
        let id = {
            let snap = self.inner.snapshot.lock();
            let matches: Vec<Uuid> = snap
                .values()
                .filter(|r| r.id.simple().to_string().starts_with(&handle))
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

    /// The auto-deny deadline this registry was built with, if any. Surfaces
    /// exist that print it, and a printed deadline must come from here rather
    /// than from a literal that can drift out of step.
    pub fn timeout(&self) -> Option<Duration> {
        self.inner.timeout
    }

    /// The short handle shown to an operator for `id`.
    pub fn handle_for(id: Uuid) -> String {
        id.simple().to_string()[..REQUEST_HANDLE_LEN].to_string()
    }
}

/// Characters of the request uuid shown in approval prompts. Long enough to be
/// unambiguous in a queue an operator can actually read, short enough to retype
/// from a phone.
pub const REQUEST_HANDLE_LEN: usize = 6;

impl Default for PendingApprovals {
    /// No timeout by default — the prompt waits indefinitely for an
    /// explicit user decision. Matches Claude Code's pause semantics:
    /// the agent doesn't make progress until the user acts. The 60s
    /// auto-deny that lived here through v0.6.50 caused the LLM to
    /// re-enter the loop with a "denied" error and explore alternative
    /// commands, defeating the "deny cancels turn" UX. Non-TUI surfaces
    /// (Telegram, webhook) that genuinely need a deadline construct
    /// the registry explicitly with `PendingApprovals::new(Some(d))`.
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deadline's auto-deny is reported as `timed_out`. A person's Deny is
    /// the same `Decision::Deny` but is not reported as timed out.
    #[tokio::test]
    async fn outcome_marks_only_the_deadline_as_timed_out() {
        let registry = PendingApprovals::new(Some(Duration::from_millis(30)));
        let timed = registry
            .request_decision_in_outcome(Uuid::new_v4(), "shell", "ls", "tui", "")
            .await;
        assert_eq!(timed.decision, Decision::Deny);
        assert!(timed.timed_out, "the deadline must be observable");

        let registry = PendingApprovals::new(Some(Duration::from_secs(10)));
        let registry2 = registry.clone();
        let task = tokio::spawn(async move {
            registry2
                .request_decision_in_outcome(Uuid::new_v4(), "shell", "ls", "tui", "")
                .await
        });
        await_registered(&registry).await;
        let id = registry.list()[0].id;
        assert!(registry.resolve(id, Decision::Deny));
        let answered = task.await.unwrap();
        assert_eq!(answered.decision, Decision::Deny);
        assert!(!answered.timed_out, "a person's Deny is not a timeout");
    }

    async fn await_registered(registry: &PendingApprovals) {
        for _ in 0..100 {
            if !registry.list().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("request never registered");
    }

    #[tokio::test]
    async fn resolve_returns_decision() {
        let registry = PendingApprovals::new(Some(Duration::from_secs(10)));
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
    async fn dropping_request_future_cleans_up_registry() {
        // A caller cancelling the approval wait drops the request_decision
        // future mid-`.await`. The RAII cleanup must remove the pending entry —
        // otherwise a phantom entry leaks and breaks resolve_by_basename.
        let registry = PendingApprovals::new(None); // waits forever
        let r = registry.clone();
        let task =
            tokio::spawn(async move { r.request_decision("brew", "brew --version", "tui").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(registry.list().len(), 1, "request should have registered");

        // Drop the awaiting future (models a tokio::select! losing the race).
        task.abort();
        let _ = task.await;
        assert!(
            registry.list().is_empty(),
            "cancelled request must be cleaned up, not leaked"
        );

        // The `waiting` map is clear too: a fresh unique request resolves.
        let r2 = registry.clone();
        let t2 =
            tokio::spawn(async move { r2.request_decision("brew", "brew --version", "tui").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(registry
            .resolve_by_basename("brew", Decision::Once)
            .is_some());
        assert_eq!(t2.await.unwrap(), Decision::Once);
    }

    #[tokio::test]
    async fn timeout_yields_deny() {
        let registry = PendingApprovals::new(Some(Duration::from_millis(50)));
        let decision = registry
            .request_decision("brew", "brew --version", "tui")
            .await;
        assert_eq!(decision, Decision::Deny);
        assert!(registry.list().is_empty());
    }

    #[tokio::test]
    async fn no_timeout_waits_for_explicit_decision() {
        // CC-style: prompt waits indefinitely until the user acts.
        // We simulate by resolving after 50ms — without my fix the
        // request would auto-deny at 60s and this test would hang for
        // 60s (or, with the prior default of 5 min, much longer).
        let registry = PendingApprovals::new(None);
        let r = registry.clone();
        let task =
            tokio::spawn(async move { r.request_decision("brew", "brew --version", "tui").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(registry
            .resolve_by_basename("brew", Decision::Session)
            .is_some());
        assert_eq!(task.await.unwrap(), Decision::Session);
    }

    #[tokio::test]
    async fn default_registry_has_no_timeout() {
        // Sanity: PendingApprovals::default() is the TUI-facing
        // constructor; it must not auto-deny.
        let registry = PendingApprovals::default();
        let r = registry.clone();
        let task =
            tokio::spawn(async move { r.request_decision("brew", "brew --version", "tui").await });
        // Wait past where the OLD 60s default would have fired. If
        // somebody ever flips the default back, this test would hang
        // for 60s+ then auto-deny — failing the assert below.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            registry.list().len(),
            1,
            "default registry must not have auto-denied"
        );
        registry.resolve_by_basename("brew", Decision::Session);
        assert_eq!(task.await.unwrap(), Decision::Session);
    }

    #[tokio::test]
    async fn resolve_by_basename_unique_match() {
        let registry = PendingApprovals::new(Some(Duration::from_secs(10)));
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
        let registry = PendingApprovals::new(Some(Duration::from_secs(10)));
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
        let registry = PendingApprovals::new(Some(Duration::from_secs(10)));
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
        let registry = PendingApprovals::new(Some(Duration::from_secs(10)));
        assert!(!registry.resolve(Uuid::new_v4(), Decision::Once));
    }

    #[tokio::test]
    async fn resolve_broadcasts_to_resolved_subscribers() {
        // A resolution must notify `subscribe_resolved` listeners (the web
        // forwarder) so a modal can close, carrying the request's reply_target.
        let registry = PendingApprovals::new(Some(Duration::from_secs(10)));
        let mut resolved = registry.subscribe_resolved();
        let id = Uuid::new_v4();
        let r = registry.clone();
        let task = tokio::spawn(async move {
            r.request_decision_in(id, "git", "git status", "console", "turn-1")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(registry.resolve(id, Decision::Once));

        let info = tokio::time::timeout(Duration::from_millis(100), resolved.recv())
            .await
            .expect("resolution within deadline")
            .expect("recv ok");
        assert_eq!(info.id, id);
        assert_eq!(info.decision, Decision::Once);
        assert_eq!(info.reply_target, "turn-1");
        assert!(!info.timed_out);
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    #[tokio::test]
    async fn timeout_broadcasts_deny_as_timed_out() {
        // The deadline auto-deny must also broadcast, flagged timed_out, so the
        // browser closes the modal instead of leaving dead buttons.
        let registry = PendingApprovals::new(Some(Duration::from_millis(50)));
        let mut resolved = registry.subscribe_resolved();
        let id = Uuid::new_v4();
        let decision = registry
            .request_decision_in(id, "git", "git status", "console", "turn-2")
            .await;
        assert_eq!(decision, Decision::Deny);

        let info = tokio::time::timeout(Duration::from_millis(100), resolved.recv())
            .await
            .expect("resolution within deadline")
            .expect("recv ok");
        assert_eq!(info.id, id);
        assert_eq!(info.decision, Decision::Deny);
        assert_eq!(info.reply_target, "turn-2");
        assert!(info.timed_out);
    }

    /// The merge rule the audit trail depends on: a refusal inside the tool
    /// outranks an outer grant, a grant inside the tool stands on its own, and
    /// a call nobody was asked about keeps the outer outcome.
    #[test]
    fn the_merged_outcome_follows_the_cell_and_the_outer_gate() {
        let cell = tool_approval_cell();
        assert_eq!(
            merge_tool_approval_outcome(ApprovalOutcome::NotRequired, &cell),
            ApprovalOutcome::NotRequired
        );
        assert_eq!(
            merge_tool_approval_outcome(ApprovalOutcome::Granted, &cell),
            ApprovalOutcome::Granted
        );

        *cell.lock() = Some(ApprovalOutcome::Granted);
        assert_eq!(
            merge_tool_approval_outcome(ApprovalOutcome::NotRequired, &cell),
            ApprovalOutcome::Granted
        );

        *cell.lock() = Some(ApprovalOutcome::Denied);
        assert_eq!(
            merge_tool_approval_outcome(ApprovalOutcome::Granted, &cell),
            ApprovalOutcome::Denied
        );
    }

    /// A call may prompt more than once (a chained shell command asks about
    /// each blocked basename, then the risk gate asks again). The recorded
    /// answer is sticky: a refusal outranks a grant, and "nobody was asked" is
    /// never recorded as an answer.
    #[tokio::test]
    async fn the_recorded_answer_is_sticky_across_several_prompts() {
        let cell = tool_approval_cell();
        TOOL_APPROVAL
            .scope(Arc::clone(&cell), async {
                record_tool_approval(ApprovalOutcome::Granted);
                record_tool_approval(ApprovalOutcome::NotRequired);
                record_tool_approval(ApprovalOutcome::Denied);
                record_tool_approval(ApprovalOutcome::Granted);
            })
            .await;
        assert_eq!(tool_approval_outcome(&cell), Some(ApprovalOutcome::Denied));
    }
}
