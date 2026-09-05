# Plan 170: Refuse cron mutations on CLI + HTTP when cron is disabled, and banner dormant jobs

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/gateway/cron_api.rs src/cron/mod.rs src/tui/commands/cron.rs docs/reference/commands.md`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

There are two gates that decide whether scheduled jobs actually fire:
`config.cron.enabled` (the feature master switch) and `config.scheduler.enabled`
(the poller loop). When either is off, the daemon never starts the scheduler
supervisor (`src/daemon/mod.rs:125-142`, which logs "Scheduler disabled …
supervisor not started"). Every agent **tool** honors this — each `cron_*` tool
returns `cron is disabled by config (cron.enabled=false)` when `cron.enabled` is
false. But the **CLI** (`src/cron/mod.rs::handle_command`), the **HTTP API**
(`src/gateway/cron_api.rs`), and the **TUI** do not. So with cron disabled an
operator can create a job from the CLI or web console, get a success response
with a confident `next_run`, and never see it fire. `docs/reference/commands.md:148`
already claims "Mutating schedule/cron actions require `cron.enabled = true`" —
a promise only the tool surface keeps today. This plan makes the CLI and HTTP
surfaces refuse mutations when `cron.enabled` is false, keeps list/history
readable, and surfaces a persistent "scheduler disabled" banner so nobody
mistakes a dormant queue for a live one.

## Current state

### `src/gateway/cron_api.rs` — HTTP handlers, no cron.enabled check

Existing error helpers (reuse these; do not invent new patterns):

```rust
fn err_500(msg: impl std::fmt::Display) -> ApiError {   // line 69
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "internal_error", "detail": msg.to_string() })))
}
fn err_400(msg: impl std::fmt::Display) -> ApiError {   // line 76
    (StatusCode::BAD_REQUEST, Json(json!({ "error": "bad_request", "detail": msg.to_string() })))
}
fn err_404(msg: impl std::fmt::Display) -> ApiError {   // line 83
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found", "detail": msg.to_string() })))
}

/// Clone the running config for store/scheduler calls (workspace_dir + autonomy).
fn cfg_snapshot(state: &AppState) -> crate::config::Config {   // line 102
    state.config.lock().clone()
}
```

Each mutating handler already takes a `cfg` snapshot early:

- `async fn create_cron` (line 197) — `let cfg = cfg_snapshot(&state);` at **line 204**
- `async fn update_cron` (line 291) — `let cfg = cfg_snapshot(&state);` at **line 298**
- `async fn delete_cron` (line 327) — `let cfg = cfg_snapshot(&state);` at **line 333**
- `async fn run_cron` (line 352) — `let cfg = cfg_snapshot(&state);` at **line 359**

Read handlers (leave untouched — must stay readable): `list_cron` (line 107),
`list_cron_runs` (line 131).

### `src/cron/mod.rs::handle_command` (lines 22–193) — CLI, no cron.enabled check

The function signature and dispatch:

```rust
pub fn handle_command(command: crate::CronCommands, config: &Config) -> Result<()> {
    match command {
        crate::CronCommands::List => { /* lines 25-63 — READ, keep open */ }
        crate::CronCommands::Add { .. } => { /* … */ }
        crate::CronCommands::AddAt { .. } => { /* … */ }
        crate::CronCommands::AddEvery { .. } => { /* … */ }
        crate::CronCommands::Once { .. } => { /* … */ }
        crate::CronCommands::Update { .. } => { /* … */ }
        crate::CronCommands::Remove { id } => remove_job(config, &id),
        crate::CronCommands::Pause { id } => { /* … */ }
        crate::CronCommands::Resume { id } => { /* … */ }
        crate::CronCommands::Run { id } => { /* … */ }
        crate::CronCommands::Runs { id, limit } => { /* lines 188-191 — READ, keep open */ }
    }
}
```

`anyhow::{bail, Result}` is already imported at the top of the file (line 3).

### `src/tui/commands/cron.rs` — list render, no banner

`fn list_text(config: &Config) -> String` (lines 82–114) formats the `/cron list`
output. `std::fmt::Write as _` and `crate::config::Config` are already imported
(lines 1, 6).

### Config defaults (both switches default to `true`)

- `config.cron.enabled` default = `true` (`src/config/schema.rs`, `CronConfig`)
- `config.scheduler.enabled` default = `true` (`src/config/schema.rs:2469`)

So `Config::default()` has cron enabled; tests must set `config.cron.enabled = false`
explicitly to exercise the refusal.

### Tool-surface message (reuse verbatim for parity)

Every `cron_*` tool refuses with exactly:
`"cron is disabled by config (cron.enabled=false)"`
(`src/tools/cron_run.rs:45`, `cron_remove.rs:81`, `cron_update.rs:82`, `cron_runs.rs:58`, `cron_add.rs:103`, `cron_list.rs:41`).

### Docs

`docs/reference/commands.md:148`:
`- Mutating schedule/cron actions require \`cron.enabled = true\`.`

