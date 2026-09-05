# Plan 164: Give scheduled/headless agent runs a non-interactive approval backend

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/approval/mod.rs src/agent/loop_.rs src/cron/scheduler.rs src/daemon/mod.rs src/main.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P0
- **Effort**: S
- **Risk**: LOW-MED
- **Depends on**: none
- **Category**: bug / security

## Why this matters

When a cron **agent** job (or a daemon heartbeat task) needs to approve a tool
call — shell, `file_write`, `http_request` under the default **Supervised**
autonomy — it currently routes to the **interactive CLI** approval backend,
which does a **synchronous blocking `stdin.read_line()`**. On a scheduled,
headless run there is no operator at a terminal. This produces three concrete
failure modes:

1. **Under systemd (stdin = null):** the blocking read hits EOF, which maps to
   `ApprovalResponse::No`, and the literal text "Denied by user." becomes the
   agent's answer — which is then what gets **announced to the user's chat**.
   Silent, misleading auto-deny.
2. **Daemon started from a terminal:** the scheduler **steals stdin** from the
   foreground process, competing with whatever is reading the terminal.
3. **Blocking read inside async:** a synchronous `read_line` never yields, so
   the `AGENT_JOB_TIMEOUT_SECS = 600` guard (`scheduler.rs:111-116`) **cannot
   fire** (timeout races a future that never awaits), parking a Tokio worker
   indefinitely.

After this plan, the scheduled/headless path selects the **auto-deny** backend
explicitly (not the CLI one), the denial is recorded into run history with a
reason an operator can understand, and `prompt_cli_interactive` refuses to block
when stdin is not a terminal (defense-in-depth). **This plan does NOT
auto-approve anything** — auto-approving on a headless run would be a capability
change and is explicitly out of scope.

## Current state

### The scheduled path passes through `agent::run` — `src/cron/scheduler.rs:240`

```rust
Box::pin(crate::agent::run(
    config.clone(),
    Some(prefixed_prompt),
    None,
    model_override,
    config.default_temperature,
    vec![],
))
.await
```

`crate::agent::run` (`src/agent/loop_.rs:2013`) currently has this signature —
**no surface/channel parameter**:

```rust
pub async fn run(
    config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    peripheral_overrides: Vec<String>,
) -> Result<String> {
```

Inside `run`, the tool loop is invoked with the channel name **hardcoded** to
`"cli"` — `src/agent/loop_.rs:2325-2344`:

```rust
let response = run_tool_call_loop(
    provider.as_ref(),
    &mut history,
    &tools_registry,
    observer.as_ref(),
    provider_name,
    model_name,
    temperature,
    false,
    Some(&approval_manager),   // loop_.rs:2334
    "cli",                     // loop_.rs:2335  ← channel_name / surface identity
    None, // channel_reply_target — CLI has no origin chat
    None, // approval_backend
    None, // guest_gate
    &config.multimodal,
    config.agent.max_tool_iterations,
    None,
    None,
    None,
)
.await?;
```

### The backend is selected from the channel name — `src/approval/mod.rs:307-319`

```rust
pub fn default_backend_for(channel_name: &str) -> Box<dyn ApprovalBackend> {
    if channel_name == "cli" {
        Box::new(CliApprovalBackend)
    } else {
        Box::new(AutoDenyBackend)
    }
}
```

So **any surface name other than `"cli"` already selects `AutoDenyBackend`**
(`approval/mod.rs:298-305`, which returns `ApprovalResponse::No`). That is the
seam this plan uses: route the headless path through a non-`"cli"` surface name.

### The blocking read — `src/approval/mod.rs:324-343`

```rust
fn prompt_cli_interactive(request: &ApprovalRequest) -> ApprovalResponse {
    let summary = summarize_args(&request.arguments);
    eprintln!();
    eprintln!("🔧 Agent wants to execute: {}", request.tool_name);
    eprintln!("   {summary}");
    eprint!("   [Y]es / [N]o / [A]lways for {}: ", request.tool_name);
    let _ = io::stderr().flush();

    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return ApprovalResponse::No;
    }

    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalResponse::Yes,
        "a" | "always" => ApprovalResponse::Always,
        _ => ApprovalResponse::No,
    }
}
```

Reached via `CliApprovalBackend::decide` → `mgr.prompt_cli(request)`
(`approval/mod.rs:288-293` and `213-215`). The imports at `approval/mod.rs:18`
are `use std::io::{self, BufRead, Write};` — **`IsTerminal` is not imported.**

### The denial message already branches on the surface — `src/agent/loop_.rs:1335`

```rust
let msg = if channel_name == "cli" {
    "Denied by user.".to_string()
} else {
    format!(
        "Tool '{}' denied: requires approval, but none was granted. \
         An operator can enable in-chat approval via \
         [channels_config].approval_owners (then reply `/approve`), \
         set [channels_config].autonomous_tools = true, or raise autonomy \
         with `rantaiclaw autonomy full` (trusted/sandboxed only).",
        call.name
```

