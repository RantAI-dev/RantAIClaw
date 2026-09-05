# Plan 236: Hot-reload `require_pairing`, not just the token list

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/gateway/mod.rs src/security/pairing.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: MED (flipping enforcement to `true` mid-flight will 401 unpaired clients — that is the intent, but log it clearly)
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

When an operator reacts to an incident by setting `require_pairing = true` in `config.toml`, the running gateway logs "reloaded running config" but the control plane stays fully open until a process restart. The asymmetry is what makes it dangerous: token REVOCATION via the same file DOES take effect (the reloader calls `sync_tokens`), so the reload appears to work — but the enforcement flag is fixed at `PairingGuard::new` time and never rebuilt. Every config/control route short-circuits on that stale flag, and a configured console password does not compensate (it gates a different surface).

After this lands: a change to `require_pairing` in `config.toml` takes effect on the next reload without a restart.

## Current state

- `src/security/pairing.rs`:
  - `require_pairing: bool` is a plain field (`pairing.rs:37`), set once by `PairingGuard::new(require_pairing, existing_tokens)` (`:60-83`). `require_pairing(&self) -> bool` (`:91-93`) reads it. `is_authenticated` returns `true` unconditionally when it is false (verify around `:175-182`).
  - `paired_tokens` IS interior-mutable (`Arc<Mutex<HashSet<String>>>`, `:41`) and `sync_tokens` reconciles it live.
- `src/gateway/mod.rs`:
  ```rust
  while watcher.reload_rx.recv().await.is_some() {          // :1267
      match Config::load_or_init().await {
          Ok(fresh) => {
              pairing.sync_tokens(&fresh.gateway.paired_tokens);   // :1274 — tokens only
              *config.lock() = fresh;
              *config_fingerprint.lock() = ...fingerprint_file(&config_path);
              tracing::info!(... "config.toml changed — reloaded running config");
          }
          Err(e) => { tracing::warn!(...); }
      }
  }
  ```
  Nothing here updates the enforcement flag.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib security::pairing` | pass |
| Test | `cargo test --lib gateway` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/security/pairing.rs` (make `require_pairing` interior-mutable; add a setter or extend `sync_tokens` into `sync_from_config`)
- `src/gateway/mod.rs` (call the setter in the reloader)

**Out of scope**:
- The brute-force counters, login keyspace, token hashing — unrelated.
- `[gateway.login]` — a separate surface; do not entangle.

## Git workflow

- Branch: `fix/require-pairing-hot-reload`
- Message e.g. `fix(security): hot-reload require_pairing alongside the token list`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Make `require_pairing` interior-mutable

Change `require_pairing: bool` to an atomic/lockable form (`Arc<AtomicBool>` is simplest — matches the "cheap flag" nature). Update `PairingGuard::new` to initialize it, `require_pairing()` to load it, and `is_authenticated` (and any other reader) to load it. `PairingGuard` already derives `Clone` and holds `Arc`s, so this is consistent.

**Verify**: `cargo test --lib security::pairing` compiles + passes (existing tests unchanged in behavior).

### Step 2: Add `sync_from_config` and call it in the reloader

Add a method `sync_from_config(&self, cfg: &GatewayConfig)` (or extend `sync_tokens`) that reconciles BOTH the token set and stores `cfg.require_pairing` into the atomic, logging at `warn` when the enforcement value CHANGES (was→now). In `gateway/mod.rs:1274`, replace the `pairing.sync_tokens(...)` call with `pairing.sync_from_config(&fresh.gateway)`.

**Verify**: `cargo test --lib security::pairing gateway` → pass; Test-plan `require_pairing_flip_takes_effect` passes.

## Test plan

- `security::pairing`: `require_pairing_flip_takes_effect` — construct a guard with `require_pairing=false`; assert `is_authenticated` allows an unpaired request; call `sync_from_config` with a `GatewayConfig` where `require_pairing=true`; assert `is_authenticated` now rejects the same unpaired request. Also assert the reverse (true→false).
- `security::pairing`: `sync_from_config_still_syncs_tokens` — proves the token-sync half wasn't lost in the refactor.
- Verification: `cargo test --lib security::pairing gateway` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped pairing + gateway tests pass with the 2 new tests
- [ ] `grep -n "sync_tokens" src/gateway/mod.rs` shows the reloader now calls the config-aware method (or `sync_tokens` still exists but `require_pairing` is also updated in the reloader)
- [ ] `git status` shows only the two in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `PairingGuard::new` signature or `is_authenticated` shape changed (drift) — STOP.
- Making `require_pairing` atomic forces a change to a public API used outside `pairing.rs`/`gateway/mod.rs` — report before widening scope.
- The reloader is being redesigned by another in-flight change (e.g. plan 246 touches `spawn_config_reloader`) — coordinate; both edit `gateway/mod.rs` near the reloader.

## Maintenance notes

- Reviewer: confirm a false→true flip actually rejects an unpaired request in a test, and that the transition is logged at `warn`.
- Interacts with plan 246 (config-watcher lifecycle) — both touch `spawn_config_reloader`; land order matters, note the conflict in whichever merges second.
