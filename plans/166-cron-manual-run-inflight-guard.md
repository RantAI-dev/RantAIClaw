# Plan 166: Guard manual cron runs against overlapping concurrent execution

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/tools/cron_run.rs src/gateway/cron_api.rs src/tui/app.rs src/cron/mod.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none (but coordinate with `plans/165-cron-batch-stall-spawn.md` — both change in-flight tracking in `src/cron/scheduler.rs`; see "Coordination" below)
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

The in-flight `HashSet` that stops a cron job from running twice concurrently is
created **inside** `scheduler::run` (`scheduler.rs:30`) and passed only to the
scheduled path (`process_due_jobs`, claim at `scheduler.rs:158-167`). The
manual force-run entry points — `run_job_manual` (`scheduler.rs:79`) and
`execute_job_now` (`scheduler.rs:69`) — never touch it. Its four callers all
run with **no overlap guard**:

- the `cron_run` tool (`src/tools/cron_run.rs:114`)
- the gateway `POST /cron/{id}/run` handler (`src/gateway/cron_api.rs:386`)
- the TUI cron panel "run" key, which fires a **detached** `tokio::spawn`
  (`src/tui/app.rs:3497-3499`) — the easiest to double-press
- the CLI `run_job_report` (`src/cron/mod.rs:240`)

So two "run now" clicks — or a manual run overlapping a scheduled tick — execute
the same command or agent **concurrently**. For a shell job that mutates state
(or an agent job that appends to session history and burns provider budget),
that is a real double-execution bug.

After this plan, all execution paths (scheduled and manual) claim the same
process-wide in-flight registry. A second concurrent run of the same job id
returns a clear "already running" result instead of executing, and the claim is
released via a drop-guard so a panic or cancellation can never leak it.

## Current state

- `src/cron/scheduler.rs` — the run-scoped set and where it is (not) claimed.
- `src/tools/cron_run.rs`, `src/gateway/cron_api.rs`, `src/tui/app.rs`,
  `src/cron/mod.rs` — the four manual callers (they call `run_job_manual`; they
  do NOT pass an in-flight set today).

The set is created per-`run` and only the scheduled path claims it
(`scheduler.rs:30-31`):

```rust
    let in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
```

`execute_job_now` and `run_job_manual` never reference `in_flight`
(`scheduler.rs:69-96`):

```rust
pub async fn execute_job_now(config: &Config, job: &CronJob) -> (bool, String) {
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
    execute_job_with_retry(config, &security, job).await
}

/// Force-run a job now: execute + record run history + update
/// `last_run`/`last_status`/`last_output`. ...
pub async fn run_job_manual(config: &Config, job: &CronJob) -> (bool, String) {
    let started_at = Utc::now();
    let (success, output) = execute_job_now(config, job).await;
    let finished_at = Utc::now();
    let duration_ms = (finished_at - started_at).num_milliseconds();
    let status = if success { "ok" } else { "error" };
    let _ = record_run(
        config,
        &job.id,
        started_at,
        finished_at,
        status,
        Some(&output),
        duration_ms,
    );
    let _ = record_last_run(config, &job.id, finished_at, success, &output);
    (success, output)
}
```

The scheduled path claims via a raw `insert` + post-`await` `remove`
(`scheduler.rs:158-173`) — note the `remove` runs only if the future completes
normally; a panic/cancel leaks the claim:

```rust
            {
                let mut guard = in_flight.lock().expect("in-flight lock poisoned");
                if !guard.insert(job.id.clone()) {
                    tracing::warn!(
                        "Scheduler job '{}' still running from a previous tick; skipping this cycle",
                        job.id
                    );
                    return (job.id.clone(), true);
                }
            }
            let result =
                execute_and_persist_job(&config, security.as_ref(), &job, &component).await;
            in_flight
                .lock()
                .expect("in-flight lock poisoned")
                .remove(&job.id);
            result
```

The TUI caller (detached spawn — a double keypress spawns two of these)
(`src/tui/app.rs:3496-3499`):

```rust
                let cfg = config.clone();
                tokio::spawn(async move {
                    let _ = crate::cron::scheduler::run_job_manual(&cfg, &job).await;
                });
```

### Coordination with plan 165

`plans/165-cron-batch-stall-spawn.md` spawns due-job batches off the poll task
and also needs an `InFlightGuard`. This plan introduces the **process-wide
registry + `InFlightGuard`**; plan 165 reuses it.

- **If plan 165 has already landed** with a local `InFlightGuard` on the
  run-scoped `Arc`: promote its backing store to the process-wide `static`
  registry from Step 1, keep its `Drop` semantics, and delete the run-scoped
  `Arc` (thread nothing).
- **If plan 165 has NOT landed**: this plan still removes the run-scoped `Arc`
  and moves the scheduled-path claim onto the process-wide registry (Step 3).
  When 165 lands later it will build on the registry this plan created.

Whichever order, the end state is one process-wide registry claimed by both the
scheduled path and the manual path, with an RAII `InFlightGuard`.

## Commands you will need

