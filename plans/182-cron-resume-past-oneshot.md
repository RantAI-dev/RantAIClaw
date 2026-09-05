# Plan 182: Refuse re-enabling a completed one-shot cron job whose `at` is in the past

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/store.rs src/cron/mod.rs src/cron/scheduler.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

A one-shot cron job (`Schedule::At`) is contractually "runs once". When such a
job fires **without** `delete_after_run`, the scheduler keeps the row for its
run history but disables it (`enabled = 0`); its `next_run` is left at the
original, now-past `at` instant. Re-enabling that row — via the TUI `p` toggle,
`rantaiclaw cron resume <id>`, or `PUT /cron/{id} {enabled:true}` — makes it
IMMEDIATELY due again, because the poller selects `enabled = 1 AND next_run <=
now`. The job re-executes with no warning. For an agent one-shot that means the
agent re-sends its message; for a shell one-shot it re-runs the command. The
operator asked for a reminder that already fired, toggled it back on to inspect
it, and silently triggered it again. This plan makes that path fail loudly and
tell the caller to supply a fresh `at`.

## Current state

Three call paths all funnel through `store::update_job`, so one fix covers all
three:

- `src/cron/mod.rs:294-303` — `resume_job` patches only `enabled: Some(true)`:

  ```rust
  pub fn resume_job(config: &Config, id: &str) -> Result<CronJob> {
      update_job(
          config,
          id,
          CronJobPatch {
              enabled: Some(true),
              ..CronJobPatch::default()
          },
      )
  }
  ```

- `src/tui/app.rs:3504-3514` — the TUI `p` toggle calls `resume_job` when the
  job is currently disabled.
- `src/gateway/cron_api.rs:291-323` — `update_cron` (`PUT /cron/{id}`) builds a
  `CronJobPatch` from the request body (`enabled: body.enabled`) and calls
  `cron::update_job`.

- `src/cron/store.rs:185-252` — `update_job`. It applies patch fields, then only
  recomputes `next_run` when `schedule_changed` is true (lines 220-222), and
  writes `job.next_run` back verbatim (line 243). For a re-enabled past `At` job
  this writes the stale past instant:

  ```rust
  pub fn update_job(config: &Config, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
      let mut job = get_job(config, job_id)?;
      let mut schedule_changed = false;

      if let Some(schedule) = patch.schedule {
          validate_schedule(&schedule, Utc::now())?;
          job.schedule = schedule;
          job.expression = schedule_cron_expression(&job.schedule).unwrap_or_default();
          schedule_changed = true;
      }
      // ... other patch fields ...
      if let Some(enabled) = patch.enabled {
          job.enabled = enabled;
      }
      // ... delivery, model, session_target, delete_after_run ...

      if schedule_changed {
          job.next_run = next_run_for_schedule(&job.schedule, Utc::now())?;
      }

      with_connection(config, |conn| { /* UPDATE ... next_run = ?12 ... */ })?;
      get_job(config, job_id)
  }
  ```

- `src/cron/store.rs:162-183` — `due_jobs` selects `WHERE enabled = 1 AND
  next_run <= ?1`, so a re-enabled past `At` row is due on the very next poll
  tick.
- The one-shot disable path that creates this state:
  `src/cron/scheduler.rs:294-313` (`persist_job_result`) — sets
  `enabled: Some(false)` after a fired `At` job that did not auto-delete, and its
  own doc comment at `scheduler.rs:324-331` explains exactly why a fired `At`
  must never become due again.

Relevant types (`src/cron/types.rs`):
- `Schedule::At { at: DateTime<Utc> }` (line 68-70).
- `CronJobPatch.enabled: Option<bool>` (line 155) — `Option<bool>` is `Copy`, so
  reading `patch.enabled` after other patch fields are moved out is fine.

Repo conventions: errors use `anyhow::bail!` / `anyhow::anyhow!` (see
`src/cron/schedule.rs:19,49` and `src/cron/store.rs:143,155`). `update_job`
returns `anyhow::Result<CronJob>`. The gateway maps store errors to HTTP status
via `map_store_error` (`src/gateway/cron_api.rs:322`), so a plain `bail!` here
surfaces as an HTTP 400/500 without extra work.

## Commands you will need

