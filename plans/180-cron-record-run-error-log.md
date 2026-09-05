# Plan 180: Log cron run-history write failures instead of discarding them

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
- **Depends on**: none (interacts with `plans/181` — see Maintenance notes)
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

The scheduler discards every run-history write result with `let _ = ...`. If a
job is deleted while it is executing, the `cron_runs` INSERT violates the
foreign key (`cron_runs.job_id REFERENCES cron_jobs(id)`) and fails **silently**:
the job ran (possibly with side effects), but its history has a hole and
`last_status` still shows the previous result. `execute_and_persist_job` even
returns `success` for a run whose result was never persisted. Operators reading
the run log then trust a record that is quietly wrong. This plan makes each of
those writes log on failure — and, for the run-history INSERT, distinguish the
"job was deleted mid-run" case with a clear message — without ever failing the
job on a history-write error.

## Current state

`src/cron/scheduler.rs` has four discarded run-history writes.

### Manual path — `run_job_manual` (lines 79–96)

```rust
pub async fn run_job_manual(config: &Config, job: &CronJob) -> (bool, String) {
    let started_at = Utc::now();
    let (success, output) = execute_job_now(config, job).await;
    let finished_at = Utc::now();
    let duration_ms = (finished_at - started_at).num_milliseconds();
    let status = if success { "ok" } else { "error" };
    let _ = record_run(                       // <-- line 85
        config,
        &job.id,
        started_at,
        finished_at,
        status,
        Some(&output),
        duration_ms,
    );
    let _ = record_last_run(config, &job.id, finished_at, success, &output);  // <-- line 94
    (success, output)
}
```

### Scheduled path — `persist_job_result` (lines 284–319)

```rust
    let _ = record_run(                       // <-- line 284
        config,
        &job.id,
        started_at,
        finished_at,
        if success { "ok" } else { "error" },
        Some(output),
        duration_ms,
    );

    if is_one_shot(job) {
        if job.delete_after_run && success {
            if let Err(e) = remove_job(config, &job.id) { ... }
        } else {
            // Not opted into auto-delete (or it failed): keep the row for history
            // but disable it so the poller can't re-fire this already-past `At`.
            let _ = record_last_run(config, &job.id, finished_at, success, output);  // <-- line 302
            if let Err(e) = update_job(...) { ... }
        }
        return success;
    }
```

### Imports

`src/cron/scheduler.rs` lines 3–6 import from `crate::cron`:
`due_jobs, next_run_for_schedule, record_last_run, record_run, remove_job,
reschedule_after_run, update_job, ...`. It does **not** import `get_job`. Use
the fully-qualified `crate::cron::get_job(...)` so the import block is untouched.

Conventions:

- `record_run` (`src/cron/store.rs:304`) returns `Result<()>`; on a deleted job
  its `INSERT INTO cron_runs` fails the FK → returns `Err`.
- `record_last_run` (`src/cron/store.rs:254`) is an `UPDATE cron_jobs ... WHERE
  id = ?4`; on a deleted job it simply affects 0 rows and returns `Ok(())` — so
  it does not error on mid-run deletion. Its `let _` only needs plain error
  logging (a genuine SQL failure), no existence re-check.
- Structured `tracing::warn!` with fields is the house style — e.g. this file's
  `tracing::warn!(target: "scheduler", error = %e, "...")` (lines 49–53).

## Commands you will need

| Purpose   | Command                                             | Expected on success        |
|-----------|-----------------------------------------------------|----------------------------|
| Format    | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint      | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests     | `cargo test --lib cron`                             | all pass                   |

Do NOT run bare `cargo test` (disk-constrained). Scope to `cron`.

## Scope

**In scope** (the only file you should modify):

- `src/cron/scheduler.rs` — the four `let _ =` sites at lines 85, 94, 284, 302,
  plus a new test in its `#[cfg(test)] mod tests`.

**Out of scope** (do NOT touch):

- `src/cron/store.rs` — the store functions' bodies stay as they are.
- The `remove_job`/`update_job` `if let Err(e)` sites (lines 296, 303) — they
  already log; leave them.
- Failing the job on a history-write error — the job's `(success, output)` and
  `persist_job_result`'s return value must NOT change.
- `plans/181`'s `spawn_blocking` wrapping — do not add it here.

## Git workflow

- Branch: `advisor/180-cron-record-run-error-log`
- Conventional commit, e.g. `fix(cron): log run-history write failures instead of discarding them`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Log the two `record_run` sites, distinguishing mid-run deletion

Replace the `let _ = record_run(...)` at line 85 (`run_job_manual`) with:

```rust
    if let Err(e) = record_run(
        config,
        &job.id,
        started_at,
        finished_at,
        status,
        Some(&output),
        duration_ms,
    ) {
        if crate::cron::get_job(config, &job.id).is_err() {
            tracing::warn!(job_id = %job.id, "cron job deleted while running; run history not recorded");
        } else {
            tracing::warn!(job_id = %job.id, error = %e, "failed to record cron run history");
        }
    }
```

Replace the `let _ = record_run(...)` at line 284 (`persist_job_result`) with
the same shape, keeping its `if success { "ok" } else { "error" }` status arg
and `Some(output)`:

```rust
    if let Err(e) = record_run(
        config,
        &job.id,
        started_at,
        finished_at,
        if success { "ok" } else { "error" },
        Some(output),
        duration_ms,
    ) {
        if crate::cron::get_job(config, &job.id).is_err() {
            tracing::warn!(job_id = %job.id, "cron job deleted while running; run history not recorded");
        } else {
            tracing::warn!(job_id = %job.id, error = %e, "failed to record cron run history");
        }
    }
```