| Purpose      | Command                                             | Expected on success        |
|--------------|-----------------------------------------------------|----------------------------|
| Format check | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint         | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests        | `cargo test --lib cron`                             | all pass (incl. new tests) |
| Drift        | `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/tools/cron_run.rs src/gateway/cron_api.rs src/tui/app.rs src/cron/mod.rs` | empty before you start |

Do NOT run a bare `cargo test`. `cargo test --lib cron` covers `src/cron/*`,
`src/tools/cron_run.rs`, and `src/gateway/cron_api.rs`.

## Suggested executor toolkit

- If `rust-skills` is available, invoke it for the `OnceLock` / RAII-guard
  pattern before Step 1.
- `std::sync::OnceLock` + `std::sync::Mutex<HashSet<String>>` is the standard
  process-wide-registry pattern; no new dependency.

## Scope

**In scope**:
- `src/cron/scheduler.rs` (registry, `InFlightGuard`, claim in `run_job_manual`,
  migrate the scheduled-path claim)

**Out of scope** (do NOT touch — these are follow-ups explicitly deferred):
- claw-ui per-row "busy" state — a separate UI repo change; note it in the PR,
  do not attempt it here.
- A TUI double-press debounce in `src/tui/app.rs` — the registry already makes
  the second run a no-op ("already running"); a UI debounce is a nice-to-have,
  not required, and would enlarge this change.
- `execute_job_now`/`run_job_manual` **security-policy** threading — that is
  `plans/169-cron-manual-run-shared-policy.md`. This plan changes the in-flight
  claim only; do not also change the `SecurityPolicy` argument here.

## Git workflow

- Branch: `advisor/166-cron-manual-run-inflight-guard`
- Conventional commits, e.g. `fix(cron): guard manual runs with a process-wide in-flight registry`.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a process-wide in-flight registry and an `InFlightGuard`

At module level in `scheduler.rs`, add:

```rust
use std::sync::OnceLock;

/// Process-wide set of cron job ids currently executing, shared by BOTH the
/// scheduled poll loop and every manual force-run entry point. A single
/// registry is what lets a manual "run now" see that a scheduled tick (or a
/// second click) is already running the same job, and refuse to double-execute.
fn in_flight_registry() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static REGISTRY: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// RAII claim on the in-flight registry. Removing the id on `Drop` releases the
/// claim even if the job panics or is cancelled — a post-`await` `remove()`
/// would leak it on any early exit.
struct InFlightGuard {
    id: String,
}

impl InFlightGuard {
    /// Claim `id`. Returns `None` if it is already claimed (still running).
    fn claim(id: &str) -> Option<Self> {
        let mut set = in_flight_registry().lock().expect("in-flight lock poisoned");
        if set.insert(id.to_string()) {
            Some(Self { id: id.to_string() })
        } else {
            None
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = in_flight_registry().lock() {
            set.remove(&self.id);
        }
    }
}
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0 (the type is
unused until later steps; that is fine mid-implementation, but the final build
must have all steps done).

### Step 2: Claim the registry in `run_job_manual`; return "already running"

Change `run_job_manual` (`scheduler.rs:79`) to claim the registry before
executing and release it (via the guard) when done. If the job is already
in-flight, return a clear, non-executing result:

```rust
pub async fn run_job_manual(config: &Config, job: &CronJob) -> (bool, String) {
    let Some(_guard) = InFlightGuard::claim(&job.id) else {
        return (false, format!("cron job '{}' is already running", job.id));
    };
    let started_at = Utc::now();
    let (success, output) = execute_job_now(config, job).await;
    let finished_at = Utc::now();
    let duration_ms = (finished_at - started_at).num_milliseconds();
    let status = if success { "ok" } else { "error" };
    let _ = record_run(
        config,
        &job.id,
        started_at,
        finished_at,
        status,
        Some(&output),
        duration_ms,
    );
    let _ = record_last_run(config, &job.id, finished_at, success, &output);
    (success, output)
    // `_guard` drops here (or on panic/cancel), releasing the claim.
}
```

Design decision (KISS + fail-safe): the "already running" result reports
`success = false` and does NOT record a run row (it never executed). Callers
already surface `output`/`error` to the operator, so the message is visible in
the TUI/console/CLI/tool result.

**Verify**: `cargo test --lib cron` → the existing
`run_job_manual_records_without_rescheduling` test (`scheduler.rs:1032`) still
passes (a single run still records exactly one row).

### Step 3: Migrate the scheduled path onto the same registry

Replace the run-scoped `Arc` claim (`scheduler.rs:158-173`) with an
`InFlightGuard::claim` on the process-wide registry, and delete the
`in_flight: Arc<...>` parameter threaded through `run` → `process_due_jobs`.

- In `process_due_jobs`, remove the `in_flight` parameter and claim inside the
  per-job closure:

```rust
        async move {
            let Some(_guard) = InFlightGuard::claim(&job.id) else {
                tracing::warn!(
                    "Scheduler job '{}' still running from a previous tick; skipping this cycle",
                    job.id
                );
                return (job.id.clone(), true);
            };
            execute_and_persist_job(&config, security.as_ref(), &job, &component).await
        }
