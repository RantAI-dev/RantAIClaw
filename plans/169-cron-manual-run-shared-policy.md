# Plan 169: Thread the caller's SecurityPolicy through the manual cron-run path

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/tools/cron_run.rs src/gateway/cron_api.rs src/tui/app.rs src/cron/mod.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none (touches the same functions as `plans/166`; if 166 has landed, `run_job_manual` already has an `InFlightGuard` claim — keep it and add the `security` parameter alongside)
- **Category**: bug / security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`execute_job_now` builds a **brand-new** `SecurityPolicy` from config on every
invocation (`scheduler.rs:70`), and `run_job_manual` routes through it. Because
policy *process state* lives on the instance (not in config), a fresh instance
means:

1. **Rate limits are unenforceable on the manual path.** The rate-limit window
   and action budget live per-instance on `SecurityPolicy`; a fresh instance
   starts with a fresh window, so a caller that loops a force-run gets a new
   budget each time and can run past `max_actions_per_hour`. This bites hardest
   on the gateway and CLI paths, which do NOT re-check the rate limit before
   calling `run_job_manual`.
2. **Runtime `/allow` grants are invisible.** Commands an operator granted at
   runtime via `/allow` (`add_runtime_command`) live in the long-lived policy's
   `runtime_allowlist`; a fresh instance built from config has an empty runtime
   allowlist, so a job the operator just granted is still refused on the manual
   path.

