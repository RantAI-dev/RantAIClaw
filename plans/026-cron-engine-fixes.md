# Plan 026: Cron engine correctness fixes (one-shot re-fire, agent timeout, in-flight guard, dead flag, manual-run helper)

> **Context**: The cron engine (`src/cron/*`) is otherwise mature — typed
> `Schedule`, a polling scheduler daemon, a per-profile sqlite store, and good
> unit coverage — but a deep scan (2026-07-18) surfaced correctness gaps that
> bite when the feature is used harder (which the new web UI, plans 027/028,
> will cause). This plan fixes the engine bugs FIRST so the HTTP API + UI sit on
> a correct foundation.
>
> **Executor note**: Self-contained. Repo verification baseline —
> `cargo fmt --all -- --check` · `cargo clippy --all-targets -- -D warnings` ·
> `cargo test`. Disk-constrained box: prefer
> `CARGO_TARGET_DIR=<shared warm target> cargo test --lib cron` +
> `touch`-ing changed files (see the repo's disk memo). ALWAYS run
> `cargo test --lib` before merging (store/scheduler are hot paths). Every fix
> ships a repro test that FAILS before and PASSES after.
>
> **Branch**: `feat/cron-engine-fixes` (non-`main`). One commit per task.
> **Risk**: MEDIUM (scheduler/store behavior; no exposure-boundary change).
> **Schema**: NO config-schema change (agent timeout is a hardcoded const like
> the shell one; `scheduler.enabled` already exists). So NO schema-version bump
> and NO drift-snapshot regen.

## Baseline evidence (confirmed against main, 2026-07-18)

- **B1 — one-shot *shell* `At` jobs re-fire forever.** `add_shell_job`
  (`src/cron/store.rs:30-65`) hardcodes `delete_after_run = 0` (the literal in the
  INSERT VALUES clause, `store.rs:48`). The scheduler's
  one-shot handler `is_one_shot_auto_delete` (`src/cron/scheduler.rs:252-254`)
  requires `delete_after_run && At`, so a shell `At` job (`delete_after_run=false`)
  falls through to `reschedule_after_run` (`scheduler.rs:245`), which sets
  `next_run = next_run_for_schedule(At{at}) = at` (`src/cron/schedule.rs:28` — the
  same, now-past, instant). `due_jobs` (`store.rs:170`,
  `WHERE enabled=1 AND next_run<=now`) then re-selects it every poll → it
  re-runs indefinitely. Reached by CLI `cron add-at` / `cron once`
  (`src/cron/mod.rs:78,178` → `add_shell_job`) and the `cron_add` tool's shell
  branch.
- **B3a — agent jobs have no execution timeout.** `run_agent_job`
  (`scheduler.rs:138-193`) awaits `crate::agent::run` with no timeout; shell jobs
  cap at `SHELL_JOB_TIMEOUT_SECS = 120` (`scheduler.rs:19,434-509`). A hung agent
  job blocks a `max_concurrent` slot forever.
- **B3b — duplicate concurrent execution.** `next_run` only advances *after* a
  run finishes (`persist_job_result` → `reschedule_after_run`). A job whose
  runtime exceeds the poll interval (`scheduler_poll_secs`, default 15) still has
  `next_run <= now` on the next tick and is fired **again** concurrently
  (`process_due_jobs`, `scheduler.rs:91-119`). No per-job in-flight guard.
- **B2 — `[scheduler].enabled` is dead config.** The daemon gates the scheduler
  supervisor on `config.cron.enabled` (`src/daemon/mod.rs:117`);
  `SchedulerConfig.enabled` (`src/config/schema.rs:2374-2377`, default `true`) is
  never read anywhere else. Two overlapping enable flags mislead operators/UI.
- **B4 — force-run path diverges from the scheduled path.** `execute_job_now`
  (`scheduler.rs:51-54`) is used by the `cron_run` tool
  (`src/tools/cron_run.rs:124-139`, which records its own run/last-run) and will
  be used by the new HTTP run endpoint (027). The record logic is inline and
  will be duplicated in two callers unless extracted.

**Out of scope (documented, intentionally NOT fixed here):**
- **B5 — `SessionTarget` Main vs Isolated is inert** (`scheduler.rs:168-180`
  matches both into one branch). Implementing real "main session" routing is a
  separate feature (touches the session system), not a bug fix. Handled in 028
  by NOT exposing it as a functional control (label it reserved). Do not
  implement session routing here.
- **B6 — `Every` reschedules from finish time** (drift by execution latency;
  `reschedule_after_run` computes from `now`). Minor; correct fixed-rate
  semantics + catch-up policy is a deliberate follow-up. Left as-is; noted here
  so it isn't re-audited.

## Scope
- **In**: `src/cron/scheduler.rs`, `src/cron/store.rs` (no change expected — B1
  fix lives in the scheduler), `src/tools/cron_run.rs`, `src/daemon/mod.rs`.
- **Out**: `src/config/schema.rs` (no new keys), `src/cron/schedule.rs`,
  `src/cron/types.rs`, any exposure boundary.

---

## Task 1 — B1: one-shot `At` jobs must never reschedule (fix infinite re-fire)

**Files:** `src/cron/scheduler.rs` (edit `persist_job_result` + rename
`is_one_shot_auto_delete` → `is_one_shot`; add a test).

**Semantics:** An `At` job fires exactly once — there is no next occurrence.
After it runs: if the caller opted into auto-delete AND it succeeded, remove it;
otherwise keep the row (for run history) but **disable** it so the poller can't
re-fire the past instant. This keeps the existing agent-one-shot behavior
(`delete_after_run=true`) and fixes the shell-one-shot bug (`delete_after_run=false`).

- [ ] **Step 1 — Write the failing repro test.** Add to the `tests` module in
  `src/cron/scheduler.rs`:

```rust
#[tokio::test]
async fn persist_job_result_disables_shell_one_shot_instead_of_refiring() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp).await;
    let at = Utc::now() + ChronoDuration::minutes(10);
    // Shell one-shot as created by CLI `add-at`/`once`: delete_after_run = false.
    let job = cron::add_shell_job(
        &config,
        Some("one-shot-shell".into()),
        crate::cron::Schedule::At { at },
        "echo hi",
    )
    .unwrap();
    assert!(!job.delete_after_run, "shell one-shot has delete_after_run=false");
    let started = Utc::now();
    let finished = started + ChronoDuration::milliseconds(10);

    let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
    assert!(success);

    // Must survive (user did NOT opt into auto-delete) …
    let stored = cron::get_job(&config, &job.id).unwrap();
    // … but be DISABLED so it never re-fires. Regression: it used to reschedule
    // next_run to the past `at` instant and re-run on every poll cycle forever.
    assert!(
        !stored.enabled,
        "a fired one-shot At job must be disabled, not rescheduled"
    );
    assert_eq!(stored.last_status.as_deref(), Some("ok"));
    // And it must not be selected as due again.
    let due = cron::due_jobs(&config, Utc::now() + ChronoDuration::days(365)).unwrap();
    assert!(
        due.iter().all(|j| j.id != job.id),
        "disabled one-shot must not be due"
    );
}
```

- [ ] **Step 2 — Run it, confirm it FAILS.**
  `CARGO_TARGET_DIR=<shared> cargo test --lib cron::scheduler::tests::persist_job_result_disables_shell_one_shot_instead_of_refiring`
  Expected: FAIL (`stored.enabled` is `true` — job was rescheduled, not disabled).

- [ ] **Step 3 — Fix.** Replace `is_one_shot_auto_delete` (`scheduler.rs:252-254`)
  with a generalized predicate:

```rust
/// An `At` job fires exactly once — there is no "next" occurrence, so it must
/// never be rescheduled (its next_run would be the same, now-past, instant,
/// which the poller would re-select every cycle → infinite re-fire). After it
/// runs we either delete it (`delete_after_run` opt-in, on success) or disable
/// it (keeping the row for its run history).
fn is_one_shot(job: &CronJob) -> bool {
    matches!(job.schedule, Schedule::At { .. })
}
```

  And change the branch in `persist_job_result` (`scheduler.rs:224-243`) from
  `if is_one_shot_auto_delete(job) {` to:

```rust
    if is_one_shot(job) {
        if job.delete_after_run && success {
            if let Err(e) = remove_job(config, &job.id) {
                tracing::warn!("Failed to remove one-shot cron job after success: {e}");
            }
        } else {
            // Not opted into auto-delete (or it failed): keep the row for history
            // but disable it so the poller can't re-fire this already-past `At`.
            let _ = record_last_run(config, &job.id, finished_at, success, output);
            if let Err(e) = update_job(
                config,
                &job.id,
                CronJobPatch {
                    enabled: Some(false),
                    ..CronJobPatch::default()
                },
            ) {
                tracing::warn!("Failed to disable one-shot cron job: {e}");
            }
        }
        return success;
    }
```

  > Note: `record_last_run`'s signature is
  > `(config, job_id, finished_at: DateTime<Utc>, success: bool, output: &str)`
  > (`src/cron/store.rs:254-260`) — the fourth arg is `success`.

- [ ] **Step 4 — Run the repro + the two existing one-shot tests, confirm PASS.**
  `CARGO_TARGET_DIR=<shared> cargo test --lib cron::scheduler`
  Expected: the new test PASSES, and `persist_job_result_success_deletes_one_shot`
  + `persist_job_result_failure_disables_one_shot` still PASS (delete-on-success
  and disable-on-failure for `delete_after_run=true` agent jobs are preserved).

- [ ] **Step 5 — Commit.**
  `git add -A && git commit -m "fix(cron): disable fired one-shot At jobs instead of rescheduling them to a past instant"`

---

## Task 2 — B3a: agent-job execution timeout

**Files:** `src/cron/scheduler.rs` (add `AGENT_JOB_TIMEOUT_SECS` const + a
`with_timeout` helper; wrap the Agent arm in `execute_job_with_retry`; test).

- [ ] **Step 1 — Write the failing tests** (in the `tests` module):

```rust
#[tokio::test]
async fn with_timeout_reports_timeout_for_slow_job() {
    let (ok, msg) = with_timeout(Duration::from_millis(20), async {
        time::sleep(Duration::from_secs(30)).await;
        (true, "should not finish".to_string())
    })
    .await;
    assert!(!ok);
    assert!(msg.contains("timed out"), "{msg}");
}

#[tokio::test]
async fn with_timeout_passes_through_fast_job() {
    let (ok, msg) =
        with_timeout(Duration::from_secs(5), async { (true, "quick".to_string()) }).await;
    assert!(ok);
    assert_eq!(msg, "quick");
}
```

- [ ] **Step 2 — Run, confirm FAIL** (compile error: `with_timeout` undefined).

- [ ] **Step 3 — Implement.** Add near the top consts (`scheduler.rs:18-20`):

```rust
const AGENT_JOB_TIMEOUT_SECS: u64 = 600;
```

  Add the helper (place beside `run_job_command_with_timeout`):

```rust
/// Apply a wall-clock timeout to a job future, returning a uniform timed-out
/// result. Used to bound agent jobs (which call `crate::agent::run` and have no
/// inner timeout of their own, unlike shell jobs).
async fn with_timeout(
    timeout: Duration,
    fut: impl std::future::Future<Output = (bool, String)>,
) -> (bool, String) {
    match time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => (
            false,
            format!("agent job timed out after {}s", timeout.as_secs_f64()),
        ),
    }
}
```

  Wrap the Agent arm in `execute_job_with_retry` (`scheduler.rs:66-69`):

```rust
        let (success, output) = match job.job_type {
            JobType::Shell => run_job_command(config, security, job).await,
            JobType::Agent => {
                with_timeout(
                    Duration::from_secs(AGENT_JOB_TIMEOUT_SECS),
                    run_agent_job(config, security, job),
                )
                .await
            }
        };
```

- [ ] **Step 4 — Run, confirm PASS.**
  `CARGO_TARGET_DIR=<shared> cargo test --lib cron::scheduler::tests::with_timeout`

- [ ] **Step 5 — Commit.**
  `git add -A && git commit -m "fix(cron): bound agent-job execution with a 600s timeout"`

---

## Task 3 — B3b: per-job in-flight guard (no duplicate concurrent execution)

**Files:** `src/cron/scheduler.rs` (thread an in-memory in-flight set through
`run` → `process_due_jobs`; skip jobs already running; test).

**Design:** The scheduler `run()` loop is a single task, so a process-local
`Arc<Mutex<HashSet<String>>>` is sufficient. Claim a job id before executing and
release it after; if a due job's id is already claimed (still running from a
previous tick), skip it this cycle. The lock is only held for the insert/remove
(never across `.await`), so `std::sync::Mutex` is fine.

