# Plan 168: Cron delivery records the execution result (not the delivery result) and never announces a security refusal

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs`
> If that file changed since this plan was written, compare the "Current state"
> excerpts against the live code before proceeding; on a mismatch, treat it as
> a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW-MED
- **Depends on**: none
- **Category**: bug / security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

Two defects live in `persist_job_result` (scheduled cron path):

1. **Status is conflated with delivery.** `deliver_if_configured` is called
   *unconditionally* before any success gate, and when delivery fails with
   `best_effort = false` it flips `success = false`. The job's recorded
   `last_status` (and the `cron_runs` row) then reads `"error"` for a job that
   actually **executed fine** — only the chat delivery hiccuped. Any alerting
   keyed on `last_status` fires on the wrong subsystem.
2. **A security refusal is announced verbatim into chat.** `deliver_if_configured`
   pushes the raw job `output` to the configured channel with no check on
   whether the job succeeded. When a shell job is refused, `output` is a string
   like `"blocked by security policy: command not allowed: <the full command>"`
   — the command text plus policy internals — and that gets sent straight into
   whatever chat `delivery` points at. The **load-bearing fix** here is the same
   reordering as point 1: record the execution result **before** delivery can
   mutate `success`, then gate delivery on `success`. Every security refusal is
   produced together with `success = false`, so gating delivery on `success`
   alone already suppresses delivery of every refusal. The explicit
   `!is_security_refusal(output)` check is therefore **defense-in-depth** — a
   redundant second guard should a future refusal path ever return
   `success = true` — not the primary suppression mechanism.

After this plan: the **execution** outcome is recorded first and is the only
thing that sets `last_status`; delivery is a separate, best-effort step that
never mutates execution success; and delivery is suppressed when the job failed
or its output is a security refusal, so command text / policy internals never
reach a chat.

## Current state

- `src/cron/scheduler.rs` — the scheduled cron execution + persistence path.
  - `persist_job_result` (lines 265–322) — the function to change.
  - `deliver_if_configured` (lines 360–394) — announce-mode delivery.
  - Refusal strings are produced in `run_job_command_with_timeout`
    (lines 490–534); every one begins with the exact prefix
    `blocked by security policy:`. The retry layer already treats that prefix as
    a marker: `src/cron/scheduler.rs:124` does
    `if last_output.starts_with("blocked by security policy:")`.
  - `execute_and_persist_job` (lines 186–199) is the sole caller; it passes the
    real execution `(success, output)` into `persist_job_result`.

Current `persist_job_result` (lines 265–322), verbatim:

```rust
async fn persist_job_result(
    config: &Config,
    job: &CronJob,
    mut success: bool,
    output: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
) -> bool {
    let duration_ms = (finished_at - started_at).num_milliseconds();

    if let Err(e) = deliver_if_configured(config, job, output).await {
        if job.delivery.best_effort {
            tracing::warn!("Cron delivery failed (best_effort): {e}");
        } else {
            success = false;
            tracing::warn!("Cron delivery failed: {e}");
        }
    }

    let _ = record_run(
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
            if let Err(e) = remove_job(config, &job.id) {
                tracing::warn!("Failed to remove one-shot cron job after success: {e}");
            }
        } else {
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

    if let Err(e) = reschedule_after_run(config, job, success, output) {
        tracing::warn!("Failed to persist scheduler run result: {e}");
    }

    success
}
```

Current `deliver_if_configured` (lines 360–394), verbatim — note it takes
`output` and sends it as-is at line 391:

```rust
async fn deliver_if_configured(config: &Config, job: &CronJob, output: &str) -> Result<()> {
    let delivery: &DeliveryConfig = &job.delivery;
    if !delivery.mode.eq_ignore_ascii_case("announce") {
        return Ok(());
    }
    // ...resolve channel + target, gate on channel_supports_announce_delivery,
    // build the channel...
    channel_impl.send(&SendMessage::new(output, target)).await?;
    Ok(())
}
```

Test fixtures already present in the `#[cfg(test)] mod tests` block at the
bottom of this file (reuse them):

- `test_config(&tmp)` (lines ~580–589) builds a `Config` with a temp workspace.
- `test_job(command)` (lines ~591–614) builds a shell `CronJob` with
  `DeliveryConfig::default()` (mode `"none"`).
- Existing persist tests to model after:
  `persist_job_result_records_run_and_reschedules_shell_job` (lines 964–979),
  `persist_job_result_failure_disables_one_shot` (lines 1006–1030). They call
  `persist_job_result(&config, &job, <success>, <output>, started, finished)`
  then assert `cron::get_job(&config, &job.id).unwrap().last_status`.

Repo conventions: `bail!`/explicit errors on failure paths (KISS, §3.5 of
`CLAUDE.md`); duplicate small local logic rather than over-abstract
(rule-of-three). Match the existing `tracing::warn!` style already in this
function.

## Commands you will need

| Purpose   | Command                                             | Expected on success       |
|-----------|-----------------------------------------------------|---------------------------|
| Format    | `cargo fmt --all -- --check`                        | exit 0, no diff           |
| Lint      | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings       |
| Tests     | `cargo test --lib cron`                             | all pass incl. new tests  |
| Drift     | `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs` | (see drift check)    |

Do **not** run a bare `cargo test` (workspace test is disk-heavy on this box).

## Scope

**In scope** (the only files you should modify):

- `src/cron/scheduler.rs`

**Out of scope** (do NOT touch):

- `src/cron/store.rs` — `record_run`, `record_last_run`, `reschedule_after_run`
  keep their signatures; you only change *what status value* you pass them.
- `run_job_manual` (scheduler.rs lines 79–96) — it deliberately does NOT run
  delivery; leave it exactly as is.
- `DeliveryConfig` / any schema change — do **not** add a DB column for delivery
  status in this plan (see Maintenance notes for the deferred richer variant).
- The refusal strings themselves (lines 490–534) — do not reword them; other
  code (line 124) depends on the `blocked by security policy:` prefix.

## Git workflow

- Branch: `advisor/168-cron-announce-status-and-refusal-leak`
- Conventional commits, e.g.
  `fix(cron): record execution status before delivery; never announce a refusal`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a private refusal-marker helper

At module scope in `src/cron/scheduler.rs` (near the other private helpers,
e.g. just above `deliver_if_configured`), add:

```rust
/// The scheduled path's security refusals (autonomy read-only, rate limit,
/// command not allowed, risk gate, forbidden path, budget) all begin with this
/// exact prefix (see `run_job_command_with_timeout`). Delivery must never push
/// such a string into a chat: it carries the rejected command text and policy
/// internals.
const SECURITY_REFUSAL_PREFIX: &str = "blocked by security policy:";

fn is_security_refusal(output: &str) -> bool {
    output.starts_with(SECURITY_REFUSAL_PREFIX)
}
```

(Optionally reuse `SECURITY_REFUSAL_PREFIX` at line 124 in place of the literal,
but that is not required.)

**Verify**: `cargo fmt --all -- --check` → exit 0.

### Step 2: Reorder `persist_job_result` — record execution first, deliver after, never conflate

Rewrite the top of `persist_job_result` so that:

1. The `success` parameter is **no longer `mut`** and is never reassigned from a
   delivery outcome. Change the signature parameter `mut success: bool` to
   `success: bool`.
2. `record_run` is called **first**, using the execution `success` for its
   status — before any delivery.
3. Delivery runs **after** recording, and only when the job **succeeded** and its
   output is **not** a security refusal. Delivery failure only logs; it never
   touches `success`.

Target shape for the top of the function (replace the current lines 273–292):

```rust
    let duration_ms = (finished_at - started_at).num_milliseconds();

    // Record the EXECUTION outcome first. `last_status` / the `cron_runs` row
    // describe whether the JOB ran — delivery is a separate concern and must
    // never flip this (a chat hiccup is not a job failure).
    let _ = record_run(
        config,
        &job.id,
        started_at,
        finished_at,
        if success { "ok" } else { "error" },
        Some(output),
        duration_ms,
    );

    // Deliver only a job that actually succeeded and whose output is not a
    // security refusal. A refused job's output is the rejected command text +
    // policy internals; announcing it verbatim would leak it into the
    // configured chat. Delivery is best-effort: its failure is logged, never
    // recorded as a job error.
    if success && !is_security_refusal(output) {
        if let Err(e) = deliver_if_configured(config, job, output).await {
            if job.delivery.best_effort {
                tracing::warn!("Cron delivery failed (best_effort): {e}");
            } else {
                tracing::warn!("Cron delivery failed: {e}");
            }
        }
    } else if job.delivery.mode.eq_ignore_ascii_case("announce") {
        // Announce was requested but withheld: do not push failed/ refused
        // output into chat. Stated once, without the raw output.
        tracing::warn!(
            "Cron job '{}' output withheld from delivery ({})",
            job.id,
            if is_security_refusal(output) { "security refusal" } else { "job failed" }
        );
    }
```

Leave everything from `if is_one_shot(job) {` onward unchanged — it already keys
off the (now delivery-independent) execution `success`.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0 (in particular
no "variable does not need to be mutable" warning for `success`).

### Step 3: Add regression tests

In the `#[cfg(test)] mod tests` block at the bottom of
`src/cron/scheduler.rs`, add two tests modelled on
`persist_job_result_records_run_and_reschedules_shell_job`:

1. `persist_job_result_delivery_failure_does_not_mark_job_errored` — build a
   job whose `delivery` requests announce to a channel that is NOT configured in
   the test `Config` (so `deliver_if_configured` returns `Err`), with
   `best_effort = false`. Call
   `persist_job_result(&config, &job, true, "job ran fine", started, finished)`
   and assert:
   - the returned bool is `true`, and
   - `cron::get_job(&config, &job.id).unwrap().last_status.as_deref() == Some("ok")`.

   Construct the job with `cron::add_job(...)` — the job must be persisted so
   `get_job` can read its `last_status` — and override `delivery`:
   ```rust
   let mut job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
   job.delivery = DeliveryConfig {
       mode: "announce".into(),
       channel: Some("telegram".into()),
       to: Some("123".into()),
       best_effort: false,
   };
   ```
   (No `telegram` config is present in `test_config`, so delivery errors with
   "telegram channel not configured" — proving delivery failure no longer
   flips status.)

2. `persist_job_result_does_not_deliver_a_security_refusal` — a **direct unit
   test of the refusal marker**, NOT a behavioural guard on the suppression
   branch. The full "delivery is not invoked on a refusal" behaviour is **not
   isolatable in this unit harness**: no channel is available, so proving it
   would require a delivery spy/counter. Do not claim this test guards
   suppression. Its teeth are the two direct assertions:
   `assert!(is_security_refusal("blocked by security policy: command not allowed: x"));`
   and `assert!(!is_security_refusal("all good"));`.
   You MAY also call
   `persist_job_result(&config, &job, false, "blocked by security policy: command not allowed: rm -rf /", started, finished)`
   and assert `last_status == Some("error")` and that it returns `false`, but be
   aware those assertions hold **identically before and after the fix** (a job
   passed `success = false` records `"error"` and returns `false` in either
   version, and they survive deleting the refusal guard) — they are
   documentation, not regression proof. The marker assertions are what this test
   actually verifies.

**Verify**: `cargo test --lib cron` → all pass, including the 2 new tests.

## Test plan

- **Primary regression proof — Test 1:**
  `persist_job_result_delivery_failure_does_not_mark_job_errored` — the
  status-conflation A/B: a job that executes `success = true` but whose delivery
  fails records `"ok"`, not `"error"`, after the fix. This is the exact
  regression and the only new test that actually distinguishes fixed from
  unfixed code.
- **Test 2 —** `persist_job_result_does_not_deliver_a_security_refusal`: a direct
  unit test of `is_security_refusal` (the two marker assertions are its teeth).
  It does **not** guard refusal-suppression — the full "delivery is not invoked
  on a refusal" behaviour is not isolatable in this unit harness (it would need a
  delivery spy/counter), and any `last_status`/return-value assertions on a
  refused job hold identically before and after the fix.
