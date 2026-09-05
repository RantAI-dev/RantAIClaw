# Plan 163: Stop one-shot agent cron jobs from cascade-deleting their own run history

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/tools/cron_add.rs src/cron/scheduler.rs src/cron/store.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P0
- **Effort**: S
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Cross-ref**: plans that also touch `delete_after_run` / one-shot semantics
  (shell delete-after-run and shell delivery). Coordinate if executed together;
  this plan changes only the **default** for agent one-shots.

## Why this matters

The single most common cron use case — "remind me / do X in N minutes" — is an
**agent one-shot** (`Schedule::At`). Today such a job runs, produces output,
records it to run history, and then **deletes both itself and the only record
of that output**, leaving the user with nothing. The tool's own description
promises the opposite: that without a delivery channel "its output is only
recorded in run history (visible in the Schedules view)". Silent, total data
loss on the happy path.

The chain:

1. `cron_add.rs:151` defaults `delete_after_run = true` for **every** `At`
   (one-shot) job.
2. After a successful run, `persist_job_result` calls `remove_job` because
   `delete_after_run && success` (`scheduler.rs:295-296`).
3. `cron_runs` has `ON DELETE CASCADE` on `job_id` (`store.rs:554`) with
   `PRAGMA foreign_keys = ON` (`store.rs:524`), so removing the job deletes the
   run row that was just inserted.

After this plan, an agent one-shot with **no delivery** keeps its run row
(job disabled + kept, so the output survives in Schedules history); only jobs
whose output already reached the user another way (announce delivery) or that
the user explicitly opted into `delete_after_run` are removed.

## Current state

### The bad default — `src/tools/cron_add.rs:151-155`

```rust
let default_delete_after_run = matches!(schedule, Schedule::At { .. });
let delete_after_run = args
    .get("delete_after_run")
    .and_then(serde_json::Value::as_bool)
    .unwrap_or(default_delete_after_run);
```

`default_delete_after_run` is computed **before** the `match job_type` block
and is used **only** in the `JobType::Agent` branch (passed to
`cron::add_agent_job` at `cron_add.rs:241`). The `JobType::Shell` branch calls
`cron::add_shell_job` (`cron_add.rs:182`), which does **not** take
`delete_after_run` (shell one-shots default to `false` — see the scheduler test
`persist_job_result_disables_shell_one_shot_instead_of_refiring`,
`scheduler.rs:1073-1110`).

`delivery` is parsed later, inside the agent branch, at `cron_add.rs:215-227`
into an `Option<DeliveryConfig>`. `DeliveryConfig` has a `mode: String` field
(values like `"announce"`; see `scheduler.rs:362` which checks
`delivery.mode.eq_ignore_ascii_case("announce")`).

### The tool's promise — `src/tools/cron_add.rs:69-73`

```
Without `delivery`, the job still runs on schedule but its output is only
recorded in run history (visible in the Schedules view) — it is NOT pushed
anywhere.
```

This promise is what the current default breaks.

### The cascade delete — `src/cron/scheduler.rs:294-315`

```rust
if is_one_shot(job) {
    if job.delete_after_run && success {
        if let Err(e) = remove_job(config, &job.id) {
            tracing::warn!("Failed to remove one-shot cron job after success: {e}");
        }
    } else {
        // Not opted into auto-delete (or it failed): keep the row for history
        // but disable it so the poller can't re-fire this already-past `At`.
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
```

The `else` branch is exactly the behavior we want for a no-delivery one-shot:
keep the row, disable it. The bug is that the bad default routes these jobs into
the `if` branch (`remove_job` → cascade).

The run row is inserted just above, at `scheduler.rs:284-292` (`record_run`),
so the cascade destroys a row written milliseconds earlier.

### The cascade FK — `src/cron/store.rs:524` and `src/cron/store.rs:554`

```
PRAGMA foreign_keys = ON;
...
CREATE TABLE IF NOT EXISTS cron_runs (
    ...
    FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
);
```

Do **not** change the schema in this plan (see Scope). The fix is at the
default-selection layer.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format check | `cargo fmt --all -- --check` | exit 0, no diff |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0, no warnings |
| Scoped tests (tool) | `cargo test --lib tools::cron_add` | all pass, incl. new tests |
| Scoped tests (scheduler) | `cargo test --lib cron::scheduler` | all pass |

Do **NOT** run bare `cargo test` — it builds ~27G and will exhaust the disk.

## Scope

**In scope** (the only files you should modify):
- `src/tools/cron_add.rs` — change the `delete_after_run` default + new tests
- `src/cron/scheduler.rs` — (optional) one scheduler-level regression test

