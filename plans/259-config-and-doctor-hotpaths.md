# Plan 259: Speed up config reloads, the secret-key read, the model probe, and the TUI doctor path

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/gateway/config_api.rs src/gateway/mod.rs src/config/fingerprint.rs src/security/secrets.rs src/config/schema.rs src/doctor/legacy.rs src/onboard/wizard.rs src/tui/commands/config.rs`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: LOW–MED (a wrong cache/skip could serve stale config; guard carefully)
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Four hot-path inefficiencies in the config/doctor cluster:

- **I2** Each console config write costs TWO full config loads (handler `load_or_init` + the watcher reload on the file it just wrote) plus a redundant process-env mutation from a background task. `persist_and_swap` updates `state.config` but not `state.config_fingerprint`, so `GET /version` reports a stale fingerprint for ~500ms.
- **I3** `load_or_create_key` re-reads `.secret_key` (blocking `std::fs`) once per secret field → ~10-60 reads per config load/save, on an async worker.
- **I1** The all-provider model catalog probe is a serial `for` loop with an 8s timeout each → `doctor models`/`models refresh --all` worst case ~34×8s behind a firewall.
- **F10** TUI `/doctor` runs a full `Config::load_or_init()` (migrations + possible file write + env-override + proxy-env mutation, blocking the render thread) just to resolve a directory path.

## Current state (confirm before editing)

- **I2**: `src/gateway/config_api.rs:244` `lock_and_load` calls `Config::load_or_init()` on every write; `persist_and_swap` (`:253`) sets `state.config` but not `state.config_fingerprint`. The watcher (`gateway/mod.rs:1267`) reloads again ~500ms later. `src/config/fingerprint.rs:12` provides the cheap comparison; it's computed at `gateway/mod.rs:1276` only AFTER the reload. `src/config/watcher.rs:84` debounces but has no content check (compare the skills watcher's before/after diff at `tui/app.rs:3782-3790`).
- **I3**: `src/security/secrets.rs:171` `load_or_create_key` does `fs::read_to_string(&self.key_path)` with no caching, called from `encrypt`/`decrypt`. `SecretStore` is constructed fresh per operation (`schema.rs:4500`). Key file is written once at creation (`secrets.rs:189`), never rotated in-process.
- **I1**: `src/doctor/legacy.rs:169` `refresh_model_catalogs` iterates `doctor_model_targets(None)` (all 34 providers, `:141`) in a plain `for`; `wizard.rs:1416` sets 8s/4s timeouts. `src/doctor/mod.rs:167` already fans the check registry with `join_all`.
- **F10**: `src/tui/commands/config.rs:275-284` — `block_on(Config::load_or_init())` just to call `daemon::state_file_path(&c)`; `Config::resolve_active_paths()` (`schema.rs:3926-3931`) returns the path without reading/migrating.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib gateway config security doctor tui` (filtered) | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: the files in the drift check.
**Out of scope**: the reloader cancellation (plan 246 — coordinate on `spawn_config_reloader`); the migration write-back (plan 241).

## Git workflow

- Branch: `perf/config-and-doctor-hotpaths`
- Message e.g. `perf(config): fingerprint-gate reloads, cache the secret key, parallelize model probe`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (I2): fingerprint-gate the reload and publish the fingerprint on self-writes

In `spawn_config_reloader`, compute `fingerprint_file` on tick and `continue` when unchanged. Have `persist_and_swap` set `state.config_fingerprint` right after `save()` so the gateway's own writes self-suppress (the watcher sees a matching fingerprint and skips).

**Verify**: Test-plan `self_write_does_not_trigger_second_reload` passes (or a fingerprint-equality unit test if the full loop is hard to drive).

### Step 2 (I3): memoize the secret key

Add a `OnceCell<Vec<u8>>` (or `OnceLock`) to `SecretStore` populated on first `load_or_create_key`; wrap the `save()`/`load_or_init` decrypt loops so filesystem work leaves the async executor (`spawn_blocking`) where practical. The key is written once and never rotated in-process, so the cache lifetime is bounded.

**Verify**: Test-plan `secret_key_read_once_per_store` passes.

### Step 3 (I1): parallelize the model probe with a concurrency cap

Fan `refresh_model_catalogs` out over `spawn_blocking` with a semaphore of ~8, collect `(provider, outcome)`, and render in `targets` order so output stays deterministic (the loop currently prints per-provider as it goes — buffer and print in stable order). Mirror `doctor::run_all_detailed`'s structure.

**Verify**: `cargo test --lib doctor` → pass; manual: `rantaiclaw doctor models` no longer takes minutes behind a firewall.

### Step 4 (F10): resolve the path without a full load

Replace the `load_or_init()` in `tui/commands/config.rs:275` with `Config::resolve_active_paths()` (or cache the already-loaded `Config` on `TuiContext` — `app_config` is in scope at `tui/app.rs:8220`).

**Verify**: `cargo test --lib tui::commands::config` → pass; the `/doctor` path no longer triggers a config write/reload.

## Test plan

- `self_write_does_not_trigger_second_reload` — a gateway self-write leaves the fingerprint matching, so the watcher skips (assert the fingerprint equality / a reload counter).
- `secret_key_read_once_per_store` — a `SecretStore` decrypting N fields reads the key file once (count reads via a temp key path + a wrapper, or assert the cache is populated).
- `doctor` model probe: assert results are returned in `targets` order after parallelization.
- Verification: `cargo test --lib gateway config security doctor tui` (filtered) → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped tests pass with the new tests
- [ ] `grep -n "load_or_init" src/tui/commands/config.rs` no longer shows the path-only call
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Fingerprint-gating could skip a reload for a genuinely changed file (hash collision is not a concern, but a mis-timed fingerprint update might) — a skipped reload for byte-identical content is a no-op by definition; if the fingerprint can go stale wrongly, report.
- Parallelizing the probe changes output ordering in a way a test/CI parses — buffer + stable order fixes it; report if a consumer depends on streaming output.
- Plan 246 is refactoring `spawn_config_reloader` — coordinate (both edit it).

## Maintenance notes

- Reviewer: confirm no stale config can be served (Step 1's fingerprint update is on the self-write) and the model probe output stays deterministic.
- I2 overlaps plan 246 (reloader) — land order matters.