| Purpose   | Command                                   | Expected on success        |
|-----------|-------------------------------------------|----------------------------|
| Format    | `cargo fmt --all -- --check`              | exit 0, no diff            |
| Lint      | `cargo clippy --all-targets -- -D warnings` | exit 0, no warnings      |
| Tests     | `cargo test --lib cron`                   | all pass, incl. new test   |

Do NOT run a bare `cargo test` (disk-constrained box — the full workspace test
target is very large). Use the filtered `--lib cron` form above.

## Scope

**In scope** (the only files you should modify):
- `src/cron/store.rs` — add the guard in `update_job`; add a unit test.

**Out of scope** (do NOT touch, even though they look related):
- `src/cron/mod.rs` (`resume_job`/`pause_job`) — the guard belongs in the single
  chokepoint (`update_job`), not in each caller. Leave these untouched.
- `src/tui/app.rs`, `src/gateway/cron_api.rs` — they call `update_job`
  transitively and inherit the fix. Do not add duplicate checks there.
- `src/cron/scheduler.rs` `persist_job_result` — the disable-after-fire behavior
  is correct; do not change how one-shots are disabled.
- The `due_jobs` SQL — do not change the selection predicate.

## Git workflow

- Branch: `advisor/182-cron-resume-past-oneshot`
- Conventional commit, e.g.
  `fix(cron): refuse re-enabling a fired one-shot without a fresh 'at'`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the guard in `update_job`

In `src/cron/store.rs`, inside `update_job`, after ALL patch fields are applied
and after the `if schedule_changed { ... }` block (i.e. immediately before the
`with_connection(config, |conn| { ... })` call at line ~224), add a guard that
rejects re-enabling a one-shot whose final schedule is an `At` instant in the
past.

Target shape:

```rust
    if schedule_changed {
        job.next_run = next_run_for_schedule(&job.schedule, Utc::now())?;
    }

    // A fired one-shot (`Schedule::At`) is disabled but keeps its now-past
    // `at` as next_run. Re-enabling it (resume / PUT {enabled:true} / TUI `p`)
    // would make it due on the next poll tick and silently re-run a job whose
    // contract is "runs once". Refuse unless the caller also supplies a fresh,
    // future `at` in the same patch (which reschedules via schedule_changed).
    if patch.enabled == Some(true) {
        if let Schedule::At { at } = job.schedule {
            if at <= Utc::now() {
                anyhow::bail!(
                    "cannot re-enable one-shot cron job '{job_id}': its scheduled \
                     time ({at}) is in the past. Supply a new future 'at' to \
                     reschedule it, or create a new job."
                );
            }
        }
    }

    with_connection(config, |conn| {
```

Notes:
- Use `at` (a `DateTime<Utc>`) formatted with the default `Display` (RFC3339-ish)
  via `{at}` in the message; that is fine for an error string.
- `Schedule` is already imported at the top of `store.rs` (line 4). `Utc` is
  imported (line 7).
- Because the check reads the FINAL `job.schedule` (after any `patch.schedule`
  was applied at lines 189-194), a caller who supplies both a new future `at`
  and `enabled: true` in one patch passes the guard — that is the intended
  escape hatch.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0, no warnings.

### Step 2: Add a regression test

Add a `#[test]` to the `tests` module at the bottom of `src/cron/store.rs`
(after `reschedule_after_run_truncates_last_output`, ~line 835). Model it on the
existing `due_jobs_filters_by_timestamp_and_enabled` test (lines 617-642) for
setup style.

