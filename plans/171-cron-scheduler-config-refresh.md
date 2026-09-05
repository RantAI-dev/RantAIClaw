# Plan 171: Apply the scheduler's per-tick config reload to scheduler/cron/channel fields, not just autonomy

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs src/cron/store.rs src/gateway/config_api.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none (coordinate with plans 168/194 on stale channels; see notes)
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

The scheduler loop reloads config every poll tick but throws almost all of it
away. `src/cron/scheduler.rs:45-54` calls `Config::load_or_init().await`, applies
**only** `security.apply_config(&cfg.autonomy)`, and drops `cfg`. The
`config: Config` value moved into `run()` at line 21 is never reassigned, so the
**boot-time snapshot** still supplies everything else the tick uses:

- `scheduler.max_tasks` — via `due_jobs(&config, …)` (`store.rs:163`)
- `scheduler.max_concurrent` — `process_due_jobs` (`scheduler.rs:149`)
- `reliability.scheduler_retries` / `provider_backoff_ms` (`scheduler.rs:104-105`)
- `cron.max_run_history` — `record_run` prune (`store.rs:334`)
- the channels config used for delivery — `build_configured_channels(config)` (`scheduler.rs:387`)

Two concrete costs:

**(a) Edits don't take effect.** An operator who changes `scheduler.max_concurrent`,
`cron.max_run_history`, or a rotated bot token / added delivery channel through the
config API sees it saved, and nothing changes until the daemon restarts — with no
"requires restart" signal anywhere (`rg restart_required src/gateway/config_api.rs`
finds only Telegram-specific reconnect logic). This is exactly the class that
commit `7457e9f` fixed for `[autonomy]` but left open for `[scheduler]`/`[cron]`/channels.

**(b) Heavyweight entry point on a timer.** `Config::load_or_init` (`schema.rs:3918`)
runs legacy-layout migrations, `create_dir_all`, TOML parse, credential-strip, and a
write-back-if-migrated — on a ~15s timer. **Note: the loop already pays this cost
today** (line 45 already calls `load_or_init` every tick for the autonomy reload).
So threading the already-loaded `cfg` into the rest of the tick adds *zero* extra
load cost — it just stops discarding a value already computed.

The per-tick autonomy reload cost itself is an **accepted tradeoff** (`7457e9f`) —
do not relitigate it. The finding is the **partial application** and the
**heavyweight entry point**, not the existence of the reload.

## Current state

### `src/cron/scheduler.rs` — `run()` (lines 21–67)

```rust
pub async fn run(config: Config) -> Result<()> {                       // 21  config = boot snapshot
    let poll_secs = config.reliability.scheduler_poll_secs.max(MIN_POLL_SECONDS); // 22
    let mut interval = time::interval(Duration::from_secs(poll_secs)); // 23
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)); // 25-28
    let in_flight = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    crate::health::mark_component_ok(SCHEDULER_COMPONENT);

    loop {
        interval.tick().await;
        crate::health::mark_component_ok(SCHEDULER_COMPONENT);

        // Refresh the config half once per poll tick. […accepted per-tick reload…]
        match Config::load_or_init().await {                           // 45
            Ok(cfg) => security.apply_config(&cfg.autonomy),           // 46  ← ONLY autonomy applied; cfg dropped
            Err(e) => tracing::warn!(target: "scheduler", error = %e,
                "config reload failed; keeping the previously applied autonomy settings"),
        }

        let jobs = match due_jobs(&config, Utc::now()) {               // 56  ← stale boot config
            Ok(jobs) => jobs,
            Err(e) => { /* mark error; continue */ }
        };

        process_due_jobs(&config, &security, jobs, SCHEDULER_COMPONENT, &in_flight).await; // 65  ← stale
    }
}
```

Downstream readers of the config passed in:
- `process_due_jobs` line 149: `let max_concurrent = config.scheduler.max_concurrent.max(1);`
- `execute_job_with_retry` lines 104-105: `config.reliability.scheduler_retries`, `config.reliability.provider_backoff_ms`
- `deliver_if_configured` line 387: `crate::channels::build_configured_channels(config)`
- `store::due_jobs` line 163: `config.scheduler.max_tasks`
- `store::record_run` line 334: `config.cron.max_run_history`

### `src/config/schema.rs` — only a heavyweight loader exists

`pub async fn load_or_init() -> Result<Self>` at line 3918 (migrations,
`create_dir_all`, parse, credential strip, write-back). There is **no** lightweight
`Config::load` / `reload_from_disk` today — `load_or_init` (3918) and `save` (4478)
are the only entry points.

### `src/gateway/config_api.rs` — no restart signal

