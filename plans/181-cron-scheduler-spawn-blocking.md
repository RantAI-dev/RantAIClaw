# Plan 181: Run the scheduler's blocking SQLite calls inside `spawn_blocking`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs`
> If `src/cron/scheduler.rs` changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none (suggested after `plans/179`; interacts with `plans/180` — see Maintenance notes)
- **Category**: perf / bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

The cron scheduler runs synchronous rusqlite calls **directly on the tokio
async task** with no `spawn_blocking`: `due_jobs` on every poll tick, and
`record_run` / `remove_job` / `update_job` / `reschedule_after_run` after each
run. rusqlite is blocking file I/O, so each of these parks a tokio runtime
worker on disk (plus, before `plans/179`, per-call DDL scans). Under lock
contention that stall blocks unrelated gateway and channel work sharing the
runtime. The gateway already does this correctly — `src/gateway/cron_api.rs`
wraps every store call in `spawn_blocking` (its module doc, lines 9–11, says so
explicitly, and handlers at lines 113, 139, 227, 259, 319, 335, 362 follow the
pattern). This plan brings the scheduler to parity.

## Current state

### The gateway pattern to copy (`src/gateway/cron_api.rs`)

Module doc (lines 7–11):

```rust
//! Cron jobs live in the per-profile sqlite store (`workspace_dir/cron/jobs.db`),
//! NOT in `config.toml`, so these handlers do not touch the config write lock.
//! They clone the running `Config` (for `workspace_dir` + `autonomy`) and call
//! the synchronous `crate::cron` store functions inside `spawn_blocking`
//! (rusqlite is blocking).
```

An example handler (lines 113–116):

```rust
    let jobs = tokio::task::spawn_blocking(move || cron::list_jobs(&cfg))
        .await
        .map_err(err_500)?
        .map_err(err_500)?;
```

The closure takes ownership of a **cloned** `Config` (`let cfg = cfg_snapshot(&state);`
before the call) plus any owned args (e.g. an owned `id: String`).

### The five scheduler call sites to wrap (`src/cron/scheduler.rs`)

1. `run()` — `due_jobs` (lines 56–63):

```rust
        let jobs = match due_jobs(&config, Utc::now()) {
            Ok(jobs) => jobs,
            Err(e) => {
                crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                tracing::warn!("Scheduler query failed: {e}");
                continue;
            }
        };
```

2. `persist_job_result` — `record_run` (lines 284–292):

```rust
    let _ = record_run(
        config,
        &job.id,
        started_at,
        finished_at,
        if success { "ok" } else { "error" },
        Some(output),
        duration_ms,
    );
```

3. `persist_job_result` — `remove_job` (lines 296–298):

```rust
            if let Err(e) = remove_job(config, &job.id) {
                tracing::warn!("Failed to remove one-shot cron job after success: {e}");
            }
```

4. `persist_job_result` — `update_job` (lines 303–312):

```rust
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
```

5. `persist_job_result` — `reschedule_after_run` (lines 317–319):

```rust
    if let Err(e) = reschedule_after_run(config, job, success, output) {
        tracing::warn!("Failed to persist scheduler run result: {e}");
    }
```

Both `run()` and `persist_job_result` are `async fn`, so `.await` is available.
`config` is a `&Config`; `job` is a `&CronJob`. `CronJob` derives `Clone`
(cloned as `job.clone()` at scheduler.rs:952). `spawn_blocking` needs `'static`
data, so **clone** `config` and the needed owned values into each closure.

`spawn_blocking(f).await` returns `Result<T, tokio::task::JoinError>` where `T`
is the store fn's own `Result<..>`. So a store call that returns `Result<()>`
becomes `Result<Result<()>, JoinError>` — handle both layers.

## Commands you will need

| Purpose   | Command                                             | Expected on success        |
|-----------|-----------------------------------------------------|----------------------------|
| Format    | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint      | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests     | `cargo test --lib cron`                             | all pass                   |

Do NOT run bare `cargo test` (disk-constrained). Scope to `cron`.

## Scope

**In scope** (the only file you should modify):

- `src/cron/scheduler.rs` — wrap the five call sites above in
  `tokio::task::spawn_blocking`.

**Out of scope** (do NOT touch):

- `run_job_manual`'s `record_run` / `record_last_run` (lines 85, 94) — NOT in
  this plan's five sites; leave them (they belong to `plans/180`).
- The `record_last_run` at line 302 — also not in this plan's five.
- The store functions in `src/cron/store.rs`.
- The gateway (`src/gateway/cron_api.rs`) — reference only.

## Git workflow

- Branch: `advisor/181-cron-scheduler-spawn-blocking`
- Conventional commit, e.g. `perf(cron): run scheduler SQLite calls inside spawn_blocking`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Wrap `due_jobs` in `run()`

Replace the `match due_jobs(&config, Utc::now())` block (lines 56–63) with a
version that clones `config` into the blocking closure and unwraps both result
layers:

```rust
        let jobs = {
            let cfg = config.clone();
            match tokio::task::spawn_blocking(move || due_jobs(&cfg, Utc::now())).await {
                Ok(Ok(jobs)) => jobs,
                Ok(Err(e)) => {
                    crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                    tracing::warn!("Scheduler query failed: {e}");
                    continue;
                }
                Err(e) => {
                    crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                    tracing::warn!("Scheduler due_jobs task failed: {e}");
                    continue;
                }
            }
        };
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Wrap the four `persist_job_result` call sites

`persist_job_result` receives `config: &Config` and `job: &CronJob`. Clone the
needed owned data into each closure.

`record_run` (lines 284–292) →

```rust
    {
        let cfg = config.clone();
        let job_id = job.id.clone();
        let status = if success { "ok" } else { "error" };
        let out = output.to_string();
        if let Err(e) =
            tokio::task::spawn_blocking(move || record_run(&cfg, &job_id, started_at, finished_at, status, Some(&out), duration_ms))
                .await
        {
            tracing::warn!("Failed to join cron record_run task: {e}");
        }
    }
