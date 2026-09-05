# Plan 190: Remove the `println!` from `remove_job`; print the confirmation in the CLI arm

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 2aefb9f..HEAD -- src/cron/store.rs src/cron/mod.rs`
> If either file changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch, treat
> it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: tech-debt / bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`remove_job` in the store layer prints `✅ Removed cron job {id}` straight to
stdout. But the store is called from three non-CLI surfaces that must not print:

- the daemon scheduler's one-shot auto-delete (`src/cron/scheduler.rs:296`),
  where a raw stdout line bypasses `tracing`;
- the HTTP handler (`src/gateway/cron_api.rs:335`), which returns JSON;
- the ratatui TUI (`src/tui/app.rs:3516`), where a raw `println!` writes into the
  alternate-screen buffer and **corrupts the rendered frame** until the next
  redraw (the TUI then sets its own status message anyway).

A store function is the wrong layer to emit user-facing CLI output. Only the CLI
`cron remove` command should print a confirmation — and the CLI is the one
caller that currently relies on the store to do it. This plan moves the print to
where it belongs.

## Current state

### `src/cron/store.rs` — `remove_job` (lines 148–160)

```rust
pub fn remove_job(config: &Config, id: &str) -> Result<()> {
    let changed = with_connection(config, |conn| {
        conn.execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])
            .context("Failed to delete cron job")
    })?;

    if changed == 0 {
        anyhow::bail!("Cron job '{id}' not found");
    }

    println!("✅ Removed cron job {id}");   // <-- line 158: DELETE THIS
    Ok(())
}
```

### `src/cron/mod.rs` — the CLI `Remove` arm (line 169)

```rust
        crate::CronCommands::Remove { id } => remove_job(config, &id),
```

For contrast, the adjacent CLI arms already print their own confirmations —
`Pause` (lines 170–174) and `Resume` (lines 175–179):

```rust
        crate::CronCommands::Pause { id } => {
            pause_job(config, &id)?;
            println!("⏸️  Paused cron job {id}");
            Ok(())
        }
        crate::CronCommands::Resume { id } => {
            resume_job(config, &id)?;
            println!("▶️  Resumed cron job {id}");
            Ok(())
        }
```

### Non-CLI callers that must stay silent (do not edit — just confirm they exist)

- `src/cron/scheduler.rs:296` — `if let Err(e) = remove_job(config, &job.id) { ... }`
- `src/gateway/cron_api.rs:335` — `spawn_blocking(move || cron::remove_job(&cfg, &id_for_store))`
- `src/tui/app.rs:3516` — `'d' => match crate::cron::remove_job(&config, &id) { ... }`
  (already emits its own `self.cron_system_msg("🗑 Removed cron job {id}")` on Ok)

Convention: keep the exact user-facing string `✅ Removed cron job {id}` so the
CLI output is byte-for-byte what it was before (the store printed that literal).

## Commands you will need

| Purpose   | Command                                             | Expected on success        |
|-----------|-----------------------------------------------------|----------------------------|
| Format    | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint      | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests     | `cargo test --lib cron`                             | all pass                   |

Do NOT run bare `cargo test` (disk-constrained). Scope to `cron`.

## Scope

**In scope** (the only files you should modify):

- `src/cron/store.rs` — delete line 158 (the `println!`) from `remove_job`.
- `src/cron/mod.rs` — change the `Remove` arm (line 169) to call `remove_job`
  and then print.

**Out of scope** (do NOT touch):

- `src/cron/scheduler.rs`, `src/gateway/cron_api.rs`, `src/tui/app.rs` — these
  callers already handle their own (or no) output; leave them.
- Any other `println!` in the store — there are none to change; this plan is
  only about `remove_job`.

## Git workflow

- Branch: `advisor/190-cron-store-remove-println`
- Conventional commit, e.g. `refactor(cron): move remove_job confirmation from the store to the CLI arm`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Delete the `println!` from `remove_job`

In `src/cron/store.rs`, remove line 158 so `remove_job` ends:

```rust
    if changed == 0 {
        anyhow::bail!("Cron job '{id}' not found");
    }

    Ok(())
}
```

**Verify**: `grep -n "println!" src/cron/store.rs` → **no** matches.

### Step 2: Print the confirmation from the CLI `Remove` arm

In `src/cron/mod.rs`, replace line 169:

```rust
        crate::CronCommands::Remove { id } => remove_job(config, &id),
```

with:

```rust
        crate::CronCommands::Remove { id } => {
            remove_job(config, &id)?;
            println!("✅ Removed cron job {id}");
            Ok(())
        }
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Final validation

**Verify**:
- `cargo fmt --all -- --check` → exit 0, no diff.
- `cargo clippy --all-targets -- -D warnings` → exit 0.
- `cargo test --lib cron` → all pass (the existing `add_list_remove_roundtrip`
  and `remove_job_cascades_run_history` tests call `remove_job` and must still
  pass — they assert on store state, not stdout).

## Test plan

- No new automated test: the change is a print-location move with no logic
  change, and stdout is not asserted in the store tests. The existing
  `add_list_remove_roundtrip` (store.rs) and `remove_job_cascades_run_history`
  (store.rs) exercise `remove_job`'s deletion + not-found error paths and must
  continue to pass unchanged.
- Manual confirmation is optional and NOT required for done criteria; if you do
  it: `cargo run -- cron remove <id>` still prints `✅ Removed cron job <id>`.
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0
- [ ] `grep -n "println!" src/cron/store.rs` returns no matches
- [ ] `grep -n "Removed cron job" src/cron/mod.rs` returns a match in the
      `Remove` arm
- [ ] Only `src/cron/store.rs` and `src/cron/mod.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `remove_job` (store.rs) or the `Remove` arm (mod.rs) do not match the excerpts.
- Removing the `println!` produces an unused-import or dead-code warning that
  implies another caller depended on it — investigate and report rather than
  re-adding the print.
- `cargo test --lib cron` fails twice after a reasonable fix attempt.
- The fix appears to require touching a file outside the in-scope list.

## Maintenance notes

- The silent `remove_job` is what several other cron plans assume (parity work
  that calls `remove_job` from non-CLI surfaces expects no stdout side effect).
  Keep it silent; any future user-facing confirmation belongs in the calling
  surface, not the store.
- Reviewer should scrutinize: the CLI arm still returns `Ok(())` (it must, to
  match the `Result<()>` arm type) and still propagates the not-found error via
  `?` before printing success.
