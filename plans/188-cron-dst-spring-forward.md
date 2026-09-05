# Plan 188: Warn when a timezone cron schedule lands on a DST spring-forward gap

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **THIS IS A MED-CONFIDENCE FINDING.** Step 1 is a mandatory runtime probe that
> must REPRODUCE the DST skip before you write any fix. If it does not reproduce,
> STOP and report — the fix is unnecessary.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/schedule.rs`
> If `src/cron/schedule.rs` changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Confidence**: MED — the exact affected set depends on cron-field cardinality;
  Step 1 confirms the behavior at runtime before any change.
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

For a timezone-qualified cron schedule, `next_run_for_schedule` uses
`cron.after(&localized_from).next()`. The `cron` 0.15 crate skips a local
wall-clock time that does not exist (the DST spring-forward gap:
`LocalResult::None => continue`). For a normalized 5-field expression each of
seconds/minutes/hours has exactly one member, so on the spring-forward date the
whole day is exhausted and the job jumps to the NEXT day. A job at `0 2 * * *` in
`America/New_York` therefore silently does not run on the spring-forward date —
once a year, with no log line and no hint in `next_run`. (Fall-back, the
duplicated hour, is handled correctly.) This plan does NOT change firing behavior
(whether to fire the skipped time is a separate decision); it surfaces a WARNING
at schedule-creation/update time naming the affected date, so the operator is not
silently surprised.

## Current state

- `src/cron/schedule.rs:7-39` — `next_run_for_schedule`, tz branch:

  ```rust
  Schedule::Cron { expr, tz } => {
      let normalized = normalize_expression(expr)?;
      let cron = CronExprSchedule::from_str(&normalized)
          .with_context(|| format!("Invalid cron expression: {expr}"))?;
      if let Some(tz_name) = tz {
          let timezone = chrono_tz::Tz::from_str(tz_name)
              .with_context(|| format!("Invalid IANA timezone: {tz_name}"))?;
          let localized_from = from.with_timezone(&timezone);
          let next_local = cron.after(&localized_from).next().ok_or_else(|| {
              anyhow::anyhow!("No future occurrence for expression: {expr}")
          })?;
          Ok(next_local.with_timezone(&Utc))
      } else {
          cron.after(&from).next().ok_or_else(|| ...)
      }
  }
  ```

- `src/cron/schedule.rs:41-61` — `validate_schedule`. The `Cron` arm currently
  ignores `tz` (`Schedule::Cron { expr, .. }`) and only checks the expression
  parses and has a future occurrence:

  ```rust
  Schedule::Cron { expr, .. } => {
      let _ = normalize_expression(expr)?;
      let _ = next_run_for_schedule(schedule, now)?;
      Ok(())
  }
  ```

  This is the natural home for the warning — it runs on every `add_shell_job` /
  `add_agent_job` / `update_job` (see `src/cron/store.rs:37,79,190`), i.e. exactly
  when a schedule is created or changed.

- `src/cron/schedule.rs:70-83` — `normalize_expression`: a 5-field expression
  becomes `0 <expr>` (6 fields: sec min hour day month weekday), so
  seconds/minutes/hours each have a single value — this single-cardinality is
  what makes the whole spring-forward day get skipped.

- `src/cron/schedule.rs:104-113` — existing tz test
  `next_run_for_schedule_supports_timezone` uses `America/Los_Angeles` and a fixed
  `from` via `Utc.with_ymd_and_hms(...)`. Copy this style for deterministic tests.

Available crate items (already used in this file): `chrono_tz::Tz`,
`chrono::{DateTime, Utc, TimeZone}`, `std::str::FromStr`, `cron::Schedule as
CronExprSchedule`. You will additionally use `chrono::{NaiveDate, LocalResult}`
and `chrono::Duration`.

Known spring-forward date for the probe: **2026-03-08** in `America/New_York`,
where clocks jump 02:00 → 03:00 (so 02:00:00–02:59:59 local does not exist).

Repo conventions: warnings via `tracing::warn!` (see scheduler.rs usage). Errors
via `anyhow`. Keep control flow explicit.

## Commands you will need

| Purpose   | Command                                      | Expected on success       |
|-----------|----------------------------------------------|---------------------------|
| Format    | `cargo fmt --all -- --check`                 | exit 0, no diff           |
| Lint      | `cargo clippy --all-targets -- -D warnings`  | exit 0, no warnings       |
| Tests     | `cargo test --lib cron`                      | all pass, incl. new tests |

Do NOT run a bare `cargo test` (disk-constrained box). Use `--lib cron`.

## Scope

**In scope** (the only file you should modify):
- `src/cron/schedule.rs` — the reproduction test (Step 1), the detection helper,
  the warning in `validate_schedule`, and the fix test.

**Out of scope** (do NOT touch):
- `next_run_for_schedule`'s firing behavior — do NOT change the skip into a fire.
  That is a separate behavior decision; this plan only adds an advisory warning.
- Fall-back (autumn) handling — already correct; do not touch.
- Any store/scheduler/config file — the warning lives entirely in schedule
  validation.

## Git workflow

- Branch: `advisor/188-cron-dst-spring-forward`
- Conventional commit, e.g.
  `feat(cron): warn when a tz schedule skips a DST spring-forward date`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1 (MANDATORY): Reproduce the skip with a runtime probe

Before writing any fix, add a test to `src/cron/schedule.rs::tests` that proves
the crate skips the nonexistent local time. This test asserts CURRENT (buggy)
firing behavior and will remain valid after the fix (the fix does not change
firing).

```rust
    #[test]
    fn tz_schedule_skips_nonexistent_local_time_on_spring_forward() {
        // America/New_York springs forward 2026-03-08: 02:00 local does not
        // exist. A `0 2 * * *` job must therefore skip 03-08 and next fire on
        // 03-09. This documents the crate's skip behavior (the reason for the
        // warning added by this plan).
        let from = Utc.with_ymd_and_hms(2026, 3, 7, 12, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 2 * * *".into(),
            tz: Some("America/New_York".into()),
        };
        let next = next_run_for_schedule(&schedule, from).unwrap();
        let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let next_local = next.with_timezone(&ny);
        assert_eq!(
            next_local.date_naive(),
            chrono::NaiveDate::from_ymd_opt(2026, 3, 9).unwrap(),
            "spring-forward day 2026-03-08 must be skipped; got {next_local}"
        );
    }
