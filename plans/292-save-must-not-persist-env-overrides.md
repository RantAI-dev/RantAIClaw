# Plan 292: `Config::save()` must not write env-override values into config.toml

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/config/schema.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P1 (ledger W1-3, part c)
- **Effort**: M
- **Risk**: MED — touches every save path in the product
- **Category**: bug / exposure boundary
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

`load_or_init` applies environment overrides onto the in-memory `Config`, and `save()`
serialises that same struct. So the first console, TUI or setup write bakes whatever the
environment happened to hold into `config.toml` permanently.

For a container started with `RANTAICLAW_ALLOW_PUBLIC_BIND=true` or `HOST=0.0.0.0`, an
exposure setting that was meant to last one run outlives its cause — and `RANTAICLAW_API_KEY`
becomes a stored credential. Two call sites already hand-roll "load without env" to dodge
this, which is the smell that the default is wrong.

PR #650 deferred this deliberately; nothing has fixed it since.

## Current state (verified at `4b8f61e`)

```rust
// src/config/schema.rs:4337
pub fn apply_env_overrides(&mut self) {
// src/config/schema.rs:4678 — serialises self, provenance and all
pub async fn save(&self) -> Result<()> {
```

`load_or_init` calls `apply_env_overrides` on the struct it returns. There are ~45 `save()`
call sites across ~30 files, including the config API and the TUI.

Sites that already work around it: `src/tools/proxy_config.rs` and `src/channels/telegram.rs`
both re-read config without env applied.

## Steps

1. **Choose the mechanism and write it in the PR description first.** Two workable shapes:
   (a) keep the on-disk config and the effective config as separate values, saving the
   former; (b) record which fields env overrode and restore them before serialising.
   (a) is cleaner but larger; (b) is contained. Do not do both.
   **Verify**: the choice is stated before code is written.

2. **Implement it in `save()` so every call site benefits without changing.** The fix must
   not require 45 edits — if it does, the mechanism is wrong.
   **Verify**: `rg -n '\.save\(\)' src/ | wc -l` is unchanged.

3. **Prove it with the exposure case.** Test: set `RANTAICLAW_ALLOW_PUBLIC_BIND=true`, load,
   save, then read the file back and assert the persisted value is still `false`. Use the
   crate's shared `ENV_LOCK` — env-mutating tests here must not race.
   **Verify**: `cargo test --lib config` passes; the test fails if step 2 is reverted.

4. **Retire the two workarounds** in `proxy_config.rs` and `telegram.rs` if the fix makes
   them redundant, so the codebase has one way of doing this.
   **Verify**: those files no longer re-implement env-free loading.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib config` passes with the new test.
- An env-set exposure flag never appears in a saved `config.toml`.

## STOP conditions

- The chosen mechanism would change what the *running* process sees (as opposed to what is
  written) → STOP. Env overrides must still take effect at runtime; only persistence changes.
- More than a handful of files need editing → STOP; re-read step 2.

## Test plan

One test per override class: a boolean exposure flag and a credential. Both under `ENV_LOCK`
with a `Drop`-based guard, following the pattern PR #678 established.

## Maintenance note

Any new env override added to `apply_env_overrides` inherits this behaviour automatically
once fixed — that is the point of fixing it inside `save()` rather than at call sites.

## Rollback

One commit. Behaviour-only; no schema or storage change.