**Out of scope** (do NOT touch, even though they look related):
- `src/cron/store.rs` — leave the FK cascade and schema exactly as-is. The fix
  is to stop deleting the job, not to detach run rows. (An alternative approach
  that nulls/archives the FK before delete is noted in Maintenance notes but is
  deliberately NOT taken here — it needs an orphan-pruning story.)
- CLI one-shot commands (`add-at`, `once`) that create **shell** one-shots —
  those already default to `delete_after_run = false` and are unaffected.

## Git workflow

- Branch: `advisor/163-cron-oneshot-output-cascade-loss`
- Conventional-commit title, e.g.
  `fix(cron): keep one-shot agent run history unless output was delivered`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Default `delete_after_run` on delivery, not merely on being one-shot

In `src/tools/cron_add.rs`, remove the pre-`match` computation at lines
151-155, and instead compute the default **inside the `JobType::Agent` branch**,
after `delivery` is parsed (after `cron_add.rs:227`).

Delete this (lines 151-155):

```rust
let default_delete_after_run = matches!(schedule, Schedule::At { .. });
let delete_after_run = args
    .get("delete_after_run")
    .and_then(serde_json::Value::as_bool)
    .unwrap_or(default_delete_after_run);
```

Then, in the `JobType::Agent` branch, after the `let delivery = match args.get("delivery") { … };`
block (currently `cron_add.rs:215-227`) and before the
`enforce_mutation_allowed` call, insert:

```rust
// Auto-delete a fired one-shot only when its output was delivered to the
// user another way (announce delivery). Without delivery, the ONLY record
// of the output is the run-history row — deleting the job would cascade
// that row away (cron_runs FK ON DELETE CASCADE), so keep+disable instead.
let delivered = delivery
    .as_ref()
    .is_some_and(|d| d.mode.eq_ignore_ascii_case("announce"));
let default_delete_after_run = matches!(schedule, Schedule::At { .. }) && delivered;
let delete_after_run = args
    .get("delete_after_run")
    .and_then(serde_json::Value::as_bool)
    .unwrap_or(default_delete_after_run);
```

Leave the explicit-override behavior intact: an explicit `delete_after_run` in
the tool args still wins via `unwrap_or`. The `delete_after_run` variable is
consumed at the `cron::add_agent_job(...)` call (`cron_add.rs:233-242`); make
sure it is in scope there.

Confirm `schedule` is still available for the `matches!` — it is passed **by
value** into `add_agent_job` on the same call, so the `matches!` borrow must
come first (it does, since you compute the default just above the call).

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0, no warnings.

### Step 2: Test — no-delivery agent one-shot defaults to keep

Add a test in the `#[cfg(test)] mod tests` block of `src/tools/cron_add.rs`.
The existing tests build a `CronAddTool`, call `.execute(json!({…}))`, and read
`result.output` / `result.error`. The tool's success output JSON includes the
job `id` (see `cron_add.rs:249-256`) but **not** `delete_after_run`, so read the
stored job back via `cron::get_job` to assert the flag.

Structure (model after `adds_shell_job`, `cron_add.rs:294-310`):

```rust
#[tokio::test]
async fn agent_oneshot_without_delivery_keeps_run_history() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

    // A future `at` one-shot agent job, NO delivery.
    let at = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    let result = tool
        .execute(json!({
            "schedule": { "kind": "at", "at": at },
            "job_type": "agent",
            "prompt": "remind me"
        }))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);

    // Parse the job id out of the output JSON and load the stored job.
    let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    let id = v["id"].as_str().unwrap();
    let job = crate::cron::get_job(&cfg, id).unwrap();
    assert!(
        !job.delete_after_run,
        "a no-delivery agent one-shot must NOT auto-delete (would cascade away its run history)"
    );
}
```

Confirm the `Schedule` deserialization for `{ "kind": "at", "at": <rfc3339> }`
matches how `Schedule::At` is parsed (the tool does
`serde_json::from_value::<Schedule>` at `cron_add.rs:108`). If the `at` field
expects a different shape, mirror an existing passing `at` construction from the
codebase; if none exists in tests, use the `Schedule` type directly to discover
the field name and STOP if it does not deserialize.

**Verify**: `cargo test --lib tools::cron_add` → all pass including the new
test. Then temporarily restore the old default (`= matches!(schedule, Schedule::At { .. })`
with no `&& delivered`) and re-run: this test must FAIL, proving it pins the
fix. Restore the correct code afterward.

### Step 3: Test — announce-delivery agent one-shot still auto-deletes

Add a second test proving the announce path is unchanged (so we did not
over-correct):

