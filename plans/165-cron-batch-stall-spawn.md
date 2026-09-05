# Plan 165: Stop one hung cron job from freezing the whole scheduler poll loop

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/daemon/mod.rs`
> If either file changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none (but coordinate with `plans/166-cron-manual-run-inflight-guard.md` — both change the in-flight tracking in `src/cron/scheduler.rs`; see "Coordination" below)
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

The scheduler's poll loop awaits `process_due_jobs(...)` **inline** on the same
task that drives `interval.tick()` (`scheduler.rs:65`). `process_due_jobs`
drains its entire `buffer_unordered` stream before returning
(`scheduler.rs:179-183`). A single agent job can run for
`AGENT_JOB_TIMEOUT_SECS` (600s) multiplied by `1 + scheduler_retries`, with
exponential backoff sleeps between attempts (`scheduler.rs:107-134`,
`with_timeout` at `scheduler.rs:457-468`). So **one slow or hung job holds the
poll task for up to ~30 minutes and blocks every other schedule from firing**,
and the per-tick `mark_component_ok` health refresh (`scheduler.rs:38`) never
runs during that window, so health reports stale liveness. `scheduler.max_concurrent`
only bounds concurrency *within* one batch, not the poll cadence.

After this plan, execution runs on separate tasks: `interval.tick()` keeps
firing on schedule regardless of how long any job takes, health stays fresh,
and a panicking or cancelled job cannot leak an in-flight claim.

## Current state

- `src/cron/scheduler.rs` — the cron scheduler. Key regions:

The poll loop awaits the batch inline (`scheduler.rs:35-66`):

```rust
    loop {
        interval.tick().await;
        // Keep scheduler liveness fresh even when there are no due jobs.
        crate::health::mark_component_ok(SCHEDULER_COMPONENT);

        // Refresh the config half once per poll tick. ...
        match Config::load_or_init().await {
            Ok(cfg) => security.apply_config(&cfg.autonomy),
            Err(e) => tracing::warn!(
                target: "scheduler",
                error = %e,
                "config reload failed; keeping the previously applied autonomy settings"
            ),
        }

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

The run-scoped in-flight set is created in `run` and threaded through
(`scheduler.rs:30-31`):

```rust
    let in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
```

`process_due_jobs` claims each job, drains a `buffer_unordered` stream, then
removes the claim (`scheduler.rs:139-184`):

```rust
async fn process_due_jobs(
    config: &Config,
    security: &Arc<SecurityPolicy>,
    jobs: Vec<CronJob>,
    component: &str,
    in_flight: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) {
    // Refresh scheduler health on every successful poll cycle, including idle cycles.
    crate::health::mark_component_ok(component);

    let max_concurrent = config.scheduler.max_concurrent.max(1);
    let mut in_flight_stream = stream::iter(jobs.into_iter().map(|job| {
        let config = config.clone();
        let security = Arc::clone(security);
        let component = component.to_owned();
        let in_flight = Arc::clone(in_flight);
        async move {
            // Claim the job; skip if a previous (long-running) invocation is still going ...
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

Notice the per-job closure **already clones** `config` and `Arc`-clones
`security`/`in_flight` and owns `component` as a `String` — so each job future
is already `'static`-ready and can be moved into a spawned task without new
lifetime work.

How the scheduler task is supervised and shut down
(`src/daemon/mod.rs:125-136` and `:167-170`):

```rust
    if scheduler_enabled(&config) {
        let scheduler_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "scheduler",
            initial_backoff,
            max_backoff,
            shutdown.clone(),
            move || {
                let cfg = scheduler_cfg.clone();
                async move { crate::cron::scheduler::run(cfg).await }
            },
        ));
    }
```

```rust
    // The remaining components (channels/heartbeat/scheduler) have no in-flight
    // request state to save, so abort them directly.
    for handle in &handles {
```

**Shutdown contract that MUST be preserved**: the daemon shuts the scheduler
down by `abort()`-ing its `JoinHandle` (the `for handle in &handles` loop). If
you spawn job tasks with a bare `tokio::spawn`, they become **detached** and
survive the abort — a leak on every shutdown. The fix below owns a
`tokio::task::JoinSet` inside `run`, so when `run`'s future is aborted the
`JoinSet` is dropped and **all its tasks are aborted with it**. Preserving this
is a hard requirement.

### Coordination with plan 166

`plans/166-cron-manual-run-inflight-guard.md` hoists the in-flight set to a
**process-wide** registry and introduces an `InFlightGuard` RAII type so a
claim is released on drop (panic/cancel safe). This plan ALSO needs a
drop-guard, because detaching execution makes a leaked claim permanent.

- **If plan 166 has already landed** (a shared registry + `InFlightGuard` exist
  in `scheduler.rs`): reuse them. Do NOT reintroduce the run-scoped `Arc`;
  claim/release through the shared registry's guard inside the spawned task.
- **If plan 166 has NOT landed**: introduce a local `InFlightGuard` in this plan
  (Step 2) that wraps the existing run-scoped `Arc`. When 166 lands later, its
  executor will replace the backing store with the process-wide registry.

Either way the guard type is the same shape (RAII, removes the id on `Drop`).

## Commands you will need

| Purpose      | Command                                             | Expected on success        |
|--------------|-----------------------------------------------------|----------------------------|
| Format check | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint         | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests        | `cargo test --lib cron`                             | all pass (incl. new tests) |
| Drift        | `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/daemon/mod.rs` | empty before you start |

Do NOT run a bare `cargo test` (the full workspace build is disk-heavy on this
box). `cargo test --lib cron` runs every test whose module path contains
`cron`, which covers `src/cron/*`, `src/tools/cron_run.rs`, and
`src/gateway/cron_api.rs`.

## Suggested executor toolkit

- If the `rust-skills` skill is available, invoke it before writing the async
  task-spawning / `JoinSet` code in Step 3.
- `tokio::task::JoinSet` docs: a set of spawned tasks; dropping the set aborts
  all still-running tasks; `join_next()`/`try_join_next()` reap completed ones.

## Scope

**In scope** (the only file you should modify):
- `src/cron/scheduler.rs`

**Out of scope** (do NOT touch):
- `src/daemon/mod.rs` — the supervisor + abort-based shutdown is correct as-is;
  this plan preserves it, it does not change it. (Read it only to confirm the
  shutdown contract above.)
- A **global concurrency cap across overlapping batches** (a process-wide
  `Semaphore`). Today `max_concurrent` bounds only within a batch; with batches
  now overlapping across ticks, distinct jobs from different ticks can exceed
  it. This is a throughput/resource concern, not the acute correctness bug
  (the in-flight set still prevents the *same* job running twice), so it is a
  documented follow-up in "Maintenance notes", not part of this plan. Do not
  add it here.
- The manual-run paths (`run_job_manual`, `execute_job_now`) — those are plan
  166/169 territory.

## Git workflow

- Branch: `advisor/165-cron-batch-stall-spawn`
- Conventional commits, e.g. `fix(cron): run due-job batches off the scheduler poll task`.
  Example from `git log`: `fix(config): one decrypt pass shared by load_or_init and the TUI reload (#567)`.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Confirm you are looking at the right code

Run the drift check:

```
git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/daemon/mod.rs
```

**Verify**: empty output. If not empty, STOP (drift condition).

### Step 2: Add an `InFlightGuard` RAII type and use it in `process_due_jobs`

At module level in `scheduler.rs`, add a guard that removes a job id from the
in-flight set on `Drop`. (If plan 166 already added a guard, reuse it and skip
this addition.)

Target shape:

```rust
/// RAII claim on the in-flight set. Removing the id on `Drop` guarantees the
/// claim is released even if the job task panics or is cancelled — a bare
/// `remove()` after the `.await` would leak the claim on any early exit.
struct InFlightGuard {
    set: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    id: String,
}

impl InFlightGuard {
    /// Claim `id`. Returns `None` if it was already claimed (still running).
    fn claim(
        set: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        id: &str,
    ) -> Option<Self> {
        let mut guard = set.lock().expect("in-flight lock poisoned");
        if guard.insert(id.to_string()) {
            Some(Self { set: Arc::clone(set), id: id.to_string() })
        } else {
            None
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.id);
        }
    }
}
```

Rewrite the per-job closure body in `process_due_jobs` to claim via the guard
and hold it across the `.await` (replacing the manual `insert` /
`in_flight.lock().remove(...)` at `scheduler.rs:158-173`):

```rust
        async move {
            let Some(_guard) = InFlightGuard::claim(&in_flight, &job.id) else {
                tracing::warn!(
                    "Scheduler job '{}' still running from a previous tick; skipping this cycle",
                    job.id
                );
                return (job.id.clone(), true);
            };
            execute_and_persist_job(&config, security.as_ref(), &job, &component).await
            // `_guard` drops here (or on panic/cancel), releasing the claim.
        }
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.
`cargo test --lib cron` → all existing tests pass (the
`process_due_jobs_skips_job_already_in_flight` test at `scheduler.rs:932` still
passes — it pre-claims the id and expects no run recorded).

### Step 3: Spawn each batch off the poll task via an owned `JoinSet`

In `run`, own a `JoinSet` and stop awaiting `process_due_jobs` inline. Each
tick: reap any finished batch tasks, then spawn the current batch.

`process_due_jobs` currently borrows `&config`, `&Arc<SecurityPolicy>`,
`&str`, `&Arc<...>`. A spawned task needs `'static`, so move **owned** clones
into the task and call `process_due_jobs` on the owned values inside the async
block. Target shape for the loop:

```rust
    let mut batches: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    loop {
        interval.tick().await;
        crate::health::mark_component_ok(SCHEDULER_COMPONENT);

        // Reap finished batch tasks so the set doesn't grow unbounded. A batch
        // task returns () and never panics under normal operation; log if it does.
        while let Some(res) = batches.try_join_next() {
            if let Err(e) = res {
                if e.is_panic() {
                    tracing::error!("Scheduler batch task panicked: {e}");
                }
            }
        }

        match Config::load_or_init().await {
            Ok(cfg) => security.apply_config(&cfg.autonomy),
            Err(e) => tracing::warn!(
                target: "scheduler",
                error = %e,
                "config reload failed; keeping the previously applied autonomy settings"
            ),
        }

        let jobs = match due_jobs(&config, Utc::now()) {
            Ok(jobs) => jobs,
            Err(e) => {
                crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                tracing::warn!("Scheduler query failed: {e}");
                continue;
            }
        };

        if jobs.is_empty() {
            // Preserve the idle-cycle health refresh that process_due_jobs used
            // to provide.
            crate::health::mark_component_ok(SCHEDULER_COMPONENT);
            continue;
        }

        // Spawn the batch so a slow/hung job can never stall interval.tick().
        // The batch task owns its clones; the in-flight set (see Step 2 /
        // plan 166) prevents a job from a still-running earlier batch from
        // being run again here.
        let config = config.clone();
        let security = Arc::clone(&security);
        let in_flight = Arc::clone(&in_flight);
        batches.spawn(async move {
            process_due_jobs(
                &config,
                &security,
                jobs,
                SCHEDULER_COMPONENT,
                &in_flight,
            )
            .await;
        });
    }
```

Notes:
- Keep the `in_flight` declaration at `scheduler.rs:30-31` (unless plan 166 has
  replaced it with a process-wide registry, in which case follow 166's shape
  and drop this local `Arc`).
- Do NOT convert `batches.spawn` into a bare `tokio::spawn` — the owned
  `JoinSet` is what preserves abort-on-shutdown (see the shutdown contract in
  "Current state").
- `process_due_jobs`'s own leading `mark_component_ok(component)` at
  `scheduler.rs:146-147` still runs inside the spawned task; the loop-top and
  idle-path refreshes above keep health fresh on the poll cadence even while a
  batch is stuck.

**Verify**: `cargo fmt --all -- --check` → exit 0.
`cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Add a regression test proving the poll cadence is decoupled

Add a `#[tokio::test]` to the `tests` module in `scheduler.rs` that proves the
poll loop is no longer blocked by a long-running batch. Because the full `run`
loop never returns, test the decoupling at the unit that matters:

- Assert that spawning a batch into a `JoinSet` returns control immediately even
  though the batch task is still running. Concretely: build a `JoinSet`, spawn
  a task that sleeps well past a short deadline, and assert the spawn call plus
  a subsequent `interval.tick()`-sized wait completes far faster than the task's
  sleep. Use `tokio::time` with `tokio::time::pause()`/`advance` OR a real short
  sleep (e.g. spawn sleeps 5s; assert the "poll" side proceeds in < 500ms).
- Keep the existing `process_due_jobs_*` tests green — they call
  `process_due_jobs(...).await` directly and still work because `process_due_jobs`
  is unchanged in signature.

Model the test structure after the existing `with_timeout_reports_timeout_for_slow_job`
test at `scheduler.rs:1051-1060` (it already demonstrates the `tokio::time`
sleep-vs-deadline pattern).

> **Honesty note — what this test proves.** This test exercises **tokio's
> `JoinSet` semantics** (spawning a task returns immediately while the task keeps
> running), not the scheduler's decoupling specifically. Reverting Step 3 (going
> back to the inline `process_due_jobs(...).await`) would NOT fail it, because
> the test never drives the real `run` loop. Keep it as an **illustrative**
> demonstration of the mechanism, but do not treat it as a regression guard. The
> real proof that the poll task is decoupled is the **Done-criteria greps** (no
> `process_due_jobs(...).await` directly awaited on the poll task; no bare
> `tokio::spawn` for job execution; `process_due_jobs` runs only inside a
> `batches.spawn(...)` block; in-flight release via `Drop`).

