# Plan 185: Record cron retry attempts as separate run-history rows

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/cron/store.rs src/cron/types.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW-MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`execute_job_with_retry` (`scheduler.rs:98-137`) loops up to
`1 + scheduler_retries` times, overwriting `last_output` each pass and sleeping
`backoff + jitter` between attempts. But the timing is stamped **once around the
whole loop**: `started_at` before the loop in `execute_and_persist_job`
(`scheduler.rs:195`), `finished_at` after (`scheduler.rs:197`), and a single
`record_run` writes one row (`scheduler.rs:284`). Consequences:

- **`duration_ms` is useless** — it includes every retry's execution *plus* the
  backoff sleeps (up to ~30 min), not the time the job actually ran.
- **Failed attempts are hidden** — a job that failed twice then succeeded shows
  one clean `ok` row. Operators debugging a flaky job see no evidence it retried.

After this plan, each retry attempt is its own `cron_runs` row with its own
timing and an attempt index, so run history shows the real sequence
(`error`, `error`, `ok`) and each `duration_ms` reflects that attempt's actual
execution.

The recommended approach (Option A below) is **per-attempt rows behind the
existing `max_run_history` cap**. A simpler lower-risk alternative (Option B) is
described at the end; use it only if a STOP condition sends you there.

## Current state

- `src/cron/scheduler.rs` — the retry loop and the single-row recording.
- `src/cron/store.rs` — `record_run` (insert + prune) and the `cron_runs` schema.
- `src/cron/types.rs` — the `CronRun` struct returned by `list_runs`.

The retry loop stamps no per-attempt timing (`scheduler.rs:98-137`):

```rust
async fn execute_job_with_retry(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    let mut last_output = String::new();
    let retries = config.reliability.scheduler_retries;
    let mut backoff_ms = config.reliability.provider_backoff_ms.max(200);

    for attempt in 0..=retries {
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
        last_output = output;

        if success {
            return (true, last_output);
        }

        if last_output.starts_with("blocked by security policy:") {
            return (false, last_output);
        }

        if attempt < retries {
            let jitter_ms = u64::from(Utc::now().timestamp_subsec_millis() % 250);
            time::sleep(Duration::from_millis(backoff_ms + jitter_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
        }
    }

    (false, last_output)
}
```

Timing stamped once around the loop, one row recorded
(`scheduler.rs:195-198` and `:284-292`):

```rust
    let started_at = Utc::now();
    let (success, output) = execute_job_with_retry(config, security, job).await;
    let finished_at = Utc::now();
    let success = persist_job_result(config, job, success, &output, started_at, finished_at).await;
```

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

`record_run` inserts one row and prunes to `max_run_history` **per job**
(`store.rs:304-352`, cap at `:334`):

```rust
        let keep = i64::from(config.cron.max_run_history.max(1));
        tx.execute(
            "DELETE FROM cron_runs
             WHERE job_id = ?1
               AND id NOT IN (
                 SELECT id FROM cron_runs
                 WHERE job_id = ?1
                 ORDER BY started_at DESC, id DESC
                 LIMIT ?2
               )",
            params![job_id, keep],
        )
```

The `cron_runs` schema has no attempt column (`store.rs:546-554`):

```rust
        CREATE TABLE IF NOT EXISTS cron_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id      TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            status      TEXT NOT NULL,
            output      TEXT,
            duration_ms INTEGER,
            FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
        );
```