```

Run `cargo test --lib cron tz_schedule_skips_nonexistent_local_time_on_spring_forward`.

- If it **passes** (next fire is 2026-03-09, i.e. 03-08 was skipped): the bug
  reproduces — proceed to Step 2.
- If it **fails** (the crate fired on 2026-03-08, e.g. at 03:00): the skip does
  NOT reproduce on this crate version → **STOP and report**. The warning is
  unnecessary and this plan should be closed as REJECTED.

**Verify**: the named test passes (skip reproduced). If not, STOP.

### Step 2: Add the DST-gap detection helper

Add a pure, testable helper to `src/cron/schedule.rs` that returns the upcoming
local dates on which the schedule's wall-clock time falls into a nonexistent
(spring-forward) gap. It enumerates occurrences in the UTC frame — whose naive
components equal the intended wall-clock fields — and checks each against the
target timezone.

```rust
use chrono::{LocalResult, NaiveDate, TimeZone};

/// Scan up to ~400 days (bounded) of upcoming occurrences for a tz-qualified
/// cron schedule and return the local dates whose scheduled wall-clock time does
/// not exist because of a DST spring-forward gap. Coarse schedules (daily/weekly)
/// are fully covered; very-high-frequency schedules are bounded by the iteration
/// cap and may not be scanned to the DST date (a single skipped instance there is
/// immaterial).
fn dst_skipped_dates(expr: &str, tz_name: &str, from: DateTime<Utc>) -> Result<Vec<NaiveDate>> {
    const MAX_PROBE_DAYS: i64 = 400;
    const MAX_PROBE_ITERS: usize = 5_000;

    let normalized = normalize_expression(expr)?;
    let cron = CronExprSchedule::from_str(&normalized)
        .with_context(|| format!("Invalid cron expression: {expr}"))?;
    let tz = chrono_tz::Tz::from_str(tz_name)
        .with_context(|| format!("Invalid IANA timezone: {tz_name}"))?;

    let horizon = from + ChronoDuration::days(MAX_PROBE_DAYS);
    let mut skipped: Vec<NaiveDate> = Vec::new();

    // Enumerate in the UTC frame: each occurrence's naive Y-M-D H:M:S equals the
    // intended local wall-clock fields (02:00 stays 02:00 regardless of frame).
    for occ in cron.after(&from).take(MAX_PROBE_ITERS) {
        if occ > horizon {
            break;
        }
        let naive = occ.naive_utc();
        if let LocalResult::None = tz.from_local_datetime(&naive) {
            let date = naive.date();
            if skipped.last() != Some(&date) {
                skipped.push(date);
            }
        }
    }
    Ok(skipped)
}
```

`ChronoDuration` is already imported (`chrono::Duration as ChronoDuration`,
schedule.rs:3). Add `use chrono::{LocalResult, NaiveDate, TimeZone};` (or
fully-qualify). `TimeZone` is required because the helper calls
`tz.from_local_datetime(&naive)` — a `chrono::TimeZone` trait method — and
`TimeZone` is NOT in scope at module level in `schedule.rs` (it is imported only
inside `mod tests`), so without this the helper will not compile.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Emit the warning from `validate_schedule`

Change the `Cron` arm of `validate_schedule` to capture `tz` and, when present,
log a warning for each skipped date. Keep returning `Ok(())` — this is advisory,
not a rejection.

```rust
    Schedule::Cron { expr, tz } => {
        let _ = normalize_expression(expr)?;
        let _ = next_run_for_schedule(schedule, now)?;
        if let Some(tz_name) = tz {
            match dst_skipped_dates(expr, tz_name, now) {
                Ok(dates) => {
                    for date in dates {
                        tracing::warn!(
                            target: "cron",
                            expr = %expr,
                            tz = %tz_name,
                            date = %date,
                            "cron schedule falls on a nonexistent local time (DST spring-forward); it will be skipped that day"
                        );
                    }
                }
                // Detection is best-effort; never fail validation because the
                // probe errored (the schedule itself already validated above).
                Err(e) => tracing::debug!(target: "cron", error = %e, "DST probe failed"),
            }
        }
        Ok(())
    }
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Test the detection helper deterministically