**Verify**: `cargo test --lib cron` → all pass, including the new test.

## Test plan

- New test in `src/cron/scheduler.rs` `mod tests`: an **illustrative**
  poll-cadence decoupling test — a spawned batch task that sleeps does NOT block
  the spawner (see Step 4). This exercises tokio's `JoinSet` semantics, not the
  scheduler's decoupling; it is not a regression guard (reverting Step 3 would
  not fail it). The real proof is the Done-criteria greps.
- Existing tests that must stay green (do not delete or weaken):
  - `process_due_jobs_marks_component_ok_even_when_idle` (`scheduler.rs:891`)
  - `process_due_jobs_failure_does_not_mark_component_unhealthy` (`scheduler.rs:912`)
  - `process_due_jobs_skips_job_already_in_flight` (`scheduler.rs:932`) — proves
    the `InFlightGuard` claim still skips an already-claimed job.
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the new poll-cadence test exists and passes
- [ ] `run`'s loop no longer contains `process_due_jobs(...).await` directly
      awaited on the poll task (`grep -n "process_due_jobs(&config, &security, jobs, SCHEDULER_COMPONENT" src/cron/scheduler.rs` shows it only inside a `batches.spawn(...)` block)
- [ ] No bare `tokio::spawn(` was introduced for job execution
      (`grep -n "tokio::spawn" src/cron/scheduler.rs` returns nothing — use `JoinSet`)
