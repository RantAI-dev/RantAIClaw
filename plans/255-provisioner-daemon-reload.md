# Plan 255: Reload the managed daemon after channel provisioning

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/onboard/provision/mod.rs src/channels/admin.rs src/tui/app.rs src/main.rs src/gateway/config_api.rs`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: MED (restarting the managed daemon from inside the TUI process bounces the user's own channel runtime)
- **Depends on**: coordinates with plan 248 (blocking-call offload)
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

A channel configured through the TUI setup overlay or `rantaiclaw setup <channel> --non-interactive` gets its config saved, but the running managed daemon keeps the old channel set — the channel appears configured and does nothing. The SAME action through the web console (`config_api::schedule_daemon_reload`) or `rantaiclaw channel bind-telegram` (`announce_daemon_reload`) reloads correctly. This is the "one contract implemented four times, one copy missing it" shape that already produced #566/#567/#569. `finalize_channel` — the shared post-provisioner hook — does not reload.

## Current state (confirm before editing)

- `src/onboard/provision/mod.rs:78` — `finalize_channel()` is the shared post-provisioner hook (installs core skills, returns owner guidance); it does NOT reload the daemon. Its callers: `src/tui/app.rs:4201` and `src/main.rs:3084`. Its doc (`provision/mod.rs:38-46`) already records finding this SAME class of omission for the core-skill install on this same path.
- The reload primitives that the other paths use: `src/gateway/config_api.rs:563` `schedule_daemon_reload()` → `crate::channels::reload_managed_daemon`; `src/channels/admin.rs:59,98` `announce_daemon_reload()` → `maybe_restart_managed_daemon_service()` (`admin.rs:194-268`). `reload_managed_daemon` has exactly one caller (config_api).
- The two drivers already render `owner_claim_guidance` differently (TUI vs CLI), so the pattern for "silent in TUI, printed in CLI" exists.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib onboard::provision` | pass |
| Build | `cargo build --lib` | exit 0 |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: `src/onboard/provision/mod.rs` (`finalize_channel` returns a reload decision), and the two drivers (`tui/app.rs:4201`, `main.rs:3084`) to act on it.
**Out of scope**: the reload primitives' internals; the blocking-call offload (plan 248 — but if the reload is blocking, wrap it per that plan's pattern).

## Git workflow

- Branch: `fix/provisioner-daemon-reload`
- Message e.g. `fix(onboard): reload the managed daemon after channel provisioning`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: `finalize_channel` returns a structured reload outcome

Have `finalize_channel` (it already takes category + profile + config) compute the reload decision (mirror `config_api::needs_runtime_restart`/`runtime_restart_note`) and return it as a structured outcome the driver renders. Do the reload via the existing `maybe_restart_managed_daemon_service`/`reload_managed_daemon` primitive; keep the `Ok(false)`/no-service branch silent in the TUI and printed in the CLI. If the reload call is blocking, wrap it in `spawn_blocking` (per plan 248).

**Verify**: `cargo build --lib` exit 0; `cargo test --lib onboard::provision` → pass.

### Step 2: Drivers act on the outcome

Update `tui/app.rs:4201` and `main.rs:3084` to read the outcome and render appropriately (TUI: silent/overlay note; CLI: printed line).

**Verify**: Test-plan `finalize_channel_signals_reload` passes.

## Test plan

- `onboard::provision`: `finalize_channel_signals_reload` — after finalizing a channel on a config with a managed daemon, the returned outcome indicates a reload was requested (assert the decision, not a live restart, so the test needs no running daemon).
- Verification: `cargo test --lib onboard::provision` → pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped test passes; `finalize_channel` returns a reload decision
- [ ] both drivers act on the outcome (grep `tui/app.rs:4201`, `main.rs:3084` context)
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Reloading from inside the TUI process would bounce the user's OWN runtime in a way that breaks the overlay — if so, prefer signalling the daemon (not an in-process restart) and report the constraint.
- `finalize_channel` has more than the two known callers (drift) — enumerate first.

## Maintenance notes

- Reviewer: confirm the reload decision is one shared computation, not a fifth copy — the whole point is to converge `finalize_channel`, `config_api`, `admin.rs`, and the CLI on one decision.
- Interacts with plan 248 (blocking-call offload) if the reload path blocks.
