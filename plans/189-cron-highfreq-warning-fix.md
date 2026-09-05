# Plan 189: Fix the always-firing high-frequency warning for cron agent jobs

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs`
> If it changed since this plan was written, compare the "Current state"
> excerpts against the live code before proceeding; on a mismatch, treat it as
> a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`warn_if_high_frequency_agent_job` is meant to warn when an **agent** cron job is
scheduled more often than every 5 minutes. Its `Schedule::Cron` branch
(`scheduler.rs:339-348`) computes the next occurrence after `now` and the next
occurrence after `now + 1s`, and treats a delta under 5 minutes as "too
frequent". But `cron.after()` returns the first occurrence strictly **after** the
given instant, so unless an actual occurrence falls inside that one-second
window, **both calls return the same occurrence**, the delta is ~0, and the
warning fires for *every* cron agent job — including a once-a-day `0 9 * * *`.
The log is pure noise and trains operators to ignore it.

After this plan the warning compares the gap between two **consecutive**
occurrences, so it fires only for genuinely high-frequency schedules (e.g.
`*/1 * * * *`) and stays silent for normal ones (e.g. daily).

## Current state

- `src/cron/scheduler.rs` — `warn_if_high_frequency_agent_job` and its buggy
  `Cron` branch.
- `src/cron/schedule.rs` — `next_run_for_schedule`, whose `Cron` arm uses
  `cron.after(&from).next()` (strictly-after semantics, `schedule.rs:18` and
  `:23`).

The buggy function (`scheduler.rs:333-358`):

```rust
fn warn_if_high_frequency_agent_job(job: &CronJob) {
    if !matches!(job.job_type, JobType::Agent) {
        return;
    }
    let too_frequent = match &job.schedule {
        Schedule::Every { every_ms } => *every_ms < 5 * 60 * 1000,
        Schedule::Cron { .. } => {
            let now = Utc::now();
            match (
                next_run_for_schedule(&job.schedule, now),
                next_run_for_schedule(&job.schedule, now + chrono::Duration::seconds(1)),
            ) {
                (Ok(a), Ok(b)) => (b - a).num_minutes() < 5,
                _ => false,
            }
        }
        Schedule::At { .. } => false,
    };

    if too_frequent {
        tracing::warn!(
            "Cron agent job '{}' is scheduled more frequently than every 5 minutes",
            job.id
        );
    }
}
```

The intended semantics are visible in the `Every` branch: compare the *interval
between fires* against 5 minutes. `next_run_for_schedule` returns the first
occurrence strictly after `from` (`src/cron/schedule.rs:7-27`):

```rust
pub fn next_run_for_schedule(schedule: &Schedule, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    match schedule {
        Schedule::Cron { expr, tz } => {
            ...
            cron.after(&from)
                .next()
                .ok_or_else(|| anyhow::anyhow!("No future occurrence for expression: {expr}"))
        }
        ...
    }
}
```

So the correct approach is: `a = next_run_for_schedule(schedule, now)`, then
`b = next_run_for_schedule(schedule, a)` — because `after(a)` is strictly after
`a`, `b` is the occurrence *following* `a`. `b - a` is the true gap between
consecutive fires.

Worked examples:
- `0 9 * * *` (daily 9am): `b - a` = 1440 min → not `< 5` → no warning. ✓
- `*/1 * * * *` (every minute): `b - a` = 1 min → `< 5` → warning. ✓

`warn_if_high_frequency_agent_job` is called once per job execution at
`scheduler.rs:193` (inside `execute_and_persist_job`) and its only effect is a
`tracing::warn!` side effect — which is why Step 1 extracts a pure, testable
predicate.

## Commands you will need

| Purpose      | Command                                             | Expected on success        |
|--------------|-----------------------------------------------------|----------------------------|
| Format check | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint         | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests        | `cargo test --lib cron`                             | all pass (incl. new tests) |
| Drift        | `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs` | empty before you start |

Do NOT run a bare `cargo test`.

## Scope

**In scope**:
- `src/cron/scheduler.rs` (fix the predicate; extract a testable helper; add tests)

**Out of scope** (do NOT touch):
- `src/cron/schedule.rs` — `next_run_for_schedule` is correct; do not change it.
- The 5-minute threshold value or the `Every`/`At` branches — only the `Cron`
  branch is wrong.

## Git workflow

- Branch: `advisor/189-cron-highfreq-warning-fix`
- Conventional commits, e.g. `fix(cron): compare consecutive occurrences in the high-frequency warning`.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Extract a pure predicate `is_high_frequency_agent_job`

Because the current function's only output is a `tracing::warn!` side effect
(hard to assert in a test), split the decision into a pure function that returns
`bool`, and keep `warn_if_high_frequency_agent_job` as a thin wrapper that logs.

Target shape:

```rust
/// True when an *agent* job is scheduled more often than every 5 minutes.
/// For `Cron`, compares the gap between two CONSECUTIVE occurrences — NOT
/// `next(now)` vs `next(now + 1s)`, which return the same occurrence unless one
/// happens to fall in that 1-second window (the bug this replaces, which warned
/// on every cron agent job including a daily `0 9 * * *`).
fn is_high_frequency_agent_job(job: &CronJob) -> bool {
    if !matches!(job.job_type, JobType::Agent) {
        return false;
    }
    match &job.schedule {
        Schedule::Every { every_ms } => *every_ms < 5 * 60 * 1000,
        Schedule::Cron { .. } => {
            let now = Utc::now();
            match next_run_for_schedule(&job.schedule, now) {
                Ok(a) => match next_run_for_schedule(&job.schedule, a) {
                    Ok(b) => (b - a).num_minutes() < 5,
                    Err(_) => false,
                },
                Err(_) => false,
            }
        }
        Schedule::At { .. } => false,
    }
}

