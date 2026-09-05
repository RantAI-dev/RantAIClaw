# Plan 003: Fix MCP supervisor backoff/give-up defeated by respawn resetting the failure counter

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/mcp/`
> If any file under `src/mcp/` changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

The MCP supervisor documents "Exponential backoff: 1s → 2s → … → 60s" and "After
5 consecutive failures, server is marked Error and not restarted." Neither
actually happens. `respawn()` sets `consecutive_failures = 0` on every spawn, so
for a server that starts fine but exits/crashes quickly (bad config, missing
env, immediate error exit) the counter cycles 0→1→0→1 forever: backoff never
escalates past the first 1s step, and the 5-failure give-up never fires. A
broken MCP server is respawned roughly every ~6s indefinitely, with continuous
process-spawn churn and log spam, and is never marked `Error`. This makes the
documented safety net dead code.

## Current state

- `src/mcp/supervisor.rs` — the poll loop. Header comment (lines 1-3) promises
  the behavior that doesn't happen:
  ```rust
  //! Exponential backoff: 1s → 2s → 4s → 8s → 16s → 32s → 60s (cap).
  //! After 5 consecutive failures, server is marked Error and not restarted.
  ```
  Failure/restart logic (lines 40-75):
  ```rust
  } else if !handle.is_running() {
      warn!("MCP server '{}' exited unexpectedly", id);
      if handle.record_failure() {
          Some(handle.consecutive_failures)
      } else {
          error!("MCP server '{}' permanently failed after 5 attempts", id);
          None
      }
  } ...
  if let Some(failures) = needs_restart {
      let delay = backoff_delay(failures);
      ...
      tokio::time::sleep(delay).await;
      let mut reg = registry.write().await;
      if let Some(handle) = reg.get_server_mut(&id) {
          match handle.respawn().await { ... }
      }
      break; // Only handle one restart per poll cycle
  }
  ```

- `src/mcp/handle.rs` — `respawn` (lines 66-86) resets the counter:
  ```rust
  pub async fn respawn(&mut self) -> Result<()> {
      let process = Command::new(&self.command)...spawn()...?;
      self.process = process;
      self.status = McpStatus::Running;
      self.consecutive_failures = 0;   // <-- line 79: defeats backoff + give-up
      ...
  }
  ```
  `spawn` (line 61) correctly initializes `consecutive_failures: 0` for the
  first launch. `record_failure()` and `is_failed()` / `MAX_CONSECUTIVE_FAILURES`
  also live in `handle.rs` — read them: `grep -n "record_failure\|is_failed\|MAX_CONSECUTIVE_FAILURES\|consecutive_failures\|last_" src/mcp/handle.rs`.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| MCP tests | `cargo test mcp` | all pass, incl. new |

## Scope

**In scope**:
- `src/mcp/handle.rs` (stop resetting the counter in `respawn`; add a stability
  field/method)
- `src/mcp/supervisor.rs` (reset the counter only after a stability window)
- New tests (inline `#[cfg(test)]` in `src/mcp/supervisor.rs` or `handle.rs`)

**Out of scope** (do NOT touch):
- The env-clear hardening (that is plan 002 — if 002 already landed, keep it).
- `backoff_delay` math (line 17-20) — it is correct; only the counter lifecycle
  is broken.
- The one-restart-per-poll `break` (line 74) — a separate, lower-priority
  liveness nuance; do not change it here.

## Git workflow

- Branch: `advisor/003-mcp-supervisor-backoff-reset`
- One commit; message e.g.
  `fix(mcp): escalate backoff and honor 5-failure give-up (don't reset counter on respawn)`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Stop `respawn()` from resetting the failure counter

In `src/mcp/handle.rs::respawn`, delete the line `self.consecutive_failures = 0;`
(line 79). `respawn` succeeding is not evidence the server is healthy — only
sustained uptime is.

Add a way to record when the last respawn happened so the supervisor can decide
whether the server has been stable long enough to clear the counter. Add a field
to the handle struct (e.g. `last_respawn: Option<std::time::Instant>`), set it in
`respawn()` after a successful spawn, and initialize it to `None` in `spawn()`.
(`Instant` is allowed in runtime code — this is not a workflow script.)

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

### Step 2: Clear the counter only after a stability window in the supervisor

In `src/mcp/supervisor.rs`, in the poll branch that handles a server that **is**
still running (the current `else { None }` at line 54-56), add: if the server is
running AND `last_respawn` is `Some(t)` AND `t.elapsed() >= STABILITY_WINDOW`,
clear `consecutive_failures` to 0 and set `last_respawn = None`. Define
`const STABILITY_WINDOW: Duration = Duration::from_secs(30);` near the other
consts (line 13-15).

This makes the intended semantics real: crashes escalate backoff; only a server
that stays up for 30s is considered recovered and gets its counter cleared; a
server that keeps crashing hits `record_failure` returning false at 5 and is
marked failed (via the existing `is_failed()` path at line 44).

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

### Step 3: Confirm the give-up path is now reachable

Read the branch at `supervisor.rs:44` (`handle.is_failed()` → `None`, no
restart) and confirm that after 5 `record_failure()` calls without a stability
reset, `is_failed()` returns true and the server stops being respawned.

**Verify**: covered by the test in the next section.

## Test plan

- New unit tests (inline in `src/mcp/handle.rs` and/or `supervisor.rs`), no real
  processes needed — test the counter lifecycle directly:
  1. `respawn_does_not_reset_consecutive_failures`: construct a handle, call
     `record_failure()` twice (counter = 2), call `respawn()` (or the internal
     spawn if `respawn` needs a real command — if so, factor the counter logic
     so it is testable without spawning; e.g. test the field directly), assert
     `consecutive_failures` is still 2, not 0.
  2. `gives_up_after_five_consecutive_failures`: call `record_failure()` five
     times; assert the 5th returns the give-up signal (`false`) and `is_failed()`
     is true.
  3. `backoff_delay_escalates`: assert `backoff_delay(1) < backoff_delay(3)` and
     `backoff_delay(10) == BACKOFF_CAP` (already-correct function; lock it in).
  - If any test needs to spawn a process to exercise `respawn`, use a portable
    fast-exit command and gate it appropriately; prefer testing the counter
    logic without spawning.
- Verification: `cargo test mcp` → all pass including the three new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `grep -n "consecutive_failures = 0" src/mcp/handle.rs` shows the reset is
      gone from `respawn` (only `spawn`'s initializer, if any, remains — and that
      one is a struct-literal field init, not an assignment inside `respawn`)
- [ ] `cargo test mcp` passes; the three new counter-lifecycle tests exist
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `respawn` / `record_failure` / `is_failed` signatures don't match the excerpts
  (drift since `4d35107`).
- Testing the counter requires spawning real processes and there is no portable
  way to do it deterministically — report so the test approach can be decided.
- `MAX_CONSECUTIVE_FAILURES` is not 5 or the give-up path is structured
  differently than described — surface the actual structure.

## Maintenance notes

- The 30s `STABILITY_WINDOW` is a tunable; if MCP servers legitimately take
  longer to become stable, raise it. Document the constant.
- A future improvement (explicitly deferred): handle more than one restart per
  poll cycle (remove/replace the `break` at supervisor.rs:74) so many
  simultaneous crashes recover faster. Out of scope here.
- Reviewer should confirm the stability reset can't be triggered by a
  fast-crashing server that happens to be observed "running" mid-restart — the
  `last_respawn.elapsed() >= STABILITY_WINDOW` guard is what prevents that.
