# Plan 184: Honor `delete_after_run` for shell cron jobs at creation time

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/store.rs src/cron/mod.rs src/cron/scheduler.rs src/tools/cron_add.rs src/gateway/cron_api.rs src/tui/commands/cron.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Cross-ref**: plan 163 (context); plan 191 also changes the `add_shell_job`
  signature — see "Coordination with plan 191" below.
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`add_shell_job` hardcodes the `delete_after_run` column to `0` and does not even
accept the parameter. Yet the `cron_add` tool computes a `delete_after_run` value
(defaulting to `true` for one-shot `At` jobs), advertises it in its JSON schema,
and forwards it on the **agent** branch — but silently drops it on the **shell**
branch. The result is an inconsistency: a shell one-shot created with
`delete_after_run: true` does NOT self-delete after firing (it is only disabled),
while the same request for an agent job does. `update_job` can even set the flag
on a shell job after the fact, and `persist_job_result` honors it at fire time —
so the two paths disagree about what a shell one-shot does. This plan threads
`delete_after_run` through `add_shell_job` so creation-time intent is preserved.

## Current state

- `src/cron/store.rs:30-65` — `add_shell_job`. Signature has no
  `delete_after_run`; the INSERT hardcodes the column to `0`:

  ```rust
  pub fn add_shell_job(
      config: &Config,
      name: Option<String>,
      schedule: Schedule,
      command: &str,
  ) -> Result<CronJob> {
      // ...
      conn.execute(
          "INSERT INTO cron_jobs (
              id, expression, command, schedule, job_type, prompt, name, session_target, model,
              enabled, delivery, delete_after_run, created_at, next_run
           ) VALUES (?1, ?2, ?3, ?4, 'shell', NULL, ?5, 'isolated', NULL, 1, ?6, 0, ?7, ?8)",
          params![
              id, expression, command, schedule_json, name,
              serde_json::to_string(&DeliveryConfig::default())?,
              now.to_rfc3339(), next_run.to_rfc3339(),
          ],
      )
      // ...
  }
  ```

  Compare `add_agent_job` (`src/cron/store.rs:67-111`) which DOES take
  `delete_after_run: bool` and binds `if delete_after_run { 1 } else { 0 }`.

- `src/tools/cron_add.rs:151-155` — the tool computes the value:

  ```rust
  let default_delete_after_run = matches!(schedule, Schedule::At { .. });
  let delete_after_run = args
      .get("delete_after_run")
      .and_then(serde_json::Value::as_bool)
      .unwrap_or(default_delete_after_run);
  ```

  …forwards it on the agent branch (`cron_add.rs:233-243`) but drops it on the
  shell branch (`cron_add.rs:182`): `cron::add_shell_job(&self.config, name,
  schedule, command)`.

- `src/cron/scheduler.rs:294-298` — `persist_job_result` honors the flag at fire
  time:

  ```rust
  if is_one_shot(job) {
      if job.delete_after_run && success {
          if let Err(e) = remove_job(config, &job.id) { /* ... */ }
      } else {
          // disable, keep row for history
      }
  }
  ```

- `src/cron/mod.rs:195-219` — helper doc admits the limitation: *"shell jobs
  ignore it (store limitation)"*. Update that comment (Step 6).

### All `add_shell_job` call sites (every one must be updated for the new arg)

Production callers:
- `src/cron/store.rs:27` — `add_job` (legacy 5-field convenience) → pass `false`.
- `src/cron/mod.rs:217` — `add_scheduled` shell branch → pass the existing
  `delete_after_run` param the function already receives.
- `src/cron/mod.rs:280` — `add_once_at` → pass `false` (CLI shell one-shots stay
  and get disabled after firing; plan 026 behavior — do not change it).
- `src/tools/cron_add.rs:182` — the tool shell branch → pass the computed
  `delete_after_run` (the fix).
- `src/gateway/cron_api.rs:259` — HTTP create shell branch → compute
  `delete_after` the same way the agent branch does (see Step 4) and pass it.
- `src/tui/commands/cron.rs:140` — TUI `/cron add` (only 5-field cron, never
  `At`) → pass `false`.

Test callers (update to the new arity):
- `src/cron/mod.rs:343, 465, 546, 565` — helper/tests → pass `false`.
- `src/cron/scheduler.rs:1078` — `persist_job_result_disables_shell_one_shot_instead_of_refiring`
  → pass `false` (its `assert!(!job.delete_after_run)` still holds).

Repo conventions: `add_agent_job` is the exemplar for the binding style
(`if delete_after_run { 1 } else { 0 }`). It carries
`#[allow(clippy::too_many_arguments)]` because it has 8 params; `add_shell_job`
goes from 4→5 params, well under the threshold, so it needs NO such attribute.

### Coordination with plan 191

