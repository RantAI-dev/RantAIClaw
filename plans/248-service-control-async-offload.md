# Plan 248: Run blocking service-control calls off the async runtime

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/main.rs src/onboard/provision/whatsapp_web.rs src/channels/admin.rs src/service/mod.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW (mechanical `spawn_blocking` wrapping)
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Blocking service-control calls (`systemctl restart` can block up to the unit's `TimeoutStopSec=30`) run synchronously inside async functions at four sites — the same class as the shipped #624 doctor fix. The worst is the whatsapp_web provisioner, which freezes the TUI's runtime worker (and every other in-flight async task) for up to 30s during a restart. The pattern to copy already exists: `config_api.rs:566` wraps the same function in `spawn_blocking`.

## Current state

- `src/main.rs:2056-2062` — `service::handle_command` is called directly inside `#[tokio::main] async fn main`; it is synchronous and shells out via `std::process::Command::output()` (`service/mod.rs:1244-1251`), including `systemctl --user restart`.
- `src/onboard/provision/whatsapp_web.rs:251-254` — `crate::service::apply_channel_config(...)` (blocking `daemon-reload`+`restart`, `service/mod.rs:200-215`) called inline in an `async fn` (the TUI provisioning overlay). **Worst site.**
- `src/channels/admin.rs:59,98` — `announce_daemon_reload()` → `maybe_restart_managed_daemon_service()` (blocking `launchctl`/`rc-service`/`systemctl` at `admin.rs:194-268`) called from `async fn bind_telegram_identity`/`unbind_telegram_identity`; also `src/main.rs:2563`.
- Exemplar (already correct): `src/gateway/config_api.rs:566` wraps the same function in `spawn_blocking`; `src/main.rs:1993,2015,2071,2087` do the same for `models`/`ui`/`doctor models`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Build | `cargo build --lib` | exit 0 |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/main.rs` (the `service` and `permissions`→`announce_daemon_reload` call sites)
- `src/onboard/provision/whatsapp_web.rs`
- `src/channels/admin.rs` (the two async callers of `maybe_restart_managed_daemon_service`)

**Out of scope**:
- The service-control functions' internals (they stay synchronous; you only move the CALL off the async worker).
- The launchd race / unit generation (plans 247, 244).

## Git workflow

- Branch: `fix/service-control-async-offload`
- Message e.g. `fix(service): run blocking service-control calls via spawn_blocking`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Wrap the four call sites in `spawn_blocking`

For each site, move the blocking call into `tokio::task::spawn_blocking(move || …).await?` (the functions take owned/`Clone` inputs — clone what's needed). Mirror `config_api.rs:566`. The four sites: `main.rs:2056` (service), `main.rs:2563` (announce_daemon_reload), `whatsapp_web.rs:251` (apply_channel_config — the priority), `channels/admin.rs:59` + `:98` (announce_daemon_reload).

**Verify**: `cargo build --lib` exit 0; `cargo clippy --lib -- -D warnings` exit 0.

### Step 2: Document the blocking contract

Add a short doc note (or a `_blocking` suffix if the team prefers) to `service::handle_command`/`apply_channel_config`/`maybe_restart_managed_daemon_service` warning callers they block, so future async callers wrap them.

**Verify**: `cargo fmt --all -- --check` exit 0.

## Test plan

- No new behavior tests are practical (these shell out); the change is mechanical. If a seam allows, add a compile-time check that the call sites are inside `spawn_blocking` (or just rely on review).
- Verification: `cargo build --lib` + `cargo clippy --lib -- -D warnings` → clean.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] all four sites now call through `spawn_blocking` (grep each file for `spawn_blocking` near the cited lines)
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- A call site holds a non-`Send` value across the await (e.g. a `parking_lot` guard) — restructure so the blocking work owns only `Send` data before `spawn_blocking`; report if it can't be cleanly separated.
- A site is on a genuinely synchronous (non-async) path — leave it; only the async callers need wrapping.

## Maintenance notes

- Reviewer: confirm the whatsapp_web site (the TUI-freezing one) is wrapped; that is the priority.
- This closes the residual #624 class in the lifecycle paths; a future audit should grep for `std::process::Command` inside `async fn` to catch new instances.