`CronRun` (`types.rs:138-147`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronRun {
    pub id: i64,
    pub job_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: String,
    pub output: Option<String>,
    pub duration_ms: Option<i64>,
}
```

The existing column-migration helper is hardcoded to `cron_jobs`
(`store.rs:483-511`), so `cron_runs` needs either a generalized helper or a
parallel one:

```rust
fn add_column_if_missing(conn: &Connection, name: &str, sql_type: &str) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(cron_jobs)")?;
    ...
    conn.execute(&format!("ALTER TABLE cron_jobs ADD COLUMN {name} {sql_type}"), [])
    ...
}
```

`record_run`'s only **production** callers are `run_job_manual`
(`scheduler.rs:85`) and `persist_job_result` (`scheduler.rs:284`); all other
call sites are tests (`src/cron/mod.rs:575`, `src/cron/store.rs:769/782/805`,
`src/tools/cron_runs.rs:144`). Keeping `record_run`'s signature unchanged (Step
2) means none of those tests need edits.

## Commands you will need

| Purpose      | Command                                             | Expected on success        |
|--------------|-----------------------------------------------------|----------------------------|
| Format check | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint         | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests        | `cargo test --lib cron`                             | all pass (incl. new tests) |
| Drift        | `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/cron/store.rs src/cron/types.rs` | empty before you start |

Do NOT run a bare `cargo test`.

## Scope

**In scope**:
- `src/cron/store.rs` (migration + schema + a new attempt-aware record fn + `list_runs` SELECT)
- `src/cron/types.rs` (`CronRun.attempt` field)
- `src/cron/scheduler.rs` (per-attempt timing + per-attempt recording)

**Out of scope** (do NOT touch):
- `src/tools/cron_runs.rs` / `src/gateway/cron_api.rs` — they read `CronRun` and
  serialize it; adding a field is additive and needs no change there. Do not edit
  their formatting unless a test forces it (if so, that is a STOP condition —
  report it).
- The `max_run_history` config key or its default — keep the prune as a raw
  per-job row count (see Maintenance for the documented semantic shift).

## Git workflow

- Branch: `advisor/185-cron-retry-run-history`
- Conventional commits, e.g. `fix(cron): record each retry attempt as its own run row`.
- Do NOT push or open a PR unless the operator instructed it.

## Steps (Option A — recommended: per-attempt rows)

### Step 1: Add an `attempt` column (schema + migration)

In `store.rs`:

- Add `attempt INTEGER NOT NULL DEFAULT 1` to the `CREATE TABLE cron_runs`
  statement (`store.rs:546-554`), after `duration_ms`. The `DEFAULT 1` means old
  code paths and existing rows read as attempt 1.
- Add an ALTER migration for existing databases. **Preferred (fewer touch
  points): add a parallel `add_cron_runs_column_if_missing`** that mirrors
  `add_column_if_missing` but targets `cron_runs` — this leaves the existing
  helper and its callers untouched. The alternative — generalizing
  `add_column_if_missing` to take a `table: &str` parameter (changing the
  `PRAGMA table_info(cron_jobs)` and `ALTER TABLE cron_jobs`) — requires updating
  **all nine existing callers** (`store.rs:562-570`), not one, so prefer the
  parallel helper unless you have reason to consolidate. Then, in the same place
  the `cron_jobs` migrations run, call the new helper for
  `("attempt", "INTEGER NOT NULL DEFAULT 1")` on `cron_runs`. Find where
  `add_column_if_missing` is currently invoked and mirror it.

**Verify**: `cargo test --lib cron` → existing store tests pass (they insert via
`record_run`, which now relies on the column DEFAULT).

### Step 2: Add an attempt-aware recording function (keep `record_run` intact)

Leave `record_run` (`store.rs:304`) exactly as-is (its INSERT omits `attempt`, so
new rows get DEFAULT 1 — existing callers/tests keep working). Add a sibling that
sets `attempt` explicitly and reuses the same insert+prune transaction shape:

```rust
/// Like `record_run` but stamps the retry `attempt` index (1-based). Used by the
/// scheduler so each retry of a job becomes its own history row.
pub fn record_run_attempt(
    config: &Config,
    job_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
    attempt: u32,
) -> Result<()> {
    // Same transaction + prune body as `record_run`, but the INSERT includes the
    // `attempt` column. Prune stays per-job row count (keep = max_run_history).
    ...
    tx.execute(
        "INSERT INTO cron_runs
           (job_id, started_at, finished_at, status, output, duration_ms, attempt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![job_id, started_at.to_rfc3339(), finished_at.to_rfc3339(), status,
                bounded_output.as_deref(), duration_ms, i64::from(attempt)],
    )?;
    // ... identical prune DELETE + commit as record_run ...
}
```

Export `record_run_attempt` from `src/cron/mod.rs` alongside `record_run` (see
the `pub use store::{...}` block around `mod.rs:16`).

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Return `attempt` from `list_runs` and add it to `CronRun`

- `types.rs`: add `pub attempt: i64,` to `CronRun` (after `duration_ms`).
- `store.rs` `list_runs` (`store.rs:373-404`): add `attempt` to the SELECT column
  list and to the `CronRun { ... }` construction (`attempt: row.get(7)?`).

**Verify**: `cargo test --lib cron` → existing `list_runs` tests pass (they will
now see `attempt = 1` for rows written via `record_run`).

### Step 4: Stamp per-attempt timing and surface attempts from the retry loop

In `scheduler.rs`, add an outcome struct and change `execute_job_with_retry` to
time each attempt and return the per-attempt records.

**Chosen visibility fix (required — do not skip):** `AttemptOutcome` becomes part
of the return type of the `pub` `execute_job_now` (scheduler.rs:69), so it MUST
be declared `pub`. A private type in a `pub` signature triggers the
`private_interfaces` lint (warn-by-default since Rust 1.74), which the Done
criterion `cargo clippy --all-targets -- -D warnings` promotes to an ERROR.
Declare it `pub struct AttemptOutcome`. (The equivalent alternative — demoting
`execute_job_now` itself to non-`pub`, valid because its only caller is
`run_job_manual` at scheduler.rs:81 — is NOT the path this plan takes; keep
`execute_job_now` public and make the struct public.)

```rust
pub struct AttemptOutcome {
    attempt: u32,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    success: bool,
    output: String,
}
```

```rust
async fn execute_job_with_retry(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String, Vec<AttemptOutcome>) {
    let mut attempts = Vec::new();
    let mut last_output = String::new();
    let retries = config.reliability.scheduler_retries;
    let mut backoff_ms = config.reliability.provider_backoff_ms.max(200);

    for attempt in 0..=retries {
        let started_at = Utc::now();
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
        let finished_at = Utc::now();
        attempts.push(AttemptOutcome {
            attempt: attempt + 1,
            started_at,
            finished_at,
            success,
            output: output.clone(),
        });
        last_output = output;

        if success {
            return (true, last_output, attempts);
        }
        if last_output.starts_with("blocked by security policy:") {
            return (false, last_output, attempts);
        }
        if attempt < retries {
            let jitter_ms = u64::from(Utc::now().timestamp_subsec_millis() % 250);
            time::sleep(Duration::from_millis(backoff_ms + jitter_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
        }
    }
    (false, last_output, attempts)
}
```

Both callers of `execute_job_with_retry` change:

- `execute_job_now` (`scheduler.rs:69-72`) now returns the attempts too:
  `pub async fn execute_job_now(...) -> (bool, String, Vec<AttemptOutcome>)` and
  just forwards. (Only `run_job_manual` calls it — see plan 169 if that signature
  is also being changed; keep both edits compatible.)
- `execute_and_persist_job` (`scheduler.rs:186-201`) destructures the third value
  and passes it to `persist_job_result`.

Update the two in-file tests that call `execute_job_with_retry` directly
(`scheduler.rs:804`, `:819`) to destructure the third tuple element (e.g.
`let (success, output, _attempts) = ...`).

### Step 5: Record one row per attempt in the two persist paths

**Scheduled path** (`persist_job_result`, `scheduler.rs:265-322`): change its
signature to take `attempts: &[AttemptOutcome]` instead of the single
`started_at`/`finished_at`. Record each attempt as its own row; the delivery
step still applies to the FINAL result only:

- Run delivery (`deliver_if_configured`) as today; it may flip the final
  `success` to `false` when not `best_effort`.
- For attempts `0..len-1`, record with their own `success`/timing/`duration_ms`
  and their `attempt` index via `record_run_attempt`.
- For the LAST attempt, record with the **delivery-adjusted** final `status`
  (so run history's most recent row still reflects delivery failure, matching
  today's behavior), its own timing/`duration_ms`, and its `attempt` index.
- Keep the one-shot delete/disable and reschedule logic unchanged; they already
  use the final `success`.

Concrete target shape (illustrative — adapt to the surrounding code; this is the
highest-risk edit in the plan, so map it against the live function before typing):

```rust
// New signature — `attempts` replaces the single started_at/finished_at pair.
async fn persist_job_result(
    config: &Config,
    job: &CronJob,
    success: bool,
    output: &str,
    attempts: &[AttemptOutcome],
) -> bool {
    // Delivery still runs ONCE, on the final result only (unchanged from today);
    // it may flip `success` to false when the channel is not best_effort.
    let success = deliver_if_configured(config, job, success, output).await;

    if let Some((last, earlier)) = attempts.split_last() {
        // Attempts before the last keep their own real status/output/timing.
        for a in earlier {
            let duration_ms = (a.finished_at - a.started_at).num_milliseconds();
            let _ = record_run_attempt(
                config, &job.id, a.started_at, a.finished_at,
                if a.success { "ok" } else { "error" },
                Some(&a.output), duration_ms, a.attempt,
            );
        }
        // The LAST row carries the delivery-adjusted final status (matches prior
        // behavior: run history's newest row reflects delivery failure).
        let duration_ms = (last.finished_at - last.started_at).num_milliseconds();
        let _ = record_run_attempt(
            config, &job.id, last.started_at, last.finished_at,
            if success { "ok" } else { "error" },
            Some(output), duration_ms, last.attempt,
        );
    }

    // ... one-shot delete/disable + reschedule logic UNCHANGED (still uses `success`) ...
    success
}
```

(If plan 180 has already landed, the `let _ = record_run_attempt(...)` calls
above become the `if let Err(e) = ... { tracing::warn!(...) }` shape it
introduced — keep that logging.)

**Manual path** (`run_job_manual`, `scheduler.rs:79-96`): manual runs do not
deliver. Destructure the attempts from `execute_job_now` and record each with
`record_run_attempt` (its own timing/status/`duration_ms`/index). Keep the single
`record_last_run` using the final `success`/`output`.

Concrete target shape (illustrative):

```rust
let (success, output, attempts) = execute_job_now(config, job).await;
for a in &attempts {
    let duration_ms = (a.finished_at - a.started_at).num_milliseconds();
    let _ = record_run_attempt(
        config, &job.id, a.started_at, a.finished_at,
        if a.success { "ok" } else { "error" },
        Some(&a.output), duration_ms, a.attempt,
    );
}
// Final last-run stamp uses the last attempt's finished_at and the final result.
let finished_at = attempts.last().map_or_else(Utc::now, |a| a.finished_at);
let _ = record_last_run(config, &job.id, finished_at, success, &output);
(success, output)
```

Guidance for duration: `duration_ms` for each row = `(finished_at - started_at).num_milliseconds()`
of that attempt — this is the fix for the "duration includes sleeps" bug, because
the backoff `sleep` happens *between* attempts and is outside any attempt's
`started_at..finished_at` window.

**Verify**: `cargo fmt --all -- --check` → exit 0.
`cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 6: Add regression tests

Add tests to `scheduler.rs` `mod tests`:

1. **Retried job records one row per attempt.** Set
   `config.reliability.scheduler_retries = 1`, `provider_backoff_ms = 1`, allow
   `sh`, and write the fail-once-then-succeed script exactly as
   `execute_job_with_retry_recovers_after_first_failure` does (`scheduler.rs:788-807`).
   Run it through `execute_and_persist_job` (or `persist_job_result` fed by the
   attempts) and assert:
   - `cron::list_runs(&config, &job.id, 10).unwrap().len() == 2`
   - the rows have `attempt` values `1` and `2`
   - statuses are `error` (attempt 1) then `ok` (attempt 2)
   (Model job/config setup after `persist_job_result_records_run_and_reschedules_shell_job`,
   `scheduler.rs:964-979`.)
2. **Single-attempt success records one row with attempt = 1** — proves the
   default/index for the common no-retry case.

**Verify**: `cargo test --lib cron` → all pass, including the new tests.

## Alternative implementation (Option B — lower risk, use only via a STOP)

If Step 4/5's return-type change proves too invasive (e.g. plan 169 landed and
the tuple shapes conflict in a way you cannot reconcile), fall back to:

- Add an `attempts INTEGER NOT NULL DEFAULT 1` column to `cron_runs` (count of
  attempts made this execution).
- Time **only** the final execution attempt: move the `started_at`/`finished_at`
  stamping *inside* the loop so `duration_ms` measures the last attempt, not the
  whole window, and pass the attempt count out of `execute_job_with_retry`
  (`-> (bool, String, u32)`).
- Keep the single `record_run`, extended to also write `attempts`.

Option B fixes the misleading `duration_ms` and records how many attempts
happened, but does NOT preserve per-attempt output/status history. Prefer Option
A; only use B if directed by a STOP.

## Test plan

- New tests in `src/cron/scheduler.rs` `mod tests`: per-attempt row count +
  attempt index + status sequence for a retried job; single-attempt success
  writes one row with `attempt = 1`.
- Existing tests to keep green (update only tuple destructuring where they call
  `execute_job_with_retry`): `execute_job_with_retry_recovers_after_first_failure`
  (`scheduler.rs:788`), `execute_job_with_retry_exhausts_attempts`
  (`scheduler.rs:809`), and the `persist_job_result_*` tests (`scheduler.rs:964`,
  `:981`, `:1006`, `:1072`) whose call signature changes if `persist_job_result`
  now takes `&[AttemptOutcome]`.
- Store tests unchanged (they use `record_run`, whose signature is untouched).
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the retry-records-two-rows test exists and passes
- [ ] `cron_runs` has an `attempt` column (`grep -n "attempt" src/cron/store.rs` shows it in the CREATE TABLE and the migration)
- [ ] `CronRun` has an `attempt` field (`grep -n "pub attempt" src/cron/types.rs`)
- [ ] Each attempt is timed individually (`grep -n "AttemptOutcome" src/cron/scheduler.rs` shows the struct and per-attempt `started_at`/`finished_at`)
- [ ] Only `src/cron/scheduler.rs`, `src/cron/store.rs`, `src/cron/types.rs` (and `src/cron/mod.rs` for the `pub use` export) are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check is non-empty (code moved since this plan was written).
- Adding `CronRun.attempt` forces a change in `src/tools/cron_runs.rs` or
  `src/gateway/cron_api.rs` (they should compile against the additive field
  unchanged) — report before editing an out-of-scope file.
- The `persist_job_result` signature change ripples into a test asserting
  *behavior* you would have to change (as opposed to updating the call).
- The `execute_job_with_retry` return-type change conflicts irreconcilably with a
  landed plan 169 — then switch to Option B and note it in your report.
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

For the human/agent who owns this after the change lands:

- **Prune-cap semantics changed.** `max_run_history` is still a per-job row count,
  but with retries a single execution now occupies up to `1 + scheduler_retries`
  rows, so history holds fewer distinct *executions* at the same cap. This was a
  deliberate KISS choice; if operators want "keep N executions", change the prune
  to group by execution (e.g. a per-execution id) — a follow-up, not this plan.
- Reviewer should scrutinize: (1) the last attempt's recorded status reflects the
  delivery-adjusted final success (scheduled path), matching prior behavior;
  (2) `duration_ms` per row excludes the backoff sleep (timing is inside the
  loop, sleep is between attempts); (3) `record_run` was left untouched so its
  many test callers did not need edits.
- The `attempt` column is additive and defaults to 1, so old rows and any code
  path still using `record_run` read as attempt 1 — no data migration needed
  beyond the ALTER.