`persist_and_swap` (line 253) saves the config and swaps it into `state.config`.
Only Telegram writes emit a runtime-restart note (`needs_runtime_restart`, line 585;
`runtime_restart_note`, line 595). Scheduler/cron/channel edits return no
"requires restart" hint.

## The two options (present both; default = Option A)

This plan documents both fixes the finding calls for. **Default to Option A** —
it is near-zero-cost (the reload already runs) and consistent with how `[autonomy]`
already behaves. Fall back to Option B only if a reviewer explicitly wants
restart-only semantics for these fields.

- **Option A (recommended)** — Make the already-reloaded `cfg` the working config
  for the tick: thread it into `due_jobs`, `process_due_jobs`, the execute path,
  the store prune, and delivery. Optionally add a lightweight `reload_from_disk`
  to cut the existing per-tick load cost (perf follow-up, can be deferred).
- **Option B (minimal)** — Leave the loop as-is; mark scheduler/cron/channel
  fields as restart-only in the config API response and document it. Cheaper but
  inconsistent (autonomy already hot-reloads) and leaves the edits silently inert.

> **STOP-and-confirm** only if a reviewer has signaled a preference for Option B.
> Otherwise implement Option A.

## Commands you will need

| Purpose   | Command                                          | Expected on success |
|-----------|--------------------------------------------------|---------------------|
| Format    | `cargo fmt --all -- --check`                     | exit 0, no diff     |
| Lint      | `cargo clippy --all-targets -- -D warnings`      | exit 0, no warnings |
| Tests     | `cargo test --lib cron`                          | all pass            |

Do NOT run a bare `cargo test` (disk-constrained box).

## Scope

**In scope (Option A):**
- `src/cron/scheduler.rs` — thread the reloaded cfg through the tick.
- (Optional perf) `src/config/schema.rs` — add `reload_from_disk`.

**In scope (Option B):**
- `src/gateway/config_api.rs` — add a restart-required note to scheduler/cron/channel edits.
- `docs/reference/config.md` — document the restart requirement.

**Out of scope:**
- The autonomy reload (`security.apply_config`) — keep it exactly as-is; do not
  relitigate the accepted per-tick cost.
- The poll interval (`interval`, built once at line 23) — changing
  `reliability.scheduler_poll_secs` at runtime is a separate, harder change
  (rebuilding the interval mid-loop). Leave it restart-only and note it.
- Delivery-channel *correctness* beyond passing the fresh config — plans 168/194
  own the deeper stale-channels work.

## Git workflow

- Branch: `advisor/171-cron-scheduler-config-refresh`
- Conventional commits, e.g. `fix(cron): apply the per-tick config reload to scheduler/cron/channel fields`.
- Do NOT push or open a PR unless the operator instructed it.

## Steps — Option A (default)

### Step 1: Make the reloaded cfg the working config for the tick

In `src/cron/scheduler.rs::run`, rename the moved boot snapshot to a mutable
working config, and reassign it from the reload each tick. Change line 21's binding
usage so the loop body reads from `working` instead of `config`.

Target shape:

```rust
pub async fn run(config: Config) -> Result<()> {
    let poll_secs = config.reliability.scheduler_poll_secs.max(MIN_POLL_SECONDS);
    let mut interval = time::interval(Duration::from_secs(poll_secs));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let in_flight = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    crate::health::mark_component_ok(SCHEDULER_COMPONENT);

    // The tick works against `working`, refreshed from disk each cycle so an
    // operator editing scheduler/cron/channel config (or rotating a delivery
    // token) reaches scheduled jobs without a daemon restart — the same reload
    // that already keeps `security` (autonomy) current. Only the poll interval
    // stays fixed at its boot value.
    let mut working = config;

    loop {
        interval.tick().await;
        crate::health::mark_component_ok(SCHEDULER_COMPONENT);

        match Config::load_or_init().await {
            Ok(cfg) => {
                security.apply_config(&cfg.autonomy);
                working = cfg;
            }
            Err(e) => tracing::warn!(
                target: "scheduler",
                error = %e,
                "config reload failed; keeping the previously applied config for this tick"
            ),
        }

        let jobs = match due_jobs(&working, Utc::now()) {
            Ok(jobs) => jobs,
            Err(e) => {
                crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                tracing::warn!("Scheduler query failed: {e}");
                continue;
            }
        };

        process_due_jobs(&working, &security, jobs, SCHEDULER_COMPONENT, &in_flight).await;
    }
}
```

Do NOT change the signatures of `due_jobs`, `process_due_jobs`,
`execute_job_with_retry`, `deliver_if_configured`, or the store functions — they
already take `&Config`; you are only changing which `Config` reference flows in.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0 (watch for an
"unused `config`" / "value moved" error — the fix is that `working` owns it now).