The test must:
1. Create an `At` job in the FUTURE via `add_shell_job` (validation requires a
   future `at`), then simulate it having fired-and-been-disabled by writing a
   past state directly. The simplest deterministic way: insert an already-past,
   disabled `At` row through `with_connection` + raw SQL (mirror the raw-INSERT
   style in `job_type_from_sql_reads_valid_value`, lines 674-700), OR create the
   future job and then use `update_job` with a `schedule: Some(Schedule::At{ at:
   past })` — but note `validate_schedule` REJECTS a past `at` (see
   `src/cron/schedule.rs:48-52`), so you must write the past `at` via raw SQL,
   not via a patch.

   Recommended: raw-insert a disabled past one-shot:

   ```rust
   #[test]
   fn update_job_refuses_reenabling_a_past_one_shot() {
       let tmp = TempDir::new().unwrap();
       let config = test_config(&tmp);
       let past = Utc::now() - ChronoDuration::minutes(10);
       let schedule = Schedule::At { at: past };
       let schedule_json = serde_json::to_string(&schedule).unwrap();

       with_connection(&config, |conn| {
           conn.execute(
               "INSERT INTO cron_jobs (id, expression, command, schedule, job_type,
                   session_target, enabled, delivery, delete_after_run, created_at, next_run)
                VALUES (?1, '', 'echo once', ?2, 'shell', 'isolated', 0, ?3, 0, ?4, ?5)",
               params![
                   "past-oneshot",
                   schedule_json,
                   serde_json::to_string(&DeliveryConfig::default()).unwrap(),
                   past.to_rfc3339(),
                   past.to_rfc3339(),
               ],
           )?;
           Ok(())
       })
       .unwrap();

       // Re-enabling with no new schedule must be refused.
       let err = update_job(
           &config,
           "past-oneshot",
           CronJobPatch { enabled: Some(true), ..CronJobPatch::default() },
       )
       .unwrap_err();
       assert!(
           err.to_string().contains("in the past"),
           "expected a past-one-shot refusal, got: {err}"
       );

       // Escape hatch: supplying a fresh future `at` in the same patch succeeds.
       let future = Utc::now() + ChronoDuration::hours(1);
       let ok = update_job(
           &config,
           "past-oneshot",
           CronJobPatch {
               enabled: Some(true),
               schedule: Some(Schedule::At { at: future }),
               ..CronJobPatch::default()
           },
       )
       .unwrap();
       assert!(ok.enabled);
       assert!(ok.next_run > Utc::now());
   }
   ```

   `DeliveryConfig` and `Schedule` are in scope via `super::*` (the module `use`
   at `store.rs:2-5` re-exports them through the crate `cron` module). If the
   test module does not already see `DeliveryConfig`, add
   `use crate::cron::DeliveryConfig;` inside the `tests` module. `params!`,
   `ChronoDuration`, `Utc`, `TempDir` are already imported in the test module
   (lines 577-580).

**Verify**: `cargo test --lib cron` → all pass, including
`update_job_refuses_reenabling_a_past_one_shot`.

### Step 3: Prove the guard is not vacuous

Temporarily comment out the `anyhow::bail!` guard body from Step 1 and re-run
`cargo test --lib cron update_job_refuses_reenabling_a_past_one_shot`. The test
MUST fail (the refusal no longer happens). Restore the guard and confirm it
passes again. This proves the test exercises the new behavior rather than
passing regardless.

**Verify**: with the guard removed the named test FAILS; with it restored it
PASSES.

## Test plan

- New test in `src/cron/store.rs::tests`:
  `update_job_refuses_reenabling_a_past_one_shot` — covers (a) refusal of
  `enabled:true` on a past `At` with no new schedule, and (b) the escape hatch
  where a fresh future `at` in the same patch succeeds.
- Structural pattern: `due_jobs_filters_by_timestamp_and_enabled` (setup) +
  `job_type_from_sql_reads_valid_value` (raw INSERT).
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the new test exists and passes
- [ ] With the guard body removed the new test fails (Step 3 mutation check)
- [ ] No files outside `src/cron/store.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The code in `update_job` no longer matches the excerpt (e.g. `next_run` is
  already recomputed unconditionally, or a past-`At` guard already exists) — the
  codebase has drifted.
- `validate_schedule` no longer rejects a past `at` (which would let you write
  the test's past state via a patch instead of raw SQL) — re-read
  `src/cron/schedule.rs` and adapt, or STOP if the semantics changed.
- A step's verification fails twice after a reasonable fix attempt.
- Making the test pass appears to require editing `resume_job`, the gateway, or
  the TUI (it must not — the fix is entirely in `update_job`).

## Maintenance notes

- Anyone adding a new "re-enable" surface for cron jobs gets this guard for free
  as long as they go through `update_job`. If a future path writes `enabled = 1`
  with raw SQL, it bypasses the guard — keep re-enable operations funneled
  through `update_job`.
- A reviewer should confirm the guard reads the POST-patch schedule (so the
  new-`at` escape hatch works) and only triggers on `patch.enabled == Some(true)`
  (so the one-shot disable path in `scheduler.rs`, which sets
  `enabled: Some(false)`, is unaffected).
- Deferred: this does not change the disable-after-fire behavior for one-shots
  (that stays in `persist_job_result`); it only guards re-enabling.