Plan 191 also changes the `add_shell_job` signature (it adds a `delivery`
parameter). Whichever plan lands first, the other rebases the signature so both
new parameters coexist. Recommended parameter order to converge on:
`(config, name, schedule, command, delivery, delete_after_run)` — but match
whatever ordering the already-merged plan established. If plan 191 has already
landed, insert `delete_after_run: bool` as the LAST parameter and update call
sites accordingly.

## Commands you will need

| Purpose   | Command                                      | Expected on success       |
|-----------|----------------------------------------------|---------------------------|
| Format    | `cargo fmt --all -- --check`                 | exit 0, no diff           |
| Lint      | `cargo clippy --all-targets -- -D warnings`  | exit 0, no warnings       |
| Tests     | `cargo test --lib cron`                      | all pass, incl. new test  |

`cargo clippy --all-targets` compiles the gateway, TUI, and test targets too, so
it is your primary guard that every call site was updated. Do NOT run a bare
`cargo test` (disk-constrained box).

## Scope

**In scope**:
- `src/cron/store.rs` — add the parameter, bind it in the INSERT, fix the
  `add_job` caller.
- `src/cron/mod.rs` — update the `add_scheduled`, `add_once_at`, and test
  callers; fix the doc comment.
- `src/tools/cron_add.rs` — pass the computed value on the shell branch.
- `src/gateway/cron_api.rs` — compute + pass on the HTTP shell branch.
- `src/tui/commands/cron.rs` — pass `false` on the TUI shell branch.
- `src/cron/scheduler.rs` — update the test caller; add the new self-delete test.

**Out of scope** (do NOT touch):
- `add_agent_job` — already correct.
- `persist_job_result` firing logic — already honors the flag; do not change it.
- The DB schema / migrations — the `delete_after_run` column already exists
  (`src/cron/store.rs:537`); this is a binding change, not a schema change.
- CLI shell one-shot behavior (`add_once_at`) — keep `false` (stays + disabled).

## Git workflow

- Branch: `advisor/184-cron-shell-delete-after-run`
- Conventional commit, e.g.
  `fix(cron): honor delete_after_run when creating a shell job`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the parameter and bind it in `add_shell_job`

In `src/cron/store.rs`, add `delete_after_run: bool` to `add_shell_job`'s
signature (as the last param, unless plan 191 already fixed a different order —
see Coordination). Change the INSERT so the `delete_after_run` column is a bound
parameter, not the literal `0`, and reindex the trailing placeholders:

```rust
pub fn add_shell_job(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    command: &str,
    delete_after_run: bool,
) -> Result<CronJob> {
    // ... unchanged validation / id / schedule_json ...
    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, created_at, next_run
             ) VALUES (?1, ?2, ?3, ?4, 'shell', NULL, ?5, 'isolated', NULL, 1, ?6, ?7, ?8, ?9)",
            params![
                id,
                expression,
                command,
                schedule_json,
                name,
                serde_json::to_string(&DeliveryConfig::default())?,
                if delete_after_run { 1 } else { 0 },
                now.to_rfc3339(),
                next_run.to_rfc3339(),
            ],
        )
        .context("Failed to insert cron shell job")?;
        Ok(())
    })?;

    get_job(config, &id)
}
```

Also update `add_job` in the same file (line 27) to pass `false`:
`add_shell_job(config, None, schedule, command, false)`.

**Verify**: `cargo build --lib` compiles `store.rs` (other call sites will still
fail to compile until later steps — that is expected; continue).

### Step 2: Update the `mod.rs` callers

In `src/cron/mod.rs`:
- `add_scheduled` shell branch (line 217): `add_shell_job(config, None, schedule,
  payload, delete_after_run)` — pass the function's own `delete_after_run`
  parameter (already threaded in from the callers; for CLI shell paths this is
  `false`, preserving behavior).
- `add_once_at` (line 280): `add_shell_job(config, None, schedule, command,
  false)`.
- Test callers at lines 343, 465, 546, 565: add a trailing `, false` argument.

