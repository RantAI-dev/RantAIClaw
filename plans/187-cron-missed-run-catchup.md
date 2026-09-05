# Plan 187: Bound cron catch-up on restart — coalesce-to-one + staleness gate + startup stagger

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **DESIGN DECIDED (2026-08-19).** An earlier draft of this plan proposed a
> configurable "replay everything within a time window" and an `Every`
> occurrence-by-occurrence catch-up. That was rejected after researching mature
> schedulers (K8s CronJob, Quartz, systemd `Persistent`, anacron, Celery, cloud
> schedulers): **none replay all missed runs; the universal norm is
> coalesce-to-at-most-one-per-job**, with a small staleness gate on that single
> run. This plan implements that norm. Default `max_catchup_age_secs = 86400`
> (1 day). No operator STOP-confirm remains.
>
> **THIS PLAN CHANGES A DEFAULT POLICY** (adds a config field + bumps the schema
> version) — record it in the PR/CHANGELOG per CLAUDE.md §3.6.
>
> **Drift check (run first)**:
> `git diff --stat 434141c..HEAD -- src/cron/store.rs src/cron/scheduler.rs src/config/schema.rs src/config/migrations.rs tests/schema_drift.rs`
> If any in-scope file changed since this plan was written, re-locate the named
> functions and compare against live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: none (coordinates with plan 165 on startup batching — see notes)
- **Category**: bug
- **Planned at**: commit `434141c`, 2026-08-19 (rewritten to coalesce-one)

## Why this matters