### Step 2 (optional perf — defer if uncertain): lightweight `reload_from_disk`

Only do this if you are confident. Add to `impl Config` in `src/config/schema.rs`:

```rust
/// Reload config from the active `config.toml` for a hot path (the scheduler
/// tick), skipping the one-time work `load_or_init` does: legacy-layout
/// migrations, directory creation, and migrated write-back. Still runs schema
/// migrations in memory and decrypts secrets so the returned value matches
/// `load_or_init`'s shape. Falls back to `load_or_init` semantics for path
/// resolution.
pub async fn reload_from_disk() -> Result<Self> { /* … */ }
```

If added, swap the loop's `Config::load_or_init().await` for
`Config::reload_from_disk().await`. **This is the MED-risk part** — it can drift
from `load_or_init` (e.g. miss a future migration). Only add it with a test that
`reload_from_disk` and `load_or_init` produce the same `scheduler`/`cron`/
`channels_config` for a representative config. If you are not confident it stays in
sync, SKIP this step — Option A Step 1 already fixes the correctness bug at
zero added cost, and the perf optimization can be a separate plan.

**Verify**: `cargo test --lib cron` → all pass.

## Steps — Option B (only if a reviewer chose it)

### Step B1: Emit a restart-required note on scheduler/cron/channel edits

In `src/gateway/config_api.rs`, for the handlers that persist scheduler/cron/channel
fields, include a `"restart_required": true` field (and a human note) in the JSON
response, following the shape of `runtime_restart_note` (line 595). Do NOT change
the scheduler loop.

### Step B2: Document it

In `docs/reference/config.md`, add a note that `[scheduler]`, `[cron]`, and channel
delivery config changes take effect only after a daemon restart (while `[autonomy]`
hot-reloads).

## Test plan (Option A)

The existing scheduler tests already prove `apply_config` reaches the run path
(`cron_scheduler_applies_an_autonomy_change`, `scheduler.rs:686`). Add a test that
proves the reloaded config's *scheduler/cron* fields flow through, without spinning
up the real 15s loop. Model after `persist_job_result_records_run_and_reschedules_shell_job`:

- `record_run_prunes_to_current_max_run_history` — insert N runs with
  `config.cron.max_run_history` set low, call `record_run`, assert history is
  pruned to the configured value. (A store-level test proving the field is honored
  when passed the current config; there is already
  `due_jobs_respects_scheduler_max_tasks_limit` at `store.rs:645` and a
  max_run_history test at `store.rs:762` — extend/mirror those rather than
  duplicating.)

The correctness of "the reloaded cfg reaches the tick" is best covered by a focused
review of the `run()` diff plus the existing store tests, since driving the full
`run()` loop deterministically requires wall-clock control. State this in the PR.

If Step 2 (`reload_from_disk`) is done, add:
- `reload_from_disk_matches_load_or_init_for_scheduler_and_cron` — write a config
  with non-default `scheduler.max_concurrent`, `cron.max_run_history`, then assert
  both loaders return equal values for those fields.

Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold (Option A):

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0
- [ ] `rg -n "working" src/cron/scheduler.rs` shows the tick reads `&working`, and
      `due_jobs`/`process_due_jobs` are called with `&working`, not `&config`
- [ ] `security.apply_config(&cfg.autonomy)` is still present (autonomy reload unchanged)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The `run()` code does not match the "Current state" excerpt (drift).
- A reviewer wants Option B — STOP and switch to the Option B steps.
- After Step 1, clippy reports a borrow/move error you cannot resolve by making
  the boot `config` the `mut working` binding — report the exact error.
- You start Step 2 and cannot convince yourself `reload_from_disk` stays in sync
  with `load_or_init` — SKIP Step 2 and ship Step 1 only; note the deferral.
- `cargo test --lib cron` fails twice after a reasonable fix attempt.

## Maintenance notes

- **Poll interval is still restart-only**: `reliability.scheduler_poll_secs` is
  read once at line 22. Changing it at runtime needs the `interval` rebuilt inside
  the loop — deliberately deferred here. Note it in the PR so operators know.
- **Runtime cron.enabled flip**: with `working` refreshed each tick, a future
  addition could `continue` the loop when `working.cron.enabled` becomes false,
  making a runtime disable actually pause firing. Out of scope here; flag as a
  possible follow-up.
- **Cross-ref plans 168/194** — they cover delivery reading stale channels. After
  this plan, delivery gets the *fresh* channels config each tick, which should
  reduce (but may not fully close) those findings. Coordinate so the three don't
  overwrite each other's `deliver_if_configured` edits.
- **Reviewer scrutiny**: confirm the reload's error branch keeps the *previous*
  `working` (never falls back to a permissive/default config), matching the
  existing autonomy error-branch guarantee.