## Design decisions (read before implementing)

1. **Gate on `cron.enabled` only** for the refusal — this matches the tool
   surface and the existing docs sentence exactly. Do NOT also refuse on
   `scheduler.enabled = false`; that stays a *banner-only* signal so behavior is
   identical across tools, CLI, and HTTP.
2. **Keep reads open** (`list`, `runs`) on the CLI and HTTP surfaces so an
   operator can inspect dormant jobs. (The agent-tool surface blocks reads too,
   but this plan deliberately diverges for better operator UX — call this out in
   the PR.)
3. **Banner** wording: when `cron.enabled` is false OR (`cron.enabled` is true
   but `scheduler.enabled` is false), list output warns that listed jobs will
   not fire.

## Commands you will need

| Purpose   | Command                                          | Expected on success |
|-----------|--------------------------------------------------|---------------------|
| Format    | `cargo fmt --all -- --check`                     | exit 0, no diff     |
| Lint      | `cargo clippy --all-targets -- -D warnings`      | exit 0, no warnings |
| Tests     | `cargo test --lib cron`                          | all pass            |

Do NOT run a bare `cargo test` (disk-constrained box). Use the filtered command above.

## Scope

**In scope** (Rust — the only files you should modify):
- `src/gateway/cron_api.rs` — add `ensure_cron_enabled` guard + one call in each of the 4 mutating handlers; add the two enabled flags to the `list_cron` response.
- `src/cron/mod.rs` — add the mutating-command guard to `handle_command`; add the banner to the `List` arm.
- `src/tui/commands/cron.rs` — add the banner to `list_text`.
- `docs/reference/commands.md` — clarify the sentence at line 148 now covers CLI + HTTP.

**In scope (claw-ui — SEPARATE repo `/home/sulthannauval/project/rantai/claw-ui`, its own build/release)** — do this only after the Rust half is green; see Step 6:
- `src/lib/api.ts` — extend the `cron()` response type.
- `src/components/ops/cron-panel.tsx` — render the banner.

**Out of scope** (do NOT touch):
- The agent-tool files under `src/tools/cron_*.rs` — already gated; leave them.
- `src/daemon/mod.rs` — the supervisor start/skip logic is correct; do not change it.
- Read handlers `list_cron`, `list_cron_runs`, and the CLI `List`/`Runs` arms' data path — keep them readable (you only ADD a banner to List).
- Plan 174 territory (broader create_cron review) — coordinate but do not merge scope.

## Git workflow

- Branch: `advisor/170-cron-enabled-enforcement`
- Conventional commits, e.g. `fix(cron): refuse CLI/HTTP mutations when cron.enabled=false`.
- Commit the Rust half and the claw-ui half separately (different repos).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the HTTP guard helper + enabled flags to `cron_api.rs`

Add a new error helper and a guard fn near the other `err_*` helpers (after `err_404`, around line 88):

```rust
fn err_409(msg: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "cron_disabled", "detail": msg.to_string() })),
    )
}

/// Refuse mutating cron operations when the cron feature switch is off — mirrors
/// the agent-tool surface (every `cron_*` tool checks `cron.enabled`). Read
/// endpoints (`list_cron`, `list_cron_runs`) deliberately stay open so an
/// operator can still inspect dormant jobs.
fn ensure_cron_enabled(cfg: &crate::config::Config) -> Result<(), ApiError> {
    if cfg.cron.enabled {
        Ok(())
    } else {
        Err(err_409("cron is disabled by config (cron.enabled=false)"))
    }
}
```

In each of the four mutating handlers, add the guard on the line immediately
after the existing `let cfg = cfg_snapshot(&state);`:

- `create_cron` (after line 204)
- `update_cron` (after line 298)
- `delete_cron` (after line 333)
- `run_cron` (after line 359)

```rust
    let cfg = cfg_snapshot(&state);
    ensure_cron_enabled(&cfg)?;
```

In `list_cron` (lines 107–119), add the two flags to the response JSON so the
web console can render its banner. Change the final `Ok(Json(...))` to:

```rust
    Ok(Json(json!({
        "jobs": jobs,
        "count": count,
        "cron_enabled": cfg.cron.enabled,
        "scheduler_enabled": cfg.scheduler.enabled,
    })))
```

(`cfg` is already in scope in `list_cron` — it is captured before `spawn_blocking`.
Re-read the running config for the flags rather than the moved snapshot: take the
flags from a fresh `cfg_snapshot(&state)` at the top of the handler if the
original `cfg` was moved into the closure. Confirm by reading lines 111–118 and
adapt so the flags come from a value still in scope.)

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Add the CLI mutating-command guard in `handle_command`

At the very start of `handle_command` (before the `match command`), add:

```rust
    let mutating = matches!(
        command,
        crate::CronCommands::Add { .. }
            | crate::CronCommands::AddAt { .. }
            | crate::CronCommands::AddEvery { .. }
            | crate::CronCommands::Once { .. }
            | crate::CronCommands::Update { .. }
            | crate::CronCommands::Remove { .. }
            | crate::CronCommands::Pause { .. }
            | crate::CronCommands::Resume { .. }
            | crate::CronCommands::Run { .. }
    );
    if mutating && !config.cron.enabled {
        bail!("cron is disabled by config (cron.enabled=false)");
    }
```

`matches!` only borrows `command`, so the subsequent `match command` still moves
it. `List` and `Runs` are intentionally absent from the list — they stay open.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Add the banner to the CLI `List` arm

At the top of the `crate::CronCommands::List` arm (line 25), before `list_jobs`,
print a warning when the queue is dormant:

```rust
        crate::CronCommands::List => {
            if !config.cron.enabled {
                println!(
                    "⚠️  Scheduler disabled (cron.enabled=false) — listed jobs will NOT fire until you re-enable it."
                );
            } else if !config.scheduler.enabled {
                println!(
                    "⚠️  Scheduler loop disabled (scheduler.enabled=false) — listed jobs will NOT fire until you re-enable it."
                );
            }
            let jobs = list_jobs(config)?;
            /* … rest unchanged … */
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Add the banner to the TUI `list_text`

In `src/tui/commands/cron.rs`, `fn list_text` (lines 82–114), prepend a banner
line to the returned string when dormant. In the `Ok(jobs)` non-empty arm (and
the empty arm), build the output starting with:

```rust
    let banner = if !config.cron.enabled {
        "⚠️  Scheduler disabled (cron.enabled=false) — jobs will NOT fire.\n"
    } else if !config.scheduler.enabled {
        "⚠️  Scheduler loop disabled (scheduler.enabled=false) — jobs will NOT fire.\n"
    } else {
        ""
    };
```

Prefix `banner` onto the `out`/message string returned in both the empty and
non-empty `Ok` arms (leave the `Err` arm as-is). Keep it a single leading line.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 5: Update the docs sentence

In `docs/reference/commands.md`, line 148, change:

```
- Mutating schedule/cron actions require `cron.enabled = true`.
```

to:

```
- Mutating schedule/cron actions (CLI `cron add/update/remove/pause/resume/run`,
  the `cron_*` tools, and the `POST/PUT/DELETE /api/v1/cron*` endpoints) require
  `cron.enabled = true`; when it is false they are refused. Listing and run
  history stay readable. If `scheduler.enabled = false`, jobs persist but never
  fire — `cron list` shows a "scheduler disabled" banner.