- [ ] **Step 1 — Write the failing test:**

```rust
#[tokio::test]
async fn process_due_jobs_skips_job_already_in_flight() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp).await;
    let job = cron::add_job(&config, "*/5 * * * *", "echo hi").unwrap();
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));
    let in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    // Pretend the job is still running from a previous tick.
    in_flight.lock().unwrap().insert(job.id.clone());
    let component = unique_component("scheduler-inflight");

    process_due_jobs(&config, &security, vec![job.clone()], &component, &in_flight).await;

    // It must have been skipped → no run recorded.
    let runs = cron::list_runs(&config, &job.id, 10).unwrap();
    assert!(runs.is_empty(), "an in-flight job must be skipped, not executed");
}
```

- [ ] **Step 2 — Run, confirm FAIL** (compile error: `process_due_jobs` takes 4
  args, test passes 5).

- [ ] **Step 3 — Implement.** In `run` (`scheduler.rs:22-49`), create the set once
  before the loop and pass it in:

```rust
    let in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    crate::health::mark_component_ok(SCHEDULER_COMPONENT);

    loop {
        interval.tick().await;
        crate::health::mark_component_ok(SCHEDULER_COMPONENT);

        let jobs = match due_jobs(&config, Utc::now()) {
            Ok(jobs) => jobs,
            Err(e) => {
                crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                tracing::warn!("Scheduler query failed: {e}");
                continue;
            }
        };

        process_due_jobs(&config, &security, jobs, SCHEDULER_COMPONENT, &in_flight).await;
    }
```

  Update `process_due_jobs` (`scheduler.rs:91-119`) signature + body:

```rust
async fn process_due_jobs(
    config: &Config,
    security: &Arc<SecurityPolicy>,
    jobs: Vec<CronJob>,
    component: &str,
    in_flight: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) {
    crate::health::mark_component_ok(component);

    let max_concurrent = config.scheduler.max_concurrent.max(1);
    let mut in_flight_stream = stream::iter(jobs.into_iter().map(|job| {
        let config = config.clone();
        let security = Arc::clone(security);
        let component = component.to_owned();
        let in_flight = Arc::clone(in_flight);
        async move {
            // Claim the job; skip if a previous (long-running) invocation is still
            // going, so a job slower than the poll interval isn't run concurrently.
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
        }
    }))
    .buffer_unordered(max_concurrent);

    while let Some((job_id, success)) = in_flight_stream.next().await {
        if !success {
            tracing::warn!("Scheduler job '{job_id}' failed");
        }
    }
}
```

  Update the two existing callers of `process_due_jobs` in the `tests` module
  (`process_due_jobs_marks_component_ok_even_when_idle` and
  `process_due_jobs_failure_does_not_mark_component_unhealthy`) to pass a fresh
  empty in-flight set as the 5th arg, e.g.:

```rust
    let in_flight = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    process_due_jobs(&config, &security, Vec::new(), &component, &in_flight).await;
```