Commit `7457e9f` ("make the whole autonomy section refresh on a running
policy") documents this split explicitly: *"the rate-limit window,
`/allow`-granted commands, and the approval registry stay on `SecurityPolicy`
and survive a refresh."* The scheduled loop already holds a long-lived
`Arc<SecurityPolicy>` (`scheduler.rs:25`) and threads it through
`execute_and_persist_job`. Only the **manual** path fabricates a throwaway one.

After this plan, `execute_job_now`/`run_job_manual` accept a `&SecurityPolicy`
and each caller passes the instance it holds, so runtime grants and the shared
budget apply to force-runs the same way they apply everywhere else.

## Current state

- `src/cron/scheduler.rs` — `execute_job_now` fabricates the policy; only the
  manual path does this (the scheduled path threads its long-lived `Arc`).
- The four production callers of `run_job_manual`.

`execute_job_now` builds a fresh policy every call (`scheduler.rs:69-72`):

```rust
pub async fn execute_job_now(config: &Config, job: &CronJob) -> (bool, String) {
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
    execute_job_with_retry(config, &security, job).await
}
```

`run_job_manual` calls it (`scheduler.rs:79-96`, `execute_job_now` at line 81):

```rust
pub async fn run_job_manual(config: &Config, job: &CronJob) -> (bool, String) {
    let started_at = Utc::now();
    let (success, output) = execute_job_now(config, job).await;
    ...
}
```

The scheduled path is already correct — it does NOT use `execute_job_now`; it
calls `execute_job_with_retry` directly with its long-lived `security`
(`scheduler.rs:186-201`):

```rust
async fn execute_and_persist_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    component: &str,
) -> (String, bool) {
    ...
    let (success, output) = execute_job_with_retry(config, security, job).await;
    ...
}
```

The four production callers of `run_job_manual` (confirmed by
`grep -rn run_job_manual src/`):

1. **`cron_run` tool** — holds `self.security: Arc<SecurityPolicy>`
   (`src/tools/cron_run.rs:9-12`, call at `:114`):

```rust
pub struct CronRunTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}
```
```rust
        let (success, output) = cron::scheduler::run_job_manual(&self.config, &job).await;
```

2. **Gateway `POST /cron/{id}/run`** — builds a `SecurityPolicy` locally for the
   gate at `src/gateway/cron_api.rs:374`, then calls `run_job_manual` at `:386`:

```rust
    let security = SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir);
    if !security.can_act() { ... }
    if matches!(job.job_type, JobType::Shell) {
        if let Err(reason) = security.validate_command_execution(&job.command, q.approved) { ... }
    }

    let (success, output) = cron::scheduler::run_job_manual(&cfg, &job).await;
```

   **IMPORTANT drift note vs the original finding.** The finding said "gateway
   has AppState [holding the policy]". It does NOT: `AppState`
   (`src/gateway/mod.rs:429-478`) has no `SecurityPolicy` field. The gateway's
   long-lived policy is captured *inside the `build_tools_factory` closure*
   (`src/gateway/mod.rs:500-518`) and is unreachable from `run_cron`. So the
   instance to pass here is the one already built at `cron_api.rs:374` (used for
   the gate). This is still a per-request instance, so it makes the job
   execution use the *same* policy the gate used (consistency) but does not
   share a rate-limit window across requests. Fully fixing the gateway's
   cross-request budget would require adding a shared policy to `AppState` — a
   larger change deferred to "Maintenance notes". Do NOT expand this plan to do
   that.

3. **TUI cron panel "run"** — detached spawn, no policy in scope today
   (`src/tui/app.rs:3496-3499`):

```rust
                let cfg = config.clone();
                tokio::spawn(async move {
                    let _ = crate::cron::scheduler::run_job_manual(&cfg, &job).await;
                });
```

4. **CLI `run_job_report`** — one-shot process, no policy in scope today
   (`src/cron/mod.rs:238-247`):

```rust
async fn run_job_report(config: &Config, id: &str) -> Result<String> {
    let job = get_job(config, id)?;
    let (ok, output) = crate::cron::scheduler::run_job_manual(config, &job).await;
    ...
}
```

The runtime `/allow` grant API and the fact the allowlist is shared across
clones (`src/security/policy.rs:1056-1082`, test at `:2421`):

```rust
    /// Add a basename to the runtime allowlist.
    pub fn add_runtime_command(&self, basename: &str, persist: bool) -> anyhow::Result<()> {
```

## Commands you will need

| Purpose      | Command                                             | Expected on success        |
|--------------|-----------------------------------------------------|----------------------------|
| Format check | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint         | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests        | `cargo test --lib cron`                             | all pass (incl. new tests) |
| Drift        | `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/tools/cron_run.rs src/gateway/cron_api.rs src/tui/app.rs src/cron/mod.rs` | empty before you start |

Do NOT run a bare `cargo test`.

## Scope

**In scope**:
- `src/cron/scheduler.rs` (signatures of `execute_job_now` + `run_job_manual`,
  test call sites)
- `src/tools/cron_run.rs` (pass `&self.security`; new regression test)
- `src/gateway/cron_api.rs` (pass the instance built at line 374)
- `src/tui/app.rs` (construct one policy from config before the spawn, move it in)
- `src/cron/mod.rs` (construct one policy from config in `run_job_report`)

**Out of scope** (do NOT touch):
- Adding a shared `SecurityPolicy` to `AppState` — deferred (see Maintenance).
- The in-flight overlap guard — that is `plans/166`. If 166 has landed, leave its
  `InFlightGuard::claim` in `run_job_manual` in place and simply add the
  `security` parameter alongside it.
- Any change to `execute_job_with_retry`, `run_job_command`, or `run_agent_job`
  signatures — they already take `&SecurityPolicy`.

## Git workflow

- Branch: `advisor/169-cron-manual-run-shared-policy`
- Conventional commits, e.g. `fix(cron): thread the caller's SecurityPolicy through manual runs`.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Change the two scheduler signatures to accept `&SecurityPolicy`

In `scheduler.rs`:

```rust
pub async fn execute_job_now(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    execute_job_with_retry(config, security, job).await
}
```

```rust
pub async fn run_job_manual(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    // (If plan 166 landed, its `InFlightGuard::claim(&job.id)` early-return
    //  stays here, unchanged.)
    let started_at = Utc::now();
    let (success, output) = execute_job_now(config, security, job).await;
    ...
}
```

Remove the `SecurityPolicy::from_config(...)` line that was inside
`execute_job_now`. The `use crate::security::SecurityPolicy;` import stays
(still referenced by other functions).

Update the in-file test call site `run_job_manual(&config, &job)` at
`scheduler.rs:1039` to build a policy and pass it, e.g.:

```rust
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (ok, _) = run_job_manual(&config, &security, &job).await;
```

**Verify**: `cargo test --lib cron` (will fail to compile until callers are
updated — expected; proceed to Step 2). Do not treat a compile error naming the
other call sites as a STOP condition yet.

### Step 2: Update the `cron_run` tool to pass `self.security`

`src/tools/cron_run.rs:114`:

```rust
        let (success, output) =
            cron::scheduler::run_job_manual(&self.config, &self.security, &job).await;
```

`&self.security` is `&Arc<SecurityPolicy>`; deref-coerces to `&SecurityPolicy`.
If the compiler complains, write `self.security.as_ref()`.

This path benefits fully: `self.security` is the agent's long-lived policy, so
runtime `/allow` grants and the shared budget now reach the job execution.

**Verify**: after Step 5, `cargo test --lib cron` passes.

### Step 3: Update the gateway handler to pass its line-374 instance

`src/gateway/cron_api.rs:386` — pass the `security` already in scope:

```rust
    let (success, output) = cron::scheduler::run_job_manual(&cfg, &security, &job).await;
```

(No new construction — reuse the `security` built at `cron_api.rs:374` for the
gate.)

### Step 4: Update the TUI and CLI callers to construct one policy and pass it

TUI (`src/tui/app.rs:3496-3499`) — build the policy from the already-loaded
`config` before the spawn and move it into the task:

```rust
                let cfg = config.clone();
                let security = std::sync::Arc::new(
                    crate::security::SecurityPolicy::from_config(
                        &cfg.autonomy,
                        &cfg.workspace_dir,
                    ),
                );
                tokio::spawn(async move {
                    let _ = crate::cron::scheduler::run_job_manual(&cfg, &security, &job).await;
                });
```

(Use the crate path already used elsewhere in `app.rs`; if `SecurityPolicy` is
already imported there, drop the full path. Confirm the import with
`grep -n "SecurityPolicy" src/tui/app.rs`.)

CLI (`src/cron/mod.rs:238-247`) — construct once and pass:

```rust
async fn run_job_report(config: &Config, id: &str) -> Result<String> {
    let job = get_job(config, id)?;
    let security =
        crate::security::SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
    let (ok, output) = crate::cron::scheduler::run_job_manual(config, &security, &job).await;
    ...
}
```

### Step 5: Add a regression test proving `/allow` grants reach manual runs

Add a `#[tokio::test]` to the `tests` module in `src/tools/cron_run.rs` (the tool
path is the one that fully realizes the fix, since it passes a long-lived
`self.security`). The test:

1. Build a `Config` with `autonomy.allowed_commands` EMPTY (so the boot allowlist
   does not include the command) and `autonomy.level = Supervised`.
2. Build `security = Arc::new(SecurityPolicy::from_config(...))` and grant a real
   low-risk command at runtime: `security.add_runtime_command("true", false).unwrap();`
   (`true` is a real binary, low-risk, no path args, so only the allowlist gates it.)
3. Create a shell cron job with command `"true"`
   (`cron::add_job(&cfg, "*/5 * * * *", "true")`).
4. `CronRunTool::new(cfg, security)` and `execute(json!({ "job_id": job.id }))`.
5. Assert `result.success` is `true` and the output/status is not
   "command not allowed".

This fails on pre-fix code (the throwaway policy has an empty runtime allowlist,
so `true` is blocked) and passes after (the granted policy is threaded through).
Model the setup after `force_runs_job_and_records_history`
(`src/tools/cron_run.rs:159-171`).

Note: the `cron_run` tool gate itself calls `record_action` on `self.security`
(`cron_run.rs:103`) before `run_job_manual`, and the shell job execution calls
`record_action` again on the now-shared policy — so with the fix a force-run
decrements the shared budget twice (gate + execution) instead of once on two
separate instances. That is the correct, enforceable behavior; keep the test
focused on the `/allow`-grant assertion (crisp A/B) rather than exact budget
arithmetic.

**Verify**: `cargo test --lib cron` → all pass, including the new test.

## Test plan

- New test in `src/tools/cron_run.rs` `mod tests`: a runtime `/allow` grant on
  the tool's `self.security` makes a shell job using that command succeed on the
  manual path (would be blocked with the old throwaway policy).
- Existing tests that must stay green (update only their call signatures where
  they call `run_job_manual`):
  - `run_job_manual_records_without_rescheduling` (`src/cron/scheduler.rs:1032`)
  - all `src/tools/cron_run.rs` tests (`force_runs_job_and_records_history`, the
    approval-gate tests) — they go through `run_job_manual` transitively via the
    tool; ensure they still compile/pass.
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the new `/allow`-reaches-manual-run test exists and passes
- [ ] `execute_job_now` no longer constructs a policy — no `SecurityPolicy::from_config` INSIDE `execute_job_now` (the line-25 hit in `run()` is the scheduled loop's long-lived policy and is expected to remain; `#[cfg(test)]` setup hits are also expected). Confirm with `grep -n "SecurityPolicy::from_config" src/cron/scheduler.rs` and check none of the hits fall within `execute_job_now`.
- [ ] `run_job_manual` and `execute_job_now` both take a `security: &SecurityPolicy` parameter (`grep -n "fn run_job_manual\|fn execute_job_now" src/cron/scheduler.rs`)
- [ ] All four production callers pass a `security` argument (`grep -rn "run_job_manual(" src/tools/cron_run.rs src/gateway/cron_api.rs src/tui/app.rs src/cron/mod.rs` each show a three-argument call)
- [ ] Only the five in-scope files are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check is non-empty (code moved since this plan was written).
- `AppState` has grown a `SecurityPolicy` field since this plan was written
  (someone did the deferred change) — then reconsider whether the gateway should
  pass *that* instead of the line-374 one; report before deciding.
- `&self.security` will not deref-coerce and `self.security.as_ref()` also fails
  to type-check — report the exact error rather than restructuring the tool.
- A test failure would require changing what a test *asserts about behavior*
  (as opposed to adding the new `security` argument).
- Plan 166 has landed and its `InFlightGuard` early-return in `run_job_manual`
  conflicts with adding the `security` parameter in a way you cannot reconcile —
  report the conflict.

## Maintenance notes

For the human/agent who owns this after the change lands:

- **Deferred: gateway cross-request budget.** The gateway still builds a
  per-request `SecurityPolicy` (`cron_api.rs:374`), so `max_actions_per_hour` is
  not shared across gateway force-run requests. A proper fix adds a long-lived
  `Arc<SecurityPolicy>` to `AppState` (built once, refreshed via `apply_config`
  like `build_tools_factory` does at `mod.rs:500-518`) and passes it here. That
  is a broader gateway change, intentionally out of scope; note it in the PR.
- Same limitation applies to the detached TUI spawn (constructs its own policy)
  and the one-shot CLI process (a fresh process each invocation) — both are
  acceptable because those surfaces do not maintain a long-lived rate-limit
  window the way the agent tool loop does; the fully-realized fix is the
  `cron_run` tool path, which is what the regression test pins.
- Reviewer should scrutinize: the `cron_run` tool now passes its long-lived
  `self.security`, so a job's execution shares the operator's `/allow` grants and
  budget — confirm no double-gate regression (the tool gate + the execution gate
  are both intentional).
