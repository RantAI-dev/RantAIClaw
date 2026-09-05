# Plan 191: Thread `delivery` into shell cron jobs so "run this and message me" announces

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/store.rs src/tools/cron_add.rs`
> If either file changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition. In particular, if `add_shell_job` already has a
> `delete_after_run` parameter, plan 184 landed first — see Step 1's note.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none (coordinates with plans/184-*.md — see Step 1)
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

The origin-chat safety net in the agent loop injects a `delivery` block into
**any** `cron_add` call from an announce-capable chat channel — it is gated only
on the tool name, the channel, and the absence of an existing delivery; it does
**not** check the job type (`src/agent/loop_.rs:1058–1094`). But the `cron_add`
tool reads `delivery` only in its `JobType::Agent` branch
(`src/tools/cron_add.rs:215–227`); the shell branch calls
`cron::add_shell_job(...)` (line 182), which hardcodes
`DeliveryConfig::default()` (mode `"none"`) in the INSERT
(`src/cron/store.rs:55`).

So when a user says "every morning run this check and message me" and the model
answers by picking `command` (→ a shell job), the announce is **silently dropped
at creation** — even though the scheduler's delivery is job-type agnostic
(`deliver_if_configured` fires for every job) and `cron_update` *can* set
delivery on a shell job afterwards (`src/cron/store.rs:207`). After this plan,
`add_shell_job` accepts an optional `delivery`, the `cron_add` shell branch
threads it through, and a shell job created with a delivery persists it.

## Current state

- `src/tools/cron_add.rs`
  - `DeliveryConfig` is already imported: line 3,
    `use crate::cron::{self, DeliveryConfig, JobType, Schedule, SessionTarget};`.
  - Shell branch (lines 158–183) — parses `command`, validates it, enforces the
    mutation gate, then calls `cron::add_shell_job(&self.config, name, schedule, command)`
    (line 182). It **never reads `delivery`**.
  - Agent branch delivery parse (lines 215–227), the pattern to mirror:
    ```rust
    let delivery = match args.get("delivery") {
        Some(v) => match serde_json::from_value::<DeliveryConfig>(v.clone()) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid delivery config: {e}")),
                });
            }
        },
        None => None,
    };
    ```

- `src/cron/store.rs`
  - `add_shell_job` (lines 30–65):
    ```rust
    pub fn add_shell_job(
        config: &Config,
        name: Option<String>,
        schedule: Schedule,
        command: &str,
    ) -> Result<CronJob> {
        // ...
        with_connection(config, |conn| {
            conn.execute(
                "INSERT INTO cron_jobs (
                    id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run
                 ) VALUES (?1, ?2, ?3, ?4, 'shell', NULL, ?5, 'isolated', NULL, 1, ?6, 0, ?7, ?8)",
                params![
                    id,
                    expression,
                    command,
                    schedule_json,
                    name,
                    serde_json::to_string(&DeliveryConfig::default())?,   // ?6 — the hardcoded default
                    now.to_rfc3339(),
                    next_run.to_rfc3339(),
                ],
            )
            // ...
        })?;
        get_job(config, &id)
    }
    ```
  - `add_agent_job` (lines 67–111) is the model: it takes
    `delivery: Option<DeliveryConfig>` and does `let delivery = delivery.unwrap_or_default();`
    then binds `serde_json::to_string(&delivery)?`.
  - `update_job` already applies a `delivery` patch to any job type (lines
    207–209, 241) — proof the storage/scheduler side is already job-type
    agnostic.

- **All current `add_shell_job` call sites** (every one must be updated in
  Step 3 — the compiler will list them once the signature changes):
  - `src/cron/store.rs:27` — `add_job` wrapper → pass `None`
  - `src/cron/mod.rs:217`, `:280`, `:343`, `:465`, `:546`, `:565` → pass `None`
  - `src/cron/scheduler.rs:1078` (a test) → pass `None`
  - `src/gateway/cron_api.rs:259` → pass `None`
  - `src/tui/commands/cron.rs:140` → pass `None`
  - `src/tools/cron_add.rs:182` → pass the **real** parsed `delivery` (Step 2)

Conventions: mirror `add_agent_job`'s `delivery: Option<DeliveryConfig>` +
`unwrap_or_default()` pattern exactly. Duplicate the small delivery-parse block
into the shell branch rather than abstracting (rule-of-three; only two copies).

## Commands you will need

| Purpose   | Command                                             | Expected on success       |
|-----------|-----------------------------------------------------|---------------------------|
| Format    | `cargo fmt --all -- --check`                        | exit 0, no diff           |
| Lint      | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings       |
| Build+test| `cargo test --lib cron`                             | compiles whole lib; cron tests pass |
| Drift     | `git diff --stat 2aefb9f..HEAD -- src/cron/store.rs src/tools/cron_add.rs` | (see drift check) |

`cargo test --lib cron` compiles the **entire** lib crate (so it flags every
broken `add_shell_job` caller in gateway/tui/tools) but runs only the cron
tests. Do **not** run a bare `cargo test`.

## Scope

**In scope** (the only files you should modify):

- `src/cron/store.rs` — `add_shell_job` signature + INSERT bind; test module.
- `src/tools/cron_add.rs` — shell branch: parse `delivery`, pass it through.
- Every current `add_shell_job` caller listed above (add a trailing `None`).

**Out of scope** (do NOT touch):

- `src/agent/loop_.rs` safety net — it is already job-type agnostic and correct.
- `add_agent_job` — leave it exactly as is.
- `deliver_if_configured` / the scheduler delivery path — already job-type
  agnostic (see plan 168 for its separate fixes).
- `DeliveryConfig` struct / DB schema — the `delivery` column already exists
  (`add_column_if_missing(&conn, "delivery", "TEXT")`, store.rs:569).

## Git workflow

- Branch: `advisor/191-cron-shell-job-delivery`
- Conventional commits, e.g.
  `fix(cron): persist delivery on shell jobs created via cron_add`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a `delivery` parameter to `add_shell_job`

In `src/cron/store.rs`, change `add_shell_job` to accept
`delivery: Option<DeliveryConfig>` as its **final** parameter:

```rust
pub fn add_shell_job(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    command: &str,
    delivery: Option<DeliveryConfig>,
) -> Result<CronJob> {
```

Inside the function, before `with_connection`, add:

```rust
let delivery = delivery.unwrap_or_default();
```

and change the `?6` bind from `serde_json::to_string(&DeliveryConfig::default())?`
to `serde_json::to_string(&delivery)?`. Leave the SQL string and all other binds
unchanged.

> **Coordination with plan 184**: If the drift check shows `add_shell_job`
> already has a `delete_after_run: bool` parameter (plan 184 landed first),
> place the new `delivery: Option<DeliveryConfig>` parameter **before**
> `delete_after_run`, mirroring `add_agent_job`'s parameter order
> (`delivery`, then `delete_after_run`). The two columns are independent; do not
> otherwise change plan 184's work.

**Verify**: `cargo fmt --all -- --check` → exit 0. (The build will not yet
succeed — callers are fixed in Steps 2–3.)

### Step 2: Parse and thread `delivery` in the `cron_add` shell branch

In `src/tools/cron_add.rs`, inside the `JobType::Shell` branch (after the
`validate_command_execution` check and before `enforce_mutation_allowed`, i.e.
between current lines 176 and 178), add a delivery parse identical to the agent
branch's:

```rust
let delivery = match args.get("delivery") {
    Some(v) => match serde_json::from_value::<DeliveryConfig>(v.clone()) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Invalid delivery config: {e}")),
            });
        }
    },
    None => None,
};
```

Then change the call at line 182 to pass it:

```rust
cron::add_shell_job(&self.config, name, schedule, command, delivery)
```

(If plan 184 landed first and `add_shell_job` also takes `delete_after_run`,
pass `delivery` then `delete_after_run` to match the new signature.)

**Verify**: `cargo fmt --all -- --check` → exit 0.

### Step 3: Update every other `add_shell_job` caller to pass `None`

Run `cargo build --lib` (or `cargo test --lib cron`) and add a trailing `None`
argument to every reported `add_shell_job(...)` call **except** the one you just
edited in `src/tools/cron_add.rs`. The known sites are listed in "Current state";
the compiler is the source of truth. None of these callers has a delivery to
pass, so `None` preserves today's behavior (mode `"none"`).

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0 (compiles, no
warnings).

### Step 4: Add tests

In `src/cron/store.rs`'s `#[cfg(test)] mod tests` block, add:

