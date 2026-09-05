# Plan 265: Fatal-vs-transient daemon supervisor + graceful channels drain

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Follow-up to plan 246.** Plan 246 shipped the two self-contained resource-lifecycle fixes (D11 backoff race, D4 reloader leak) in PR #656. This plan carries the two remaining lifecycle changes that alter observable daemon behavior / a channels contract, split off so they land as their own reviewable, independently rollback-able PR (CLAUDE.md §3.8, §10).

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (supervisor error-propagation contract + channels shutdown semantics)
- **Depends on**: plan 246 / PR #656 (merged) — the backoff race and per-run reloader are assumed present
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Two defects remain in `src/daemon/mod.rs` after PR #656:

1. **Retries fatal startup errors forever** (D3). `spawn_component_supervisor` restarts on ANY `Err` with capped backoff and never gives up. A misconfigured public bind, a bad host, or (arguably) an occupied port is fatal but retried in a silent infinite loop — the process prints "🧠 RantaiClaw daemon started" *before* the first bind is known to have succeeded, stays alive so systemd sees a healthy unit, and only a `tracing::error!` every backoff period signals trouble. An operator running `systemctl status` sees "active (running)" for a daemon that has never served a request.
2. **Channels hard-aborted on shutdown** (D9). The daemon runs channels via the non-cancellable `start_channels` (`daemon/mod.rs:102`) and `abort()`s the handle outright on shutdown (`:169-171`, in the `handles` loop) → dropped in-flight replies, uncommitted long-poll offsets (duplicate reprocessing on next start). `start_channels_with_cancellation` already exists and is used by the TUI; it drains cleanly when its token is cancelled.

## Current state (post-#656)

- `spawn_component_supervisor` (`src/daemon/mod.rs:257`) — generic over `FnMut() -> Fut`, `Fut::Output = Result<()>`, returns `JoinHandle<()>`. The loop checks `shutdown.is_cancelled()` after `run_component()` returns and treats every `Err` as transient (restart after backoff, now raced against shutdown).
- `daemon::run` (`:27`) spawns the gateway supervisor (`:74-90`) and the channels/heartbeat/scheduler supervisors into `handles` (`:70`, `:95-142`), prints the "daemon started" banner unconditionally at `:144-147`, awaits `shutdown_signal()`, then drains the gateway under `GATEWAY_DRAIN_TIMEOUT` (`:159-165`) and `abort()`s the rest (`:169-174`).
- Gateway fatal errors: `run_gateway` `bail!`s on the `allow_public_bind` refusal (`gateway/mod.rs`) and address-parse / `TcpListener::bind` — all the same anyhow `Err` shape the supervisor retries.
- `start_channels_with_cancellation(config, shutdown)` (`channels/mod.rs:553`) — cancelling `shutdown` makes each listener stop, drops the message-bus senders, closes the dispatch loop, returns `Ok(())`.

## Scope

**In scope**:
- `src/daemon/mod.rs` — fatal-vs-transient classification that propagates a fatal error out of `daemon::run` (non-zero exit, visible in `systemctl status`); move the "daemon started" banner to after the gateway's first successful bind; channels via `start_channels_with_cancellation` + a bounded drain window.
- `src/gateway/mod.rs` — if D3 needs a typed error, give `run_gateway` (or a thin wrapper) a way to distinguish fatal bind/config failures from transient ones.

**Out of scope**:
- Anything shipped in #656 (backoff race, reloader lifecycle) — do not revert it.
- `require_pairing` hot-reload (plan 236) and host resolution (plan 245).

## Design decisions to make (resolve before coding; STOP if still ambiguous)

- **Fatal classification.** Treat bind-refusal (`allow_public_bind`) and address-parse as **fatal** (exit). `EADDRINUSE` is ambiguous — during a restart it can be transient (old process still releasing the port) but under a real port conflict it is fatal. Decision: **fatal-after-N-retries** for `EADDRINUSE` (a small bounded retry count, then propagate). Document the choice inline.
- **Propagation mechanism.** The supervisor runs in a spawned task and returns `JoinHandle<()>`; a fatal error must reach `daemon::run`. Options: (a) a `oneshot`/`watch` "fatal" channel the supervisor sends on before exiting, which `run` selects against alongside `shutdown_signal()`; (b) a readiness channel that also carries the first-bind result. Prefer the smallest change that (1) exits non-zero on fatal, (2) lets the banner print only after first successful bind.
- **Banner-after-bind.** The gateway must signal "bound OK" once. A `tokio::sync::oneshot`/`Notify` passed into `run_gateway` (fired right after `TcpListener::bind` succeeds) is the likely shape; `run` awaits it (or a fatal error) before printing the banner.

## Steps

### Step 1: Fatal-error predicate + propagation

Introduce a way for a supervised component to report a fatal (non-retryable) failure. On fatal: cancel `shutdown`, stop the supervisor, and cause `daemon::run` to return that error. Classify gateway bind-refusal + address-parse as fatal; `EADDRINUSE` as fatal-after-N.

**Verify**: `fatal_bind_error_exits_not_loops` — a stub component returning a typed fatal error makes `daemon::run` (or a testable inner) return `Err` rather than loop.

### Step 2: Banner after first successful bind

Thread a first-bind readiness signal from `run_gateway` into `run`; print "🧠 RantaiClaw daemon started" only after it fires (or exit on a fatal error before it).

**Verify**: inspection + a test that the banner path is gated on the readiness signal.

### Step 3: Channels drain instead of hard-abort

Switch the channels supervisor closure to `start_channels_with_cancellation(cfg, shutdown.clone())`. On shutdown, give the channels handle a bounded drain window (mirror the gateway drain at `:159-165`) before falling back to `abort()`. This likely means holding the channels handle separately from the `handles` vec (like `gateway_handle`).

**Verify**: `channels_get_a_drain_window` (or a compile+inspection check if a full channel runtime is too heavy to unit-test).

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Test | `cargo test --lib -- daemon gateway channels` | pass |
| Strict-delta gate | `BASE_SHA=$(git merge-base origin/main HEAD) bash scripts/ci/rust_strict_delta_gate.sh` | no blocking issues on changed lines |

**Disk constraint**: never bare `cargo test`.

## Git workflow

- Branch: `fix/daemon-fatal-exit-and-channels-drain`
- Message e.g. `fix(daemon): exit on fatal startup failures and drain channels cleanly on shutdown`
- Do NOT push/PR unless instructed.

## Done criteria

- [ ] A fatal bind/config error EXITS the process (non-zero, visible in `systemctl status`) instead of looping with a false "started" banner
- [ ] The banner prints only after the gateway's first successful bind
- [ ] Channels get a bounded drain window on shutdown (no bare `abort()` first)
- [ ] `cargo fmt --all -- --check` exit 0; scoped daemon/gateway/channels tests pass
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- The fatal/transient classification is ambiguous for a given error beyond the documented `EADDRINUSE` case — report rather than guess.
- Making channels cancellable requires changing `start_channels_with_cancellation`'s contract (it already exists for the TUI; if the daemon needs a different shape, report).
- Threading a readiness/fatal channel would require restructuring `daemon::run` in a way that touches unrelated components — report and propose the smaller cut.

## Maintenance notes

- Reviewer: confirm a fatal bind error now EXITS (systemctl shows failed), the banner is gated on first bind, and channels drain (not abort) on stop.
- Interacts with plans 236 (require_pairing reload) and 245 (host resolution); all touch gateway/daemon startup — sequence carefully.
