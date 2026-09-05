# Plan 220: Scope web-console tool approvals to the stream that raised them, tell the browser when one resolves, and stop "Always" grants leaking

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 8503328..HEAD -- src/gateway/api_v1.rs src/gateway/web_approval.rs src/gateway/config_api.rs src/security/pending.rs src/agent/events.rs src/approval/ src/tools/shell.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED — touches the shared `PendingApprovals` registry that the channel relay also uses; every step has its own test and its own commit so a bad step can be reverted alone.
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `8503328`, 2026-08-24
- **Branch**: `fix/web-approval-scoping`
- **One PR**: this whole plan is one PR. Commit per step (see "Git workflow").

## Why this matters

The web console pauses an agent turn on a tool that needs approval and shows a modal. Today that modal traffic is **process-global**: every open SSE chat stream subscribes to the one `PendingApprovals` registry and forwards *any* shell approval request to its own browser, using the shell command's basename (`git`, `npm`, …) as the id. So browser A sees browser B's command text, either browser can approve the other's call, and two concurrent turns waiting on the same basename can be resolved by neither (they stall until the 300 s auto-deny). Separately, when the 300 s deadline auto-denies, nothing tells the browser — the modal stays open with buttons that now 404. And the per-session "Always" grants are keyed by the raw `session_id` string with no empty-string filter, are only revoked by one of the three autonomy-tightening paths, and survive a session delete.

After this plan: each stream only sees and can only resolve its own requests (UUID ids, no basename guessing), the browser receives an `approval_resolved` event when a request is answered or expires, and grants are keyed safely and revoked wherever autonomy is tightened.

## Current state

Files:

- `src/gateway/api_v1.rs` — SSE handler `agent_chat_stream` (~line 573); the shell-approval forwarder at ~660–683; `resolve_approval` handler at ~436–453; `history_session_id` grant seeding at ~634 and harvest at ~705; `sessions_delete` at ~1024.
- `src/gateway/web_approval.rs` — `WebModalApprovalBackend::decide` (Layer-A modal, ~62–100), `resolve` (~109–118), `SESSION_GRANTS` + `session_granted_tools` / `record_session_grants` / `clear_all_session_grants` (~118–170), tests (~280–310).
- `src/security/pending.rs` — `PendingRequest` (44+), `TURN_SCOPE` task-local + `current_turn_scope()` (~89–100), `request_decision` / `request_decision_in` (~209–262), `resolve(Uuid, Decision)` (~281), `resolve_by_basename` (~293), `resolve_by_basename_in` (~323).
- `src/tools/shell.rs` — shell tool registers Layer-B approvals via `request_decision_in(uuid, basename, command, channel, reply_target)` with `(channel, reply_target) = current_turn_scope()` (~336–352).
- `src/channels/dispatch.rs:574-590` — the channel path sets `TURN_SCOPE.scope((msg.channel, msg.reply_target), …)` around the tool loop. **This is the pattern to copy.**
- `src/gateway/config_api.rs:414-421` — the only caller of `clear_all_session_grants()`.
- `src/approval/policy_writer.rs` — `apply_preset_to_config` is the shared tightening entry point used by CLI (`src/main.rs:1812`), TUI (`src/tui/app.rs:694`, `src/tui/commands/autonomy.rs:136`).
- `src/agent/events.rs` — `AgentEvent` enum (`ApprovalRequest` at ~38).

### The forwarder today (`src/gateway/api_v1.rs:657-683`)

```rust
                    if let Some(sec) = agent.security() {
                        sec.set_pending(web_approvals.clone());
                    }
                    let mut shell_rx = web_approvals.subscribe();
                    let fwd_tx = events_tx.clone();
                    shell_approval_forwarder = Some(tokio::spawn(async move {
                        loop {
                            match shell_rx.recv().await {
                                Ok(req) if req.channel.is_empty() => {
                                    let _ = fwd_tx
                                        .send(crate::agent::AgentEvent::ApprovalRequest {
                                            id: req.basename.clone(),
                                            tool: "shell".to_string(),
                                            args: serde_json::json!({
                                                "command": req.full_command,
                                            }),
                                        })
                                        .await;
                                }
                                // Layer-A (non-shell) requests emit their own
                                // modal; a lagged receiver just skips ahead.
                                Ok(_)
                                | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }));
```

`web_approvals` is the single `Arc<PendingApprovals>` built once in `src/gateway/mod.rs:771-773` (`PendingApprovals::new(Some(Duration::from_secs(300)))`). `sec.set_pending(web_approvals.clone())` points every per-request agent at that one registry.

