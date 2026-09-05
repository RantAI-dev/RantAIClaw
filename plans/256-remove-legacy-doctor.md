# Plan 256: Remove the dead legacy doctor path

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/doctor/mod.rs src/doctor/legacy.rs src/doctor/checks/config.rs`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: LOW (removing unreachable code + one bug-parity fix; verify coverage before deleting)
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

`doctor/legacy.rs` `run` + its exclusive helpers (~630 of 1274 lines) are unreachable — kept alive only by their own ~20 tests, exported under an `#[allow(unused_imports)]`. The surviving live copy of `provider_validation_error` regressed the fix its dead twin documents (`None` vs `Some("doctor-shape-check")`), so for providers that require a credential at construction, `ConfigSchemaCheck` reports a missing key as "default_provider is invalid" and fails the doctor run with the wrong diagnosis. Removing the dead half makes "what does doctor actually check" answerable, and porting the placeholder-key behavior fixes the live misdiagnosis.

## Current state (confirm before editing)

- `src/doctor/mod.rs:16-17` — `#[allow(unused_imports)] pub use legacy::run;`. Grep confirms no caller: `grep -rn "doctor::run\b" src/ tests/` returns nothing; `main.rs:2103` uses `run_all`, `:2088` `run_models`, `:1998` `refresh_all_model_catalogs`.
- `src/doctor/legacy.rs` — `run()` (`:62`) and its exclusive helpers `check_config_semantics:318`, `check_workspace:572`, `check_daemon_state:685`, `check_environment:819`, plus `DiagItem:22` are reachable only through `run`. The genuinely-live functions to KEEP: `doctor_model_targets`, `refresh_model_catalogs`, `run_models`, `refresh_all_model_catalogs`, `ProbeSummary`, `format_error_chain`.
- Live drift: `legacy.rs:527` `provider_validation_error` passes `Some("doctor-shape-check")` with a 5-line comment explaining why a placeholder key is required; the ACTIVE copy at `src/doctor/checks/config.rs:224` passes `None`, called from `:26,:44`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib doctor` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: `src/doctor/mod.rs`, `src/doctor/legacy.rs` (delete `run` + exclusive helpers + their tests; keep the model functions — consider renaming the file to `models.rs`), `src/doctor/checks/config.rs` (port the placeholder-key behavior).
**Out of scope**: the `format_error_chain` credential scrub (plan 233 — coordinate if it also edits legacy.rs); the doctor honesty items (plan 251).

## Git workflow

- Branch: `refactor/remove-legacy-doctor`
- Message e.g. `refactor(doctor): remove the dead legacy doctor path and fix the live provider probe`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Port the placeholder-key behavior into the live check

In `checks/config.rs:224`, change the `None` to `Some("doctor-shape-check")` (or the equivalent placeholder) with the explanatory comment from `legacy.rs:527`, so a provider that requires a credential at construction isn't misdiagnosed as "default_provider is invalid".

**Verify**: Test-plan `provider_requiring_key_is_not_misdiagnosed` passes.

### Step 2: Verify `checks/` covers what `legacy::run` did, then delete the dead half

Confirm `checks/` covers workspace-writability (`legacy.rs:653`) and command availability (`legacy.rs:847` — likely `checks/system_deps.rs`). For anything not covered, port it to a `checks/` module FIRST. Then delete `legacy::run`, `check_config_semantics`, `check_workspace`, `check_daemon_state`, `check_environment`, `DiagItem`, and their tests. Remove the `#[allow(unused_imports)] pub use legacy::run;` at `doctor/mod.rs:16`.

**Verify**: `cargo test --lib doctor` → pass; `cargo clippy --lib -- -D warnings` → no unused-code warnings.

### Step 3: Keep and relocate the live model functions

Keep `doctor_model_targets`/`refresh_model_catalogs`/`run_models`/`refresh_all_model_catalogs`/`ProbeSummary`/`format_error_chain`; optionally move them to `src/doctor/models.rs` and update imports.

**Verify**: `cargo build --lib` exit 0; `cargo test --lib doctor` → pass.

## Test plan

- `provider_requiring_key_is_not_misdiagnosed` — a config whose `default_provider` requires a credential at construction, with no key, yields a "missing key" diagnosis, not "default_provider is invalid".
- Verification: `cargo test --lib doctor` → all pass; the model-refresh tests still pass after relocation.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0 (no unused warnings)
- [ ] `grep -rn "doctor::run\b" src/ tests/` returns nothing; `pub use legacy::run` removed
- [ ] `cargo test --lib doctor` passes incl. the parity test
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `checks/` does NOT cover workspace-writability or command availability — port those to `checks/` BEFORE deleting; do not lose a check.
- Plan 233 is editing `format_error_chain` in the same file — coordinate so the KEEP list isn't deleted.

## Maintenance notes

- Reviewer: confirm no live check is lost (Step 2's coverage check) and that the placeholder-key parity fix lands.
- After this, "what does doctor check" = the `checks/` registry only — a future reader won't be misled by a dead twin.
