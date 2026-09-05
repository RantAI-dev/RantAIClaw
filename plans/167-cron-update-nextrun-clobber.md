# Plan 167: Stop `update_job` from clobbering a scheduler-written `next_run`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 2aefb9f..HEAD -- src/cron/store.rs`
> If `src/cron/store.rs` changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (HIGH on the write path, MED on the interleaving)
- **Depends on**: none (interacts with `plans/179-cron-store-busy-timeout-wal.md` — see Maintenance notes)
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`update_job` in `src/cron/store.rs` reads a job on one SQLite connection, then
writes **every** column back on a *second* connection — including
`next_run` — with no shared transaction. The scheduler's `reschedule_after_run`
writes a fresh `next_run` on yet another connection. When an operator edit
(rename / pause / command change via TUI, HTTP, or the cron tool) interleaves
with a scheduler reschedule, the edit's write-back carries the **stale, now-past**
`next_run` it read before the reschedule. `due_jobs` selects
`enabled = 1 AND next_run <= now`, so the past `next_run` makes the job due
again immediately → a re-fire loop. The all-columns write can also silently
revert a concurrent `enabled`/`command` change made by another surface.

After this plan: `update_job` reads and writes on **one** connection inside a
single `IMMEDIATE` transaction (so no other writer can interleave between the
read and the write), and it emits `next_run` in the `UPDATE` **only** when the
operator actually changed the schedule. An operator edit can no longer overwrite
a `next_run` the scheduler owns.

## Current state

- `src/cron/store.rs` — the cron job store. `update_job` is lines 185–252.
  - Line 186 reads via `get_job(config, job_id)?` (opens its own connection).
  - Lines 220–222 recompute `job.next_run` **only** when `schedule_changed`.
  - Lines 224–249 open a *second* connection via `with_connection` and run an
    `UPDATE` that writes `next_run = ?12` **unconditionally** (value at line 243
    is `job.next_run.to_rfc3339()`).

Verbatim `update_job` as it exists today (lines 185–252):

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
    if let Some(command) = patch.command {
        job.command = command;
    }
    if let Some(prompt) = patch.prompt {
        job.prompt = Some(prompt);
    }
    if let Some(name) = patch.name {
        job.name = Some(name);
    }
    if let Some(enabled) = patch.enabled {
        job.enabled = enabled;
    }
    if let Some(delivery) = patch.delivery {
        job.delivery = delivery;
    }
    if let Some(model) = patch.model {
        job.model = Some(model);
    }
    if let Some(target) = patch.session_target {
        job.session_target = target;
    }
    if let Some(delete_after_run) = patch.delete_after_run {
        job.delete_after_run = delete_after_run;
    }

    if schedule_changed {
        job.next_run = next_run_for_schedule(&job.schedule, Utc::now())?;
    }

    with_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                 session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11,
                 next_run = ?12
             WHERE id = ?13",
            params![
                job.expression,
                job.command,
                serde_json::to_string(&job.schedule)?,
                <JobType as Into<&str>>::into(job.job_type).to_string(),
                job.prompt,
                job.name,
                job.session_target.as_str(),
                job.model,
                if job.enabled { 1 } else { 0 },
                serde_json::to_string(&job.delivery)?,
                if job.delete_after_run { 1 } else { 0 },
                job.next_run.to_rfc3339(),
                job.id,
            ],
        )
        .context("Failed to update cron job")?;
        Ok(())
    })?;

    get_job(config, job_id)
}
```

Conventions this plan honors:

- The repo prefers **explicit match/branch over dynamic behavior** (CLAUDE.md
  §3.1). So we use two literal `UPDATE` statements (with / without `next_run`)
  rather than building SQL strings dynamically. Do **not** introduce dynamic
  column-list SQL — it is explicitly out of scope (see Scope).
- `map_cron_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob>` is a
  module function (lines 416–451) that decodes a full row. `get_job` uses it at
  lines 140–144. Reuse it inside the transaction.
- rusqlite version is `0.37` (Cargo.toml line 123).
  `rusqlite::Transaction::new_unchecked(conn, behavior)` creates an RAII
  transaction from a shared `&Connection` (this is exactly what
  `conn.unchecked_transaction()` wraps, but with a chosen behavior). Use
  `rusqlite::TransactionBehavior::Immediate`. Because `with_connection` opens a
  fresh `Connection` and drops it at function end, an early `?` return inside
  the closure drops the connection and SQLite auto-rolls-back the open
  transaction — so no explicit `ROLLBACK` is needed on the error path.
- `Transaction` derefs to `Connection`, so `tx.prepare(...)`, `tx.execute(...)`,
  and `tx.commit()` all work, and `map_cron_job_row` (which takes a
  `&rusqlite::Row`) works against a `tx.query(...)` cursor.

## Commands you will need

| Purpose   | Command                                             | Expected on success        |
|-----------|-----------------------------------------------------|----------------------------|
| Format    | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint      | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests     | `cargo test --lib cron`                             | all pass, incl. new tests  |

Do NOT run bare `cargo test` (this box is disk-constrained). Scope to `cron`.

## Scope

**In scope** (the only file you should modify):

- `src/cron/store.rs` — rewrite `update_job` (lines 185–252) and add a
  regression test in its `#[cfg(test)] mod tests` (lines 575–836).