```

**Verify**: `git diff docs/reference/commands.md` shows only this sentence changed.

### Step 6 (claw-ui — separate repo, do last): render the banner

Only after the Rust half is green. Work in `/home/sulthannauval/project/rantai/claw-ui`.

1. `src/lib/api.ts` line 196 — extend the response type:
   ```ts
   cron: () => rc<{ jobs: CronJob[]; count: number; cron_enabled?: boolean; scheduler_enabled?: boolean }>("cron"),
   ```
2. `src/components/ops/cron-panel.tsx` — inside the `PanelFrame`/`Card` that maps
   `data?.jobs` (around line 326–328), render a banner above the job list when
   `data?.cron_enabled === false || data?.scheduler_enabled === false`:
   ```tsx
   {(data?.cron_enabled === false || data?.scheduler_enabled === false) && (
     <div className="border-b border-border bg-warning/10 px-3 py-2 text-[11px] text-warning">
       Scheduler disabled — these jobs will not fire until it is re-enabled in Configuration.
     </div>
   )}
   ```
   (The fields are optional; `undefined` from an older backend must NOT show the
   banner — that is why the comparison is `=== false`.)

**Verify (claw-ui)**:
```bash
cd /home/sulthannauval/project/rantai/claw-ui && ./node_modules/.bin/next build
```
→ build succeeds. Then load the console against a daemon with `cron.enabled = false`
and confirm the banner shows and the Schedules panel still lists jobs.

## Test plan

Add to the existing `#[cfg(test)] mod tests` in `src/cron/mod.rs` (models after
`update_no_flags_fails`, which uses `test_config`):

- `cron_add_refused_when_disabled` — build `test_config`, set
  `config.cron.enabled = false`, call `handle_command(CronCommands::Add { … }, &config)`,
  assert `is_err()` and the message contains `cron.enabled=false`. This is the
  CLI-surface regression test.
- `cron_list_allowed_when_disabled` — set `config.cron.enabled = false`, call
  `handle_command(CronCommands::List, &config)`, assert `is_ok()` (reads stay open).

Add to `#[cfg(test)] mod tests` in `src/gateway/cron_api.rs` (models after
`resolve_job_kind_infers_from_fields`) — test the guard helper directly, since a
full handler test would need an `AppState`:

- `ensure_cron_enabled_refuses_when_disabled` — build a `crate::config::Config`
  with `cron.enabled = false`, assert `ensure_cron_enabled(&cfg)` is `Err` and the
  status is `StatusCode::CONFLICT`.
- `ensure_cron_enabled_allows_when_enabled` — `cron.enabled = true` → `Ok`.

Verification: `cargo test --lib cron` → all pass, including the 4 new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the 4 new tests exist and pass
- [ ] `rg -n "ensure_cron_enabled" src/gateway/cron_api.rs` shows the helper + 4 call sites
- [ ] `rg -n "mutating && !config.cron.enabled" src/cron/mod.rs` returns 1 match
- [ ] `list_cron` response JSON includes `cron_enabled` and `scheduler_enabled`
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] (claw-ui) `next build` succeeds and the banner renders when disabled
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The code at the line locations in "Current state" does not match the excerpts
  (drift since this plan was written).
- Adding the guard to `delete_cron` provokes a design objection you can't resolve
  (some operators expect to delete dormant jobs while cron is off). If a reviewer
  wants delete/remove exempt, STOP and confirm before diverging — the tool
  surface gates remove too, so this plan gates it for parity.
- `cargo test --lib cron` fails twice after a reasonable fix attempt.
- You cannot obtain a value still in scope for the `cron_enabled`/`scheduler_enabled`
  flags in `list_cron` without moving `cfg` twice — STOP and report the exact
  borrow error rather than restructuring the handler heavily.

## Maintenance notes

- **Reviewer scrutiny**: confirm reads (`list`, `runs`) remain open on CLI + HTTP,
  and that the refusal fires for ALL mutating verbs (create/update/delete/run on
  HTTP; add/add-at/add-every/once/update/remove/pause/resume/run on CLI).
- **Overlaps plan 174**, which also notes `create_cron` lacks the check. If 174
  lands first, this plan's `ensure_cron_enabled` may already partly exist — merge,
  don't duplicate.
- **Delete-while-disabled tradeoff**: this plan gates delete for parity with the
  `cron_remove` tool. If operators complain they can't clean up dormant jobs, the
  follow-up is to exempt delete/remove from the guard (a one-line change).
- **`scheduler.enabled` refusal**: deliberately NOT enforced (banner only). If a
  future change wants full refusal when the poller is off, reuse the private
  `scheduler_enabled(config)` logic from `src/daemon/mod.rs:23-24` (promote it to a
  shared helper first).
