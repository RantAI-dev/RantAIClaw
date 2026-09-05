# Plan 254: Harden the config migration version parser and gate

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/config/migrations.rs src/config/schema.rs tests/schema_drift.rs`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P2
- **Effort**: S–M
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug / tests
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

- **F7 (malformed version → v0)**: `raw.get(SCHEMA_VERSION_KEY).and_then(|v| v.as_integer()).map(|i| i as u32).unwrap_or(0)` — a `schema_version = "23"` (string) or `23.0` (float) yields `None` and falls to `0`, silently re-running the entire v0→v23 chain and rewriting the file on every load; a negative integer wraps via `as u32` into a bogus "update rantaiclaw" error for a corrupt config.
- **F8 (v18 depends on process env)**: `migrate_v18` reads `KB_EMBEDDING_API_KEY` from `std::env` as migration evidence → the outcome depends on which process ran first; once stamped to current, the migration never re-runs, so the KB is off permanently with the key present.
- **H3 (self-referential gate)**: 27/28 migration tests assert against `CURRENT_VERSION`, and `tests/schema_drift.rs` suffixes snapshots by `CURRENT_VERSION`, so a bump with no arm + `insta accept` ships green.

## Current state (confirm before editing)

- `src/config/migrations.rs`:
  - `:49-53` — `raw.get(SCHEMA_VERSION_KEY).and_then(|v| v.as_integer()).map(|i| i as u32).unwrap_or(0)`.
  - `:36` — `CURRENT_VERSION: u32 = 23`.
  - `:55-57` — `migrate` returns early once `version == CURRENT_VERSION`; `:58-66` — the guard for a version ABOVE current tells the operator to "Update rantaiclaw".
  - `migrate_v18` (`:366-375`) reads `KB_EMBEDDING_API_KEY` from `std::env`; `:387-398` writes nothing when neither file nor env has a key. `migrations.rs:9-13` promises each migration is a pure `toml::Value` transform.
  - `src/config/schema.rs:4209-4213` — `apply_env_overrides` sets `knowledge.embedding_api_key` from that env var but never sets `knowledge.enabled`.
- `tests/schema_drift.rs:40` — `snapshot_suffix => format!("v{}", CURRENT_VERSION)`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib config::migrations` | pass |
| Test | `cargo test --lib config::schema` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: `src/config/migrations.rs` (version parse + pure v18), `src/config/schema.rs` (fold the KB-enabled evidence into `apply_env_overrides`), `tests/schema_drift.rs` (a companion check that fails if a `@vN.snap` exists for an N with no `if from < N` arm).
**Out of scope**: default alignment (plan 253); the migration write-back atomicity (plan 241).

## Git workflow

- Branch: `fix/config-migration-robustness`
- Message e.g. `fix(config): parse schema_version strictly and make v18 pure`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (F7): parse `schema_version` explicitly

Replace the `unwrap_or(0)` (`:49-53`) with an explicit match: `None` → 0; `Some(Integer(i))` with `0 <= i <= CURRENT_VERSION` → `i as u32`; anything else (string, float, negative, above-current-but-not-integer) → `bail!` naming the bad value's type. Keep the existing above-current guard for a valid-but-too-new integer.

**Verify**: Test-plan `string_version_is_rejected` + `negative_version_is_rejected` pass.

### Step 2 (F8): make `migrate_v18` pure; move the KB-enabled evidence to env-overrides

Drop the `std::env` read from `migrate_v18` (making it a pure `toml::Value` transform). In `apply_env_overrides` (`schema.rs:4209`), when it folds in a non-empty `KB_EMBEDDING_API_KEY` and no explicit `knowledge.enabled` was written, set `knowledge.enabled = true`.

**Verify**: Test-plan `v18_is_pure` + `kb_env_key_enables_kb` pass.

### Step 3 (H3): a gate that fails on a version with no migration arm

Add a `tests/schema_drift.rs` companion (or a `config::migrations` test) that asserts, for every `@vN.snap` file present, there is a corresponding `if from < N` arm in `migrations.rs` source; and add a literal `assert_eq!(CURRENT_VERSION, 23)`-style pin so a bump is a deliberate two-file edit.

**Verify**: temporarily bump `CURRENT_VERSION` with no arm → the companion test FAILS; revert.

## Test plan

- `config::migrations`: `string_version_is_rejected`, `negative_version_is_rejected`, `v18_is_pure` (no env read affects it), plus the existing `fresh_current_version_config_is_noop` still passes.
- `config::schema`: `kb_env_key_enables_kb` — `KB_EMBEDDING_API_KEY` set, no explicit `enabled` → `knowledge.enabled=true` (use panic-safe env guard).
- The gate companion (Step 3) — proven to fail on a no-arm bump.
- Verification: `cargo test --lib config::migrations config::schema` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped tests pass with the new tests
- [ ] `grep -n "std::env" src/config/migrations.rs` shows no env read inside a migration arm
- [ ] the no-arm bump fails the companion gate (verified in Step 3)
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Making `migrate_v18` pure loses a case that genuinely needed the env evidence AND `apply_env_overrides` can't cover it — report; keep the env read but document it, rather than silently changing behavior.
- Any cited excerpt doesn't match — STOP that finding, continue others.

## Maintenance notes

- Reviewer: confirm a string `schema_version` now errors clearly (not silently re-migrates), and the no-arm-bump gate actually fails.
- Interacts with plan 253 (which bumps `CURRENT_VERSION`) — the literal pin in Step 3 will need updating alongside that bump; that is the intended friction.
