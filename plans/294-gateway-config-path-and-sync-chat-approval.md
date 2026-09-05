# Plan 294: Make the gateway read the config it is running, and stop sync chat prompting a TTY

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/gateway/config_api.rs src/gateway/mod.rs src/gateway/api_v1.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P1 (ledger W1-5, part b)
- **Effort**: M
- **Risk**: MED (config resolution is load-bearing)
- **Category**: bug
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

Two independent defects on the same surface.

**Split-brain config.** The config API and the hot-reloader both call `Config::load_or_init()`,
which re-resolves the config path from the environment and workspace markers. The gateway is
already running against a specific path. If a marker changes after boot, a console write
read-modify-saves a *different* file and swaps it into the running state. The pairing-token
writer was already fixed to use the running path; these two were not.

**Sync chat prompts a terminal.** The streaming chat path installs a web approval backend; the
synchronous one does not, so it inherits the CLI backend. Running `rantaiclaw gateway` in a
terminal means a gated tool call blocks the HTTP request on the operator's TTY until the
request timeout. Under systemd it silently auto-denies. Same endpoint, different behaviour
depending on how the process was started.

## Current state (verified at `4b8f61e`)

```rust
// src/gateway/config_api.rs:355-358
async fn lock_and_load(
    ...
    let cfg = crate::config::Config::load_or_init()
```

```rust
// src/gateway/mod.rs:1381 — the hot-reloader, watching config_path but loading by resolution
            match Config::load_or_init().await {
```

The already-correct pattern to copy is the pairing-token persistence in the same file, which
loads from the running `config_path`.

```rust
// src/gateway/api_v1.rs:775 — the streaming path sets an approval backend
                    agent.set_approval(Some(manager), Some(backend));
```

The synchronous chat handler has no equivalent call.

## Steps

1. **Load from the running path in both places.** Use the `config_path` the gateway booted
   with, matching the pairing-token writer.
   **Verify**: `rg -n 'load_or_init' src/gateway/` returns nothing in the config-API and
   reloader paths.

2. **Give the sync chat handler a non-interactive approval backend.** It must never prompt
   stdin. Use the same web backend the streaming path uses if a request-scoped one is
   available; otherwise an auto-deny backend, so behaviour is identical under systemd and in
   a terminal.
   **Verify**: the surface string passed to the agent is not `"cli"` on this path.

3. **Tests.** (a) A config-API write persists to the path the gateway was started with, even
   when the environment points elsewhere — set `RANTAICLAW_CONFIG_DIR` to a different
   directory under `ENV_LOCK` and assert the file that changed. (b) A sync chat request whose
   tool call needs approval resolves without reading stdin.
   **Verify**: `cargo test --lib gateway` and `cargo test --test config_api` pass.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib gateway`, `cargo test --test config_api` pass with both new tests.
- No gateway path re-resolves the config location after boot.

## STOP conditions

- `load_or_init` also performs migration or decryption that the running path needs and
  `load_from_path` does not → STOP and report; the two loaders must be reconciled first.
- Making sync chat non-interactive changes an existing documented behaviour → STOP; check
  `docs/reference/api-v1.md` before changing.

## Test plan

Two tests. The config one must hold `ENV_LOCK` — this repo has been bitten by env-mutating
tests racing.

## Maintenance note

`tests/config_api.rs` documents this mismatch in its header comment; update that comment when
the fix lands so the next reader is not misled.

## Rollback

One commit across three files plus tests.
