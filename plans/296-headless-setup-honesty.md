# Plan 296: Make headless `setup` fail loudly instead of exiting zero after doing nothing

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/main.rs src/onboard/provision/mcp.rs src/onboard/provision/provider.rs docs/reference/commands.md`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P2 (ledger W1-6)
- **Effort**: S–M
- **Risk**: LOW — turns silent success into honest failure
- **Category**: bug / operator honesty
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

`setup <topic> --non-interactive` is the path installers and CI use, and it lies in both
directions. When a provisioner errors, aborts or times out it prints a message and still
returns `Ok`, and the config is saved anyway — while the abort message says "nothing saved".
An installer cannot tell success from failure by exit code.

The headless prompt driver answers every choice with option 0. For the provider section that
is "Re-enter", so an empty secret loops until a two-minute timeout and then exits zero. For
MCP it is "install all zero-auth servers", so a documented no-op instead registers
subprocess-spawning servers — and the registration's `Result` is discarded while the success
count is still incremented.

The documentation says this command emits a hint and exits.

## Current state (verified at `4b8f61e`)

```rust
// src/main.rs:3026
async fn run_provisioner_headless(
// src/main.rs:3113 — every Choose answered with the first option
                        .send(onboard::provision::ProvisionResponse::Selection(vec![0]))
```

The error/abort/timeout arms print and fall through to `Ok(...)`, and the caller saves the
config unconditionally. `docs/reference/commands.md` describes the headless behaviour as
"emit each section's headless hint and exit".

## Steps

1. **Return a non-zero exit for `Err`, `Aborted` and timeout**, and **save only on
   `Configured`**. Validate before saving, as the interactive section path already does.
   **Verify**: `rantaiclaw setup telegram --non-interactive` with no token exits non-zero and
   leaves `config.toml` byte-identical.

2. **Replace the blanket option-0 default with per-prompt headless defaults.** The default for
   a choice in an unattended run must be the inert one: skip, or abort with a message naming
   the missing input. Never "install everything".
   **Verify**: headless MCP setup registers nothing; headless provider setup with an empty
   secret aborts immediately instead of looping to the timeout.

3. **Stop discarding registration results.** In `src/onboard/provision/mcp.rs` the
   `register_mcp` result is dropped with `let _` while the counter still increments; report
   failures and count only successes.
   **Verify**: `rg -n 'let _ = .*register_mcp' src/` returns nothing.

4. **Reconcile the docs with whatever behaviour you land.** `docs/reference/commands.md`
   currently describes a no-op; the sections write real files. Fix the doc, and the `onboard`
   alias claim beside it if it is also wrong.

5. **Tests.** Headless failure exits non-zero and writes nothing; headless MCP registers
   nothing; a genuine headless success still saves.
   **Verify**: `cargo test --test setup_e2e` and `cargo test --lib onboard` pass.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --test setup_e2e`, `cargo test --lib onboard` pass with the new tests.
- Exit code distinguishes success from failure on every headless section.

## STOP conditions

- Changing exit codes breaks an existing CI workflow or bootstrap script that relies on the
  current always-zero behaviour → STOP and report; `rg -n 'setup .*--non-interactive'
  scripts/ .github/` before you start.

## Test plan

Three tests. `setup_e2e` already drives the real binary — extend it rather than mocking.

## Maintenance note

The rule this restores: a command that did not do the thing must not exit zero. The same
question is worth asking of any other `--non-interactive` path added later.

## Rollback

One commit across `main.rs`, one provisioner and a doc. Behaviour-only.