### Why the shell request has an empty channel

`src/tools/shell.rs:340` reads `crate::security::current_turn_scope()`; the only writer of `TURN_SCOPE` is `src/channels/dispatch.rs:580`. The gateway SSE path never enters a scope, so the request registers with `channel == ""` and `reply_target == ""`.

### Resolution today (`src/gateway/web_approval.rs:109-118`)

```rust
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
```

and `resolve_by_basename` (`src/security/pending.rs:293-305`) returns `None` when more than one pending request shares the basename. `PendingApprovals::resolve(&self, id: Uuid, decision) -> bool` (`pending.rs:281`) already exists and is what we will use.

### Layer-A modal today (`src/gateway/web_approval.rs:62-85`)

```rust
    async fn decide(&self, _mgr: &ApprovalManager, request: &ApprovalRequest) -> ApprovalResponse {
        let id = Uuid::new_v4().to_string();
        // Tell the browser to show the modal. ...
        if self.events.send(AgentEvent::ApprovalRequest { id: id.clone(), tool: ..., args: ... }).await.is_err() {
            return ApprovalResponse::No;
        }
        // ... The id sits in the `basename` slot so `resolve_by_basename(id, …)` is unambiguous.
        match self.relay.request_decision(id, summarize_args(&request.arguments), "console").await {
```

Note the id string is put in the *basename slot* and the request's real `id` field is a different fresh UUID minted inside `request_decision`. Step 2 makes the two the same UUID.

### Grants today (`src/gateway/api_v1.rs:590-594, 630-637, 705-710`)

```rust
    let req_session_id = body.session_id.clone();
    let scope_session_id = req_session_id.clone();
    let history_session_id = body.session_id.clone();
    ...
                    if let Some(sid) = history_session_id.as_deref() {
                        manager.seed_session_allowlist(
                            crate::gateway::web_approval::session_granted_tools(sid),
                        );
    ...
                if let (Some(mgr), Some(sid)) =
                    (approval_for_harvest.as_ref(), history_session_id.as_deref())
                {
                    crate::gateway::web_approval::record_session_grants(
                        sid,
                        &mgr.session_allowlist(),
```

`scope_session_id` is filtered with `.filter(|s| !s.is_empty())` at ~612 and `load_session_history` filters at ~300; `history_session_id` is not, so `"session_id": ""` keys the grant map under `""`. (The claw-ui console always sends a UUID; this only affects other API clients — still fix it.)

`src/gateway/web_approval.rs:148-157`:

```rust
    if tools.is_empty() {
        return;
    }
    let mut map = SESSION_GRANTS.lock();
    if !map.contains_key(session_id) && map.len() >= MAX_GRANT_SESSIONS {
        return;
    }
```

The existing test `session_grants_accumulate_and_empty_is_noop` (`web_approval.rs:295-307`) would still pass if the `tools.is_empty()` guard were deleted (extending with an empty set is a no-op anyway). It pins nothing.

### Conventions to match

- Handler errors: `err_400` / `err_404` / `err_500` helpers in `api_v1.rs:169-200`.
- Tests: `#[tokio::test]` inside `mod tests` at the bottom of `api_v1.rs`; `test_state()` (~2175) builds an `AppState` with pairing off; `paired_state("tok")` builds one with pairing on (used at `api_v1.rs:2524-2532` for the 401 test pattern). Any test that opens the session store must take `crate::test_env::ENV_LOCK` and pin `HomeGuard::set(tmp.path())` exactly as `sse_chat_emits_chunk_then_done` does at `api_v1.rs:2290-2315`.
- Dependency direction (CLAUDE.md §6.4): `src/approval/` must not import `src/gateway/`. That is why step 5 moves the grant store.
- Naming (CLAUDE.md §6.3): tests named `<subject>_<expected_behavior>`; fixture ids neutral (`sess-…`, `rantaiclaw_user`).

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Unit tests (scoped) | `cargo test --lib web_approval` / `cargo test --lib pending` / `cargo test --lib api_v1::tests` | all pass |
| Full lib suite (before PR) | `cargo test --lib` | all pass |
| Never | bare `cargo test` (workspace) | — disk-constrained machine; use `--lib` and `--test <name>` |

If a `grep` you rely on prints nothing unexpectedly, rerun it as `rtk proxy grep …` — a shell hook filters grep output on this machine.

## Scope

