# Plan 266: Delete the dead legacy-doctor code (follow-up to 256)

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Follow-up to plan 256.** Plan 256's Step 1 (the placeholder-key parity fix — the only user-facing behavior) shipped in PR #666. This plan carries the mechanical dead-code deletion (Steps 2-3), split off because it is ~630 lines of UNREACHABLE code across 14 interdependent functions with zero behavior change, and deserves its own reviewable, isolated-rollback PR (CLAUDE.md §3.8).

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: LOW (removing unreachable code; the compiler + `clippy -D warnings` catch any over/under-deletion)
- **Depends on**: plan 256 / PR #666 (the parity fix) — soft
- **Category**: tech-debt
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

`doctor/legacy.rs` `run` + its exclusive helpers are unreachable — kept alive only by their own tests, exported under an `#[allow(unused_imports)] pub use legacy::run`. Removing the dead half makes "what does doctor actually check" answerable (it becomes exactly the `checks/` registry).

## Coverage analysis (DONE in 256 — verified before this plan)

`legacy::run` ran four checks; each is covered by a live check in the `run_all_detailed` registry (`doctor/mod.rs:159-170`), so nothing is lost:

- `check_config_semantics` → `checks::config::ConfigSchemaCheck`
- `check_workspace` (exists + writable) → `checks::config::PathsCheck` (`writable_probe`)
- `check_daemon_state` → `checks::daemon::DaemonRegistrationCheck`
- `check_environment` (command availability) → `checks::system_deps::SystemDepsCheck`

**One deliberate drop**: `check_workspace`'s best-effort disk-space warning (`disk_available_mb` via `df`, warns < 100 MB) has no `checks/` equivalent. It is NOT in the plan-256 STOP set (writability + command-availability, both covered). Decide when executing: either accept the drop (note it in the PR) or port a `DiskSpaceCheck` to `checks/` FIRST.

## Function map (legacy.rs — verified 2026-08-27)

**DELETE** (dead; callers only inside dead functions):
- consts `DAEMON_STALE_SECONDS` / `SCHEDULER_STALE_SECONDS` / `CHANNEL_STALE_SECONDS` / `COMMAND_VERSION_PREVIEW_CHARS`
- local `enum Severity` + `struct DiagItem` + its impl
- `run` (`:62`), `check_config_semantics` (`:318`), `provider_validation_error` (`:527` — the DEAD copy; the live one is `checks/config.rs`), `embedding_provider_validation_error` (`:545`), `check_workspace` (`:572`), `check_file_exists` (`:636`), `disk_available_mb` (`:653`), `parse_df_available_mb` (`:666`), `workspace_probe_path` (`:672`), `check_daemon_state` (`:685`), `check_environment` (`:819`), `check_command_available` (`:847`), `parse_rfc3339` (`:900`)
- the tests exercising only the above.
- `doctor/mod.rs:16-17` — the `#[allow(unused_imports)] pub use legacy::run;`.

**KEEP** (live model path): `enum ModelProbeOutcome`, `classify_model_probe_error` (`:111`), `ProbeSummary` (`:154`), `refresh_all_model_catalogs` (`:243`, exported), `run_models` (`:268`, exported), `format_error_chain` (`:872`), `truncate_for_display` (`:888`, used by `run_models`) + their tests.

## Steps

### Step 1: (optional) port the disk-space check
If keeping disk-space, add a `checks/` check (or fold it into `PathsCheck`) using `disk_available_mb`/`parse_df_available_mb` before deleting them. Otherwise note the drop.

### Step 2: delete the dead functions + their tests
Simplest given tool friction: reconstruct `legacy.rs` (or rename it `models.rs`) containing ONLY the KEEP list + KEEP tests + the imports they need. Then `cargo build --lib` (catches over-deletion) + `cargo clippy --lib -- -D warnings` (catches unused imports/consts left behind) — iterate until both are clean. Remove `pub use legacy::run` from `doctor/mod.rs`.

**Verify**: `grep -rn "doctor::run\b" src/ tests/` returns nothing; `cargo test --lib doctor` passes; `cargo clippy --lib -- -D warnings` clean.

## Done criteria

- [ ] `cargo fmt`/`clippy --lib -D warnings` clean (no unused warnings)
- [ ] `pub use legacy::run` removed; `grep doctor::run` empty
- [ ] `cargo test --lib doctor` passes (KEEP tests survive)
- [ ] disk-space either ported or its drop noted
- [ ] `plans/README.md` row updated

## STOP conditions

- The compiler flags a KEEP function using a "dead" helper not in the map — re-classify that helper as KEEP; the map was built from `legacy.rs`-internal callers only.
- Plan 233 is mid-edit on `format_error_chain` (KEEP) — coordinate.