This is why routing the headless path to a non-`"cli"` surface **also gives a
more informative denial string** in run history — the explanatory branch, not
the terse "Denied by user."

### Default autonomy makes this common — `src/config/schema.rs:2217-2218`

```rust
fn default_auto_approve() -> Vec<String> {
    vec!["file_read".into(), "memory_recall".into()]
}
```

Under the default **Supervised** level, only `file_read` and `memory_recall`
are auto-approved; `shell`, `file_write`, and `http_request` all reach
`needs_approval` — so a scheduled agent job that does anything meaningful hits
this path routinely.

### The three callers of `agent::run` (all must compile after a signature change)

```
src/cron/scheduler.rs:240   — cron agent job (HEADLESS → non-interactive)
src/daemon/mod.rs:323       — heartbeat task loop (HEADLESS → non-interactive)
src/main.rs:1846            — interactive `run` command (real terminal → "cli")
```

`src/daemon/mod.rs:319-323`:

```rust
for task in tasks {
    let prompt = format!("[Heartbeat Task] {task}");
    let temp = config.default_temperature;
    if let Err(e) =
        crate::agent::run(config.clone(), Some(prompt), None, None, temp, vec![]).await
```

`src/main.rs:1846`:

```rust
}) => agent::run(config, message, provider, model, temperature, peripheral)
```

### The TUI force-run path (already correct — do not change)

`src/tui/app.rs:3496-3499` spawns `run_job_manual` detached, which calls
`execute_job_now` → `execute_job_with_retry` → `run_agent_job` →
`crate::agent::run`. Because it goes through `agent::run`, it inherits whatever
surface the cron/scheduler path passes. This is fine: a TUI force-run is also
headless with respect to the blocking stdin prompt (crossterm owns the
terminal), so it should also get the non-interactive backend. No separate change
is needed there — it rides on the scheduler path fix.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format check | `cargo fmt --all -- --check` | exit 0, no diff |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0, no warnings |
| Scoped tests (approval) | `cargo test --lib approval` | all pass, incl. new tests |
| Scoped tests (scheduler) | `cargo test --lib cron::scheduler` | all pass |
| Build (whole crate, compile only) | `cargo build` | exit 0 — proves all 3 callers updated |

Do **NOT** run bare `cargo test` — it builds ~27G and will exhaust the disk.
`cargo build` (compile-only) is acceptable and is how you confirm the three
callers were all updated.

## Scope

**In scope** (the only files you should modify):
- `src/agent/loop_.rs` — add a surface parameter to `agent::run`, thread it to
  the `run_tool_call_loop` channel-name slot (line ~2335)
- `src/cron/scheduler.rs` — pass a non-interactive surface at line ~240; record
  the deny reason into run output (see Step 4)
- `src/daemon/mod.rs` — pass a non-interactive surface at line ~323
- `src/main.rs` — pass `"cli"` at line ~1846 (behavior preserved)
- `src/approval/mod.rs` — guard `prompt_cli_interactive` with `IsTerminal`;
  new unit tests

**Out of scope** (do NOT touch):
- Do NOT auto-**approve** on any headless surface — that is a capability change.
  The headless surface must select `AutoDenyBackend` (deny), never a
  yes-by-default path.
- Do NOT change the session-origin literal `"cli"` at `loop_.rs:2299`
  (`new_session(model_name, "cli")`) — that only labels session origin and is
  orthogonal; changing it is a separate concern.
- Do NOT change `default_backend_for` (`approval/mod.rs:313`) — its
  `!= "cli" ⇒ AutoDenyBackend` behavior is exactly what we rely on.
- Do NOT touch the gateway/channel approval relay paths.

## Git workflow

- Branch: `advisor/164-cron-headless-approval-backend`
- Conventional-commit title, e.g.
  `fix(cron): route scheduled agent runs to a non-interactive approval backend`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a `surface` parameter to `agent::run` and thread it

In `src/agent/loop_.rs`, add a `surface: &str` parameter to `pub async fn run`
(`loop_.rs:2013`). Place it last for the smallest diff, e.g.:

```rust
pub async fn run(
    config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    peripheral_overrides: Vec<String>,
    surface: &str,
) -> Result<String> {
```

Then change the hardcoded channel name passed to `run_tool_call_loop` from
`"cli"` (`loop_.rs:2335`) to `surface`:

```rust
    Some(&approval_manager),
    surface,   // was "cli"
    None, // CLI — no origin chat
```

Leave the session-origin `"cli"` at `loop_.rs:2299` unchanged (out of scope).

**Verify (after Step 3, when callers are updated)**: compilation. For now,
`cargo build` will fail until Steps 2–3 update the callers — that is expected.