Add a test that `dst_skipped_dates` reports 2026-03-08 for the New York `0 2 * *
*` case, and reports nothing for a time that always exists (e.g. `0 12 * * *`,
noon).

```rust
    #[test]
    fn dst_skipped_dates_flags_spring_forward_and_ignores_safe_times() {
        let from = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();

        let skipped = dst_skipped_dates("0 2 * * *", "America/New_York", from).unwrap();
        assert!(
            skipped.contains(&chrono::NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()),
            "expected 2026-03-08 to be flagged, got {skipped:?}"
        );

        // Noon always exists → no gap.
        let safe = dst_skipped_dates("0 12 * * *", "America/New_York", from).unwrap();
        assert!(
            !safe.contains(&chrono::NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()),
            "noon must never be flagged as a DST gap, got {safe:?}"
        );
    }
```

**Verify**: `cargo test --lib cron` → all pass, including both new tests from
Steps 1 and 4.

### Step 5: Mutation check

Temporarily change the helper's `LocalResult::None` match to `LocalResult::Single(_)`
(so it flags the wrong case) and run
`cargo test --lib cron dst_skipped_dates_flags_spring_forward_and_ignores_safe_times`.
The test MUST fail. Restore `LocalResult::None` and confirm it passes. This proves
the test detects the actual spring-forward gap, not an unconditional result.

**Verify**: with the mutated match the test fails; with `LocalResult::None` it
passes.

## Test plan

- New tests in `src/cron/schedule.rs::tests`:
  - `tz_schedule_skips_nonexistent_local_time_on_spring_forward` (Step 1 —
    reproduces the crate skip; stays as documentation).
  - `dst_skipped_dates_flags_spring_forward_and_ignores_safe_times` (Step 4 —
    detection correctness + no false positive at noon).
- Structural pattern: `next_run_for_schedule_supports_timezone`
  (`src/cron/schedule.rs:104-113`).
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] Step 1 probe reproduced the skip (else the plan is REJECTED, not done)
- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; both new tests exist and pass
- [ ] With the `LocalResult` match mutated, the detection test fails (Step 5)
- [ ] `grep -n "DST spring-forward\|nonexistent local time" src/cron/schedule.rs`
      shows the warning was added
- [ ] `next_run_for_schedule` firing behavior is unchanged (only a warning was
      added; no `LocalResult` handling added to the firing path)
- [ ] No files outside `src/cron/schedule.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- Step 1's probe does NOT reproduce the skip (the crate fired on 2026-03-08) —
  the fix is unnecessary; report and mark REJECTED.
- `validate_schedule` / `next_run_for_schedule` no longer match the excerpts —
  the codebase has drifted (e.g. someone already handles `LocalResult::None`).
- The `cron` crate version changed from `0.15` (check `Cargo.lock`) and the probe
  behavior differs — re-confirm with Step 1 before proceeding.
- Any step's verification fails twice after a reasonable fix attempt.
- The fix appears to require changing firing behavior or editing a file outside
  `schedule.rs` — it must not.

## Maintenance notes

- The detection is advisory only. If the project later decides a skipped
  spring-forward occurrence SHOULD instead fire at the post-gap instant (e.g.
  03:00), that is a separate behavior change to `next_run_for_schedule`, not to
  this warning.
- The probe is bounded (`MAX_PROBE_DAYS = 400`, `MAX_PROBE_ITERS = 5_000`), so
  sub-daily schedules may not be scanned all the way to the next DST date. That is
  intentional: a job that runs many times a day loses nothing meaningful by
  skipping one instance. A reviewer should confirm the bound cannot make
  validation slow (a coarse daily/weekly job is ≤400 tz lookups).
- A reviewer should confirm the warning fires at add/update time (via
  `validate_schedule`) and that a detection error never fails validation.