`due_jobs` selects every enabled job with `next_run <= now`. `reschedule_after_run`
already computes the next run from `Utc::now()` — i.e. after a job fires it
re-anchors forward and **coalesces** all its missed occurrences into that single
fire. That coalesce behavior is *correct and intended* (it matches K8s, systemd
`Persistent`, anacron, and Quartz's default), so this plan does **not** change it.

Two real problems remain, and only these:

- **Stale fire "as if on time"** — a job whose `next_run` passed *months* ago
  fires immediately on restart with no staleness awareness and no log line. For a
  daily reminder that is nonsense: the operator wants it skipped, not fired late.
- **Startup batch herd** — on the first tick after a restart, every overdue job
  (up to `scheduler.max_tasks`, default 64, each agent job hitting the provider)
  becomes due at once and launches in one burst.

This plan adds a **staleness gate** (skip-and-log a job whose miss is older than
`max_catchup_age_secs`, re-anchoring it forward without firing — a one-shot `At`
is disabled instead, since a missed reminder must not fire late) and a **bounded
startup stagger**. It does **not** add any occurrence-replay: a fresh overdue job
still fires exactly once (coalesced) and re-anchors, unchanged.

## Current state

- `src/cron/store.rs` — `due_jobs`: `WHERE enabled = 1 AND next_run <= ?1 ORDER BY
  next_run ASC LIMIT ?2` (`?2` = `config.scheduler.max_tasks`, default 64). No
  staleness bound.
- `src/cron/store.rs` — `reschedule_after_run(config, job, success, output)`:

  ```rust
  pub fn reschedule_after_run(config: &Config, job: &CronJob, success: bool, output: &str) -> Result<()> {
      let now = Utc::now();
      let next_run = next_run_for_schedule(&job.schedule, now)?;   // re-anchors to now (coalesce) — KEEP
      // ... UPDATE next_run, last_run, last_status, last_output ...
  }
  ```

  **This function stays exactly as-is** — re-anchoring to `now` IS the coalesce
  semantics this plan wants. Do NOT change it (the earlier draft's `Every` anchor
  is deliberately dropped).
- `src/cron/scheduler.rs` — the `run` loop: each tick reloads config, then builds
  `due_jobs(&working, Utc::now())` inside `spawn_blocking` and hands the result to
  `process_due_jobs(&config, &security, jobs, SCHEDULER_COMPONENT)`. (Note: since
  the DB-hardening work, `due_jobs` is already wrapped in `spawn_blocking` and the
  batch runs on its own JoinSet task — re-read the loop and keep that shape.)
- `src/cron/scheduler.rs` — `process_due_jobs` runs the batch with a
  `buffer_unordered(config.scheduler.max_concurrent.max(1))` stream (default 4),
  each job claiming a process-wide `InFlightGuard`. This bounds *concurrency* but
  not *batch arrival*.
- `src/config/schema.rs` — `CronConfig`:

  ```rust
  pub struct CronConfig {
      #[serde(default = "default_true")]
      pub enabled: bool,
      #[serde(default = "default_max_run_history")]
      pub max_run_history: u32,
  }
  fn default_max_run_history() -> u32 { 50 }
  impl Default for CronConfig {
      fn default() -> Self { Self { enabled: true, max_run_history: default_max_run_history() } }
  }
  ```

- `src/config/migrations.rs` — `pub const CURRENT_VERSION: u32 = 22;` (verify;
  bump to 23, or to next+1 if another migration has since landed). The migration
  runner has one `if from < N { ... }` arm per version; the most recent is an
  additive-default-only "burn a version slot" arm. `set_schema_version(raw,
  CURRENT_VERSION)` is stamped unconditionally.
- `tests/schema_drift.rs` — `config_schema_does_not_drift_unannounced` snapshots
  the `Config` JSON Schema with `snapshot_suffix => v{CURRENT_VERSION}`. Bumping
  `CURRENT_VERSION` produces a new `tests/snapshots/schema_drift__config_schema@v23.snap`
  that must be accepted.

Repo conventions:
- Config additions: additive field + `#[serde(default = "...")]` + `fn default_*`
  + update `Default` impl. A scheduler-behavior default change requires a schema
  version bump (§3.6) — hence the migration arm and snapshot refresh.
- Errors via `anyhow`; scheduler logs via `tracing::warn!(target: "scheduler", ...)`.

## Commands you will need

| Purpose        | Command                                                    | Expected on success        |
|----------------|-----------------------------------------------------------|----------------------------|
| Format         | `cargo fmt --all -- --check`                              | exit 0, no diff            |
| Lint           | `cargo clippy --all-targets -- -D warnings`               | exit 0, no warnings        |
| Cron unit tests| `cargo test --lib cron`                                   | all pass, incl. new tests  |
| Migration test | `cargo test --lib migrations`                             | all pass, incl. new test   |
| Schema drift   | `INSTA_UPDATE=auto cargo test --test schema_drift`        | passes after snapshot accept |

Do NOT run a bare `cargo test` (disk-constrained box). The `--test schema_drift`
run compiles the whole lib but is required for the snapshot refresh; run it once,
after Step 3.

## Scope

**In scope**:
- `src/config/schema.rs` — add `CronConfig.max_catchup_age_secs` (default 86400).
- `src/config/migrations.rs` — bump `CURRENT_VERSION` 22→23, add the arm + a test.
- `tests/snapshots/schema_drift__config_schema@v23.snap` — new accepted snapshot.
- `src/cron/store.rs` — new `is_run_stale` + `set_next_run` + `skip_stale_run`
  helpers; tests. (`reschedule_after_run` is NOT modified.)
- `src/cron/scheduler.rs` — partition stale jobs out of the due batch in the `run`
  loop (skip+re-anchor+log; run the fresh set); optional startup stagger; tests.
- `docs/reference/config.md` — a brief `[cron]` note (Step 7).

**Out of scope** (do NOT touch):
- `reschedule_after_run` — its re-anchor-to-now IS the coalesce behavior; keep it.
- Any occurrence-by-occurrence replay for `Cron` OR `Every` — coalesce-to-one is
  the whole point; do not reintroduce catch-up-per-missed-occurrence.
- `due_jobs` SQL selection — leave the predicate as-is; staleness is handled in
  the scheduler, not by narrowing the query (a narrowed query would leave stale
  rows perpetually selected-and-skipped without advancing `next_run`).
- `persist_job_result` one-shot delete/disable logic — unchanged.

## Git workflow

- Branch (content-based, e.g.): `feat/cron-catchup-staleness-gate`.
- Conventional commits (config; migration; scheduler).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: (design — no action) confirm the coalesce-one model

No STOP. The model, for reference while implementing:
- A due job within `max_catchup_age_secs` fires **once** (existing behavior) and
  re-anchors forward (existing `reschedule_after_run`). This coalesces missed
  occurrences — intended.
- A due job whose `next_run` is **older** than `max_catchup_age_secs` is **not
  fired**: a recurring schedule re-anchors to its next future occurrence (and
  stays enabled); a one-shot `At` is disabled. The skip is logged.
- Default `max_catchup_age_secs = 86400` (1 day): matches AWS EventBridge's 24h
  `MaximumEventAgeInSeconds` and comfortably covers any real restart/deploy
  window while dropping a months-late fire. `0` = gate disabled (a due job always
  fires once — still coalesced, so still safe; NOT "replay everything").

### Step 2: Add the config field (default 86400)

In `src/config/schema.rs`, extend `CronConfig`:

```rust
pub struct CronConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_run_history")]
    pub max_run_history: u32,
    /// Skip (and log) a scheduled run whose `next_run` is older than this many
    /// seconds instead of firing it "late" on restart. The job is not lost — a
    /// recurring schedule re-anchors to its next future occurrence; a one-shot
    /// `At` is disabled. Firing that does happen is always coalesced to a single
    /// run (missed occurrences are never replayed). `0` disables the gate (a due
    /// job always fires once).
    #[serde(default = "default_max_catchup_age_secs")]
    pub max_catchup_age_secs: u64,
}

fn default_max_catchup_age_secs() -> u64 {
    86_400 // 1 day
}
```

Update the `Default` impl to include `max_catchup_age_secs:
default_max_catchup_age_secs()`. If a `CronConfig` default test exists, extend it.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Bump the schema version, add a migration arm + test, refresh the snapshot

In `src/config/migrations.rs`:
- Change `pub const CURRENT_VERSION: u32 = 22;` to `23` (or next+1 if already >22).
- After the current top `if from < 22 { ... }` arm, add an additive-default-only
  arm modeled on it:

  ```rust
  // v22 → v23: `[cron] max_catchup_age_secs` (u64, default 86400) was added — a
  // staleness gate that skips-and-re-anchors a missed scheduled run older than
  // the window instead of firing it late on restart. Additive with a serde
  // default, nothing to transform — this arm burns a version slot so the
  // schema_drift fingerprint is accepted with intent.
  if from < 23 {
      // (no transformation; additive default-only field)
  }
  ```

- Add a v22→v23 migration unit test near the other version tests: a config at
  `schema_version = 22` migrates to `23` with content intact.

Refresh the snapshot: `INSTA_UPDATE=auto cargo test --test schema_drift`. Confirm
the new `@v23.snap` file exists and `git status` shows it added; re-run without
`INSTA_UPDATE` → passes.

**Verify**: `cargo test --lib migrations` passes (incl. the v23 test);
`cargo test --test schema_drift` passes; the `@v23.snap` file is added.

### Step 4: Add the staleness helpers (store.rs)

`reschedule_after_run` is NOT touched. Add three small, testable helpers to
`src/cron/store.rs`:

```rust
/// A due run is "stale" when its scheduled instant is older than `max_age_secs`.
/// `0` disables the gate (never stale) — a due job then always fires once.
pub fn is_run_stale(next_run: DateTime<Utc>, now: DateTime<Utc>, max_age_secs: u64) -> bool {
    if max_age_secs == 0 {
        return false;
    }
    let Ok(secs) = i64::try_from(max_age_secs) else { return false };
    match now.checked_sub_signed(chrono::Duration::seconds(secs)) {
        Some(cutoff) => next_run < cutoff,
        None => false,
    }
}

/// Overwrite only the `next_run` column (no run-history / last_* side effects).
pub fn set_next_run(config: &Config, job_id: &str, next_run: DateTime<Utc>) -> Result<()> {
    with_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET next_run = ?1 WHERE id = ?2",
            params![next_run.to_rfc3339(), job_id],
        )
        .context("Failed to set cron next_run")?;
        Ok(())
    })
    .map(|_| ())
}

/// Skip a stale due job WITHOUT running it: a recurring schedule re-anchors to
/// its next future occurrence (and stays enabled — never silently disable a
/// recurring job on a long outage, the K8s footgun); a one-shot `At` is disabled
/// (a missed reminder must not fire late).
pub fn skip_stale_run(config: &Config, job: &CronJob, now: DateTime<Utc>) -> Result<()> {
    match job.schedule {
        Schedule::At { .. } => {
            update_job(
                config,
                &job.id,
                CronJobPatch { enabled: Some(false), ..CronJobPatch::default() },
            )?;
        }
        _ => {
            let next_run = next_run_for_schedule(&job.schedule, now)?;
            set_next_run(config, &job.id, next_run)?;
        }
    }
    Ok(())
}
```

Match the exact `with_connection` return shape used by the sibling store fns
(some return `Result<T>` and need a `.map(|_| ())`; adapt if the compiler
complains). Re-export `is_run_stale`, `set_next_run`, `skip_stale_run` from
`src/cron/mod.rs`'s `pub use store::{...}` block.

**Verify**: `cargo test --lib cron` → still green; `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 5: Partition stale jobs out of the due batch (scheduler.rs)

In the `run` loop, after `due_jobs` returns the batch and before handing it to
`process_due_jobs`, split off the stale jobs, skip+re-anchor+log them, and run
only the fresh set. Adapt to the current loop shape (the batch is spawned on a
JoinSet; `due_jobs` runs in `spawn_blocking`):

```rust
        let now = Utc::now();
        // ... existing spawn_blocking(due_jobs) producing `jobs` ...

        let max_age = working.cron.max_catchup_age_secs;
        let (fresh, stale): (Vec<CronJob>, Vec<CronJob>) = jobs
            .into_iter()
            .partition(|j| !crate::cron::is_run_stale(j.next_run, now, max_age));

        for job in &stale {
            tracing::warn!(
                target: "scheduler",
                job_id = %job.id,
                next_run = %job.next_run.to_rfc3339(),
                "skipping stale cron run (older than max_catchup_age_secs); re-anchoring, not firing"
            );
            if let Err(e) = crate::cron::skip_stale_run(&working, job, now) {
                tracing::warn!(target: "scheduler", job_id = %job.id, error = %e, "failed to re-anchor stale cron job");
            }
        }

        // hand only `fresh` to the batch (was: `jobs`)
```

Use whichever config binding the current loop holds (`working` post-reload, or
`config`); the `skip_stale_run` DB writes are cheap and can run inline on the
poll task (they are single-row UPDATEs) or, to match the DB-hardening pattern,
inside a `spawn_blocking` — prefer consistency with the surrounding code.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0; `cargo test --lib cron` → pass.

### Step 6 (optional — may split to a follow-up): startup stagger

Add a bounded, deterministic per-slot stagger so a big first-tick batch does not
all launch at once. Pure helper in `src/cron/scheduler.rs`:

```rust
/// Deterministic per-slot stagger (ms) to spread a large due batch across a
/// short window. Returns 0 when the batch fits the concurrency budget.
fn batch_stagger_ms(index: usize, batch_len: usize, max_concurrent: usize, cap_ms: u64) -> u64 {
    if batch_len <= max_concurrent {
        return 0;
    }
    (index as u64 * cap_ms) / (batch_len.max(1) as u64)
}
```

Apply inside `process_due_jobs` (enumerate jobs; `time::sleep(Duration::from_millis(
batch_stagger_ms(index, batch_len, max_concurrent, STARTUP_STAGGER_CAP_MS)))` at
the top of each job's async block, before claiming the in-flight guard). Add
`const STARTUP_STAGGER_CAP_MS: u64 = 2_000;` near the other consts. Zero when
`batch_len <= max_concurrent`, so existing `process_due_jobs_*` tests are
unaffected.

> If wiring the sleep into the stream proves fiddly, ship Steps 1-5 (the
> correctness fixes) and split this stagger to a follow-up — note that in the PR.
> Do NOT block the correctness fixes on the stagger.

**Verify**: `cargo test --lib cron` → pass (incl. the `batch_stagger_ms` test).

### Step 7: Tests + docs

Add the tests below, then add a short `[cron]` note to `docs/reference/config.md`
documenting `max_catchup_age_secs` (default 86400, coalesce-to-one semantics, `0`
= gate off). Keep it to a couple of sentences if there is no per-section table.

**Verify**: `cargo test --lib cron` and `cargo test --lib migrations` → pass.

## Test plan

New tests:
- `src/cron/store.rs::tests`:
  - `is_run_stale_respects_cutoff_and_zero_disables` — `0` ⇒ never stale; a
    `next_run` older than the cutoff ⇒ stale; within the cutoff ⇒ not stale.
    **Mutation check**: flip `next_run < cutoff` to `>` (or the `== 0` guard to
    `!= 0`) and confirm this test FAILS; restore.
  - `skip_stale_run_reanchors_recurring_and_disables_oneshot` — a `Cron`/`Every`
    job's `next_run` is advanced to a future instant and it stays enabled; an
    `At` one-shot is disabled. (Insert a job with a known past `next_run` via a
    raw UPDATE / `set_next_run`, then call `skip_stale_run`.)
- `src/cron/scheduler.rs::tests`:
  - `batch_stagger_ms_is_zero_within_budget_and_bounded_above` — 0 when
    `batch_len <= max_concurrent`; otherwise every value `<= cap_ms`.
  - (if practical) a test that a stale due job is partitioned out and NOT run:
    seed a job with a very-old `next_run`, a small `max_catchup_age_secs`, drive
    one poll cycle (or the partition directly), assert the job did not execute
    (no new run-history row) and its `next_run` advanced to the future.
- `src/config/migrations.rs::tests`: a v22→v23 additive-migration test.

Structural patterns to copy: `reschedule_after_run_persists_last_status_and_last_run`
(store.rs) for store-test setup; an existing additive-migration test; the
existing `process_due_jobs_*` tests for scheduler setup.

Verification: `cargo test --lib cron`, `cargo test --lib migrations`, and
`cargo test --test schema_drift` all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; new store + scheduler tests pass
- [ ] `cargo test --lib migrations` exits 0; the v23 migration test passes
- [ ] `cargo test --test schema_drift` exits 0; `tests/snapshots/schema_drift__config_schema@v23.snap` is added
- [ ] `grep -n "CURRENT_VERSION: u32 = 23" src/config/migrations.rs` matches (or next+1)
- [ ] `grep -n "86_400\|86400" src/config/schema.rs` shows the new default
- [ ] `reschedule_after_run` is UNCHANGED (`git diff` on it is empty) — no occurrence-replay was reintroduced
- [ ] `is_run_stale` mutation check: flipping the comparison makes its test fail
- [ ] Only in-scope files are modified (`git status`)
- [ ] `plans/README.md` status row updated; the PR/CHANGELOG records the new default

## STOP conditions

Stop and report back (do not improvise) if:

- `CronConfig`, `CURRENT_VERSION`, `reschedule_after_run`, or the scheduler `run`
  loop no longer matches the excerpts (drift since 434141c) — re-locate and
  re-verify before proceeding.
- `CURRENT_VERSION` is already `> 22` (another migration landed): add your arm and
  bump to the next number; the snapshot suffix follows the new version.
- Implementing the staleness partition appears to require changing
  `reschedule_after_run` or `next_run_for_schedule` — it must not; you only add
  the pre-`process_due_jobs` partition + the skip helpers.
- `cargo test --test schema_drift` fails to compile or the snapshot cannot be
  accepted after two attempts.

## Maintenance notes

- **Design lineage**: coalesce-to-one is the researched norm (K8s CronJob
  coalesces then caps; systemd `Persistent=` fires once; anacron runs an overdue
  job once; Quartz `SMART_POLICY` fires one catch-up; Celery/systemd-monotonic
  re-anchor to now). `max_catchup_age_secs` is a staleness gate on that single
  run, mirroring AWS's 24h event-age default — NOT a replay window. If a future
  change is tempted to "replay missed occurrences," that is a conscious reversal
  of this decision; require an explicit product call.
- **Footgun avoided**: `skip_stale_run` never disables a *recurring* job (only a
  one-shot `At`), so a long outage cannot silently kill a schedule the way K8s'
  `null` deadline + fixed 100-missed cap does.
- Coordinates with plan 165 (startup batching): the staleness partition shrinks
  the batch 165 also addresses. If 165 lands first, the Step 6 stagger may be
  redundant — check before implementing.
- A reviewer should scrutinize: (1) `reschedule_after_run` is untouched; (2) `0`
  truly disables the gate end-to-end; (3) the migration is additive-only and the
  snapshot diff is exactly the one new field; (4) a stale `At` one-shot is
  disabled, a stale recurring job is re-anchored-and-kept-enabled.