1. `add_shell_job_persists_delivery` — create a shell job with an announce
   delivery and assert it round-trips:
   ```rust
   let job = add_shell_job(
       &config,
       Some("morning-check".into()),
       Schedule::Cron { expr: "0 9 * * *".into(), tz: None },
       "echo ok",
       Some(DeliveryConfig {
           mode: "announce".into(),
           channel: Some("telegram".into()),
           to: Some("42".into()),
           best_effort: true,
       }),
   ).unwrap();
   assert_eq!(job.delivery.mode, "announce");
   assert_eq!(job.delivery.channel.as_deref(), Some("telegram"));
   assert_eq!(job.delivery.to.as_deref(), Some("42"));
   ```
   (Use the existing test-module setup for building `config`; model the
   overall shape after a nearby `add_*_job` test in the same block.)

2. `add_shell_job_without_delivery_defaults_to_none` — regression guard that the
   `None` path still stores mode `"none"`:
   ```rust
   let job = add_shell_job(&config, None,
       Schedule::Cron { expr: "0 9 * * *".into(), tz: None }, "echo ok", None).unwrap();
   assert_eq!(job.delivery.mode, "none");
   ```

**Verify**: `cargo test --lib cron` → all pass, including the 2 new tests.

## Test plan

- New tests in `src/cron/store.rs` `mod tests`:
  - `add_shell_job_persists_delivery` — the fix: an announce delivery survives
    creation (previously impossible for a shell job).
  - `add_shell_job_without_delivery_defaults_to_none` — regression: the `None`
    path is unchanged.
