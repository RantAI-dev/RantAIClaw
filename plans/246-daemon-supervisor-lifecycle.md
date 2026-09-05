# Plan 246: Make the daemon component supervisor and shutdown correct

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/daemon/mod.rs src/gateway/mod.rs src/channels/mod.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (supervisor + shutdown lifecycle; the fatal/transient distinction must be right)
- **Depends on**: plan 245 (so a loopback host is no longer a fatal error) — soft
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27
- **Execution outcome (2026-08-27)**: **SPLIT.** D11 (backoff race, Step 2) + D4 (reloader
  leak, Step 3) shipped in **PR #656** — both self-contained resource-lifecycle fixes that roll
  back independently. D3 (fatal-vs-transient, Step 1) + D9 (channels drain, Step 4) are behavior/
  contract changes deferred to **plan 265** (`fix/daemon-fatal-exit-and-channels-drain`) so they
  land as their own reviewable, isolated-rollback PR (CLAUDE.md §3.8, §10).

## Why this matters

Four defects in the daemon lifecycle:

1. **Retries fatal errors forever** (D3). The supervisor restarts on ANY `Err` with capped backoff and never gives up. A misconfigured public bind, a bad host, or an occupied port is fatal but retried in a silent infinite loop — the daemon prints "daemon started", the process stays alive so systemd sees a healthy unit, and only a `tracing::error!` every backoff period signals trouble.
2. **Backoff sleep not raced against shutdown** (D11). The `sleep(backoff)` is not raced against the cancellation token, so SIGTERM during backoff is ignored until the sleep elapses.
3. **Config-watcher task leaked per restart** (D4). `build_gateway_router` unconditionally spawns `spawn_config_reloader` (an inotify watch + task, no cancellation) → leaked on every gateway restart; in a crash loop it exhausts `max_user_watches`.
4. **Channels hard-aborted on shutdown** (D9). The channels supervisor discards the cancellation token and `abort()`s channels outright on shutdown → dropped replies, uncommitted long-poll offsets (dup reprocessing). `start_channels_with_cancellation` exists and is used by the TUI.

## Current state

- `src/daemon/mod.rs` `spawn_component_supervisor` (`:266-296`):
  ```rust
  loop {
      crate::health::mark_component_ok(name);
      let outcome = run_component().await;
      if shutdown.is_cancelled() { break; }        // :275 — only checked after run returns
      match outcome { Ok(()) => {...reset backoff...}, Err(e) => {...error, continue...} }  // :285 — any Err retried
      crate::health::bump_component_restart(name);
      tokio::time::sleep(Duration::from_secs(backoff)).await;   // :292 — not raced against shutdown
      backoff = backoff.saturating_mul(2).min(max_backoff);
  }
  ```
- Gateway fatal errors: `run_gateway` `bail!`s on the `allow_public_bind` refusal (`gateway/mod.rs:902`) and address-parse / `TcpListener::bind` (`:911-912`) — all the same `Err` shape the supervisor retries.
- `src/gateway/mod.rs:812` — `build_gateway_router` calls `spawn_config_reloader` unconditionally; `spawn_config_reloader` (`:1253`) takes no cancellation token.
- `src/channels/mod.rs` — `start_channels_with_cancellation` (`:533-556`) exists and documents that a well-behaved channel aborts its long-poll cleanly via the token; the daemon uses the non-cancellable `start_channels` (`daemon/mod.rs:95`) and `abort()`s on shutdown (`:169-174`).

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib daemon` | pass |
| Test | `cargo test --lib gateway` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/daemon/mod.rs` (fatal-vs-transient supervisor, backoff raced against token, channels via cancellable entrypoint + bounded drain)
- `src/gateway/mod.rs` (thread a cancellation token into `spawn_config_reloader`; move the spawn out of `build_gateway_router` into `run_gateway` so the factory is side-effect-free)

**Out of scope**:
- `require_pairing` hot-reload (plan 236) — coordinate: both touch `spawn_config_reloader`.
- The gateway host resolution (plan 245).

## Git workflow

- Branch: `fix/daemon-supervisor-lifecycle`
- Message e.g. `fix(daemon): stop retrying fatal startup failures and cancel components cleanly on shutdown`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Classify fatal vs transient; propagate fatal out of the supervisor

Give the supervisor a fatal-error predicate (or have `run_gateway` return a typed error whose bind/config variants are fatal). On a fatal error: cancel `shutdown` and return the error from `daemon::run` so the process exits non-zero and the failure is visible in `systemctl status` (systemd's `Restart=always`+`RestartSec=3` then does the retrying WITH the failure visible). Move the "daemon started" banner to AFTER the gateway's first successful bind.

**Verify**: Test-plan `fatal_bind_error_exits_not_loops` passes.

### Step 2: Race the backoff sleep against the shutdown token

Wrap the `sleep(backoff)` (`daemon/mod.rs:292`) in `tokio::select! { () = sleep(...) => {}, () = shutdown.cancelled() => break }`.

**Verify**: Test-plan `supervisor_stops_promptly_during_backoff` passes.

### Step 3: Cancel the config reloader; make the router factory side-effect-free

Thread the existing `CancellationToken` into `spawn_config_reloader` and `select!` it against `reload_rx.recv()`. Move the `spawn_config_reloader` call out of `build_gateway_router` into `run_gateway` so the factory (used by tests/embedders) no longer starts a filesystem watch. Update the factory's doc comment (it currently claims "cheap and hermetic").

**Verify**: `cargo test --lib gateway` → pass; the factory no longer spawns a watcher (assert by inspection / a test that builds the router in a temp dir and confirms no reload task lingers if feasible).

### Step 4: Cancel channels instead of hard-aborting

In `daemon/mod.rs`, pass `shutdown.clone()` into `start_channels_with_cancellation` (`:95`), and on shutdown await the channels handle under a bounded `timeout(...)` (mirror the gateway drain at `:159-165`) before falling back to `abort()`.

**Verify**: Test-plan `channels_get_a_drain_window` passes (or a compile+inspection check if a full channel runtime is too heavy to unit-test).

## Test plan

- `daemon`: `fatal_bind_error_exits_not_loops` — a supervised component returning a fatal error causes `daemon::run` to return Err (not loop). Use a stub component returning a typed fatal error.
- `supervisor_stops_promptly_during_backoff` — cancel the token while the component is in backoff; assert the supervisor task completes promptly.
- `gateway`: assert `build_gateway_router` in a temp dir does not leave a running reloader (best-effort).
- Verification: `cargo test --lib daemon gateway` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped daemon + gateway tests pass incl. the fatal-exit and prompt-stop tests
- [ ] `spawn_config_reloader` takes a cancellation token (`grep -n "fn spawn_config_reloader" src/gateway/mod.rs`)
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- The fatal/transient classification is ambiguous for a given error (e.g. EADDRINUSE could be transient during a restart) — treat bind-refusal + address-parse as fatal, EADDRINUSE as fatal-after-N-retries, and document the choice; report if unsure.
- Plan 236 has already refactored `spawn_config_reloader` — merge with its `sync_from_config` change rather than reverting it.
- Making channels cancellable requires changing `start_channels_with_cancellation`'s contract — it already exists for the TUI; if the daemon needs a different shape, report.

## Maintenance notes

- Reviewer: confirm a fatal bind error now EXITS the process (visible in systemctl) rather than looping with a false "started" banner, and that the reloader is cancellable.
- Interacts with plans 236 (require_pairing reload) and 245 (host resolution) — all three touch gateway/daemon startup; sequence carefully.