fn warn_if_high_frequency_agent_job(job: &CronJob) {
    if is_high_frequency_agent_job(job) {
        tracing::warn!(
            "Cron agent job '{}' is scheduled more frequently than every 5 minutes",
            job.id
        );
    }
}
```

Keep the call site at `scheduler.rs:193` unchanged (it still calls
`warn_if_high_frequency_agent_job(job)`).

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Add unit tests for the predicate

Add `#[tokio::test]` or plain `#[test]` cases to the `tests` module in
`scheduler.rs`. Use the existing `test_job(...)` helper (`scheduler.rs:591`) to
build a job, then override `job_type`/`schedule`. Cover:

1. A daily cron agent job does NOT warn:
   ```rust
   let mut job = test_job("");
   job.job_type = JobType::Agent;
   job.schedule = crate::cron::Schedule::Cron { expr: "0 9 * * *".into(), tz: None };
   assert!(!is_high_frequency_agent_job(&job), "a daily cron agent job must not warn");
   ```
2. An every-minute cron agent job DOES warn:
   ```rust
   job.schedule = crate::cron::Schedule::Cron { expr: "*/1 * * * *".into(), tz: None };
   assert!(is_high_frequency_agent_job(&job), "an every-minute cron agent job must warn");
   ```
3. A shell job with the same every-minute schedule does NOT warn (guarded by the
   `JobType::Agent` check):
   ```rust
   job.job_type = JobType::Shell;
   assert!(!is_high_frequency_agent_job(&job));
   ```
4. (Optional, documents intent) `Schedule::Every { every_ms: 60_000 }` agent job
   warns; `Schedule::Every { every_ms: 600_000 }` does not.

Name the tests by behavior, e.g.
`daily_cron_agent_job_is_not_high_frequency`,
`every_minute_cron_agent_job_is_high_frequency`,
`shell_job_is_never_flagged_high_frequency`.

**Verify**: `cargo test --lib cron` → all pass, including the new tests.
Specifically confirm the daily-cron test fails on the OLD predicate: to prove the
test is not vacuous, you may temporarily paste the old two-arg
`next(now)`/`next(now+1s)` logic and confirm
`daily_cron_agent_job_is_not_high_frequency` FAILS, then restore the fix. (This
step is verification only — do not commit the reverted logic.)

## Test plan

- New tests in `src/cron/scheduler.rs` `mod tests` (see Step 2): daily cron agent
  job not flagged; every-minute cron agent job flagged; shell job never flagged.
- Structural pattern: reuse `test_job(...)` (`scheduler.rs:591`) as in the
  existing agent-job tests (e.g. `run_agent_job_blocks_readonly_mode`,
  `scheduler.rs:859`).
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the three named predicate tests exist and pass
- [ ] The `Cron` branch no longer uses `now + chrono::Duration::seconds(1)`
      (`grep -n "seconds(1)" src/cron/scheduler.rs` returns nothing)
- [ ] A pure `is_high_frequency_agent_job` helper exists
      (`grep -n "fn is_high_frequency_agent_job" src/cron/scheduler.rs`)
- [ ] Only `src/cron/scheduler.rs` is modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check is non-empty (code moved since this plan was written).
- `next_run_for_schedule(schedule, a)` returns an error for a valid 5-field cron
  expression in your test (it should not — `after(a).next()` yields the following
  occurrence). Report the error rather than working around it.
- A test verification fails twice after a reasonable fix attempt.

## Maintenance notes

For the human/agent who owns this after the change lands:

- If timezone-aware cron schedules ever need the warning, note that
  `next_run_for_schedule` already handles `tz` (`schedule.rs:14-21`); the
  consecutive-occurrence approach works for tz schedules too since `after(a)` is
  computed in the schedule's own timezone.
- Reviewer should scrutinize: the gap is measured between `a` and `b = next(a)`
  (consecutive), not between `now` and `next(now)` — the latter would measure the
  *time until the next fire*, not the *interval*, and would spuriously flag a job
  whose next fire is soon.
- The threshold is `< 5` minutes with `num_minutes()` truncation: a 4-minute gap
  flags, an exact 5-minute gap does not — consistent with the `Every` branch's
  `< 5 * 60 * 1000` ms.