**Verify**: (deferred to Step 5's full clippy run.)

### Step 3: Pass the computed value on the tool's shell branch

In `src/tools/cron_add.rs:182`, change:

```rust
cron::add_shell_job(&self.config, name, schedule, command)
```

to:

```rust
cron::add_shell_job(&self.config, name, schedule, command, delete_after_run)
```

`delete_after_run` is already in scope (computed at `cron_add.rs:151-155`).

### Step 4: Compute + pass on the gateway HTTP shell branch

In `src/gateway/cron_api.rs`, the shell branch (around line 243-263) does not
currently compute a delete-after value. Mirror the agent branch
(`cron_api.rs:218-220`):

```rust
        JobType::Shell => {
            let command = /* ... unchanged ... */;
            // ... unchanged security check ...
            let delete_after = body
                .delete_after_run
                .unwrap_or(matches!(body.schedule, Schedule::At { .. }));
            let (name, schedule) = (body.name.clone(), body.schedule.clone());
            tokio::task::spawn_blocking(move || {
                cron::add_shell_job(&cfg, name, schedule, &command, delete_after)
            })
            .await
            .map_err(err_500)?
            .map_err(err_400)?
        }
```

`CreateCronBody` already has `delete_after_run: Option<bool>` and `schedule:
Schedule` (it is used identically in the agent branch), so no struct change is
needed. Confirm the field name by reading the `CreateCronBody` struct in the same
file before editing.

### Step 5: Pass `false` on the TUI shell branch

In `src/tui/commands/cron.rs:140`:
`cron::add_shell_job(config, None, schedule, &payload, false)` — TUI `/cron add`
only builds 5-field `Schedule::Cron`, never `At`, so `false` is correct.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0 (this confirms
EVERY call site was updated).

### Step 6: Fix the stale doc comment

In `src/cron/mod.rs:195-196`, the `add_scheduled` doc says shell jobs ignore
`delete_after_run` "(store limitation)". Update it to reflect that the limitation
is gone, e.g.:

```rust
/// Create a shell or agent job from a resolved schedule. `delete_after_run`
/// controls whether a one-shot (`At`) self-deletes on success; it is honored
/// for both shell and agent jobs.
```

### Step 7: Add the self-delete regression test

In `src/cron/scheduler.rs::tests` (after
`persist_job_result_disables_shell_one_shot_instead_of_refiring`, ~line 1110),
add a test that a shell one-shot created with `delete_after_run = true`
self-deletes after a successful fire. Model it on the existing
`persist_job_result_success_deletes_one_shot` (lines 981-1004, which uses an
agent job) but build a shell job:

```rust
    #[tokio::test]
    async fn persist_job_result_deletes_shell_one_shot_when_flagged() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_shell_job(
            &config,
            Some("one-shot-shell".into()),
            crate::cron::Schedule::At { at },
            "echo hi",
            true, // delete_after_run
        )
        .unwrap();
        assert!(job.delete_after_run, "shell one-shot must carry the flag now");

        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);
        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success);

        // It opted into auto-delete → the row must be gone.
        assert!(
            cron::get_job(&config, &job.id).is_err(),
            "a flagged shell one-shot must self-delete after a successful run"
        );
    }
```

Also update the existing 4-arg `add_shell_job` call in
`persist_job_result_disables_shell_one_shot_instead_of_refiring` (line 1078) to
pass `false` as the new fifth argument.

**Verify**: `cargo test --lib cron` → all pass, including the new test.

### Step 8: Mutation check

Temporarily revert Step 1's binding back to the literal `0` (so the flag is
ignored) and run
`cargo test --lib cron persist_job_result_deletes_shell_one_shot_when_flagged`.
The test MUST fail (the job is disabled, not deleted). Restore the binding and
confirm it passes.

**Verify**: with the `0` literal the new test fails; with the bound param it
passes.

## Test plan

- New test in `src/cron/scheduler.rs::tests`:
  `persist_job_result_deletes_shell_one_shot_when_flagged` — a shell one-shot
  created with `delete_after_run: true` self-deletes on a successful fire.
- Structural pattern: `persist_job_result_success_deletes_one_shot` (agent
  variant) in the same module.
- Verification: `cargo test --lib cron` → all pass; `cargo clippy --all-targets`
  proves every call site compiles.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0 (every call site updated)
- [ ] `cargo test --lib cron` exits 0; the new test exists and passes
- [ ] `grep -n "delete_after_run, created_at, next_run" src/cron/store.rs` shows
      the shell INSERT no longer uses a hardcoded `0` for that column (it binds a
      placeholder)
- [ ] With the `0` literal restored the new test fails (Step 8 mutation check)
- [ ] Only the in-scope files are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `add_shell_job`'s signature already includes `delete_after_run` (plan 184 was
  already applied) — the codebase has drifted; nothing to do.
- Plan 191 landed first and changed `add_shell_job`'s parameter list in a way the
  excerpts do not match — re-read the current signature, insert
  `delete_after_run: bool` consistently, and update call sites, OR STOP if the
  merge is ambiguous.
- `CreateCronBody` in `src/gateway/cron_api.rs` lacks a `delete_after_run` field
  (the agent branch would not compile as excerpted) — re-read the struct and
  adapt, or STOP.
- A step's verification fails twice after a reasonable fix attempt.

## Maintenance notes

- After this lands, shell and agent one-shots behave identically w.r.t.
  `delete_after_run` at both creation (`add_*_job`) and fire time
  (`persist_job_result`). A reviewer should confirm the HTTP and tool paths pass
  the computed value while the CLI/TUI 5-field-cron paths pass `false`.
- Follow-up explicitly deferred: none. If plan 191's `delivery` parameter lands
  in the same window, reconcile the signature order in one of the two PRs and
  note it in the PR description.
