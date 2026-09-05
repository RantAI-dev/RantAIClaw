# Plan 033: Web console hangs on a Supervised shell command needing command-level approval (Layer-B has no web resolver)

> **Context**: Found during live-testing of the web-console approval parity work
> (PR #308). On the web console in a Supervised preset (Manual/Smart), when the
> agent runs a `shell` command that is NOT on the boot allowlist, the turn
> **hangs** (no `tool_call_end`) until the gateway's 120 s request timeout kills
> it — the agent appears frozen for two minutes, then the turn dies. Approving
> the `shell` *tool* in the in-browser modal does not help: that modal is the
> **Layer-A** `ApprovalManager` gate (tool-name), a different registry from the
> shell tool's own **Layer-B** command gate.
>
> This is pre-existing (not introduced by #308) and out of scope for the parity
> PRs, but it makes the web console's Supervised shell UX effectively broken.
>
> **Executor note**: Self-contained. Repo verification baseline —
> `cargo fmt --all -- --check` · `cargo clippy --all-targets -- -D warnings` ·
> `cargo test`. Disk-constrained box: prefer a shared warm `CARGO_TARGET_DIR` +
> `cargo test --lib` + `touch`-ing changed files. Gateway/SSE code is `src/gateway/**`
> (high-risk tier). Every fix ships a repro test that FAILS before / PASSES after.
>
> **Branch**: `fix/web-shell-command-approval` (non-`main`).
> **Risk**: MEDIUM–HIGH (`src/gateway/**` + agent construction; security-sensitive
> — must stay fail-closed). No exposure-boundary change; schema impact depends on
> the option chosen (Option A/B2 = none; B1 = none).

## Baseline evidence (confirmed against `feat/web-approval-parity`, 2026-07-23)

The agent has **two independent approval layers**, and the web console wires a
resolver for only one:

- **Layer-A — `ApprovalManager`** (tool-name gate). The console sets this via
  `agent.set_approval(Some(manager), Some(WebModalApprovalBackend))`
  (`src/gateway/api_v1.rs` `agent_chat_stream`). Its registry is
  `state.web_approvals`; the browser modal + `POST /api/v1/approvals/{id}`
  resolve it. **This works.**
- **Layer-B — the shell tool's own command gate** (`ShellTool::execute`,
  `src/tools/shell.rs:302-381`). In Supervised, `validate_command_execution`
  rejects a non-allowlisted basename, and the cascade calls
  `self.security.pending().request_decision(basename, …).await` to ask the user.

The Layer-B registry is attached to **every** agent at construction:

```
// src/agent/agent.rs:388-391
// Bind the async-approval registry to the policy so the shell tool can ask the
// user (via whichever UI is subscribed) when it hits an allowlist miss …
let pending = Arc::new(crate::security::PendingApprovals::default());   // NO timeout
security.set_pending(pending);
```

`PendingApprovals::default()` has **no timeout** (`src/security/pending.rs`
`Default = new(None)`), so `request_decision` waits **indefinitely** for a
resolver. The resolvers that exist:

- **TUI** — subscribes (`pending_approvals_rx`) and resolves via Y/A/N keys.
- **Channels** — wire `ChatRelayApprovalBackend` / `approval_relay`
  (`src/channels/approval_relay.rs`) and resolve via `/approve` chat replies.
- **Web console** — wires **neither**. `agent_chat_stream` never subscribes to
  `security.pending()` and never resolves it. The only web resolver is the
  Layer-A modal, which resolves `web_approvals`, a **different** registry.

Result: a Supervised web shell command → Layer-B `request_decision` blocks with
no resolver → the turn hangs until `REQUEST_TIMEOUT_SECS = 120`
(`src/gateway/mod.rs:53`) drops the SSE request (`CancelOnDrop` then cancels the
turn). **Live-reproduced**: modal-approve `shell` → `tool_call_start` → no
`tool_call_end` (a *deny* completes fine because it cancels the turn before
Layer-B runs).

Reachability: Supervised preset (Manual/Smart) + a shell command not on
`autonomy.allowed_commands`. Not reachable in the default `full`/Off preset
(`is_command_allowed` returns true) or Strict (the `shell` tool is dropped).

## Options (pick one; A or B2 as the safe fix, B1 for full parity)

### Option A — give the console's Layer-B registry a timeout (smallest, interim)
Attach a *bounded* `PendingApprovals::new(Some(d))` to the console agent's
security instead of the no-timeout default, so the command auto-denies after `d`
instead of hanging. Since `agent.rs:390` is shared by all surfaces (the TUI
*wants* no timeout), do this at the console seam: after `from_config`, in
`agent_chat_stream`, `security.set_pending(Arc::new(PendingApprovals::new(Some(…))))`
(needs an `Agent`/`SecurityPolicy` accessor to re-bind, or a builder hook).
- Pro: tiny; removes the hang. Con: still a wait-then-deny; the web user can't
  actually *approve* the command (must edit the allowlist). Not real parity.

### Option B2 — fail-closed immediately on the console (safe, recommended Phase 1)
When the surface has a Layer-A `ApprovalManager` but no Layer-B resolver (the
console), the shell tool should **not** block on `request_decision` at all — it
should return the existing hard-block error + `BLOCKED_COMMAND_REMEDIATION`
(`src/tools/shell.rs`) immediately, telling the operator to add the command to
`autonomy.allowed_commands` (or raise autonomy). Implement by making the console
**not attach** a Layer-B registry (bind `None`), so `security.pending()` is
`None` and the cascade takes its existing no-registry hard-block branch
(`shell.rs:329-341`). Deterministic, fail-closed, no hang.
- Pro: safe, minimal, uses an existing code path. Con: no in-modal command
  approval on the web (parity gap remains, but honestly surfaced).

### Option B1 — surface command approval in the web modal (full parity, Phase 2)
Make the console subscribe to the Layer-B registry and emit an
`AgentEvent::ApprovalRequest` (command-level) over the SSE, resolvable via the
same `POST /api/v1/approvals/{id}` — mirroring the TUI's `pending_approvals_rx`
and the channels' relay. The web user then approves the *command* (`ls`) in a
modal, just like the TUI's Y/A/N. Reuse `web_approvals` (or a second registry)
as the Layer-B resolver and add an SSE emitter + a `basename`-scoped resolve.
- Pro: true parity; the web can approve arbitrary commands. Con: most work; two
  modals (tool then command) unless unified; careful cancel-safety.

**Recommendation**: ship **B2** first (kills the hang, fail-closed, small), then
**B1** as a follow-up if in-browser command approval is wanted. Do **not** "skip
Layer-B when Layer-A approved" — Layer-A approves the tool category, Layer-B the
specific command; skipping it would let the model run any command after one
tool-approval (security regression).

## Repro test (write first, both phases)
A gateway/agent-level test: console-style agent (Layer-A `ApprovalManager` set,
Supervised, empty `allowed_commands`, `web_approvals` for Layer-A) runs a `shell`
tool call for a non-allowlisted command with a bounded overall deadline.
- Before: the tool call does not complete within the deadline (hang).
- After B2: it returns a non-blocking "not allowed" error (fail-closed) well
  under the deadline.
- After B1: an `ApprovalRequest` SSE event carries the command; resolving it
  Yes/Always runs it, No denies.

## Non-goals
- No change to the TUI (its Layer-B resolver already works) or to the channels.
- No relaxation of the command allowlist or the deny-fails-closed posture.
- No exposure-boundary or schema change.

## Risk & rollback
- Security-sensitive (`src/gateway/**` + agent wiring) — keep fail-closed; a
  denied/blocked command must never fall through to execution. Single-commit,
  revertible per option. B2 is the low-risk path; B1 needs cancel-safety review
  (dropping the SSE must clean up the Layer-B pending entry, like the Layer-A
  path already does).