### Step 2: Update the interactive caller to preserve behavior

In `src/main.rs:1846`, pass `"cli"` so the interactive `run` command keeps the
interactive backend:

```rust
}) => agent::run(config, message, provider, model, temperature, peripheral, "cli")
```

### Step 3: Update the two headless callers to a non-interactive surface

Pick a single, stable surface name for headless agent runs. Use `"scheduler"`
(any non-`"cli"` string works because `default_backend_for` maps everything
else to `AutoDenyBackend`; `"scheduler"` is descriptive and shows up in the
informative denial message).

- `src/cron/scheduler.rs:240` — add `"scheduler"` as the final argument:

```rust
Box::pin(crate::agent::run(
    config.clone(),
    Some(prefixed_prompt),
    None,
    model_override,
    config.default_temperature,
    vec![],
    "scheduler",
))
.await
```

- `src/daemon/mod.rs:323` — add `"scheduler"` (heartbeat is equally headless):

```rust
crate::agent::run(config.clone(), Some(prompt), None, None, temp, vec![], "scheduler").await
```

**Verify**: `cargo build` → exit 0 (all three callers now compile), then
`cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Ensure the deny reason reaches run history

The scheduled path already records `run_agent_job`'s returned string into run
history (`persist_job_result` → `record_run`, `scheduler.rs:284-292`). With
Step 1–3, an approval-required tool on the `"scheduler"` surface produces the
**informative** denial branch (`loop_.rs:1337-1344`, "requires approval, but
none was granted…"), and that flows back as part of the agent response. Confirm
by reading `run_agent_job` (`scheduler.rs:203-263`): its `Ok(response)` arm
returns `response`, which is exactly what `agent::run` returns. So no extra
plumbing is required for the reason to appear in run output — the surface change
alone upgrades the message.

If, after the surface change, the returned response for a fully-denied run is
empty (the loop may return an empty string when every tool was denied and the
model produced no text), add a fallback in `run_agent_job`'s `Ok` arm so the run
output is never a bare empty/"agent job executed" string when a denial
occurred. The existing arm already substitutes `"agent job executed"` for empty
responses (`scheduler.rs:253-259`); leave that as-is unless a test shows the
denial reason is lost — in which case surface it (STOP and report if unsure how
to detect the denial from within `run_agent_job`, since it does not currently
see per-tool decisions).

**Verify**: covered by the test in Step 6.

### Step 5: Guard `prompt_cli_interactive` with `IsTerminal` (defense-in-depth)

In `src/approval/mod.rs`, add `IsTerminal` to the `std::io` import at line 18:

```rust
use std::io::{self, BufRead, IsTerminal, Write};
```

At the top of `prompt_cli_interactive` (`approval/mod.rs:324`), before printing
the prompt, bail out to deny when stdin is not a terminal:

```rust
fn prompt_cli_interactive(request: &ApprovalRequest) -> ApprovalResponse {
    // No interactive approver present (systemd stdin=null, piped input, or a
    // scheduled/headless run that reached this path by mistake): never block on
    // a read that can't be answered — auto-deny, matching AutoDenyBackend.
    if !io::stdin().is_terminal() {
        return ApprovalResponse::No;
    }
    let summary = summarize_args(&request.arguments);
    // … unchanged …
```

This makes the blocking-read failure mode impossible even if some future caller
routes a headless run through `"cli"`.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 6: Tests

Add unit tests in `src/approval/mod.rs`'s test module (find it with
`grep -n "mod tests" src/approval/mod.rs`; if none exists, add
`#[cfg(test)] mod tests { use super::*; … }` at the end of the file):

1. **Surface selects the right backend.** Assert that a non-`"cli"` surface maps
   to auto-deny and `"cli"` maps to the interactive backend. Since
   `ApprovalBackend` is a trait object, test via the observable decision rather
   than the concrete type: call `default_backend_for("scheduler")` and
   `.decide(...)` on a request that requires approval, and assert
   `ApprovalResponse::No`. Build a minimal `ApprovalManager` and
   `ApprovalRequest` the way existing approval tests do (grep the file for an
   existing `ApprovalRequest { … }` construction to copy the field set). If the
   `"cli"` backend cannot be tested without a terminal, assert only the
   `"scheduler"` → `No` direction and note the `"cli"` case is covered by the
   `IsTerminal` guard test below.

```rust
#[tokio::test]
async fn scheduler_surface_auto_denies_without_hanging() {
    let mgr = /* build a minimal ApprovalManager, copy an existing test */;
    let request = /* an ApprovalRequest for an approval-required tool */;
    let backend = default_backend_for("scheduler");
    let decision = backend.decide(&mgr, &request).await;
    assert_eq!(decision, ApprovalResponse::No);
}
```

2. **`prompt_cli_interactive` denies when stdin is not a terminal.** In the test
   harness, stdin is typically not a TTY, so
   `prompt_cli_interactive(&request)` (or `mgr.prompt_cli(&request)`) should
   return `ApprovalResponse::No` **without blocking**. Guard the test so it does
   not hang if run interactively:

```rust
#[test]
fn cli_prompt_denies_when_stdin_not_a_terminal() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return; // interactive run: skip, the guard only fires for non-TTY stdin
    }
    let request = /* an ApprovalRequest */;
    assert_eq!(prompt_cli_interactive(&request), ApprovalResponse::No);
}
```

If `prompt_cli_interactive` is private and not reachable from the test module,
test through `CliApprovalBackend::decide` / `mgr.prompt_cli` instead (both are
in the same module).

**Verify**: `cargo test --lib approval` → all pass, including the 2 new tests,
and the run completes quickly (no hang). Also run
`cargo test --lib cron::scheduler` → all existing scheduler tests still pass.

### Step 7: Format

**Verify**: `cargo fmt --all -- --check` → exit 0.

## Test plan

- New tests in `src/approval/mod.rs`:
  - `default_backend_for("scheduler")` decides `No` on an approval-required
    request (auto-deny, no hang).
  - `prompt_cli_interactive` returns `No` when stdin is not a terminal
    (guarded to skip on interactive TTY).
- **Honesty note on test #1's coverage.** `default_backend_for("scheduler")`
  returning `No` was already true before this change — `AutoDenyBackend` ignores
  its arguments and denies unconditionally, and any non-`"cli"` surface already
  selected it. So this test does NOT pin the routing change (that the headless
  callers now pass `"scheduler"` instead of `"cli"`). The routing itself is
  verified **structurally** — by `cargo build` (all three `agent::run` callers
  compile with the new surface argument) and the grep on
  `src/cron/scheduler.rs`/`src/daemon/mod.rs` (they pass `"scheduler"`, not
  `"cli"`) — not by test #1.
- Existing scheduler tests (`src/cron/scheduler.rs`) must still pass — they
  cover the security gates and one-shot handling on the scheduled path.
- Verification: `cargo test --lib approval` and
  `cargo test --lib cron::scheduler` → all pass; `cargo build` → exit 0 (proves
  all three `agent::run` callers were updated).

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo build` exits 0 (all three `agent::run` callers updated)
- [ ] `cargo test --lib approval` exits 0; the 2 new tests exist and pass
- [ ] `cargo test --lib cron::scheduler` exits 0
- [ ] `grep -n '"cli"' src/cron/scheduler.rs src/daemon/mod.rs` returns no
      match for an `agent::run` surface argument (they pass `"scheduler"`)
- [ ] `grep -n "IsTerminal" src/approval/mod.rs` shows the import and the guard
- [ ] No auto-**approve** path was added on any headless surface (review the
      diff: headless surfaces select `AutoDenyBackend`, never Yes)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `agent::run` at `loop_.rs:2013` has a different signature than the excerpt, or
  the `run_tool_call_loop` call no longer passes `"cli"` at `loop_.rs:2335`
  (drift).
- There are more callers of `crate::agent::run(` / `agent::run(` than the three
  listed (run `grep -rn "agent::run(" src/`) — every caller must be updated to
  compile; report the extras and how you handled them.
- `default_backend_for` (`approval/mod.rs:313`) no longer maps non-`"cli"` to
  `AutoDenyBackend` — the surface-name approach depends on it.
- You cannot obtain the deny reason in run output without seeing per-tool
  decisions inside `run_agent_job` (Step 4 fallback) — report rather than
  inventing a plumbing change beyond scope.
- A step's verification fails twice after a reasonable fix attempt.

## Maintenance notes

For the human/agent who owns this code after the change lands:

- A reviewer should verify: (1) the headless surfaces select **deny**, never
  approve; (2) the `IsTerminal` guard cannot regress interactive CLI approval
  (an interactive terminal still prompts); (3) all three `agent::run` callers
  compile and pass the intended surface.
- The `surface` string flows into `run_tool_call_loop`'s `channel_name`, which
  also drives the denial-message branch (`loop_.rs:1335`) and could interact
  with any future per-channel logic keyed on that name. If a new channel-keyed
  behavior is added, confirm `"scheduler"` lands in the intended branch.
- **Deferred (explicitly out of scope):** letting an operator opt a specific
  scheduled job into auto-approval (e.g. a per-job `autonomous_tools`-style
  flag) is a capability change and must be designed separately with its own
  threat notes. This plan only stops the hang / silent-deny and makes the
  denial legible.
- The session-origin literal `"cli"` at `loop_.rs:2299` still labels scheduled
  runs as CLI-origin in `sessions.db`; if that becomes confusing in
  `session list`, thread the surface there too in a follow-up.