**Out of scope** (do NOT touch):

- `get_job` (lines 131–146) — leave it exactly as is; it is still the public
  read path and other callers depend on it.
- `reschedule_after_run` (lines 275–302) and `record_run` (lines 304–352) —
  their transaction/pragma concerns belong to `plans/179`.
- Dynamic "only patched columns" SQL. With the `IMMEDIATE` transaction the
  read sees the latest committed row, so writing the unchanged columns back is a
  no-op — dynamic column building adds complexity for no correctness gain
  (KISS / YAGNI). Two static `UPDATE` statements are the intended shape.
- `with_connection`'s signature — keep it `FnOnce(&Connection)`; use
  `Transaction::new_unchecked` on the shared `&Connection`.

## Git workflow

- Branch: `advisor/167-cron-update-nextrun-clobber`
- Conventional commits, e.g. `fix(cron): update_job reads+writes in one IMMEDIATE txn and only writes next_run on schedule change`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Rewrite `update_job` to use one connection + one `IMMEDIATE` transaction

Replace the entire `update_job` body (lines 185–252) so that:

1. The row is read **inside** the transaction (not via the outer `get_job`).
2. The patch is applied in memory exactly as today.
3. `next_run` is recomputed only when `schedule_changed` (unchanged logic).
4. The `UPDATE` includes `next_run` **only** when `schedule_changed`.

Target shape:

```rust
pub fn update_job(config: &Config, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
    with_connection(config, |conn| {
        // IMMEDIATE takes the write lock up front so a concurrent
        // reschedule_after_run cannot commit between our read and our write.
        let tx = rusqlite::Transaction::new_unchecked(
            conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;

        // Read the current row inside the write transaction. Scoped in a block
        // so the prepared statement/cursor are dropped before we UPDATE.
        let mut job = {
            let mut stmt = tx.prepare(
                "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                        enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output
                 FROM cron_jobs WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![job_id])?;
            match rows.next()? {
                Some(row) => map_cron_job_row(row).map_err(anyhow::Error::from)?,
                None => anyhow::bail!("Cron job '{job_id}' not found"),
            }
        };

        let mut schedule_changed = false;
        if let Some(schedule) = patch.schedule {
            validate_schedule(&schedule, Utc::now())?;
            job.schedule = schedule;
            job.expression = schedule_cron_expression(&job.schedule).unwrap_or_default();
            schedule_changed = true;
        }
        if let Some(command) = patch.command {
            job.command = command;
        }
        if let Some(prompt) = patch.prompt {
            job.prompt = Some(prompt);
        }
        if let Some(name) = patch.name {
            job.name = Some(name);
        }
        if let Some(enabled) = patch.enabled {
            job.enabled = enabled;
        }
        if let Some(delivery) = patch.delivery {
            job.delivery = delivery;
        }
        if let Some(model) = patch.model {
            job.model = Some(model);
        }
        if let Some(target) = patch.session_target {
            job.session_target = target;
        }
        if let Some(delete_after_run) = patch.delete_after_run {
            job.delete_after_run = delete_after_run;
        }

        if schedule_changed {
            job.next_run = next_run_for_schedule(&job.schedule, Utc::now())?;
        }

        // Compute the shared encoded values once.
        let schedule_json = serde_json::to_string(&job.schedule)?;
        let job_type_str = <JobType as Into<&str>>::into(job.job_type).to_string();
        let delivery_json = serde_json::to_string(&job.delivery)?;
        let enabled_int = if job.enabled { 1 } else { 0 };
        let delete_after_int = if job.delete_after_run { 1 } else { 0 };

        if schedule_changed {
            // Only a schedule edit is allowed to move next_run.
            tx.execute(
                "UPDATE cron_jobs
                 SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                     session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11,
                     next_run = ?12
                 WHERE id = ?13",
                params![
                    job.expression,
                    job.command,
                    schedule_json,
                    job_type_str,
                    job.prompt,
                    job.name,
                    job.session_target.as_str(),
                    job.model,
                    enabled_int,
                    delivery_json,
                    delete_after_int,
                    job.next_run.to_rfc3339(),
                    job.id,
                ],
            )
            .context("Failed to update cron job")?;
        } else {
            // Leave next_run untouched so an operator edit can never clobber a
            // next_run the scheduler wrote.
            tx.execute(
                "UPDATE cron_jobs
                 SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                     session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11
                 WHERE id = ?12",
                params![
                    job.expression,
                    job.command,
                    schedule_json,
                    job_type_str,
                    job.prompt,
                    job.name,
                    job.session_target.as_str(),
                    job.model,
                    enabled_int,
                    delivery_json,
                    delete_after_int,
                    job.id,
                ],
            )
            .context("Failed to update cron job")?;
        }

        tx.commit().context("Failed to commit cron update")?;
        Ok(())
    })?;

    get_job(config, job_id)
}
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0, no warnings.

### Step 2: Add tests

> **Important — what these tests can and cannot prove.** The bug is a
> **two-connection race**: `update_job` reads on one connection and writes on
> another while `reschedule_after_run` writes `next_run` on a third. A
> **sequential** unit test cannot reproduce that interleaving, so it CANNOT
> "fail on the pre-fix code" — the pre-fix `update_job` also preserves
> `next_run` when a reschedule and an edit run one after the other. Do NOT try
> to write a unit test that fails before the fix; that framing is wrong for this
> defect. The **primary proof of the fix is the structural Done-criteria greps**
> (the conditional `next_run` write appears only in the schedule-changed branch,
> `Transaction::new_unchecked(..., Immediate)` is present inside `update_job`,
> and the no-schedule `UPDATE` omits `next_run`). The test below is a behavioral
> sanity check on the sequential path, plus an over-correction guard; it is not
> the race regression proof.

Add this test inside `mod tests` in `src/cron/store.rs` (after the existing
`reschedule_after_run_persists_last_status_and_last_run` test, ~line 672). It
checks that a non-schedule edit leaves a scheduler-written `next_run` intact on
the sequential path.

```rust
#[test]
fn update_job_without_schedule_change_preserves_next_run() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "*/5 * * * *", "echo hi").unwrap();

    // Simulate a scheduler reschedule writing a fresh next_run.
    reschedule_after_run(&config, &job, true, "ran").unwrap();
    let after_reschedule = get_job(&config, &job.id).unwrap().next_run;
    // NOTE: do NOT assert_ne!(after_reschedule, job.next_run) here — for a
    // `*/5 * * * *` schedule the recomputed boundary can be identical
    // (reschedule recomputes from Utc::now(); the next 5-minute boundary is the
    // same instant), so such a precondition would panic deterministically.

    // An operator edit that does NOT touch the schedule (rename) must not
    // move next_run back.
    let _ = update_job(
        &config,
        &job.id,
        CronJobPatch {
            name: Some("renamed".into()),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    let final_job = get_job(&config, &job.id).unwrap();
    assert_eq!(
        final_job.next_run, after_reschedule,
        "a non-schedule edit must preserve the scheduler-written next_run"
    );
    assert_eq!(final_job.name.as_deref(), Some("renamed"));
}

#[test]
fn update_job_with_schedule_change_recomputes_next_run() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "*/5 * * * *", "echo hi").unwrap();
    let original_next = job.next_run;

    // A schedule change is the ONLY edit allowed to move next_run.
    let updated = update_job(
        &config,
        &job.id,
        CronJobPatch {
            schedule: Some(Schedule::Cron {
                expr: "0 0 1 1 *".into(),
                tz: None,
            }),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    assert_ne!(
        updated.next_run, original_next,
        "a schedule change must recompute next_run"
    );
    assert_eq!(updated.expression, "0 0 1 1 *");
}
```

**Verify**: `cargo test --lib cron` → all pass, including the two new tests.

### Step 3: Final validation

**Verify**:
- `cargo fmt --all -- --check` → exit 0, no diff.
- `cargo clippy --all-targets -- -D warnings` → exit 0.
- `cargo test --lib cron` → all pass.

## Test plan

- **Primary proof is structural, not a failing test.** The defect is a
  two-connection race that a sequential unit test cannot exercise, so the tests
  below do NOT fail on the pre-fix code. The fix is verified by the
  Done-criteria greps: the conditional `next_run` write is present only in the
  schedule-changed branch, `Transaction::new_unchecked(..., Immediate)` is
  present inside `update_job`, and the no-schedule `UPDATE` omits `next_run`.
- New tests in `src/cron/store.rs` `mod tests`:
  - `update_job_without_schedule_change_preserves_next_run` — a behavioral
    sanity check on the sequential path: a rename leaves the
    scheduler-written `next_run` intact. (This is not the race regression proof;
    see the note in Step 2 — a sequential test cannot reproduce the race.)
  - `update_job_with_schedule_change_recomputes_next_run` — guards against
    over-correction: a genuine schedule change must still move `next_run`.
- Existing `due_jobs_filters_by_timestamp_and_enabled` (lines 617–642) already
  exercises `update_job` with an `enabled` patch and must still pass.
- Structural pattern: model the new tests after the existing store tests
  (`add_list_remove_roundtrip`, `reschedule_after_run_persists_...`).
- Verification: `cargo test --lib cron` → all pass, including 2 new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the 2 new tests exist and pass
- [ ] `update_job` opens exactly one connection and wraps read+write in an
      `IMMEDIATE` transaction (`grep -n "Transaction::new_unchecked" src/cron/store.rs`
      shows a match inside `update_job`)
- [ ] The no-schedule-change `UPDATE` in `update_job` does not mention `next_run`
- [ ] No files outside `src/cron/store.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The `update_job` body at lines 185–252 does not match the "Current state"
  excerpt (the file drifted since this plan was written).
- `rusqlite::Transaction::new_unchecked` does not exist in the pinned rusqlite
  version (compile error) — report; do NOT switch `with_connection` to
  `&mut Connection` as a workaround without flagging it.
- `cargo test --lib cron` fails twice after a reasonable fix attempt.
- The fix appears to require touching any file other than `src/cron/store.rs`.

## Maintenance notes

- `plans/179-cron-store-busy-timeout-wal.md` also edits `src/cron/store.rs`
  (adds `busy_timeout`/WAL to `with_connection` and switches `record_run` to an
  `IMMEDIATE` transaction). The two plans do not touch the same functions, but
  if 179 landed first, re-run the drift check — `with_connection`'s body will
  differ from the excerpt here even though `update_job` will not.
- Without plan 179's `busy_timeout`/WAL, the new `IMMEDIATE` transaction here
  can return `SQLITE_BUSY` under the very concurrency this plan targets (a
  concurrent writer already holds the write lock). Correctness is still
  preserved — the edit **fails** rather than clobbering a scheduler-written
  `next_run` — but land 179 alongside/before this to make the edit robust under
  contention rather than merely safe.
- Reviewer should scrutinize: that the no-schedule-change branch truly omits
  `next_run`, and that the read happens inside the transaction (not via the
  outer `get_job`, which would reintroduce the two-connection race).
- Deferred: making `reschedule_after_run` and `update_job` share a higher-level
  locking discipline is unnecessary once each read-modify-write is `IMMEDIATE`;
  no further transaction work is planned here.
