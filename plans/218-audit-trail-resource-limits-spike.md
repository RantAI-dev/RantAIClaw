# Plan 218 (SPIKE): The audit trail and resource limits are configured but inert — wire or mark dead honestly

> **Executor instructions**: DECISION spike. Choose per-subsystem (audit /
> resources), record it, and either produce a follow-up plan (wire) or make the
> config honest (mark dead — directly executable, overlaps plan 216). On a
> "STOP condition", stop and report. When done, update this plan's status row in
> `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/audit.rs src/approval/mod.rs src/config/schema.rs src/runtime/native.rs src/profile/paths.rs`

## Status

- **Priority**: P2 (security — no forensic trail; resource caps do nothing)
- **Effort**: M (wire) / S (mark dead)
- **Risk**: MED
- **Depends on**: 215/216 (the `SecurityConfig`→`Config` wiring decision is shared)
- **Category**: security / direction (spike)
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

Two more configured-but-inert security subsystems sit under `[security.*]`:

1. **Audit trail is dead.** `AuditLogger` (`src/security/audit.rs`) — a persistent,
   HMAC-signable event log — is constructed only in its own tests. No security
   event (command execution, approval, denial, allowlist change, autonomy change,
   auth) is ever written to disk. The only runtime "audit log" is a
   process-ephemeral `Mutex<Vec<ApprovalLogEntry>>` in `src/approval/mod.rs:69`
   that evaporates on restart and is used only to drive the in-chat prompt. The
   profile `audit.log` path (`src/profile/paths.rs:106`) is computed but never
   written. `sign_events` promises HMAC tamper-evidence with **no HMAC code**.
   A security boundary that approves privileged shell actions keeps zero
   tamper-evident record of what it approved.

2. **Resource limits are inert.** `ResourceLimitsConfig` (`max_memory_mb`,
   `max_cpu_time_seconds`, `max_subprocesses`, `memory_monitoring`) is read by
   nobody — there is no `setrlimit`/cgroup/ulimit anywhere (grep: 0 hits). The
   shell child is bounded only by a wall-clock timeout and env-clearing; a
   runaway tool command can consume unbounded memory/CPU and fork freely.

Both are gated behind the same root as the sandbox: `SecurityConfig` is not a
field of `Config`, so `[security.audit]`/`[security.resources]` are silently
dropped.

## The decision (produce this, per subsystem)

### Audit trail

- **Wire (A):** construct one `AuditLogger` at startup from the profile's
  `audit.log` path and append a signed JSON line from
  `ApprovalManager::record_decision`, the shell tool executor, and the
  autonomy/allowlist mutation paths. Implement `sign_events` as per-line
  HMAC-SHA256 (key from `SecretStore`) + a verify path. Bounded/async on the hot
  path (the shell tool has process-group/timeout logic — don't block it on
  `sync_all`).
- **Mark dead (B):** delete `AuditLogger`/`AuditConfig`/`sign_events` (or clearly
  mark them roadmap in schema + docs, per plan 216) so the config stops implying
  a tamper-evident trail exists.

Recommendation: audit logging of approval/denial decisions is high-value for a
security boundary — lean toward **A** for at least the approval-decision events,
even if command-execution logging is deferred.

### Resource limits

- **Wire (A):** add `libc::setrlimit(RLIMIT_AS / RLIMIT_CPU / RLIMIT_NPROC)` in
  the Unix `pre_exec` in `src/runtime/native.rs` (post-fork, pre-exec), gated on
  `ResourceLimitsConfig`. Over-tight limits break compilers/package managers, so
  defaults must be generous and documented.
- **Mark dead (B):** delete `ResourceLimitsConfig` (or mark roadmap) so
  `max_memory_mb` stops implying a cap that does not exist.

## Deliverable of this spike

- Per-subsystem decisions (A/B) with rationale, in the PR + `plans/README.md`.
- If A (audit): a follow-up plan wiring `AuditLogger` into the approval + shell +
  policy-mutation paths, with `sign_events` HMAC, rotation, and tests (an
  approval decision produces a signed on-disk line that survives restart).
- If A (resources): a follow-up plan adding `setrlimit` in `pre_exec` with tests
  (a memory/fork-bomb command hits the limit; a normal command is unaffected).
- If B (either): the deletion/relabel PR (overlaps plan 216 — coordinate so the
  `SecurityConfig`→`Config` decision is made once).

## Files (for eventual implementation)

- Audit: `src/security/audit.rs`, `src/approval/mod.rs`, `src/tools/shell.rs`,
  `src/approval/permissions.rs`, `src/profile/paths.rs`, `src/config/schema.rs`.
- Resources: `src/runtime/native.rs`, `src/config/schema.rs`.

## STOP conditions

- The audit hot-path write must not block the shell tool's process-group/timeout
  reaping — design it bounded/async before wiring, or STOP and report.
- `setrlimit` defaults that are too tight will break legitimate builds — do not
  ship aggressive defaults; if unsure, choose B for resources and document.
- Coordinate the `SecurityConfig`→`Config` wiring with plans 215/216 so it is
  done once, not three times.

## Done criteria (spike)

- Decisions recorded; the follow-up plans (A) exist and/or the deletion/relabel
  PR (B) is opened and passes `cargo fmt`/`clippy`/`cargo test`.

## Risk & rollback

- Spike is low-risk. Audit-wiring is MED (hot-path I/O); resource-limit wiring is
  MED (can break builds). Both land behind their own tests and generous defaults.

## Maintenance note

A security boundary needs a tamper-evident record of its decisions; approval/
denial logging is the minimum. Resource limits are secondary but real for
runaway containment. Whatever is decided, the config must not present an inert
key as an active control (plan 216 enforces that labeling).