- [ ] **Step 4 — Run, confirm PASS.**
  `CARGO_TARGET_DIR=<shared> cargo test --lib cron::scheduler`

- [ ] **Step 5 — Commit.**
  `git add -A && git commit -m "fix(cron): skip a scheduled job that is still running from a previous tick"`

---

## Task 4 — B2: wire `[scheduler].enabled` into the daemon gate

**Files:** `src/daemon/mod.rs` (gate on both flags via a testable predicate; test).

- [ ] **Step 1 — Write the failing test** (in `src/daemon/mod.rs` `tests` module):

```rust
#[test]
fn scheduler_enabled_requires_both_cron_and_scheduler_flags() {
    let mut c = crate::config::Config::default();
    assert!(scheduler_enabled(&c), "both flags default to true");
    c.scheduler.enabled = false;
    assert!(!scheduler_enabled(&c), "scheduler.enabled=false disables it");
    c.scheduler.enabled = true;
    c.cron.enabled = false;
    assert!(!scheduler_enabled(&c), "cron.enabled=false disables it");
}
```

- [ ] **Step 2 — Run, confirm FAIL** (compile error: `scheduler_enabled` undefined).

- [ ] **Step 3 — Implement.** Add the predicate and use it at the gate
  (`daemon/mod.rs:117`):

