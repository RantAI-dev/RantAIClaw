# Plan 193: Verify the `?approved=true` cron-run parameter, then record policy refusals as a distinct run status

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **RE-SCOPED (2026-08-19).** The original plan presented a STOP-and-confirm A/B
> (Option A: restore #356 API force-run by plumbing `operator_approved` through 5
> signatures; Option B: abandon API force-run, reversing recorded decision #356).
> That framing rests on a premise that is **contradicted by a passing pinned
> test**: the plan claims `?approved=true` is inert because the scheduler applies
> an unconditional `validate_command_execution(cmd, false)` at fire time — yet
> `tests/cron_api.rs::cron_run_honours_an_operator_supplied_approval` asserts that
> force-running a medium-risk job WITH `?approved=true` **succeeds**. Both cannot
> be true. So this plan **verifies the premise first** (Step 1). The A/B redesign
> is only reached if the probe proves the parameter is genuinely dead — which the
> pinned test suggests it is not. The uncontested, always-valuable change (record
> a policy refusal as a distinct run status) ships regardless.
>
> **Drift check (run first)**:
> `git diff --stat 434141c..HEAD -- src/gateway/cron_api.rs src/cron/scheduler.rs tests/cron_api.rs`
> If any changed since this plan was written, re-locate the named functions and
> compare against live code; on a mismatch, treat it as a STOP.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug / honesty
- **Planned at**: commit `434141c`, 2026-08-19 (re-scoped to verify-first)

## Why this matters

Two independent things:

1. **A possibly-stale finding.** The original claim: `POST /cron/{id}/run?approved=true`
   is dead because the handler's approval gate is followed by an unconditional
   `validate_command_execution(&job.command, false)` deep in the manual-run path
   (`run_job_command_with_timeout`), so an operator's approval clears the handler
   but is then refused by the scheduler — yielding `200 OK {"success":false,
   "output":"blocked by security policy: …"}`. If true, that is a misleading
   success code + a stale "the only way to force-run a gated job" comment. **But
   the pinned test asserts the opposite** (force-run with approval runs), so the
   premise must be re-verified before any redesign.

2. **An undifferentiated run status (valuable either way).** A run refused by
   policy is recorded with status `"error"` — indistinguishable from a command
   that actually ran and failed. Refused runs have a stable sentinel: `output`
   starts with `"blocked by security policy:"`. Recording them as a distinct
   status (e.g. `"refused"`) makes run history honest regardless of how the
   `?approved` question resolves.

## Current state

- `src/gateway/cron_api.rs` — `run_cron`: `RunQuery { approved: bool }`; gates a
  shell job on `validate_command_execution(&job.command, q.approved)`; a stale
  comment claims `?approved=true` is "the only way to force-run a gated job from
  the API"; then calls `run_job_manual` and returns `{ id, success, output }`.
- `src/cron/scheduler.rs`:
  - `run_job_manual` → `execute_job_now` → `execute_job_with_retry` →
    `run_job_command` → `run_job_command_with_timeout`. In that last function the
    allowlist refusal and `validate_command_execution(&job.command, false)`
    refusal both return `(false, "blocked by security policy: …")`.
  - `is_security_refusal(output: &str) -> bool` already exists (it matches the
    `"blocked by security policy:"` prefix; used to gate delivery). Reuse it.
  - Run status is written as `"ok"`/`"error"` only, in two places: `run_job_manual`
    (`let status = if success { "ok" } else { "error" };`) and `persist_job_result`
    (the per-attempt `record_attempt` path — status derived per attempt from
    `a.success`).
- `tests/cron_api.rs::cron_run_honours_an_operator_supplied_approval` — creates a
  `touch` (medium-risk, allowlisted) job, asserts run **without** approval → 400,
  run **with** `?approved=true` → (read the exact tail assertion in Step 1).

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format  | `cargo fmt --all -- --check` | exit 0 |
| Lint    | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Cron    | `cargo test --lib cron` | all pass |
| Gateway | `cargo test --test cron_api` | all pass |

Do NOT run a bare `cargo test`.

## Steps

### Step 1 (MANDATORY): Verify whether `?approved=true` actually force-runs a gated job today

Read `tests/cron_api.rs::cron_run_honours_an_operator_supplied_approval` in full,
including its assertion after the `?approved=true` request. Then trace
`run_job_command_with_timeout`: does the fire-time
`validate_command_execution(&job.command, false)` run on the **manual** path, and
does it refuse a medium-risk allowlisted command?

Resolve the contradiction one of two ways — add a focused test if needed:

- **If the probe shows force-run WORKS** (the pinned test genuinely runs the
  medium job with approval): the original "parameter is dead" finding is **STALE**.
  Do NOT do any A/B redesign. Skip to Step 2 (status only) and Step 3 (comment).
  Record in the PR: "verified `?approved=true` works end-to-end; the dead-parameter
  finding was stale."
- **If the probe shows force-run is genuinely refused** (200 with success:false,
  and the pinned test somehow does not actually cover the run outcome): STOP and
  report. The A/B decision (restore #356 vs abandon it) is a product call that
  reverses a recorded decision — do not choose it unilaterally. Bring the probe
  evidence to the operator.

**Verify**: you can state, with test evidence, which branch holds. Proceed only
on the "STALE" branch without operator input; the "genuinely dead" branch is a STOP.

### Step 2: Record a policy refusal as a distinct run status (`"refused"`)

Regardless of Step 1's outcome (this is the honest, uncontested change):

In both status-writing sites, when the output is a security refusal, write
`"refused"` instead of `"error"`. Reuse `is_security_refusal`:

- `run_job_manual`: replace `let status = if success { "ok" } else { "error" };`
  with a form that yields `"refused"` when `!success && is_security_refusal(&output)`,
  else the existing `"ok"`/`"error"`.
- `persist_job_result` / the per-attempt `record_attempt` path: derive each
  attempt's status the same way — `"refused"` when that attempt's output is a
  security refusal, else `"ok"`/`"error"`.

Extract a tiny helper to avoid drift, e.g.:

```rust
fn run_status(success: bool, output: &str) -> &'static str {
    if success { "ok" }
    else if is_security_refusal(output) { "refused" }
    else { "error" }
}
```

Note: `CronRun.status` is a free `String` column — no schema/enum change is
needed; `"refused"` is just a new value. Confirm no consumer (TUI/web/`cron_runs`
tool) pattern-matches `status` on an exhaustive set that would break on a new
value (they render it as text — verify).

**Verify**: `cargo test --lib cron` → pass; `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Fix the stale `run_cron` comment (and, only on the STALE branch, keep the parameter)

On the **STALE** branch (Step 1 showed force-run works): correct the `run_cron`
comment so it accurately describes that `?approved=true` force-runs a gated job
via the authenticated API path, and note the asymmetry with the `cron_run` tool
(recorded decision #356). Do NOT remove `RunQuery`/`approved`.

**Verify**: the comment matches observed behavior; `cargo test --test cron_api` → pass.

### Step 4: Tests

- `run_status` unit test: `(true, "...")` → `"ok"`; `(false, "boom")` → `"error"`;
  `(false, "blocked by security policy: command not allowed …")` → `"refused"`.
  **Mutation check**: remove the `is_security_refusal` branch and confirm the
  `"refused"` case fails; restore.
- Confirm `cron_run_honours_an_operator_supplied_approval` still passes unchanged.

**Verify**: `cargo test --lib cron` and `cargo test --test cron_api` → pass.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] Step 1 verified with test evidence which branch holds (recorded in the PR)
- [ ] Policy refusals record status `"refused"` (not `"error"`), via a
      mutation-checked `run_status` helper shared by both write sites
- [ ] `cargo test --lib cron` and `cargo test --test cron_api` pass, incl. the
      pinned approval test (UNCHANGED)
- [ ] No status consumer breaks on the new `"refused"` value
- [ ] Only in-scope files modified; `plans/README.md` status row updated

## STOP conditions

Stop and report if:
- Step 1 shows the parameter is **genuinely dead** — the A/B redesign reverses a
  recorded decision (#356) and needs an operator call; bring the evidence.
- Changing the status value breaks a consumer that exhaustively matches `status`.
- The named functions no longer match (drift since 434141c).

## Maintenance notes

- The `?approved` HTTP-vs-tool asymmetry is a recorded decision (#356 / commit
  `1d8f1f5`): the API may force-run gated jobs, the `cron_run` tool may not. Do
  not relitigate it here.
- If Step 1 ever flips (a refactor makes the parameter genuinely inert), the
  A/B decision resurfaces — Option A (restore, 5 signatures) vs Option B (abandon,
  reverse #356). That is a product call, not a refactor.
- The `"refused"` status pairs well with plan 185's per-attempt rows: an operator
  now sees `refused` distinctly from `error` per attempt.
