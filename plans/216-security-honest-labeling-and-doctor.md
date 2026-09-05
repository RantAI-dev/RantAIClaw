# Plan 216: Stop the docs/config claiming security controls that don't run, and surface the real state in doctor/status

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/traits.rs src/security/mod.rs docs/security/ src/main.rs src/doctor/`

## Status

- **Priority**: P1 (honesty — docs/config assert active security controls that are inert)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none (complements plan 215; this is the immediate honesty half)
- **Category**: docs / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

Several security controls are configured-but-inert, and the code/docs assert
they are active — which is worse than silence, because a reviewer or operator
trusting them concludes the agent is protected when it is not:

- `src/security/traits.rs:6-7` states "The agent runtime selects and applies a
  sandbox backend before executing any shell command" and "calls `wrap_command`
  before every shell execution". Plan 215 proves this is false — the sandbox is
  never applied.
- `docs/security/*` (at least `agnostic-security.md`, `frictionless-security.md`,
  `sandboxing.md`, `security-roadmap.md`, `audit-logging.md`) instruct operators
  to set `[security.sandbox]` / `[security.resources]` / `[security.audit]` —
  blocks that `config.toml` silently drops (`SecurityConfig` is not a field of
  `Config`).
- `sign_events` (`schema.rs`) promises HMAC tamper-evidence with **no HMAC code**.
- No surface (`doctor`, `status`) reports which sandbox backend is actually
  active vs configured, so a silent degrade to noop is invisible.

This plan does the **immediate, low-risk honesty pass** while plan 215 decides
the deeper wire-or-delete question. It does not implement enforcement; it stops
the false claims and surfaces the true state.

## Current state

- `src/security/traits.rs:6-17` and `src/security/mod.rs:9-13` — doc comments
  assert the sandbox is applied.
- `docs/security/*` — operator instructions to set `[security.*]` keys that are
  silently dropped. (Note: the roadmap banners in `audit-logging.md:3`,
  `resource-limits.md:3`, `frictionless-security.md:3` are honest; the *config
  instructions* elsewhere are not.)
- `src/main.rs:1940-1963` — the `status` "security" section is sourced only from
  `config.autonomy.*`; no sandbox/audit/resource state.
- `src/doctor/checks/` — no sandbox/audit/resource check.

## The fix

### Step 1 — correct the code doc comments

Rewrite `src/security/traits.rs:6-17` and `src/security/mod.rs:9-13` to state
the current reality: the `Sandbox` trait exists but is **not currently wired**
to command execution (cross-reference plan 215 / a tracking issue). Do not
assert an active control. If plan 215 lands as Option A first, make the docs
true instead; otherwise state the unwired reality.

### Step 2 — make the dead `[security.*]` keys honest

Two acceptable approaches (pick one, coordinate with plan 215):

- **If keeping the keys** (215 leaning Option A): add `pub security: SecurityConfig`
  to `Config` so the keys are at least settable and appear in `config schema`,
  and add an inline doc note on `[security.sandbox]`/`[security.resources]`/
  `[security.audit]` that they are **not yet enforced** (roadmap), matching the
  doc banners.
- **If deleting** (215 leaning Option B): remove the orphaned `SecurityConfig`
  subtree so the keys stop implying support, and delete the `[security.*]` setup
  instructions from `docs/security/*` (keep the roadmap docs clearly labeled).

Either way, the config surface and the docs must agree, and neither may present
an inert key as an active control.

### Step 3 — doctor/status surface the real state

Add a `doctor` check and/or a `status` line that reports the **resolved** sandbox
backend and its availability (active vs configured), and the audit/resource
state (enabled-in-config vs actually-enforced). Even while the layer is unwired,
this should say "sandbox: not enforced (layer unwired)" rather than implying a
backend is protecting the process. Use `create_sandbox`'s resolution logic (or a
read-only probe) without actually applying anything.

## Files

- **In scope**: `src/security/traits.rs`, `src/security/mod.rs` (doc comments),
  `docs/security/*` (the config instructions), `src/config/schema.rs` (only if
  keeping+wiring the field per 215-A), `src/main.rs` (status line),
  `src/doctor/checks/` + `src/doctor/mod.rs` (a new check).
- **Out of scope**: actually wiring the sandbox / audit / resource enforcement
  (plans 215, 218) — this plan is labeling + reporting only.

## STOP conditions

- Coordinate with plan 215: if 215 has decided A or B, follow that; if 215 has
  not run, do the **conservative** honesty pass — correct the doc comments and
  add the "not enforced" doctor/status line, and leave the `SecurityConfig`→
  `Config` decision to 215. Do not delete the structs unilaterally if 215 might
  choose to wire them.

## Done criteria

1. `cargo fmt`/`clippy` clean; `cargo build -p rantaiclaw --bin rantaiclaw`.
2. `cargo test -p rantaiclaw --lib doctor` clean, plus a test asserting the new
   doctor check reports the sandbox as not-enforced (given the current unwired
   state) rather than active.
3. A docs check / manual confirmation that no `docs/security/*` page or code
   comment asserts the sandbox is applied to shell execution while it is unwired.

## Test plan

- Doctor: add a check test mirroring the existing doctor check tests (they build
  a `DoctorContext`), asserting the sandbox status string reflects "not
  enforced" in the current build.
- Docs/comments: grep-based assertion (or manual) that the corrected wording
  landed.

## Risk & rollback

- **Risk**: LOW — documentation + a read-only status/doctor line; no enforcement
  behavior changes.
- **Rollback**: revert the touched files.

## Maintenance note

When plan 215 (and 218) actually wire a control, update these docs/status lines
to say so — the honesty must track the implementation in both directions. A
`doctor` check that reports active-vs-configured for every security control is
the durable guard against a silent degrade.