```rust
#[tokio::test]
async fn agent_oneshot_with_announce_delivery_still_auto_deletes() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp).await;
    let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

    let at = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    let result = tool
        .execute(json!({
            "schedule": { "kind": "at", "at": at },
            "job_type": "agent",
            "prompt": "remind me",
            "delivery": { "mode": "announce", "channel": "telegram", "to": "123" }
        }))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);

    let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    let id = v["id"].as_str().unwrap();
    let job = crate::cron::get_job(&cfg, id).unwrap();
    assert!(
        job.delete_after_run,
        "an announce-delivery one-shot should still auto-delete (output already reached the user)"
    );
}
```

Confirm the `delivery` JSON shape matches `DeliveryConfig` deserialization
(`cron_add.rs:216`). If `channel`/`to` field names differ, match the struct.

**Verify**: `cargo test --lib tools::cron_add` → all pass.

### Step 4: (Optional) scheduler-level regression test

Add a test in `src/cron/scheduler.rs` proving the run row survives when
`delete_after_run = false`, i.e. the cascade cannot fire. Model after
`persist_job_result_disables_shell_one_shot_instead_of_refiring`
(`scheduler.rs:1072-1110`), which already builds an `At` job and calls
`persist_job_result`. Add an assertion that `cron::list_runs` returns 1 row
after a successful `persist_job_result` on a kept (disabled) one-shot:

```rust
#[tokio::test]
async fn persist_job_result_keeps_run_history_for_undeleted_one_shot() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp).await;
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = cron::add_agent_job(
        &config,
        Some("one-shot".into()),
        crate::cron::Schedule::At { at },
        "Hello",
        SessionTarget::Isolated,
        None,
        None,
        false, // delete_after_run = false → keep+disable
    )
    .unwrap();
    let started = Utc::now();
    let finished = started + ChronoDuration::milliseconds(10);

    let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
    assert!(success);

    // Job kept but disabled …
    let stored = cron::get_job(&config, &job.id).unwrap();
    assert!(!stored.enabled);
    // … and its run-history row survived (no cascade).
    assert_eq!(cron::list_runs(&config, &job.id, 10).unwrap().len(), 1);
}
```

**Verify**: `cargo test --lib cron::scheduler` → all pass.

### Step 5: Format

**Verify**: `cargo fmt --all -- --check` → exit 0.

## Test plan

- New tests in `src/tools/cron_add.rs`:
  - agent one-shot without delivery ⇒ `delete_after_run == false` (regression:
    was `true` → cascade data loss).
  - agent one-shot with announce delivery ⇒ `delete_after_run == true`
    (unchanged happy path).
- Optional new test in `src/cron/scheduler.rs`: a kept (disabled) one-shot
  retains its `cron_runs` row after `persist_job_result`.
- Structural patterns: `adds_shell_job` (`cron_add.rs:294`) and
  `persist_job_result_disables_shell_one_shot_instead_of_refiring`
  (`scheduler.rs:1073`).
- Verification: `cargo test --lib tools::cron_add` and
  `cargo test --lib cron::scheduler` → all pass; plus the mutation check in
  Step 2.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib tools::cron_add` exits 0; the 2 new tests exist and pass
- [ ] `cargo test --lib cron::scheduler` exits 0
- [ ] `grep -n "matches!(schedule, Schedule::At" src/tools/cron_add.rs` shows
      the `matches!` is now `&& delivered` (or equivalent) — no bare
      one-shot-implies-delete default remains
- [ ] `src/cron/store.rs` is unchanged (`git status`)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `src/tools/cron_add.rs:151-155` or the `delivery` parsing block
  (`cron_add.rs:215-227`) does not match the "Current state" excerpts.
- `DeliveryConfig` has no `mode` field, or announce is detected differently
  than `mode.eq_ignore_ascii_case("announce")` — re-read `scheduler.rs:360-385`
  and match the real announce check.
- The `Schedule::At` JSON shape (`{ "kind": "at", "at": … }`) does not
  deserialize in the test — report the actual expected shape rather than
  guessing.
- A step's verification fails twice after a reasonable fix attempt.

## Maintenance notes

For the human/agent who owns this code after the change lands:

- A reviewer should confirm the default is now `At && announce-delivery`, that an
  explicit `delete_after_run: true|false` in the tool args still overrides, and
  that the shell branch (`add_shell_job`) is untouched.
- **Alternative not taken (documented for the record):** instead of not deleting
  the job, one could detach run rows before the cascade (null the FK or copy to
  an archive table) so history survives even with auto-delete. Rejected here
  because it introduces orphaned run rows with no owning job and needs a pruning
  policy — more surface than the default change, for the same user-visible
  outcome.
- If a future change makes agent one-shots default to announce delivery on the
  web console / TUI (which have no push channel), revisit this default so those
  surfaces still retain run history.
- Related deferred work (other plans in this batch) touches shell
  `delete_after_run` and shell delivery; keep the "delete only when delivered"
  invariant consistent across shell and agent one-shots if those land.