```

(Keeping the original "discard the inner store Result" behavior — this plan only
moves the call off the async worker. If `plans/180` already landed, this site
will instead log the inner `Err`; preserve that logging — see Maintenance notes.)

`remove_job` (lines 296–298) →

```rust
            let cfg = config.clone();
            let job_id = job.id.clone();
            match tokio::task::spawn_blocking(move || remove_job(&cfg, &job_id)).await {
                Ok(Err(e)) => tracing::warn!("Failed to remove one-shot cron job after success: {e}"),
                Err(e) => tracing::warn!("Failed to join cron remove_job task: {e}"),
                Ok(Ok(())) => {}
            }
```

`update_job` (lines 303–312) →

```rust
            let cfg = config.clone();
            let job_id = job.id.clone();
            match tokio::task::spawn_blocking(move || {
                update_job(
                    &cfg,
                    &job_id,
                    CronJobPatch {
                        enabled: Some(false),
                        ..CronJobPatch::default()
                    },
                )
            })
            .await
            {
                Ok(Err(e)) => tracing::warn!("Failed to disable one-shot cron job: {e}"),
                Err(e) => tracing::warn!("Failed to join cron update_job task: {e}"),
                Ok(Ok(_)) => {}
            }
```

`reschedule_after_run` (lines 317–319) → clone `job` (it takes `&CronJob`):

```rust
    {
        let cfg = config.clone();
        let job_clone = job.clone();
        let out = output.to_string();
        match tokio::task::spawn_blocking(move || reschedule_after_run(&cfg, &job_clone, success, &out)).await {
            Ok(Err(e)) => tracing::warn!("Failed to persist scheduler run result: {e}"),
            Err(e) => tracing::warn!("Failed to join cron reschedule task: {e}"),
            Ok(Ok(())) => {}
        }
    }
```

Note: `success` is a `bool` (Copy) and `started_at`/`finished_at`/`duration_ms`
are Copy, so they move into the closure without cloning. `output` is `&str`, so
it must be turned into an owned `String` (`output.to_string()`).

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Final validation

**Verify**:
- `cargo fmt --all -- --check` → exit 0, no diff.
- `cargo clippy --all-targets -- -D warnings` → exit 0.
- `cargo test --lib cron` → all pass (the existing `persist_job_result_*`,
  `process_due_jobs_*`, and `run_job_manual_*` tests exercise these paths).

## Test plan

- No new test is required: this is a threading/offloading change with no
  behavior change, and it is covered by existing async tests that drive the
  scheduled path end to end:
  - `persist_job_result_records_run_and_reschedules_shell_job`
  - `persist_job_result_success_deletes_one_shot` (exercises `remove_job`)
  - `persist_job_result_failure_disables_one_shot` (exercises `update_job`)
  - `persist_job_result_disables_shell_one_shot_instead_of_refiring`
  - `process_due_jobs_skips_job_already_in_flight` (exercises `due_jobs`)
  These run under `#[tokio::test]`, so `spawn_blocking().await` is valid in them.
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0
- [ ] `grep -c "spawn_blocking" src/cron/scheduler.rs` returns at least 5
- [ ] `grep -n "spawn_blocking(move || due_jobs" src/cron/scheduler.rs` returns
      a match (the poll-loop `due_jobs` is now offloaded). Note: the production
      call is the only non-`cron::`-prefixed `due_jobs(&...)`; the test file uses
      `cron::due_jobs(...)`, so do not rely on a bare `due_jobs(&config` grep.
- [ ] No files outside `src/cron/scheduler.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- Any of the five call sites do not match the excerpts.
- A `spawn_blocking` closure fails to compile because a captured value is not
  `Send`/`'static` — the fix is to clone owned data into the closure, not to
  change a store signature; if that is not enough, report it.
- `cargo test --lib cron` fails twice after a reasonable fix attempt (in
  particular, a test that runs outside a tokio runtime cannot call
  `spawn_blocking` — but all five sites are inside `async fn`s driven by
  `#[tokio::test]`, so this should not arise).
- The fix appears to require touching any file other than
  `src/cron/scheduler.rs`.

## Maintenance notes

- **Ordering with `plans/180`**: 180 adds error logging (and a mid-run-deletion
  existence re-check) to the `record_run` at line 284 and the `record_last_run`
  at line 302. This plan wraps the line-284 `record_run` in `spawn_blocking`. If
  180 landed first, the line-284 site will already log the inner `Err` — keep
  that logging inside the `Ok(Err(e)) => ...` arm when you wrap it (do not
  regress to discarding). If this plan lands first, 180 must adapt to the
  `spawn_blocking(...).await` shape. Re-run the drift check.
- **Suggested after `plans/179`**: WAL + `busy_timeout` shorten the window each
  blocking call holds a worker, but this plan is independent and correct on its
  own.
- Reviewer should scrutinize: every closure owns its captures (no borrow of
  `config`/`job` escapes into the blocking task), and no site silently swallows
  a `JoinError` without a log line.