**In scope** (the only files you should modify):
- `src/gateway/api_v1.rs`
- `src/gateway/web_approval.rs`
- `src/gateway/config_api.rs` (one call site)
- `src/security/pending.rs` (add a resolved-broadcast; no behaviour change for existing callers)
- `src/agent/events.rs` (one new variant) and the match sites the compiler then points at (add a no-op arm; do not change their behaviour)
- `src/approval/session_grants.rs` (create) and `src/approval/mod.rs` (export)
- `src/approval/policy_writer.rs` (one call)
- `docs/reference/api-v1-streaming.md` (one row for the new event)

**Out of scope** (do NOT touch, even though they look related):
- `src/tools/shell.rs` — it already scopes correctly via `current_turn_scope()`; the gateway must *provide* the scope, not the tool.
- `src/channels/**` — the channel relay uses `resolve_by_basename_in`; leave it.
- Approval *policy* (`src/security/policy.rs`, presets, `always_ask`) — audited last week.
- The claw-ui side (modal queue, `approval_resolved` handling) — plan 227.
- The sync `POST /api/v1/agent/chat` path — it has no approval backend; documenting that is plan 222.

## Git workflow

- Branch: `fix/web-approval-scoping` off `main`.
- One commit per step, conventional-commit style matching `git log`: e.g. `fix(security): scope web-console shell approvals to the SSE turn that raised them`.
- No `Co-Authored-By: Claude` trailer (repo convention).
- Do NOT push or open a PR unless the operator instructed it. When they do: PR title = the step-1 commit subject; body per `.github/pull_request_template.md`, listing each step as a bullet.

## Steps

### Step 1: Give every SSE turn a scope and forward only its own shell requests

In `agent_chat_stream` (`src/gateway/api_v1.rs`, the `tokio::spawn` at ~607):