```rust
/// The background scheduler runs only when BOTH the cron feature master switch
/// (`[cron].enabled`) and the scheduler-loop switch (`[scheduler].enabled`) are
/// on. Previously only `[cron].enabled` was honored, leaving `[scheduler].enabled`
/// dead config.
fn scheduler_enabled(config: &Config) -> bool {
    config.cron.enabled && config.scheduler.enabled
}
```

  Change `if config.cron.enabled {` (line 117) to `if scheduler_enabled(&config) {`.
  Update the else-branch log (`daemon/mod.rs:131`) to:

```rust
        tracing::info!("Scheduler disabled (cron.enabled/scheduler.enabled); supervisor not started");
```

  Update the `SchedulerConfig.enabled` doc comment (`src/config/schema.rs:2375`)
  to reflect that it now gates the daemon loop together with `[cron].enabled`:

```rust
    /// Enable the background scheduler loop. Both this and `[cron].enabled` must
    /// be true for the daemon to run the scheduler.
```

- [ ] **Step 4 — Run, confirm PASS.**
  `CARGO_TARGET_DIR=<shared> cargo test --lib daemon::`

- [ ] **Step 5 — Commit.**
  `git add -A && git commit -m "fix(cron): honor [scheduler].enabled in the daemon gate (was dead config)"`

---

## Task 5 — B4: shared manual-run helper (`run_job_manual`)

**Files:** `src/cron/scheduler.rs` (add `pub async fn run_job_manual`; test) +
`src/tools/cron_run.rs` (call it, removing the inline record block).

**Semantics:** A manual/force run executes the job now, records a run + updates
`last_run/status/output`, but — unlike the scheduled path — does NOT reschedule,
auto-delete one-shots, or run delivery. Manual runs are for testing and must not
shift the schedule or consume a one-shot. Callers keep their own security/approval
gate. Plan 027's HTTP `POST /cron/{id}/run` reuses this same helper.

- [ ] **Step 1 — Write the failing test** (in `scheduler.rs` `tests` module):