- The end-to-end "announces on fire" behaviour is exercised by the scheduler's
  existing `deliver_if_configured` (already job-type agnostic) once the delivery
  is persisted; a full channel send is not unit-testable here without a live
  channel, so the persistence tests are the tractable proof.
- Verification: `cargo test --lib cron` → all pass, including 2 new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the 2 new tests exist and pass
- [ ] `grep -n "DeliveryConfig::default()" src/cron/store.rs` no longer matches
      inside `add_shell_job` (the INSERT now binds the passed `delivery`)
- [ ] Every `add_shell_job` call compiles (whole-lib build is green)
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `add_shell_job` or the `cron_add` shell branch don't match the "Current
  state" excerpts (drift since this plan was written), beyond the documented
  plan-184 `delete_after_run` case.
- Adding the parameter reveals an `add_shell_job` caller outside the listed
  set that has a real delivery to pass — report it rather than guessing.
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- **Shared signature coordination**: `add_shell_job`'s signature is now changed
  by both this plan (adds `delivery`) and plan 184 (adds `delete_after_run`).
  Whichever lands first, the other rebases the signature and re-fixes all
  callers. Keep the parameter order aligned with `add_agent_job`:
  `(config, name, schedule, command/prompt…, delivery, delete_after_run)`.
- A reviewer should confirm the `None` was added to every non-`cron_add` caller
  (a missed one is a build error, so this is compiler-enforced) and that the
  shell-branch delivery parse matches the agent branch's error handling.
- Deferred: unifying the two identical delivery-parse blocks in `cron_add.rs`
  into one helper is a rule-of-three call — leave it at two copies for now.
