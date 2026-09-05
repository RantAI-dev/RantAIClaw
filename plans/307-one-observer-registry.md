# Plan 307: One metrics registry, fed by every runtime path

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/observability/ src/gateway/mod.rs src/daemon/mod.rs src/channels/mod.rs src/agent/`

## Status

- **Priority**: P2 (ledger W2-4, other half) · **Effort**: M · **Risk**: MED
- **Category**: bug / operability
- **Planned at**: commit `bf77d26`, 2026-09-05

## Why this matters

`create_observer` is called from six places — the gateway, the daemon, the channel runtime,
agent construction and twice in the agent loop — and each builds its own registry. `/metrics`
serves the gateway's. So a daemon-mode install, where channels and cron do the actual work,
exposes metrics that describe almost nothing that happened.

Several metrics are inert even in the served registry: the LLM request and response events are
no-ops in the Prometheus observer, and the gauges for tokens, active sessions, queue depth and
heartbeat ticks have no emitter at all. Pillar 7 calls this Stable.

## Steps

1. **Map the six call sites and decide the ownership.** One registry per process, created at
   startup and passed down — the `from_config_with_observer` seam already exists for exactly
   this and is the shape to follow.
   **Verify**: write the intended ownership in the PR body before coding.
2. **Thread one observer through**, so the daemon's channels, cron and heartbeat feed the same
   registry the gateway serves.
3. **Either emit the inert metrics or delete them.** A gauge with no emitter is worse than a
   missing gauge: it reads as zero. If tokens depend on plan 306, delete the gauge here and
   let 306 add it back with real data.
4. **Fix pillar 7's claims**, including the config keys it documents that do not exist.
5. **Test what serving actually returns.** Drive a channel turn and assert the served
   `/metrics` reflects it — the property that has never held.

## Done criteria

- One registry per process; `/metrics` reflects work done by channels, cron and heartbeat.
- No gauge is exposed without an emitter.
- `cargo test --lib observability`, `--lib gateway` pass with the new end-to-end assertion.

## STOP conditions

- A single registry would force a global or a static that the architecture avoids → STOP and
  report; passing it through is the intent, not a shortcut around it.
- The daemon and gateway turn out to be separate processes in some deployment → STOP; then the
  right answer is one registry per process and a documented scrape target per process.

## Maintenance note

The rule: a metric is added together with its emitter and a test that the served endpoint
shows it. Four gauges arrived without one.

## Rollback

One commit; observability-only, no behaviour change to the agent.