1. Before the spawn, mint `let turn_scope = uuid::Uuid::new_v4().to_string();` and clone it for the task and for the forwarder.
2. Wrap the agent turn (the `agent.turn_streaming(...)`/`turn` call inside the spawned task — everything from `restore_history` through the turn's completion) in
   ```rust
   crate::security::TURN_SCOPE
       .scope(("console".to_string(), turn_scope_for_task.clone()), async move { /* existing body */ })
       .await
   ```
   exactly as `src/channels/dispatch.rs:580` does. The shell tool then registers with `channel = "console"`, `reply_target = <turn_scope>`.
3. Change the forwarder filter from `Ok(req) if req.channel.is_empty()` to
   ```rust
   Ok(req) if req.channel == "console" && req.reply_target == turn_scope_for_fwd
   ```
   and forward `id: req.id.to_string()` (the UUID) instead of `req.basename`. Keep the basename for display: `args: json!({"command": req.full_command, "basename": req.basename})`.
4. Extract the predicate into a `fn forward_to_this_stream(req: &PendingRequest, turn_scope: &str) -> bool` (private, next to `CancelOnDrop`) so it can be unit-tested without a stream.

**Verify**: `cargo test --lib api_v1::tests::forwarder_only_matches_its_own_turn_scope` (write it in step 6; for now `cargo clippy --all-targets -- -D warnings` → exit 0).

### Step 2: Register the Layer-A modal with the same scope and a single UUID

In `src/gateway/web_approval.rs` `WebModalApprovalBackend::decide`:

- `let id = Uuid::new_v4();` (a `Uuid`, not a `String`).
- Send `AgentEvent::ApprovalRequest { id: id.to_string(), … }` as today.
- Read `let (channel, reply_target) = crate::security::current_turn_scope();` — inside the turn task this now yields `("console", <turn_scope>)`.
- Replace `self.relay.request_decision(id, summarize_args(..), "console")` with
  `self.relay.request_decision_in(id, id.to_string(), summarize_args(&request.arguments), channel, reply_target)`.

Now the request's `id` field equals the string the browser holds.

**Verify**: `cargo test --lib web_approval` → existing `decide_*` tests still pass.

### Step 3: Resolve by UUID only

`src/gateway/web_approval.rs::resolve`:

```rust
pub fn resolve(relay: &PendingApprovals, id: &str, approve: bool, always: bool) -> bool {
    let decision = /* unchanged */;
    match Uuid::parse_str(id) {
        Ok(uuid) => relay.resolve(uuid, decision),
        Err(_) => false,
    }
}
```

Remove the `resolve_by_basename` call from this function. Update the doc comment on `resolve_approval` in `api_v1.rs` ("The `id` is the one carried by the `approval_request` SSE event") — still true, now it is always a UUID.

The test `resolve_approval_endpoint_resolves_pending_request` (`api_v1.rs:2426`) registers `request_decision("modal-1", …)` and resolves `"modal-1"` — a basename, not a UUID. Rewrite it: register with `request_decision_in(uuid, uuid.to_string(), "tool: web_search", "console", "turn-x")` and resolve `uuid.to_string()`. Keep `resolve_approval_endpoint_unknown_id_is_404`.

**Verify**: `cargo test --lib api_v1::tests::resolve_approval` → 2 tests pass.

### Step 4: Broadcast resolutions and emit `approval_resolved` on the stream

1. `src/security/pending.rs`: add to the shared inner struct a `resolved_tx: broadcast::Sender<(Uuid, Decision)>` (capacity 64, same as `notify_tx`). In `resolve()` after a successful `tx.send`, `let _ = self.inner.resolved_tx.send((id, decision));`. In `request_decision_in`, in the timeout branch that yields `Decision::Deny`, also send `(id, Decision::Deny)`. Add `pub fn subscribe_resolved(&self) -> broadcast::Receiver<(Uuid, Decision)>`.
2. `src/agent/events.rs`: add
   ```rust
   /// A pending in-browser approval was answered (or expired). Lets the
   /// console close the modal instead of leaving dead buttons on screen.
   ApprovalResolved { id: String, approved: bool, timed_out: bool },
   ```
   Add a no-op arm wherever the compiler reports a non-exhaustive match (TUI event loops etc.). Do not add behaviour there.
3. In the forwarder task in `api_v1.rs`, also subscribe `let mut resolved_rx = web_approvals.subscribe_resolved();` and `tokio::select!` over both receivers. Track the ids this stream forwarded (a `HashSet<Uuid>` — both the shell ids from step 1 and the Layer-A ids: have `decide` also push its id into the same set via the events channel, or simpler: emit `ApprovalResolved` for any resolved id whose request had `reply_target == turn_scope` — carry `reply_target` in the broadcast tuple instead: `(Uuid, Decision, String /*reply_target*/)`). Use the reply_target form; it needs no shared set.
   `timed_out` = the broadcast came from the timeout branch (add a `bool` to the tuple).
4. In the SSE payload match (`api_v1.rs:736-853`) add:
   ```rust
   crate::agent::AgentEvent::ApprovalResolved { id, approved, timed_out } => serde_json::json!({
       "type": "approval_resolved", "id": id, "approved": approved, "timed_out": timed_out,
   }),
   ```
5. `docs/reference/api-v1-streaming.md` event table: add the row
   `| approval_resolved | id, approved, timed_out | The approval identified by id was answered (approved true/false) or expired (timed_out true). Close the modal. |`

**Verify**: `cargo test --lib pending` → existing tests pass; new test `resolve_broadcasts_to_resolved_subscribers` (step 6) passes.

### Step 5: Grants — safe keys, revoke everywhere, clear on delete

1. Create `src/approval/session_grants.rs` and move `SESSION_GRANTS`, `MAX_GRANT_SESSIONS`, `session_granted_tools`, `record_session_grants`, `clear_all_session_grants` there verbatim, plus a new `pub fn clear_session_grants(session_id: &str)`. Add `pub mod session_grants;` in `src/approval/mod.rs`. In `src/gateway/web_approval.rs` replace the bodies with `pub use crate::approval::session_grants::{…};` so existing gateway callers compile unchanged.
2. `record_session_grants` and `session_granted_tools`: return early / return empty when `session_id.trim().is_empty()`.
3. When the `MAX_GRANT_SESSIONS` cap is hit: `tracing::warn!(session_id = %session_id, cap = MAX_GRANT_SESSIONS, "web approval grant not persisted: session cap reached");`.
4. `api_v1.rs` ~592: `let history_session_id = body.session_id.clone().filter(|s| !s.is_empty());`.
5. `api_v1.rs` `sessions_delete` (~1024): after a successful delete, call `crate::approval::session_grants::clear_session_grants(&resolved_id)`.
6. `src/approval/policy_writer.rs::apply_preset_to_config`: call `crate::approval::session_grants::clear_all_session_grants();` at the end. Keep the existing call in `config_api.rs:419` (harmless double clear) — or remove it if `set_autonomy` already goes through `apply_preset_to_config`; check with `rtk proxy grep -n apply_preset_to_config src/gateway/config_api.rs`.

**Verify**: `cargo test --lib session_grants` and `cargo test --lib web_approval` → pass (tests in step 6).

### Step 6: Tests

Add / rewrite:

- `src/gateway/api_v1.rs` tests:
  - `forwarder_only_matches_its_own_turn_scope`: build two `PendingRequest`s (via `PendingApprovals::request_decision_in` on a registry with a 10 s deadline, spawned) with reply_targets `"t1"`, `"t2"` and the same basename `"git"`; assert `forward_to_this_stream(&req_t1, "t1")` is true and `forward_to_this_stream(&req_t2, "t1")` is false; assert a request with `channel == ""` is false.
  - `resolve_approval_requires_auth_when_pairing_enabled`: `resolve_approval(State(paired_state("tok")), HeaderMap::new(), Path("…".into()), Json(..))` → `UNAUTHORIZED` (mirror `providers_list_requires_auth_when_pairing_enabled` at ~2524).
  - `agent_chat_requires_auth_when_pairing_enabled`: same for `agent_chat` (sync body) → 401.
  - `resolve_by_basename_is_rejected_on_the_web_path`: register a request whose basename is `"git"`; `resolve(&relay, "git", true, false)` → `false`; resolve by its UUID → `true`.
- `src/security/pending.rs` tests: `resolve_broadcasts_to_resolved_subscribers` (subscribe, register, resolve, receive `(id, Once, reply_target, false)`); `timeout_broadcasts_deny_as_timed_out` (deadline 50 ms, receive `(id, Deny, _, true)`).
- `src/approval/session_grants.rs` tests: move the existing test and make it real: `record_session_grants("", …)` leaves the map without a `""` key; at the cap, a new id is not persisted but an existing id still accumulates; `clear_session_grants(id)` empties only that id; `clear_all_session_grants` empties all.

**Verify**: `cargo test --lib api_v1::tests`, `cargo test --lib pending`, `cargo test --lib session_grants` → all pass; then `cargo test --lib` → all pass.

### Step 7: Format, lint, full suite

`cargo fmt --all` (then `--check`), `cargo clippy --all-targets -- -D warnings`, `cargo test --lib`.

## Test plan

Covered by step 6. Model the handler tests on `api_v1.rs:2426-2456` (resolve) and `:2524-2532` (401). Model the registry tests on the existing `mod tests` in `pending.rs`. No test may write to the operator's real `sessions.db` — none of these open the store; if you add one that does, use the `ENV_LOCK` + `HomeGuard` pattern from `api_v1.rs:2290-2315`.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib` exits 0; the seven new tests named in step 6 exist and pass
- [ ] `rtk proxy grep -n "resolve_by_basename(" src/gateway/web_approval.rs` returns no match
- [ ] `rtk proxy grep -n "req.channel.is_empty()" src/gateway/api_v1.rs` returns no match
- [ ] `rtk proxy grep -n "approval_resolved" src/gateway/api_v1.rs docs/reference/api-v1-streaming.md` returns one match in each file
- [ ] `rtk proxy grep -rn "static SESSION_GRANTS" src/` returns exactly one match, in `src/approval/session_grants.rs`
- [ ] `rtk proxy grep -n "clear_all_session_grants" src/approval/policy_writer.rs` returns one match
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The code at the locations in "Current state" doesn't match the excerpts (drift).
- `TURN_SCOPE.scope(...)` cannot wrap the turn because the gateway path spawns a *new task* between the scope and `Tool::execute` (task-locals do not cross `tokio::spawn`). Check `src/agent/loop_.rs` for a `tokio::spawn` around tool execution before assuming; if one exists, report — the fix would be to pass the scope explicitly, which is a design change.
- Adding the `ApprovalResolved` variant requires behavioural changes (not just no-op arms) in more than three files.
- `apply_preset_to_config` is not the common path for CLI + TUI tightening (verify with `rtk proxy grep -rn apply_preset_to_config src/`); if the TUI has a separate writer, report it rather than adding a second call site blindly.
- A step's verification fails twice after a reasonable fix attempt.

## Maintenance notes

- Any new approval surface (a future mobile client, a second console) must (a) set `TURN_SCOPE` around its turn and (b) resolve by UUID. The forwarder predicate `forward_to_this_stream` is the single place that decides "mine or not".
- The sync `POST /api/v1/agent/chat` path has no approval backend and is cut by the request timeout (plan 221 wires the timeout to config; plan 222 documents that approvals require the SSE path).
- Reviewer focus: the `select!` in the forwarder must not drop `Lagged` errors silently on the resolved receiver either; and the `ENV_LOCK` discipline in any new test that touches the store.
- Deferred on purpose: a "pending approvals" read endpoint for tab-reload recovery (claw-ui plan 227 handles reload by dismissing the modal on `approval_resolved`/`done`).