- [ ] In-flight release goes through the `Drop` impl, not a post-`await`
      `remove()` (`grep -n "\.remove(&job.id)" src/cron/scheduler.rs` returns nothing outside the `Drop` impl)
- [ ] Only `src/cron/scheduler.rs` is modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check in Step 1 is non-empty (code moved since this plan was written).
- You cannot preserve abort-on-shutdown — i.e. the only way you can make it
  compile is a detached `tokio::spawn`. The `JoinSet`-owned-in-`run` design is
  mandatory; if it won't compile, report the exact error rather than falling
  back to detached spawns.
- Making `process_due_jobs` spawnable requires changing its **signature**
  (it should stay `async fn process_due_jobs(&Config, &Arc<SecurityPolicy>,
  Vec<CronJob>, &str, &Arc<Mutex<HashSet<String>>>)`; only the call site moves
  into a task with owned clones).
- Any `process_due_jobs_*` test starts failing and the fix would require
  changing what the test asserts about behavior (as opposed to updating a
  constructor call).
- Plan 166 has landed and its in-flight registry shape conflicts with Step 2/3
  such that you cannot reconcile them by "reuse 166's guard" — report the
  conflict.

## Maintenance notes

For the human/agent who owns this after the change lands:

- **Deferred follow-up — global concurrency cap.** With batches now overlapping
  across ticks, distinct jobs from consecutive ticks can run concurrently beyond
  `scheduler.max_concurrent` (which still caps only within a single batch). If
  resource pressure appears, add a process-wide `tokio::sync::Semaphore`
  (`max_concurrent` permits) acquired inside each job future before executing.
  This was deliberately left out to keep this risky change small and reversible.
- Reviewer should scrutinize: (1) that shutdown still aborts in-flight jobs — the
  `JoinSet` must be a local owned by `run`, never a `static` or detached spawn;
  (2) that health `mark_component_ok` still fires on the poll cadence when a
  batch is stuck (loop-top + idle path), not only inside `process_due_jobs`;
  (3) that the `InFlightGuard` is held across the whole `execute_and_persist_job`
  await, not dropped early.
- Interaction with plan 166: if 166 lands after this, its executor must replace
  the run-scoped `in_flight` `Arc` with the process-wide registry while keeping
  the `InFlightGuard` drop semantics this plan relies on.