```

- In `run`, delete the `in_flight` local (`scheduler.rs:30-31`) and drop it from
  the `process_due_jobs(...)` call (`scheduler.rs:65`).
- Update the three `process_due_jobs(...)` test call sites that pass an
  `in_flight` argument (`scheduler.rs:903`, `:925`, `:947-954`) to drop that
  argument. The test `process_due_jobs_skips_job_already_in_flight`
  (`scheduler.rs:932`) pre-inserts into the run-scoped set; rewrite it to claim
  the process-wide registry first (hold an `InFlightGuard` for `job.id`, or
  insert into `in_flight_registry()`), so the assertion "an in-flight job must
  be skipped, not executed" still holds. **Use a unique job id** in that test so
  a leaked global claim cannot bleed into another test (the job id from
  `cron::add_job` is already unique per row — verify it is).

**Verify**: `cargo fmt --all -- --check` → exit 0.
`cargo clippy --all-targets -- -D warnings` → exit 0.
`cargo test --lib cron` → all pass.

### Step 4: Add a regression test for concurrent manual runs

Add a `#[tokio::test]` to the `tests` module in `scheduler.rs` proving two
concurrent `run_job_manual` calls for the same job do NOT both execute:

- Create a shell job whose command observably mutates state and takes long
  enough to overlap — e.g. `sh -c 'echo x >> counter; sleep 1'` in the temp
  workspace (allowlist `sh`). Or, more deterministically, hold an
  `InFlightGuard` for the job id, then call `run_job_manual` and assert it
  returns the "already running" message and records **no** run row
  (`cron::list_runs(...).len() == 0`), then drop the guard and assert a
  subsequent `run_job_manual` executes and records one row.
- Model the test setup after `run_job_manual_records_without_rescheduling`
  (`scheduler.rs:1032-1049`) for config/job construction.

**Verify**: `cargo test --lib cron` → all pass, including the new test.

## Test plan

- New test in `src/cron/scheduler.rs` `mod tests`: a job already claimed in the
  registry causes `run_job_manual` to return "already running" and record no
  run; releasing the claim lets it run and record exactly one row.
- Existing tests that must stay green:
  - `run_job_manual_records_without_rescheduling` (`scheduler.rs:1032`)
  - `process_due_jobs_skips_job_already_in_flight` (`scheduler.rs:932`, rewritten
    to use the process-wide registry as described in Step 3)
  - `process_due_jobs_marks_component_ok_even_when_idle` (`scheduler.rs:891`) and
    `process_due_jobs_failure_does_not_mark_component_unhealthy`
    (`scheduler.rs:912`) — update their `process_due_jobs(...)` calls to drop the
    `in_flight` argument.
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the new concurrent-manual-run test exists and passes
- [ ] `run_job_manual` claims the registry (`grep -n "InFlightGuard::claim" src/cron/scheduler.rs` shows a hit inside `run_job_manual`)
- [ ] The run-scoped `Arc<Mutex<HashSet<String>>>` param is gone from `process_due_jobs` and `run` (`grep -n "in_flight: &Arc" src/cron/scheduler.rs` returns nothing)
- [ ] Release goes through `Drop` (`grep -n "\.remove(&job.id)" src/cron/scheduler.rs` returns nothing outside the `InFlightGuard` `Drop` impl)
- [ ] Only `src/cron/scheduler.rs` is modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check is non-empty (code moved since this plan was written).
- Plan 165 has already landed and its `JoinSet`/spawn restructuring makes the
  scheduled-path claim in Step 3 conflict such that you cannot reconcile by
  "promote the local guard's backing store to the process-wide registry" —
  report the exact conflict.
- Removing the `in_flight` parameter breaks a test in a way that requires
  changing what the test *asserts about behavior* (as opposed to dropping an
  argument or switching to `in_flight_registry()`).
- You find you also need to change `run_job_manual`'s `SecurityPolicy` argument
  to make this compile — that is plan 169's scope; report rather than expanding.

## Maintenance notes

For the human/agent who owns this after the change lands:

- The registry is a process `static`, so it is shared across unit tests in the
  same test binary. The `InFlightGuard` `Drop` releases claims even on panic, so
  a failing test cannot permanently poison the set — but always use unique job
  ids in tests to avoid accidental cross-test collisions.
- Deferred follow-ups (call these out in the PR body): claw-ui per-row "busy"
  indicator, and an optional TUI double-press debounce in `src/tui/app.rs`. Both
  are UX polish on top of the now-correct core; neither is required for
  correctness.
- Reviewer should scrutinize: (1) the guard is held across the entire
  `execute_job_now(...)`/`execute_and_persist_job(...)` await, not dropped early;
  (2) the "already running" path records **no** run row; (3) both the scheduled
  and all four manual callers now funnel through the same registry.
- Interaction with plan 165: 165's detached-batch design depends on this
  registry + guard for leak-safety; keep the `Drop` semantics intact.