- Model after `persist_job_result_records_run_and_reschedules_shell_job`
  (lines 964–979) for the config/job/started/finished setup and the
  `cron::get_job(...).last_status` assertion.
- Verification: `cargo test --lib cron` → all pass, including 2 new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the 2 new tests exist and pass
- [ ] `success` is no longer reassigned from a delivery result anywhere in
      `persist_job_result` (`grep -n "success = false" src/cron/scheduler.rs`
      shows no hit inside `persist_job_result`)
- [ ] `deliver_if_configured` is called only inside the `if success && !is_security_refusal(...)` guard
- [ ] No files outside `src/cron/scheduler.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The `persist_job_result` or `deliver_if_configured` bodies don't match the
  "Current state" excerpts (drift since this plan was written).
- Removing the `success = false` reassignment breaks an existing test that
  *asserts* a delivery failure marks the job errored — that would mean the old
  conflated behavior was deliberately depended on; report it rather than
  deleting the test.
- A verification fails twice after a reasonable fix attempt.
- You find delivery being triggered from any path other than `persist_job_result`
  (it should not be — `run_job_manual` explicitly skips it).

## Maintenance notes

- **Behavior change to recorded statuses**: after this plan, `best_effort = false`
  no longer flips `last_status`/`cron_runs.status` to `"error"` on a delivery
  failure. Any dashboard or alert keyed on cron `last_status` will now reflect
  execution only — which is the intended fix. Call this out in the PR body.
- **Deferred richer variant**: if operators later need to *see* delivery
  outcomes, add a dedicated `last_delivery_status` column (schema migration +
  `record_run`/`record_last_run` changes) rather than reusing `last_status`.
  Explicitly out of scope here to keep the change reversible and schema-stable.
- **Refusal marker coupling**: `is_security_refusal` and the retry check at
  line 124 both depend on the `blocked by security policy:` prefix produced in
  `run_job_command_with_timeout`. If those messages are ever reworded, both
  readers must be updated together.
- Reviewer should scrutinize: that `record_run` now runs before delivery, that
  no `success` mutation survives, and that the suppression branch cannot itself
  panic or send.