```rust
#[tokio::test]
async fn run_job_manual_records_without_rescheduling() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp).await;
    let job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
    let before = cron::get_job(&config, &job.id).unwrap().next_run;

    let (ok, _) = run_job_manual(&config, &job).await;
    assert!(ok);

    let after = cron::get_job(&config, &job.id).unwrap();
    assert_eq!(after.next_run, before, "a manual run must NOT reschedule the job");
    assert_eq!(cron::list_runs(&config, &job.id, 10).unwrap().len(), 1);
    assert_eq!(after.last_status.as_deref(), Some("ok"));
}
```

- [ ] **Step 2 — Run, confirm FAIL** (compile error: `run_job_manual` undefined).

- [ ] **Step 3 — Implement** in `scheduler.rs` (beside `execute_job_now`):

```rust
/// Force-run a job now: execute + record run history + update
/// `last_run`/`last_status`/`last_output`. Unlike the scheduled path this does
/// NOT reschedule, auto-delete one-shots, or run delivery — a manual run is for
/// testing and must not shift the schedule or consume a one-shot. Callers must
/// enforce their own security/approval gate before calling.
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

  In `src/tools/cron_run.rs`, replace the inline block (`cron_run.rs:124-139`,
  the `started_at`/`execute_job_now`/`record_run`/`record_last_run` lines) with:

```rust
        let (success, output) = cron::scheduler::run_job_manual(&self.config, &job).await;
        let status = if success { "ok" } else { "error" };
```

  (The tool's `ToolResult` output JSON at `cron_run.rs:141-154` still references
  `status` and `output`; drop the now-unused `duration_ms` field from that JSON
  object, or keep it by having the tool recompute — simplest is to drop it.)

  **Also remove the now-orphaned import** `use chrono::Utc;` (`cron_run.rs:6`):
  after deleting the inline block, `Utc` was only used at the deleted lines 124/126,
  so it becomes unused and would fail the `clippy -D warnings` done-criterion.
  `use crate::cron::{self, JobType};` (line 3) stays — both `cron` and `JobType`
  remain in use.

- [ ] **Step 4 — Run, confirm PASS** (new test + all 4 existing `cron_run` tool
  tests):
  `CARGO_TARGET_DIR=<shared> cargo test --lib "cron::scheduler::tests::run_job_manual OR tools::cron_run"`
  (or run `cron::scheduler` and `tools::cron_run` separately).

- [ ] **Step 5 — Commit.**
  `git add -A && git commit -m "refactor(cron): extract shared run_job_manual helper for force-runs (tool + upcoming HTTP)"`

---

## Done criteria (all must hold)
- [ ] Task 1: shell one-shot `At` job is disabled (not rescheduled) after firing;
  repro test passes; both existing one-shot tests still pass.
- [ ] Task 2: agent jobs are bounded by a 600s timeout; `with_timeout` tests pass.
- [ ] Task 3: a job still running from a prior tick is skipped; repro test passes.
- [ ] Task 4: daemon gates on `cron.enabled && scheduler.enabled`; predicate test passes.
- [ ] Task 5: `run_job_manual` is `pub`, used by the `cron_run` tool; test passes.
- [ ] `cargo test --lib cron daemon tools::cron_run` all green.
- [ ] `cargo fmt --all -- --check` clean; `cargo clippy --all-targets -- -D warnings`
  clean on changed files (run scoped clippy; strict-clippy-delta is a post-merge gate).
- [ ] No schema change; no exposure-boundary change.

## STOP conditions
- If renaming `is_one_shot_auto_delete` breaks a caller outside `scheduler.rs`
  (there should be none — it's a private fn) — stop and report.
- If the in-flight `std::sync::Mutex` triggers a `clippy::await_holding_lock`
  warning, the guard is being held across an `.await` — restructure so the lock
  is dropped before `execute_and_persist_job` (as written it is; verify).
- If any existing cron test regresses in a way the plan doesn't predict — stop;
  do not "fix" the test to match new behavior without confirming the behavior is
  intended.

## Rollback
Each task is its own commit. Revert per-commit. No migrations, no schema change,
no persisted-state format change → clean rollback.