(The existence re-check runs only on the error path, so the happy path adds no
extra query.)

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Log the two `record_last_run` sites

Replace the `let _ = record_last_run(...)` at line 94 (`run_job_manual`) with:

```rust
    if let Err(e) = record_last_run(config, &job.id, finished_at, success, &output) {
        tracing::warn!(job_id = %job.id, error = %e, "failed to record cron last-run fields");
    }
```

Replace the `let _ = record_last_run(...)` at line 302 (`persist_job_result`,
one-shot keep-but-disable branch) with (note: `output` here is `&str`, so no `&`):

```rust
            if let Err(e) = record_last_run(config, &job.id, finished_at, success, output) {
                tracing::warn!(job_id = %job.id, error = %e, "failed to record cron last-run fields");
            }
```

**Verify**: `grep -n "let _ = record_run\|let _ = record_last_run" src/cron/scheduler.rs`
→ **no** matches.

### Step 3: Add a return-contract regression test — a mid-run deletion is survived, not fatal

Add this test inside `mod tests` in `src/cron/scheduler.rs` (after
`run_job_manual_records_without_rescheduling`, ~line 1049).

**What this test does and does NOT cover.** The logging side-effect itself —
that a `tracing::warn!` fired at all, and that the deleted-job branch was chosen
over the generic error branch — is genuinely hard to unit-test without a tracing
capture subscriber, which this crate's `mod tests` does not set up. So this test
is a RETURN-CONTRACT guard ONLY: it proves that when a job's row is gone,
`record_run` fails internally but `run_job_manual` still returns success and does
not panic. It does NOT pin the logging behavior or the branch choice — on the
pre-fix code (`let _ = ...`) it passes identically. The STRUCTURAL presence of
the `get_job(...)` existence re-check and the distinct "job deleted while
running" branch is enforced instead by the grep-based Done criteria below, not by
this test.

```rust
#[tokio::test]
async fn run_job_manual_survives_missing_job_row() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp).await;
    // A job value whose row was never inserted into the store: recording its
    // run must fail the FK INSERT internally but must not fail the run itself.
    let job = test_job("echo ok");

    let (ok, output) = run_job_manual(&config, &job).await;
    assert!(ok, "the command ran successfully");
    assert!(output.contains("ok"));
    // No run row exists because the parent job row is absent — the write result
    // was swallowed here (logging is a side-effect this test does not assert),
    // not propagated.
    assert!(cron::list_runs(&config, &job.id, 10).unwrap().is_empty());
}
```

`test_job` (lines 591–614) builds a `CronJob` with id `"test-job"` that is not
inserted into any store, so `record_run`'s FK INSERT fails — exactly the
mid-run-deletion shape. `cron::list_runs` is already imported via
`use crate::cron::{self, ...}` (test module, line 574).

**Verify**: `cargo test --lib cron` → all pass, including the new test.

### Step 4: Final validation

**Verify**:
- `cargo fmt --all -- --check` → exit 0, no diff.
- `cargo clippy --all-targets -- -D warnings` → exit 0.
- `cargo test --lib cron` → all pass.

## Test plan

- New test `run_job_manual_survives_missing_job_row` in
  `src/cron/scheduler.rs` `mod tests` — the regression: a run against an
  absent job row is logged and swallowed, the run still reports success, and no
  run row is created. On the pre-fix code this test also passes (the `let _`
  already swallows), so its value is guarding that the new logging path does not
  change the return contract or panic. The logging behavior and the deleted-job
  branch choice are covered STRUCTURALLY by the grep Done criteria below, not by
  this test.
- Existing tests that exercise these sites must still pass:
  `run_job_manual_records_without_rescheduling`,
  `persist_job_result_records_run_and_reschedules_shell_job`,
  `persist_job_result_failure_disables_one_shot`.
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the new test exists and passes
- [ ] `grep -n "let _ = record_run\|let _ = record_last_run" src/cron/scheduler.rs`
      returns no matches
- [ ] `grep -n "job deleted while running" src/cron/scheduler.rs` returns 2 matches
- [ ] `grep -n "crate::cron::get_job" src/cron/scheduler.rs` returns 2 matches —
      the existence re-check on each `record_run` error path. This is the
      STRUCTURAL proof that the deleted-job branch sits in the error arm (the
      unit test cannot assert branch choice), paired with the grep above proving
      its message string is present in both places.
- [ ] No files outside `src/cron/scheduler.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The four `let _ =` sites at lines 85, 94, 284, 302 do not match the excerpts.
- `crate::cron::get_job` is not resolvable (it should be — it is `pub use`d in
  `src/cron/mod.rs:16-19`); report rather than adding an import.
- `cargo test --lib cron` fails twice after a reasonable fix attempt.
- The fix appears to require touching any file other than
  `src/cron/scheduler.rs`.

## Maintenance notes

- `plans/181-cron-scheduler-spawn-blocking.md` wraps the `record_run` /
  `record_last_run` / `remove_job` / `update_job` / `due_jobs` calls in
  `tokio::task::spawn_blocking`. That plan and this one both edit the
  `record_run` site at line 284 and the `record_last_run` site at line 302. If
  181 lands first, those calls will already be `spawn_blocking(...).await`
  expressions returning `Result<Result<()>, JoinError>` — adapt the logging to
  match, keeping the mid-run-deletion existence re-check. If this plan lands
  first, 181 must preserve the logging when it wraps the calls. Re-run the drift
  check either way.
- Reviewer should scrutinize: the job's return value is unchanged (a
  history-write failure must never turn a successful run into a failure).
